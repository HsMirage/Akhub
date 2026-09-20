# Akhub

一个简单、可靠、高性能的 AI 协议网关与组内负载均衡工具。

> 许可证说明：本项目为源码公开、非商业许可项目，不是 OSI 定义的“开源软件”。
> 非商业使用必须保留署名；商业使用需事先取得书面授权，并明确注明 Akhub、HsMirage
> 及源码仓库来源。详见 [LICENSE](LICENSE)。

完整的产品与技术方案见 [`计划.md`](计划.md)。本文件只描述**当前已实现的部分**。

## 当前状态：核心链路可运行，功能完整性与发布验证尚未完成

阶段 1 的目标是"三个入口能真实干活"——同协议原生透传，让 Claude Code、Codex CLI
和 OpenAI Chat 客户端都能通过 Akhub 完成真实工作。阶段 2 的目标是"可靠组内路由"：
多目标之间按严格优先级阶梯与层内评分调度，失败时在不重复浪费的前提下切换。
阶段 3 的目标是"跨协议互转"：一个分组里只有 Messages 端点、而客户端说的是
Chat Completions 时，请求照样能送达，且丢什么能力事先说了算。阶段 4 加入
Responses 状态链与模型发现（选择集、别名、自动同步、能力学习）。阶段 5 加入
成本视图、校准助手、账号复制/测试与配置备份恢复。

已实现：

- Rust 服务、SQLite（WAL）、主密钥与首次管理员设置。
- 分组、下游 Key、上游账号、逻辑模型与调度目标。
- 三个入口的**同协议原生透传**，普通与 SSE：
  - `POST /v1/chat/completions`
  - `POST /v1/messages`
  - `POST /v1/messages/count_tokens`
  - `POST /v1/responses`
  - `POST /v1/images/generations` 与 `POST /v1/images/edits`（仅原生 OpenAI 兼容上游）
- `GET /v1/models` 与 `/v1/models/{model}`，响应形状按鉴权头双形态切换。
- 请求 ID、稳定错误码、§18.3 的 HTTP 状态码映射、上游凭据加密与日志脱敏。
- **严格优先级阶梯**：优先级数字相同的目标构成一层，高层只要还有合格目标就
  优先使用高层；常规路径层内全忙时在本层排队，不降层。普通粘性绑定也只在
  当前最高合格层内查找，低层绑定不会绕过已恢复的高层（曾有的缺陷已于
  2026-09-06 修复，回归用例见 `review/2026-09-06/gateway_probes.rs`）。
- **层内四维评分**（倍率 / 可靠性 / 首字延迟 / 输出速度）：比值归一化、分组级
  可调权重、`score^k` 加权随机分配；性能用 EWMA 统计，样本不足时取中性分。
- **会话粘性**：Responses 状态链 → 显式会话头 → `prompt_cache_key` → 稳定前缀
  哈希（system prompt + 工具定义，不含用户消息，`/compact` 不断链）；按请求体
  体积 × 缓存新鲜度计算等待预算；带 `Retry-After` 的 429 在预算内原地等待。
- **熔断与限流**：60 秒窗口内连续或高比例故障进入指数冷却，冷却后只放行一个
  半开试运行请求；401/403 停整个账号，429 只停"账号 + 模型"；RPM / TPM / 最大
  并发按账号默认、目标覆盖。
- **故障切换边界**：廉价失败不计入任何次数上限；流式响应在第一个有语义的事件
  之前仍可切换，之后禁止拼接第二个上游；慢请求与总超时不重放。
- **自动倍率**：Sub2API 与 New API 探针、抖动刷新与退避、按风险余量的宽限期、
  探针系统性故障保护、手动倍率不受影响。
- **重启恢复**：粘性绑定与性能 EWMA 每 60 秒落盘，重启后恢复；超过 24 小时的
  快照丢弃。
