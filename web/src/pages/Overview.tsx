/**
 * 概览页。
 *
 * 配置链是 分组 → 账号 → 逻辑模型 → 调度目标，缺任何一环都无法对外服务，
 * 而错误只会在真实调用时以 503 的形式暴露。所以配置未完成时，这一页的主体
 * 是一份引导清单：直接告诉你缺哪一步、点哪里补。
 */
import type { Data } from "../lib/store";
import type { Route } from "../routes";
import { Badge, Button, Card, EmptyState } from "../components/ui";
import {
  IconAlert,
  IconArrowRight,
  IconCheck,
  IconCube,
  IconInbox,
  IconKey,
  IconRoute,
  IconServer,
} from "../components/Icons";
import { formatRelative, formatDuration, formatStaleFor, statusTone } from "../lib/format";
import { TARGET_STATUS_LABELS } from "../lib/types";
import type { TargetStatus } from "../lib/types";

export function Overview({
  data,
  navigate,
}: {
  data: Data;
  navigate: (route: Route) => void;
}) {
  const { overview, groups, accounts, models, targets, requests } = data;

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
      title: "定义逻辑模型",
      desc: "下游看到的模型名，隐藏真实上游与账号",
      done: models.length > 0,
      route: "models" as const,
    },
    {
      title: "绑定调度目标",
      desc: "把逻辑模型接到「账号 + 具体上游模型」上",
      done: targets.length > 0,
      route: "targets" as const,
    },
  ];
  const currentStep = steps.findIndex((step) => !step.done);
  const ready = currentStep === -1;

  const failures = requests.filter((r) => r.http_status >= 400).length;
  const successRate =
    requests.length > 0
      ? Math.round(((requests.length - failures) / requests.length) * 100)
      : null;

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

      <div className="stat-grid">
        <Stat
          icon={<IconKey size={13} />}
          label="分组"
          value={overview.groups}
          hint={`${accounts.length} 个上游账号`}
        />
        <Stat
          icon={<IconCube size={13} />}
          label="对外可用模型"
          value={overview.listable_models}
          hint={
            overview.logical_models > overview.listable_models
              ? `${overview.logical_models - overview.listable_models} 个因零目标未上架`
              : "全部已上架"
          }
        />
        <Stat
          icon={<IconRoute size={13} />}
          label="调度目标"
          value={overview.dispatch_targets}
          hint={
            unhealthy.length === 0
              ? overview.dispatch_targets > 0
                ? `全部正常 · ${overview.sticky_bindings} 个粘性绑定`
                : undefined
              : unhealthy.map(([status, count]) => `${count} 个${TARGET_STATUS_LABELS[status]}`).join("，")
          }
        />
        <Stat
          icon={<IconInbox size={13} />}
          label="近期成功率"
          value={successRate === null ? "—" : `${successRate}%`}
          hint={
            requests.length === 0
              ? "尚无请求"
              : `最近 ${requests.length} 次请求中 ${failures} 次失败`
          }
        />
      </div>

      {!ready && (
        <Card
          title="配置进度"
          description="完成这四步后，下游客户端即可开始调用。"
        >
          <div className="steps">
            {steps.map((step, index) => (
              <div
                key={step.title}
                className={`step ${step.done ? "step-done" : index === currentStep ? "step-current" : ""}`}
              >
                <div className="step-num">
                  {step.done ? <IconCheck size={13} /> : index + 1}
                </div>
                <div>
                  <div className="step-title">{step.title}</div>
                  <div className="step-desc">{step.desc}</div>
                </div>
                {index === currentStep && (
                  <div className="step-action">
                    <Button
                      variant="primary"
                      size="sm"
                      onClick={() => navigate(step.route)}
                      icon={<IconArrowRight size={13} />}
                    >
                      去配置
                    </Button>
                  </div>
                )}
              </div>
            ))}
          </div>
        </Card>
      )}

      {ready && <ReadyPanel data={data} />}

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
function ReadyPanel({ data }: { data: Data }) {
  const firstGroup = data.groups[0];
  const sample = data.models.find((model) => model.listed);

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
        <pre
          className="key-reveal"
          style={{ display: "block", margin: 0, whiteSpace: "pre-wrap" }}
        >
{`# Claude Code / Anthropic SDK
export ANTHROPIC_BASE_URL=${window.location.origin}
export ANTHROPIC_AUTH_TOKEN=${firstGroup?.key_prefix ?? "akh-"}…

# OpenAI 客户端
curl ${window.location.origin}/v1/chat/completions \\
  -H "Authorization: Bearer ${firstGroup?.key_prefix ?? "akh-"}…" \\
  -d '{"model":"${sample?.name ?? "你的逻辑模型"}","messages":[]}'`}
        </pre>
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
}: {
  icon: React.ReactNode;
  label: string;
  value: React.ReactNode;
  hint?: string;
}) {
  return (
    <div className="stat">
      <div className="stat-label">
        {icon}
        {label}
      </div>
      <div className="stat-value tabular">{value}</div>
      {hint && <div className="stat-hint">{hint}</div>}
    </div>
  );
}

export const OverviewIcon = IconServer;
