# mirageai.vip 的 524 与「已重试模型请求」：根因与处置

日期：2026-11-19　范围：源站 nginx（已改并生效）、akhub（已改，二进制已放到生产的待部署目录）、sub2api（只诊断）

## 一、ccmax / opus-5-5 报 524：nginx 的 HTTP/2 上传窗口太小

**结论：不是 akhub，也不是上游，是源站 nginx 收请求体太慢。**

- 524 那一条（16:33:36Z 开始）在 sub2api 里**从来没到 `gateway_check_start`**：
  125.6 秒都花在读请求体上，akhub 一侧根本没有这条记录。
- 同一客户端（43.128.225.152，Claude Code，请求体约 10.6 MB）在修复前的 178 条大请求里，
  请求体上传中位速度 **249 KB/s**，单条上传 37–75 秒，3 条超过 120 秒 → CF 524。
  akhub 自己的首字只有 20–40 秒。
- 原因：`proxy_request_buffering off` 时，nginx 给每个 h2 流的接收窗口只有
  `max(client_body_buffer_size=16k, http2_body_preread_size=64k)` = 64 KB。
  CF 亚洲边缘回源到 Vienna 的 RTT 实测 **241–261 ms**（`ss -ti`），
  64 KB / 0.25 s ≈ 256 KB/s，和实测值对得上。

**处置（已生效，2026-11-18 17:06Z）**：`/etc/nginx/nginx.conf` 的 http 块加上
`client_body_buffer_size 8m;`（备份 `nginx.conf.bak-20261119-h2window`），然后 `nginx -t` + reload。
对 mirageai.vip、ai.hsnb.fun 这些反代站都生效。内存按 min(请求体, 8m) 逐请求分配。

**验证**：同一客户端改后的 23 条 >1 MB 请求，上传中位速度 **4354 KB/s**（快了约 17 倍），
1.8 MB 只要 0.4 秒。换算下来，10.6 MB 约 2.5 秒，离 120 秒很远。

回滚：`cp /etc/nginx/nginx.conf.bak-20261119-h2window /etc/nginx/nginx.conf && nginx -t && systemctl reload nginx`

## 二、DeepSeek 官 Key「已重试模型请求」

### 2.1 实际触发重试的是 sub2api 回给 DSH 的 502

本机 DSH 会话日志里共有 412 次 `llm/retry`，其中 **406 次是 mirageai 的 CF 502**，
另外有 2 次 520、2 次 524、1 次 TRANSPORT、1 次 TIMEOUT。
这 406 次全部落在 13:19Z–15:42Z 之间。

sub2api `ops_error_logs` 里同期有 404 条 502，内容都是
`Post http://89.58.17.138:8088/...: write: broken pipe`，`upstream_latency_ms=1`：
sub2api 往 akhub 写请求体的那一刻连接就已经断了，akhub 没有收到、也没有记录这些请求。
group 39 只有 1 个账号，所以 sub2api 换不了号，只能直接回 502。

**现状**：15:42Z 之后一条都没再出现（00:00 之后的 127 条全部成功），
我在 sub2api 上压测 16 条 200 KB 的 /responses 也全部 200。
这个窗口里 sub2api 重启了 5 次（13:24、15:16、15:18、15:40、15:54Z），akhub 在 15:46Z 也重启过一次，
所以分不出是哪一次重启让它消失的，**根因没有定位到**。akhub 不会在 100 秒内主动关闭空闲连接（已实测），
也就是说那条 RST 不是 akhub 的空闲超时引起的。

**建议**：如果它再出现，先看 `ops_error_logs` 里的 `broken pipe`。
给 group 39 再挂一个指向 akhub 的账号，可以让 sub2api 遇到这种传输错误时有号可换。

New API 侧最近 24 小时 deepseek 模型的 `type=5` 错误日志是 0 条，
ai.hsnb.fun 的 503 都是 1 ms 级的「无可用渠道」，和 DeepSeek 无关。
第一节的上传窗口修复对 New API 同样生效。

### 2.2 akhub：DeepSeek 流 93% 被误记成 client_gone（已修，待部署）

最近 24 小时 DeepSeek 官 key 分组的流式 Chat 里，1959 / 2100 条记成 `client_gone`，
其中 2031 条带完整 usage。实测一条完全正常的 sub2api 请求，akhub 也照样记成 `client_gone`：
下游（sub2api / New API）读到终止事件就关闭连接，不等 akhub 收尾。

此前的处理是把这类请求降成 `TimingOnly` 样本：不计入成功率，记录页标红。
**本次改为：终止信号已经发出、流里也没有错误，就按 `Completed` 结算**
（`src/gateway/settle.rs`）。这样记录页不再大片标红，评分也能拿到真实的成功样本。
真正中途断流的（没有终止信号）仍然记 `client_gone`，不计入证据。

- 测试：编译机 `cargo fmt --check` 通过，`cargo test --release --locked` **913 通过 / 0 失败**
  （`tests/stream_settlement.rs` 的对应断言已改为期望 `error_code = None`）
- 产物 sha256：`bc778faedf2d98bcdaf526e91639c6ab52462684b41ac322808293cb5bcd4563`
- 已放到生产 `/opt/akhub/staged/akhub-settle-bc778fae`，**还没替换、没重启**，
  免得打断正在跑的流。部署按上次的流程：一致性备份（含 `master.key`）→ 停容器 →
  拷到 `/var/lib/akhub/runtime/akhub` → 起容器。
