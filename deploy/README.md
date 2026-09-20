# Akhub 部署指南（§25）

四种部署方式，按「从最省事到最可控」排列。选一种就行，不用全看。

| 方式 | 适合 | 升级方式 | 文档 |
|---|---|---|---|
| **Docker / Compose** | 绝大多数 Linux 服务器 | `docker compose pull` 或换 tag 重启 | 本文 §1 |
| **一键脚本（Linux / macOS）** | 不想装 Docker、想直接用系统服务 | 重跑同一行命令 | 本文 §2 |
| **systemd 原生** | 需要精细控制权限与环境变量 | 换二进制 + `systemctl restart` | 本文 §3 |
| **Windows** | Windows Server / 桌面 | `install.ps1` 重跑 | [README.windows.md](README.windows.md) |
| **源码构建** | 开发者、需要打补丁 | 自己重新编译 | 仓库根目录 README「开发」一节 |

四种方式共用同一份二进制与同一个数据目录约定，数据可以在它们之间搬来搬去：
数据目录里只有 `akhub.sqlite`（配置与请求元数据）和 `master.key`（主密钥）。

> **升级前必读**：数据目录里的 `master.key` 一旦丢失，数据库中加密保存的
> 上游 API Key **无法恢复**。备份数据目录时务必带上它，或改用 `AKHUB_MASTER_KEY`
> 自行托管密钥。另外建议在管理后台「设置 → 配置备份」导出一份口令加密的配置备份
> （不含请求记录）。

---

## 0. 先决定三件事

1. **监听地址**。默认 `127.0.0.1:8080`，只对本机开放。要对外提供服务，
   推荐让 Caddy / Nginx 终止 HTTPS 后反代过来（§5），而不是把 Akhub 直接暴露。
2. **数据目录**。容器里固定是 `/data`；原生部署默认 `./data`，systemd 用
   `/var/lib/akhub`。这是唯一需要持久化和备份的目录。
3. **主密钥归属**。默认自动生成在数据目录里；需要脱离数据目录管理时用
   `AKHUB_MASTER_KEY` 注入 32 字节 hex 或 base64。

部署完成后，打开 `http://<监听地址>/admin` 设置管理员密码，然后在「账号 →
模型管理」里配好上游即可。**首次启动前不需要任何配置文件。**

---

## 1. Docker / Compose

镜像在 GHCR 上，多架构（`linux/amd64`、`linux/arm64`），
非 root 运行，只暴露一个 `/data` 持久卷。

### 1.1 开箱即用

仓库里的 `docker-compose.yml` 默认拉 GHCR 镜像：

```bash
docker compose up -d
# 或指定版本
AKHUB_IMAGE=ghcr.io/hsmirage/akhub:1.1.3 docker compose up -d
```

没有 compose 时用裸 `docker run`：

```bash
docker run -d \
  --name akhub \
  --restart unless-stopped \
  -p 127.0.0.1:8080:8080 \
  -v akhub-data:/data \
  --stop-timeout 200 \
  -e RUST_LOG=akhub=info,warn \
  ghcr.io/hsmirage/akhub:latest
```

`--stop-timeout 200` 不是可选项：Docker 默认只给 10 秒停止宽限，会在
「优雅关闭」的 180 秒宽限期走完之前就 `SIGKILL` 掉在途的长流式请求（§25.3）。
compose 里对应的写法是 `stop_grace_period: 200s`，仓库文件里已经配好。

然后打开 `http://127.0.0.1:8080/admin`。

### 1.2 就地构建

不想用预构建镜像时：

```bash
docker compose build
docker compose up -d
```

或直接用脚本（多架构、可推送，见 `scripts/docker-build.sh --help`）：

```bash
./scripts/docker-build.sh --load          # 构建本机架构并载入本地镜像
./scripts/docker-build.sh                 # 构建 amd64+arm64 到 buildx 缓存
./scripts/docker-build.sh --push --tag myregistry/akhub:dev
```

> QEMU 下模拟 arm64 编译 Rust + `aws-lc-sys` 的 C 代码会慢一个数量级。
> 需要多架构镜像时，请让每个架构都在自己的原生机器上构建
> （CI 就是这么做的，见 `.github/workflows/release.yml`），别指望在 x86 上
> `--platform linux/arm64` 一把梭。

### 1.3 数据卷

用**命名卷**（默认）时，Docker 会把镜像里 `/data` 的属主（uid 10001）带到卷上，
容器里的非 root 用户开箱即用。

