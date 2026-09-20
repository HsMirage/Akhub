//! Akhub 可执行入口。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use akhub::app::{AppState, Settings};
use anyhow::{Context, Result};

/// 版本号取自 Cargo.toml，与 `/admin` 接口里上报的是同一个值。
///
/// 用 `option_env!` 而不是 `env!`：Windows 资源脚本会在编译期注入同一个值，
/// 缺少该变量时应当退回 Cargo 的版本，而不是编译失败。
const VERSION: &str = match option_env!("AKHUB_BUILD_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};

#[tokio::main]
async fn main() -> Result<()> {
    // 自更新必须排在 --version / --help 之前：`akhub --update --version v1.1.2`
    // 里的 --version 是"装哪个版本"，不是"打印版本号"。
    //
    // 也刻意排在打开数据库之前：升级路径上数据目录可能还是旧结构，换个二进制
    // 不该先动数据（结构迁移留给新版本启动时做）。
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--update") {
        init_tracing();
        std::process::exit(akhub::update::cli_update(&args).await);
    }

    // 出问题时第一件事就是确认跑的是哪个二进制，所以这个分支要排在
    // 监听地址解析之前——本机 AKHUB_LISTEN 写坏了也得能 --version。
    if let Some(code) = handle_early_exit() {
        std::process::exit(code);
    }

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

/// 处理不需要数据库、不需要监听的早期退出参数。
///
/// 返回 `Some(退出码)` 表示「这个参数只要求打印点东西就走」，`None` 表示继续正常启动。
/// 用 `--flag` 前缀而不是位置参数，避免将来加子命令时把现有调用撞坏。
fn handle_early_exit() -> Option<i32> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |flag: &str| args.iter().any(|arg| arg == flag);

    if has("--version") || has("-V") {
        // 与管理后台概览页显示的是同一个字符串，便于比对升级是否生效。
        println!("akhub {VERSION}");
        return Some(0);
    }
    if has("--help") || has("-h") {
        print!("{HELP}");
        return Some(0);
    }
    None
}

/// `--help` 的正文。列出的是真正会被解析的参数与最常用的环境变量，
/// 完整的变量表在 README 里。
const HELP: &str = concat!(
    "akhub ",
    env!("CARGO_PKG_VERSION"),
    " —— AI 协议网关与组内负载均衡\n\n",
    "用法：\n",
    "  akhub                 启动服务（监听 AKHUB_LISTEN，默认 127.0.0.1:8080）\n",
    "  akhub --healthcheck   探测本机 /health/ready，就绪退出码 0，否则非 0\n",
    "  akhub --version       打印版本号\n",
    "  akhub --update        从 GitHub Release 下载并替换本二进制（需要写权限）\n",
    "  akhub --help          显示本帮助\n",
    "\n",
    "常用环境变量：\n",
    "  AKHUB_LISTEN                 监听地址，默认 127.0.0.1:8080\n",
    "  AKHUB_DATA_DIR               数据目录，默认 ./data\n",
    "  AKHUB_MASTER_KEY             32 字节 hex 或 base64 主密钥，优先于数据目录里的密钥文件\n",
    "  AKHUB_SHUTDOWN_GRACE_SECS    关闭时给在途请求的完成时间，默认 180\n",
    "  RUST_LOG                     日志级别，默认 akhub=info,warn\n",
    "\n",
    "管理后台：http://<监听地址>/admin\n",
);

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

/// 默认只打印 info 及以上；用环境变量 RUST_LOG 覆盖。
///
/// 输出统一经过 RedactingFormat：日志脱敏不能只靠"每个调用点记得调一次"，
/// 新增的日志点很容易漏（§20.2、§23.4）。
fn init_tracing() {
    use tracing_subscriber::layer::SubscriberExt as _;
    use tracing_subscriber::util::SubscriberInitExt as _;
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("akhub=info,tower_http=warn,warn"));
    let layer = fmt::layer()
        .with_writer(std::io::stderr)
        .event_format(RedactingFormat {
            inner: fmt::format(),
        });
    tracing_subscriber::registry()
        .with(filter)
        .with(layer)
        .init();
}

/// 把事件先渲染成文本、过一遍脱敏再输出（§20.2、§23.4）。
///
/// 包在格式化器外面而不是做成 Layer：tracing 的字段是结构化的，逐字段判断
/// 类型既繁琐又容易漏；先渲染成最终要写出去的那串文本，再对文本做一次与手工
/// 脱敏完全相同的处理，覆盖面就是百分之百。
struct RedactingFormat<F> {
    inner: F,
}

impl<S, N, F> tracing_subscriber::fmt::format::FormatEvent<S, N> for RedactingFormat<F>
where
    F: tracing_subscriber::fmt::format::FormatEvent<S, N>,
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::format::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let mut rendered = String::new();
        self.inner.format_event(
            ctx,
            tracing_subscriber::fmt::format::Writer::new(&mut rendered),
            event,
        )?;
        // 渲染结果一定以换行结尾；text() 按空白切分并原样保留，换行不会丢。
        write!(writer, "{}", akhub::security::redact::text(&rendered))
    }
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
