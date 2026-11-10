//! 不可变配置快照与原子切换（§21）。
//!
//! 所有配置写操作只有一条路径 [`AppState::write_config`]：`BEGIN IMMEDIATE` →
//! 乐观锁检查 → 改行与调和 → 校验并装配候选快照 → 提交 → `ArcSwap` 一次替换。
//! 在途请求持有旧 `Arc`，因此不会读到一半新一半旧的配置。

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use arc_swap::ArcSwap;

use crate::app::AppState;
use crate::domain::{Account, DispatchTarget, Group, Limits, LogicalModel};
use crate::storage::{ConfigTx, Store};

/// 一个调度目标及其解析后的账号与有效优先级。
#[derive(Debug)]
pub struct TargetView {
    pub target: DispatchTarget,
    pub account: Arc<Account>,
    /// 有效优先级只来自账号默认人工优先级（§9.2 修订）。
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
    /// 主名是否对下游暴露。
    ///
    /// 账号打开"隐藏原始模型"后，没有下游模型名的目标不暴露主名；所有目标
    /// 都这样时，这个逻辑模型整体不可达。
    pub exposed: bool,
    /// 同一逻辑模型额外暴露给下游的名字。
    ///
    /// 一个上游模型设置了别名、且所属账号未隐藏原始模型时，上游真名会出现在
    /// 这里；多个账号把不同上游名归并到同一个下游模型名时，所有真名都会列出，
    /// 指向同一组目标（§16.4）。
    pub aliases: Vec<String>,
}

impl LogicalModelView {
    /// 是否应当出现在 `/v1/models` 中：启用、有目标且至少有一个暴露名。
    pub fn is_listable(&self) -> bool {
        self.model.enabled && self.exposed && !self.targets.is_empty()
    }

    /// 主名是否可达。零目标或整体被隐藏时返回 false。
    pub fn is_exposed(&self) -> bool {
        self.model.enabled && self.exposed
    }

    /// 该逻辑模型对外暴露的全部名字（主名 + 额外别名）。
    pub fn exposed_names(&self) -> Vec<String> {
        let mut names = Vec::with_capacity(self.aliases.len() + 1);
        names.push(self.model.name.clone());
        names.extend(self.aliases.iter().cloned());
        names
    }
}

/// 一个分组及其逻辑模型索引。
#[derive(Debug)]
pub struct GroupView {
    pub group: Group,
    /// 逻辑模型主名 → 视图。分组是调度硬边界，查找绝不跨组。
    pub models: HashMap<String, Arc<LogicalModelView>>,
}

impl GroupView {
    /// 按下游请求里的模型名找逻辑模型：先查主名，再查归并进来的上游别名。
    pub fn find_model(&self, name: &str) -> Option<&Arc<LogicalModelView>> {
        self.models.get(name).or_else(|| {
            self.models
                .values()
                .find(|model| model.aliases.iter().any(|alias| alias == name))
        })
    }
}

/// 一份完整、不可变的运行时配置。
#[derive(Debug, Default)]
pub struct RuntimeConfig {
    /// 配置修订号（§7.4）：管理端每次写入都会推进，持久化在库里，重启不归零。
    /// 后台托管同步改了目标也不动它——那不是"别的管理员改过配置"。
    pub version: u64,
    pub groups: Vec<Arc<GroupView>>,
}

impl RuntimeConfig {
    /// 按下游 Key 的摘要定位分组（§19.2、§7.2）。
    ///
    /// 用**定长常量时间扫描**而不是哈希查表。表里存的是 Key 的 HMAC 摘要而非
    /// Key 本身，所以即使旁路出摘要也无法反推出可用的 Key；但提前返回的比较会
    /// 泄漏"前几个字符猜对了"，而分组数量是个位数、每次比较 64 字节，这点代价
    /// 换来的是这个问题彻底消失。
    ///
    /// 注意：找到之后**不能 break**，否则耗时又和"第几个分组命中"相关。
    pub fn group_by_key_digest(&self, digest_hex: &str) -> Option<&Arc<GroupView>> {
        let candidate = digest_hex.as_bytes();
        let mut found: Option<&Arc<GroupView>> = None;
        for group in &self.groups {
            if crate::security::ct_eq(candidate, group.group.key_digest_hex.as_bytes()) {
                found = Some(group);
            }
        }
        found
    }

    pub fn group_by_id(&self, id: &str) -> Option<&Arc<GroupView>> {
        self.groups.iter().find(|g| g.group.id == id)
    }

