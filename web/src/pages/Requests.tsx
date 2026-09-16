/** 请求记录：只有元数据，没有正文。 */
import { Fragment, useEffect, useMemo, useState } from "react";
import { api } from "../lib/api";
import type { Data } from "../lib/store";
import {
  ENDPOINT_LABELS,
  PROTOCOL_LABELS,
  type AttemptRecord,
  type RequestFilters,
  type RequestStatus,
} from "../lib/types";
import {
  formatBytes,
  formatDuration,
  formatTime,
  statusTone,
} from "../lib/format";
import { Badge, Button, Card, EmptyState, Field, useToast } from "../components/ui";
import { IconInbox } from "../components/Icons";

type TimeRange = "1h" | "24h" | "7d" | "all";

interface RequestFilterForm {
  timeRange: TimeRange;
  groupId: string;
  logicalModel: string;
  status: "" | RequestStatus;
  errorCode: string;
  requestId: string;
}

const PAGE_SIZE = 50;
const EMPTY_FILTERS: RequestFilterForm = {
  timeRange: "all",
  groupId: "",
  logicalModel: "",
  status: "",
  errorCode: "",
  requestId: "",
};
const RANGE_SECONDS: Record<Exclude<TimeRange, "all">, number> = {
  "1h": 60 * 60,
  "24h": 24 * 60 * 60,
  "7d": 7 * 24 * 60 * 60,
};

const REQUEST_COLUMN_COUNT = 10;

function toRequestFilters(form: RequestFilterForm): RequestFilters {
  const filters: RequestFilters = {};
  if (form.timeRange !== "all") {
    const until = Math.floor(Date.now() / 1000);
    filters.since = until - RANGE_SECONDS[form.timeRange];
    filters.until = until;
  }
  if (form.groupId) filters.group_id = form.groupId;
  if (form.logicalModel) filters.logical_model = form.logicalModel;
  if (form.status) filters.status = form.status;
  if (form.errorCode.trim()) filters.error_code = form.errorCode.trim();
  if (form.requestId.trim()) filters.request_id = form.requestId.trim();
  return filters;
}

function formatTokenPair(input: number | null, output: number | null): string {
  if (input === null || output === null) return "—";
  return `${input.toLocaleString()} / ${output.toLocaleString()}`;
}

function formatFirstToken(ms: number | null): string {
  return ms === null ? "—" : `${(ms / 1000).toFixed(1)}s`;
}

function attemptTarget(attempt: AttemptRecord): string {
  return attempt.account_id ?? attempt.upstream_model ?? "—";
}

function attemptOutcomeTone(
  outcome: string,
): "success" | "danger" | "neutral" {
  switch (outcome) {
    case "ok":
      return "success";
    case "failed":
      return "danger";
    case "missing_endpoint":
      return "neutral";
    default:
      return "neutral";
  }
}

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

