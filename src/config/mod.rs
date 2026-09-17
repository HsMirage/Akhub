//! 不可变配置快照与原子切换（§21）。
//!
//! 所有管理端写操作先落库，再整体重建一份 [`RuntimeConfig`] 并通过 `ArcSwap`
//! 一次替换。在途请求持有旧 `Arc`，因此不会读到一半新一半旧的配置。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use arc_swap::ArcSwap;

use crate::domain::{Account, DispatchTarget, Group, Limits, LogicalModel};
use crate::storage::Store;

/// 一个调度目标及其解析后的账号与有效优先级。
#[derive(Debug)]
pub struct TargetView {
    pub target: DispatchTarget,
    pub account: Arc<Account>,
    /// 目标覆盖值优先于账号默认值（§9.2）。
    pub priority: i32,
}

impl TargetView {
    /// 该目标实际生效的 RPM / TPM / 最大并发（§17.1）。
    ///
    /// 账号级的并发与额度由同一把 Key 下的所有模型共享，目标覆盖值用于更细
    /// 的限制。
    pub fn limits(&self) -> Limits {
        self.account.limits.overridden_by(self.target.limits)
    }
}

/// 一个逻辑模型及其全部候选目标，目标按有效优先级从高到低排序。
#[derive(Debug)]
pub struct LogicalModelView {
    pub model: LogicalModel,
    pub targets: Vec<Arc<TargetView>>,
}

impl LogicalModelView {
    /// 是否应当出现在 `/v1/models` 中：启用且至少有一个调度目标（§7.3）。
    pub fn is_listable(&self) -> bool {
        self.model.enabled && !self.targets.is_empty()
    }
}

/// 一个分组及其逻辑模型索引。
#[derive(Debug)]
pub struct GroupView {
    pub group: Group,
    /// 逻辑模型名 → 视图。分组是调度硬边界，查找绝不跨组。
    pub models: HashMap<String, Arc<LogicalModelView>>,
}

/// 一份完整、不可变的运行时配置。
#[derive(Debug, Default)]
pub struct RuntimeConfig {
    pub version: u64,
    pub groups: Vec<Arc<GroupView>>,
    /// 下游 Key 摘要 → 分组。鉴权只需一次哈希查表。
    by_key_digest: HashMap<String, Arc<GroupView>>,
}

impl RuntimeConfig {
    /// 按下游 Key 的摘要定位分组。
    pub fn group_by_key_digest(&self, digest_hex: &str) -> Option<&Arc<GroupView>> {
        self.by_key_digest.get(digest_hex)
    }

    pub fn group_by_id(&self, id: &str) -> Option<&Arc<GroupView>> {
        self.groups.iter().find(|g| g.group.id == id)
    }

    /// 当前配置中全部账号与目标的 ID，供动态状态表清理已消失的条目。
    pub fn live_ids(&self) -> (Vec<String>, Vec<String>) {
        let mut accounts = Vec::new();
        let mut targets = Vec::new();
        for group in &self.groups {
            for model in group.models.values() {
                for target in &model.targets {
                    accounts.push(target.account.id.clone());
                    targets.push(target.target.id.clone());
                }
            }
        }
        accounts.sort_unstable();
        accounts.dedup();
        (accounts, targets)
    }
}

/// 持有当前配置快照，并负责从数据库整体重建。
pub struct ConfigService {
    store: Store,
    current: ArcSwap<RuntimeConfig>,
    version: AtomicU64,
}

impl ConfigService {
    /// 从数据库加载首个快照。
    pub async fn load(store: Store) -> Result<Self> {
        let service = Self {
            store,
            current: ArcSwap::from_pointee(RuntimeConfig::default()),
            version: AtomicU64::new(0),
        };
        service.reload().await?;
        Ok(service)
    }

