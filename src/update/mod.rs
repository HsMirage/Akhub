//! 版本检查与自更新。
//!
//! 后台顶部的版本号点开之后要做两件事：一是告诉管理员"现在跑的是哪个版本、
//! 上游最新是哪个版本"，二是在原生二进制部署下把这台机器上的二进制真的换掉。
//! 二者都只依赖公开的 GitHub Release，不引入任何额外的服务端组件。
//!
//! 边界（刻意的）：
//!
//! - **只替换二进制，不改数据目录。** 数据库结构升级仍由启动时的迁移逻辑负责，
//!   升级前的旧二进制会拒绝打开新库并提示升级，而不是写坏数据。
//! - **容器与源码构建不支持自更新。** 前者换的是镜像，后者换的是源码；两者都
//!   给出明确的可复制命令，而不是让"立即更新"按钮点了没反应。
//! - **先校验再替换。** 下载的归档用 Release 里的 checksums.txt 校验 sha256，
//!   通过后才在新文件上做原子 rename；任何一步失败都保持原二进制不动。

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// 默认的 Release 查询入口（GitHub API）。
const DEFAULT_API: &str = "https://api.github.com/repos/HsMirage/Akhub";
/// 源码仓库，用于拼出给人看的链接与命令。
pub const REPO: &str = "HsMirage/Akhub";
/// GitHub 要求所有请求带 User-Agent，顺带让对方的日志里能看出是谁在查。
const USER_AGENT: &str = concat!("akhub/", env!("CARGO_PKG_VERSION"));
/// 一次成功检查的缓存时长。
const CACHE_TTL: Duration = Duration::from_secs(1800);
/// 连着点"重新检查"时的最短间隔：GitHub 对匿名请求按 IP 限流，不能任人连点。
const FORCE_FLOOR: Duration = Duration::from_secs(15);
/// 失败结果的缓存时长：对方挂了的时候不该每点一次就打一次。
const FAILURE_TTL: Duration = Duration::from_secs(60);
/// 单个下载文件的上限。发行包不到 20 MB，256 MB 只是防止被喂一个无穷流。
const MAX_DOWNLOAD: u64 = 256 * 1024 * 1024;
/// 检查请求的超时。
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
/// 下载发行包的总超时。
///
/// 18 MB 在国际链路上可能要几分钟，卡得太紧会变成"点一下没反应"；这里给足，
/// 由下面的读超时负责发现"彻底卡住"。
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(900);
/// 读超时：连续这么久一个字节都没读到，就当链路断了并重试。
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// 建立连接的超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 下载失败的重试次数。GitHub 资产走 CDN，跨国链路上偶发中断是常态。
const DOWNLOAD_ATTEMPTS: usize = 3;

/// 当前进程的版本号，与 `--version`、`/health/version` 是同一个字符串。
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 这套部署是怎么装出来的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Deploy {
    /// 原生二进制（一键脚本 / systemd / 手工安装），可以自更新。
    Binary,
    /// 容器：换的是镜像，不是二进制。
    Docker,
    /// 源码构建（cargo run / cargo build）：换的是源码。
    Source,
    /// Windows：正在运行的 exe 无法被自己覆盖。
    Windows,
}

impl Deploy {
    pub fn as_str(self) -> &'static str {
        match self {
            Deploy::Binary => "binary",
            Deploy::Docker => "docker",
            Deploy::Source => "source",
            Deploy::Windows => "windows",
        }
    }
}

/// 判断当前部署形态。
///
/// 允许用 `AKHUB_DEPLOY=binary|docker|source` 显式覆盖：把二进制塞进自定义
/// 镜像或自建 init 系统的部署方式判断不出来，这时以运维说了算。
pub fn deploy() -> Deploy {
    if let Ok(raw) = std::env::var("AKHUB_DEPLOY") {
        match raw.trim().to_ascii_lowercase().as_str() {
            "binary" => return Deploy::Binary,
            "docker" | "container" => return Deploy::Docker,
            "source" | "cargo" => return Deploy::Source,
            _ => {}
        }
    }
    if cfg!(windows) {
        return Deploy::Windows;
    }
    if in_container() {
        return Deploy::Docker;
    }
    if looks_like_source_build(&exe_path()) {
        return Deploy::Source;
    }
    Deploy::Binary
}

/// 是否跑在容器里。`/.dockerenv` 覆盖 Docker，`container` 变量覆盖 podman 等。
fn in_container() -> bool {
    Path::new("/.dockerenv").exists() || std::env::var_os("container").is_some()
}

/// 可执行文件是不是 cargo 的构建产物（`target/debug/...` 或 `target/release/...`）。
fn looks_like_source_build(exe: &Path) -> bool {
    let mut components = exe.components().peekable();
    while let Some(component) = components.next() {
        if component.as_os_str() != "target" {
            continue;
        }
        if let Some(next) = components.peek()
            && matches!(next.as_os_str().to_str(), Some("debug" | "release"))
        {
            return true;
        }
    }
    false
}

/// 当前可执行文件的路径。
pub fn exe_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("akhub"))
}

/// 是否允许从后台触发"重启服务"。
///
/// 只有确定有东西会把我们拉起来时才允许：systemd 会为每个服务设置
/// `INVOCATION_ID`，进程退出后按 `Restart=always` 立刻重启。前台手工
/// 运行（`cargo run`、`./akhub`）没有这层保障，点一下就等于把服务停掉。
pub fn can_restart() -> bool {
    if cfg!(windows) {
        return false;
    }
    if std::env::var_os("INVOCATION_ID").is_some() {
        return true;
    }
    // 容器里靠 restart 策略把进程拉回来。入口脚本在**确认二进制位于可写卷**之后
    // 才设这个变量——它同时说明了两件事：替换自己可行，以及退出后有人会拉起我们。
    // 所以在容器里这一条既是"能重启"，也是"自更新可行"。
    std::env::var(ALLOW_RESTART_ENV).is_ok_and(|value| value == "1")
}

/// 入口脚本用来声明"二进制可替换、退出后会被拉起"的环境变量名。
pub const ALLOW_RESTART_ENV: &str = "AKHUB_ALLOW_RESTART";

/// 当前进程能不能就地替换自己。
///
/// 两条路都算数：
///
/// - **原生二进制 + systemd**：进程对所在目录有写权限，退出后有 `Restart=always`
///   把它拉起来；
/// - **容器**：入口脚本把二进制复制到**可写的数据卷**再运行（见
///   deploy/docker-entrypoint.sh），并设 [`ALLOW_RESTART_ENV`] 声明这件事。镜像里的
///   二进制在只读镜像层、属主 root，以非 root 运行的服务进程覆盖不了它——这正是
///   容器部署过去升不了级的原因。
///
/// 为什么用显式声明而不是"试着写一下当前二进制"：探测成功也可能在真正 rename
/// 时失败，而这条判断要拿去回答"面板上该不该给一键升级按钮"。
pub fn can_self_replace() -> bool {
    if cfg!(windows) {
        return false;
    }
    if std::env::var_os("INVOCATION_ID").is_some() {
        return true;
    }
    std::env::var(ALLOW_RESTART_ENV).is_ok_and(|value| value == "1")
}

