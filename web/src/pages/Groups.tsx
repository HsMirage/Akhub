/** 分组：调度硬边界，签发下游 Key。 */
import { useState } from "react";
import { api } from "../lib/api";
import type { Group, GroupAlert, KeyReveal, SchedulingWeights } from "../lib/types";
import { validateMultiplier } from "../lib/format";
import type { Data } from "../lib/store";
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
  CopyButton,
  Drawer,
  EmptyState,
  Field,
  Modal,
  useToast,
} from "../components/ui";
import { IconKey, IconPlus, IconRefresh, IconTrash } from "../components/Icons";

/**
 * 分组行内的告警（§6.3）。
 *
 * 只显示这个分组自己的问题：账号硬停、倍率过期/未知、目标不可用、模型列表为空。
 * 没有告警时给一个明确的"正常"，避免留白让人以为没加载出来。
 */
function GroupAlerts({ alerts }: { alerts: GroupAlert[] }) {
  if (alerts.length === 0) {
    return (
      <Badge tone="success" dot>
        正常
      </Badge>
    );
  }
  // 最严重的排前面：一行里能看到的内容有限，先让人看到"新请求会失败"的那种。
  const sorted = [...alerts].sort((a, b) =>
    a.level === b.level ? 0 : a.level === "danger" ? -1 : 1,
  );
  return (
    <div className="group-alerts">
      {sorted.map((alert, index) => (
        <Badge key={index} tone={alert.level === "danger" ? "danger" : "warn"} dot>
          {alert.text}
        </Badge>
      ))}
    </div>
  );
}

