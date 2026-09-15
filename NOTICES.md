# 第三方声明（Third-party notices）

本项目自身源码不采用 MIT 许可，而采用根目录 `LICENSE` 中的 Akhub 非商业署名许可。
第三方依赖继续遵循各自的原始许可证；本文件不改变任何第三方依赖的授权范围。

Akhub 直接依赖的第三方库及其许可证。运行 `cargo generate-lockfile` 后，
每个包的完整许可证文本位于 `~/.cargo/registry/src/*/包名-版本/LICENSE*`。

| 依赖 | 版本 | 许可证 |
|---|---|---|
| anyhow | 1.0.104 | MIT OR Apache-2.0 |
| arc-swap | 1.9.2 | MIT OR Apache-2.0 |
| argon2 | 0.6.0 | MIT OR Apache-2.0 |
| async-stream | 0.3.6 | MIT |
| axum | 0.8.9 | MIT |
| base64 | 0.23.1 | MIT OR Apache-2.0 |
| chacha20poly1305 | 0.11.0 | MIT OR Apache-2.0 |
| futures | 0.3.34 | MIT OR Apache-2.0 |
| getrandom | 0.4.3 | MIT OR Apache-2.0 |
| hex | 0.4.3 | MIT OR Apache-2.0 |
| hmac | 0.13.0 | MIT OR Apache-2.0 |
| rand | 0.10.2 | MIT OR Apache-2.0 |
| reqwest | 0.13.4 | MIT OR Apache-2.0 |
| rust-embed | 8.12.0 | MIT |
| secrecy | 0.10.3 | MIT OR Apache-2.0 |
| serde / serde_json | 1.0.229 / 1.0.151 | MIT OR Apache-2.0 |
| sha2 | 0.11.0 | MIT OR Apache-2.0 |
| sqlx | 0.9.0 | MIT OR Apache-2.0 |
| tempfile | 3.27.0 | MIT OR Apache-2.0 |
| thiserror | 2.0.20 | MIT OR Apache-2.0 |
| time | 0.3.55 | MIT OR Apache-2.0 |
| tokio | 1.53.1 | MIT |
| tower / tower-http | 0.5.3 / 0.7.1 | MIT |
| tracing / tracing-subscriber | 0.1.44 / 0.3.23 | MIT |
| ulid | 3.0.0 | MIT |
| zeroize | 1.9.0 | MIT OR Apache-2.0 |

前端构建（`web/`）依赖 React、Vite 及其传递依赖，各自遵循 node_modules 中
登记的许可证（以 MIT / Apache-2.0 为主）。

## 内置能力目录的来源

`assets/capabilities.json` 由 LiteLLM 的公开模型目录
（<https://github.com/BerriAI/litellm>，MIT License）在发布时转换为 Akhub 的
精简快照生成，只保留能力标记与上下文上限，不含价格数据，并注明了源版本。

## 行为参考

协议适配部分为干净实现。行为参考了以下项目（未直接复制其实现代码）：

- New API（AGPL-3.0）
- Sub2API（LGPL-3.0）
- AxonHub（Apache-2.0 / LGPL-3.0）
- LiteLLM（MIT）