改成 bind mount 时必须先对齐属主，否则启动会以
「创建主密钥文件失败：Permission denied」退出：

```bash
sudo mkdir -p /srv/akhub/data
sudo chown 10001:10001 /srv/akhub/data
```

镜像的 entrypoint 会尝试自动纠正目录属主（以 root 起步、随即 `gosu` 降权到
10001），但它**只改目录本身，不改里面的文件**，所以宿主侧先 `chown` 仍然是正确做法。

### 1.4 备份

```bash
# 只备份配置与密钥（体积小，包含 SQLite + 主密钥）
docker run --rm -v akhub-data:/data -v "$PWD:/backup" alpine \
  tar -czf /backup/akhub-$(date +%F).tar.gz -C /data akhub.sqlite master.key

# 恢复：先停容器，再解回去
docker compose stop
docker run --rm -v akhub-data:/data -v "$PWD:/backup" alpine \
  tar -xzf /backup/akhub-2026-09-20.tar.gz -C /data
docker compose start
```

SQLite 用 WAL 模式，所以直接拷 `akhub.sqlite` 可能拿到不一致的快照。
上面的做法在容器停止后执行，是安全的；要热备份就用
`sqlite3 /data/akhub.sqlite ".backup /data/backup.sqlite"`。

### 1.5 健康检查

镜像内置 `HEALTHCHECK`，用的是二进制自己的探测子命令：

```bash
docker inspect --format '{{.State.Health.Status}}' akhub   # healthy
docker exec akhub /usr/local/bin/akhub --healthcheck        # 手动跑一次
```

可以直接打到 HTTP 上：

| 端点 | 含义 | 用途 |
|---|---|---|
| `/health/live` | 进程事件循环正常 | liveness 探针 |
| `/health/ready` | 数据库、配置与主密钥已就绪 | readiness 探针、升级后的验收点 |
| `/health/version` | `{"status":"ok","version":"1.1.3"}` | 确认部署的是哪个版本 |

三个端点都不需要凭据，也不暴露账号、模型或倍率信息。

---

## 2. 一键安装脚本（Linux / macOS）

### 2.1 Linux

```bash
curl -fsSL https://raw.githubusercontent.com/HsMirage/Akhub/master/install.sh | sh
```

建议先下载看一眼再执行：

```bash
curl -fsSLO https://raw.githubusercontent.com/HsMirage/Akhub/master/install.sh
less install.sh
sh install.sh --version v1.1.3 --service
```

脚本做这些事：探测平台 → 下载对应资产 → 用 `checksums.txt` 校验 sha256 →
解包 → 备份已有的 `akhub` → 安装到 `/usr/local/bin/akhub`。加 `--service`
还会创建 `akhub` 系统用户、装好 systemd 单元并启动。

常用参数（`sh install.sh --help` 有完整列表）：

| 参数 | 说明 |
|---|---|
| `--version vX.Y.Z` | 装指定版本，默认取最新 Release |
| `--dir <path>` | 安装目录，默认 `/usr/local/bin` |
| `--libc glibc` | Linux 上改用 glibc 版本（默认 `musl` 静态链接） |
| `--service` | 额外安装并启用 systemd 单元 |
| `--dry-run` | 只打印将要做什么 |
| `AKHUB_BASE_URL` | 改用自建镜像下载根，目录结构为 `<根>/<tag>/<资产名>` |

**为什么 Linux 默认给 musl 版**：静态链接，不依赖目标机器的 glibc 版本，
在任何发行版上都是拷过去就能跑。发行包同时提供 glibc 版
（`linux-x86_64`、`linux-aarch64`），装完跑不起来时用 `--libc glibc` 换一个。

### 2.2 macOS

同一个脚本，macOS 上会去拿 `macos-aarch64` 或 `macos-x86_64` 资产：

```bash
curl -fsSL https://raw.githubusercontent.com/HsMirage/Akhub/master/install.sh | sh
```

macOS 没有 systemd，脚本装完只打印后续步骤，`--service` 在 macOS 上会被跳过并提示。

直接跑（前台）：

```bash
AKHUB_DATA_DIR="$HOME/Library/Application Support/Akhub" akhub
```

后台常驻推荐用 launchd，写一个 plist：

