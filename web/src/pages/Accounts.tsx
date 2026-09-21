/** 上游账号：凭据、连接、倍率来源与限制。 */
import { useEffect, useMemo, useState } from "react";
import { api, type AccountInput } from "../lib/api";
import type {
  Account,
  AccountHealth,
  BatchMultiplierRefreshResult,
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
  formatRelative,
  formatStaleFor,
  parseLimit,
  validateMultiplier,
} from "../lib/format";
import { useRouteParams } from "../lib/store";
import type { Data } from "../lib/store";
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
  Drawer,
  EmptyState,
  Field,
  FormSection,
  InfoTip,
  Menu,
  Modal,
  Switch,
  useToast,
} from "../components/ui";
import { ModelSelectionDialog } from "../components/ModelSelectionDialog";
import {
  KeyPoolEditor,
  draftsFromKeys,
  draftsToInputs,
  emptyDraft,
  validateDrafts,
  type KeyDraft,
} from "../components/KeyPoolEditor";
import { CalibrationDialog } from "../components/CalibrationDialog";
import { IconPlus, IconRefresh, IconSearch, IconServer } from "../components/Icons";

type AccountStatusFilter = "all" | "enabled" | "disabled";
type AccountUpstreamFilter = "all" | UpstreamType;

export function Accounts({
  data,
  refresh,
}: {
  data: Data;
  refresh: () => Promise<unknown>;
}) {
  const toast = useToast();
  const [editing, setEditing] = useState<Account | "new" | null>(null);
  const [confirm, setConfirm] = useState<Account | null>(null);
  /** 新建成功后询问是否立刻测试连接，避免"保存→回列表→找按钮"的来回。 */
  const [createdAccount, setCreatedAccount] = useState<Account | null>(null);
  // 从请求记录跳过来时带着 account=<id>：定位并高亮该账号（§6.6）。
  const params = useRouteParams();
  const highlightId = params.get("account");
  const [highlight, setHighlight] = useState<string | null>(highlightId);

  useEffect(() => {
    if (!highlightId) {
      setHighlight(null);
      return;
    }
    if (!data.accounts.some((account) => account.id === highlightId)) {
      toast.error("这条请求记录里的账号已经不存在了");
      setHighlight(null);
      return;
    }
    setHighlight(highlightId);
    const scrollTimer = window.setTimeout(() => {
      document
        .querySelector(`tr[data-account-id="${highlightId}"]`)
        ?.scrollIntoView({ block: "center", behavior: "smooth" });
    }, 80);
    const clearTimer = window.setTimeout(() => setHighlight(null), 2600);
    return () => {
      window.clearTimeout(scrollTimer);
      window.clearTimeout(clearTimer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [highlightId, data.accounts.length]);
  const [selecting, setSelecting] = useState<Account | null>(null);
  /** 已处理过的 `manage=1&account=...` 深链，避免关闭弹窗后被 effect 再次打开。 */
  const [manageHandled, setManageHandled] = useState<string | null>(null);

  useEffect(() => {
    if (!params.get("manage")) return;
    const accountId = params.get("account");
    if (!accountId || accountId === manageHandled) return;
    const target = data.accounts.find((account) => account.id === accountId);
    if (!target) return;
    setManageHandled(accountId);
    setSelecting(target);
  }, [params, data.accounts, manageHandled]);

  // 全局数据刷新后同步弹窗里的账号对象，避免刚保存的"隐藏原始模型"等
  // 开关在重新打开弹窗时又退回旧值。
  useEffect(() => {
    setSelecting((current) =>
      current
        ? (data.accounts.find((account) => account.id === current.id) ?? current)
        : null,
    );
  }, [data.accounts]);
  const [calibrating, setCalibrating] = useState<Account | null>(null);
  const [busyId, setBusyId] = useState<string | null>(null);
  const [refreshingMultiplierId, setRefreshingMultiplierId] = useState<string | null>(null);
  /** 批量刷新倍率进行中（§11.3）。 */
  const [refreshingAll, setRefreshingAll] = useState(false);
  const [syncingModelId, setSyncingModelId] = useState<string | null>(null);
  const [groupFilter, setGroupFilter] = useState("all");
  const [statusFilter, setStatusFilter] = useState<AccountStatusFilter>("all");
  const [upstreamFilter, setUpstreamFilter] = useState<AccountUpstreamFilter>("all");
  const [search, setSearch] = useState("");
  /** 批量刷新倍率的结果面板；null 表示未展示。 */
  const [refreshReport, setRefreshReport] = useState<BatchMultiplierRefreshResult | null>(
    null,
  );

  const upstreamTypes = useMemo(
    () =>
      Array.from(new Set(data.accounts.map((account) => account.upstream_type))).sort(
        (left, right) =>
          UPSTREAM_LABELS[left].localeCompare(UPSTREAM_LABELS[right], "zh-CN"),
      ),
    [data.accounts],
  );

  const filteredAccounts = useMemo(
    () =>
      data.accounts.filter((account) => {
        const needle = search.trim().toLowerCase();
        if (needle) {
          const haystack = `${account.name} ${account.base_url}`.toLowerCase();
          if (!haystack.includes(needle)) return false;
        }
        if (groupFilter !== "all" && account.group_id !== groupFilter) return false;
        if (statusFilter === "enabled" && !account.enabled) return false;
        if (statusFilter === "disabled" && account.enabled) return false;
        if (upstreamFilter !== "all" && account.upstream_type !== upstreamFilter) return false;
        return true;
      }),
    [data.accounts, groupFilter, statusFilter, upstreamFilter, search],
  );

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

  const refreshAllMultipliers = async () => {
    if (refreshingAll) return;
    setRefreshingAll(true);
    try {
      const result = await api.refreshAllMultipliers();
      await refresh();
      setRefreshReport(result);
      if (result.failed > 0) {
        // 部分失败要如实说清楚是哪几个，不能只报"完成"（§11.3）。
        const names = result.errors.map((item) => item.name).join("、");
        toast.error(`${result.refreshed}/${result.total} 成功；失败：${names}`);
      } else {
        toast.success(result.notice);
      }
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "批量刷新失败");
    } finally {
      setRefreshingAll(false);
    }
  };

  const syncModels = async (account: Account) => {
    if (syncingModelId === account.id) return;
    setSyncingModelId(account.id);
    try {
      const count = account.auto_sync
        ? (await api.syncAccountModels(account.id)).managed_models
        : (await api.refreshAccountModels(account.id)).length;
      await refresh();
      toast.success(`已同步 ${count} 个模型`);
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "同步模型失败");
    } finally {
      setSyncingModelId(null);
    }
  };

  const copy = async (account: Account) => {
    setBusyId(account.id);
    try {
      const created = await api.copyAccount(account.id);
      await refresh();
      setEditing(created);
      toast.success(`已创建停用状态的「${created.name}」，请修改后启用`);
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
        description="一个账号 = 一套独立凭据。同一把 Key 要同时用在两个分组，请复制成两个账号；只是换归属，在编辑里改「所属分组」，模型会一起迁过去。"
        actions={
          <div className="row" style={{ gap: 8 }}>
            {/* 批量刷新：一次探测所有自动倍率账号（§11.3）。 */}
            <Button
              icon={<IconRefresh size={13} />}
              onClick={() => void refreshAllMultipliers()}
              disabled={refreshingAll || data.accounts.length === 0}
              title="重新探测所有自动倍率账号"
            >
              {refreshingAll ? "刷新中…" : "刷新全部倍率"}
            </Button>
            <Button
              variant="primary"
              icon={<IconPlus />}
              onClick={() => setEditing("new")}
              disabled={data.groups.length === 0}
            >
              新建账号
            </Button>
          </div>
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
          <>
            <div className="table-filters account-filters">
              <div className="table-filter-fields account-filters">
              <Field label="搜索">
                {(id) => (
                  <div className="input-with-icon">
                    <IconSearch size={14} />
                    <input
                      id={id}
                      className="input"
                      value={search}
                        placeholder="按名称或 Base URL 搜索"
                        onChange={(event) => setSearch(event.target.value)}
                      />
                    </div>
                  )}
                </Field>
                <Field label="分组">
                  {(id) => (
                    <select
                      id={id}
                      className="select"
                      value={groupFilter}
                      onChange={(event) => setGroupFilter(event.target.value)}
                    >
                      <option value="all">全部分组</option>
                      {data.groups.map((group) => (
                        <option key={group.id} value={group.id}>
                          {group.name}
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
                      value={statusFilter}
                      onChange={(event) =>
                        setStatusFilter(event.target.value as AccountStatusFilter)
                      }
                    >
                      <option value="all">全部</option>
                      <option value="enabled">已启用</option>
                      <option value="disabled">已停用</option>
                    </select>
                  )}
                </Field>
                <Field label="上游类型">
                  {(id) => (
                    <select
                      id={id}
                      className="select"
                      value={upstreamFilter}
                      onChange={(event) =>
                        setUpstreamFilter(event.target.value as AccountUpstreamFilter)
                      }
                    >
                      <option value="all">全部类型</option>
                      {upstreamTypes.map((upstreamType) => (
                        <option key={upstreamType} value={upstreamType}>
                          {UPSTREAM_LABELS[upstreamType]}
                        </option>
                      ))}
                    </select>
                  )}
                </Field>
              </div>
              <div className="table-filter-summary tabular">
                共 {data.accounts.length} 个账号（筛选后 {filteredAccounts.length} 个）
              </div>
              <Button
                size="sm"
                variant="ghost"
                disabled={
                  !search &&
                  groupFilter === "all" &&
                  statusFilter === "all" &&
                  upstreamFilter === "all"
                }
                onClick={() => {
                  setSearch("");
                  setGroupFilter("all");
                  setStatusFilter("all");
                  setUpstreamFilter("all");
                }}
              >
                清除筛选
              </Button>
            </div>
            <div className="model-sync-meta">
              <span>上次同步：</span>
              <strong>
                {(() => {
                  const latest = filteredAccounts.reduce<number | null>(
                    (current, account) =>
                      account.model_synced_at !== null &&
                      (current === null || account.model_synced_at > current)
                        ? account.model_synced_at
                        : current,
                    null,
                  );
                  return latest === null ? "从未" : formatRelative(latest);
                })()}
              </strong>
              <span className="text-faint">模型目录可手动更新，也可由自动同步托管</span>
            </div>
            <div className="table-wrap">
              <table className="data">
                <thead>
                  <tr>
                    <th>账号</th>
                    <th>Key</th>
                    <th>分组</th>
                    <th>Base URL</th>
                    <th>优先级</th>
                    <th>有效倍率</th>
                    <th>限制</th>
                    <th>健康</th>
                    <th>状态</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  {filteredAccounts.length === 0 ? (
                    <tr>
                      <td colSpan={9} className="table-empty-cell">
                        没有符合条件的账号
                      </td>
                    </tr>
                  ) : (
                    filteredAccounts.map((account) => (
                      <tr
                        key={account.id}
                        data-account-id={account.id}
                        className={highlight === account.id ? "is-highlighted" : undefined}
                      >
                        <td>
                          <div className="cell-strong">{account.name}</div>
                          <div className="text-faint" style={{ fontSize: 12 }}>
                            {UPSTREAM_LABELS[account.upstream_type]} ·{" "}
                            {PROTOCOL_LABELS[account.preferred_protocol]}
                          </div>
                        </td>
                        <td>
                          <KeyCountCell account={account} />
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
                          <AccountHealthBadge health={account.health} />
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
                            <Button
                              size="sm"
                              variant="primary"
                              onClick={() => setSelecting(account)}
                            >
                              模型管理
                            </Button>
                            <Button size="sm" onClick={() => setEditing(account)}>
                              编辑
                            </Button>
                            <Menu
                              label={`更多操作：${account.name}`}
                              items={[
                                ...(account.auto_sync
                                  ? [
                                      {
                                        label:
                                          syncingModelId === account.id
                                            ? "同步中…"
                                            : "立即同步模型",
                                        disabled: syncingModelId === account.id,
                                        onSelect: () => void syncModels(account),
                                      },
                                    ]
                                  : []),
                                {
                                  label: "校准倍率",
                                  hint: "按单模型对账反算校准系数",
                                  onSelect: () => setCalibrating(account),
                                },
                                {
                                  label: "复制账号",
                                  hint: "生成一个停用状态的副本",
                                  disabled: busyId === account.id,
                                  onSelect: () => void copy(account),
                                },
                                {
                                  label: "测试连接",
                                  hint: "发送一次真实 hi 测试连接",
                                  disabled: busyId === account.id,
                                  onSelect: () => void test(account),
                                },
                                {
                                  label: "删除账号",
                                  danger: true,
                                  onSelect: () => setConfirm(account),
                                },
                              ]}
                            />
                          </div>
                        </td>
                      </tr>
                    ))
                  )}
                </tbody>
              </table>
            </div>
          </>
        )}
      </Card>

      <AccountDrawer
        key={editing === "new" ? "new" : (editing?.id ?? "closed")}
        data={data}
        account={editing === "new" ? null : editing}
        open={editing !== null}
        onClose={() => setEditing(null)}
        onSaved={async (created) => {
          setEditing(null);
          if (created) setCreatedAccount(created);
          await refresh();
        }}
      />

      <ModelSelectionDialog
        account={selecting}
        data={data}
        open={selecting !== null}
        onClose={() => setSelecting(null)}
        onChanged={refresh}
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
        requireText={confirm?.name}
        requireLabel={`请输入账号名「${confirm?.name ?? ""}」以确认删除`}
        message={
          <>
            删除「{confirm?.name}」会同时移除它承担的全部调度目标。
            正在使用这些目标的逻辑模型可能因此没有可用目标。
          </>
        }
        onClose={() => setConfirm(null)}
        onConfirm={() => void remove(confirm!)}
      />

      <Modal
        open={createdAccount !== null}
        onClose={() => setCreatedAccount(null)}
        title="账号已创建"
        footer={
          <>
            <Button onClick={() => setCreatedAccount(null)}>稍后再说</Button>
            <Button
              variant="primary"
              onClick={() => {
                const target = createdAccount;
                setCreatedAccount(null);
                if (target) void test(target);
              }}
            >
              立即测试连接
            </Button>
          </>
        }
      >
        <div className="stack" style={{ gap: 10 }}>
          <p style={{ margin: 0 }}>
            账号「{createdAccount?.name}」已保存。现在发送一次真实
            <code> hi </code>测试请求验证连接吗？
          </p>
          <p className="field-hint" style={{ margin: 0 }}>
            测试不会参与统计，也不会影响该账号的调度状态；失败时会给出具体错误原因。
          </p>
        </div>
      </Modal>

      <Modal
        open={refreshReport !== null}
        onClose={() => setRefreshReport(null)}
        title="批量刷新倍率结果"
        footer={
          <Button variant="primary" onClick={() => setRefreshReport(null)}>
            知道了
          </Button>
        }
      >
        <div className="stack" style={{ gap: 12 }}>
          <p style={{ margin: 0 }}>
            共 {refreshReport?.total ?? 0} 个自动倍率账号，成功{" "}
            {refreshReport?.refreshed ?? 0} 个，失败 {refreshReport?.failed ?? 0} 个。
          </p>
          {refreshReport && refreshReport.errors.length > 0 ? (
            <div className="table-wrap" style={{ maxHeight: 280, overflowY: "auto" }}>
              <table className="data">
                <thead>
                  <tr>
                    <th>账号</th>
                    <th>失败原因</th>
                  </tr>
                </thead>
                <tbody>
                  {refreshReport.errors.map((item) => (
                    <tr key={item.name}>
                      <td className="cell-strong">{item.name}</td>
                      <td className="cell-dim">{item.error}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          ) : (
            <div className="callout callout-info">全部账号刷新成功。</div>
          )}
        </div>
      </Modal>
    </>
  );
}

/**
 * 账号级健康徽标（§6.9）。
 *
 * 账号的熔断与额度是整账号范围的（同一把 Key 下的所有模型共享，§12.1），所以
 * 必须在这个列表里就能看见——否则只能逐个点进目标页猜，而"这个号还能不能用"
 * 正是翻这个列表时最想知道的事。
 */
/**
 * Key 池一格（§4.2.1）："3 把 / 1 把异常"。
 *
 * 多 Key 之后"这个号还有没有能用的凭据"和"这个号整体正不正常"是两个问题，
 * 所以 Key 数量单独占一格，异常时直接标出来。
 */
function KeyCountCell({ account }: { account: Account }) {
  const total = account.health.key_total ?? account.keys?.length ?? 0;
  const enabled = account.health.key_enabled ?? total;
  const unhealthy = (account.keys ?? []).filter(
    (key) => key.enabled && key.health.status !== "active",
  ).length;

  if (total === 0) {
    return <Badge tone="danger">未配置</Badge>;
  }
  return (
    <div title={unhealthy > 0 ? `${unhealthy} 把 Key 当前不可用` : undefined}>
      <span className="mono">
        {enabled}/{total}
      </span>
      {unhealthy > 0 && (
        <div className="text-faint" style={{ fontSize: 11, marginTop: 2 }}>
          <Badge tone="danger">{unhealthy} 把异常</Badge>
        </div>
      )}
    </div>
  );
}

function AccountHealthBadge({ health }: { health: AccountHealth }) {
  const labels: Record<string, string> = {
    active: "正常",
    cooldown: "冷却中",
    half_open: "半开试运行",
    quota_exhausted: "额度耗尽",
    key_invalid: "Key 失效",
    no_key: "没有可用的 Key",
    disabled: "已停用",
  };
  const tones: Record<string, "success" | "warn" | "danger" | "neutral"> = {
    active: "success",
    cooldown: "warn",
    half_open: "warn",
    quota_exhausted: "danger",
    key_invalid: "danger",
    no_key: "danger",
    disabled: "neutral",
  };
  const tone = tones[health.status] ?? "neutral";
  const label = labels[health.status] ?? health.status;
  // 不可用目标的计数只在有问题的显示，正常时不占版面。
  const unhealthy = Object.entries(health.targets)
    .filter(([status]) => status !== "active")
    .reduce((sum, [, count]) => sum + count, 0);

  return (
    <div title={health.reason ?? undefined}>
      <Badge tone={tone} dot>
        {label}
      </Badge>
      {health.target_total > 0 && unhealthy > 0 && (
        <div className="text-faint" style={{ fontSize: 11, marginTop: 2 }}>
          {health.target_total - unhealthy}/{health.target_total} 目标可用
        </div>
      )}
      {health.target_total === 0 && (
        <div className="text-faint" style={{ fontSize: 11, marginTop: 2 }}>
          无目标
        </div>
      )}
    </div>
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
            // 触达区域不小于 24×24：低于这个尺寸在触屏与高分屏上都难点中。
            style={{ padding: "0 6px", minHeight: 26, minWidth: 26 }}
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
  onSaved: (created?: Account) => void | Promise<void>;
}) {
  const toast = useToast();
  const editing = account !== null;

  // New API 分组下拉：点「拉取分组」时现场拉一次，不缓存过期数据。
  const [groups, setGroups] = useState<{ name: string; ratio: string; description: string | null }[]>(
    [],
  );
  const [groupsLoading, setGroupsLoading] = useState(false);
  const loadGroups = async () => {
    if (!account) return;
    setGroupsLoading(true);
    try {
      const result = await api.multiplierGroups(account.id);
      setGroups(result.groups);
      const [only] = result.groups;
      if (!only) {
        toast.error("这个账号下没有可用分组");
      } else if (!form.new_api_group && result.groups.length === 1) {
        // 只有一个分组时直接选中，不必再点一次。
        set("new_api_group", only.name);
      } else {
        toast.success(`拉到 ${result.groups.length} 个分组，从下拉框里选一个`);
      }
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "拉取分组失败");
    } finally {
      setGroupsLoading(false);
    }
  };

  const [form, setForm] = useState({
    group_id: account?.group_id ?? data.groups[0]?.id ?? "",
    name: account?.name ?? "",
    upstream_type: account?.upstream_type ?? ("openai_compatible" as UpstreamType),
    base_url: account?.base_url ?? "",
    api_key: "",
    preferred_protocol: account?.preferred_protocol ?? ("openai_chat" as Protocol),
    default_priority: String(account?.default_priority ?? 0),
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
    hide_original: account?.hide_original ?? false,
    adaptive_protocol: account?.adaptive_protocol ?? true,
  });
  const [busy, setBusy] = useState(false);
  const [testing, setTesting] = useState(false);
  /** 改分组待确认时暂存的提交内容（§4.2.2）。 */
  const [pendingMove, setPendingMove] = useState<AccountInput | null>(null);
  const groupName = (id: string) => data.groups.find((group) => group.id === id)?.name ?? id;
  /**
   * 目标分组的上限低于账号当前有效倍率时，迁过去它不会被调度（§11.5 的红线），
   * 而这件事在账号列表上完全看不出来——提前说一句。
   */
  const moveEligibilityNotice = (() => {
    if (!editing || !account || form.group_id === account.group_id) return null;
    const target = data.groups.find((group) => group.id === form.group_id);
    if (!target) return null;
    const limit = Number(target.multiplier_limit);
    const effective = Number(account.effective_multiplier);
    if (!Number.isFinite(limit) || !Number.isFinite(effective) || effective <= limit) return null;
    return `注意：账号当前有效倍率 ${account.effective_multiplier} 高于「${target.name}」的上限 ${target.multiplier_limit}，迁过去后它不会被调度，除非同时降低倍率或提高该分组的上限。`;
  })();
  // Key 池（§4.2.1）：编辑已有账号时从后台带回的元数据起手，明文一律为空。
  const [keyDrafts, setKeyDrafts] = useState<KeyDraft[]>(() =>
    account ? draftsFromKeys(account.keys ?? []) : [emptyDraft()],
  );
  // 逐把 Key 的测试结果，按 Key 行 ID 对齐；新增行用序号兜底。
  const [keyTestResults, setKeyTestResults] = useState<
    Record<string, { ok: boolean; message: string }>
  >({});
  const [initialSnapshot] = useState(() => JSON.stringify(form));
  const [initialKeys] = useState(() => JSON.stringify(keyDrafts));
  const dirty =
    JSON.stringify(form) !== initialSnapshot || JSON.stringify(keyDrafts) !== initialKeys;

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

  // 站点级凭据：该 Base URL 配过一次就不必在账号里重复填（§6.4）。
  const [sites, setSites] = useState<{ base_url: string; user_id: string }[]>([]);
  useEffect(() => {
    if (!open) return;
    api
      .newApiSites()
      .then((result) => setSites(result.sites))
      .catch(() => setSites([]));
  }, [open]);
  const normalizeSite = (value: string) => value.trim().replace(/\/+$/, "").toLowerCase();
  const siteCredential = sites.find(
    (site) => normalizeSite(site.base_url) === normalizeSite(form.base_url),
  );
  const hasAccountToken = account?.has_new_api_token ?? false;
  const usesSiteCredential = Boolean(
    form.multiplier_mode === "new_api" &&
      siteCredential &&
      !form.new_api_token.trim() &&
      !hasAccountToken,
  );

  const needsNewApiToken =
    form.multiplier_mode === "new_api" &&
    !form.new_api_token.trim() &&
    !hasAccountToken &&
    !usesSiteCredential;
  const needsNewApiUser =
    form.multiplier_mode === "new_api" && !form.new_api_user_id.trim() && !usesSiteCredential;

  // Key 池自己的校验：新增的 Key 必须填明文、限额必须是正整数（§4.2.1）。
  const keyDraftError = validateDrafts(keyDrafts);
  const invalid =
    !form.name.trim() ||
    !form.base_url.trim() ||
    !!keyDraftError ||
    !!multiplierError ||
    !!calibrationError ||
    !!priorityError ||
    Object.values(limitErrors).some(Boolean) ||
    needsNewApiToken ||
    needsNewApiUser;

  /** 编辑态下用已保存的凭据发一次真实请求；新建时必须先保存。 */
  const testConnection = async () => {
    if (!account || testing) return;
    setTesting(true);
    try {
      const result = await api.testAccount(account.id);
      // 逐把 Key 的结果按行 ID 落到编辑器上：多 Key 账号最有用的诊断动作就是
      // "哪几把已经死了"（§4.2.1）。
      const perKey: Record<string, { ok: boolean; message: string }> = {};
      for (const entry of result.keys ?? []) {
        perKey[entry.id] = { ok: entry.ok, message: entry.message };
      }
      setKeyTestResults(perKey);
      const total = result.key_total ?? 0;
      if (total > 1) {
        const healthy = result.healthy_keys ?? 0;
        const summary = `「${account.name}」${healthy}/${total} 把 Key 可用`;
        if (healthy === total) toast.success(summary);
        else toast.error(summary);
      } else if (result.ok) {
        toast.success(`「${account.name}」${result.message}`);
      } else {
        toast.error(`「${account.name}」${result.message}`);
      }
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "测试失败");
    } finally {
      setTesting(false);
    }
  };

  /** 真正落库。编辑态下 keys 是整体替换：带 id 的沿用原密文、带明文的轮换、没出现的删除。 */
  const save = async (payload: AccountInput, moving: boolean) => {
    setBusy(true);
    try {
      if (editing) {
        await api.updateAccount(account.id, payload);
        toast.success(
          moving
            ? `账号已迁入「${groupName(form.group_id)}」，模型已按对外名一并迁移`
            : "账号已更新",
        );
        await onSaved();
      } else {
        const created = await api.createAccount(payload);
        toast.success("账号已创建");
        await onSaved(created);
      }
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "保存失败");
    } finally {
      setBusy(false);
    }
  };

  const submit = () => {
    if (invalid) return;
    const payload: AccountInput = {
      group_id: form.group_id,
      name: form.name.trim(),
      upstream_type: form.upstream_type,
      base_url: form.base_url.trim(),
      keys: draftsToInputs(keyDrafts),
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
      hide_original: form.hide_original,
      adaptive_protocol: form.adaptive_protocol,
    };
    if (form.new_api_token.trim()) payload.new_api_token = form.new_api_token.trim();
    // 改分组会把整台账号的模型搬到另一个分组（§4.2.2）：旧分组可能因此少了
    // 这些模型，先让管理员看清后果再发。
    if (editing && form.group_id !== account.group_id) {
      setPendingMove(payload);
      return;
    }
    void save(payload, false);
  };

  return (
    <>
    <Drawer
      open={open}
      onClose={onClose}
      dirty={dirty}
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
        <FormSection title="基本信息" description="账号身份、归属与上游地址。">
        <Field
          label="所属分组"
          hint={
            editing
              ? "改分组会把账号的模型按对外名一起迁过去：新分组缺同名逻辑模型会自动建好，旧分组里只靠它提供的自动模型会随最后一个目标消失。"
              : "账号必须属于某个分组；分组决定它能被哪把下游 Key 使用。"
          }
        >
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

        <KeyPoolEditor
          drafts={keyDrafts}
          editing={!!editing}
          testing={testing}
          testResults={keyTestResults}
          onChange={setKeyDrafts}
          onTest={() => void testConnection()}
        />

        <Field
          label="默认人工优先级"
          error={priorityError ?? undefined}
          hint="账号级人工优先级，默认 0。相同数字同属一层，层内完全按综合评分分配；只有需要「硬保底顺序」时才把某几个账号调高。"
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

        </FormSection>

        <FormSection title="倍率与校准" description="有效倍率由上游倍率与校准系数共同决定。">
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
            {usesSiteCredential && siteCredential && (
              <p className="text-faint" style={{ margin: "0 0 10px", fontSize: 12.5 }}>
                这个 Base URL 已配置站点凭据（用户 ID {siteCredential.user_id}），将自动使用，无需在本账号重复填写。
              </p>
            )}
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
              label="分组名"
              hint="这把 Key 在 New API 上所属的分组；点「拉取分组」从账号里选。留空会按可用分组的最高倍率保守估算。"
            >
              {(id) => (
                <div className="row" style={{ gap: 8 }}>
                  {groups.length > 0 ? (
                    <select
                      id={id}
                      className="select mono"
                      value={form.new_api_group}
                      onChange={(e) => set("new_api_group", e.target.value)}
                    >
                      <option value="">（留空：按最高档估算）</option>
                      {groups.map((group) => (
                        <option key={group.name} value={group.name}>
                          {group.name} · {group.ratio}
                          {group.description ? ` · ${group.description}` : ""}
                        </option>
                      ))}
                    </select>
                  ) : (
                    <input
                      id={id}
                      className="input mono"
                      value={form.new_api_group}
                      onChange={(e) => set("new_api_group", e.target.value)}
                      placeholder="default"
                    />
                  )}
                  <Button
                    variant="ghost"
                    type="button"
                    onClick={() => void loadGroups()}
                    disabled={groupsLoading || (!form.new_api_user_id.trim() && !siteCredential) || !account}
                  >
                    {groupsLoading && <span className="spinner" aria-hidden="true" />}
                    {groupsLoading ? "拉取中…" : "拉取分组"}
                  </Button>
                </div>
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
          <InfoTip label="什么是校准系数">
            倍率是上游的计费折扣；校准系数把不同站点的口径对齐到同一把尺子上。
          </InfoTip>
        </p>

        </FormSection>

        <FormSection
          title="限制与运行时"
          description="账号级默认限制与运行时适配；不确定时保持默认即可。"
          collapsible
          defaultOpen={false}
        >
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

        {/* 运行时适配（§6.4）：关掉之后只走账号自己声明的首选端点，不再按
            端点证据猜别的路径。上游只肯接受一种协议时才关。 */}
        <Switch
          checked={form.adaptive_protocol}
          onChange={(value) => set("adaptive_protocol", value)}
          label="运行时自动适配端点"
          hint={
            form.adaptive_protocol
              ? "按端点证据依次尝试：能无损表达请求的端点优先，猜错会自动回退。"
              : "只用首选端点，不做任何推断。上游只接受一种协议、或回退会造成副作用时关闭。"
          }
        />

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
        <Switch
          checked={form.hide_original}
          onChange={(value) => set("hide_original", value)}
          label="隐藏原始模型名"
          hint={
            form.hide_original
              ? "只暴露在「模型管理」里填写了下游模型名的模型；没填的模型下游无法获取。"
              : "下游既能用下游模型名，也能用上游原模型名。"
          }
        />
        {editing && form.auto_sync && account?.model_synced_at != null && (
          <p className="field-hint" style={{ marginTop: -6 }}>
            上次同步：{new Date(account.model_synced_at * 1000).toLocaleString()}
          </p>
        )}
        </FormSection>
      </div>
    </Drawer>

    <ConfirmDialog
      open={pendingMove !== null}
      title="迁移账号分组"
      confirmLabel="迁移"
      message={
        editing && account ? (
          <>
            保存后「{account.name}」会从「{groupName(account.group_id)}」迁到「
            {groupName(form.group_id)}」。
            <br />
            模型目录按对外名一起迁过去：新分组缺同名逻辑模型会自动建好；旧分组里
            只靠这台账号提供的自动逻辑模型会随最后一个目标消失，旧分组的下游 Key
            可能因此取不到这些模型。
            {moveEligibilityNotice && (
              <>
                <br />
                <strong>{moveEligibilityNotice}</strong>
              </>
            )}
          </>
        ) : null
      }
      onClose={() => setPendingMove(null)}
      onConfirm={() => {
        const payload = pendingMove;
        setPendingMove(null);
        if (payload) void save(payload, true);
      }}
    />
    </>
  );
}