    /// 按账号 ID 找它在这份配置里的一个目标（§14.9 的生图任务轮询用）。
    ///
    /// 轮询手里只有"当初接单的账号"，没有模型，因此走不了调度链路——按 ID
    /// 反查是唯一能把请求送回原账号的办法。账号被删或不再被任何分组引用时
    /// 返回 None，调用方按"任务不存在"处理。
    pub fn target_by_account(&self, account_id: &str) -> Option<&Arc<TargetView>> {
        self.groups
            .iter()
            .flat_map(|group| group.models.values())
            .flat_map(|model| model.targets.iter())
            .find(|target| target.account.id == account_id)
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

/// 一次配置写入要不要过乐观锁、要不要推进修订号（§7.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Revision {
    /// 管理端：必须与当前修订号一致，提交时推进。
    Expect(u64),
    /// 管理端显式声明"无条件写入"（请求头 `*`，语义同 HTTP `If-Match: *`），
    /// 给脚本用。不做比较，但仍然推进，别的标签页照样会被拦下。
    Any,
    /// 后台任务（托管同步、启动调和）：不比较也不推进。它们不是"别的管理员"，
    /// 推进版本只会让正在编辑表单的管理员收到误报。
    Background,
}

/// 配置写入的失败。
#[derive(Debug)]
pub enum ConfigWriteError {
    /// 客户端手上的修订号已经过期。
    Conflict {
        expected: u64,
        current: u64,
    },
    Internal(anyhow::Error),
}

impl std::fmt::Display for ConfigWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict { expected, current } => write!(
                f,
                "config_conflict: 配置已被其他修改更新（你的版本 {expected}，当前 {current}），\
                 请重新加载后再保存，避免覆盖别人的修改。"
            ),
            Self::Internal(error) => write!(f, "{error:#}"),
        }
    }
}

impl std::error::Error for ConfigWriteError {}

impl From<anyhow::Error> for ConfigWriteError {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

/// 持有当前配置快照，并负责从数据库整体重建。
pub struct ConfigService {
    store: Store,
    current: ArcSwap<RuntimeConfig>,
    /// 串行化所有"读库 → 装配 → 切换快照"。没有它，两次重载可能乱序完成，
    /// 旧快照盖掉新快照。写事务全程持有，所以写与重载也不会交错。
    switch: tokio::sync::Mutex<()>,
}

impl ConfigService {
    /// 从数据库加载首个快照。
    pub async fn load(store: Store) -> Result<Self> {
        let service = Self {
            store,
            current: ArcSwap::from_pointee(RuntimeConfig::default()),
            switch: tokio::sync::Mutex::new(()),
        };
        service.reload().await?;
        Ok(service)
    }

    /// 当前快照。热路径上只做一次原子指针读取（§19.4）。
    pub fn current(&self) -> arc_swap::Guard<Arc<RuntimeConfig>> {
        self.current.load()
    }

    pub fn version(&self) -> u64 {
        self.current.load().version
    }

    /// 重新读取全部配置并原子替换当前快照。
    ///
    /// 四张表与修订号在同一个读事务里读：分开读可能拼出"账号是新的、目标是
    /// 旧的"这种从未存在过的配置。
    pub async fn reload(&self) -> Result<Arc<RuntimeConfig>> {
        let _switch = self.switch.lock().await;
        let mut tx = self.store.begin_config().await?;
        let candidate = assemble(&mut tx).await?;
        let revision = tx.revision().await?;
        tx.commit().await?;
        Ok(self.install(revision, candidate))
    }