/// 重启提示：给人看的那一句。
pub fn restart_command() -> String {
    if std::env::var_os("INVOCATION_ID").is_some() {
        "systemctl restart akhub".to_string()
    } else {
        "重启 akhub 进程".to_string()
    }
}

/// 升级的引导命令：按部署形态给出**能直接粘进终端**的那一条。
pub fn update_command(deploy: Deploy, latest: Option<&str>) -> Option<String> {
    let tag = latest.map(|value| format!("v{value}")).unwrap_or_default();
    match deploy {
        Deploy::Binary => Some(if tag.is_empty() {
            "sudo akhub --update".to_string()
        } else {
            format!("sudo akhub --update --version {tag}")
        }),
        Deploy::Docker => Some(if tag.is_empty() {
            "docker compose pull && docker compose up -d".to_string()
        } else {
            format!("AKHUB_IMAGE=ghcr.io/hsmirage/akhub:{tag} docker compose up -d")
        }),
        Deploy::Source => Some("git pull && cargo build --release".to_string()),
        Deploy::Windows => Some(
            "irm https://raw.githubusercontent.com/HsMirage/Akhub/master/install.ps1 | iex"
                .to_string(),
        ),
    }
}

// ------------------------------------------------------------------ 检查结果

/// 一次版本检查的结果，直接作为后台接口的响应体。
#[derive(Debug, Clone, Serialize)]
pub struct Status {
    /// 是否启用了更新检查（`AKHUB_UPDATE_DISABLED=1` 时关闭）。
    pub enabled: bool,
    /// 当前进程的版本。
    pub current: String,
    /// 上游最新版本（查询失败时为 null）。
    pub latest: Option<String>,
    pub has_update: bool,
    pub release_url: Option<String>,
    pub release_name: Option<String>,
    pub published_at: Option<String>,
    /// Release 说明的前若干字符，够在弹窗里看个大概。
    pub notes: Option<String>,
    pub checked_at: u64,
    /// 这次响应是否来自缓存。
    pub cached: bool,
    /// 查询失败的原因（拿不到 GitHub 时明确说，而不是假装已是最新）。
    pub error: Option<String>,
    pub deploy: Deploy,
    /// 点"立即更新"是否有意义。
    pub can_self_update: bool,
    pub can_restart: bool,
    /// 一句话说明该怎么升级（给人看）。
    pub update_hint: Option<String>,
    /// 可复制的升级命令。
    pub update_command: Option<String>,
    /// 已经落盘但还没重启生效的版本。
    pub pending_version: Option<String>,
    /// 这次部署能不能在后台**一键**升到最新版（下载+替换+重启一次完成）。
    pub can_one_click: bool,
    /// 当前一键升级进度。
    pub progress: Progress,
}

impl Status {
    fn disabled(deploy: Deploy, pending: Option<String>) -> Self {
        Self {
            enabled: false,
            current: current().to_string(),
            latest: None,
            has_update: false,
            release_url: None,
            release_name: None,
            published_at: None,
            notes: None,
            checked_at: now_unix(),
            cached: false,
            error: None,
            deploy,
            can_self_update: false,
            can_restart: can_restart(),
            update_hint: Some("已通过 AKHUB_UPDATE_DISABLED 关闭更新检查。".to_string()),
            update_command: None,
            pending_version: pending,
            can_one_click: false,
            progress: Progress::idle(),
        }
    }
}

/// 一键升级的进度。
///
/// 放在进程内而不是返回给请求方就丢掉：升级要几分钟（拉镜像、重建容器），
/// 管理员在这期间刷新页面、换设备、甚至反向代理掐断连接都是常态。进度留在
/// 服务端，重新打开面板就能接着看到那一次进行到哪一步了。
#[derive(Debug, Clone, Default, Serialize)]
pub struct Progress {
    /// `idle` / `running` / `done` / `failed`。
    pub phase: String,
    /// 目标版本。
    pub to: Option<String>,
    /// 当前步骤（给人看的一句话）。
    pub step: Option<String>,
    /// 失败原因。
    pub error: Option<String>,
    /// 开始时刻（unix 秒）；`idle` 时为 0。
    pub started_at: u64,
}

impl Progress {
    fn idle() -> Self {
        Self {
            phase: "idle".to_string(),
            ..Default::default()
        }
    }

    pub fn is_running(&self) -> bool {
        self.phase == "running"
    }

    /// 这次升级是不是已经收尾（成功或失败）。
    pub fn is_settled(&self) -> bool {
        matches!(self.phase.as_str(), "done" | "failed")
    }
}

#[derive(Debug, Clone)]
struct Cached {
    at: u64,
    status: Status,
}

/// 版本检查与更新的进程内状态。
///
/// 放在 `Runtime` 里而不是模块级静态量：测试会在同一个进程里起多台 Akhub，
/// 静态缓存会互相串味。
pub struct Registry {
    client: reqwest::Client,
    /// Release API 根地址，默认 GitHub；自建镜像或测试可以覆盖。
    api: Mutex<String>,
    disabled: AtomicBool,
    cache: Mutex<Option<Cached>>,
    /// 同一时刻只允许一个更新在跑。
    busy: AtomicBool,
    /// 已落盘、等待重启生效的版本。
    pending: Mutex<Option<String>>,
    /// 一键升级的进度（见 [`Progress`]）。
    progress: Mutex<Progress>,
    /// 部署形态覆盖：测试用它扮演二进制部署，免得去改进程环境变量。
    ///
    /// 用 Mutex 而不是直接存值：集成测试要通过 `SharedState` 在运行期改它。
    deploy: Mutex<Option<Deploy>>,
    /// "能否就地替换自己"的覆盖，同样只给测试用。
    ///
    /// **为什么要有这个缝**：生产里这条判断读环境变量（入口脚本设的
    /// `AKHUB_ALLOW_RESTART`），而环境变量是**进程全局**的——测试并行跑在同一个
    /// 进程里，一个用例 set 了它，另一个用例就会看到。CI 上就是这么挂的：
    /// "容器不可替换"那条断言拿到 true，因为并行的另一个用例刚把它设成 1。
    /// 用显式覆盖就不存在这个竞态。
    self_replace: Mutex<Option<bool>>,
}

