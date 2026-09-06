# Akhub 网关与自动负载均衡检查

## 结论

**核心问题已修复，但仍不能认定为完整生产验收通过。** 三协议转发、优先级分层、评分、排队、倍率保护和基础状态链已有实现；本轮定向验收先复现了四个会影响流量分配、限额与工具调用的缺陷，随后全部修复并通过同一组回归用例。

本轮修复了一批局部问题并新增回归测试。常规测试目前为 300 个单元测试、99 个集成测试全部通过；四个缺陷复现用例也全部通过。仍不能把测试通过等同于完整生产验收：流式完整 usage 结算、Responses 后台生命周期、临时文件缓冲、真实上游和官方 SDK 仍待验证。此前 README 的“阶段 1–5 已完成”表述已收紧。

建议继续补齐 Responses 生命周期、流式结算与资源限制，最后进行官方客户端和真实上游验收。

## 尚未修复的主要问题

### F-001：账号总并发没有跨模型共享（已修复）

- 严重性：高，P1；类别：限额实现；状态：fixed；置信度：高。
- 证据：E-SRC、E-PROBE。
- 位置：`src/config/mod.rs:30`、`src/health/mod.rs:186`、`src/health/mod.rs:224`。
- 原因：账号默认限制被合并到每个调度目标，但并发信号量、RPM 和 TPM 窗口都按目标 ID 保存。账号本身只有鉴权与额度熔断状态。
- 复现：同一账号设置并发 1，建立两个模型；第一个请求挂起，再请求另一个模型。
- 修复：增加账号共享容量与账号共享 RPM/TPM 预算；目标覆盖值只约束单个目标，准入必须同时取得账号和目标容量。
- 回归：账号并发与四项定向复现中的账号并发用例现在通过。
- 影响：模型数增加时可能成倍突破账号总限制，触发上游限流或超载。
- 建议：区分账号总预算与目标局部预算，在实际准入时同时获取两者，统一处理等待、退还和结束释放。

### F-002：普通会话粘性可以绕过高优先级（已修复）

- 严重性：高，P1；类别：调度约束；状态：fixed；置信度：高。
- 证据：E-SRC、E-PROBE。
- 位置：`src/gateway/passthrough.rs:257`、`src/routing/mod.rs:124`。
- 原因：粘性绑定在整个候选计划中查找，随后先于分层循环执行，没有限制到当前最高合格层。
- 复现：优先级 100 的目标第一次返回 500，优先级 50 接管并建立普通前缀粘性；下一次请求时高层已可用。
- 修复：普通粘性只在当前最高合格层内查找；高层恢复后低层绑定会被清除并重新选择。
- 回归：高优先级恢复后的定向复现现在通过。
- 影响：界面中的人工优先级不再是硬规则，恢复后的主目标可能长期拿不到这些会话的流量。
- 建议：普通粘性只能在当前最高合格层内生效；Responses 原生资源绑定要与普通缓存亲和明确分开。

### F-003：TPM 结算只保留输出用量（已修复）

- 严重性：高，P1；类别：Token 限额；状态：fixed；置信度：高。
- 证据：E-SRC、E-PROBE。
- 位置：`src/gateway/passthrough.rs:547`、`src/gateway/stream.rs:231`、`src/health/mod.rs:393`。
- 原因：预留包含估算输入与最大输出，但成功结算传入的是 `success.output_tokens`；输出用量不能代表实际总用量。
- 复现：设置 TPM 1000，上游返回输入 900、输出 10，再立即发送同等请求。
- 修复：非流式响应从 usage 中提取 `total_tokens`，缺失时合并输入/输出字段；性能统计仍单独使用输出 Token。
- 回归：输入 900 + 输出 10 的 TPM 定向复现现在按预期返回限流。
- 影响：大量输入请求的预算被过度退还，后台显示的限额不能可靠约束上游消耗。
- 建议：输出速度继续使用输出 Token，TPM 则单独使用目标供应商定义的完整 usage；补齐缓存用量、估算不足和跨时间窗口结算测试。

### F-004：Responses 切换上游时丢失工具返回（已修复）

