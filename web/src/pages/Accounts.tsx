/** 上游账号：凭据、连接、倍率来源与限制。 */
import { useState } from "react";
import { api, type AccountInput } from "../lib/api";
import type {
  Account,
  Limits,
  MultiplierMode,
  Protocol,
  TestResult,
  UpstreamType,
} from "../lib/types";
import {
  MULTIPLIER_MODE_LABELS,
  PROTOCOL_LABELS,
  UPSTREAM_DEFAULTS,
  UPSTREAM_LABELS,
} from "../lib/types";
import {
  formatLimits,
  formatStaleFor,
  parseLimit,
  validateMultiplier,
} from "../lib/format";
import type { Data } from "../lib/store";
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
  Drawer,
  EmptyState,
  Field,
  Switch,
  useToast,
} from "../components/ui";
import { ModelSelectionDialog } from "../components/ModelSelectionDialog";
import { CalibrationDialog } from "../components/CalibrationDialog";
import { IconPlus, IconRefresh, IconServer, IconTrash } from "../components/Icons";

export function Accounts({
  data,
  refresh,
}: {
  data: Data;
  refresh: () => Promise<void>;
}) {
  const toast = useToast();
  const [editing, setEditing] = useState<Account | "new" | null>(null);
  const [confirm, setConfirm] = useState<Account | null>(null);
  const [selecting, setSelecting] = useState<Account | null>(null);
  const [calibrating, setCalibrating] = useState<Account | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [refreshingMultiplierId, setRefreshingMultiplierId] = useState<string | null>(null);

  const groupName = (id: string) =>
    data.groups.find((group) => group.id === id)?.name ?? id;

  const remove = async (account: Account) => {
    try {
      await api.deleteAccount(account.id);
      await refresh();
      toast.success(`账号「${account.name}」已删除`);
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "删除失败");
    }
  };

  const toggle = async (account: Account) => {
    try {
      await api.updateAccount(account.id, { enabled: !account.enabled });
      await refresh();
      toast.success(account.enabled ? "账号已停用" : "账号已启用");
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "操作失败");
    }
  };

  const refreshMultiplier = async (account: Account) => {
    if (refreshingMultiplierId === account.id) return;
    setRefreshingMultiplierId(account.id);
    try {
      const result = await api.refreshMultiplier(account.id);
      await refresh();
      toast.success(result.notice);
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "操作失败");
    } finally {
      setRefreshingMultiplierId(null);
    }
  };

  const copy = async (account: Account) => {
    setBusyId(account.id);
    try {
      const copy = await api.copyAccount(account.id);
      await refresh();
      toast.success(`已创建停用状态的「${copy.name}」，请编辑后启用`);
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "复制失败");
    } finally {
      setBusyId(null);
    }
  };

  const test = async (account: Account) => {
    setBusyId(account.id);
    try {
      const result: TestResult = await api.testAccount(account.id);
      if (result.ok) {
        toast.success(`「${account.name}」${result.message}`);
      } else {
        toast.error(`「${account.name}」${result.message}`);
      }
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "测试失败");
    } finally {
      setBusyId(null);
    }
  };

  return (
    <>
      <Card
        title="上游账号"
        description="一个账号 = 一套独立凭据。同一把 Key 要用在两个分组，请复制成两个账号。"
        actions={
          <Button
            variant="primary"
            icon={<IconPlus />}
            onClick={() => setEditing("new")}
            disabled={data.groups.length === 0}
          >
            新建账号
          </Button>
        }
      >
        {data.groups.length === 0 ? (
          <EmptyState
            icon={<IconServer size={19} />}
            title="请先创建分组"
            description="账号必须归属于某个分组——分组决定了它能被哪把下游 Key 使用。"
          />
        ) : data.accounts.length === 0 ? (
          <EmptyState
            icon={<IconServer size={19} />}
            title="还没有上游账号"
            description="填 Base URL、上游 API Key 与首选协议就行。不需要填写上下文长度、多模态、工具或思考等模型能力字段——这些由真实调用结果自动处理。"
            action={
              <Button variant="primary" icon={<IconPlus />} onClick={() => setEditing("new")}>
                新建账号
              </Button>
            }
          />
        ) : (
          <div className="table-wrap">
            <table className="data">
              <thead>
                <tr>
                  <th>账号</th>
                  <th>分组</th>
                  <th>Base URL</th>
                  <th>优先级</th>
                  <th>有效倍率</th>
                  <th>限制</th>
                  <th>状态</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {data.accounts.map((account) => (
                  <tr key={account.id}>
                    <td>
                      <div className="cell-strong">{account.name}</div>
                      <div className="text-faint" style={{ fontSize: 12 }}>
                        {UPSTREAM_LABELS[account.upstream_type]} ·{" "}
                        {PROTOCOL_LABELS[account.preferred_protocol]}
                      </div>
                    </td>
                    <td className="cell-dim">{groupName(account.group_id)}</td>
                    <td>
                      <div
                        className="mono cell-dim cell-truncate"
                        style={{ maxWidth: 180, fontSize: 12 }}
                        title={account.base_url}
                      >
                        {account.base_url}
                      </div>
                      {account.allow_private_network && <Badge tone="warn">内网</Badge>}
                    </td>
                    <td className="mono">{account.default_priority}</td>
                    <td>
                      <MultiplierCell
                        account={account}
                        onRefresh={refreshMultiplier}
                        refreshing={refreshingMultiplierId === account.id}
                      />
                    </td>
                    <td className="cell-dim" style={{ fontSize: 12 }}>
                      {formatLimits(account.limits)}
                    </td>
                    <td>
                      <button
                        className="btn btn-ghost btn-sm"
                        onClick={() => void toggle(account)}
                        title={account.enabled ? "点击停用" : "点击启用"}
                      >
                        <Badge tone={account.enabled ? "success" : "neutral"} dot>
                          {account.enabled ? "启用" : "停用"}
                        </Badge>
                      </button>
                    </td>
                    <td>
                      <div className="cell-actions">
                        <Button size="sm" onClick={() => setSelecting(account)}>
                          模型
                        </Button>
                        <Button
                          size="sm"
                          onClick={() => setCalibrating(account)}
                          title="按单模型对账反算校准系数"
                        >
                          校准
                        </Button>
                        <Button size="sm" onClick={() => setEditing(account)}>
                          编辑
                        </Button>
                        <Button
                          size="sm"
                          disabled={busyId === account.id}
                          title="一键独立复制（停用状态）"
                          onClick={() => void copy(account)}
                        >
                          复制
                        </Button>
                        <Button
                          size="sm"
                          disabled={busyId === account.id}
                          title="发送一次真实 hi 测试连接"
                          onClick={() => void test(account)}
                        >
                          测试
                        </Button>
                        <Button
                          size="sm"
                          variant="danger"
                          icon={<IconTrash size={13} />}
                          title="删除账号"
                          onClick={() => setConfirm(account)}
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

      <AccountDrawer
        key={editing === "new" ? "new" : (editing?.id ?? "closed")}
        data={data}
        account={editing === "new" ? null : editing}
        open={editing !== null}
        onClose={() => setEditing(null)}
        onSaved={async () => {
          setEditing(null);
          await refresh();
        }}
      />

      <ModelSelectionDialog
        account={selecting}
        open={selecting !== null}
        onClose={async () => {
          setSelecting(null);
          await refresh();
        }}
      />

      <CalibrationDialog
        account={calibrating}
        models={data.models.map((m) => m.name)}
        open={calibrating !== null}
        onClose={() => setCalibrating(null)}
      />

      <ConfirmDialog
        open={confirm !== null}
        title="删除账号"
        danger
        confirmLabel="删除"
        message={
          <>
            删除「{confirm?.name}」会同时移除它承担的全部调度目标。
            正在使用这些目标的逻辑模型可能因此没有可用目标。
          </>
        }
        onClose={() => setConfirm(null)}
        onConfirm={() => void remove(confirm!)}
      />
    </>
  );
}

/**
 * 有效倍率一格：数字 + 来源 + 状态。
 *
 * 宽限期内显示"已过期 N 分钟"（黄），宽限期结束显示"倍率未知"（红）——
 * 这两种状态直接决定目标能不能被调度，必须在列表里就看见（§11.4）。
 */
function MultiplierCell({
  account,
  onRefresh,
  refreshing,
}: {
  account: Account;
  onRefresh: (account: Account) => Promise<void>;
  refreshing: boolean;
}) {
  const automatic = account.multiplier_mode !== "manual";
  return (
    <div className="stack" style={{ gap: 3 }}>
      <div className="row" style={{ gap: 6 }}>
        <span className="mono cell-strong">{account.effective_multiplier}</span>
        {account.multiplier_status === "multiplier_stale" && (
          <Badge tone="warn" dot>
            已过期 {formatStaleFor(account.multiplier_stale_for ?? 0)}
          </Badge>
        )}
        {account.multiplier_status === "multiplier_unknown" && (
          <Badge tone="danger" dot>
            倍率未知，已暂停
          </Badge>
        )}
      </div>
      <div className="row" style={{ gap: 6 }}>
        <span className="text-faint" style={{ fontSize: 11.5 }}>
          {MULTIPLIER_MODE_LABELS[account.multiplier_mode]}
          {account.calibration !== "1" && ` · 校准 ×${account.calibration}`}
        </span>
        {automatic && (
          <button
            className="btn btn-ghost btn-sm"
            style={{ padding: "0 4px", height: 18 }}
            title={account.multiplier_error ?? "立即刷新倍率"}
            aria-label={`刷新「${account.name}」倍率`}
            disabled={refreshing}
            onClick={() => void onRefresh(account)}
          >
            {refreshing ? (
              <span className="spinner spinner-sm" aria-hidden="true" />
            ) : (
              <IconRefresh size={12} />
            )}
          </button>
        )}
      </div>
      {account.multiplier_error && account.multiplier_status !== "known" && (
        <div
          className="text-faint cell-truncate"
          style={{ fontSize: 11, maxWidth: 220 }}
          title={account.multiplier_error}
        >
          {account.multiplier_error}
        </div>
      )}
    </div>
  );
}

function AccountDrawer({
  data,
  account,
  open,
  onClose,
  onSaved,
}: {
  data: Data;
  account: Account | null;
  open: boolean;
  onClose: () => void;
  onSaved: () => void | Promise<void>;
}) {
  const toast = useToast();
  const editing = account !== null;

  const [form, setForm] = useState({
    group_id: account?.group_id ?? data.groups[0]?.id ?? "",
    name: account?.name ?? "",
    upstream_type: account?.upstream_type ?? ("openai_compatible" as UpstreamType),
    base_url: account?.base_url ?? "",
    api_key: "",
    preferred_protocol: account?.preferred_protocol ?? ("openai_chat" as Protocol),
    default_priority: String(account?.default_priority ?? 50),
    multiplier_mode: account?.multiplier_mode ?? ("manual" as MultiplierMode),
    manual_multiplier: account?.manual_multiplier ?? "1",
    calibration: account?.calibration ?? "1",
    new_api_token: "",
    new_api_user_id: account?.new_api_user_id ?? "",
    new_api_group: account?.new_api_group ?? "",
    rpm: account?.limits.rpm?.toString() ?? "",
    tpm: account?.limits.tpm?.toString() ?? "",
    max_concurrency: account?.limits.max_concurrency?.toString() ?? "",
    allow_private_network: account?.allow_private_network ?? false,
    auto_sync: account?.auto_sync ?? false,
  });
  const [busy, setBusy] = useState(false);

  const set = <K extends keyof typeof form>(key: K, value: (typeof form)[K]) =>
    setForm((current) => ({ ...current, [key]: value }));

  /** 换上游类型时顺带填好该类型的常见 Base URL、协议与倍率来源，减少手输。 */
  const changeUpstreamType = (type: UpstreamType) => {
    const preset = UPSTREAM_DEFAULTS[type];
    setForm((current) => ({
      ...current,
      upstream_type: type,
      base_url: current.base_url || preset.base_url,
      preferred_protocol: preset.protocol,
      multiplier_mode:
        type === "sub2api" ? "sub2api" : type === "new_api" ? "new_api" : current.multiplier_mode,
    }));
  };

  const multiplierError = validateMultiplier(form.manual_multiplier);
  const calibrationError = validateMultiplier(form.calibration);
  const priority = Number(form.default_priority);
  const priorityError =
    Number.isInteger(priority) && priority >= 0 && priority <= 100
      ? null
      : "必须是 0 到 100 之间的整数";

  const limits: Limits = {
    rpm: parseLimit(form.rpm) ?? null,
    tpm: parseLimit(form.tpm) ?? null,
    max_concurrency: parseLimit(form.max_concurrency) ?? null,
  };
  const limitErrors = {
    rpm: parseLimit(form.rpm) === undefined ? "必须是正整数，留空表示不限" : null,
    tpm: parseLimit(form.tpm) === undefined ? "必须是正整数，留空表示不限" : null,
    max_concurrency:
      parseLimit(form.max_concurrency) === undefined ? "必须是正整数，留空表示不限" : null,
  };

  const needsNewApiToken =
    form.multiplier_mode === "new_api" && !form.new_api_token.trim() && !account?.has_new_api_token;
  const needsNewApiUser = form.multiplier_mode === "new_api" && !form.new_api_user_id.trim();

  const invalid =
    !form.name.trim() ||
    !form.base_url.trim() ||
    (!editing && !form.api_key.trim()) ||
    !!multiplierError ||
    !!calibrationError ||
    !!priorityError ||
    Object.values(limitErrors).some(Boolean) ||
    needsNewApiToken ||
    needsNewApiUser;

  const submit = async () => {
    if (invalid) return;
    setBusy(true);
    try {
      const payload: AccountInput = {
        group_id: form.group_id,
        name: form.name.trim(),
        upstream_type: form.upstream_type,
        base_url: form.base_url.trim(),
        api_key: form.api_key.trim(),
        preferred_protocol: form.preferred_protocol,
        default_priority: priority,
        multiplier_mode: form.multiplier_mode,
        manual_multiplier: form.manual_multiplier.trim(),
        calibration: form.calibration.trim(),
        new_api_user_id: form.new_api_user_id.trim() || undefined,
        new_api_group: form.new_api_group.trim() || undefined,
        limits,
        allow_private_network: form.allow_private_network,
        auto_sync: form.auto_sync,
      };
      if (form.new_api_token.trim()) payload.new_api_token = form.new_api_token.trim();
      if (editing) {
        // api_key 留空表示保持原有凭据；后台不提供读取完整 Key 的接口。
        const { group_id: _group, api_key, ...rest } = payload;
        await api.updateAccount(account.id, api_key ? { ...rest, api_key } : rest);
      } else {
        await api.createAccount(payload);
      }
      toast.success(editing ? "账号已更新" : "账号已创建");
      await onSaved();
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
      title={editing ? `编辑「${account.name}」` : "新建上游账号"}
      description="不需要填写上下文长度、多模态、工具或思考等模型能力字段。"
      footer={
        <>
          <Button onClick={onClose}>取消</Button>
          <Button variant="primary" onClick={submit} disabled={busy || invalid}>
            {busy ? "保存中…" : "保存"}
          </Button>
        </>
      }
    >
      <div className="stack">
        {!editing && (
          <Field label="所属分组" hint="账号归属分组后不可迁移，改分组请新建。">
            {(id) => (
              <select
                id={id}
                className="select"
                value={form.group_id}
                onChange={(e) => set("group_id", e.target.value)}
              >
                {data.groups.map((group) => (
                  <option key={group.id} value={group.id}>
                    {group.name}
                  </option>
                ))}
              </select>
            )}
          </Field>
        )}

        <Field label="名称">
          {(id) => (
            <input
              id={id}
              className="input"
              value={form.name}
              onChange={(e) => set("name", e.target.value)}
              placeholder="账号A"
            />
          )}
        </Field>

        <div className="form-row-2">
          <Field label="上游类型">
            {(id) => (
              <select
                id={id}
                className="select"
                value={form.upstream_type}
                onChange={(e) => changeUpstreamType(e.target.value as UpstreamType)}
              >
                {Object.entries(UPSTREAM_LABELS).map(([value, label]) => (
                  <option key={value} value={value}>
                    {label}
                  </option>
                ))}
              </select>
            )}
          </Field>

          <Field label="首选协议">
            {(id) => (
              <select
                id={id}
                className="select"
                value={form.preferred_protocol}
                onChange={(e) => set("preferred_protocol", e.target.value as Protocol)}
              >
                {Object.entries(PROTOCOL_LABELS).map(([value, label]) => (
                  <option key={value} value={value}>
                    {label}
                  </option>
                ))}
              </select>
            )}
          </Field>
        </div>

        <Field
          label="Base URL"
          hint="以 /v1 结尾会被识别为已含版本段，不会拼出 /v1/v1/messages。"
        >
          {(id) => (
            <input
              id={id}
              className="input mono"
              value={form.base_url}
              onChange={(e) => set("base_url", e.target.value)}
              placeholder="https://api.example.com"
            />
          )}
        </Field>

        <Field
          label={editing ? "API Key（留空则不变）" : "API Key"}
          hint="加密保存。后台不提供读取完整 Key 的接口，只能覆盖更新。"
        >
          {(id) => (
            <input
              id={id}
              className="input mono"
              type="password"
              value={form.api_key}
              onChange={(e) => set("api_key", e.target.value)}
              placeholder={editing ? "保持原有凭据" : "sk-…"}
              autoComplete="new-password"
            />
          )}
        </Field>

        <Field
          label="默认人工优先级"
          error={priorityError ?? undefined}
          hint="0–100，越大越优先。数字相同的目标属于同一层，只有高层全部不可用时才会降到低层。"
        >
          {(id) => (
            <input
              id={id}
              className="input"
              type="number"
              min={0}
              max={100}
              value={form.default_priority}
              onChange={(e) => set("default_priority", e.target.value)}
            />
          )}
        </Field>

        <Field
          label="倍率来源"
          hint={
            form.multiplier_mode === "manual"
              ? "手动倍率始终视为已知，不受自动刷新失败影响。"
              : form.multiplier_mode === "sub2api"
                ? "调用 Key 级 /v1/sub2api/billing，用上面的 API Key 即可。刷新失败后按风险余量进入宽限期。"
                : "调用 /api/user/self/groups，需要额外的访问令牌与用户 ID——推理用的 sk-xxx 不被这个接口接受。"
          }
        >
          {(id) => (
            <select
              id={id}
              className="select"
              value={form.multiplier_mode}
              onChange={(e) => set("multiplier_mode", e.target.value as MultiplierMode)}
            >
              {Object.entries(MULTIPLIER_MODE_LABELS).map(([value, label]) => (
                <option key={value} value={value}>
                  {label}
                </option>
              ))}
            </select>
          )}
        </Field>

        {form.multiplier_mode === "new_api" && (
          <>
            <div className="form-row-2">
              <Field
                label={
                  account?.has_new_api_token ? "访问令牌（留空则不变）" : "访问令牌"
                }
                error={needsNewApiToken ? "New API 自动倍率需要访问令牌" : undefined}
                hint="在 New API 的个人设置页生成，与 API Key 是两把不同的凭据。"
              >
                {(id) => (
                  <input
                    id={id}
                    className="input mono"
                    type="password"
                    value={form.new_api_token}
                    onChange={(e) => set("new_api_token", e.target.value)}
                    placeholder={account?.has_new_api_token ? "已保存" : ""}
                    autoComplete="new-password"
                  />
                )}
              </Field>
              <Field
                label="用户 ID"
                error={needsNewApiUser ? "New-Api-User 请求头必填" : undefined}
              >
                {(id) => (
                  <input
                    id={id}
                    className="input mono"
                    value={form.new_api_user_id}
                    onChange={(e) => set("new_api_user_id", e.target.value)}
                    placeholder="42"
                  />
                )}
              </Field>
            </div>
            <Field
              label="分组名（可选）"
              hint="这把 Key 在 New API 上所属的分组。留空时取可用分组中的最高倍率——把成本估高才是安全方向。"
            >
              {(id) => (
                <input
                  id={id}
                  className="input mono"
                  value={form.new_api_group}
                  onChange={(e) => set("new_api_group", e.target.value)}
                  placeholder="default"
                />
              )}
            </Field>
          </>
        )}

        <div className="form-row-2">
          <Field
            label={form.multiplier_mode === "manual" ? "上游倍率" : "初始倍率"}
            error={multiplierError ?? undefined}
            hint={
              form.multiplier_mode === "manual"
                ? undefined
                : "首次自动刷新成功前先用这个值顶着，同时立即开始计宽限期。"
            }
          >
            {(id) => (
              <input
                id={id}
                className="input mono"
                value={form.manual_multiplier}
                onChange={(e) => set("manual_multiplier", e.target.value)}
              />
            )}
          </Field>
          <Field label="校准系数" error={calibrationError ?? undefined}>
            {(id) => (
              <input
                id={id}
                className="input mono"
                value={form.calibration}
                onChange={(e) => set("calibration", e.target.value)}
              />
            )}
          </Field>
        </div>
        <p className="field-hint" style={{ marginTop: -8 }}>
          有效倍率 = 上游倍率 × 校准系数，必须不高于分组上限。校准系数用来编码
          「站 A 的 x1 大约相当于站 B 的 x0.7」这类跨站点差异。
        </p>

        <Field
          label="限制（账号默认值）"
          hint="留空表示不限。最大并发与额度由同一把 Key 下的所有模型共享；调度目标可以逐项覆盖得更严。TPM 按请求体保守估算，是「估算限流」。"
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
                    className={limitErrors[key] ? "field-error" : "text-faint"}
                    style={{ fontSize: 11.5 }}
                  >
                    {label}
                  </span>
                  <input
                    className="input mono"
                    inputMode="numeric"
                    value={form[key]}
                    onChange={(e) => set(key, e.target.value)}
                    placeholder="不限"
                  />
                </label>
              ))}
            </div>
          )}
        </Field>

        <Switch
          checked={form.allow_private_network}
          onChange={(value) => set("allow_private_network", value)}
          label="允许访问内网地址"
          hint="默认阻止环回、内网与云元数据地址。上游确实部署在局域网时才打开。"
        />

        <Switch
          checked={form.auto_sync}
          onChange={(value) => set("auto_sync", value)}
          label="模型自动同步"
          hint={
            form.auto_sync
              ? "已托管：上游全部模型进入调度，选择集被忽略。关闭后回到之前勾选的模型。"
              : "开启后忽略模型选择集，按设置页的同步间隔（默认 30 分钟）全量托管上游模型。"
          }
        />
        {editing && form.auto_sync && account?.model_synced_at != null && (
          <p className="field-hint" style={{ marginTop: -6 }}>
            上次同步：{new Date(account.model_synced_at * 1000).toLocaleString()}
          </p>
        )}
      </div>
    </Drawer>
  );
}