impl std::fmt::Debug for Registry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("update::Registry").finish_non_exhaustive()
    }
}

impl Default for Registry {
    fn default() -> Self {
        let api = std::env::var("AKHUB_UPDATE_API")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_API.to_string());
        let disabled = std::env::var("AKHUB_UPDATE_DISABLED")
            .is_ok_and(|value| value != "0" && !value.is_empty());
        Self::new(api, disabled)
    }
}

impl Registry {
    pub fn new(api: impl Into<String>, disabled: bool) -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(READ_TIMEOUT)
                .timeout(DOWNLOAD_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            api: Mutex::new(api.into()),
            disabled: AtomicBool::new(disabled),
            cache: Mutex::new(None),
            busy: AtomicBool::new(false),
            pending: Mutex::new(None),
            progress: Mutex::new(Progress::idle()),
            deploy: Mutex::new(None),
            self_replace: Mutex::new(None),
        }
    }

    /// 覆盖部署形态（只给测试用；生产走 [`deploy`] 的自动判定与 AKHUB_DEPLOY）。
    pub fn with_deploy(self, deploy: Deploy) -> Self {
        *self.deploy.lock().unwrap() = Some(deploy);
        self
    }

    fn deploy(&self) -> Deploy {
        self.deploy.lock().unwrap().unwrap_or_else(deploy)
    }

    /// 运行期覆盖部署形态。
    ///
    /// 与 `with_deploy` 的区别是它作用在 `Arc` 里的同一个实例上：后台接口的
    /// 集成测试只能拿到 `SharedState`，没法在构造时就指定形态。
    pub fn set_deploy_for_tests(&self, deploy: Deploy) {
        // 与 with_deploy 同一条读路径（deploy 字段），只是绕过构造期。
        let mut slot = self.deploy.lock().unwrap();
        *slot = Some(deploy);
    }

    /// 覆盖"能否就地替换自己"（只给测试用）。
    ///
    /// 见 `self_replace` 字段上的说明：生产读环境变量，但那是进程全局的，
    /// 测试并行时会互相干扰。
    pub fn set_can_self_replace_for_tests(&self, value: bool) {
        *self.self_replace.lock().unwrap() = Some(value);
    }

    /// 当前部署能不能就地替换自己：先看测试覆盖，再看环境变量。
    fn self_replace_capable(&self) -> bool {
        self.self_replace
            .lock()
            .unwrap()
            .unwrap_or_else(can_self_replace)
    }

    /// 覆盖 Release API 地址（测试注入假 GitHub 用）。
    pub fn set_api_base(&self, api: impl Into<String>) {
        *self.api.lock().unwrap() = api.into();
        *self.cache.lock().unwrap() = None;
    }

    pub fn api_base(&self) -> String {
        self.api.lock().unwrap().clone()
    }

    fn pending_version(&self) -> Option<String> {
        self.pending.lock().unwrap().clone()
    }

    /// 查询最新 Release。`force` 表示用户手动点的"重新检查"。
    pub async fn check(&self, force: bool) -> Status {
        let deploy = self.deploy();
        if self.disabled.load(Ordering::Relaxed) {
            return Status::disabled(deploy, self.pending_version());
        }

        let now = now_unix();
        if let Some(cached) = self.cache.lock().unwrap().as_ref() {
            let age = now.saturating_sub(cached.at);
            let ttl = if cached.status.error.is_some() {
                FAILURE_TTL
            } else {
                CACHE_TTL
            };
            // 手动检查也要守住最小间隔：GitHub 匿名限流是 60 次/小时/IP。
            if age < ttl.as_secs() && (!force || age < FORCE_FLOOR.as_secs()) {
                let mut status = cached.status.clone();
                status.cached = true;
                return self.decorate(status);
            }
        }

        let status = match self.fetch_latest().await {
            Ok(status) => status,
            Err(error) => {
                tracing::warn!(%error, "检查新版本失败");
                Status {
                    enabled: true,
                    current: current().to_string(),
                    latest: None,
                    has_update: false,
                    release_url: None,
                    release_name: None,
                    published_at: None,
                    notes: None,
                    checked_at: now,
                    cached: false,
                    error: Some(error),
                    deploy,
                    can_self_update: false,
                    can_restart: can_restart(),
                    update_hint: None,
                    update_command: None,
                    pending_version: self.pending_version(),
                    can_one_click: false,
                    progress: self.progress(),
                }
            }
        };

        *self.cache.lock().unwrap() = Some(Cached {
            at: now,
            status: status.clone(),
        });
        self.decorate(status)
    }

    /// 补上"跟部署形态有关"的字段；缓存里存的只有跟 Release 有关的部分。
    fn decorate(&self, mut status: Status) -> Status {
        status.pending_version = self.pending_version();
        status.can_restart = can_restart();
        // 一键升级要求"有新版本"且"这条路走得通"：二进制与容器都能就地替换自己
        // （见 can_one_click / can_self_replace），源码构建与 Windows 不行。
        status.can_one_click = status.has_update && self.can_one_click();
        status.progress = self.progress();
        status.can_self_update =
            status.deploy == Deploy::Binary && status.enabled && status.has_update;
        status.update_command = if status.has_update {
            update_command(status.deploy, status.latest.as_deref())
        } else {
            None
        };
        status.update_hint = if status.error.is_some() {
            Some("查询 GitHub 失败：请确认服务器能访问 github.com，或稍后重试。".to_string())
        } else if !status.has_update {
            Some(match status.deploy {
                Deploy::Binary => "已是最新版本。".to_string(),
                Deploy::Docker => "已是最新版本。".to_string(),
                Deploy::Source => "已是最新版本；更新源码后重新编译即可。".to_string(),
                Deploy::Windows => "已是最新版本。".to_string(),
            })
        } else {
            Some(match status.deploy {
                Deploy::Binary => {
                    "可以直接点「立即更新」：下载 → 校验 sha256 → 原子替换二进制，重启服务后生效。"
                        .to_string()
                }
                Deploy::Docker => {
                    "可以直接点「一键升级」：下载 → 校验 sha256 → 替换容器内的二进制，                     然后自动重启容器。"
                        .to_string()
                }
                Deploy::Source => "源码构建请更新源码后重新编译。".to_string(),
                Deploy::Windows => {
                    "Windows 下运行中的 exe 无法自我覆盖，请下载新版本或重跑安装脚本。".to_string()
                }
            })
        };
        status
    }

    /// 拉取 `/releases/latest` 并解析成一次检查结果。
    async fn fetch_latest(&self) -> Result<Status, String> {
        let url = format!("{}/releases/latest", self.api_base().trim_end_matches('/'));
        let response = self
            .client
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(CHECK_TIMEOUT)
            .send()
            .await
            .map_err(|error| format!("请求 {url} 失败：{error}"))?;
        let status_code = response.status();
        if !status_code.is_success() {
            return Err(format!("GitHub 返回 {status_code}"));
        }
        let release: Release = response
            .json()
            .await
            .map_err(|error| format!("解析 Release 响应失败：{error}"))?;
        Ok(self.status_from(release))
    }

    fn status_from(&self, release: Release) -> Status {
        let deploy = self.deploy();
        let latest = normalize_tag(&release.tag_name);
        let has_update = compare_versions(&latest, current()).is_gt();
        Status {
            enabled: true,
            current: current().to_string(),
            latest: Some(latest),
            has_update,
            release_url: release.html_url,
            release_name: release.name,
            published_at: release.published_at,
            notes: release.body.map(|body| truncate_chars(&body, 1200)),
            checked_at: now_unix(),
            cached: false,
            error: None,
            deploy,
            can_self_update: false,
            can_restart: can_restart(),
            update_hint: None,
            update_command: None,
            pending_version: None,
            can_one_click: false,
            progress: self.progress(),
        }
    }

    /// 对某个具体版本再查一次，拿到它的资产列表（自更新要按平台挑文件）。
    async fn fetch_release(&self, tag: &str) -> Result<Release, String> {
        let url = format!(
            "{}/releases/tags/{}",
            self.api_base().trim_end_matches('/'),
            tag
        );
        let response = self
            .client
            .get(&url)
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(CHECK_TIMEOUT)
            .send()
            .await
            .map_err(|error| format!("请求 {url} 失败：{error}"))?;
        let status_code = response.status();
        if !status_code.is_success() {
            return Err(format!("GitHub 返回 {status_code}（{tag} 可能不存在）"));
        }
        response
            .json()
            .await
            .map_err(|error| format!("解析 Release 响应失败：{error}"))
    }

    /// 占住"更新中"这个坑。
    ///
    /// 返回的守卫在**被丢弃**时复位标志：管理员关掉页面、反向代理掐断连接都会让
    /// 请求 future 被丢弃。早期实现把复位写在 await 之后，于是中断一次就永久卡在
    /// "已经有一个更新在进行中"，只能重启服务——守卫让中断也能正确收尾。
    fn acquire(&self) -> Result<BusyGuard<'_>, String> {
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err("已经有一个更新在进行中，请稍候。".to_string());
        }
        Ok(BusyGuard(&self.busy))
    }

    /// 把 \`target\` 指向的二进制换成最新版（或指定版本）。
    ///
    /// 整个过程只有最后一步 rename 会改动现场，之前的失败都只是白下载一次。
    pub async fn install(&self, target: &Path, version: Option<&str>) -> Result<Outcome, String> {
        let _guard = self.acquire()?;
        self.install_inner(target, version).await
    }

    // -------------------------------------------------------------- 一键升级

    /// 当前一键升级进度。
    pub fn progress(&self) -> Progress {
        self.progress.lock().unwrap().clone()
    }

    /// 这次部署能不能在后台一键升到最新版。
    ///
    /// 两条路，判据都是"能不能替换自己 + 有没有人把进程拉起来"：
    ///
    /// - **原生二进制**：对所在目录有写权限，退出后由 systemd 拉起；
    /// - **容器**：入口脚本把二进制复制到可写的数据卷再运行（见
    ///   deploy/docker-entrypoint.sh），退出后由 `restart` 策略拉起同一个容器；
    /// - **源码构建 / Windows / 二进制不可替换的容器**：给可复制的命令，
    ///   而不是让按钮点了没反应。
    pub fn can_one_click(&self) -> bool {
        match self.deploy() {
            // 原生二进制与容器走同一条路：替换自己，然后退出等监督者拉起。
            // 走不走得通由 self_replace_capable 判定，不再依赖宿主机上的额外组件。
            Deploy::Binary | Deploy::Docker => self.self_replace_capable(),
            Deploy::Source | Deploy::Windows => false,
        }
    }

    /// 一键升级到最新版：**一次点击走完全部步骤**。
    ///
    /// 与 [\`Self::install\`] 的区别是不需要调用方再点一次"重启服务"：
    /// 原生二进制走"替换 → 自己触发优雅关闭 → systemd 拉起新进程"，
    /// 容器走"宿主代理换镜像 → 重建容器"，两条路都是点一下就结束。
    ///
    /// \`restart\` 是"替调用方触发重启"的钩子（生产是 \`Runtime::begin_shutdown\`）；
    /// 传 None 表示只替换、由调用方自己收尾。抽成参数是为了让测试能在不真的
    /// 关掉测试进程的前提下断言"重启确实被请求了"。
    pub async fn upgrade<F>(&self, target: &Path, restart: Option<F>) -> Result<Progress, String>
    where
        F: FnOnce() + Send + 'static,
    {
        // 互斥与 install 共用同一个标志：一键升级和命令行更新不能同时跑。
        let _guard = self.acquire()?;

        // 一进来就置 running：即使下面立刻失败，失败原因也要写进进度。
        // 早期实现把置位放在"检查通过之后"，于是"不支持一键升级"这类失败
        // 只回给了这一次请求——面板停在 idle，用户看到的是"点了没反应"。
        self.set_progress(Progress {
            phase: "running".to_string(),
            to: None,
            step: Some("检查新版本".to_string()),
            error: None,
            started_at: now_unix(),
        });

        let result = self.upgrade_inner(target, restart).await;
        let snapshot = self.progress();
        match result {
            Ok((version, step)) => {
                self.set_progress(Progress {
                    phase: "done".to_string(),
                    to: Some(version),
                    step: Some(step),
                    error: None,
                    started_at: snapshot.started_at,
                });
            }
            Err(error) => {
                self.set_progress(Progress {
                    phase: "failed".to_string(),
                    to: snapshot.to,
                    step: None,
                    error: Some(error.clone()),
                    started_at: snapshot.started_at,
                });
                return Err(error);
            }
        }
        Ok(self.progress())
    }

    /// 升级的实际步骤；返回（目标版本，收尾说明）。
    async fn upgrade_inner<F>(
        &self,
        target: &Path,
        restart: Option<F>,
    ) -> Result<(String, String), String>
    where
        F: FnOnce() + Send + 'static,
    {
        if !self.can_one_click() {
            return Err(self.one_click_refusal());
        }

        let status = self.check(true).await;
        if let Some(error) = status.error.clone() {
            return Err(error);
        }
        let version = status
            .latest
            .clone()
            .ok_or_else(|| "没能从 Release 里读出版本号".to_string())?;
        if !status.has_update {
            return Err(format!("当前已经是最新版本 v{}。", status.current));
        }
        self.set_to(&version);

        // 容器与二进制走**同一条路**：下载 → 校验 → 替换自己 → 退出，由监督者
        // （systemd，或容器的 restart 策略）拉起新版本。
        //
        // 这不是偷懒，而是同机部署的 sub2api 已经验证过的做法：它的二进制在容器
        // 可写层、属主就是运行用户，于是它覆盖自己再退出，靠 restart 策略把**同一个
        // 容器**拉起来（实测 RestartCount 会增长）。我们让入口脚本把二进制复制到数据
        // 卷再运行，效果一样，但连"镜像层可写"都不需要。
        let step = match self.deploy() {
            Deploy::Binary | Deploy::Docker => {
                self.upgrade_binary(target, &version, restart).await?
            }
            Deploy::Source | Deploy::Windows => return Err(self.one_click_refusal()),
        };
        Ok((version, step))
    }

    /// 记下目标版本：进度面板在"检查"阶段之后就该显示要升到哪个版本。
    fn set_to(&self, version: &str) {
        if let Ok(mut progress) = self.progress.lock() {
            progress.to = Some(version.to_string());
        }
    }

    /// 把进度落定为"已替换二进制、正在重启"。
    ///
    /// 抽成方法不只是为了复用：这段"**先读 started_at、再写新进度**"的顺序
    /// 曾经死锁过。当时 `started_at` 直接写成
    /// `self.progress.lock().unwrap().started_at` 放在 `set_progress(...)` 的
    /// **实参**位置——临时 `MutexGuard` 会活到整条语句结束，而 `set_progress`
    /// 内部又要锁同一个互斥量，std 的 `Mutex` 不可重入，于是当场死锁。
    ///
    /// 现场表现极具迷惑性：二进制已经替换成功、日志停在"准备重启"，进程既不
    /// 退出也不重启，管理接口也再没有任何响应。抽成方法后，回归测试可以直接
    /// 驱动它（见 `finishing_an_upgrade_does_not_deadlock`），而不是去测一段
    /// 复制出来的代码。
    fn mark_replaced_and_restarting(&self, to: &str) {
        // 先取值再调用：绝不要把 lock() 写在 set_progress 的实参里。
        let started_at = self.progress.lock().unwrap().started_at;
        self.set_progress(Progress {
            phase: "done".to_string(),
            to: Some(to.to_string()),
            step: Some("已替换二进制，正在重启".to_string()),
            error: None,
            started_at,
        });
    }

    fn set_progress(&self, progress: Progress) {
        *self.progress.lock().unwrap() = progress;
    }

    /// 不支持一键升级时，说清"该怎么做"，而不是只回一句不行。
    fn one_click_refusal(&self) -> String {
        match self.deploy() {
            Deploy::Binary | Deploy::Docker => format!(
                "当前部署不支持一键升级（二进制不可替换，或没有监督进程会把它拉起来）。                 请手工执行：{}",
                update_command(self.deploy(), None).unwrap_or_default()
            ),
            Deploy::Source => {
                "源码构建不支持一键升级。请更新源码后重新编译（git pull && cargo build --release）。"
                    .to_string()
            }
            Deploy::Windows => {
                "Windows 下运行中的程序无法覆盖自身。请下载新版 exe 或重跑 install.ps1。".to_string()
            }
        }
    }

    /// 原生二进制：下载校验替换，然后请求重启。
    async fn upgrade_binary<F>(
        &self,
        target: &Path,
        version: &str,
        restart: Option<F>,
    ) -> Result<String, String>
    where
        F: FnOnce() + Send + 'static,
    {
        self.set_step("下载并校验新版本");
        let outcome = self.install_inner(target, Some(version)).await?;
        if !can_restart() {
            // 二进制已经换好了，只是没人会把它拉起来。这不是失败，
            // 如实说明"还差一步手工重启"，比假装成功好。
            return Ok(format!(
                "已替换为 v{}；未检测到会自动拉起服务的监督进程，请手工重启：{}",
                outcome.to, outcome.restart_command
            ));
        }
        if let Some(restart) = restart {
            self.set_step("重启服务");
            // 先把进度落定再触发关闭：重启会立刻开始优雅关闭，此后这个进程
            // 里能写的东西只剩已经写好的内存状态。
            //
            self.mark_replaced_and_restarting(&outcome.to);
            // 给响应一点时间发出去：立刻关闭的话客户端只会看到连接被重置。
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(600)).await;
                restart();
            });
            return Ok(format!("已替换为 v{}，正在重启服务", outcome.to));
        }
        Ok(format!("已替换为 v{}，重启服务后生效", outcome.to))
    }

    fn set_step(&self, step: &str) {
        if let Ok(mut progress) = self.progress.lock() {
            progress.step = Some(step.to_string());
        }
    }

    async fn install_inner(&self, target: &Path, version: Option<&str>) -> Result<Outcome, String> {
        let deploy = self.deploy();
        if matches!(deploy, Deploy::Source | Deploy::Windows) {
            return Err(match deploy {
                Deploy::Source => {
                    "源码构建不支持自更新。请更新源码后重新编译（git pull && cargo build --release）。"
                        .to_string()
                }
                Deploy::Windows => {
                    "Windows 下运行中的程序无法覆盖自身。请下载新版 exe 或重跑 install.ps1。"
                        .to_string()
                }
                // 上面已用 matches! 排除；这里只是让 match 穷尽。
                Deploy::Binary | Deploy::Docker => unreachable!(),
            });
        }

        // 目标路径优先取真实路径：`/usr/local/bin/akhub` 可能是软链，直接 rename
        // 到链接路径会把链接本身换掉，而不是它指向的文件。
        let target = std::fs::canonicalize(target).unwrap_or_else(|_| target.to_path_buf());
        let directory = target
            .parent()
            .ok_or_else(|| format!("无法确定 {} 所在目录", target.display()))?
            .to_path_buf();
        ensure_writable(&directory)?;

        let explicit = version.map(|value| value.trim().trim_start_matches('v').to_string());
        let release = match &explicit {
            Some(value) => self.fetch_release(&format!("v{value}")).await?,
            None => {
                let status = self.check(true).await;
                if let Some(error) = status.error {
                    return Err(error);
                }
                let latest = status
                    .latest
                    .ok_or_else(|| "没能从 Release 里读出版本号".to_string())?;
                if !status.has_update {
                    return Err(format!("当前已经是最新版本 v{}。", status.current));
                }
                self.fetch_release(&format!("v{latest}")).await?
            }
        };

        let latest = normalize_tag(&release.tag_name);
        let asset = pick_asset(&release.assets, &latest).ok_or_else(|| no_asset_hint(&latest))?;
        let checksums = release
            .assets
            .iter()
            .find(|item| item.name == "checksums.txt")
            .ok_or_else(|| {
                "Release 里没有 checksums.txt，拒绝在无法校验的情况下更新。".to_string()
            })?;

        let workspace = tempfile::Builder::new()
            .prefix("akhub-update-")
            .tempdir()
            .map_err(|error| format!("创建临时目录失败：{error}"))?;
        let archive = workspace.path().join(&asset.name);
        tracing::info!(asset = %asset.name, "开始下载新版本");
        download(&self.client, &asset.browser_download_url, &archive).await?;
        let expected =
            fetch_checksum(&self.client, &checksums.browser_download_url, &asset.name).await?;
        verify_sha256(&archive, &expected)?;
        tracing::info!(asset = %asset.name, "sha256 校验通过");

        let stem = asset
            .name
            .strip_suffix(".tar.gz")
            .ok_or_else(|| format!("不认识的资产名：{}", asset.name))?;
        let extracted = extract_binary(&archive, workspace.path(), stem)?;

        // 备份旧二进制：命名与 install.sh 保持一致，升级回滚时一眼能认出来。
        let backup = backup_existing(&target)?;
        // 先写到同目录的临时文件再 rename：跨文件系统的 rename 会失败，
        // 而"写一半断电"会让目标文件变成半截二进制。
        let staged = directory.join(format!(".akhub-new-{}", std::process::id()));
        std::fs::copy(&extracted, &staged)
            .map_err(|error| format!("写入 {} 失败：{error}", staged.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
                .map_err(|error| format!("设置可执行权限失败：{error}"))?;
        }
        std::fs::rename(&staged, &target).map_err(|error| {
            let _ = std::fs::remove_file(&staged);
            format!("替换 {} 失败：{error}", target.display())
        })?;
        drop(workspace);

        *self.pending.lock().unwrap() = Some(latest.clone());
        *self.cache.lock().unwrap() = None;
        tracing::info!(%latest, target = %target.display(), "二进制已更新，等待重启生效");

        Ok(Outcome {
            from: current().to_string(),
            to: latest,
            path: target.display().to_string(),
            backup: backup.map(|path| path.display().to_string()),
            need_restart: true,
            can_restart: can_restart(),
            restart_command: restart_command(),
        })
    }
}