```bash
cat > ~/Library/LaunchAgents/com.hsmirage.akhub.plist <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.hsmirage.akhub</string>
  <key>ProgramArguments</key>
  <array><string>/usr/local/bin/akhub</string></array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>AKHUB_DATA_DIR</key><string>/Users/you/Library/Application Support/Akhub</string>
    <key>AKHUB_LISTEN</key><string>127.0.0.1:8080</string>
    <key>RUST_LOG</key><string>akhub=info,warn</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/tmp/akhub.log</string>
  <key>StandardErrorPath</key><string>/tmp/akhub.err</string>
  <!-- 与 §25.3 的 180 秒宽限期对齐；退出超时太短会掐断在途流式请求。 -->
  <key>ExitTimeOut</key><integer>200</integer>
</dict>
</plist>
PLIST

launchctl load ~/Library/LaunchAgents/com.hsmirage.akhub.plist
launchctl list | grep akhub
```

> Gatekeeper 提示"无法验证开发者"时，二进制没有签名。可以右键 →「打开」，
> 或 `xattr -d com.apple.quarantine /usr/local/bin/akhub`。

### 2.3 Windows

见 [README.windows.md](README.windows.md)。一行版本：

```powershell
irm https://raw.githubusercontent.com/HsMirage/Akhub/master/install.ps1 | iex
```

---

## 3. Linux 原生部署（systemd，§25.2）

适合不使用 Docker、由 systemd 管理的单机部署。

### 3.1 安装

用 §2 的脚本加 `--service` 是最快的路径。手工做的话：

```bash
# 1. 二进制
tar -xzf akhub-v1.1.3-linux-x86_64-musl.tar.gz
sudo install -m755 akhub-v1.1.3-linux-x86_64-musl/akhub /usr/local/bin/akhub

# 2. 系统用户（单元里写死了 User=akhub）
sudo useradd --system --home-dir /var/lib/akhub --create-home akhub

# 3. systemd 单元
sudo install -m644 deploy/akhub.service /etc/systemd/system/akhub.service
sudo systemctl daemon-reload
sudo systemctl enable --now akhub
```

单元文件里有四处不能改错：

- `Restart=always`：单实例下进程故障是常态恢复路径，重启代价应压到数秒
  （§29 的已知边界）。
- `TimeoutStopSec=200`：必须**大于** `AKHUB_SHUTDOWN_GRACE_SECS`（默认 180），
  否则 systemd 会在宽限期走完前 `SIGKILL` 掉在途的长流式请求（§25.3）。
- `StateDirectory=akhub` / `ReadWritePaths=/var/lib/akhub`：配合
  `ProtectSystem=strict`，只允许写自己的数据目录。
- `Environment=AKHUB_LISTEN=127.0.0.1:8080`：默认只监听环回，由反代对外。

### 3.2 验证

```bash
curl -s http://127.0.0.1:8080/health/ready          # 200 = 数据库、配置、主密钥就绪
curl -s http://127.0.0.1:8080/health/version        # 确认版本
sudo /usr/local/bin/akhub --healthcheck             # 容器外复用同一段探测逻辑
sudo journalctl -u akhub -f
```

### 3.3 升级

推荐用脚本（可在任意机器上对远端执行，见 §4.2），或手工：

```bash
sudo systemctl stop akhub          # SIGTERM，等待在途请求完成
sudo install -m755 akhub /usr/local/bin/akhub
sudo systemctl start akhub
```

数据库结构版本随启动自动校验：用**更旧的**二进制打开**更新的**数据库会被拒绝并
提示升级（§27），不会写坏数据。

---

## 4. 升级与回滚

### 4.1 通用原则

1. **先备份。** 数据目录里的 `akhub.sqlite` 与 `master.key`，以及在后台导出的配置备份。
2. 停服 → 换二进制 → 起服 → 等 `/health/ready` 返回 200 → 打一次真实请求。
3. 失败了就换回上一份二进制。所有安装路径都会自动留一份
   `akhub.bak-<时间戳>`（`install.sh`、`install.ps1`、`scripts/deploy.sh` 都是）。

升级不会改动数据目录的布局，所以 Docker ↔ 原生之间可以直接搬数据目录
（注意属主：容器里是 uid 10001）。

### 4.2 远端一键升级（`scripts/deploy.sh`）

本地构建前端与 Linux x86_64 二进制，上传后原子替换、重启并等待 `/health/ready`；
健康检查失败会自动回滚到上一份二进制。

```bash
AKHUB_DEPLOY_HOST=1.2.3.4 ./scripts/deploy.sh
./scripts/deploy.sh --host 1.2.3.4 --port 22 --key ~/.ssh/deploy_key
./scripts/deploy.sh --skip-build              # 复用已构建产物
```

