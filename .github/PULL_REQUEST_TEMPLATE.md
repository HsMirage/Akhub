## 背景与改动说明

<!-- 简述问题现场、根因或需求背景，以及核心改法。若本次 PR 触发正式发版，此正文将自动作为 GitHub Release 说明。 -->

-

## 影响范围与取舍

<!-- 如有兼容性变更、配置项调整或已知局限，请写明；无则可删除此节。 -->

-

## 验证

- [ ] 已跑 `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test`（或等待 CI `check` 通过）
- [ ] 前端有改动时已通过 `web` 构建与 `npm test`
- [ ] CI `docker` 冒烟通过

## 发版确认

<!-- 触发正式发版只需在本 PR 中同步升级根 `Cargo.toml` 和 `desktop/Cargo.toml` 的 `version`，合并到 `master` 后将自动创建 `v<version>` tag 并执行 Release（Linux 二进制 + Windows 桌面端 + Docker 镜像 + GitHub Release）。 -->

- [ ] 仅合入代码，不发版（未修改版本号）
- [ ] 本次合入后自动发版：已同步更新 `Cargo.toml` 与 `desktop/Cargo.toml` 版本号
