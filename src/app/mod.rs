//! 进程共享状态、后台任务与优雅关闭（§19.1、§22、§25.3）。

pub mod live_stats;
pub mod recorder;
pub mod tasks;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::auth::session::SessionStore;
use crate::capability;
use crate::config::ConfigService;
use crate::health;
use crate::multiplier;
use crate::routing::{queue, score, sticky};
use crate::security::{Cipher, KeyDigest, MasterKey};
use crate::storage::Store;
use crate::upstream::UpstreamClient;
use crate::upstream::evidence::Evidence;

pub use recorder::RequestRecorder;

/// 系统设置。第一期只保留 §6.7 中真正需要的几项。
#[derive(Debug, Clone)]
pub struct Settings {
    /// 请求总超时，达到后取消上游请求且不再重放（§13.5）。
    pub request_timeout: Duration,
    /// 单请求体上限，超过返回 `413 request_too_large`（§17.3）。
    pub max_request_bytes: usize,
    /// 请求元数据保留天数；0 表示不新增历史明细（§24.2）。
    pub retention_days: u32,
    /// Responses 状态链保留天数（§15.2）。0 表示不保存可重放正文，
    /// 只保留完成原生生命周期所需的最小定位映射。
    pub response_state_days: u32,
    /// 在途请求在关闭时的最长完成时间（§25.3）。
    pub shutdown_grace: Duration,
    /// 自动倍率刷新间隔（§11.3）。
    pub multiplier_refresh: Duration,
    /// 模型自动同步间隔（§16.2）。账号各自叠加 0–10% 抖动。
    pub model_sync: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(600),
            max_request_bytes: 64 * 1024 * 1024,
            retention_days: 30,
            response_state_days: 30,
            shutdown_grace: Duration::from_secs(180),
            multiplier_refresh: Duration::from_secs(300),
            model_sync: Duration::from_secs(1800),
        }
    }
}

/// 设置持久化用的 JSON 形状：只保存"后台能改"的字段，全部可选。
///
/// 启动时以环境变量/默认值为底，再把数据库里的覆盖项盖上去；这样以后新增
/// 设置项时，旧的持久化文件不会因为缺字段而失效。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct PersistedSettings {
    pub request_timeout_secs: Option<u64>,
    pub max_request_bytes: Option<usize>,
    pub retention_days: Option<u32>,
    pub response_state_days: Option<u32>,
    pub shutdown_grace_secs: Option<u64>,
    pub multiplier_refresh_secs: Option<u64>,
    pub model_sync_secs: Option<u64>,
}

impl PersistedSettings {
    /// 用持久化值覆盖底值。
    pub fn apply_to(&self, base: Settings) -> Settings {
        Settings {
            request_timeout: self
                .request_timeout_secs
                .map(Duration::from_secs)
                .unwrap_or(base.request_timeout),
            max_request_bytes: self.max_request_bytes.unwrap_or(base.max_request_bytes),
            retention_days: self.retention_days.unwrap_or(base.retention_days),
            response_state_days: self.response_state_days.unwrap_or(base.response_state_days),
            shutdown_grace: self
                .shutdown_grace_secs
                .map(Duration::from_secs)
                .unwrap_or(base.shutdown_grace),
            multiplier_refresh: self
                .multiplier_refresh_secs
                .map(Duration::from_secs)
                .unwrap_or(base.multiplier_refresh),
            model_sync: self
                .model_sync_secs
                .map(Duration::from_secs)
                .unwrap_or(base.model_sync),
        }
    }

    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            request_timeout_secs: Some(settings.request_timeout.as_secs()),
            max_request_bytes: Some(settings.max_request_bytes),
            retention_days: Some(settings.retention_days),
            response_state_days: Some(settings.response_state_days),
            shutdown_grace_secs: Some(settings.shutdown_grace.as_secs()),
            multiplier_refresh_secs: Some(settings.multiplier_refresh.as_secs()),
            model_sync_secs: Some(settings.model_sync.as_secs()),
        }
    }

    /// 逐字段校验；越界时报出字段名与允许范围。
    pub fn validate(&self) -> Result<()> {
        let range = |name: &str, value: Option<u64>, low: u64, high: u64| -> Result<()> {
            match value {
                Some(value) if !(low..=high).contains(&value) => {
                    anyhow::bail!("{name} 必须在 {low}–{high} 之间，收到 {value}")
                }
                _ => Ok(()),
            }
        };
        range("请求总超时（秒）", self.request_timeout_secs, 5, 86_400)?;
        range("关闭宽限期（秒）", self.shutdown_grace_secs, 5, 3600)?;
        range(
            "自动倍率刷新间隔（秒）",
            self.multiplier_refresh_secs,
            30,
            86_400,
        )?;
        range("模型同步间隔（秒）", self.model_sync_secs, 60, 86_400)?;
        range(
            "请求元数据保留（天）",
            self.retention_days.map(u64::from),
            0,
            3650,
        )?;
        range(
            "Responses 状态保留（天）",
            self.response_state_days.map(u64::from),
            0,
            3650,
        )?;
        if let Some(bytes) = self.max_request_bytes
            && !(1024..=256 * 1024 * 1024).contains(&bytes)
        {
            anyhow::bail!("请求体上限必须在 1 KiB–256 MiB 之间，收到 {bytes} 字节");
        }
        Ok(())
    }
}