    fn install(&self, revision: u64, mut config: RuntimeConfig) -> Arc<RuntimeConfig> {
        config.version = revision;
        let config = Arc::new(config);
        self.current.store(Arc::clone(&config));
        config
    }
}

/// 在事务内读四张表并装配（尚未校验、版本号未定）。
async fn assemble(tx: &mut ConfigTx) -> Result<RuntimeConfig> {
    let groups = tx.list_groups().await?;
    let accounts = tx.list_accounts().await?;
    let models = tx.list_logical_models().await?;
    let targets = tx.list_targets().await?;
    Ok(build(0, groups, accounts, models, targets))
}

/// 候选配置校验：目标必须与账号同组。[`build`] 对这类行是静默丢弃（保护热
/// 路径），写路径则要拒绝——否则一次保存会悄悄丢掉一批目标。
async fn validate(tx: &mut ConfigTx) -> Result<()> {
    let accounts = tx.list_accounts().await?;
    let models = tx.list_logical_models().await?;
    let groups_of_models: HashMap<&str, &str> = models
        .iter()
        .map(|model| (model.id.as_str(), model.group_id.as_str()))
        .collect();
    let groups_of_accounts: HashMap<&str, Option<&str>> = accounts
        .iter()
        .map(|account| (account.id.as_str(), account.group_id.as_deref()))
        .collect();
    for target in tx.list_targets().await? {
        let account_group = groups_of_accounts
            .get(target.account_id.as_str())
            .copied()
            .flatten();
        let model_group = groups_of_models
            .get(target.logical_model_id.as_str())
            .copied();
        if account_group.is_none() || account_group != model_group {
            anyhow::bail!("调度目标 {} 与账号不在同一分组，已放弃这次保存", target.id);
        }
    }
    Ok(())
}

impl AppState {
    /// 配置写入的唯一路径（§21）。
    ///
    /// `apply` 在 `BEGIN IMMEDIATE` 事务里运行，只能通过传入的 [`ConfigTx`] 读写
    /// ——任何网络请求都要放到调用之前。返回 `Err` 时整体回滚。成功后切换快照、
    /// 重建 Key 池并同步动态状态，全程只切换一次。
    pub async fn write_config<T, E>(
        &self,
        revision: Revision,
        apply: impl AsyncFnOnce(&mut ConfigTx) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<ConfigWriteError>,
    {
        let internal = |error: anyhow::Error| E::from(ConfigWriteError::Internal(error));
        let _switch = self.config.switch.lock().await;
        let mut tx = self.store.begin_config().await.map_err(internal)?;
        let current = tx.revision().await.map_err(internal)?;
        if let Revision::Expect(expected) = revision
            && expected != current
        {
            return Err(E::from(ConfigWriteError::Conflict { expected, current }));
        }
        let value = apply(&mut tx).await?;
        let next = match revision {
            Revision::Background => current,
            Revision::Expect(_) | Revision::Any => {
                tx.set_revision(current + 1).await.map_err(internal)?;
                current + 1
            }
        };
        validate(&mut tx).await.map_err(internal)?;
        let candidate = assemble(&mut tx).await.map_err(internal)?;
        tx.commit().await.map_err(internal)?;
        let config = self.config.install(next, candidate);
        drop(_switch);
        // 这几步都读提交后的最新状态，不怕乱序；已提交的写入不能因此报失败。
        if let Err(error) = self.after_config_switch(&config).await {
            tracing::error!(error = %format!("{error:#}"), "配置已保存，但同步运行时状态失败");
        }
        Ok(value)
    }

    async fn after_config_switch(&self, config: &RuntimeConfig) -> Result<()> {
        self.reload_credentials().await.context("重建 Key 池失败")?;
        self.runtime.retain(config);
        self.reseed_multipliers().await.context("重建倍率表失败")
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

    let model_names: HashMap<String, String> = models
        .iter()
        .map(|model| (model.id.clone(), model.name.clone()))
        .collect();
    let mut targets_by_model: HashMap<String, Vec<Arc<TargetView>>> = HashMap::new();
    let mut aliases_by_model: HashMap<String, Vec<String>> = HashMap::new();
    let mut exposed_by_model: HashMap<String, bool> = HashMap::new();
    for target in targets {
        // 账号可能刚被删除；引用不到账号的目标直接丢弃，不能让它进入调度。
        let Some(account) = accounts.get(&target.account_id).cloned() else {
            continue;
        };
        // 未分配账号不进调度：它没有分组，也就不属于任何下游 Key 的能力
        // 范围（§4.2.3）。正常路径下它连调度目标都没有，这里再挡一次是
        // 为了兜住"先建目标、后取消分配"这类中途状态。
        let Some(account_group) = account.group_id.as_deref() else {
            continue;
        };
        // 逻辑模型与账号必须属于同一分组；跨组目标直接丢弃，避免绕过
        // 下游 Key 的分组边界（正常管理 API 也会拒绝此类写入）。
        let Some(model_group) = model_groups.get(target.logical_model_id.as_str()) else {
            continue;
        };
        if account_group != *model_group {
            continue;
        }
        if let Some(model_name) = model_names.get(target.logical_model_id.as_str()) {
            // 设置了别名时，这个目标至少能以下游模型名暴露；没有别名又打开
            // 账号级"隐藏原始模型"时，该行整体不可见（§16.4 修订）。
            let aliased = target.upstream_model.as_str() != model_name;
            if !account.hide_original || aliased {
                exposed_by_model
                    .entry(target.logical_model_id.clone())
                    .or_insert(true);
            }
            // 未隐藏原始名的账号，额外暴露上游真名作为入口。
            if !account.hide_original && target.enabled && account.enabled && aliased {
                aliases_by_model
                    .entry(target.logical_model_id.clone())
                    .or_default()
                    .push(target.upstream_model.clone());
            }
        }
        // 调度目标不再有独立优先级：统一继承账号默认人工优先级（§9.2 修订）。
        let priority = account.default_priority;
        targets_by_model
            .entry(target.logical_model_id.clone())
            .or_default()
            .push(Arc::new(TargetView {
                target,
                account,
                priority,
            }));
    }

    // 同名冲突时主名优先，其次先创建的逻辑模型优先；避免一个别名覆盖掉
    // 另一个真正的逻辑模型。
    let mut claimed: std::collections::HashSet<String> =
        models.iter().map(|model| model.name.clone()).collect();
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
        let mut aliases = aliases_by_model.remove(&model.id).unwrap_or_default();
        aliases.sort();
        aliases.dedup();
        aliases.retain(|alias| claimed.insert(alias.clone()));
        let exposed = exposed_by_model.remove(&model.id).unwrap_or(false);
        models_by_group
            .entry(model.group_id.clone())
            .or_default()
            .insert(
                model.name.clone(),
                Arc::new(LogicalModelView {
                    model,
                    targets,
                    exposed,
                    aliases,
                }),
            );
    }

    let mut views = Vec::with_capacity(groups.len());
    for group in groups {
        let models = models_by_group.remove(&group.id).unwrap_or_default();
        views.push(Arc::new(GroupView { group, models }));
    }

    RuntimeConfig {
        version,
        groups: views,
    }
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;

    use super::*;
    use crate::domain::{Multiplier, MultiplierMode, Protocol, SchedulingWeights};

    fn group(id: &str, digest: &str) -> Group {
        Group {
            id: id.into(),
            name: id.into(),
            key_prefix: "akh-000000".into(),
            key_digest_hex: digest.into(),
            multiplier_limit: Multiplier::ONE,
            weights: SchedulingWeights::default(),
            queue_capacity: crate::domain::QUEUE_UNLIMITED,
            max_wait_secs: 60,
            allow_managed_background: false,
            allow_degrade: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn account(id: &str, group_id: &str, priority: i32) -> Account {
        Account {
            id: id.into(),
            group_id: Some(group_id.into()),
            name: id.into(),
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
            hide_original: false,
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
            enabled: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn target(id: &str, model_id: &str, account_id: &str) -> DispatchTarget {
        DispatchTarget {
            id: id.into(),
            logical_model_id: model_id.into(),
            account_id: account_id.into(),
            upstream_model: "glm-4.6".into(),
            limits: Limits::default(),
            enabled: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn targets_are_sorted_by_account_priority() {
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
                target("t1", "m1", "a1"),
                target("t2", "m1", "a2"),
                target("t3", "m1", "a3"),
            ],
        );

        let model = &config.groups[0].models["glm-4.6"];
        let priorities: Vec<_> = model.targets.iter().map(|t| t.priority).collect();
        assert_eq!(priorities, vec![100, 60, 30]);
    }

    #[test]
    fn upstream_names_are_exposed_as_aliases_unless_hidden() {
        let mut visible = target("t1", "m1", "a1");
        visible.upstream_model = "gpt-5.6-sol-openai".into();
        let mut hidden = target("t2", "m1", "a2");
        hidden.upstream_model = "gpt-5.6-sol-azure".into();
        let mut hidden_account = account("a2", "g1", 0);
        hidden_account.hide_original = true;

        let config = build(
            1,
            vec![group("g1", "d1")],
            vec![account("a1", "g1", 0), hidden_account],
            vec![model("m1", "g1", "gpt-5.6-sol")],
            vec![visible, hidden],
        );

        let group = &config.groups[0];
        let view = &group.models["gpt-5.6-sol"];
        assert_eq!(
            view.exposed_names(),
            vec!["gpt-5.6-sol".to_string(), "gpt-5.6-sol-openai".to_string()]
        );
        assert!(group.find_model("gpt-5.6-sol-openai").is_some());
        assert!(group.find_model("gpt-5.6-sol-azure").is_none());
    }

    #[test]
    fn cross_group_targets_are_dropped_from_the_runtime_snapshot() {
        let config = build(
            1,
            vec![group("g1", "d1"), group("g2", "d2")],
            vec![account("a2", "g2", 50)],
            vec![model("m1", "g1", "model")],
            vec![target("t1", "m1", "a2")],
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
            vec![target("t1", "m1", "a1")],
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
            vec![target("t1", "m1", "已删除的账号")],
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
        let mut dispatch = target("t1", "m1", "a1");
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
            vec![target("t1", "m1", "a1"), target("t2", "m2", "a1")],
        );
        let (accounts, targets) = config.live_ids();
        assert_eq!(accounts, vec!["a1".to_string()]);
        assert_eq!(targets.len(), 2);
    }
}