    /// 当前快照。热路径上只做一次原子指针读取（§19.4）。
    pub fn current(&self) -> arc_swap::Guard<Arc<RuntimeConfig>> {
        self.current.load()
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// 重新读取全部配置并原子替换当前快照。
    pub async fn reload(&self) -> Result<Arc<RuntimeConfig>> {
        let groups = self.store.list_groups().await?;
        let accounts = self.store.list_accounts().await?;
        let models = self.store.list_logical_models().await?;
        let targets = self.store.list_targets().await?;

        let version = self.version.fetch_add(1, Ordering::AcqRel) + 1;
        let config = Arc::new(build(version, groups, accounts, models, targets));
        self.current.store(Arc::clone(&config));
        Ok(config)
    }
}

/// 把扁平的数据库记录组装成按分组、逻辑模型索引的快照。
fn build(
    version: u64,
    groups: Vec<Group>,
    accounts: Vec<Account>,
    models: Vec<LogicalModel>,
    targets: Vec<DispatchTarget>,
) -> RuntimeConfig {
    // 配置快照是网关的硬隔离边界。即使数据库或备份里出现了跨分组引用，
    // 也不能让一个分组的请求拿到另一个分组的上游账号。
    let model_groups: HashMap<&str, &str> = models
        .iter()
        .map(|model| (model.id.as_str(), model.group_id.as_str()))
        .collect();
    let accounts: HashMap<String, Arc<Account>> = accounts
        .into_iter()
        .map(|a| (a.id.clone(), Arc::new(a)))
        .collect();

    let mut targets_by_model: HashMap<String, Vec<Arc<TargetView>>> = HashMap::new();
    for target in targets {
        // 账号可能刚被删除；引用不到账号的目标直接丢弃，不能让它进入调度。
        let Some(account) = accounts.get(&target.account_id).cloned() else {
            continue;
        };
        // 逻辑模型与账号必须属于同一分组；跨组目标直接丢弃，避免绕过
        // 下游 Key 的分组边界（正常管理 API 也会拒绝此类写入）。
        let Some(model_group) = model_groups.get(target.logical_model_id.as_str()) else {
            continue;
        };
        if account.group_id != *model_group {
            continue;
        }
        let priority = target.priority_override.unwrap_or(account.default_priority);
        targets_by_model
            .entry(target.logical_model_id.clone())
            .or_default()
            .push(Arc::new(TargetView {
                target,
                account,
                priority,
            }));
    }

    let mut models_by_group: HashMap<String, HashMap<String, Arc<LogicalModelView>>> =
        HashMap::new();
    for model in models {
        let mut targets = targets_by_model.remove(&model.id).unwrap_or_default();
        // 严格阶梯要求先按优先级从高到低排序；同优先级即同层（§9.2）。
        targets.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.target.id.cmp(&b.target.id))
        });
        models_by_group
            .entry(model.group_id.clone())
            .or_default()
            .insert(
                model.name.clone(),
                Arc::new(LogicalModelView { model, targets }),
            );
    }

    let mut views = Vec::with_capacity(groups.len());
    let mut by_key_digest = HashMap::with_capacity(groups.len());
    for group in groups {
        let models = models_by_group.remove(&group.id).unwrap_or_default();
        let digest = group.key_digest_hex.clone();
        let view = Arc::new(GroupView { group, models });
        by_key_digest.insert(digest, Arc::clone(&view));
        views.push(view);
    }

    RuntimeConfig {
        version,
        groups: views,
        by_key_digest,
    }
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;
    use crate::domain::{
        ModelOrigin, Multiplier, MultiplierMode, Protocol, SchedulingWeights, UpstreamType,
    };

    fn group(id: &str, digest: &str) -> Group {
        Group {
            id: id.into(),
            name: id.into(),
            key_prefix: "akh-000000".into(),
            key_digest_hex: digest.into(),
            multiplier_limit: Multiplier::ONE,
            weights: SchedulingWeights::default(),
            queue_capacity: 100,
            max_wait_secs: 60,
            allow_degrade: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn account(id: &str, group_id: &str, priority: i32) -> Account {
        Account {
            id: id.into(),
            group_id: group_id.into(),
            name: id.into(),
            upstream_type: UpstreamType::OpenAiCompatible,
            base_url: "https://api.example.com".into(),
            preferred_protocol: Protocol::OpenAiChat,
            adaptive_protocol: true,
            default_priority: priority,
            calibration: Multiplier::ONE,
            multiplier_mode: MultiplierMode::Manual,
            manual_multiplier: Multiplier::ONE,
            new_api_user_id: None,
            new_api_group: None,
            limits: Limits::default(),
            allow_private_network: false,
            enabled: true,
            auto_sync: false,
            model_synced_at: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn model(id: &str, group_id: &str, name: &str) -> LogicalModel {
        LogicalModel {
            id: id.into(),
            group_id: group_id.into(),
            name: name.into(),
            origin: ModelOrigin::Auto,
            enabled: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn target(
        id: &str,
        model_id: &str,
        account_id: &str,
        override_priority: Option<i32>,
    ) -> DispatchTarget {
        DispatchTarget {
            id: id.into(),
            logical_model_id: model_id.into(),
            account_id: account_id.into(),
            upstream_model: "glm-4.6".into(),
            priority_override: override_priority,
            limits: Limits::default(),
            enabled: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn targets_are_sorted_by_effective_priority_descending() {
        let config = build(
            1,
            vec![group("g1", "d1")],
            vec![
                account("a1", "g1", 30),
                account("a2", "g1", 100),
                account("a3", "g1", 60),
            ],
            vec![model("m1", "g1", "glm-4.6")],
            vec![
                target("t1", "m1", "a1", None),
                target("t2", "m1", "a2", None),
                // 目标覆盖值胜过账号默认值：a3 的 60 被压到 10。
                target("t3", "m1", "a3", Some(10)),
            ],
        );

        let model = &config.groups[0].models["glm-4.6"];
        let priorities: Vec<_> = model.targets.iter().map(|t| t.priority).collect();
        assert_eq!(priorities, vec![100, 30, 10]);
    }

    #[test]
    fn cross_group_targets_are_dropped_from_the_runtime_snapshot() {
        let config = build(
            1,
            vec![group("g1", "d1"), group("g2", "d2")],
            vec![account("a2", "g2", 50)],
            vec![model("m1", "g1", "model")],
            vec![target("t1", "m1", "a2", Some(100))],
        );

        assert!(config.groups[0].models["model"].targets.is_empty());
    }

    #[test]
    fn key_digest_lookup_finds_the_owning_group() {
        let config = build(
            1,
            vec![group("g1", "digest-a"), group("g2", "digest-b")],
            vec![],
            vec![],
            vec![],
        );
        assert_eq!(
            config.group_by_key_digest("digest-b").unwrap().group.id,
            "g2"
        );
        assert!(config.group_by_key_digest("digest-missing").is_none());
    }

    #[test]
    fn models_never_leak_across_groups() {
        let config = build(
            1,
            vec![group("g1", "d1"), group("g2", "d2")],
            vec![account("a1", "g1", 50)],
            vec![model("m1", "g1", "glm-4.6")],
            vec![target("t1", "m1", "a1", None)],
        );
        assert!(
            config
                .group_by_id("g1")
                .unwrap()
                .models
                .contains_key("glm-4.6")
        );
        assert!(config.group_by_id("g2").unwrap().models.is_empty());
    }

    #[test]
    fn targets_referencing_a_deleted_account_are_dropped() {
        let config = build(
            1,
            vec![group("g1", "d1")],
            vec![],
            vec![model("m1", "g1", "glm-4.6")],
            vec![target("t1", "m1", "已删除的账号", None)],
        );
        let model = &config.groups[0].models["glm-4.6"];
        assert!(model.targets.is_empty());
        assert!(!model.is_listable(), "零目标的逻辑模型不进入 /v1/models");
    }

    #[test]
    fn target_limits_override_the_account_defaults() {
        let mut account = account("a1", "g1", 50);
        account.limits = Limits {
            rpm: Some(600),
            tpm: Some(100_000),
            max_concurrency: Some(8),
        };
        let mut dispatch = target("t1", "m1", "a1", None);
        dispatch.limits = Limits {
            max_concurrency: Some(2),
            ..Limits::default()
        };
        let config = build(
            1,
            vec![group("g1", "d1")],
            vec![account],
            vec![model("m1", "g1", "glm-4.6")],
            vec![dispatch],
        );

        let limits = config.groups[0].models["glm-4.6"].targets[0].limits();
        assert_eq!(limits.max_concurrency, Some(2), "目标覆盖账号默认值");
        assert_eq!(limits.rpm, Some(600), "未覆盖的项继续继承账号");
    }

    #[test]
    fn live_ids_deduplicate_accounts_shared_by_several_models() {
        let config = build(
            1,
            vec![group("g1", "d1")],
            vec![account("a1", "g1", 50)],
            vec![model("m1", "g1", "glm-4.6"), model("m2", "g1", "glm-4.5")],
            vec![
                target("t1", "m1", "a1", None),
                target("t2", "m2", "a1", None),
            ],
        );
        let (accounts, targets) = config.live_ids();
        assert_eq!(accounts, vec!["a1".to_string()]);
        assert_eq!(targets.len(), 2);
    }
}