/// 应用级设置的持久化键。
pub const SETTINGS_KEY: &str = "settings";

/// 运行时可改的系统设置。
///
/// 热路径一次原子指针读取；后台保存后立刻对所有请求生效，不需要重启
/// （`shutdown_grace` 例外：它在进程启动时读取，下一次重启生效）。
#[derive(Debug, Clone)]
pub struct SettingsService {
    current: std::sync::Arc<arc_swap::ArcSwap<Settings>>,
}

impl SettingsService {
    pub fn new(current: Settings) -> Self {
        Self {
            current: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(current)),
        }
    }

    /// 当前设置快照。调用方应只在一次请求/一轮任务内使用同一份快照。
    pub fn get(&self) -> Arc<Settings> {
        self.current.load_full()
    }

    /// 覆盖设置并返回新快照。
    pub fn replace(&self, next: Settings) -> Arc<Settings> {
        let next = Arc::new(next);
        self.current.store(Arc::clone(&next));
        next
    }
}

/// 全部动态运行状态。
///
/// 与 [`crate::config::RuntimeConfig`] 并列而不是嵌进去：倍率、熔断、并发和
/// 额度不等待配置版本，每次真正发请求前都要读最新值（§21）。
pub struct Runtime {
    /// 熔断、冷却、半开、并发与限流。
    pub health: health::Registry,
    /// 性能 EWMA。
    pub perf: score::Registry,
    /// 会话粘性绑定。
    pub sticky: sticky::Bindings,
    /// 倍率状态与宽限期。
    pub multipliers: Arc<multiplier::Registry>,
    /// 分组级排队总容量。
    pub queues: queue::GroupQueues,
    /// 端点能力证据：已证实不存在的上游路由（§14.2、§16.7）。
    pub evidence: Evidence,
    /// 模型能力限制：已证实不支持某能力的账号模型（§16.7）。
    pub capabilities: capability::Capabilities,
    /// 关闭信号：置位后排队中的请求立即取消并返回可重试错误（§25.3 第 2 步）。
    shutdown: tokio::sync::watch::Sender<bool>,
    /// 当前在途请求数（§6.2 概览指标）。计数绑定在响应体上，流式请求直到
    /// 连接结束才算完成。
    in_flight: Arc<std::sync::atomic::AtomicU64>,
    /// 正在执行的托管后台任务（计划 §29.1）：保存句柄才能做到"真取消"。
    pub background: crate::gateway::background::RunningTasks,
    /// 保留期为 0 时的内存实时汇总（§3、§24.2）。与请求记录器共享同一个实例。
    pub live: Arc<live_stats::LiveStats>,
}

/// 在途计数守卫：随响应体一起析构，客户端断开也会准确 -1。
struct InFlightGuard(Arc<std::sync::atomic::AtomicU64>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            health: Default::default(),
            perf: Default::default(),
            sticky: Default::default(),
            multipliers: Default::default(),
            queues: Default::default(),
            evidence: Default::default(),
            capabilities: Default::default(),
            shutdown: tokio::sync::watch::channel(false).0,
            in_flight: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            background: crate::gateway::background::RunningTasks::new(),
            live: Arc::new(live_stats::LiveStats::new()),
        }
    }
}

impl Runtime {
    /// 收到停止信号：置位关闭标志，唤醒所有排队中的请求。
    pub fn begin_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    /// 是否已经进入关闭流程。
    pub fn is_shutting_down(&self) -> bool {
        *self.shutdown.borrow()
    }

    /// 订阅关闭信号；等待容量时与它一起 `select`。
    pub fn subscribe_shutdown(&self) -> tokio::sync::watch::Receiver<bool> {
        self.shutdown.subscribe()
    }

