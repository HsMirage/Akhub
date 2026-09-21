/**
 * 概览页。
 *
 * 配置链是 分组 → 账号 → 账号里的模型目录（自动生成调度目标），缺任何一环
 * 都无法对外服务，而错误只会在真实调用时以 503 的形式暴露。所以配置未完成时，
 * 这一页的主体是一份引导清单：直接告诉你缺哪一步、点哪里补。
 */
import { useEffect, useState } from "react";

import { api } from "../lib/api";
import type { Data } from "../lib/store";
import type { RequestRecord } from "../lib/types";
import type { Route } from "../routes";
import { Badge, Button, Card, CopyButton, EmptyState, InfoTip } from "../components/ui";
import { TrendChart } from "../components/TrendChart";
import {
  IconAlert,
  IconArrowRight,
  IconCheck,
  IconCube,
  IconGauge,
  IconInbox,
  IconKey,
  IconRoute,
  IconServer,
} from "../components/Icons";
import {
  formatChangeAction,
  formatChangeResult,
  formatDuration,
  formatRelative,
  formatStaleFor,
  formatTime,
  statusTone,
} from "../lib/format";
import { TARGET_STATUS_LABELS } from "../lib/types";
import type { TargetStatus } from "../lib/types";
import { errorCodeInfo } from "../lib/error-codes";

