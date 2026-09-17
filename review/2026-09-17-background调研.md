# `background:true` 生命周期：三个参考项目的做法与 Akhub 的取舍

调研时间：2026-09-17。结论用于计划 §15.3（后台响应）的实现口径。

## 结论速览

| 项目 | 对 `background:true` 的处理 | 证据 |
|---|---|---|
| LiteLLM | 自己实现一套"轮询 + 缓存"后台模式：`background=true` 且开启 `polling_via_cache` 时立刻返回 `polling_id`，后台 asyncio 任务持续消费上游流并写 Redis，客户端轮询缓存拿部分结果；也支持把某些模型标记为 `native_background_mode` 走上游原生后台 | `litellm/proxy/response_polling/{background_streaming,polling_handler}.py` |
| New API | **不实现**：`OpenAIResponsesRequest` 里 `Background` 字段被注释掉，注释写明"在后台运行推理，暂时还不支持依赖的接口" | `dto/openai_request.go:817-819`；`router/relay-router.go` 只有 `POST /responses` 与 `/responses/compact`，没有查询/取消路由 |
| Sub2API | **不支持**：实测 `POST /v1/responses` 带 `background:true` 返回 `400 Unsupported parameter: background`；`GET /v1/responses/{id}` 返回 404 | 2026-09-17 对 `http://127.0.0.1:8080` 的真实调用 |

## LiteLLM 的机制细节

`should_use_polling_for_request()` 判定是否启用缓存轮询，三个条件必须同时满足：

1. 请求带 `background: true`；
2. 配置开了 `polling_via_cache`（值为 `false` / `"all"` / 供应商列表）；
3. 有可用的 `redis_cache`。

此外若模型在 `native_background_mode` 列表里，则**跳过**缓存轮询，改走上游原生后台能力。

启用后：

- `generate_polling_id()` 生成 ID（`is_polling_id()` 用于识别），立即返回给客户端；
- `background_streaming_task` 在后台把上游流式事件组装成 OpenAI 风格的 Response 对象，逐步 `update_state()` 写 Redis（默认 TTL 3600 秒）；
- 客户端用这个 ID 轮询，拿到 `in_progress` / 完成态与已完成的部分输出。

也就是说：LiteLLM 的"后台"是**网关自己托管**的（进程内任务 + Redis 存状态），依赖一个外部缓存，且官方承认这是轮询而非推送。

## Akhub 为什么选择"只代理原生，不自己托管"

1. **单实例 + 无 Redis 是第一期硬约束**（计划 §2.2 明确不做多实例集群与 Redis 依赖）。LiteLLM 那套轮询缓存要求外部 Redis；改成进程内存表则进程重启即丢任务状态，出现"客户端拿到 ID、重启后查不到"的假后台，比不支持更糟。
2. **任务生命周期长于连接**。后台任务的语义是"连接断了任务还在跑"，而 Akhub 的调度、限额、熔断与倍率都绑定在请求生命周期上。自己托管需要一个独立的任务队列、持久化、重启恢复与配额结算——这是第二期及以后的工作量，塞进第一期会拖垮核心链路。
3. **静默降级最危险**（计划 §14.8 的同一原则）。把同步请求伪装成后台任务，用户会以为它在继续跑而实际没有；明确返回"不支持"至少是可预期的。

所以 Akhub 的口径是：

- 上游原生支持 Responses 后台（例如真正的 OpenAI）时，**代理其创建/查询/取消**，并保持目标粘性（后台任务固定在原账号，不因查询失败迁移）；
- 上游不支持时（New API、Sub2API 都不支持，实测确认），**明确返回 `unsupported_parameter` 400**，绝不伪造后台任务。

## 与参考项目的差异（记录备查）

- 相比 LiteLLM：少了"网关托管后台 + 轮询缓存"这一档能力；好处是不引入 Redis 依赖、不产生假任务；代价是只有原生支持的上游才有后台语义。
- 相比 New API / Sub2API：行为一致（它们也不提供后台），区别是 Akhub 会**明确报错并说明原因**，而不是让参数被忽略或返回一个无法查询的 ID。
- 未来要做托管后台时的最小落点：任务表（持久化）+ 独立调度循环 + 状态查询接口 + 重启恢复；届时可以照 LiteLLM 的形状，但把 Redis 换成 SQLite。