    /// 当前在途请求数。
    pub fn in_flight(&self) -> u64 {
        self.in_flight.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 把在途计数绑到响应体的生命周期上（§6.2）。
    ///
    /// 流式请求从进入网关一直算到连接结束；客户端中途断开时守卫随流一起析构，
    /// 计数不会泄漏。
    pub fn track_response(&self, response: axum::response::Response) -> axum::response::Response {
        use futures::StreamExt as _;

        self.in_flight
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let guard = InFlightGuard(Arc::clone(&self.in_flight));
        let (parts, body) = response.into_parts();
        let stream = async_stream::stream! {
            let _guard = guard;
            let mut body = body.into_data_stream();
            while let Some(item) = body.next().await {
                yield item;
            }
        };
        axum::response::Response::from_parts(parts, axum::body::Body::from_stream(stream))
    }
}

impl Runtime {
    /// 丢弃已经不在配置里的账号、目标与分组，避免状态表随改配置无限增长。
    pub fn retain(&self, config: &crate::config::RuntimeConfig) {
        let (accounts, targets) = config.live_ids();
        self.health.retain(&accounts, &targets);
        self.perf.retain(&targets);
        self.queues.retain(
            &config
                .groups
                .iter()
                .map(|g| g.group.id.clone())
                .collect::<Vec<_>>(),
        );
        // 账号的协议设置、模型列表或适配器版本可能刚被改过，旧的端点与能力
        // 证据前提已经不成立（§16.7）。
        self.evidence.clear();
        self.capabilities.clear();
    }
}

/// 全进程共享状态。所有字段要么不可变，要么内部可变且线程安全。
pub struct AppState {
    pub config: ConfigService,
    pub store: Store,
    pub cipher: Cipher,
    pub key_digest: KeyDigest,
    pub upstream: UpstreamClient,
    pub sessions: SessionStore,
    /// 运行时可改的系统设置（后台保存后热生效）。
    pub settings: SettingsService,
    pub recorder: RequestRecorder,
    pub runtime: Runtime,
    /// 倍率刷新任务的句柄，供后台的"立即刷新"按钮插队（§11.3）。
    pub refresh: tasks::RefreshHandle,
    /// 主密钥来自环境变量，供后台展示密钥状态。
    pub master_key_from_env: bool,
    /// 数据目录：大请求体的临时文件落在这里（§17.3、§19.4）。
    pub data_dir: std::path::PathBuf,
}

/// 大请求体的临时目录名（§1291、§1292）。
pub const TEMP_DIR_NAME: &str = "tmp";

pub type SharedState = Arc<AppState>;

impl AppState {
    /// 打开数据目录、加载主密钥与配置，装配出可服务的状态。
    ///
    /// 后台任务不在这里启动：由 [`tasks::spawn`] 单独拉起并只持有弱引用，
    /// 这样测试里用完即弃的 `AppState` 不会被一堆常驻任务钉住。
    pub async fn bootstrap(data_dir: &std::path::Path, settings: Settings) -> Result<SharedState> {
        let master_key = MasterKey::load_or_create(data_dir).context("加载主密钥失败")?;
        let pool = crate::storage::open(data_dir).await?;
        let store = Store::new(pool);
        let config = ConfigService::load(store.clone()).await?;
        let runtime = Runtime::default();
        restore(&store, &runtime).await?;

        // 后台保存过的设置覆盖环境变量/默认值；解析失败只告警，不让进程起不来。
        let settings = match store.app_setting(SETTINGS_KEY).await {
            Ok(Some(raw)) => match serde_json::from_str::<PersistedSettings>(&raw) {
                Ok(persisted) => persisted.apply_to(settings),
                Err(error) => {
                    tracing::warn!(%error, "持久化设置无法解析，回退到启动参数");
                    settings
                }
            },
            Ok(None) => settings,
            Err(error) => {
                tracing::warn!(%error, "读取持久化设置失败，回退到启动参数");
                settings
            }
        };

        // 上次进程遗留的托管后台任务：标记为 interrupted，绝不留假 in_progress
        // （计划 §29.1）。心跳超过 30 秒没推进的任务不可能是活的。
        match crate::gateway::background::recover_stale(&store, std::time::Duration::from_secs(30))
            .await
        {
            Ok(0) => {}
            Ok(count) => tracing::info!(count, "已把遗留的托管后台任务标记为中断"),
            Err(error) => tracing::warn!(%error, "恢复遗留托管任务失败"),
        }

        // 上次异常退出遗留的临时请求体：启动时清掉（§1291、§1292）。
        let temp_dir = data_dir.join(TEMP_DIR_NAME);
        if let Err(error) = std::fs::create_dir_all(&temp_dir) {
            tracing::warn!(%error, path = %temp_dir.display(), "创建临时目录失败");
        } else {
            sweep_stale_temp_files(&temp_dir);
        }

        // 请求记录器要看保留期决定"落库还是只进内存"，所以必须在设置服务之后建。
        let settings_service = SettingsService::new(settings);
        let recorder = RequestRecorder::spawn(
            store.clone(),
            settings_service.clone(),
            Arc::clone(&runtime.live),
        );

        let state = Arc::new(AppState {
            cipher: master_key.cipher(),
            key_digest: master_key.key_digest(),
            master_key_from_env: master_key.from_env,
            config,
            store,
            upstream: UpstreamClient::new().context("构造上游 HTTP 客户端失败")?,
            sessions: SessionStore::new(),
            settings: settings_service,
            recorder,
            runtime,
            refresh: tasks::RefreshHandle::default(),
            data_dir: data_dir.to_path_buf(),
        });
        Ok(state)
    }

