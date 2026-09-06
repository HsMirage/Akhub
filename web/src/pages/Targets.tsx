/**
 * 调度目标：分组 + 账号 + 具体上游模型。
 *
 * 这一页按「逻辑模型 → 层」两级组织。层不是配置出来的，而是优先级数字相同的
 * 目标自动构成的——把它画出来，是为了让"高层还有可用目标时绝不使用低层"
 * 这条硬规则一眼可见，而不是藏在一列数字里。
 *
 * 每一行同时显示动态状态与综合评分：前者决定它此刻能不能被选中，后者决定
 * 它在同层里分到多少流量。权重调错时，靠分维得分就能自我诊断（§6.9）。
 */
import { useMemo, useState } from "react";
import { api } from "../lib/api";
import type { Account, DispatchTarget, Limits, LogicalModel } from "../lib/types";
import { TARGET_STATUS_LABELS } from "../lib/types";
import { formatLimits, parseLimit } from "../lib/format";
import type { Data } from "../lib/store";
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
  Drawer,
  EmptyState,
  Field,
  ScoreMeter,
  useToast,
} from "../components/ui";
import { IconPlus, IconRoute, IconTrash } from "../components/Icons";

interface ResolvedTarget {
  target: DispatchTarget;
  account: Account | undefined;
}

interface Layer {
  priority: number;
  targets: ResolvedTarget[];
}

export function Targets({ data, refresh }: { data: Data; refresh: () => Promise<void> }) {
  const toast = useToast();
  const [editing, setEditing] = useState<DispatchTarget | "new" | null>(null);
  const [confirm, setConfirm] = useState<DispatchTarget | null>(null);

  /** 按逻辑模型分组，再把目标按有效优先级折叠成层。 */
  const grouped = useMemo(() => {
    const accounts = new Map(data.accounts.map((account) => [account.id, account]));
    return data.models.map((model) => {
      const resolved = data.targets
        .filter((target) => target.logical_model_id === model.id)
        .map<ResolvedTarget>((target) => ({ target, account: accounts.get(target.account_id) }));

      const byPriority = new Map<number, ResolvedTarget[]>();
      for (const item of resolved) {
        const bucket = byPriority.get(item.target.priority) ?? [];
        bucket.push(item);
        byPriority.set(item.target.priority, bucket);
      }
      const layers: Layer[] = [...byPriority.entries()]
        .sort(([a], [b]) => b - a)
        .map(([priority, targets]) => ({
          priority,
          // 同层内按综合评分降序，和调度器眼里的"谁更可能被抽中"一致。
          targets: targets.sort(
            (a, b) => (b.target.score?.total ?? 0) - (a.target.score?.total ?? 0),
          ),
        }));

      return { model, layers, count: resolved.length };
    });
  }, [data]);

  const remove = async (target: DispatchTarget) => {
    try {
      await api.deleteTarget(target.id);
      await refresh();
      toast.success("调度目标已移除");
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "删除失败");
    }
  };

  const canCreate = data.models.length > 0 && data.accounts.length > 0;

  return (
    <>
      {!canCreate && (
        <Card title="调度目标">
          <EmptyState
            icon={<IconRoute size={19} />}
            title="还差一步"
            description={
              data.accounts.length === 0
                ? "先创建至少一个上游账号，目标需要知道请求发给谁。"
                : "先创建至少一个逻辑模型，目标需要知道它对外叫什么。"
            }
          />
        </Card>
      )}

      {canCreate && (
        <Card
          title="调度目标"
          description="优先级相同的目标构成一层。高层只要还有可用目标，就绝不会使用低层；同层内按综合评分加权分配。"
          actions={
            <Button variant="primary" icon={<IconPlus />} onClick={() => setEditing("new")}>
              添加目标
            </Button>
          }
        >
          {data.targets.length === 0 ? (
            <EmptyState
              icon={<IconRoute size={19} />}
              title="还没有调度目标"
              description="把逻辑模型接到「账号 + 具体上游模型」上。绑定完成后，该模型就会出现在 /v1/models 中。"
              action={
                <Button variant="primary" icon={<IconPlus />} onClick={() => setEditing("new")}>
                  添加目标
                </Button>
              }
            />
          ) : (
            <div>
              {grouped
                .filter((entry) => entry.count > 0)
                .map((entry) => (
                  <ModelBlock
                    key={entry.model.id}
                    model={entry.model}
                    layers={entry.layers}
                    onEdit={setEditing}
                    onDelete={setConfirm}
                  />
                ))}
            </div>
          )}
        </Card>
      )}

      <TargetDrawer
        key={editing === "new" ? "new" : (editing?.id ?? "closed")}
        data={data}
        target={editing === "new" ? null : editing}
        open={editing !== null}
        onClose={() => setEditing(null)}
        onSaved={async () => {
          setEditing(null);
          await refresh();
        }}
      />

      <ConfirmDialog
        open={confirm !== null}
        title="移除调度目标"
        danger
        confirmLabel="移除"
        message="移除后该目标不再参与调度，在途请求会正常完成。"
        onClose={() => setConfirm(null)}
        onConfirm={() => void remove(confirm!)}
      />
    </>
  );
}