/// "更新中"标志的守卫：正常返回、报错、future 被丢弃都会复位。
struct BusyGuard<'a>(&'a AtomicBool);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// 替换成功后的结果。
#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub from: String,
    pub to: String,
    pub path: String,
    pub backup: Option<String>,
    pub need_restart: bool,
    pub can_restart: bool,
    pub restart_command: String,
}

// ------------------------------------------------------------------ 版本号

/// 解析 `v1.2.3` / `1.2.3` / `1.2.3-rc.2` 这类标签。
///
/// 只认「主.次.补丁」，缺的段按 0 补；预发布后缀整体作为一个字符串参与比较。
/// 刻意不引入 semver 依赖：发行流程只用得到这一种形状。
pub fn parse_version(raw: &str) -> Option<(u64, u64, u64, Option<String>)> {
    let raw = raw.trim().trim_start_matches(['v', 'V']);
    if raw.is_empty() {
        return None;
    }
    let (core, pre) = match raw.split_once('-') {
        Some((core, pre)) => (core, Some(pre.to_string())),
        None => (raw, None),
    };
    // `+build` 元数据不参与比较。
    let core = core.split('+').next().unwrap_or(core);
    let mut parts = core.split('.');
    let major = parts.next()?.trim().parse::<u64>().ok()?;
    let minor = parts
        .next()
        .map(|value| value.trim().parse::<u64>())
        .transpose()
        .ok()?
        .unwrap_or(0);
    let patch = parts
        .next()
        .map(|value| value.trim().parse::<u64>())
        .transpose()
        .ok()?
        .unwrap_or(0);
    Some((major, minor, patch, pre.filter(|value| !value.is_empty())))
}