    /// 配置写操作之后重建快照并同步动态状态表。
    pub async fn reload_config(&self) -> Result<()> {
        let config = self.config.reload().await?;
        self.runtime.retain(&config);
        self.reseed_multipliers().await
    }

    /// 用当前账号列表与持久化状态重建倍率表。
    pub async fn reseed_multipliers(&self) -> Result<()> {
        let accounts = self.store.list_accounts().await?;
        let snapshots = self.store.list_multiplier_snapshots().await?;
        self.runtime
            .multipliers
            .seed(&accounts, &snapshots, crate::storage::now_unix());
        Ok(())
    }
}

/// 启动时加载快照：粘性直接恢复，评分从快照继续（§20.1）。
async fn restore(store: &Store, runtime: &Runtime) -> Result<()> {
    let now = crate::storage::now_unix();
    // 超过 24 小时的快照宁可丢弃：拿一天前的延迟数据打分还不如中性分。
    let horizon = now - 24 * 3600;

    let bindings = store
        .load_sticky_bindings(now - sticky::TTL.as_secs() as i64)
        .await?;
    runtime.sticky.restore(&bindings);
    let snapshots = store.load_perf_snapshots(horizon).await?;
    runtime.perf.restore(&snapshots);

    let accounts = store.list_accounts().await?;
    let multipliers = store.list_multiplier_snapshots().await?;
    runtime.multipliers.seed(&accounts, &multipliers, now);

    tracing::info!(
        sticky = bindings.len(),
        perf = snapshots.len(),
        "已从快照恢复粘性绑定与性能统计"
    );
    Ok(())
}

/// 清理上次异常退出遗留的临时请求体（§1292）。
///
/// 正常路径下 `NamedTempFile` 会在请求结束、客户端断开或任务被丢弃时自动
/// 删除；只有进程崩溃会留下文件。安全时间取 6 小时，避免误删仍在途的长请求。
fn sweep_stale_temp_files(dir: &std::path::Path) {
    const HORIZON: std::time::Duration = std::time::Duration::from_secs(6 * 3600);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let path = entry.path();
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > HORIZON);
        if stale {
            match std::fs::remove_file(&path) {
                Ok(()) => removed += 1,
                Err(error) => {
                    tracing::warn!(%error, path = %path.display(), "清理临时请求体失败");
                }
            }
        }
    }
    if removed > 0 {
        tracing::info!(removed, "已清理上次异常退出遗留的临时请求体");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只有超过安全时间的遗留文件才会被清掉，新文件必须保留。
    #[test]
    fn stale_temp_files_are_swept_but_fresh_ones_survive() {
        let dir = tempfile::tempdir().unwrap();
        let fresh = dir.path().join("fresh");
        let stale = dir.path().join("stale");
        std::fs::write(&fresh, b"x").unwrap();
        std::fs::write(&stale, b"x").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 3600);
        let file = std::fs::File::options().write(true).open(&stale).unwrap();
        file.set_modified(old).unwrap();
        drop(file);

        sweep_stale_temp_files(dir.path());

        assert!(fresh.exists(), "新鲜文件不能被删");
        assert!(!stale.exists(), "超过安全时间的遗留文件必须清掉");
    }
}
