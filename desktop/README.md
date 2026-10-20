# Akhub 桌面端（Tauri v2，Windows）

桌面外壳只做四件事：挑一个空闲端口 → 以 sidecar 方式拉起 `akhub` → 等
`/health/ready` 就绪 → 打开 `http://127.0.0.1:<port>/admin`。窗口关闭时杀掉
sidecar，不留孤儿进程。管理后台本身已经由 `akhub` 用 rust-embed 嵌进二进制，
这里不再放第二份前端（`dist/index.html` 只是 sidecar 启动失败时的提示页）。

> **只打 Windows。** macOS 自 2026-10-20 起不再构建也不再发布（见
> [docs/BUILD.md](../docs/BUILD.md) §1）。`src/main.rs` 里仍留着几处 macOS 注释与
> 防呆分支，那是历史代码，不影响 Windows 打包，也不必为了"清理"去动它。

## 目录

| 路径 | 说明 |
| --- | --- |
| `src/main.rs` | 外壳全部逻辑（选端口 / 起 sidecar / 轮询就绪 / 开窗 / 收尾） |
| `tauri.conf.json` | Tauri v2 配置：`externalBin: ["binaries/akhub"]`；窗口为空（由 Rust 在就绪后创建） |
| `binaries/` | sidecar 落点，文件名必须带目标三元组；由 `prepare-sidecar.sh` 生成，**不进版本库** |
| `dist/` | 占位页面（sidecar 起不来时显示失败原因） |
| `icons/` | 打包图标（`.ico` / png；`icon.icns` 是 macOS 遗留，保留不影响 Windows） |

## 构建 Windows 安装包

正式产物由 CI 出（`.github/workflows/release.yml` 的 `desktop` job，跑在
`windows-latest` 上）。本地复现需要一台 Windows 机器或 Windows 容器：

```bash
# 1) 先构建服务端二进制（管理后台会被 rust-embed 嵌进去）
cargo build --release --target x86_64-pc-windows-msvc

# 2) 放进 sidecar 位置（文件名必须带目标三元组）
./desktop/prepare-sidecar.sh x86_64-pc-windows-msvc \
    target/x86_64-pc-windows-msvc/release/akhub.exe

# 3) 打包（只打 NSIS：安装入口留一个就够）
cd desktop
npm ci
CARGO_TARGET_DIR="$PWD/target" npx tauri build \
    --target x86_64-pc-windows-msvc --bundles nsis
```

产物：`desktop/target/x86_64-pc-windows-msvc/release/bundle/nsis/*-setup.exe`。

`desktop/` 是**独立 crate**（不在根 crate 的 workspace 里）。构建时务必用
`CARGO_TARGET_DIR=desktop/target`，否则会和根 `target/` 抢文件锁、也会把两种
完全不同的构建产物混在一起。

> `cargo build` 这一步不要在开发机上跑：完整构建会写掉 20 GB 级别的 `target/`。
> 见 [docs/BUILD.md](../docs/BUILD.md) §2。

## 运行时行为

- 数据目录：系统应用数据目录下的 `data/`（Windows：`%APPDATA%\com.hsmirage.akhub\data`）。
  首次启动在那里生成 `master.key` 与 `akhub.sqlite`。
- 服务只监听 `127.0.0.1`，端口由操作系统分配（先绑 `:0` 再释放），不对外暴露。
- sidecar 的 stdout / stderr 会转写到外壳进程的 stderr：出问题时这是第一条线索。
- 退出路径覆盖「关最后一个窗口」与系统注销，两种情况都会杀掉 sidecar。

## 首次运行被 SmartScreen 拦下

仓库没有 Windows 代码签名证书，安装包是未签名的：

- SmartScreen 会提示「已保护你的电脑」，点「更多信息」→「仍要运行」。
- 企业环境可能被组策略直接拦掉未签名安装包，那就改用 Docker 或
  [deploy/README.windows.md](../deploy/README.windows.md) 里的部署方式。

## CI 产物

| 平台 | 产物 |
| --- | --- |
| Windows x86_64 | `akhub-desktop-<tag>-windows-x86_64-setup.exe` |

Release 上同时还有 Linux 的两个服务端包（`akhub-v<tag>-linux-{x86_64,aarch64}.tar.gz`）。
**Windows 的裸二进制不发布**，只有桌面安装包。

sidecar 直接复用 `binaries` job 已发布的裸 `.exe`，因此「桌面端里跑的 akhub」与 CI 构建
的是同一份产物；`checksums.txt` 也覆盖这个安装包。