- **跨协议转换**：Chat Completions ↔ Responses ↔ Messages 六个方向全部可用。
  请求先解析成统一的中间格式再发到目标协议；工具调用、思考块、图像输入、
  结构化输出与 usage（含缓存读写）在三个协议之间往返。入口与端点协议相同
  时直接原生透传，不付解析与重排的代价。
- **能力降级白名单**：白名单外的能力（工具、结构化输出、图像/文件、角色语义）
  在目标端点无法表达时明确返回 400，绝不静默丢失；白名单内只有思考与协议
  特有的采样参数（`top_k` 等）可以丢，且仅发生在故障切换里——同一层内先试
  无损候选，全部失败才轮到降级候选。发生降级的请求带 `X-Akhub-Degraded`
  响应头，请求记录标红，可按"仅降级"筛选。
- **端点证据**：某账号的上游没有某端点（404/405）时记下来，24 小时内不再
  把该端点当作候选；配置变更立即清空。概览页会提示这类"端点缺失"证据。
- **未知字段透传**：本网关不认识的顶层字段原样带给原生协议的上游，跨协议
  时才明确拒绝。
- 嵌入二进制的 React + TypeScript 管理后台（`/admin`）：账号内的模型管理
  （获取/手动添加/逐行启用停用/删除，别名归并，隐藏原始模型）、只读调度视图
  （按分组与模型展示综合评分、可用性、倍率、首字延迟、速度与预计分配）、
  倍率告警、调度权重编辑、降级筛选与端点转换标记、分组的"允许降级"开关与
  端点缺失告警。账号人工优先级默认 0，同层由评分决定分配。
- Docker 与 Linux 部署示例。

阶段 4–5 已有 Responses 状态链、模型发现与选择集、别名与自动归并、成本视图、
校准助手、账号复制/测试、备份恢复和管理员审计的实现及基础测试，但不能据此视为完整验收。

2026-09-06 的功能审查曾复现四项问题：账号总并发未跨模型共享、低优先级粘性绑定绕过已恢复的高层、
TPM 结算漏算输入 Token、Responses 故障切换丢失工具返回；这些问题已在当前工作区修复并由定向复现回归验证。
随后的加固轮次又补上了流式结算、Responses 生命周期、内存上限、SSRF 解析与关闭宽限期，
详见 `2026-09-06_功能审查-Akhub-report.md` 与本节的"已补齐"清单。

**仍待完成**（发布硬化与真实环境验收）：

| 能力 | 计划阶段 |
|---|---|
| §26.9 完整性能验收（固定硬件、64 KB/8 MB/64 MB 请求体、连接复用率与每请求分配量） | 阶段 6 |
| Docker 容器内"空数据目录首次启动 + 真实调用"的**本地**实跑记录（CI 已用 `docker` 作业在每次推送时覆盖这条，见 `.github/workflows/ci.yml`） | 阶段 6 |
| 真实上游上的跨账号故障切换实测（现有测试 Key 下同一逻辑模型只有一个可用目标） | 阶段 6 |

已完成（本轮校正）：