export function Overview({
  data,
  navigate,
}: {
  data: Data;
  navigate: (route: Route, params?: Record<string, string>) => void;
}) {
  const { overview, groups, accounts, models, targets } = data;

  // "最近请求"自己拉一小片：请求记录不参与全局刷新（它是全量刷新里最贵的一项，
  // 其他页面又完全用不到），所以这里按需取最近 6 条。
  const [requests, setRequests] = useState<RequestRecord[]>([]);
  useEffect(() => {
    let alive = true;
    void api
      .requests({ limit: 6 })
      .then((result) => {
        if (alive) setRequests(result.data);
      })
      .catch(() => {
        if (alive) setRequests([]);
      });
    return () => {
      alive = false;
    };
  }, [overview.requests, overview.trend]);

  const steps = [
    {
      title: "创建分组",
      desc: "分组是调度的硬边界，同时签发下游 API Key",
      done: groups.length > 0,
      route: "groups" as const,
    },
    {
      title: "接入上游账号",
      desc: "填 Base URL 与上游 Key，不需要填任何模型能力字段",
      done: accounts.length > 0,
      route: "accounts" as const,
    },
    {
      title: "配置模型名",
      desc: "在账号的模型管理里获取模型；把同一个模型填成相同的下游模型名，就会合并显示",
      done: models.length > 0,
      route: "accounts" as const,
    },
    {
      title: "启用调度",
      desc: "启用模型后自动生成调度目标，按账号评分与可用性自动分配",
      done: targets.length > 0,
      route: "targets" as const,
    },
  ];
  const currentStep = steps.findIndex((step) => !step.done);
  const ready = currentStep === -1;
  const windowLabel = formatWindow(overview.window_secs);

  const unhealthy = (Object.entries(overview.target_status) as [TargetStatus, number][]).filter(
    ([status, count]) => status !== "active" && count > 0,
  );

  return (
    <>
      <Alerts
        overview={overview}
        unhealthy={unhealthy}
        navigate={navigate}
      />

      {/* 保留期为 0 时明细不落库，统计只覆盖当日——不说清楚会被误读成"没数据"（§24.2）。 */}
      {overview.retention_off && (
        <div className="callout callout-warn" role="status">
          <span style={{ flex: 1 }}>
            请求元数据保留天数当前为 0：明细不落库，这里的运行指标来自内存汇总，
            <b>只覆盖当天</b>，重启后清零。
          </span>
          <Button size="sm" variant="secondary" onClick={() => navigate("settings")}>
            去设置
          </Button>
        </div>
      )}

      <div className="stat-sections">
        <section className="stat-section">
          <div className="stat-section-head">
            <h2 className="stat-section-title">配置规模</h2>
            <span className="stat-section-desc">
              这条链路缺任何一环，请求都会以 503 失败
            </span>
          </div>
          <div className="stat-grid">
            <Stat
              icon={<IconKey size={13} />}
              label="分组"
              value={overview.groups}
              hint="调度的硬边界"
              onClick={() => navigate("groups")}
            />
            <Stat
              icon={<IconServer size={13} />}
              label="上游账号"
              value={accounts.length}
              hint={`${overview.groups} 个分组内`}
              onClick={() => navigate("accounts")}
            />
            <Stat
              icon={<IconCube size={13} />}
              label="对外可用模型"
              value={overview.listable_models}
              hint={
                overview.unlisted_models > 0
                  ? `${overview.unlisted_models} 个逻辑模型因零目标未上架`
                  : `${overview.logical_models} 个逻辑模型全部已上架`
              }
              onClick={() => navigate("targets")}
            />
            <Stat
              icon={<IconRoute size={13} />}
              label="调度目标"
              value={overview.dispatch_targets}
              hint={
                unhealthy.length === 0
                  ? overview.dispatch_targets > 0
                    ? `全部正常 · ${overview.sticky_bindings} 个粘性绑定`
                    : "还没有目标"
                  : unhealthy
                      .map(([status, count]) => `${count} 个${TARGET_STATUS_LABELS[status]}`)
                      .join("，")
              }
              tone={unhealthy.length > 0 ? "warn" : undefined}
              onClick={() => navigate("targets")}
            />
          </div>
        </section>

        <section className="stat-section">
          <div className="stat-section-head">
            <h2 className="stat-section-title">流量健康</h2>
            <span className="stat-section-desc">{windowLabel}窗口 · 按开始时间统计</span>
          </div>
          <div className="stat-grid">
            <Stat
              icon={<IconInbox size={13} />}
              label="窗口内请求数"
              value={formatMetricNumber(overview.requests)}
              hint="点击查看请求明细"
              onClick={() => navigate("requests")}
            />
            <Stat
              icon={<IconCheck size={13} />}
              label="成功率"
              value={formatSuccessRate(overview.success_rate)}
              hint={`${windowLabel}窗口`}
              tone={
                overview.success_rate !== null && overview.success_rate < 0.95
                  ? "warn"
                  : undefined
              }
            />
            <Stat
              icon={<IconGauge size={13} />}
              label="平均延迟"
              value={formatMetricDuration(overview.avg_latency_ms)}
              hint={`P50 ${formatMetricDuration(overview.p50_latency_ms)}`}
            />
            <Stat
              icon={<IconGauge size={13} />}
              label="P95 延迟"
              value={formatMetricDuration(overview.p95_latency_ms)}
              hint={`${windowLabel}窗口尾部延迟`}
            />
          </div>
        </section>

        <section className="stat-section">
          <div className="stat-section-head">
            <h2 className="stat-section-title">队列与并发</h2>
            <span className="stat-section-desc">实时值，随调度变化即时更新</span>
          </div>
          <div className="stat-grid">
            <Stat
              icon={<IconRoute size={13} />}
              label="当前在途"
              value={formatMetricNumber(overview.in_flight)}
              hint="流式请求在连接结束后扣除"
            />
            <Stat
              icon={<IconInbox size={13} />}
              label="当前排队"
              value={formatMetricNumber(overview.queued)}
              hint="正在等待调度的请求"
            />
            <Stat
              icon={<IconAlert size={13} />}
              label="队列超时数"
              value={formatMetricNumber(overview.queue_timeouts)}
              hint={`${windowLabel}窗口`}
              tone={overview.queue_timeouts > 0 ? "warn" : undefined}
            />
          </div>
        </section>
      </div>

      <Card
        title="请求趋势"
        description={`${windowLabel}窗口，按小时聚合；柱子深色部分为成功请求。`}
      >
        <div className="card-body">
          <TrendChart points={overview.trend} />
        </div>
      </Card>

      {!ready && (
        <Card
          title="配置进度"
          description="完成这四步后，下游客户端即可开始调用。"
        >
          <div className="steps">
            {steps.map((step, index) => (
              <div
                key={step.title}
                className={`step is-clickable ${step.done ? "step-done" : index === currentStep ? "step-current" : ""}`}
                role="button"
                tabIndex={0}
                title={`前往「${step.title}」`}
                onClick={() => navigate(step.route)}
                onKeyDown={(event) => {
                  if (event.key === "Enter" || event.key === " ") {
                    event.preventDefault();
                    navigate(step.route);
                  }
                }}
              >
                <div className="step-num">
                  {step.done ? <IconCheck size={13} /> : index + 1}
                </div>
                <div>
                  <div className="step-title">{step.title}</div>
                  <div className="step-desc">{step.desc}</div>
                </div>
                <div className="step-action">
                  {index === currentStep ? (
                    <Button
                      variant="primary"
                      size="sm"
                      onClick={(event) => {
                        event.stopPropagation();
                        navigate(step.route);
                      }}
                      icon={<IconArrowRight size={13} />}
                    >
                      去配置
                    </Button>
                  ) : step.done ? (
                    <span className="step-link">
                      查看 <IconArrowRight size={12} />
                    </span>
                  ) : null}
                </div>
              </div>
            ))}
          </div>
        </Card>
      )}

      {ready && <ReadyPanel data={data} navigate={navigate} />}

      <div className="overview-list-grid">
        <Card
          title="最近错误"
          description={`${windowLabel}窗口内最近 5 条失败请求。点击行可查看调度明细。`}
          actions={
            overview.recent_errors.length > 0 && (
              <Button size="sm" onClick={() => navigate("requests", { status: "error" })}>
                查看全部失败
              </Button>
            )
          }
        >
          {overview.recent_errors.length === 0 ? (
            <div className="table-empty">窗口内没有失败请求</div>
          ) : (
            <div className="table-wrap">
              <table className="data">
                <thead>
                  <tr>
                    <th>时间</th>
                    <th>请求 ID</th>
                    <th>模型</th>
                    <th>状态码</th>
                    <th>错误码</th>
                  </tr>
                </thead>
                <tbody>
                  {overview.recent_errors.slice(0, 5).map((error) => (
                    <tr
                      key={error.request_id}
                      className="is-clickable"
                      title="点击查看该请求的调度明细"
                      onClick={() =>
                        navigate("requests", { request_id: error.request_id })
                      }
                    >
                      <td className="cell-dim">{formatTime(error.started_at)}</td>
                      <td>
                        <div className="row" style={{ gap: 6 }}>
                          <div
                            className="mono cell-dim cell-truncate"
                            style={{ maxWidth: 150 }}
                            title={error.request_id}
                          >
                            {error.request_id}
                          </div>
                          <span onClick={(event) => event.stopPropagation()}>
                            <CopyButton
                              value={error.request_id}
                              iconOnly
                              label="复制请求 ID"
                            />
                          </span>
                        </div>
                      </td>
                      <td className="mono">{error.logical_model ?? "—"}</td>
                      <td>
                        <Badge tone={statusTone(error.http_status)}>
                          {error.http_status}
                        </Badge>
                      </td>
                      <td className="mono text-faint" title={error.error_code ?? undefined}>
                        <span className="row" style={{ gap: 4 }}>
                          {error.error_code ?? "—"}
                          {errorCodeInfo(error.error_code) && (
                            <InfoTip
                              label={`错误码说明：${errorCodeInfo(error.error_code)?.label ?? ""}`}
                            >
                              <b>{errorCodeInfo(error.error_code)?.label}</b>
                              <br />
                              {errorCodeInfo(error.error_code)?.hint}
                            </InfoTip>
                          )}
                        </span>
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </Card>

        <Card
          title="最近配置变化"
          description={`最近 5 条管理操作，时间范围为 ${windowLabel}。`}
        >
          {overview.recent_changes.length === 0 ? (
            <div className="table-empty">窗口内没有配置变化</div>
          ) : (
            <div className="table-wrap">
              <table className="data">
                <thead>
                  <tr>
                    <th>时间</th>
                    <th>动作</th>
                    <th>对象</th>
                    <th>结果</th>
                  </tr>
                </thead>
                <tbody>
                  {overview.recent_changes.slice(0, 5).map((change, index) => (
                    <tr key={`${change.occurred_at}-${change.action}-${index}`}>
                      <td className="cell-dim">{formatTime(change.occurred_at)}</td>
                      <td>
                        <div>{formatChangeAction(change.action)}</div>
                        <div className="text-faint" style={{ fontSize: 11 }}>
                          {change.actor}
                        </div>
                      </td>
                      <td>
                        <div
                          className="mono cell-dim cell-truncate"
                          style={{ maxWidth: 180 }}
                          title={change.object}
                        >
                          {change.object}
                        </div>
                      </td>
                      <td className="mono cell-dim">
                        {formatChangeResult(change.result)}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </Card>
      </div>

      <Card
        title="最近请求"
        description="只记录元数据，不保存消息正文、图片、工具参数或思考内容。"
        actions={
          requests.length > 0 && (
            <Button size="sm" onClick={() => navigate("requests")}>
              查看全部
            </Button>
          )
        }
      >
        {requests.length === 0 ? (
          <EmptyState
            icon={<IconInbox size={19} />}
            title="还没有请求经过网关"
            description={
              ready
                ? "配置已就绪。用分组 Key 向 /v1/messages 或 /v1/chat/completions 发一次请求，这里就会出现记录。"
                : "先完成上面的配置步骤。"
            }
          />
        ) : (
          <div className="table-wrap">
            <table className="data">
              <thead>
                <tr>
                  <th>时间</th>
                  <th>逻辑模型</th>
                  <th>协议</th>
                  <th>耗时</th>
                  <th>状态</th>
                </tr>
              </thead>
              <tbody>
                {requests.slice(0, 6).map((record) => (
                  <tr key={record.request_id}>
                    <td className="cell-dim">{formatRelative(record.started_at)}</td>
                    <td className="mono">{record.logical_model ?? "—"}</td>
                    <td className="cell-dim">
                      {record.streaming ? "流式" : "普通"}
                    </td>
                    <td className="cell-dim">{formatDuration(record.duration_ms)}</td>
                    <td>
                      <Badge tone={statusTone(record.http_status)}>
                        {record.http_status}
                        {record.error_code ? ` ${record.error_code}` : ""}
                      </Badge>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>
    </>
  );
}

/**
 * 顶部告警（§11.4）。
 *
 * 黄色：倍率过期但在宽限期内，仍可调度但已降权。红色：宽限期已结束硬停，
 * 或探针侧系统性故障。这两类都要求管理员动手，不能藏在列表里。
 */
function Alerts({
  overview,
  unhealthy,
  navigate,
}: {
  overview: Data["overview"];
  unhealthy: [TargetStatus, number][];
  navigate: (route: Route) => void;
}) {
  const items: { tone: "warn" | "danger"; text: string; route: Route }[] = [];
  if (overview.probe_systemic_failure) {
    items.push({
      tone: "danger",
      text: "超过半数账号在同一轮倍率刷新中失败，判定为探针侧系统性故障。所有账号的宽限期已统一延长到 60 分钟；请检查 Akhub 的出站网络。",
      route: "accounts",
    });
  }
  if (overview.multiplier_unknown.length > 0) {
    items.push({
      tone: "danger",
      text: `${overview.multiplier_unknown.map((a) => `「${a.name}」`).join("")} 的倍率未知且已超过宽限期，已被硬停。刷新成功后会自动恢复。`,
      route: "accounts",
    });
  }
  if (overview.multiplier_stale.length > 0) {
    items.push({
      tone: "warn",
      text: `${overview.multiplier_stale
        .map((a) => `「${a.name}」已过期 ${formatStaleFor(a.stale_for ?? 0)}`)
        .join("，")}。宽限期内仍参与调度，但层内评分已降权。`,
      route: "accounts",
    });
  }
  if (overview.missing_endpoints > 0) {
    items.push({
      tone: "warn",
      text: `有 ${overview.missing_endpoints} 条「上游没有这个端点」的证据。这本身不是故障——Akhub 已经改走跨协议转换——但如果账号的首选协议填对了，通常不该出现它。证据 24 小时后自动过期。`,
      route: "accounts",
    });
  }
  const hardStopped = unhealthy.filter(([status]) => status === "key_invalid" || status === "quota_exhausted");
  if (hardStopped.length > 0) {
    items.push({
      tone: "danger",
      text: hardStopped
        .map(([status, count]) => `${count} 个目标${TARGET_STATUS_LABELS[status]}`)
        .join("，"),
      route: "targets",
    });
  }
  if (items.length === 0) return null;

  return (
    <div className="alerts">
      {items.map((item) => (
        <div key={item.text} className={`callout callout-${item.tone}`}>
          <IconAlert size={15} />
          <span style={{ flex: 1 }}>{item.text}</span>
          <Button size="sm" variant="ghost" onClick={() => navigate(item.route)}>
            查看
          </Button>
        </div>
      ))}
    </div>
  );
}

/** 配置就绪后展示接入方式，省得用户去翻文档。 */
function ReadyPanel({
  data,
  navigate,
}: {
  data: Data;
  navigate: (route: Route, params?: Record<string, string>) => void;
}) {
  const firstGroup = data.groups[0];
  const sample = data.models.find((model) => model.listed);
  const origin = window.location.origin;
  const keyPlaceholder = "<YOUR_GROUP_KEY>";
  const modelName = sample?.name ?? "你的逻辑模型";
  const anthropicCmd = [
    "# Claude Code / Anthropic SDK",
    `export ANTHROPIC_BASE_URL=${origin}`,
    `export ANTHROPIC_AUTH_TOKEN=${keyPlaceholder}`,
  ].join("\n");
  const openaiCmd = [
    "# OpenAI 客户端",
    `curl ${origin}/v1/chat/completions \\`,
    `  -H "Authorization: Bearer ${keyPlaceholder}" \\`,
    `  -d '{"model":"${modelName}","messages":[]}'`,
  ].join("\n");

  return (
    <Card
      title="接入方式"
      description="配置已就绪。下游只需要分组 Key 与逻辑模型名。"
      padded
    >
      <div className="stack">
        <div className="callout callout-info">
          <span>
            分组 Key 同时接受 <code>Authorization: Bearer</code> 与{" "}
            <code>x-api-key</code>。使用 <code>x-api-key</code> 时，
            <code>/v1/models</code> 返回 Anthropic 形状。
          </span>
        </div>

        <div className="callout callout-warn">
          <IconAlert size={15} />
          <span style={{ flex: 1 }}>
            完整分组 Key 只在创建或重置时显示一次，之后无法找回。如果还没有保存，
            请到「分组」页重置一把新 Key。
          </span>
          <Button size="sm" variant="secondary" onClick={() => navigate("groups")}>
            去分组页
          </Button>
        </div>

        <div className="code-block">
          <div className="code-block-head">
            <span>Claude Code / Anthropic SDK</span>
            <CopyButton value={anthropicCmd} label="复制命令" />
          </div>
          <pre>{anthropicCmd}</pre>
        </div>

        <div className="code-block">
          <div className="code-block-head">
            <span>OpenAI 客户端</span>
            <CopyButton value={openaiCmd} label="复制命令" />
          </div>
          <pre>{openaiCmd}</pre>
        </div>

        {firstGroup && (
          <p className="field-hint" style={{ margin: 0 }}>
            当前示例使用第一个分组「{firstGroup.name}」（Key 前缀{" "}
            <span className="mono">{firstGroup.key_prefix}…</span>）。
          </p>
        )}

        {data.overview.dropped_request_records > 0 && (
          <div className="callout callout-warn">
            <IconAlert size={15} />
            <span>
              有 {data.overview.dropped_request_records} 条请求元数据因写入队列积压被丢弃。
              这不影响推理调用本身——热路径不会等待落盘。
            </span>
          </div>
        )}
      </div>
    </Card>
  );
}

function Stat({
  icon,
  label,
  value,
  hint,
  onClick,
  tone,
}: {
  icon: React.ReactNode;
  label: string;
  value: React.ReactNode;
  hint?: string;
  /** 传入后整张卡片可点击，用于下钻到对应页面。 */
  onClick?: () => void;
  tone?: "warn";
}) {
  const className = [
    "stat",
    onClick ? "is-clickable" : "",
    tone === "warn" ? "stat-warn" : "",
  ]
    .filter(Boolean)
    .join(" ");
  const content = (
    <>
      <div className="stat-label">
        {icon}
        {label}
      </div>
      <div className="stat-value tabular">{value}</div>
      {hint && <div className="stat-hint">{hint}</div>}
    </>
  );
  if (onClick) {
    return (
      <button type="button" className={className} onClick={onClick}>
        {content}
      </button>
    );
  }
  return <div className={className}>{content}</div>;
}

export const OverviewIcon = IconServer;

function formatMetricNumber(value: number | null | undefined): string {
  return value === null || value === undefined || !Number.isFinite(value)
    ? "—"
    : value.toLocaleString();
}

function formatMetricDuration(value: number | null | undefined): string {
  return value === null || value === undefined || !Number.isFinite(value)
    ? "—"
    : formatDuration(Math.round(value));
}

function formatSuccessRate(value: number | null | undefined): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return "—";
  return `${(value * 100).toFixed(1)}%`;
}

function formatWindow(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined || !Number.isFinite(seconds)) {
    return "当前";
  }
  if (seconds % 3_600 === 0) return `${seconds / 3_600} 小时`;
  return `${seconds.toLocaleString()} 秒`;
}