export function Requests({ data }: { data: Data; refresh: () => Promise<void> }) {
  const toast = useToast();
  const [filters, setFilters] = useState<RequestFilterForm>(() => ({ ...EMPTY_FILTERS }));
  const [appliedFilters, setAppliedFilters] = useState<RequestFilters>({});
  const [records, setRecords] = useState(data.requests);
  const [total, setTotal] = useState(data.requests.length);
  const [page, setPage] = useState(1);
  const [loading, setLoading] = useState(true);
  const [expandedRequestIds, setExpandedRequestIds] = useState<Set<string>>(
    () => new Set(),
  );

  const logicalModels = useMemo(
    () =>
      Array.from(new Set(data.models.map((model) => model.name))).sort((left, right) =>
        left.localeCompare(right),
      ),
    [data.models],
  );

  useEffect(() => {
    let active = true;
    setLoading(true);
    void api
      .requests({
        ...appliedFilters,
        limit: PAGE_SIZE,
        offset: (page - 1) * PAGE_SIZE,
      })
      .then((result) => {
        if (!active) return;
        const lastPage = Math.max(1, Math.ceil(result.total / PAGE_SIZE));
        setTotal(result.total);
        if (page > lastPage) {
          setPage(lastPage);
          return;
        }
        setRecords(result.data);
        setExpandedRequestIds(new Set());
      })
      .catch((cause: unknown) => {
        if (!active) return;
        toast.error(cause instanceof Error ? cause.message : "查询请求记录失败");
      })
      .finally(() => {
        if (active) setLoading(false);
      });
    return () => {
      active = false;
    };
  }, [appliedFilters, data.requests, page, toast]);

  const totalPages = Math.max(1, Math.ceil(total / PAGE_SIZE));
  const hasFilters = Object.keys(appliedFilters).length > 0;

  const updateFilter = <K extends keyof RequestFilterForm>(
    key: K,
    value: RequestFilterForm[K],
  ) => {
    setFilters((current) => ({ ...current, [key]: value }));
  };

  const query = () => {
    setPage(1);
    setAppliedFilters(toRequestFilters(filters));
  };

  const reset = () => {
    setFilters({ ...EMPTY_FILTERS });
    setPage(1);
    setAppliedFilters({});
  };

  const accountName = (id: string | null) =>
    id ? (data.accounts.find((account) => account.id === id)?.name ?? id) : "—";

  const toggleExpanded = (requestId: string) => {
    setExpandedRequestIds((current) => {
      const next = new Set(current);
      if (next.has(requestId)) {
        next.delete(requestId);
      } else {
        next.add(requestId);
      }
      return next;
    });
  };

  return (
    <Card
      title="请求记录"
      description="默认保留 30 天。不保存消息正文、图片、工具参数或思考内容。"
    >
      <form
        className="table-filters request-filters"
        onSubmit={(event) => {
          event.preventDefault();
          query();
        }}
      >
        <div className="table-filter-fields">
          <Field label="时间范围">
            {(id) => (
              <select
                id={id}
                className="select"
                value={filters.timeRange}
                onChange={(event) =>
                  updateFilter("timeRange", event.target.value as TimeRange)
                }
              >
                <option value="1h">近 1 小时</option>
                <option value="24h">近 24 小时</option>
                <option value="7d">近 7 天</option>
                <option value="all">全部</option>
              </select>
            )}
          </Field>
          <Field label="分组">
            {(id) => (
              <select
                id={id}
                className="select"
                value={filters.groupId}
                onChange={(event) => updateFilter("groupId", event.target.value)}
              >
                <option value="">全部分组</option>
                {data.groups.map((group) => (
                  <option key={group.id} value={group.id}>
                    {group.name}
                  </option>
                ))}
              </select>
            )}
          </Field>
          <Field label="逻辑模型">
            {(id) => (
              <select
                id={id}
                className="select"
                value={filters.logicalModel}
                onChange={(event) => updateFilter("logicalModel", event.target.value)}
              >
                <option value="">全部模型</option>
                {logicalModels.map((model) => (
                  <option key={model} value={model}>
                    {model}
                  </option>
                ))}
              </select>
            )}
          </Field>
          <Field label="状态">
            {(id) => (
              <select
                id={id}
                className="select"
                value={filters.status}
                onChange={(event) =>
                  updateFilter("status", event.target.value as RequestFilterForm["status"])
                }
              >
                <option value="">全部</option>
                <option value="ok">只有成功</option>
                <option value="error">只有失败</option>
              </select>
            )}
          </Field>
          <Field label="错误码">
            {(id) => (
              <input
                id={id}
                className="input mono"
                value={filters.errorCode}
                placeholder="如 upstream_timeout"
                onChange={(event) => updateFilter("errorCode", event.target.value)}
              />
            )}
          </Field>
          <Field label="请求 ID">
            {(id) => (
              <input
                id={id}
                className="input mono"
                value={filters.requestId}
                placeholder="完整请求 ID"
                onChange={(event) => updateFilter("requestId", event.target.value)}
              />
            )}
          </Field>
        </div>
        <div className="table-filter-actions">
          <Button type="submit" variant="primary" disabled={loading}>
            查询
          </Button>
          <Button type="button" onClick={reset} disabled={loading && !hasFilters}>
            重置
          </Button>
        </div>
      </form>

      {records.length === 0 ? (
        <EmptyState
          icon={<IconInbox size={19} />}
          title={loading ? "正在查询请求记录" : hasFilters ? "没有符合条件的记录" : "还没有请求记录"}
          description={
            loading
              ? "正在从后台读取当前页。"
              : !hasFilters
              ? "用分组 Key 向 /v1/messages 或 /v1/chat/completions 发一次请求就会出现。元数据由后台任务批量落盘，热路径不等待写入。"
              : "调整筛选条件后重新查询。"
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
                <th>Token</th>
                <th>首字</th>
                <th>耗时</th>
                <th>调度</th>
                <th>倍率</th>
                <th>状态</th>
              </tr>
            </thead>
            <tbody>
              {records.map((record) => {
                const expanded = expandedRequestIds.has(record.request_id);
                return (
                  <Fragment key={record.request_id}>
                    <tr
                      className={`request-row${record.degraded !== null ? " row-degraded" : ""}`}
                      tabIndex={0}
                      aria-expanded={expanded}
                      onClick={() => toggleExpanded(record.request_id)}
                      onKeyDown={(event) => {
                        if (event.key === "Enter" || event.key === " ") {
                          event.preventDefault();
                          toggleExpanded(record.request_id);
                        }
                      }}
                    >
                      <td className="cell-dim" style={{ whiteSpace: "nowrap" }}>
                        <span className="row" style={{ gap: 8 }}>
                          <Button
                            size="sm"
                            variant="ghost"
                            className="request-expand-button"
                            aria-label={expanded ? "收起尝试明细" : "展开尝试明细"}
                            onClick={(event) => {
                              event.stopPropagation();
                              toggleExpanded(record.request_id);
                            }}
                          >
                            {expanded ? "-" : "+"}
                          </Button>
                          <span>
                            {formatTime(record.started_at)}
                            <span
                              className="mono text-faint"
                              style={{ display: "block", fontSize: 11, marginTop: 2 }}
                            >
                              {record.request_id}
                            </span>
                          </span>
                        </span>
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
                      <td className="mono cell-dim">{formatTokenPair(record.input_tokens, record.output_tokens)}</td>
                      <td className="mono cell-dim">{formatFirstToken(record.first_token_ms)}</td>
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
                    {expanded && (
                      <tr key={`${record.request_id}-details`} className="request-details-row">
                        <td colSpan={REQUEST_COLUMN_COUNT}>
                          <div className="request-details">
                            {record.attempts_detail.length === 0 ? (
                              <span className="text-faint">无尝试明细</span>
                            ) : (
                              <div className="table-wrap">
                                <table className="data request-attempts">
                                  <thead>
                                    <tr>
                                      <th>#序号</th>
                                      <th>目标</th>
                                      <th>端点</th>
                                      <th>耗时(ms)</th>
                                      <th>结果</th>
                                      <th>错误码</th>
                                      <th>是否计入预算</th>
                                    </tr>
                                  </thead>
                                  <tbody>
                                    {record.attempts_detail.map((attempt) => (
                                      <tr key={`${record.request_id}-${attempt.seq}`}>
                                        <td className="mono cell-dim">{attempt.seq}</td>
                                        <td className="mono cell-dim">{attemptTarget(attempt)}</td>
                                        <td className="cell-dim">
                                          {attempt.endpoint === null
                                            ? "—"
                                            : ENDPOINT_LABELS[attempt.endpoint] ?? attempt.endpoint}
                                        </td>
                                        <td className="mono cell-dim">{attempt.duration_ms} ms</td>
                                        <td>
                                          <Badge tone={attemptOutcomeTone(attempt.outcome)}>
                                            {attempt.outcome}
                                          </Badge>
                                        </td>
                                        <td className="mono text-faint">{attempt.error_code ?? "—"}</td>
                                        <td className="cell-dim">
                                          {attempt.counts_against_budget ? "是" : "否"}
                                        </td>
                                      </tr>
                                    ))}
                                  </tbody>
                                </table>
                              </div>
                            )}
                          </div>
                        </td>
                      </tr>
                    )}
                  </Fragment>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
      <div className="table-pagination" aria-live="polite">
        <span className="text-dim tabular">
          第 {page} / {totalPages} 页，共 {total.toLocaleString()} 条
        </span>
        <div className="row">
          <Button
            size="sm"
            disabled={loading || page <= 1}
            onClick={() => setPage((current) => Math.max(1, current - 1))}
          >
            上一页
          </Button>
          <Button
            size="sm"
            disabled={loading || page * PAGE_SIZE >= total}
            onClick={() => setPage((current) => current + 1)}
          >
            下一页
          </Button>
        </div>
      </div>
    </Card>
  );
}