/** 动态状态徽标（§12.2）。 */
function StatusBadge({ target, account }: { target: DispatchTarget; account?: Account }) {
  if (!target.enabled || account?.enabled === false) {
    return (
      <Badge tone="neutral" dot>
        停用
      </Badge>
    );
  }
  if (account && account.multiplier_status === "multiplier_unknown") {
    return (
      <Badge tone="danger" dot>
        倍率未知
      </Badge>
    );
  }
  switch (target.status) {
    case "active":
      return (
        <Badge tone={account?.multiplier_status === "multiplier_stale" ? "warn" : "success"} dot>
          {account?.multiplier_status === "multiplier_stale" ? "可用 · 倍率过期" : "正常"}
        </Badge>
      );
    case "cooldown":
      return (
        <Badge tone="warn" dot>
          冷却中{target.cooldown_secs !== null ? ` · ${target.cooldown_secs}s` : ""}
        </Badge>
      );
    case "half_open":
      return (
        <Badge tone="info" dot>
          {TARGET_STATUS_LABELS.half_open}
        </Badge>
      );
    case "quota_exhausted":
    case "key_invalid":
      return (
        <Badge tone="danger" dot>
          {TARGET_STATUS_LABELS[target.status]}
        </Badge>
      );
  }
}

function ModelBlock({
  model,
  layers,
  onEdit,
  onDelete,
}: {
  model: LogicalModel;
  layers: Layer[];
  onEdit: (target: DispatchTarget) => void;
  onDelete: (target: DispatchTarget) => void;
}) {
  return (
    <div style={{ borderBottom: "1px solid var(--border)" }}>
      <div
        className="row"
        style={{ padding: "12px 24px", background: "var(--surface-hover)" }}
      >
        <span className="mono cell-strong">{model.name}</span>
        <Badge tone={model.listed ? "success" : "warn"}>
          {model.listed ? "已上架" : "未上架"}
        </Badge>
        <span className="spacer" />
        <span className="text-faint" style={{ fontSize: 12 }}>
          {layers.length} 层 · {layers.reduce((sum, l) => sum + l.targets.length, 0)} 个目标
        </span>
      </div>

      {layers.map((layer, index) => (
        <div className="layer" key={layer.priority}>
          <div className="layer-head">
            <Badge tone={index === 0 ? "accent" : "neutral"}>第 {index + 1} 层</Badge>
            <span className="layer-rank">优先级 {layer.priority}</span>
            <span className="layer-note">
              {index === 0
                ? layer.targets.length > 1
                  ? "同层，按综合评分加权分配；全忙时在本层排队，不降层"
                  : "日常流量都走这里"
                : "上一层全部不可用时才会用到"}
            </span>
          </div>

          <div className="table-wrap">
            <table className="data" style={{ tableLayout: "fixed" }}>
              {/* 每一层是独立的表格，列宽必须显式固定，否则各层的列对不齐。 */}
              <colgroup>
                <col style={{ width: "30%" }} />
                <col style={{ width: "10%" }} />
                <col style={{ width: "22%" }} />
                <col style={{ width: "16%" }} />
                <col style={{ width: "6%" }} />
                <col style={{ width: "10%" }} />
                <col style={{ width: 120 }} />
              </colgroup>
              <thead>
                <tr>
                  <th>目标</th>
                  <th>有效倍率</th>
                  <th>综合评分</th>
                  <th>限制</th>
                  <th>在途</th>
                  <th>状态</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {layer.targets.map(({ target, account }) => (
                  <tr key={target.id}>
                    <td
                      style={{ overflow: "hidden", textOverflow: "ellipsis" }}
                      title={`${account?.name ?? "账号已删除"} / ${target.upstream_model}`}
                    >
                      <span className="chain">
                        <span className="cell-strong">{account?.name ?? "账号已删除"}</span>
                        <span className="chain-arrow">/</span>
                        <span className="mono cell-dim">{target.upstream_model}</span>
                      </span>
                      {target.priority_override !== null && (
                        <div className="text-faint" style={{ fontSize: 11, marginTop: 2 }}>
                          优先级已覆盖（账号默认 {account?.default_priority ?? "—"}）
                        </div>
                      )}
                    </td>
                    <td className="mono">{account?.effective_multiplier ?? "—"}</td>
                    <td>
                      {target.score ? (
                        <ScoreMeter score={target.score} />
                      ) : (
                        <span className="text-faint">—</span>
                      )}
                    </td>
                    <td className="cell-dim" style={{ fontSize: 12 }}>
                      {formatLimits(target.effective_limits)}
                    </td>
                    <td className="mono cell-dim">{target.inflight}</td>
                    <td>
                      <StatusBadge target={target} account={account} />
                    </td>
                    <td>
                      <div className="cell-actions">
                        <Button size="sm" onClick={() => onEdit(target)}>
                          编辑
                        </Button>
                        <Button
                          size="sm"
                          variant="danger"
                          icon={<IconTrash size={13} />}
                          title="移除目标"
                          onClick={() => onDelete(target)}
                        />
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      ))}
    </div>
  );
}

function TargetDrawer({
  data,
  target,
  open,
  onClose,
  onSaved,
}: {
  data: Data;
  target: DispatchTarget | null;
  open: boolean;
  onClose: () => void;
  onSaved: () => void | Promise<void>;
}) {
  const toast = useToast();
  const editing = target !== null;
  const [modelId, setModelId] = useState(target?.logical_model_id ?? data.models[0]?.id ?? "");
  const [accountId, setAccountId] = useState(target?.account_id ?? "");
  const [upstreamModel, setUpstreamModel] = useState(target?.upstream_model ?? "");
  const [override, setOverride] = useState(target?.priority_override?.toString() ?? "");
  const [enabled, setEnabled] = useState(target?.enabled ?? true);
  const [limitsForm, setLimitsForm] = useState({
    rpm: target?.limits.rpm?.toString() ?? "",
    tpm: target?.limits.tpm?.toString() ?? "",
    max_concurrency: target?.limits.max_concurrency?.toString() ?? "",
  });
  const [busy, setBusy] = useState(false);

  const model = data.models.find((item) => item.id === modelId);
  /** 分组是硬边界：只列出与所选逻辑模型同组的账号。 */
  const candidates = data.accounts.filter(
    (account) => account.group_id === model?.group_id,
  );
  const account =
    candidates.find((item) => item.id === accountId) ?? candidates[0];

  const overrideValue = override.trim() === "" ? null : Number(override);
  const overrideError =
    overrideValue === null ||
    (Number.isInteger(overrideValue) && overrideValue >= 0 && overrideValue <= 100)
      ? null
      : "必须是 0 到 100 之间的整数";

  const limits: Limits = {
    rpm: parseLimit(limitsForm.rpm) ?? null,
    tpm: parseLimit(limitsForm.tpm) ?? null,
    max_concurrency: parseLimit(limitsForm.max_concurrency) ?? null,
  };
  const limitsInvalid = (Object.keys(limitsForm) as (keyof typeof limitsForm)[]).some(
    (key) => parseLimit(limitsForm[key]) === undefined,
  );

  const duplicate =
    !editing &&
    data.targets.some(
      (item) =>
        item.logical_model_id === modelId &&
        item.account_id === account?.id &&
        item.upstream_model === upstreamModel.trim(),
    );

  const submit = async () => {
    if (!account) return;
    setBusy(true);
    try {
      if (editing) {
        await api.updateTarget(target.id, {
          upstream_model: upstreamModel.trim(),
          priority_override: overrideValue,
          limits,
          enabled,
        });
        toast.success("调度目标已更新");
      } else {
        await api.createTarget({
          logical_model_id: modelId,
          account_id: account.id,
          upstream_model: upstreamModel.trim(),
          priority_override: overrideValue,
          limits,
        });
        toast.success("调度目标已添加");
      }
      await onSaved();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "保存失败");
    } finally {
      setBusy(false);
    }
  };

  const effectivePriority = overrideValue ?? account?.default_priority ?? 50;
  const effectiveLimits: Limits = account
    ? {
        rpm: limits.rpm ?? account.limits.rpm,
        tpm: limits.tpm ?? account.limits.tpm,
        max_concurrency: limits.max_concurrency ?? account.limits.max_concurrency,
      }
    : limits;

  return (
    <Drawer
      open={open}
      onClose={onClose}
      title={editing ? "编辑调度目标" : "添加调度目标"}
      description="目标是「分组 + 账号 + 具体上游模型」的确定组合。"
      footer={
        <>
          <Button onClick={onClose}>取消</Button>
          <Button
            variant="primary"
            onClick={submit}
            disabled={
              busy ||
              !account ||
              !upstreamModel.trim() ||
              !!overrideError ||
              limitsInvalid ||
              duplicate
            }
          >
            {busy ? "保存中…" : editing ? "保存" : "添加"}
          </Button>
        </>
      }
    >
      <div className="stack">
        <Field label="逻辑模型">
          {(id) => (
            <select
              id={id}
              className="select"
              value={modelId}
              disabled={editing}
              onChange={(e) => {
                setModelId(e.target.value);
                setAccountId("");
              }}
            >
              {data.models.map((item) => (
                <option key={item.id} value={item.id}>
                  {item.name}
                </option>
              ))}
            </select>
          )}
        </Field>

        <Field
          label="上游账号"
          hint={
            candidates.length === 0
              ? "该逻辑模型所在的分组下还没有账号。分组是硬边界，不允许跨组绑定。"
              : undefined
          }
        >
          {(id) => (
            <select
              id={id}
              className="select"
              value={account?.id ?? ""}
              disabled={candidates.length === 0 || editing}
              onChange={(e) => setAccountId(e.target.value)}
            >
              {candidates.map((item) => (
                <option key={item.id} value={item.id}>
                  {item.name}（优先级 {item.default_priority}）
                </option>
              ))}
            </select>
          )}
        </Field>

        <Field
          label="上游真实模型名"
          error={duplicate ? "该账号与模型的组合已经是调度目标" : undefined}
          hint="上游站点实际接受的名字，例如 claude-sonnet-4-5-20250929。它不会暴露给下游。"
        >
          {(id) => (
            <input
              id={id}
              className="input mono"
              value={upstreamModel}
              onChange={(e) => setUpstreamModel(e.target.value)}
              placeholder="claude-sonnet-4-5-20250929"
            />
          )}
        </Field>

        <Field
          label="优先级覆盖（可选）"
          error={overrideError ?? undefined}
          hint="留空则继承账号默认值。想让两个目标自动分担流量，就把它们设成同一个数字。"
        >
          {(id) => (
            <input
              id={id}
              className="input"
              type="number"
              min={0}
              max={100}
              value={override}
              onChange={(e) => setOverride(e.target.value)}
              placeholder={`继承账号：${account?.default_priority ?? 50}`}
            />
          )}
        </Field>

        <Field
          label="限制覆盖（可选）"
          hint={`留空的项继承账号默认值。实际生效：${formatLimits(effectiveLimits)}。`}
        >
          {() => (
            <div className="weights" style={{ gridTemplateColumns: "repeat(3, 1fr)" }}>
              {(
                [
                  ["rpm", "RPM"],
                  ["tpm", "TPM"],
                  ["max_concurrency", "最大并发"],
                ] as const
              ).map(([key, label]) => (
                <label key={key} className="stack" style={{ gap: 4 }}>
                  <span
                    className={
                      parseLimit(limitsForm[key]) === undefined ? "field-error" : "text-faint"
                    }
                    style={{ fontSize: 11.5 }}
                  >
                    {label}
                  </span>
                  <input
                    className="input mono"
                    inputMode="numeric"
                    value={limitsForm[key]}
                    onChange={(e) =>
                      setLimitsForm((current) => ({ ...current, [key]: e.target.value }))
                    }
                    placeholder={
                      account?.limits[key] !== null && account?.limits[key] !== undefined
                        ? `账号 ${account.limits[key]}`
                        : "不限"
                    }
                  />
                </label>
              ))}
            </div>
          )}
        </Field>

        {editing && (
          <Field label="状态">
            {(id) => (
              <select
                id={id}
                className="select"
                value={enabled ? "on" : "off"}
                onChange={(e) => setEnabled(e.target.value === "on")}
              >
                <option value="on">启用</option>
                <option value="off">停用（不参与调度，粘性绑定会被清除）</option>
              </select>
            )}
          </Field>
        )}

        {account && (
          <div className="callout callout-info">
            <span>
              该目标的有效优先级为 <strong>{effectivePriority}</strong>。
              分组内所有优先级为 {effectivePriority} 的目标同属一层。
            </span>
          </div>
        )}
      </div>
    </Drawer>
  );
}