- **发布流程与部署方式**（2026-09-20 重做）：
  - **六个平台的二进制**：`linux-x86_64`、`linux-aarch64`、`linux-x86_64-musl`、
    `macos-aarch64`、`macos-x86_64`、`windows-x86_64`。全部在 **各自平台的原生 runner**
    上构建（`ubuntu-24.04-arm` 跑 arm64），不走 QEMU，也不用 `actions-rust-cross`；
    Release 附带 `checksums.txt`。
  - **多架构镜像**：每个架构在自己或原生 runner 上构建、先推 `<tag>-<arch>`，
    再由 `docker-manifest` 用 `buildx imagetools create` 合成 manifest——
    两个架构推同一 tag 会互相覆盖，这一步不能省。
  - **一键安装**：`install.sh`（Linux/macOS，支持 `--service` 直接装 systemd 单元）
    与 `install.ps1`（Windows）。两者都校验 sha256，升级前自动备份旧二进制，
    支持 `AKHUB_BASE_URL` 指向自建镜像。
  - **打包规则单一来源**：`scripts/package.sh` 同时被 CI 与 `scripts/release.sh` 调用，
    资产命名与归档布局不会两边分叉。
  - **容器**：`Dockerfile` 改用 `tini` 转发信号 + entrypoint 纠正数据目录属主后
    `gosu` 降权，`cmake` 补齐 `aws-lc-sys` 的构建依赖；`docker-compose.yml` 支持
    `AKHUB_IMAGE` 拉预构建镜像，并补了日志轮转与 `no-new-privileges`。
  - **可运维性**：新增 `/health/version` 与 `akhub --version` / `--help`；
    CI 增加容器冒烟作业，在**空数据目录**上跑通首次启动、真实 HTTP 调用与
    非 root 校验（§28 完成标准里的那一条）。
  - **部署文档**：`deploy/README.md` 覆盖 Docker / 一键脚本 / systemd / 升级回滚 /
    反代，并新增 `deploy/Caddyfile`、`deploy/Caddyfile.windows`、`deploy/nginx.conf`
    与 `deploy/README.windows.md`。
- **关闭信号取消排队请求**（§25.3 第 2 步）：`src/app/mod.rs:239 begin_shutdown`、
  `src/routing/queue.rs:130-152`、`src/gateway/passthrough.rs:480-484` 返回可重试的
  `queue_timeout`，并有回归测试 `src/routing/queue.rs:262,291`。

已补齐（本轮）：

- 流式结算：按流内真实 usage 回补 TPM；拿不到 usage 时保持保守预留；
  性能 EWMA 改用真实流结束时间，流内错误不再伪装成成功记录。
- 流式 Responses：结束时用 `response.completed` 的最终对象补写完整输出项；
  流失败或客户端中断时删除状态链，后续引用得到明确的 `response_state_expired`。
- 跨协议进入 Responses 同样使用网关 ID 并登记状态链。
- 查询/取消/删除/输入项：有原生映射时转发给原账号；没有时查询回放真实响应
  对象，取消则明确拒绝，绝不伪报成功。
- `POST /v1/responses/compact` 与 `/v1/responses/input_tokens` 接入完整调度链路
  后原生转发给 Responses 上游；上游没有该路由时返回明确的不支持（不重试、不估算）。
- 图片接口 `POST /v1/images/generations` 与 `POST /v1/images/edits`：仅原生转发到
  OpenAI 兼容上游，不做跨协议转换与 provider 适配；edits 保留 multipart 文件字节。
- 官方 SDK 黑盒验收（§26.2）：已用官方 `openai` / `anthropic` SDK 对真实上游
  跑通 14/14（Chat/Responses/Messages 的流式与非流式、工具往返、查询与续链、
  错误对象、401），并完成真实生图与图生图；脚本在 `tests/sdk/acceptance.py`。
- 真实上游接入与探针现场字段：Sub2API 计费探针已兼容现场 `sub2api.key_billing`
  形状（自动识别 effective 倍率），newapi 走 OpenAI 兼容 + Anthropic 端点；
  过程记录见 `review/2026-09-15-真实上游验收.md`。
- 请求体超过 8 MiB 落数据目录临时文件（自动清理 + 启动清理），非流式上游
  响应体施加 64 MiB 硬上限。
- SSRF 校验移入 DNS 解析器：连接使用的地址就是校验过的地址，消除 DNS
  Rebinding 的二次解析窗口。
- 锁毒化恢复、内置与容器优雅关闭宽限期、数据库文件权限 0600、安全响应头、
  NOTICES 版本与 Cargo.lock 对齐（并有测试钉住）。

常规自动化测试在本地已通过，但不能以测试数量替代完整验收：上面的剩余能力缺口
与真实环境验收仍需在具备 Docker 和测试账号的机器上执行。

