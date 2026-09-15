# 官方 SDK 黑盒验收（§26.2）

用官方 `openai` 与 `anthropic` Python SDK 作为黑盒客户端，对一个 Akhub 入口
（或某个上游站点做基线自检）跑协议矩阵。密钥只从参数或环境变量传入，不落盘。

## 依赖

```bash
python3 -m pip install --user openai anthropic
```

## 用法

```bash
AKHUB_TEST_KEY=akh-... python3 tests/sdk/acceptance.py \
  --base-url https://akhub-89-58-17-138.nip.io \
  --key "$AKHUB_TEST_KEY" \
  --chat-model glm-5.3-flash \
  --responses-model glm-5.3-flash \
  --messages-model glm-5.3-flash \
  --count-tokens --image-model gpt-image-1
```

- `--base-url` 可以是根地址或带 `/v1`：脚本会按 SDK 约定自动归一化
  （OpenAI SDK 需要 `/v1`，Anthropic SDK 需要根地址）。
- 只想跑某一面：`--suite openai|anthropic|images`。
- 上游本就没有的能力（例如 `GET /v1/models/{model}`、`count_tokens`）记 SKIP；
  加 `--strict` 可把 SKIP 也算失败，用于 Akhub 这种"应该全都有"的验收对象。

## 覆盖

| 面 | 用例 |
|---|---|
| OpenAI | 模型列表、Chat 非流式/流式、工具调用往返、Responses 非流式/流式/查询/续链、错误对象与状态码、无效 Key → 401、单模型查询 |
| Anthropic | Messages 非流式/流式、工具调用往返、`count_tokens` |
| Images | `images.generate`（有 b64_json 或 url 即通过） |

脚本输出逐项 ✅/❌/⏭️ 与耗时，任一项失败时退出码为 1，可直接接进 CI 或发布检查。