/// 去掉标签前缀，得到纯粹的版本号字符串。
pub fn normalize_tag(tag: &str) -> String {
    tag.trim().trim_start_matches(['v', 'V']).to_string()
}

/// 比较两个版本：`a > b` 时返回 `Ordering::Greater`。
///
/// 预发布版本小于同号正式版（`1.0.0-rc.1 < 1.0.0`），这与 semver 一致，
/// 也让"最新 Release 是 rc、当前是正式版"时不会被误判成有更新。
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (Some(left), Some(right)) = (parse_version(a), parse_version(b)) else {
        // 解析不了就按字符串比，至少不会永远说"已是最新"。
        return a.trim().cmp(b.trim());
    };
    let core = (left.0, left.1, left.2).cmp(&(right.0, right.1, right.2));
    if core != Ordering::Equal {
        return core;
    }
    match (&left.3, &right.3) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(one), Some(other)) => one.cmp(other),
    }
}

// ------------------------------------------------------------------ 平台与资产

/// 当前平台在发行资产名里的后缀，与 `scripts/package.sh` 的命名一一对应。
///
/// **只有 Linux 还有命令行版的发行资产**：macOS 与 Windows 从 v1.1.17 起改以桌面
/// 应用发布（见 `desktop/`），Release 里没有它们的裸二进制，也就没有可自动更新的
/// 包。这两个平台返回 None，由 no_asset_hint 给出"去下桌面安装包"的提示。
///
/// 公开出去是为了让测试能拼出与真实发行包同构的资产名；生产代码只用它挑文件。
pub fn asset_suffix() -> Option<&'static str> {
    // 显式覆盖：给自动化测试用（在 macOS 上验证 Linux 的下载→替换链路），
    // 也留给交叉部署。只认下面这两个值，避免被用来拉任意文件名。
    if let Ok(raw) = std::env::var("AKHUB_ASSET_SUFFIX") {
        let raw = raw.trim();
        if raw == "linux-x86_64" || raw == "linux-aarch64" {
            return Some(if raw == "linux-x86_64" {
                "linux-x86_64"
            } else {
                "linux-aarch64"
            });
        }
        tracing::warn!(value = %raw, "AKHUB_ASSET_SUFFIX 不是已知的平台标识，已忽略");
    }
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x86_64"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        _ => None,
    }
}

