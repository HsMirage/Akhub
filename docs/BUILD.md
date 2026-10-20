# 构建与发布（平台上限：Linux + Windows）

这份文档只回答一件事：**这个项目在哪里编译、怎么编译、出什么产物**。
部署与运行看 [deploy/README.md](../deploy/README.md)，桌面端外壳看
[desktop/README.md](../desktop/README.md)。

---

## 1. 支持哪些平台

| 平台 | 架构 | 产物形态 | 是否构建 |
|---|---|---|---|
| Linux | x86_64 | 单文件二进制 `akhub`（tar.gz） | ✅ |
| Linux | aarch64 | 单文件二进制 `akhub`（tar.gz） | ✅ |
| Windows | x86_64 | 桌面端 NSIS 安装器（内含 sidecar） | ✅ |
| macOS | arm64 / x86_64 | —— | ❌ 已停止 |

**macOS 自 2026-10-20 起不再构建、不再发布。** CI 里的两个 macOS runner 已经删掉，
`scripts/release.sh` 的平台表里也去掉了对应条目。历史上（v1.1.17–v1.1.33）发过的
`.dmg` 仍留在旧 Release 上，可以继续下载，但不再更新。

不要再往这个仓库里加 macOS 目标：不补 runner、不补 `aarch64-apple-darwin` /
`x86_64-apple-darwin` 的构建步骤、也不修 macOS 专属问题。

这条上限有机械门禁：[scripts/check-platforms.sh](../scripts/check-platforms.sh)。
`ci.yml` 与 `release.yml` 都调用它（tag 发版不经过 `ci.yml`，所以两边都得卡）。
它扫的是真正会产出苹果产物的三处——`release.yml`、`scripts/release.sh`、`install.sh`；
文档里写"macOS 已停止支持"不算违规，所以不扫 `*.md`。
往这三处里加回 `macos-latest` 或 `*-apple-darwin`，CI 会直接失败。

---

## 2. 不要在本机（这台 Mac）上编译

**本仓库的完整构建不在开发机上跑**，这是硬性约定，不是建议。

原因是实测的：完整 `cargo test` 的中间产物在 `target/` 里能到 **20 GB 以上**，
把开发机的磁盘填满之后连日志都写不出来，还会顺带拖慢整台机器。2026-10-20 清掉的那份
`target/` 就是 23 GB。

具体红线：

- ❌ 不在本机跑 `cargo build` / `cargo test` / `cargo clippy` / `cargo zigbuild`
- ❌ 不在本机跑 `scripts/release.sh`（它本身就是交叉编译 + 全量测试）
- ❌ 不为了"试一下"就随手 `cargo run`
- ✅ 前端构建（`cd web && npm run build`）可以在本机跑：秒级，只产出 `web/dist`

> 例外只有一种：你已经确认本机磁盘余量充足，并且**明确只跑 `cargo check`**（不产出
> 可执行文件、不跑测试）。这仍然会写 `target/`，跑完请自己清掉。

### 那本机还能做什么

| 想做的事 | 怎么做 |
|---|---|
| 验证代码能不能编译 | 推到分支，让 CI 的 `check` job 跑（见 §3） |
| 跑全量测试 | 同上，CI 的 `cargo test --all --no-fail-fast` |
| 看格式/静态检查是否通过 | 同上，CI 跑 `cargo fmt --check` 与 `cargo clippy -D warnings` |
| 真的想要一个二进制 | 用 Docker 在 Linux 上编（§4.2），或直接跑 `docker compose up -d` |
| 只改管理后台 | 本机 `cd web && npm run build` 即可 |

---

## 3. 平时开发：CI 就是编译器

`.github/workflows/ci.yml` 在每次 push 与 PR 上跑四个关卡，全部在 GitHub 的
Ubuntu runner 上完成，不占本机资源：

| job | 内容 |
|---|---|
| `check` | 前端构建 → `cargo fmt --check` → `cargo clippy --all-targets -D warnings` → `cargo test --all` → 平台上限门禁 → 发布脚本语法检查 |
| `docker` | 真实构建运行镜像，在空数据目录上首次启动、真实 HTTP 调用、校验非 root 运行 |

所以日常流程是：**改代码 → push → 看 CI**。CI 绿了就等于"能编译、测试通过、格式没问题"，
不需要在本机复现一遍。

发布脚本（`install.sh`、`scripts/*.sh`、`deploy/docker-entrypoint.sh`）的语法也在
`check` job 里按 shebang 选解释器验证过（`sh -n` / `bash -n`）——这是运维唯一直接执行的
东西，语法错误不该等用户在服务器上敲下去才发现。

---

## 4. 正式产物从哪来

### 4.1 发版（推荐）：打 tag，交给 CI

