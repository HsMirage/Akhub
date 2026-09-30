// Akhub 桌面外壳（Tauri v2）。
//
// 设计上刻意保持"极简"：这里不做任何业务逻辑，只负责四件事——
//   1. 在 127.0.0.1 上挑一个空闲端口；
//   2. 用 Tauri externalBin 把 akhub 二进制作为 sidecar 拉起，并把数据目录 /
//      监听地址通过环境变量交给它（AKHUB_DATA_DIR / AKHUB_LISTEN）；
//   3. 轮询 http://127.0.0.1:<port>/health/ready，就绪后用 WebviewWindowBuilder
//      打开 http://127.0.0.1:<port>/admin；
//   4. 窗口关闭 / 进程退出时杀掉 sidecar，不留孤儿进程。
//
// 不在这里内嵌前端资源：管理后台已经由 akhub 二进制用 rust-embed 嵌进去了，
// 再放一份只会带来"两份前端版本不一致"的问题（frontendDist 因此只是占位目录）。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{Manager, RunEvent, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_shell::process::CommandChild;
use tauri_plugin_shell::ShellExt;

/// 就绪轮询总预算。空数据目录首次启动要建库、生成主密钥，正常 1~2 秒完成；
/// 给到 60 秒是为了覆盖首次运行时被杀软 / Spotlight / 冷启动拖慢的情况。
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// 两次探测之间的间隔。
const READY_INTERVAL: Duration = Duration::from_millis(150);
/// 单次 TCP 连接的上限，避免探测本身卡死。
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
/// 探测请求的读超时。
const READ_TIMEOUT: Duration = Duration::from_millis(1000);
/// 唯一窗口的 label。
const WINDOW_LABEL: &str = "main";

/// 记录 sidecar 的 pid 与端口。
///
/// 强杀外壳（Force Quit、kill -9、注销时被系统送走）不会执行 Tauri 的退出回调，
/// sidecar 会活下来。下次启动靠这份记录把它认出来并送走——否则孤儿进程会越积越多，
/// 每一个都占着内存和一份数据库连接。
const HINT_FILE: &str = "desktop-sidecar.json";

#[derive(serde::Serialize, serde::Deserialize)]
struct SidecarHint {
    pid: u32,
    port: u16,
}

#[derive(Default)]
struct SidecarState {
    child: Option<CommandChild>,
    /// 记录文件路径。为空表示还没走到建 sidecar 这一步。
    hint: std::path::PathBuf,
}

/// sidecar 子进程句柄与记录文件路径。放在 Tauri 的托管状态里，退出时统一回收。
struct Sidecar(Mutex<SidecarState>);

/// 让操作系统分配一个空闲端口。
///
/// 做法是先绑 127.0.0.1:0 拿到端口再立刻释放：从释放到 sidecar 真正监听之间
/// 存在极小的竞争窗口，但在桌面场景（单用户、本机）里可以接受；换成固定端口
/// 反而会与用户已有的 akhub / 其它服务打架。
fn free_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// 用最朴素的 HTTP/1.1 请求探测 /health/ready。
///
/// 这里刻意不引入 HTTP 客户端依赖：目标只有本机一个 GET，判断第一行是不是
/// 200 就够了，换来的是桌面外壳的依赖树保持最小。
fn is_ready(port: u16) -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
    let request = format!(
        "GET /health/ready HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let head = String::from_utf8_lossy(&buf);
    head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200")
}

/// 阻塞直到 sidecar 就绪；超时返回失败原因。
fn wait_ready(port: u16) -> Result<(), String> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if is_ready(port) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "{} 秒内 /health/ready 没有返回 200",
                READY_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(READY_INTERVAL);
    }
}

/// 结束 sidecar 并清掉记录文件。重复调用安全（take 之后就是 None）。
fn kill_sidecar(app: &tauri::AppHandle) {
    if let Some(state) = app.try_state::<Sidecar>() {
        if let Ok(mut guard) = state.0.lock() {
            if let Some(child) = guard.child.take() {
                if let Err(err) = child.kill() {
                    eprintln!("[akhub-desktop] 结束 sidecar 失败：{err}");
                }
            }
            if !guard.hint.as_os_str().is_empty() {
                let _ = std::fs::remove_file(&guard.hint);
            }
        }
    }
}

/// 送走一个进程。侧车只有在本机、由本外壳拉起，pid 也是我们自己记下来的。
fn terminate_process(pid: u32) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill").arg(pid.to_string()).status();
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
    }
}

