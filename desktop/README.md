# Akhub 桌面端（Tauri v2）

桌面外壳只做四件事：挑一个空闲端口 → 以 sidecar 方式拉起 `akhub` → 等
`/health/ready` 就绪 → 打开 `http://127.0.0.1:<port>/admin`。窗口关闭时杀掉
sidecar，不留孤儿进程。管理后台本身已经由 `akhub` 用 rust-embed 嵌进二进制，
这里不再放第二份前端（`dist/index.html` 只是 sidecar 启动失败时的提示页）。

## 目录

| 路径 | 说明 |
| --- | --- |
| `src/main.rs` | 外壳全部逻辑（选端口 / 起 sidecar / 轮询就绪 / 开窗 / 收尾） |
| `tauri.conf.json` | Tauri v2 配置：`externalBin: ["binaries/akhub"]`；窗口为空（由 Rust 在就绪后创建） |
| `binaries/` | sidecar 落点，文件名必须带目标三元组；由 `prepare-sidecar.sh` 生成，**不进版本库** |
| `dist/` | 占位页面（sidecar 起不来时显示失败原因） |
| `icons/` | 打包图标（`.icns` / `.ico` / png） |

## 本地构建

```bash
# 1) 先构建服务端二进制（管理后台会被 rust-embed 嵌进去）
cargo build --release

# 2) 放进 sidecar 位置（文件名必须带目标三元组）
./desktop/prepare-sidecar.sh aarch64-apple-darwin target/release/akhub
#   Windows: ./desktop/prepare-sidecar.sh x86_64-pc-windows-msvc \
#            target/x86_64-pc-windows-msvc/release/akhub.exe

# 3) 打包
cd desktop
npm ci
CARGO_TARGET_DIR="$PWD/target" npx tauri build --bundles dmg        # macOS
CARGO_TARGET_DIR="$PWD/target" npx tauri build --bundles msi,nsis  # Windows
```

产物（macOS）：`desktop/target/release/bundle/dmg/Akhub_<版本>_<arch>.dmg`，
展开后的应用在 `desktop/target/release/bundle/macos/Akhub.app`。

`desktop/` 是**独立 crate**（不在根 crate 的 workspace 里）。构建时务必用
`CARGO_TARGET_DIR=desktop/target`，否则会和根 `target/` 抢文件锁、也会把两种
完全不同的构建产物混在一起。

## 运行时行为

- 数据目录：系统应用数据目录下的 `data/`（macOS：`~/Library/Application Support/com.hsmirage.akhub/data`；
  Windows：`%APPDATA%\com.hsmirage.akhub\data`）。首次启动在那里生成 `master.key` 与 `akhub.sqlite`。
- 服务只监听 `127.0.0.1`，端口由操作系统分配（先绑 `:0` 再释放），不对外暴露。
- sidecar 的 stdout / stderr 会转写到外壳进程的 stderr：出问题时这是第一条线索。
- 退出路径覆盖「关最后一个窗口」与 macOS `Cmd+Q` / 系统注销，两种情况都会杀掉 sidecar。

## 首次打开的放行（安装包未签名）

仓库没有 Apple Developer ID 与 Windows 代码签名证书，所以安装包是未签名的：

- **macOS**：把 Akhub 拖进「应用程序」后第一次打开会被 Gatekeeper 拦下。
  右键应用 →「打开」→ 再点「打开」；或者先执行
  `xattr -dr com.apple.quarantine /Applications/Akhub.app`。
- **Windows**：SmartScreen 会提示「已保护你的电脑」，点「更多信息」→「仍要运行」。

## CI

`.github/workflows/release.yml` 的 `desktop` job 在推 `v*` tag 时构建三份产物，
并挂到与裸二进制同一个 Release 下：

| 平台 | 产物 |
| --- | --- |
| macOS arm64 | `akhub-desktop-<tag>-macos-arm64.dmg` |
| macOS x86_64 | `akhub-desktop-<tag>-macos-x86_64.dmg` |
| Windows x86_64 | `akhub-desktop-<tag>-windows-x86_64-setup.exe` |

Release 上同时还有 Linux 的两个服务端包（`akhub-v<tag>-linux-{x86_64,aarch64}.tar.gz`）；
**macOS / Windows 的裸二进制从这里起不再发布**，只有桌面安装包。

sidecar 直接复用 `binaries` job 已发布的 akhub 工件（macOS 取 `.tar.gz` 里的二进制，
Windows 取裸 `.exe`），因此「桌面端里跑的 akhub」与用户手工下载的是同一份产物；
`checksums.txt` 也覆盖这些安装包。

本地验证过的命令（macOS arm64，v1.1.17）：

```bash
cargo build --release
./desktop/prepare-sidecar.sh aarch64-apple-darwin target/release/akhub
cd desktop && CARGO_TARGET_DIR="$PWD/target" npx tauri build --bundles dmg
```