```bash
# 1. 版本号必须一致：根 Cargo.toml、desktop/Cargo.toml、tag 三者对得上
grep -m1 '^version' Cargo.toml desktop/Cargo.toml

# 2. 打 tag 并推送
git tag v1.1.34
git push origin v1.1.34
```

`.github/workflows/release.yml` 被 tag 触发后：

1. `verify`：校验 tag 与根 `Cargo.toml`、`desktop/Cargo.toml` 的版本一致
2. `web`：构建管理后台（产物在编译期由 rust-embed 嵌进二进制）
3. `binaries`：在 **Linux x86_64 / Linux arm64 / Windows** 三个原生 runner 上编译
4. `desktop`：Windows runner 上用 Tauri 打 NSIS 安装器（sidecar 复用上一步的产物）
5. `release`：生成 `checksums.txt`，创建 GitHub Release
6. `docker` / `docker-manifest`：两个架构的镜像 + 多架构 manifest 推到 GHCR

Release 上只有这几个文件：

```
akhub-v<tag>-linux-x86_64.tar.gz
akhub-v<tag>-linux-aarch64.tar.gz
akhub-desktop-<tag>-windows-x86_64-setup.exe
checksums.txt
```

### 4.2 本地/离线构建：用 Docker，别用宿主机

需要在自己机器上出一个 Linux 二进制时，走容器——编译发生在容器里，宿主机只留镜像层：

```bash
# 运行镜像（构建与运行都在容器内）
docker build -f Dockerfile --target runtime -t akhub:local .

# 冒烟：空数据目录启动 + 真实请求
docker run --rm -p 127.0.0.1:8080:8080 -v akhub-data:/data akhub:local
curl -fsS http://127.0.0.1:8080/health/ready
```

> `Dockerfile` 的 `builder` 阶段用 `rust:1.96-slim-bookworm`，产出的就是 Linux 二进制。
>
> **Windows 产物只能由 CI 的 `windows-latest` runner 出。** `scripts/release.sh` 里虽然有
> `windows-x86_64` 这一项（走 `cargo-zigbuild`），但它需要一台已装好 zig 与对应 rustup target
> 的 Linux 构建机，且只用于本地预演——**正式产物一律以 CI 为准**。在这台 Mac 上什么都编不出。

### 4.3 `scripts/release.sh`：只给 Linux 构建机用

```bash
./scripts/release.sh                        # 全量：校验 + 全部平台 + 校验和
./scripts/release.sh --skip-tests           # 跳过 cargo test（仍跑 fmt/clippy）
./scripts/release.sh --only linux-aarch64   # 只构建指定平台
```

平台表只有三项：`linux-x86_64`、`linux-aarch64`、`windows-x86_64`。它依赖
`cargo-zigbuild` 与对应 rustup target，缺哪个就跳过哪个并给出提示。

**这个脚本会跑全量测试与交叉编译，不要在开发机上执行。**

---

## 5. 版本号与产物命名的约定

一处定义，几处共用——改的时候要一起改：

| 位置 | 作用 |
|---|---|
| 根 `Cargo.toml` 的 `version` | 唯一版本源，`--version` 与 `/health/version` 都取它 |
| `desktop/Cargo.toml` 的 `version` | 桌面安装包版本，必须与根 crate 一致（CI 卡） |
| `.github/workflows/release.yml` 的 `matrix.name` | 资产名里的平台段 |
| `scripts/release.sh` 的 `PLATFORMS` | 本地发版的平台表 |
| `scripts/package.sh` | 打包布局与命名，CI 与本地共用这一份 |
| `install.sh` 的平台探测 | 拼资产名去下载 |

资产名规则：`akhub-<tag>-<platform>.tar.gz`（Windows 桌面端是
`akhub-desktop-<tag>-windows-x86_64-setup.exe`），归档内层目录名 == 资产名去掉扩展名，
安装脚本依赖这一点定位可执行文件。

---

## 6. 桌面端（Windows）

`desktop/` 是**独立 crate**，不在根 crate 的 workspace 里，构建时必须用独立 target 目录：

```bash
CARGO_TARGET_DIR=desktop/target npx tauri build --target x86_64-pc-windows-msvc --bundles nsis
```

sidecar 的放置与完整流程见 [desktop/README.md](../desktop/README.md)。
`desktop/tauri.conf.json` 里的 `icon.icns` 是 macOS 图标，保留不影响 Windows 打包
（Tauri 按目标平台挑图标），不必删。

---

## 7. 检查清单

发版前对照一遍：

- [ ] 根 `Cargo.toml` 与 `desktop/Cargo.toml` 的版本号一致
- [ ] tag 是 `v<版本>`，与上面两个版本号一致
- [ ] CI 的 `check` 与 `docker` 两个 job 都是绿的
- [ ] 没有在本机跑过完整构建（`target/` 不该重新长回来）
- [ ] 本次改动没有重新引入 macOS 目标