- 严重性：高，P1；类别：状态链保真；状态：fixed；置信度：高。
- 证据：E-SRC、E-PROBE。
- 位置：`src/gateway/responses.rs:446`、`src/gateway/responses.rs:490`。
- 原因：历史合并只提取最后一条用户或 message 输入，不保留完整增量数组，`function_call_output` 不符合筛选条件。
- 复现：第一轮产生函数调用；第二轮提交函数返回并引用前一响应，同时让原上游失败。
- 修复：Responses 续链合并当前请求的完整 `input` 数组，不再只筛选最后一条 user/message 项。
- 回归：工具返回跨故障切换定向复现现在通过。
- 影响：工具循环失败、模型重复调用工具，或在缺少工具结果时继续生成不正确答案。
- 建议：按协议项完整追加本轮输入，保存累计历史而不只是当前请求；覆盖并行工具结果、多条输入和三轮以上故障切换。

## 其他明确边界

以下为源码检查结果，未对每项完成新的动态验收，状态均为 candidate，置信度高，证据为 E-SRC。

| 编号 | 优先级 | 缺口及影响 | 位置 | 建议 |
|---|---|---|---|---|
| F-005 | P1 | 流式 Responses 只在首段保存输入骨架，没有保存后续完整输出；跨协议流式 Responses 分支也未完成同等网关状态登记。无法承诺流式后的无损跨上游续链。 | `src/gateway/passthrough.rs:1107`、`src/gateway/passthrough.rs:1151` | 在流式完成时保存标准输出项、工具项和累计历史，测试跨协议流式续链。 |
| F-006 | P1 | 查询响应是从请求体拼出的简化对象，取消只返回 cancelled，没有调用上游取消接口。`input_items`、`compact`、`input_tokens` 路由缺失。后台任务查询/取消不是真实代理。 | `src/gateway/responses.rs:276`、`src/gateway/responses.rs:325`、`src/gateway/mod.rs:24` | 补原生上游生命周期代理；不支持时明确拒绝，不能伪报取消成功。 |
| F-007 | P1 | 流式性能与成功记录仍在首段提交时生成；流内错误被包装为正常字节后，外层无法据此判为失败。吞吐、完整耗时与可靠性评分不准确。 | `src/gateway/passthrough.rs:550`、`src/gateway/passthrough.rs:936`、`src/gateway/translate.rs:173` | 以完成/未完成/错误/取消结果统一结束结算，解析最终 usage。 |
| F-008 | P2 | 64 MiB 请求仍全部在内存解析，未实现计划中的 8 MiB 以上临时文件；非流式上游成功/错误体整体读取没有流量上限。 | `src/gateway/passthrough.rs:1651`、`src/gateway/passthrough.rs:1275` | 增加可重放的大请求存储和逐块响应限额，做高并发内存验收。 |
| F-009 | P2 | 地址检查先解析一次 DNS，真正发送由共享 HTTP 客户端再次解析，两次结果未绑定。不能宣称完整防住 DNS 重绑定。 | `src/security/url_guard.rs:68`、`src/gateway/passthrough.rs:1008` | 在实际连接使用的解析器处检查地址，保证校验和连接使用同一结果。 |

## 本轮已修复

以下是本轮代码改动，不表示上述待办已解决。

| 修复 | 结果与覆盖 |
|---|---|
| 账号额度熔断的半开资格检查 | 检查不再提前占名额；真正准入占用并在后续失败、取消时退还。新增恢复回归测试。 |
| 发往上游前倍率终检失败 | 专门退回预留 RPM/TPM；区分倍率未知与倍率超限。新增完整退还测试。 |
| 最大并发动态调整竞态 | 串行化账号与目标信号量容量调整。新增多线程同时扩容测试。 |
| 流式请求过早释放并发名额 | 准入绑定到响应流，等流结束或取消后释放。新增可控挂起流测试。此处不等于 F-007 已解决。 |
| 忙的无损目标被降级目标越过 | 分阶段等待无损候选，忙不算故障。新增 HTTP 回归测试。此处不等于 F-002 已解决。 |
| Responses ID 分块改写 | 正确处理跨块、CRLF、多行 data、UTF-8 与末帧；不完整帧设置 8 MiB 上限，超限以流式错误终止。 |
| 跨组配置防御 | 配置快照丢弃跨组目标，备份恢复拒绝交叉引用。新增单元及后台 API 测试。 |
| Responses 过期清理遗漏 | 后台任务接入状态表清理，新增过期删除/新鲜保留测试。 |
| 请求时间、等待与模型目录边界 | 记录真实请求开始时间；Retry-After 余量不越过总期限；模型目录按块执行 8 MiB 上限。 |

## 验证范围