## 快速开始

### Docker

```bash
docker compose up -d
# 打开 http://127.0.0.1:8080/admin 设置管理员密码
```

### 本地运行

```bash
cargo run --release
# 默认监听 127.0.0.1:8080，数据目录 ./data
```

### 配置一条可用链路

在 `/admin` 中依次完成三步：

1. **建分组**，记下只显示一次的下游 Key（形如 `akh-...`）。
2. **建账号**：填 Base URL、上游 API Key 与首选协议。不需要填写上下文长度、
   多模态、工具或思考等模型能力字段。
3. **配置模型**：打开账号的「模型管理」，点「获取上游模型」并启用要用的模型。
   每行的**对外名**就是下游看到的模型名；留空则跟随上游真名。把不同上游站的
   同一模型设成相同对外名即可自动归并，调度视图里能按模型查看账号评分、
   可用性与速度。

设置别名后，默认会同时暴露别名与原始上游名；勾选「隐藏原始模型」后下游只会
看到别名。模型启用后会自动生成调度目标，不再需要单独创建逻辑模型或手工绑定
目标。

然后就可以直接用了：

```bash
# Claude Code / Anthropic SDK
export ANTHROPIC_BASE_URL=http://127.0.0.1:8080
export ANTHROPIC_AUTH_TOKEN=akh-你的分组Key

# OpenAI 客户端
curl http://127.0.0.1:8080/v1/chat/completions \
  -H "Authorization: Bearer akh-你的分组Key" \
  -H "content-type: application/json" \
  -d '{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}]}'
```

## 环境变量

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

## 几个必须知道的行为

- **主密钥丢失后，数据库中的上游 Key 无法恢复。** 备份数据目录时务必包含
  `master.key`，或改用 `AKHUB_MASTER_KEY` 自行托管。
- **下游 Key 只在创建与重新生成时完整显示一次**，之后只保留 HMAC 摘要与前缀。
- **默认阻止访问环回、内网与云元数据地址。** 上游确实在内网时，需要为该账号
  显式开启"允许内网访问"。
- **Base URL 以 `/v1` 结尾时会被识别为已含版本段**，不会拼出 `/v1/v1/messages`。
- **普通请求的正文不落日志**，请求记录只保存元数据。
- **不做下游计费、多租户与跨分组调度。** 分组是调度的硬边界。
- **"忙"不是"坏"。** 高优先级层全部满载时，请求在该层排队而不是降到下一层；
  想让两个账号自动分担流量，把它们设成同一个优先级。
- **自动倍率刷新失败不会立刻停用账号。** 最后已知值保留并进入宽限期：有效倍率
  不超过分组上限的 60% 时宽限 60 分钟，60%–90% 时 15 分钟，超过 90% 立即硬停。
  New API 探针需要在个人设置页生成的访问令牌与用户 ID，推理用的 `sk-xxx` 不被
  分组接口接受。
- **Sub2API 计费接口的字段名取自方案 §11.2**（`object` / `version` / `scope` /
  `effective_multiplier` / `observed_at` / 峰值字段）。接入真实站点前请用它的
  实际响应核对一次；校验不过时探针只会报错，不会退回默认倍率。

## 开发

管理后台是独立的 Vite 应用，构建产物在编译期嵌入 Rust 二进制。

```bash
# 1. 构建前端（改动 web/ 后需要重跑）
cd web && npm install && npm run build && cd ..

# 2. 构建与测试后端
cargo test          # 单元测试 + 端到端测试
cargo clippy --all-targets
cargo fmt
```

跳过第 1 步也能 `cargo build`：`build.rs` 会放一个占位页面，`/v1` 网关接口
不受影响，只是 `/admin` 会提示你先构建前端。

前端热更新开发时，先在一个终端跑 `cargo run`，另一个终端跑 `cd web && npm run dev`，
Vite 会把 `/admin/api` 与 `/v1` 代理到 `127.0.0.1:8080`。

