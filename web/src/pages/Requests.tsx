/** 请求记录：只有元数据，没有正文。 */
import { useMemo, useState } from "react";
import type { Data } from "../lib/store";
import { ENDPOINT_LABELS, PROTOCOL_LABELS } from "../lib/types";
import {
  formatBytes,
  formatDuration,
  formatTime,
  statusTone,
} from "../lib/format";
import { Badge, Button, Card, EmptyState } from "../components/ui";
import { IconInbox } from "../components/Icons";

type Filter = "all" | "failed" | "streaming" | "degraded";

/** 请求入口协议与实际上游端点是否同一个协议。 */
function sameProtocol(protocol: string, endpoint: string): boolean {
  const of: Record<string, string> = {
    chat_completions: "openai_chat",
    responses: "openai_responses",
    messages: "anthropic_messages",
    count_tokens: "anthropic_messages",
  };
  return of[endpoint] === protocol;
}

export function Requests({ data, refresh }: { data: Data; refresh: () => Promise<void> }) {
  const [filter, setFilter] = useState<Filter>("all");

  const records = useMemo(() => {
    switch (filter) {
      case "failed":
        return data.requests.filter((record) => record.http_status >= 400);
      case "streaming":
        return data.requests.filter((record) => record.streaming);
      case "degraded":
        return data.requests.filter((record) => record.degraded !== null);
      default:
        return data.requests;
    }
  }, [data.requests, filter]);

  const accountName = (id: string | null) =>
    id ? (data.accounts.find((account) => account.id === id)?.name ?? id) : "—";

  return (
    <Card
      title="请求记录"
      description="默认保留 30 天。不保存消息正文、图片、工具参数或思考内容。"
      actions={
        <>
          <div className="row" style={{ gap: 2 }}>
            {(
              [
                ["all", "全部"],
                ["failed", "仅失败"],
                ["streaming", "仅流式"],
                ["degraded", "仅降级"],
              ] as const
            ).map(([value, label]) => (
              <Button
                key={value}
                size="sm"
                variant={filter === value ? "secondary" : "ghost"}
                onClick={() => setFilter(value)}
              >
                {label}
              </Button>
            ))}
          </div>
          <Button size="sm" onClick={() => void refresh()}>
            刷新
          </Button>
        </>
      }
    >
      {records.length === 0 ? (
        <EmptyState
          icon={<IconInbox size={19} />}
          title={filter === "all" ? "还没有请求记录" : "没有符合条件的记录"}
          description={
            filter === "all"
              ? "用分组 Key 向 /v1/messages 或 /v1/chat/completions 发一次请求就会出现。元数据由后台任务批量落盘，热路径不等待写入。"
              : filter === "degraded"
                ? "没有请求发生过能力降级。工具、图片与结构化输出永远不会被丢弃；只有 thinking 与协议独有采样参数会，而且只在故障切换时。"
                : "换一个筛选条件试试。"
          }
        />
      ) : (
        <div className="table-wrap">
          <table className="data">
            <thead>
              <tr>
                <th>时间 / 请求 ID</th>
                <th>逻辑模型</th>
                <th>实际目标</th>
                <th>体积</th>
                <th>耗时</th>
                <th>调度</th>
                <th>倍率</th>
                <th>状态</th>
              </tr>
            </thead>
            <tbody>
              {records.map((record) => (
                <tr
                  key={record.request_id}
                  className={record.degraded !== null ? "row-degraded" : undefined}
                >
                  <td className="cell-dim" style={{ whiteSpace: "nowrap" }}>
                    {formatTime(record.started_at)}
                    <div className="mono text-faint" style={{ fontSize: 11, marginTop: 2 }}>
                      {record.request_id}
                    </div>
                  </td>
                  <td>
                    <div className="mono">{record.logical_model ?? "—"}</div>
                    <div
                      className="text-faint row"
                      style={{ fontSize: 11, marginTop: 2, gap: 6 }}
                    >
                      <span>
                        {PROTOCOL_LABELS[record.protocol]}
                        {record.streaming && " · 流式"}
                        {/* 端点与入口协议不一致，说明这一次走了跨协议转换。 */}
                        {record.endpoint && !sameProtocol(record.protocol, record.endpoint) && (
                          <> → {ENDPOINT_LABELS[record.endpoint] ?? record.endpoint}</>
                        )}
                      </span>
                      {/* 降级是转换的产物，所以和协议放在同一行，而不是调度列。 */}
                      {record.degraded && (
                        <span title={`为完成这次请求丢弃了：${record.degraded}`}>
                          <Badge tone="danger">降级 {record.degraded}</Badge>
                        </span>
                      )}
                    </div>
                  </td>
                  <td>
                    <span className="chain">
                      <span className="cell-dim">{accountName(record.account_id)}</span>
                      {record.upstream_model && (
                        <>
                          <span className="chain-arrow">/</span>
                          <span className="mono cell-dim">{record.upstream_model}</span>
                        </>
                      )}
                    </span>
                  </td>
                  <td className="cell-dim">{formatBytes(record.request_bytes)}</td>
                  <td className="cell-dim">{formatDuration(record.duration_ms)}</td>
                  <td className="cell-dim" style={{ fontSize: 12 }}>
                    <span className="row" style={{ gap: 6 }}>
                      {record.sticky_hit && <Badge tone="accent">粘性</Badge>}
                      {record.attempts > 1 && (
                        <Badge tone="warn">切换 {record.attempts - 1} 次</Badge>
                      )}
                      {record.queued_ms > 0 && (
                        <span title="排队等待">等 {formatDuration(record.queued_ms)}</span>
                      )}
                      {!record.sticky_hit && record.attempts <= 1 && record.queued_ms === 0 && "直达"}
                    </span>
                  </td>
                  <td className="mono cell-dim" style={{ fontSize: 12 }}>
                    {record.effective_multiplier ?? "—"}
                    {record.cheapest_multiplier &&
                      record.dearest_multiplier &&
                      record.cheapest_multiplier !== record.dearest_multiplier && (
                        <div className="text-faint" style={{ fontSize: 11 }}>
                          区间 {record.cheapest_multiplier}–{record.dearest_multiplier}
                        </div>
                      )}
                  </td>
                  <td>
                    <Badge tone={statusTone(record.http_status)}>
                      {record.http_status}
                    </Badge>
                    {record.error_code && (
                      <div className="mono text-faint" style={{ fontSize: 11, marginTop: 2 }}>
                        {record.error_code}
                      </div>
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Card>
  );
}