export function Groups({ data, refresh }: { data: Data; refresh: () => Promise<void> }) {
  const toast = useToast();
  const [editing, setEditing] = useState<Group | "new" | null>(null);
  const [reveal, setReveal] = useState<KeyReveal | null>(null);
  const [confirm, setConfirm] = useState<
    { kind: "delete" | "regenerate"; group: Group } | null
  >(null);

  const run = async (action: () => Promise<void>, success: string) => {
    try {
      await action();
      await refresh();
      toast.success(success);
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "操作失败");
    }
  };

  return (
    <>
      <Card
        title="分组"
        description="分组之间不共享账号、Key、状态与调度。跨分组调度不存在。"
        actions={
          <Button variant="primary" icon={<IconPlus />} onClick={() => setEditing("new")}>
            新建分组
          </Button>
        }
      >
        {data.groups.length === 0 ? (
          <EmptyState
            icon={<IconKey size={19} />}
            title="还没有分组"
            description="分组是一切配置的起点：它持有下游 API Key、倍率上限与调度权重。一个人用一个分组就够。"
            action={
              <Button variant="primary" icon={<IconPlus />} onClick={() => setEditing("new")}>
                新建分组
              </Button>
            }
          />
        ) : (
          <div className="table-wrap">
            <table className="data">
              <thead>
                <tr>
                  <th>名称</th>
                  <th>下游 Key</th>
                  <th>倍率上限</th>
                  <th>调度权重</th>
                  <th>队列容量</th>
                  <th>最长等待</th>
                  <th>降级</th>
                  <th>模型 / 目标</th>
                  <th>告警</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {data.groups.map((group) => (
                  <tr key={group.id}>
                    <td className="cell-strong">{group.name}</td>
                    <td className="mono cell-dim">{group.key_prefix}…</td>
                    <td className="mono">{group.multiplier_limit}</td>
                    <td className="cell-dim mono" title="倍率 / 可靠性 / 首字延迟 / 输出速度">
                      {group.weights.multiplier}·{group.weights.reliability}·
                      {group.weights.first_token}·{group.weights.throughput}
                    </td>
                    <td className="cell-dim">{group.queue_capacity}</td>
                    <td className="mono tabular">
                      {group.max_wait_secs === 0 ? "跟随总超时" : `${group.max_wait_secs}s`}
                    </td>
                    <td>
                      <Badge tone={group.allow_degrade ? "neutral" : "warn"}>
                        {group.allow_degrade ? "允许" : "禁止"}
                      </Badge>
                    </td>
                    <td>
                      <Badge tone={group.dispatch_targets > 0 ? "success" : "neutral"}>
                        {group.logical_models} / {group.dispatch_targets}
                      </Badge>
                    </td>
                    <td>
                      <GroupAlerts alerts={group.alerts} />
                    </td>
                    <td>
                      <div className="cell-actions">
                        <Button size="sm" onClick={() => setEditing(group)}>
                          编辑
                        </Button>
                        <Button
                          size="sm"
                          icon={<IconRefresh size={13} />}
                          title="重新生成下游 Key"
                          onClick={() => setConfirm({ kind: "regenerate", group })}
                        />
                        <Button
                          size="sm"
                          variant="danger"
                          icon={<IconTrash size={13} />}
                          title="删除分组"
                          onClick={() => setConfirm({ kind: "delete", group })}
                        />
                      </div>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>

      <GroupDrawer
        key={editing === "new" ? "new" : (editing?.id ?? "closed")}
        group={editing === "new" ? null : editing}
        open={editing !== null}
        onClose={() => setEditing(null)}
        onSaved={async (created) => {
          setEditing(null);
          if (created) setReveal(created);
          await refresh();
        }}
      />

      <KeyRevealModal reveal={reveal} onClose={() => setReveal(null)} />

      <ConfirmDialog
        open={confirm?.kind === "delete"}
        title="删除分组"
        danger
        confirmLabel="删除"
        message={
          <>
            删除「{confirm?.group.name}」会连带删除它的全部账号、逻辑模型与调度目标，
            并使其下游 Key 立即失效。此操作不可撤销。
          </>
        }
        onClose={() => setConfirm(null)}
        onConfirm={() =>
          void run(
            () => api.deleteGroup(confirm!.group.id),
            `分组「${confirm!.group.name}」已删除`,
          )
        }
      />

      <ConfirmDialog
        open={confirm?.kind === "regenerate"}
        title="重新生成下游 Key"
        confirmLabel="重新生成"
        message={
          <>
            「{confirm?.group.name}」的旧 Key 会在新 Key 生成的同一刻失效，
            正在使用它的客户端会立即收到 401。
          </>
        }
        onClose={() => setConfirm(null)}
        onConfirm={() =>
          void (async () => {
            try {
              const result = await api.regenerateKey(confirm!.group.id);
              await refresh();
              setReveal(result);
            } catch (cause) {
              toast.error(cause instanceof Error ? cause.message : "操作失败");
            }
          })()
        }
      />
    </>
  );
}

const WEIGHT_FIELDS: { key: keyof SchedulingWeights; label: string }[] = [
  { key: "multiplier", label: "倍率" },
  { key: "reliability", label: "可靠性" },
  { key: "first_token", label: "首字延迟" },
  { key: "throughput", label: "输出速度" },
];

const DEFAULT_WEIGHTS: SchedulingWeights = {
  multiplier: 40,
  reliability: 25,
  first_token: 20,
  throughput: 15,
};

function GroupDrawer({
  group,
  open,
  onClose,
  onSaved,
}: {
  group: Group | null;
  open: boolean;
  onClose: () => void;
  onSaved: (reveal: KeyReveal | null) => void | Promise<void>;
}) {
  const toast = useToast();
  const editing = group !== null;
  const [name, setName] = useState(group?.name ?? "");
  const [limit, setLimit] = useState(group?.multiplier_limit ?? "1");
  const [queue, setQueue] = useState(String(group?.queue_capacity ?? 100));
  const [maxWait, setMaxWait] = useState(String(group?.max_wait_secs ?? 60));
  const [allowDegrade, setAllowDegrade] = useState(group?.allow_degrade ?? true);
  const [weights, setWeights] = useState<Record<keyof SchedulingWeights, string>>({
    multiplier: String(group?.weights.multiplier ?? DEFAULT_WEIGHTS.multiplier),
    reliability: String(group?.weights.reliability ?? DEFAULT_WEIGHTS.reliability),
    first_token: String(group?.weights.first_token ?? DEFAULT_WEIGHTS.first_token),
    throughput: String(group?.weights.throughput ?? DEFAULT_WEIGHTS.throughput),
  });
  const [busy, setBusy] = useState(false);

  const limitError = validateMultiplier(limit);
  const parsedWeights = WEIGHT_FIELDS.map(({ key }) => Number(weights[key]));
  const weightsValid = parsedWeights.every((w) => Number.isInteger(w) && w >= 0);
  const weightSum = parsedWeights.reduce((sum, w) => sum + (Number.isFinite(w) ? w : 0), 0);
  const weightsError = !weightsValid
    ? "每一项都必须是非负整数"
    : weightSum !== 100
      ? `四项之和必须为 100，当前 ${weightSum}`
      : null;
  const queueValue = Number(queue);
  const queueError =
    Number.isInteger(queueValue) && queueValue >= 0 ? null : "必须是非负整数";
  const maxWaitValue = Number(maxWait);
  const maxWaitError =
    Number.isInteger(maxWaitValue) && maxWaitValue >= 0 && maxWaitValue <= 3600
      ? null
      : "必须是 0 到 3600 之间的整数";

  const invalid =
    !name.trim() || !!limitError || !!weightsError || !!queueError || !!maxWaitError;

  const submit = async () => {
    if (invalid) return;
    setBusy(true);
    try {
      const payload = {
        name: name.trim(),
        multiplier_limit: limit.trim(),
        queue_capacity: queueValue,
        max_wait_secs: maxWaitValue,
        allow_degrade: allowDegrade,
        weights: {
          multiplier: Number(weights.multiplier),
          reliability: Number(weights.reliability),
          first_token: Number(weights.first_token),
          throughput: Number(weights.throughput),
        },
      };
      if (editing) {
        await api.updateGroup(group.id, payload);
        toast.success("分组已更新");
        await onSaved(null);
      } else {
        const created = await api.createGroup(payload);
        await onSaved(created);
      }
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "保存失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Drawer
      open={open}
      onClose={onClose}
      title={editing ? `编辑「${group.name}」` : "新建分组"}
      description={
        editing
          ? "修改会在保存的一刻对新请求生效，在途请求沿用旧配置。"
          : "创建后会签发一把下游 Key，且只完整显示一次。"
      }
      footer={
        <>
          <Button onClick={onClose}>取消</Button>
          <Button variant="primary" onClick={submit} disabled={busy || invalid}>
            {busy ? "保存中…" : editing ? "保存" : "创建"}
          </Button>
        </>
      }
    >
      <div className="stack">
        <Field label="名称" hint="例如「主力」「备用」。同一实例内不可重名。">
          {(id) => (
            <input
              id={id}
              className="input"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="主力"
            />
          )}
        </Field>

        <Field
          label="分组倍率上限"
          error={limitError ?? undefined}
          hint="绝对红线：有效倍率高于它的目标会被暂停，等于它仍然允许调用。想「便宜优先」请用优先级表达，不要压低这个值。"
        >
          {(id) => (
            <input
              id={id}
              className="input mono"
              value={limit}
              onChange={(e) => setLimit(e.target.value)}
              placeholder="1"
            />
          )}
        </Field>

        <Field
          label="调度权重"
          error={weightsError ?? undefined}
          hint="只决定同一层内怎么分流量，永远不会让低优先级层越过高优先级层。"
        >
          {() => (
            <div>
              <div className="weights">
                {WEIGHT_FIELDS.map(({ key, label }) => (
                  <label key={key} className="stack" style={{ gap: 4 }}>
                    <span className="text-faint" style={{ fontSize: 11.5 }}>
                      {label}
                    </span>
                    <input
                      className="input mono"
                      type="number"
                      min={0}
                      max={100}
                      value={weights[key]}
                      onChange={(e) =>
                        setWeights((current) => ({ ...current, [key]: e.target.value }))
                      }
                    />
                  </label>
                ))}
              </div>
              <div className={`weights-sum ${weightSum === 100 ? "" : "is-invalid"}`}>
                合计 {weightSum} / 100 · 默认 40 · 25 · 20 · 15
              </div>
            </div>
          )}
        </Field>

        <Field
          label="队列总容量"
          error={queueError ?? undefined}
          hint="所有目标队列加起来的上限，超出返回 429 queue_full。设为 0 表示任何需要等待的请求都直接拒绝。"
        >
          {(id) => (
            <input
              id={id}
              className="input"
              type="number"
              min={0}
              value={queue}
              onChange={(e) => setQueue(e.target.value)}
            />
          )}
        </Field>

        <Field
          label="队列最长等待（秒）"
          error={maxWaitError ?? undefined}
          hint="层内目标全忙时最多等这么久，超时返回可重试的 429；填 0 表示跟随请求总超时。"
        >
          {(id) => (
            <input
              id={id}
              className="input mono"
              type="number"
              min={0}
              max={3600}
              value={maxWait}
              onChange={(e) => setMaxWait(e.target.value)}
            />
          )}
        </Field>

        <Field
          label="能力降级"
          hint="只对白名单内的能力生效，且只在故障切换时发生：thinking 块与协议独有采样参数。工具、图片与结构化输出永远不会被丢弃，表达不了就直接报错。"
        >
          {(id) => (
            <select
              id={id}
              className="select"
              value={allowDegrade ? "on" : "off"}
              onChange={(e) => setAllowDegrade(e.target.value === "on")}
            >
              <option value="on">允许（发生时返回 X-Akhub-Degraded 并标红记录）</option>
              <option value="off">禁止（宁可失败，也不丢任何能力）</option>
            </select>
          )}
        </Field>
      </div>
    </Drawer>
  );
}

/** Key 只出现这一次，所以要给足视觉重量与复制入口。 */
function KeyRevealModal({
  reveal,
  onClose,
}: {
  reveal: KeyReveal | null;
  onClose: () => void;
}) {
  return (
    <Modal
      open={reveal !== null}
      onClose={onClose}
      title="保存下游 Key"
      footer={
        <Button variant="primary" onClick={onClose}>
          我已保存
        </Button>
      }
    >
      <div className="stack">
        <div className="callout callout-warn">
          <span>
            这是唯一一次显示完整 Key。关闭后服务端只保留它的 HMAC 摘要与前缀，
            无法再取回明文；丢失只能重新生成。
          </span>
        </div>
        <div>
          <div className="field-label" style={{ marginBottom: 6 }}>
            分组「{reveal?.group.name}」
          </div>
          <div className="key-reveal">
            <span style={{ flex: 1 }}>{reveal?.key}</span>
            {reveal && <CopyButton value={reveal.key} />}
          </div>
        </div>
      </div>
    </Modal>
  );
}