| 验证 | 结果 |
|---|---|
| `cargo test --all --no-fail-fast --quiet` | 300 单元测试与 99 集成测试通过 |
| `cargo fmt --check` | 通过 |
| `cargo clippy --all-targets -- -D warnings` | 通过 |
| `cargo build --release --locked` | 通过，本机 Release 编译 |
| `npm run build`，工作目录为 `web` | 通过 |
| 缺陷复现脚本 | 4 通过、0 失败；F-001 至 F-004 已修复 |
| 官方 SDK、真实上游、性能压测 | 本轮未执行 |
| 远端 Docker 与多架构发布 | 本轮未执行；此前 Docker 验证未完成 |

全部网络测试只访问本机模拟上游。没有测试真实账号或服务器环境；不能据此保证生产负载下的吞吐、费用或恢复行为。

## 复现方法

下面的脚本会创建临时源码副本，运行四项定向验收断言，不修改常规测试目录；当前版本应返回退出码 0。

```bash
cd /Users/mirage/AI/AiWork/Akhub
bash review/2026-09-06/run-probes.sh
```

复现代码归档在 `review/2026-09-06/gateway_probes.rs`。它们是独立的发布前验收用例，修复后仍保留用于防回归。

## 证据与调用路径

### E-SRC：本轮源码检查

- observed_at：2026-09-06。
- source_type：file。
- source_ref：各 F 项列出的具体代码位置。
- content_hash / artifact_path：n/a，工作区活跃源码，未创建全库不可变快照。
- repro_command：`nl -ba src/gateway/passthrough.rs`、`nl -ba src/health/mod.rs`、`nl -ba src/gateway/responses.rs`、`nl -ba src/routing/mod.rs`。
- raw_excerpt：修复前观察到粘性查找全部层、输出 Token 结算、只取最后 user 项和目标级独占预算；当前实现已改为最高层查找、完整 usage、完整 input 数组及账号/目标双层预算。
- linked_workitem：F-001 至 F-009。
- supersedes：none。

### E-PROBE：四个定向回归

- observed_at：2026-09-06。
- source_type：command / log。
- source_ref：`bash review/2026-09-06/run-probes.sh`，退出码 0。
- artifact_path：`review/2026-09-06/probe-output.txt`。
- content_hash：`685eb8b9e26cd75674c1f0079b707e547058dc3ec6ed9f5891bc1c4539c300a7`。
- 复现源文件 SHA-256：`c5fd061e7887138ec962ac759fd8e2384f394a2ab0a4eac09f92282059c395ba`。
- repro_command：见“复现方法”。
- raw_excerpt：账号共享容量、优先级恢复、完整 TPM usage、Responses 工具结果四项均通过；`test result: ok. 4 passed; 0 failed`。
- linked_workitem：F-001、F-002、F-003、F-004。
- supersedes：none。

### P-001：限额与路由调用路径

- path_type：callflow。
- start：已鉴权的模型请求。
- goal：按分组、优先级和共享预算选择目标，完整保留请求语义。
- 步骤 1：同时获取账号共享与目标局部容量；证据 E-SRC/E-PROBE，关联 F-001（fixed）。
- 步骤 2：只在当前最高合格层内查找普通粘性绑定；证据 E-SRC/E-PROBE，关联 F-002（fixed）。
- 步骤 3：用完整 usage 结算输入加输出预留；证据 E-SRC/E-PROBE，关联 F-003（fixed）。
- 步骤 4：发生 Responses 切换时合并当前请求完整输入数组；证据 E-SRC/E-PROBE，关联 F-004（fixed）。
- residual_risks：流式结束、完整 Responses 生命周期、真实上游格式和高并发资源边界尚未验收。

## 检查记录

本轮顺序为源码检查、局部缺陷修复、定向失败复现、完整回归和文档校准。未使用子代理；CodeGraph 返回未初始化，因此使用直接源码检查。报告按通用技术报告组织，flavor = null，没有套用不相关安全报告内容。

已完成文档核对：结论与测试证据一致；明确区分已修复与未修复；复现脚本实际运行；没有真实密钥或上游凭据；范围与限制已注明。缺陷修复前的失败日志仍保留在 Git 历史工作区外的本地运行记录中，当前仓库只保留修复后的回归结果。

参考来源：

- `计划.md` 第 9、10、12、13、15、17、26 节：当前项目约定，不代表这些能力已经实现。
- `review/2026-09-06/scope.md`：本轮范围。
- `review/2026-09-06/gateway_probes.rs`、`review/2026-09-06/probe-output.txt`：可复现源码和实际失败日志。
- `src/config/mod.rs`、`src/health/mod.rs`、`src/routing/mod.rs`、`src/gateway/`、`src/app/tasks.rs`、`src/security/url_guard.rs`：本轮检查的实现。