/// 挑不到发行包时给用户一句能照着做的话。
///
/// macOS / Windows 从 v1.1.17 起只发桌面应用，自动更新只覆盖 Linux 的单体二进制。
/// 这两个平台上还留着旧命令行版的机器，需要被告知去哪儿拿新的东西，而不是看到一句
/// "没有适配当前平台的资产"。
fn no_asset_hint(version: &str) -> String {
    no_asset_hint_for(std::env::consts::OS, version)
}

fn no_asset_hint_for(os: &str, version: &str) -> String {
    match os {
        "macos" => format!(
            "macOS 从 v1.1.17 起只以桌面应用发布：请在 Releases 页面下载 akhub-desktop-{version}-macos-arm64.dmg（Apple Silicon）或 akhub-desktop-{version}-macos-x86_64.dmg（Intel）。自动更新只支持 Linux 服务端。"
        ),
        "windows" => format!(
            "Windows 从 v1.1.17 起只以桌面应用发布：请在 Releases 页面下载 akhub-desktop-{version}-windows-x86_64-setup.exe。自动更新只支持 Linux 服务端。"
        ),
        _ => format!(
            "Release v{version} 里没有适配当前平台（{}）的资产，请手工下载安装。",
            platform_label()
        ),
    }
}

fn platform_label() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// 按平台挑资产。
///
/// Linux x86_64 历史上同时发过 musl 与 glibc 两个包（现已只发 glibc），所以这里
/// 保留一个备用名：装的是老版本、或者被固定到老 tag 时，仍然找得到对应的包。
fn pick_asset<'a>(assets: &'a [Asset], version: &str) -> Option<&'a Asset> {
    let suffix = asset_suffix()?;
    let mut wanted = vec![format!("akhub-v{version}-{suffix}.tar.gz")];
    if suffix == "linux-x86_64" {
        wanted.push(format!("akhub-v{version}-linux-x86_64-musl.tar.gz"));
    }
    wanted
        .iter()
        .find_map(|name| assets.iter().find(|item| item.name == *name))
}