端到端测试会拉起一个假上游站点和一台完整的 Akhub，覆盖鉴权、模型改写、
故障切换、双形状模型列表、后台 CRUD 与静态资源服务。

## 安装与部署

详细步骤见 [`deploy/README.md`](deploy/README.md)，下面是最短的路径。

**Docker（推荐）**

```bash
docker compose up -d
# 指定版本
AKHUB_IMAGE=ghcr.io/hsmirage/akhub:1.1.0 docker compose up -d
```

镜像在 GHCR 上，多架构（amd64 / arm64）、非 root 运行、只暴露 `/data` 一个持久卷。
**容器侧必须给足停止宽限**（compose 的 `stop_grace_period` 或 `docker run` 的
`--stop-timeout` ≥ 200 秒），否则 Docker 默认的 10 秒会强杀在途的长流式请求。

**一键脚本（Linux / macOS）**

```bash
curl -fsSL https://raw.githubusercontent.com/HsMirage/Akhub/master/install.sh | sh
```

自动探测平台 → 下载对应资产 → 用 `checksums.txt` 校验 sha256 → 安装到
`/usr/local/bin`。`--service` 会顺带装好 systemd 单元。Windows 用
[`install.ps1`](install.ps1)，细节见 [`deploy/README.windows.md`](deploy/README.windows.md)。

**验证部署**

| 端点 | 含义 |
|---|---|
| `/health/live` | 进程事件循环正常 |
| `/health/ready` | 数据库、配置与主密钥已就绪 |
| `/health/version` | 当前版本号，用于确认升级是否生效 |

`akhub --healthcheck`、`akhub --version`、`akhub --help` 三个子命令不依赖配置文件，
可以直接在服务器上敲。

## 发布

推 `v*` tag 触发 [`.github/workflows/release.yml`](.github/workflows/release.yml)，
在**各平台原生 runner** 上构建 6 个平台的单文件二进制：

| 资产 | 说明 |
|---|---|
| `akhub-v1.1.0-linux-x86_64-musl.tar.gz` | 静态链接，跨发行版直接用（推荐） |
| `akhub-v1.1.0-linux-x86_64.tar.gz` | glibc 版 |
| `akhub-v1.1.0-linux-aarch64.tar.gz` | ARM64 服务器 |
| `akhub-v1.1.0-macos-aarch64.tar.gz` / `-macos-x86_64` | Apple Silicon / Intel |
| `akhub-v1.1.0-windows-x86_64.zip` | Windows 原生 |
| `checksums.txt` | 上面所有资产的 sha256 |

同一轮还会在原生 arm64 runner 上构建 `linux/amd64` 与 `linux/arm64` 镜像，
合成多架构 manifest 后推送到 GHCR。

本地发版用 `scripts/release.sh`（前端构建 → fmt/clippy/全量测试 → 交叉编译 →
打包 → 生成校验和），打包规则由 [`scripts/package.sh`](scripts/package.sh)
统一定义，与 CI 共用，所以资产命名与目录布局不会两边分叉。

- **升级检查**：用旧版本二进制打开新版本写出的数据库会被拒绝并提示升级，
  不会写坏数据。
- **许可证与第三方声明**：见 [`LICENSE`](LICENSE) 与 [`NOTICES.md`](NOTICES.md)。

## 许可与来源

本项目以 **Akhub 非商业署名许可**发布，不是 OSI 定义的"开源软件"：非商业使用
必须保留署名，商业使用需事先取得书面授权。完整条款见 [`LICENSE`](LICENSE)，
第三方依赖清单见 [`NOTICES.md`](NOTICES.md)。

协议适配部分为干净实现，行为参考了 New API（AGPL-3.0）、Sub2API（LGPL-3.0）、
AxonHub（Apache-2.0 / LGPL-3.0）与 LiteLLM（MIT），未直接复制其实现代码。
