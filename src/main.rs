//! Akhub 可执行入口。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use akhub::app::{AppState, Settings};
use anyhow::{Context, Result};

#[tokio::main]
async fn main() -> Result<()> {
    let addr: SocketAddr = std::env::var("AKHUB_LISTEN")
        .unwrap_or_else(|_| "127.0.0.1:8080".to_string())
        .parse()
        .context("AKHUB_LISTEN 不是合法的监听地址")?;

    // 容器健康检查复用同一个二进制，镜像里就不必再装 curl（§25.1）。
    if std::env::args().any(|arg| arg == "--healthcheck") {
        return healthcheck(addr).await;
    }

    init_tracing();
    let data_dir = env_path("AKHUB_DATA_DIR", "./data");

    let settings = Settings {
        request_timeout: env_duration("AKHUB_REQUEST_TIMEOUT_SECS", 600)?,
        max_request_bytes: env_usize("AKHUB_MAX_REQUEST_BYTES", 64 * 1024 * 1024)?,
        shutdown_grace: env_duration("AKHUB_SHUTDOWN_GRACE_SECS", 180)?,
        multiplier_refresh: env_duration("AKHUB_MULTIPLIER_REFRESH_SECS", 300)?,
        response_state_days: env_u32("AKHUB_RESPONSE_STATE_DAYS", 30)?,
        model_sync: env_duration("AKHUB_MODEL_SYNC_SECS", 1800)?,
        retention_days: env_u32("AKHUB_RETENTION_DAYS", 30)?,
    };

    let state = AppState::bootstrap(&data_dir, settings).await?;
    tracing::info!(data_dir = %data_dir.display(), "数据目录已就绪");
    akhub::server::serve(state, addr).await
}

/// 探测本机 `/health/ready`。就绪返回 0，其余情况返回非 0。
async fn healthcheck(addr: SocketAddr) -> Result<()> {
    // 监听地址可能是 0.0.0.0，探测时改用环回地址。
    let host = if addr.ip().is_unspecified() {
        format!("127.0.0.1:{}", addr.port())
    } else {
        addr.to_string()
    };
    let response = reqwest::Client::new()
        .get(format!("http://{host}/health/ready"))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .context("健康检查请求失败")?;

    if response.status().is_success() {
        Ok(())
    } else {
        anyhow::bail!("健康检查返回 {}", response.status())
    }
}

/// 默认只打印 info 及以上；用 `RUST_LOG` 覆盖。
fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("akhub=info,tower_http=warn,warn"));
    fmt().with_env_filter(filter).init();
}

fn env_path(key: &str, default: &str) -> PathBuf {
    PathBuf::from(std::env::var(key).unwrap_or_else(|_| default.to_string()))
}

fn env_duration(key: &str, default: u64) -> Result<Duration> {
    Ok(Duration::from_secs(env_parse(key, default)?))
}

fn env_usize(key: &str, default: usize) -> Result<usize> {
    env_parse(key, default)
}

fn env_u32(key: &str, default: u32) -> Result<u32> {
    env_parse(key, default)
}

fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match std::env::var(key) {
        Ok(raw) => raw
            .trim()
            .parse()
            .map_err(|e| anyhow::anyhow!("{key} 取值非法：{e}")),
        Err(_) => Ok(default),
    }
}