// ------------------------------------------------------------------ 下载与校验

/// 下载发行包；跨国链路上的偶发中断重试几次再放弃。
///
/// 不区分错误类型一律重试：404 这类"地址就是不对"的错误多试两次的代价只是几秒钟，
/// 而把偶发中断当成最终失败，用户看到的就是"更新失败"四个字。
async fn download(client: &reqwest::Client, url: &str, path: &Path) -> Result<u64, String> {
    let mut last = String::new();
    for attempt in 1..=DOWNLOAD_ATTEMPTS {
        match download_once(client, url, path).await {
            Ok(bytes) => return Ok(bytes),
            Err(error) => {
                tracing::warn!(attempt, %error, "下载发行包失败");
                last = error;
                if attempt < DOWNLOAD_ATTEMPTS {
                    tokio::time::sleep(Duration::from_secs(attempt as u64 * 2)).await;
                }
            }
        }
    }
    Err(format!(
        "{last}（已重试 {DOWNLOAD_ATTEMPTS} 次；可稍后再试，或在服务器上执行 sudo akhub --update）"
    ))
}

async fn download_once(client: &reqwest::Client, url: &str, path: &Path) -> Result<u64, String> {
    use futures::StreamExt as _;
    use tokio::io::AsyncWriteExt as _;

    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| format!("下载失败：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("下载 {url} 返回 {}", response.status()));
    }
    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|error| format!("创建 {} 失败：{error}", path.display()))?;
    let mut stream = response.bytes_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| format!("下载中断：{error}"))?;
        total += chunk.len() as u64;
        if total > MAX_DOWNLOAD {
            return Err("下载内容超过体积上限，已中止".to_string());
        }
        file.write_all(&chunk)
            .await
            .map_err(|error| format!("写入下载内容失败：{error}"))?;
    }
    file.flush()
        .await
        .map_err(|error| format!("刷写下载内容失败：{error}"))?;
    Ok(total)
}

/// 从 checksums.txt 里取出某个文件的 sha256。
async fn fetch_checksum(
    client: &reqwest::Client,
    url: &str,
    asset: &str,
) -> Result<String, String> {
    let text = client
        .get(url)
        .timeout(CHECK_TIMEOUT)
        .send()
        .await
        .map_err(|error| format!("下载 checksums.txt 失败：{error}"))?
        .error_for_status()
        .map_err(|error| format!("下载 checksums.txt 失败：{error}"))?
        .text()
        .await
        .map_err(|error| format!("读取 checksums.txt 失败：{error}"))?;
    parse_checksum(&text, asset).ok_or_else(|| format!("checksums.txt 里没有 {asset}，拒绝更新"))
}

