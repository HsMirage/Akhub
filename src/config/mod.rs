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

    /// 两份快照是不是**同一份配置**（§7.4 的乐观锁版本语义）。
    ///
    /// 判据是"下游调度能看到的东西有没有变"，而不是结构体的逐字节相等：
    ///
    /// * 分组：ID 与全部调度相关字段（倍率上限、权重、队列、降级开关）；
    /// * 逻辑模型：ID、对外名、启用、来源；
    /// * 目标：ID、所属模型、账号、上游模型名、启用、隐藏原始名、限制；
    /// * 账号：ID、所属分组、启用、优先级、倍率配置、限制、隐藏原始名。
    ///
    /// 刻意**不**比较的字段是那些"与调度无关、却会被后台任务高频改写"的：
    /// `Account::model_synced_at`（每次托管同步都会 touch）、`created_at`、
    /// Key 池与凭据密文（凭据是热路径状态，不进配置版本）。把它们算进来，
    /// 后台同步就又会推进版本，误报原样复现——这正是本函数存在的理由。
    ///
    /// 顺序敏感：`build` 对分组、模型、目标都按固定顺序装配，两边都来自同一
    /// 段代码，所以直接逐项比较即可，不必排序。
    pub fn same_config(&self, other: &RuntimeConfig) -> bool {
        if self.groups.len() != other.groups.len() {
            return false;
        }
        self.groups
            .iter()
            .zip(other.groups.iter())
            .all(|(a, b)| same_group(a, b))
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

/// 两份分组视图是不是同一份配置（见 `RuntimeConfig::same_config`）。
fn same_group(a: &GroupView, b: &GroupView) -> bool {
    same_group_row(&a.group, &b.group)
        && a.models.len() == b.models.len()
        && a.models.iter().all(|(name, left)| {
            b.models
                .get(name)
                .is_some_and(|right| same_model(left, right))
        })
}

/// 分组是不是同一份配置。
///
/// 用领域类型的派生 `PartialEq` **整体**比较，而不是手写字段清单。手写清单
/// 曾经漏掉 `key_digest_hex`，于是"重新生成下游 Key"不推进版本、也不替换
/// 快照，旧 Key 继续可用——全量回归里 `regenerating_a_key_invalidates_the_previous_one`
/// 正是这么抓到的。结构性比较没有这个失败模式：领域类型新增字段时它自动覆盖。
///
/// `created_at` 参与比较是安全的：它只在插入时写一次，之后不会变。
fn same_group_row(a: &Group, b: &Group) -> bool {
    a == b
}

fn same_model(a: &LogicalModelView, b: &LogicalModelView) -> bool {
    a.model == b.model
        && a.exposed == b.exposed
        && a.aliases == b.aliases
        && a.targets.len() == b.targets.len()
        && a.targets
            .iter()
            .zip(b.targets.iter())
            .all(|(left, right)| same_target(left, right))
}

fn same_target(a: &TargetView, b: &TargetView) -> bool {
    a.target == b.target && a.priority == b.priority && same_account(&a.account, &b.account)
}

/// 账号是不是同一份配置。
///
/// 唯一被刻意忽略的字段是 `model_synced_at`：后台托管同步每轮都会 touch 它
/// （`sync_managed` 结尾的 `touch_model_sync`），而它与"下游能看到什么、
/// 请求会被调度到哪"毫无关系。把它算进配置版本，管理员每次保存都会撞上
/// 与自己的编辑无关的冲突提示——这正是要修的那个误报。
///
/// 其余字段一律整体比较（凭据与 Key 池不在 `Account` 里，它们是
/// `CredentialPool`，本来就不参与配置版本）。
fn same_account(a: &Account, b: &Account) -> bool {
    if a.model_synced_at == b.model_synced_at {
        return a == b;
    }
    // 只差这个时间戳时才有必要克隆一次；账号数量是个位数到几十个，
    // 这点开销远小于上面四次数据库查询。
    let mut left = a.clone();
    let mut right = b.clone();
    left.model_synced_at = None;
    right.model_synced_at = None;
    left == right
}

/// 持有当前配置快照，并负责从数据库整体重建。
pub struct ConfigService {
    store: Store,
    current: ArcSwap<RuntimeConfig>,
    version: AtomicU64,
}

impl ConfigService {
    /// 从数据库加载首个快照。
    ///
    /// 计数器从 **1** 起步，于是首个快照的版本就是 1，而不是 0。
    ///
    /// 这不是审美问题：客户端把 0 当作"还没读到过版本"，因而**不带**乐观锁头
    /// （见 web/src/lib/api.ts 的 `configVersion`，以及 §7.4 的兼容模式约定）。
    /// 若首启停在 0，管理员在第一次配置变更之前保存时，锁是静默关闭的。
    /// 从 1 起步还顺带对上客户端注释里"服务重启会将版本重置为 1"那条既有约定。
    pub async fn load(store: Store) -> Result<Self> {
        let service = Self {
            store,
            current: ArcSwap::from_pointee(RuntimeConfig::default()),
            version: AtomicU64::new(1),
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
    ///
    /// **版本号只在配置内容真的变了时才前进**（2026-10-15 修订）。
    ///
    /// 旧实现无条件 `fetch_add`：任何一次 `reload_config` 都会推进版本，而
    /// `reload_config` 的调用点远不止"管理员改配置"——后台的托管模型同步
    /// （`tasks::model_sync`，默认 30 分钟一轮、每轮都会 reconcile 一次）、
    /// 账号变更后的目录兜底调和都会走到这里。于是管理员在表单里刚填完内容，
    /// 一次与他的编辑毫无关系的后台同步就把他手上的版本号顶掉了，点保存必
    /// 409，弹出"配置已被其他会话修改，另一个标签页或管理员刚刚修改了配置"。
    /// 现场从来没有第二个人在操作——这是纯粹的误报。
    ///
    /// 乐观锁要防的是"两个会话各改一半、后保存的覆盖先保存的"，所以版本号
    /// 的语义必须是**配置内容的世代**，而不是"服务端重建快照的次数"。内容
    /// 没变时保留旧版本号，客户端手上的版本就仍然有效，保存不再无谓地失败。
    pub async fn reload(&self) -> Result<Arc<RuntimeConfig>> {
        let groups = self.store.list_groups().await?;
        let accounts = self.store.list_accounts().await?;
        let models = self.store.list_logical_models().await?;
        let targets = self.store.list_targets().await?;

        // 先按"当前世代"装配一份候选快照，再与在用的那一份比内容。比的是
        // 装配结果（分组、模型、目标、账号），不是数据库行的写入次数：只有
        // 真正影响调度的差异才配得上一个新版本号。
        let current = self.current.load();
        let candidate = build(current.version, groups, accounts, models, targets);
        // 首个快照：在用的那份还是 `RuntimeConfig::default()`（version 0），
        // 而计数器的起点是 1。直接采纳 1，而不是把它当成一次"内容变化"再 +1——
        // 装载不该消耗一个版本号。
        if current.version == 0 {
            let version = self.version.load(Ordering::Acquire);
            let config = Arc::new(RuntimeConfig {
                version,
                groups: candidate.groups,
            });
            self.current.store(Arc::clone(&config));
            return Ok(config);
        }
        if current.same_config(&candidate) {
            // 内容一致：保留旧快照与旧版本号，也不 store——在途请求手上那份
            // Arc 继续有效，客户端手上的版本号也仍然对得上。
            let snapshot: Arc<RuntimeConfig> = Arc::clone(&current);
            drop(current);
            return Ok(snapshot);
        }
        drop(current);

        // 内容真的变了才推进版本。候选快照的 groups 直接搬过来复用，不重装配。
        //
        // `fetch_add` 从 1 起步而不是从 0：客户端把 0 当作"还没读到过版本"
        // （见 web/src/lib/api.ts 的 configVersion），于是它**不会**带乐观锁头。
        // 空库首启时如果停在 0，管理员第一次保存前锁都是关着的——那是静默失效。
        // 从 1 起步还顺带对上"重启后版本从 1 开始"这条既有约定。
        let version = self.version.fetch_add(1, Ordering::AcqRel) + 1;
        let config = Arc::new(RuntimeConfig {
            version,
            groups: candidate.groups,
        });
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
    use crate::domain::{ModelOrigin, Multiplier, MultiplierMode, Protocol, SchedulingWeights};

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
            hide_original: false,
            priority_override: override_priority,
            limits: Limits::default(),
            enabled: true,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// 配置内容没变时，`same_config` 必须为真——这是"版本号只在内容真变时前进"
    /// 的判据，也是"配置已被其他会话修改"不再误报的根。
    #[test]
    fn identical_content_counts_as_the_same_config() {
        let build_once = || {
            build(
                1,
                vec![group("g1", "d1")],
                vec![account("a1", "g1", 50)],
                vec![model("m1", "g1", "glm-4.6")],
                vec![target("t1", "m1", "a1", None)],
            )
        };
        assert!(build_once().same_config(&build_once()));
    }

    /// 后台托管同步每轮都会 `touch_model_sync`。它改的是 `model_synced_at`，
    /// 与调度无关——把它算进配置版本，管理员保存时就会收到与自己的编辑毫无
    /// 关系的"配置已被其他会话修改"（现场症状就是这个）。
    #[test]
    fn touching_the_sync_timestamp_is_not_a_config_change() {
        let before = build(
            1,
            vec![group("g1", "d1")],
            vec![account("a1", "g1", 50)],
            vec![model("m1", "g1", "glm-4.6")],
            vec![target("t1", "m1", "a1", None)],
        );
        let mut synced_account = account("a1", "g1", 50);
        synced_account.model_synced_at = Some(1_700_000_000);
        let after = build(
            1,
            vec![group("g1", "d1")],
            vec![synced_account],
            vec![model("m1", "g1", "glm-4.6")],
            vec![target("t1", "m1", "a1", None)],
        );
        assert!(
            before.same_config(&after),
            "托管同步只是记了一次时间戳，不该让客户端手上的版本号失效"
        );
    }

    /// 真正影响调度的改动必须被认出来，否则乐观锁就形同虚设。
    #[test]
    fn real_dispatch_changes_are_detected() {
        let base = build(
            1,
            vec![group("g1", "d1")],
            vec![account("a1", "g1", 50)],
            vec![model("m1", "g1", "glm-4.6")],
            vec![target("t1", "m1", "a1", None)],
        );

        // 账号停用。
        let mut disabled = account("a1", "g1", 50);
        disabled.enabled = false;
        let disabled = build(
            1,
            vec![group("g1", "d1")],
            vec![disabled],
            vec![model("m1", "g1", "glm-4.6")],
            vec![target("t1", "m1", "a1", None)],
        );
        assert!(!base.same_config(&disabled), "账号停用必须算配置变化");

        // 目标被撤下。
        let without_target = build(
            1,
            vec![group("g1", "d1")],
            vec![account("a1", "g1", 50)],
            vec![model("m1", "g1", "glm-4.6")],
            vec![],
        );
        assert!(!base.same_config(&without_target), "目标消失必须算配置变化");

        // 分组倍率上限变了。
        let mut tighter = group("g1", "d1");
        tighter.multiplier_limit = Multiplier::from_raw(500_000);
        let tighter = build(
            1,
            vec![tighter],
            vec![account("a1", "g1", 50)],
            vec![model("m1", "g1", "glm-4.6")],
            vec![target("t1", "m1", "a1", None)],
        );
        assert!(!base.same_config(&tighter), "分组上限变了必须算配置变化");
    }

    #[test]
    fn targets_are_sorted_by_account_priority_and_ignore_legacy_overrides() {
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
                // 旧库里的目标优先级覆盖不再生效：a3 仍按账号的 60 排。
                target("t3", "m1", "a3", Some(10)),
            ],
        );

        let model = &config.groups[0].models["glm-4.6"];
        let priorities: Vec<_> = model.targets.iter().map(|t| t.priority).collect();
        assert_eq!(priorities, vec![100, 60, 30]);
    }

    #[test]
    fn upstream_names_are_exposed_as_aliases_unless_hidden() {
        let mut visible = target("t1", "m1", "a1", None);
        visible.upstream_model = "gpt-5.6-sol-openai".into();
        let mut hidden = target("t2", "m1", "a2", None);
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
