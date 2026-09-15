//! 进程共享状态、后台任务与优雅关闭（§19.1、§22、§25.3）。

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

/// 全部动态运行状态。
///
/// 与 [`crate::config::RuntimeConfig`] 并列而不是嵌进去：倍率、熔断、并发和
/// 额度不等待配置版本，每次真正发请求前都要读最新值（§21）。
#[derive(Default)]
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
    pub settings: Settings,
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
        let recorder = RequestRecorder::spawn(store.clone());
        let runtime = Runtime::default();
        restore(&store, &runtime).await?;

        // 上次异常退出遗留的临时请求体：启动时清掉（§1291、§1292）。
        let temp_dir = data_dir.join(TEMP_DIR_NAME);
        if let Err(error) = std::fs::create_dir_all(&temp_dir) {
            tracing::warn!(%error, path = %temp_dir.display(), "创建临时目录失败");
        } else {
            sweep_stale_temp_files(&temp_dir);
        }

        let state = Arc::new(AppState {
            cipher: master_key.cipher(),
            key_digest: master_key.key_digest(),
            master_key_from_env: master_key.from_env,
            config,
            store,
            upstream: UpstreamClient::new().context("构造上游 HTTP 客户端失败")?,
            sessions: SessionStore::new(),
            settings,
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