/// 解析 `sha256sum` 风格的清单；文件名可能带 `./` 或 `*` 前缀。
pub fn parse_checksum(text: &str, asset: &str) -> Option<String> {
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(hash), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = name.trim_start_matches(['*', '.', '/']);
        if name == asset && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

fn sha256_file(path: &Path) -> Result<String, String> {
    use std::io::Read as _;

    let mut file =
        std::fs::File::open(path).map_err(|error| format!("打开下载文件失败：{error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("读取下载文件失败：{error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn verify_sha256(path: &Path, expected: &str) -> Result<(), String> {
    let actual = sha256_file(path)?;
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(format!(
            "sha256 不匹配，已放弃更新（期望 {expected}，实际 {actual}）"
        ))
    }
}

/// 从归档里取出二进制。归档内层目录名 == 资产名去掉扩展名（package.sh 的约定）。
fn extract_binary(archive: &Path, workspace: &Path, stem: &str) -> Result<PathBuf, String> {
    let unpacked = workspace.join("unpacked");
    std::fs::create_dir_all(&unpacked).map_err(|error| format!("创建解压目录失败：{error}"))?;
    let file = std::fs::File::open(archive).map_err(|error| format!("打开归档失败：{error}"))?;
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(file));
    // `unpack` 会自行清洗成员路径，`../` 逃不出 unpacked。
    tar.unpack(&unpacked)
        .map_err(|error| format!("解压归档失败：{error}"))?;
    let binary = unpacked.join(stem).join("akhub");
    if binary.is_file() {
        Ok(binary)
    } else {
        Err(format!("归档里没有找到 {stem}/akhub，发行包结构可能变了"))
    }
}

/// 备份旧二进制；失败只警告，不阻断更新。
fn backup_existing(target: &Path) -> Result<Option<PathBuf>, String> {
    if !target.exists() {
        return Ok(None);
    }
    let stamp = time::OffsetDateTime::now_utc()
        .format(&time::macros::format_description!(
            "[year][month][day][hour][minute][second]"
        ))
        .unwrap_or_else(|_| "backup".to_string());
    let file_name = target
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "akhub".to_string());
    let backup = target.with_file_name(format!("{file_name}.bak-{stamp}"));
    match std::fs::copy(target, &backup) {
        Ok(_) => Ok(Some(backup)),
        Err(error) => {
            tracing::warn!(%error, "备份旧二进制失败，继续更新");
            Ok(None)
        }
    }
}

/// 提前确认目标目录可写：等下载完 20 MB 才发现没权限太浪费。
fn ensure_writable(directory: &Path) -> Result<(), String> {
    let probe = directory.join(format!(".akhub-write-test-{}", std::process::id()));
    match std::fs::File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(error) => Err(format!(
            "没有写入 {} 的权限（{error}）。请执行 sudo akhub --update，或改用一键安装脚本升级。",
            directory.display()
        )),
    }
}

fn truncate_chars(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(limit).collect();
    format!("{head}…")
}

// ------------------------------------------------------------------ 命令行入口

/// `akhub --update [--version vX.Y.Z]`。
///
/// 给 systemd 部署留的出路：服务进程自己往往没有写 `/usr/local/bin` 的权限
/// （单元文件里 `ProtectSystem=strict`），但用 sudo 跑同一个二进制就没有这层限制。
pub async fn cli_update(args: &[String]) -> i32 {
    let version = args
        .iter()
        .position(|arg| arg == "--version")
        .and_then(|index| args.get(index + 1))
        .map(|value| value.trim().trim_start_matches('v').to_string());

    println!("当前版本：v{}", current());
    let registry = Registry::default();
    match registry.install(&exe_path(), version.as_deref()).await {
        Ok(outcome) => {
            println!("已更新：v{} → v{}", outcome.from, outcome.to);
            println!("安装路径：{}", outcome.path);
            if let Some(backup) = outcome.backup {
                println!("旧版本备份：{backup}");
            }
            println!("请重启服务使新版本生效（{}）。", outcome.restart_command);
            0
        }
        Err(message) => {
            eprintln!("更新失败：{message}");
            1
        }
    }
}

// ------------------------------------------------------------------ GitHub 响应

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    #[test]
    fn versions_compare_numerically_not_lexically() {
        assert_eq!(compare_versions("1.10.0", "1.9.9"), Ordering::Greater);
        assert_eq!(compare_versions("v1.1.2", "1.1.2"), Ordering::Equal);
        assert_eq!(compare_versions("1.1", "1.1.0"), Ordering::Equal);
        assert_eq!(compare_versions("1.1.2", "1.1.2+build.7"), Ordering::Equal);
        assert_eq!(compare_versions("2.0.0", "1.99.99"), Ordering::Greater);
    }

    #[test]
    fn prereleases_rank_below_the_final_release() {
        assert_eq!(compare_versions("1.0.0-rc.1", "1.0.0"), Ordering::Less);
        assert_eq!(compare_versions("1.0.0", "1.0.0-rc.1"), Ordering::Greater);
        assert_eq!(compare_versions("1.0.0-rc.1", "1.0.0-rc.2"), Ordering::Less);
    }

    #[test]
    fn unparsable_tags_do_not_crash() {
        assert!(parse_version("nightly").is_none());
        assert_eq!(compare_versions("nightly", "1.0.0"), Ordering::Greater);
    }

    #[test]
    fn checksums_are_read_from_sha256sum_output() {
        let text = "d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2  ./akhub-v1.1.2-linux-x86_64.tar.gz\n\
            0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  *akhub-v1.1.2-windows-x86_64.exe\n";
        assert_eq!(
            parse_checksum(text, "akhub-v1.1.2-linux-x86_64.tar.gz"),
            Some("d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2d2".to_string())
        );
        assert!(parse_checksum(text, "akhub-v1.1.3-linux-x86_64.tar.gz").is_none());
    }

    /// 容器里也能就地替换自己，所以 `can_self_update` 与二进制一致。
    ///
    /// 这条在 2026-10-16 改过：早期容器不支持自更新（改不了只读镜像层里的
    /// 二进制），后来入口脚本把二进制复制到可写的数据卷再运行，容器与二进制
    /// 就走同一条路了（同机部署的 sub2api 用的就是这个办法）。
    #[test]
    fn container_deploys_offer_self_update_like_binaries() {
        let registry = Registry::new("http://127.0.0.1:1", false);
        let status = registry.decorate(Status {
            enabled: true,
            current: "1.0.0".into(),
            latest: Some("1.1.0".into()),
            has_update: true,
            release_url: None,
            release_name: None,
            published_at: None,
            notes: None,
            checked_at: 0,
            cached: false,
            error: None,
            deploy: Deploy::Docker,
            can_self_update: false,
            can_restart: false,
            update_hint: None,
            update_command: None,
            pending_version: None,
            can_one_click: false,
            progress: Progress::idle(),
        });
        // 提示语要按容器来说：不是"去宿主机敲命令"，而是"点一下就行"。
        let hint = status.update_hint.clone().unwrap_or_default();
        assert!(
            hint.contains("一键升级"),
            "容器部署的提示应当指向一键升级，而不是手工命令：{hint}"
        );
    }

    /// 升级收尾**不能死锁**。
    ///
    /// 这条钉的是一个真实死锁（复审时抓到，而且是在真机上跑出来的）：升级成功后
    /// 要写"已完成、正在重启"，而 `started_at` 取自进度锁。当时把它写成
    /// `self.progress.lock().unwrap().started_at` 放在 `set_progress(...)` 的
    /// **实参**位置——临时 `MutexGuard` 会活到整条语句结束，而 `set_progress`
    /// 内部又要锁同一个互斥量；std 的 `Mutex` 不可重入，于是当场死锁。
    ///
    /// 现场表现极具迷惑性：二进制已经替换成功、日志停在"准备重启"，进程既不
    /// 退出也不重启，管理接口也再没有任何响应。容器里这意味着 restart 策略永远
    /// 等不到进程退出，一键升级看起来完全没生效。
    ///
    /// 测试直接驱动生产方法（不是复制一段代码），所以把实现改回错误写法就会失败。
    #[test]
    fn finishing_an_upgrade_does_not_deadlock() {
        let registry = Registry::new("http://127.0.0.1:1", false);
        registry.set_progress(Progress {
            phase: "running".to_string(),
            to: Some("9.9.9".to_string()),
            step: Some("下载并校验新版本".to_string()),
            error: None,
            started_at: 12345,
        });

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            registry.mark_replaced_and_restarting("9.9.9");
            let _ = tx.send(());
        });

        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("升级收尾不该死锁：先读 started_at 再写进度必须能完成");
    }

    /// 收尾时开始时间要沿用原值，否则面板上的耗时会被重置。
    #[test]
    fn finishing_an_upgrade_keeps_the_original_start_time() {
        let registry = Registry::new("http://127.0.0.1:1", false);
        registry.set_progress(Progress {
            phase: "running".to_string(),
            to: None,
            step: None,
            error: None,
            started_at: 4242,
        });
        registry.mark_replaced_and_restarting("9.9.9");
        let progress = registry.progress();
        assert_eq!(progress.phase, "done");
        assert_eq!(progress.to.as_deref(), Some("9.9.9"));
        assert_eq!(progress.started_at, 4242, "开始时间必须沿用原值");
    }
}