**目标主机必须显式给出**：脚本不内置任何默认服务器（避免把个人基础设施信息
写进公开仓库），缺了会直接报错并打印用法。

环境变量：`AKHUB_DEPLOY_HOST`（必填）、`AKHUB_DEPLOY_PORT`（默认 22）、
`AKHUB_DEPLOY_KEY`（默认 `~/.ssh/id_ed25519`）、`AKHUB_DEPLOY_USER`（默认 `root`）、
`AKHUB_DEPLOY_SERVICE`、`AKHUB_DEPLOY_HEALTH`。

手动回滚：

```bash
sudo systemctl stop akhub
sudo install -m755 /usr/local/bin/akhub.bak-YYYYmmdd-HHMMSS /usr/local/bin/akhub
sudo systemctl start akhub
```

### 4.3 容器升级

```bash
docker compose pull && docker compose up -d
# 指定版本
AKHUB_IMAGE=ghcr.io/hsmirage/akhub:1.1.3 docker compose up -d
```

回滚就是换回旧 tag。数据卷不动，SQLite 结构会在需要时自动迁移。

### 4.4 后台一键升级与 `akhub --update`

后台左上角的版本号点开就是「版本与更新」面板：它比对 GitHub Release，显示当前版本、
最新版本与这台机器的部署方式，并在有新版本时给出升级入口。

- **原生二进制部署**点「立即更新」：服务端下载对应平台的资产 → 用 `checksums.txt` 校验
  sha256 → 把旧二进制备份成 `akhub.bak-<时间戳>` → 原子替换；随后点「重启服务」生效
  （重启按钮只在检测到 systemd 之类的监督进程时出现）。
- **systemd 加固部署**（单元里有 `ProtectSystem=strict`）通常没有写 `/usr/local/bin` 的权限，
  面板会给出等价的命令行版本：

```bash
sudo akhub --update                    # 升到最新 Release
sudo akhub --update --version v1.1.2   # 装指定版本（也是回退手段）
sudo systemctl restart akhub
```

`--update` 不读数据目录、不碰数据库，也不依赖 `curl`：适合只装了二进制、手边没有
`install.sh` 的机器。校验不过（sha256 不匹配、Release 里没有对应平台的资产）时它会
明确报错并保持原二进制不动。

容器部署请在宿主机换镜像 tag；Windows 下运行中的 exe 无法自我覆盖，请下载新版或重跑
`install.ps1`。不想让服务端访问 GitHub 时，设 `AKHUB_UPDATE_DISABLED=1` 关闭检查。

---

## 5. 反向代理终止 HTTPS（§25.2）

Akhub 第一期不自行管理证书。把 TLS 交给 Caddy 或 Nginx，Akhub 监听明文 HTTP。

仓库里有两份可直接用的示例：

- `deploy/Caddyfile`：Caddy v2，自动申请续期证书，最省事。
- `deploy/nginx.conf`：Nginx，含 SSE 与限流配置。

### 5.1 两个必须做对的点

**必须转发 `X-Forwarded-Proto`。** Akhub 用这个头判断请求是不是走 HTTPS 进来的，
进而决定会话 Cookie 要不要加 `Secure`（§23.2）。不转发的话，HTTPS 部署下会话
Cookie 会缺少 `Secure` 保护。

**必须关闭响应缓冲（Nginx 的 `proxy_buffering off`）。** SSE 是长连接流式响应，
一旦被反代攒着发，客户端看到的就不再是"首字延迟"而是"整段延迟"，流式等于白做。
超时也要放大到超过 `AKHUB_REQUEST_TIMEOUT_SECS`（默认 600 秒）。

两份示例里这两点都已经配好，直接抄即可。

### 5.2 Caddy

```bash
sudo cp deploy/Caddyfile /etc/caddy/Caddyfile   # 改掉域名与邮箱
sudo systemctl reload caddy
```

Caddy 会自动处理证书、HTTP/2 与 `X-Forwarded-Proto`。Windows 上也有单 exe 版本，
见 `deploy/Caddyfile.windows` 与 [README.windows.md](README.windows.md)。

### 5.3 Nginx

```bash
sudo cp deploy/nginx.conf /etc/nginx/conf.d/akhub.conf
sudo nginx -t && sudo systemctl reload nginx
sudo certbot --nginx -d akhub.example.com       # 它会自动填入证书路径
```

Nginx 的 `client_max_body_size` 必须 ≥ `AKHUB_MAX_REQUEST_BYTES`（默认 64 MiB），
否则大图请求会在到达网关前就被 413 掉，错误信息也指不到真正的原因。示例里已配 64m。

