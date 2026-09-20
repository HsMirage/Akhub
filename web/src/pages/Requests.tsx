/** 请求记录：只有元数据，没有正文。 */
import { Fragment, useEffect, useMemo, useRef, useState } from "react";
import { api } from "../lib/api";
import { navigateTo } from "../lib/store";
import type { Data } from "../lib/store";
import {
  ENDPOINT_LABELS,
  PROTOCOL_LABELS,
  TARGET_STATUS_LABELS,
  type AttemptRecord,
  type RequestFilters,
  type RequestRecord,
  type RequestStatus,
} from "../lib/types";
import {
  formatAttemptOutcome,
  formatBytes,
  formatDuration,
  formatTime,
  statusTone,
} from "../lib/format";
import {
  Badge,
  Button,
  Card,
  CopyButton,
  EmptyState,
  Field,
  InfoTip,
  useToast,
} from "../components/ui";
import { errorCodeInfo } from "../lib/error-codes";
import { IconDownload, IconInbox, IconX } from "../components/Icons";

type TimeRange = "1h" | "24h" | "7d" | "all";
type SortKey = "started_at" | "duration_ms" | "http_status";
type SortDir = "asc" | "desc";

interface RequestFilterForm {
  timeRange: TimeRange;
  groupId: string;
  logicalModel: string;
  /** 实际目标账号（对应 API 的 account_id）。 */
  accountId: string;
  /** 上游模型名（对应 API 的 target_id 过滤）。 */
  upstreamModel: string;
  status: "" | RequestStatus;
  errorCode: string;
  requestId: string;
}

