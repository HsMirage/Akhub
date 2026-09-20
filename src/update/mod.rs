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
    std::env::var_os("INVOCATION_ID").is_some()
        || std::env::var("AKHUB_ALLOW_RESTART").is_ok_and(|value| value == "1")
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
        }
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
    /// 部署形态覆盖：测试用它扮演二进制部署，免得去改进程环境变量。
    deploy: Option<Deploy>,
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
                .timeout(Duration::from_secs(300))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            api: Mutex::new(api.into()),
            disabled: AtomicBool::new(disabled),
            cache: Mutex::new(None),
            busy: AtomicBool::new(false),
            pending: Mutex::new(None),
            deploy: None,
        }
    }

    /// 覆盖部署形态（只给测试用；生产走 [`deploy`] 的自动判定与 AKHUB_DEPLOY）。
    pub fn with_deploy(mut self, deploy: Deploy) -> Self {
        self.deploy = Some(deploy);
        self
    }

    fn deploy(&self) -> Deploy {
        self.deploy.unwrap_or_else(deploy)
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
                Deploy::Docker => "已是最新版本；升级时用新的镜像 tag 重建容器。".to_string(),
                Deploy::Source => "已是最新版本；更新源码后重新编译即可。".to_string(),
                Deploy::Windows => "已是最新版本。".to_string(),
            })
        } else {
            Some(match status.deploy {
                Deploy::Binary => {
                    "可以直接点「立即更新」：下载 → 校验 sha256 → 原子替换二进制，重启服务后生效。"
                        .to_string()
                }
                Deploy::Docker => "容器部署升级的是镜像，请在宿主机执行下面这条命令。".to_string(),
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

    /// 把 `target` 指向的二进制换成最新版（或指定版本）。
    ///
    /// 整个过程只有最后一步 rename 会改动现场，之前的失败都只是白下载一次。
    pub async fn install(&self, target: &Path, version: Option<&str>) -> Result<Outcome, String> {
        if self.busy.swap(true, Ordering::SeqCst) {
            return Err("已经有一个更新在进行中，请稍候。".to_string());
        }
        let result = self.install_inner(target, version).await;
        self.busy.store(false, Ordering::SeqCst);
        result
    }

    async fn install_inner(&self, target: &Path, version: Option<&str>) -> Result<Outcome, String> {
        let deploy = self.deploy();
        if deploy != Deploy::Binary {
            return Err(match deploy {
                Deploy::Docker => format!(
                    "容器部署不支持自更新。请在宿主机执行：{}",
                    update_command(deploy, version).unwrap_or_default()
                ),
                Deploy::Source => {
                    "源码构建不支持自更新。请更新源码后重新编译（git pull && cargo build --release）。"
                        .to_string()
                }
                Deploy::Windows => {
                    "Windows 下运行中的程序无法覆盖自身。请下载新版 exe 或重跑 install.ps1。"
                        .to_string()
                }
                Deploy::Binary => unreachable!(),
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
        let asset = pick_asset(&release.assets, &latest).ok_or_else(|| {
            format!(
                "Release v{latest} 里没有适配当前平台（{}）的资产，请手工下载安装。",
                platform_label()
            )
        })?;
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
/// 公开出去是为了让测试能拼出与真实发行包同构的资产名；生产代码只用它挑文件。
pub fn asset_suffix() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("linux-x86_64-musl"),
        ("linux", "aarch64") => Some("linux-aarch64"),
        ("macos", "aarch64") => Some("macos-aarch64"),
        ("macos", "x86_64") => Some("macos-x86_64"),
        _ => None,
    }
}

fn platform_label() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// 按平台挑资产；Linux x86_64 优先静态链接的 musl 版，没有就退回 glibc 版。
fn pick_asset<'a>(assets: &'a [Asset], version: &str) -> Option<&'a Asset> {
    let suffix = asset_suffix()?;
    let wanted = format!("akhub-v{version}-{suffix}.tar.gz");
    if let Some(asset) = assets.iter().find(|item| item.name == wanted) {
        return Some(asset);
    }
    if suffix == "linux-x86_64-musl" {
        let fallback = format!("akhub-v{version}-linux-x86_64.tar.gz");
        return assets.iter().find(|item| item.name == fallback);
    }
    None
}

// ------------------------------------------------------------------ 下载与校验

async fn download(client: &reqwest::Client, url: &str, path: &Path) -> Result<u64, String> {
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

    #[test]
    fn container_deploys_never_offer_self_update() {
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
        });
        assert!(!status.can_self_update);
        assert!(
            status
                .update_command
                .unwrap_or_default()
                .contains("docker compose")
        );
    }

    #[test]
    fn source_builds_are_detected_by_the_target_directory() {
        assert!(looks_like_source_build(Path::new(
            "/home/me/akhub/target/release/akhub"
        )));
        assert!(!looks_like_source_build(Path::new("/usr/local/bin/akhub")));
    }

    #[test]
    fn assets_match_the_packaging_script_naming() {
        let assets = vec![
            Asset {
                name: "akhub-v1.1.2-linux-x86_64-musl.tar.gz".into(),
                browser_download_url: "https://example.invalid/musl".into(),
            },
            Asset {
                name: "akhub-v1.1.2-linux-x86_64.tar.gz".into(),
                browser_download_url: "https://example.invalid/glibc".into(),
            },
            Asset {
                name: "checksums.txt".into(),
                browser_download_url: "https://example.invalid/checksums".into(),
            },
        ];
        if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
            let picked = pick_asset(&assets, "1.1.2").unwrap();
            assert_eq!(picked.name, "akhub-v1.1.2-linux-x86_64-musl.tar.gz");
        }
        assert!(pick_asset(&assets, "1.1.9").is_none());
    }
}