---

## 6. 环境变量

| 变量 | 默认值 | 说明 |
|---|---|---|
| `AKHUB_LISTEN` | `127.0.0.1:8080` | 监听地址 |
| `AKHUB_DATA_DIR` | `./data` | SQLite、主密钥与临时文件所在目录 |
| `AKHUB_MASTER_KEY` | 无 | 32 字节 hex 或 base64；设置后优先于数据目录中的密钥文件 |
| `AKHUB_REQUEST_TIMEOUT_SECS` | `600` | 请求总超时 |
| `AKHUB_MAX_REQUEST_BYTES` | `67108864` | 单请求体上限，超过返回 413 |
| `AKHUB_SHUTDOWN_GRACE_SECS` | `180` | 关闭时给在途请求的完成时间 |
| `AKHUB_MULTIPLIER_REFRESH_SECS` | `300` | 自动倍率的刷新间隔，各账号叠加 0–25% 抖动 |
| `AKHUB_RETENTION_DAYS` | `30` | 请求元数据保留天数，0 表示不新增历史明细 |
| `AKHUB_RESPONSE_STATE_DAYS` | `30` | Responses 可重放状态保留天数，0 表示只存最小定位映射 |
| `AKHUB_MODEL_SYNC_SECS` | `1800` | 模型自动同步间隔，各账号叠加 ±10% 抖动 |
| `RUST_LOG` | `akhub=info,warn` | 日志级别 |

命令行参数（`akhub --help`）：

| 参数 | 说明 |
|---|---|
| 无 | 启动服务 |
| `--healthcheck` | 探测本机 `/health/ready`，就绪退出码 0，否则非 0 |
| `--version` / `-V` | 打印版本号 |
| `--help` / `-h` | 显示帮助 |

---

## 7. 排错

**启动即退出，日志说"创建主密钥文件失败：Permission denied"**
数据目录属主不对。容器场景用命名卷，或把宿主目录 `chown 10001:10001`；
原生部署确认 `User=akhub` 对 `/var/lib/akhub` 有写权限。

**`/health/ready` 一直是 503**
看日志。就绪检查覆盖数据库、配置与主密钥三者；数据库损坏时会明确报错而不是
静默重建（§26.8）。

**升级后还是旧版本**
`curl -s http://127.0.0.1:8080/health/version` 确认实际在跑的是哪个二进制。
常见的坑是装了新二进制但服务指向了另一份（systemd 的 `ExecStart` 路径、
容器里挂载覆盖了 `/usr/local/bin/akhub`）。

**流式响应被掐断**
停止宽限期没配够。systemd 的 `TimeoutStopSec`、Docker 的
`stop_grace_period`/`--stop-timeout`、反向代理的读超时，三者都要 ≥
`AKHUB_SHUTDOWN_GRACE_SECS`。反代还要关掉缓冲。

**大请求 413，但网关日志里没有记录**
多半是反向代理先拒了。把 `client_max_body_size`（Nginx）或
`request_body max_size`（Caddy）调到 ≥ `AKHUB_MAX_REQUEST_BYTES`。

**登录后台后立刻掉线**
HTTPS 部署下没转发 `X-Forwarded-Proto`，会话 Cookie 的 `Secure` 判定与实际协议
对不上。见 §5.1。

**容器里进程为什么是 uid 10001**
这是刻意的，非 root 运行（§25.1）。entrypoint 只在启动瞬间用 root 纠正数据目录
属主，随后立刻降权。

---

## 8. 相关文件

| 文件 | 用途 |
|---|---|
| `install.sh` / `install.ps1` | 一键安装升级（Linux/macOS / Windows） |
| `docker-compose.yml` / `Dockerfile` | 容器部署 |
| `deploy/akhub.service` | systemd 单元 |
| `deploy/docker-entrypoint.sh` | 容器入口，处理数据目录属主后降权 |
| `deploy/Caddyfile` / `deploy/Caddyfile.windows` / `deploy/nginx.conf` | 反向代理 |
| `scripts/deploy.sh` | 远端一键升级（SSH） |
| `scripts/release.sh` | 本地发版：多平台交叉编译 + 打包 + 校验和 |
| `scripts/package.sh` | 打包单个平台（CI 与本地共用） |
| `scripts/docker-build.sh` | 本地多架构镜像构建 |
| `.github/workflows/release.yml` | CI 发版：6 个平台 + 多架构镜像 |