/// 收掉上一次运行时被强杀留下的 sidecar。
///
/// 判据是"记录里的端口仍然有东西在响应 /health/ready"，不是 pid 还在不在：
/// pid 会被系统复用，端口还在服务才是同一个 akhub。收完等端口真正释放，
/// 免得紧接着的新 sidecar 撞上一个还没退干净的旧进程。
fn reap_previous_sidecar(hint: &std::path::Path) {
    let Ok(raw) = std::fs::read_to_string(hint) else {
        return;
    };
    let saved: SidecarHint = match serde_json::from_str(&raw) {
        Ok(saved) => saved,
        Err(_) => {
            let _ = std::fs::remove_file(hint);
            return;
        }
    };
    if is_ready(saved.port) {
        eprintln!(
            "[akhub-desktop] 收掉上一次遗留的 sidecar（pid={} port={}）",
            saved.pid, saved.port
        );
        terminate_process(saved.pid);
        let deadline = Instant::now() + Duration::from_secs(5);
        while is_ready(saved.port) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    let _ = std::fs::remove_file(hint);
}

/// sidecar 起不来时仍然把窗口开出来，把原因显示给用户，而不是静默退出。
fn show_startup_failure(app: &tauri::AppHandle, message: &str) {
    let Ok(window) =
        WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::App("index.html".into()))
            .title("Akhub — 启动失败")
            .inner_size(760.0, 420.0)
            .center()
            .build()
    else {
        eprintln!("[akhub-desktop] 无法创建错误提示窗口");
        return;
    };
    // 用 serde_json 编码成 JS 字符串字面量，避免消息里的引号 / 换行破坏脚本。
    if let Ok(literal) = serde_json::to_string(message) {
        let _ = window.eval(&format!("window.__akhubFail && window.__akhubFail({literal});"));
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(Sidecar(Mutex::new(SidecarState::default())))
        .setup(|app| {
            let handle = app.handle().clone();

            // 数据目录：系统应用数据目录（macOS: ~/Library/Application Support/<id>，
            // Windows: %APPDATA%/<id>）下的 data/，与服务端 ./data 的约定一致。
            let data_dir = app.path().app_data_dir()?.join("data");
            std::fs::create_dir_all(&data_dir)?;
            let hint = data_dir.join(HINT_FILE);
            // 先把上次可能遗留的 sidecar 收掉，再起新的。
            reap_previous_sidecar(&hint);
            app.state::<Sidecar>().0.lock().unwrap().hint = hint.clone();

            let port = free_port()?;
            let listen = format!("127.0.0.1:{port}");
            eprintln!(
                "[akhub-desktop] 启动 sidecar：AKHUB_LISTEN={listen} AKHUB_DATA_DIR={}",
                data_dir.display()
            );

            let (mut rx, child) = app
                .shell()
                .sidecar("akhub")?
                .env("AKHUB_DATA_DIR", data_dir.to_string_lossy().to_string())
                .env("AKHUB_LISTEN", listen)
                .spawn()?;
            let child_pid = child.pid();

            // 把 sidecar 的输出转写到本进程 stderr：出问题时这是唯一的线索来源。
            tauri::async_runtime::spawn(async move {
                use tauri_plugin_shell::process::CommandEvent;
                while let Some(event) = rx.recv().await {
                    match event {
                        CommandEvent::Stdout(line) | CommandEvent::Stderr(line) => {
                            eprint!("[akhub] {}", String::from_utf8_lossy(&line));
                        }
                        CommandEvent::Error(err) => eprintln!("[akhub] error: {err}"),
                        _ => {}
                    }
                }
            });

            {
                let state = app.state::<Sidecar>();
                let mut guard = state.0.lock().unwrap();
                guard.child = Some(child);
                // 记录要在 spawn 之后立刻写下：就绪之前被强杀同样会留孤儿。
                if let Ok(raw) = serde_json::to_string(&SidecarHint {
                    pid: child_pid,
                    port,
                }) {
                    let _ = std::fs::write(&hint, raw);
                }
            }

            if let Err(reason) = wait_ready(port) {
                let message = format!(
                    "akhub 服务未能在本机启动。\n\n{reason}\n\n数据目录：{}",
                    data_dir.display()
                );
                eprintln!("[akhub-desktop] {message}");
                show_startup_failure(&handle, &message);
                return Ok(());
            }

            let url = format!("http://127.0.0.1:{port}/admin");
            eprintln!("[akhub-desktop] sidecar 就绪，打开 {url}");
            WebviewWindowBuilder::new(app, WINDOW_LABEL, WebviewUrl::External(url.parse()?))
                .title("Akhub")
                .inner_size(1280.0, 820.0)
                .min_inner_size(960.0, 600.0)
                .center()
                .build()?;

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("构建 Akhub 桌面外壳失败")
        .run(|app, event| {
            // 最后一个窗口关闭（ExitRequested）与真正退出（Exit）都收一次：
            // 前者覆盖"用户关窗"，后者覆盖 macOS Cmd+Q / 系统注销。
            if let RunEvent::ExitRequested { .. } = event {
                kill_sidecar(app);
            }
            if let RunEvent::Exit = event {
                kill_sidecar(app);
            }
        });
}