const PAGE_SIZES = [20, 50, 100];
const EMPTY_FILTERS: RequestFilterForm = {
  timeRange: "24h",
  groupId: "",
  logicalModel: "",
  accountId: "",
  upstreamModel: "",
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
  if (form.accountId) filters.account_id = form.accountId;
  if (form.upstreamModel.trim()) filters.target_id = form.upstreamModel.trim();
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

function attemptOutcomeTone(outcome: string): "success" | "danger" | "neutral" {
  switch (outcome) {
    case "ok":
      return "success";
    case "failed":
      return "danger";
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

function statusLabel(status: string | null): string {
  if (!status) return "—";
  return (TARGET_STATUS_LABELS as Record<string, string>)[status] ?? status;
}

function csvCell(value: unknown): string {
  return `"${String(value ?? "").replace(/"/g, '""')}"`;
}

export function Requests({
  data,
  initialParams,
}: {
  data: Data;
  refresh: () => Promise<unknown>;
  /** 从概览等页面跳转过来时携带的预置筛选条件。 */
  initialParams?: URLSearchParams;
}) {
  const toast = useToast();
  const [filters, setFilters] = useState<RequestFilterForm>(() => ({ ...EMPTY_FILTERS }));
  const [appliedFilters, setAppliedFilters] = useState<RequestFilters>({});
  const [records, setRecords] = useState<RequestRecord[]>(data.requests);
  const [total, setTotal] = useState(data.requests.length);
  const [page, setPage] = useState(1);
  const [pageSize, setPageSize] = useState(50);
  const [loading, setLoading] = useState(true);
  const [expandedRequestIds, setExpandedRequestIds] = useState<Set<string>>(
    () => new Set(),
  );
  const [sort, setSort] = useState<{ key: SortKey; dir: SortDir }>({
    key: "started_at",
    dir: "desc",
  });
  const [pageInput, setPageInput] = useState("1");
  /** 查询条件指纹：只有筛选/翻页变化才清空展开行，后台刷新不打扰阅读。 */
  const lastQueryKey = useRef("");

  const logicalModels = useMemo(
    () =>
      Array.from(new Set(data.models.map((model) => model.name))).sort((left, right) =>
        left.localeCompare(right),
      ),
    [data.models],
  );
  const upstreamModels = useMemo(
    () =>
      Array.from(new Set(data.targets.map((target) => target.upstream_model))).sort(
        (left, right) => left.localeCompare(right),
      ),
    [data.targets],
  );

  // 从概览/定位链接进入时，把 URL 参数转换成筛选条件（§6.6）。
  useEffect(() => {
    if (!initialParams) return;
    const next: RequestFilterForm = { ...EMPTY_FILTERS };
    const requestId = initialParams.get("request_id");
    if (requestId) next.requestId = requestId;
    const status = initialParams.get("status");
    if (status === "ok" || status === "error") next.status = status;
    const accountId = initialParams.get("account");
    if (accountId) next.accountId = accountId;
    const model = initialParams.get("logical_model");
    if (model) next.logicalModel = model;
    const errorCode = initialParams.get("error_code");
    if (errorCode) next.errorCode = errorCode;
    setFilters(next);
    setAppliedFilters(toRequestFilters(next));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [initialParams]);

  useEffect(() => {
    let active = true;
    setLoading(true);
    void api
      .requests({
        ...appliedFilters,
        limit: pageSize,
        offset: (page - 1) * pageSize,
      })
      .then((result) => {
        if (!active) return;
        const lastPage = Math.max(1, Math.ceil(result.total / pageSize));
        setTotal(result.total);
        if (page > lastPage) {
          setPage(lastPage);
          return;
        }
        setRecords(result.data);
        const queryKey = JSON.stringify(appliedFilters) + `|${page}|${pageSize}`;
        if (lastQueryKey.current !== queryKey) {
          lastQueryKey.current = queryKey;
          setExpandedRequestIds(new Set());
        }
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
  }, [appliedFilters, data.requests, page, pageSize, toast]);

  useEffect(() => {
    setPageInput(String(page));
  }, [page]);

  // 把筛选条件写回 hash（replaceState 不触发 hashchange，不会造成循环）：
  // 刷新页面或分享链接后仍能回到同一批筛选结果。
  useEffect(() => {
    const params = new URLSearchParams();
    if (appliedFilters.request_id) params.set("request_id", appliedFilters.request_id);
    if (appliedFilters.status) params.set("status", appliedFilters.status);
    if (appliedFilters.account_id) params.set("account", appliedFilters.account_id);
    if (appliedFilters.logical_model) params.set("logical_model", appliedFilters.logical_model);
    if (appliedFilters.target_id) params.set("upstream_model", appliedFilters.target_id);
    if (appliedFilters.error_code) params.set("error_code", appliedFilters.error_code);
    const query = params.toString();
    const nextHash = `#/requests${query ? `?${query}` : ""}`;
    if (window.location.hash !== nextHash) {
      window.history.replaceState(null, "", nextHash);
    }
  }, [appliedFilters]);

  const totalPages = Math.max(1, Math.ceil(total / pageSize));
  const hasFilters = Object.keys(appliedFilters).length > 0;

  const sortedRecords = useMemo(() => {
    const list = [...records];
    const direction = sort.dir === "asc" ? 1 : -1;
    list.sort((left, right) => {
      const a = left[sort.key];
      const b = right[sort.key];
      if (typeof a === "number" && typeof b === "number") return (a - b) * direction;
      return 0;
    });
    return list;
  }, [records, sort]);

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

  const removeFilter = (key: keyof RequestFilterForm) => {
    const next = { ...filters, [key]: EMPTY_FILTERS[key] };
    setFilters(next);
    setPage(1);
    setAppliedFilters(toRequestFilters(next));
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

  const toggleSort = (key: SortKey) => {
    setSort((current) =>
      current.key === key
        ? { key, dir: current.dir === "asc" ? "desc" : "asc" }
        : { key, dir: "desc" },
    );
  };

  const exportCsv = () => {
    const header = [
      "时间",
      "请求ID",
      "逻辑模型",
      "协议",
      "流式",
      "上游账号",
      "上游模型",
      "请求体积",
      "输入Token",
      "输出Token",
      "首字ms",
      "耗时ms",
      "状态码",
      "错误码",
      "降级",
    ];
    const rows = sortedRecords.map((record) => [
      formatTime(record.started_at),
      record.request_id,
      record.logical_model ?? "",
      PROTOCOL_LABELS[record.protocol],
      record.streaming ? "是" : "否",
      accountName(record.account_id),
      record.upstream_model ?? "",
      record.request_bytes,
      record.input_tokens ?? "",
      record.output_tokens ?? "",
      record.first_token_ms ?? "",
      record.duration_ms,
      record.http_status,
      record.error_code ?? "",
      record.degraded ?? "",
    ]);
    const csv = [header, ...rows]
      .map((row) => row.map(csvCell).join(","))
      .join("\r\n");
    const blob = new Blob(["\ufeff" + csv], { type: "text/csv;charset=utf-8" });
    const url = URL.createObjectURL(blob);
    const anchor = document.createElement("a");
    anchor.href = url;
    anchor.download = `akhub-requests-page${page}.csv`;
    anchor.click();
    URL.revokeObjectURL(url);
    toast.success("已导出当前页记录");
  };

  const chips: { label: string; clear: () => void }[] = [];
  if (appliedFilters.request_id) {
    chips.push({
      label: `请求 ID：${appliedFilters.request_id}`,
      clear: () => removeFilter("requestId"),
    });
  }
  if (appliedFilters.status) {
    chips.push({
      label: `状态：${appliedFilters.status === "ok" ? "成功" : "失败"}`,
      clear: () => removeFilter("status"),
    });
  }
  if (appliedFilters.logical_model) {
    chips.push({
      label: `模型：${appliedFilters.logical_model}`,
      clear: () => removeFilter("logicalModel"),
    });
  }
  if (appliedFilters.account_id) {
    chips.push({
      label: `账号：${accountName(appliedFilters.account_id)}`,
      clear: () => removeFilter("accountId"),
    });
  }
  if (appliedFilters.target_id) {
    chips.push({
      label: `上游模型：${appliedFilters.target_id}`,
      clear: () => removeFilter("upstreamModel"),
    });
  }
  if (appliedFilters.error_code) {
    chips.push({
      label: `错误码：${appliedFilters.error_code}`,
      clear: () => removeFilter("errorCode"),
    });
  }
  if (appliedFilters.since !== undefined) {
    chips.push({
      label:
        filters.timeRange === "1h"
          ? "时间：近 1 小时"
          : filters.timeRange === "24h"
            ? "时间：近 24 小时"
            : "时间：近 7 天",
      clear: () => removeFilter("timeRange"),
    });
  }

  const retentionText =
    data.settings.retention_days === 0
      ? "当前未开启历史明细（保留天数为 0），这里只能看到内存汇总周期内的记录。"
      : `当前保留最近 ${data.settings.retention_days} 天；默认查询近 24 小时。`;

  return (
    <Card
      title="请求记录"
      description={`只记录元数据，不保存消息正文、图片、工具参数或思考内容。${retentionText}`}
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
          <Field label="上游账号">
            {(id) => (
              <select
                id={id}
                className="select"
                value={filters.accountId}
                onChange={(event) => updateFilter("accountId", event.target.value)}
              >
                <option value="">全部账号</option>
                {data.accounts.map((account) => (
                  <option key={account.id} value={account.id}>
                    {account.name}
                  </option>
                ))}
              </select>
            )}
          </Field>
          <Field label="上游模型">
            {(id) => (
              <>
                <input
                  id={id}
                  className="input mono"
                  list="request-upstream-models"
                  value={filters.upstreamModel}
                  placeholder="输入或选择上游模型名"
                  onChange={(event) => updateFilter("upstreamModel", event.target.value)}
                />
                <datalist id="request-upstream-models">
                  {upstreamModels.map((model) => (
                    <option key={model} value={model} />
                  ))}
                </datalist>
              </>
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
          <Button type="button" onClick={reset} disabled={!hasFilters && filters.timeRange === "24h"}>
            重置
          </Button>
        </div>
      </form>

      {chips.length > 0 && (
        <div className="filter-chips" aria-label="当前筛选条件">
          {chips.map((chip) => (
            <span key={chip.label} className="filter-chip">
              {chip.label}
              <button
                type="button"
                aria-label={`清除筛选：${chip.label}`}
                onClick={chip.clear}
              >
                <IconX size={12} />
              </button>
            </span>
          ))}
          <button type="button" className="link-button" onClick={reset}>
            清空全部
          </button>
        </div>
      )}

      <div className="table-toolbar">
        <span className="text-faint">
          共 {total.toLocaleString()} 条 · 第 {page}/{totalPages} 页
          {records.length > 1 && " · 点击表头可按当前页排序"}
        </span>
        <div className="row" style={{ gap: 8 }}>
          <Button size="sm" icon={<IconDownload size={13} />} onClick={exportCsv} disabled={records.length === 0}>
            导出当前页
          </Button>
        </div>
      </div>

      {records.length === 0 ? (
        <EmptyState
          icon={<IconInbox size={19} />}
          title={loading ? "正在查询请求记录" : hasFilters ? "没有符合条件的记录" : "还没有请求记录"}
          description={
            loading
              ? "正在从后台读取当前页。"
              : !hasFilters
                ? "用分组 Key 向 /v1/messages 或 /v1/chat/completions 发一次请求就会出现。元数据由后台任务批量落盘，热路径不等待写入。"
                : "调整筛选条件后重新查询，或点击「重置」查看全部。"
          }
          action={
            hasFilters ? (
              <Button variant="secondary" onClick={reset}>
                重置筛选
              </Button>
            ) : undefined
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
                <th aria-sort={sort.key === "duration_ms" ? (sort.dir === "asc" ? "ascending" : "descending") : "none"}>
                  <button type="button" className="th-sort" onClick={() => toggleSort("duration_ms")}>
                    耗时 {sort.key === "duration_ms" ? (sort.dir === "asc" ? "↑" : "↓") : ""}
                  </button>
                </th>
                <th>调度</th>
                <th>倍率</th>
                <th aria-sort={sort.key === "http_status" ? (sort.dir === "asc" ? "ascending" : "descending") : "none"}>
                  <button type="button" className="th-sort" onClick={() => toggleSort("http_status")}>
                    状态 {sort.key === "http_status" ? (sort.dir === "asc" ? "↑" : "↓") : ""}
                  </button>
                </th>
              </tr>
            </thead>
            <tbody>
              {sortedRecords.map((record) => {
                const expanded = expandedRequestIds.has(record.request_id);
                return (
                  <Fragment key={record.request_id}>
                    <tr
                      className={`request-row${record.degraded !== null ? " row-degraded" : ""}`}
                      tabIndex={0}
                      aria-expanded={expanded}
                      onClick={() => {
                        // 用户正在选中文本（例如复制请求 ID）时不要触发展开。
                        if (window.getSelection()?.toString()) return;
                        toggleExpanded(record.request_id);
                      }}
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
                            <span className="row" style={{ gap: 4, marginTop: 2 }}>
                              <span
                                className="mono text-faint"
                                style={{ fontSize: 11 }}
                              >
                                {record.request_id}
                              </span>
                              <span onClick={(event) => event.stopPropagation()}>
                                <CopyButton
                                  value={record.request_id}
                                  iconOnly
                                  label="复制请求 ID"
                                />
                              </span>
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
                          {record.degraded && (
                            <span title={`为完成这次请求丢弃了：${record.degraded}`}>
                              <Badge tone="danger">降级 {record.degraded}</Badge>
                            </span>
                          )}
                        </div>
                      </td>
                      <td>
                        <span className="chain">
                          {record.account_id ? (
                            <button
                              type="button"
                              className="link-button"
                              title={`在「上游账号」里定位 ${accountName(record.account_id)}`}
                              onClick={() =>
                                navigateTo("accounts", { account: record.account_id ?? "" })
                              }
                            >
                              {accountName(record.account_id)}
                            </button>
                          ) : (
                            <span className="cell-dim">{accountName(record.account_id)}</span>
                          )}
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
                            <span title="容量排队等待">排队 {formatDuration(record.queued_ms)}</span>
                          )}
                          {record.sticky_wait_ms != null && record.sticky_wait_ms > 0 && (
                            <span
                              className="cell-dim"
                              title={"粘性等待（缓存新鲜度系数 " + (record.sticky_freshness ?? "—") + "）"}
                            >
                              粘性等 {formatDuration(record.sticky_wait_ms)}
                            </span>
                          )}
                          {!record.sticky_hit &&
                            record.attempts <= 1 &&
                            record.queued_ms === 0 &&
                            "直达"}
                        </span>
                      </td>
                      <td className="mono cell-dim" style={{ fontSize: 12 }}>
                        {record.effective_multiplier ?? "—"}
                        {record.multiplier_source && (
                          <div className="text-faint" style={{ fontSize: 11 }}>
                            {record.multiplier_source === "manual" ? "手动" : "自动"}
                          </div>
                        )}
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
                          <div
                            className="mono text-faint row"
                            style={{ fontSize: 11, marginTop: 2, gap: 4 }}
                          >
                            {record.error_code}
                            {errorCodeInfo(record.error_code) && (
                              <InfoTip
                                label={`错误码说明：${errorCodeInfo(record.error_code)?.label ?? ""}`}
                              >
                                <b>{errorCodeInfo(record.error_code)?.label}</b>
                                <br />
                                {errorCodeInfo(record.error_code)?.hint}
                              </InfoTip>
                            )}
                          </div>
                        )}
                      </td>
                    </tr>
                    {expanded && (
                      <tr key={`${record.request_id}-details`} className="request-details-row">
                        <td colSpan={REQUEST_COLUMN_COUNT}>
                          <div className="request-details">
                            {/* 调度诊断：解释"这次为什么这么选"（§24.1）。 */}
                            <div className="request-diagnostics">
                              <span>
                                <b>选中层</b>{" "}
                                <span className="mono">
                                  {record.selected_layer ?? "—"}
                                </span>
                              </span>
                              <span>
                                <b>额度状态</b>{" "}
                                <span className="mono">{statusLabel(record.quota_status)}</span>
                              </span>
                              <span>
                                <b>输出速度</b>{" "}
                                <span className="mono">
                                  {record.output_tps == null
                                    ? "—"
                                    : record.output_tps.toFixed(1) + " tok/s"}
                                </span>
                              </span>
                              <span>
                                <b>配置版本</b>{" "}
                                <span className="mono">{record.config_version ?? "—"}</span>
                              </span>
                              <span>
                                <b>缓存读/写</b>{" "}
                                <span className="mono">
                                  {formatTokenPair(
                                    record.cache_read_tokens,
                                    record.cache_write_tokens,
                                  )}
                                </span>
                              </span>
                              <span>
                                <b>思考 Token</b>{" "}
                                <span className="mono">{record.reasoning_tokens ?? "—"}</span>
                              </span>
                              <span className="request-diagnostics-filter">
                                <b>候选过滤</b>{" "}
                                <span className="mono">{record.filter_summary ?? "—"}</span>
                              </span>
                            </div>
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
                                        <td className="mono cell-dim">
                                          {attempt.target_id ? (
                                            <button
                                              type="button"
                                              className="link-button mono"
                                              title={`在「调度目标」里定位 ${attemptTarget(attempt)}`}
                                              onClick={() =>
                                                navigateTo("targets", {
                                                  target: attempt.target_id ?? "",
                                                })
                                              }
                                            >
                                              {attemptTarget(attempt)}
                                            </button>
                                          ) : (
                                            attemptTarget(attempt)
                                          )}
                                        </td>
                                        <td className="cell-dim">
                                          {attempt.endpoint === null
                                            ? "—"
                                            : ENDPOINT_LABELS[attempt.endpoint] ?? attempt.endpoint}
                                        </td>
                                        <td className="mono cell-dim">{attempt.duration_ms} ms</td>
                                        <td>
                                          <Badge tone={attemptOutcomeTone(attempt.outcome)}>
                                            {formatAttemptOutcome(attempt.outcome)}
                                          </Badge>
                                        </td>
                                        <td className="mono text-faint">
                                          {attempt.error_code ?? "—"}
                                          {errorCodeInfo(attempt.error_code) && (
                                            <InfoTip
                                              label={`错误码说明：${errorCodeInfo(attempt.error_code)?.label ?? ""}`}
                                            >
                                              <b>{errorCodeInfo(attempt.error_code)?.label}</b>
                                              <br />
                                              {errorCodeInfo(attempt.error_code)?.hint}
                                            </InfoTip>
                                          )}
                                        </td>
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
          <label className="pagination-size">
            每页
            <select
              className="select select-sm"
              value={pageSize}
              onChange={(event) => {
                setPageSize(Number(event.target.value));
                setPage(1);
              }}
            >
              {PAGE_SIZES.map((size) => (
                <option key={size} value={size}>
                  {size}
                </option>
              ))}
            </select>
            条
          </label>
          <Button
            size="sm"
            disabled={loading || page <= 1}
            onClick={() => setPage((current) => Math.max(1, current - 1))}
          >
            上一页
          </Button>
          <span className="pagination-jump">
            跳至
            <input
              className="input input-sm mono"
              value={pageInput}
              inputMode="numeric"
              aria-label="跳转到页码"
              onChange={(event) => setPageInput(event.target.value)}
              onKeyDown={(event) => {
                if (event.key !== "Enter") return;
                const target = Number(pageInput);
                if (Number.isInteger(target) && target >= 1 && target <= totalPages) {
                  setPage(target);
                } else {
                  setPageInput(String(page));
                }
              }}
            />
            页
          </span>
          <Button
            size="sm"
            disabled={loading || page >= totalPages}
            onClick={() => setPage((current) => Math.min(totalPages, current + 1))}
          >
            下一页
          </Button>
        </div>
      </div>
    </Card>
  );
}
