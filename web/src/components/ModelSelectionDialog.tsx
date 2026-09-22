/**
 * 账号模型管理对话框。
 *
 * 这是“下游能看到什么模型”的唯一配置入口：
 * - 「下游模型名」留空 = 使用上游原名；
 * - 填写后 = 下游用这个名称调用；
 * - 多个账号填成同一个名称，会自动合并为一个模型；
 * - 账号级「隐藏原始模型名」打开后，只暴露填写了下游模型名的模型。
 *
 * 性能上只有两条规矩，但都是必需的：
 * 1. 勾选 / 改名先在本地草稿上生效，再用防抖合并成**一次** `/models/apply`。
 *    逐行 POST 会让服务端每次勾选都重调和一遍目标、重建一遍配置快照，
 *    几百个模型时界面就会卡住（`/models/apply` 一次请求只调和一遍）。
 * 2. 行组件用 `memo` 包起来，勾一个复选框只重渲染那一行，而不是整张表。
 */
import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ApiError, api } from "../lib/api";
import type { Account, AccountModel, DispatchTarget, SelectionWarning } from "../lib/types";
import { Badge, Button, ConfirmDialog, Modal, Switch, useToast } from "./ui";
import {
  IconCheck,
  IconEdit,
  IconPlus,
  IconRefresh,
  IconSearch,
  IconTrash,
  IconX,
} from "./Icons";

interface ModelManagerData {
  targets: DispatchTarget[];
}

/** 一次待提交的单行改动。同一行的多次改动会合并成一条。 */
interface PendingChange {
  alias?: string;
  selected?: boolean;
  delete?: boolean;
}

/** 提交给 `/models/apply` 的一行。 */
type ModelChange = PendingChange & { upstream_model: string };

/** 勾选合并的等待窗口（毫秒）。连点复选框时只发一次请求。 */
const FLUSH_DELAY_MS = 350;

/** 空数组常量：让未处于编辑态的行拿到稳定引用，`memo` 才不会被破功。 */
const NO_NAMES: string[] = [];

/** 把模型名压成骨架，用来提示“这两个名字很可能是同一个模型”。 */
function modelFingerprint(name: string): string {
  let value = name.toLowerCase().replace(/[^a-z0-9]+/g, "");
  for (const suffix of ["openai", "anthropic", "official", "latest"]) {
    if (value.endsWith(suffix) && value.length > suffix.length + 3) {
      value = value.slice(0, -suffix.length);
    }
  }
  return value;
}

function hasDownstreamName(row: AccountModel): boolean {
  return row.public_name !== row.upstream_model;
}

/** 与后端保持一致：一行模型在账号级隐藏开关下，下游实际能用的名称。 */
function exposedNames(row: AccountModel, hideOriginal: boolean): string[] {
  if (hideOriginal) {
    return hasDownstreamName(row) ? [row.public_name] : [];
  }
  return hasDownstreamName(row)
    ? [row.public_name, row.upstream_model]
    : [row.upstream_model];
}

export function ModelSelectionDialog({
  account,
  data,
  open,
  onClose,
  onChanged,
}: {
  account: Account | null;
  data: ModelManagerData;
  open: boolean;
  onClose: () => void;
  onChanged: () => Promise<unknown>;
}) {
  const toast = useToast();
  const accountId = account?.id ?? "";
  const managed = account?.auto_sync ?? false;

  const [rows, setRows] = useState<AccountModel[]>([]);
  const [groupModels, setGroupModels] = useState<{ public_name: string; accounts: string[] }[]>([]);
  const [hideOriginal, setHideOriginal] = useState(account?.hide_original ?? false);
  const [loading, setLoading] = useState(false);
  const [fetching, setFetching] = useState(false);
  const [busy, setBusy] = useState<string | null>(null);
  /** 防抖提交进行中：底部用它显示“保存中…”。 */
  const [saving, setSaving] = useState(false);
  const [query, setQuery] = useState("");
  const [onlyEnabled, setOnlyEnabled] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);

  const [editing, setEditing] = useState<string | null>(null);
  const [editAlias, setEditAlias] = useState("");
  const [adding, setAdding] = useState(false);
  const [newUpstream, setNewUpstream] = useState("");
  const [newAlias, setNewAlias] = useState("");
  const [pendingDelete, setPendingDelete] = useState<AccountModel | null>(null);
  /** 有流量模型被停用/删除时的二次确认（§16.3）。 */
  const [confirmWarnings, setConfirmWarnings] = useState<SelectionWarning[] | null>(null);

  // 待提交的改动与最近一次被拒的改动：前者是草稿，后者用于“仍然停用”。
  const pending = useRef<Map<string, PendingChange>>(new Map());
  const rejected = useRef<ModelChange[] | null>(null);
  const flushTimer = useRef<number | null>(null);
  const onChangedRef = useRef(onChanged);
  onChangedRef.current = onChanged;
  /** 当前账号 ID 的即时副本：慢响应回来时用它判断是否还有效。 */
  const accountIdRef = useRef(accountId);
  accountIdRef.current = accountId;
  /** 二次确认列表的即时副本，供异步回调判断确认框是否还开着。 */
  const confirmWarningsRef = useRef<SelectionWarning[] | null>(null);
  confirmWarningsRef.current = confirmWarnings;
  /** 版本冲突是否已经自动重试过一次；避免两个标签页互踩时无限重试。 */
  const conflictRetried = useRef(false);

  const load = useCallback(async () => {
    if (!accountId) return;
    setLoading(true);
    try {
      const list = await api.accountModels(accountId);
      setRows(list);
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "读取模型目录失败");
    } finally {
      setLoading(false);
    }
  }, [accountId, toast]);

  const loadGroupNames = useCallback(async () => {
    if (!account) return;
    // 未分配账号没有分组，也就没有"同组已有模型"可以挑（§4.2.3）。
    if (account.group_id === null) {
      setGroupModels([]);
      return;
    }
    try {
      const result = await api.availableGroupModels(account.group_id);
      setGroupModels(result.models);
    } catch {
      setGroupModels([]);
    }
  }, [account]);

  /** 安排一次防抖提交；`delay` 为 0 时立即提交（版本冲突重试用）。 */
  const scheduleFlushRef = useRef<(delay: number) => void>(() => {});

  /**
   * 把草稿合并成一次请求提交。
   *
   * `force` 用于二次确认之后的再次提交（最近 24 小时有流量的模型）。
   */
  const flush = useCallback(
    async (force = false) => {
      if (flushTimer.current !== null) {
        window.clearTimeout(flushTimer.current);
        flushTimer.current = null;
      }
      // 上次被 409 拦下的改动与这次新攒的改动按行合并，新意图覆盖旧意图：
      // 二次确认还开着时用户继续勾选，确认后一次全部生效。
      const merged = new Map<string, ModelChange>();
      for (const change of rejected.current ?? []) {
        merged.set(change.upstream_model, { ...change });
      }
      for (const [upstream_model, change] of pending.current) {
        merged.set(upstream_model, {
          ...(merged.get(upstream_model) ?? { upstream_model }),
          ...change,
          upstream_model,
        });
      }
      const changes = [...merged.values()];
      if (changes.length === 0) return;
      pending.current.clear();
      rejected.current = null;
      // 请求发出后账号可能已经被切走：回来的目录只属于发出它的那个账号。
      const requestedFor = accountId;
      setSaving(true);
      try {
        const list = await api.applyAccountModels(requestedFor, changes, force);
        if (accountIdRef.current === requestedFor) setRows(list);
        if (confirmWarningsRef.current !== null) setConfirmWarnings(null);
        await onChangedRef.current();
      } catch (cause) {
        // 409 有两种完全不同的含义，必须分开：
        //
        // - `config_conflict`：乐观锁拦下的并发写（多半是上一次自己的写在途，
        //   或者另一个标签页刚改过配置）。它不是"有流量的模型要确认"，
        //   把它当成后者会弹出一个**一条警告都没有**的确认框——用户只能反复
        //   点"仍然停用"，而每次都会再撞一次。正确做法是把改动放回草稿，
        //   刷新一次配置版本，然后自动重试一次。
        // - 其余 409 才是"以下模型最近 24 小时有流量"的二次确认。
        if (cause instanceof ApiError && cause.status === 409 && cause.configConflict) {
          for (const change of changes) {
            pending.current.set(change.upstream_model, {
              ...(pending.current.get(change.upstream_model) ?? {}),
              ...change,
            });
          }
          await load();
          if (!conflictRetried.current) {
            conflictRetried.current = true;
            // 等这次 flush 的 `setSaving(false)` 落定再重试，避免紧挨着的又一次
            // 请求仍带着同一个过期版本号。
            window.setTimeout(() => scheduleFlushRef.current(0), 0);
          } else {
            toast.error("配置刚被其他会话改过，已保留你的改动；请确认目录后重试");
          }
          return;
        }
        if (cause instanceof ApiError && cause.status === 409) {
          // 停用有流量的模型：先让用户确认，改动原样留在 rejected 里。
          const payload = cause.payload as
            | { warnings?: SelectionWarning[]; needs_confirm?: SelectionWarning[] }
            | null;
          rejected.current = changes;
          setConfirmWarnings(payload?.warnings ?? payload?.needs_confirm ?? []);
        } else {
          toast.error(cause instanceof Error ? cause.message : "保存失败");
          // 本地草稿可能与服务端不一致：重新读一次目录，别让界面说谎。
          await load();
        }
      } finally {
        setSaving(false);
      }
    },
    [accountId, load, toast],
  );

  const scheduleFlush = useCallback(
    (delay: number) => {
      if (flushTimer.current !== null) window.clearTimeout(flushTimer.current);
      flushTimer.current = window.setTimeout(() => {
        flushTimer.current = null;
        // 二次确认开着时先只攒着：用户点“仍然停用”会连新改动一起提交。
        if (rejected.current === null) void flush();
      }, delay);
    },
    [flush],
  );
  scheduleFlushRef.current = scheduleFlush;

  /** 记下一行改动并安排防抖提交。 */
  const queueChange = useCallback(
    (upstreamModel: string, change: PendingChange) => {
      pending.current.set(upstreamModel, {
        ...(pending.current.get(upstreamModel) ?? {}),
        ...change,
      });
      // 用户又动手了：这一批改动值得再获得一次自动重试。
      conflictRetried.current = false;
      scheduleFlush(FLUSH_DELAY_MS);
    },
    [scheduleFlush],
  );

  useEffect(() => {
    if (!open) return;
    // 切换账号前先把上一个账号的草稿落库，避免改动被静默丢弃。
    void flush();
    setQuery("");
    setOnlyEnabled(false);
    setEditing(null);
    setAdding(false);
    setNotice(null);
    setConfirmWarnings(null);
    rejected.current = null;
    conflictRetried.current = false;
    setHideOriginal(account?.hide_original ?? false);
    void load();
    void loadGroupNames();
  }, [open, account, flush, load, loadGroupNames]);

  // 组件消失时把草稿补交一次；已提交的请求不受影响。
  useEffect(
    () => () => {
      if (flushTimer.current !== null) {
        window.clearTimeout(flushTimer.current);
        flushTimer.current = null;
      }
      const changes = [...pending.current.entries()].map(([upstream_model, change]) => ({
        upstream_model,
        ...change,
      }));
      if (changes.length > 0) void api.applyAccountModels(accountId, changes, false);
    },
    [accountId],
  );

  const existingNames = useMemo(() => {
    const names = new Set<string>();
    for (const item of groupModels) names.add(item.public_name);
    for (const row of rows) names.add(row.public_name);
    return [...names].sort((a, b) => a.localeCompare(b, "zh-CN"));
  }, [groupModels, rows]);

  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return [...rows]
      .sort((a, b) => a.public_name.localeCompare(b.public_name, "zh-CN"))
      .filter((row) => {
        if (onlyEnabled && !row.selected) return false;
        if (!needle) return true;
        return (
          row.upstream_model.toLowerCase().includes(needle) ||
          row.public_name.toLowerCase().includes(needle)
        );
      });
  }, [rows, onlyEnabled, query]);

  const mergeSuggestions = useMemo(() => {
    if (!editing) return [];
    const row = rows.find((item) => item.upstream_model === editing);
    if (!row) return [];
    const fingerprint = modelFingerprint(row.upstream_model);
    return existingNames.filter(
      (name) => name !== row.public_name && modelFingerprint(name) === fingerprint,
    );
  }, [editing, rows, existingNames]);

  const enabledCount = rows.filter((row) => row.selected).length;
  const renamedCount = rows.filter(hasDownstreamName).length;
  const blockedCount = hideOriginal ? rows.filter((row) => !hasDownstreamName(row)).length : 0;
  const deleteTargets = pendingDelete
    ? data.targets.filter(
        (target) =>
          target.account_id === account?.id &&
          target.upstream_model === pendingDelete.upstream_model,
      ).length
    : 0;

  const beginEdit = useCallback((upstreamModel: string) => {
    setEditing(upstreamModel);
    setRows((current) => {
      const row = current.find((item) => item.upstream_model === upstreamModel);
      setEditAlias(row && hasDownstreamName(row) ? row.public_name : "");
      return current;
    });
  }, []);

  const cancelEdit = useCallback(() => {
    setEditing(null);
    setEditAlias("");
  }, []);

  const saveEdit = useCallback(() => {
    if (!account || !editing) return;
    const alias = editAlias.trim();
    setRows((current) =>
      current.map((row) => {
        if (row.upstream_model !== editing) return row;
        const updated: AccountModel = {
          ...row,
          public_name: alias === "" ? row.upstream_model : alias,
        };
        return { ...updated, exposed_names: exposedNames(updated, hideOriginal) };
      }),
    );
    queueChange(editing, { alias });
    cancelEdit();
    setNotice(
      alias
        ? `已保存：下游用「${alias}」调用这个模型。`
        : "已清空下游模型名，这个模型会使用上游原名。",
    );
  }, [account, editing, editAlias, hideOriginal, queueChange, cancelEdit]);

  const saveHideOriginal = async (next: boolean) => {
    if (!account) return;
    setBusy("hide");
    try {
      await flush();
      await api.updateAccount(account.id, { hide_original: next });
      setHideOriginal(next);
      await load();
      toast.success(next ? "已打开：只暴露设置了下游模型名的模型" : "已关闭：允许下游使用原模型名");
      await onChanged();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "保存失败");
    } finally {
      setBusy(null);
    }
  };

  const toggleRow = useCallback(
    (upstreamModel: string, selected: boolean) => {
      if (!account) return;
      setRows((current) =>
        current.map((row) =>
          row.upstream_model === upstreamModel ? { ...row, selected } : row,
        ),
      );
      queueChange(upstreamModel, { selected });
    },
    [account, queueChange],
  );

  const requestDelete = useCallback((upstreamModel: string) => {
    setRows((current) => current.filter((row) => row.upstream_model !== upstreamModel));
    queueChange(upstreamModel, { delete: true });
    setPendingDelete(null);
  }, [queueChange]);

  const refreshUpstream = async () => {
    if (!account) return;
    setFetching(true);
    setNotice(null);
    try {
      await flush();
      const list = await api.refreshAccountModels(account.id);
      setRows(list);
      const created = list.filter((row) => row.is_new).length;
      const missing = list.filter((row) => row.missing).length;
      setNotice(
        `已从上游拉取 ${list.length} 个模型${created > 0 ? `，其中 ${created} 个是新增` : ""}${
          missing > 0 ? `；${missing} 个已从上游消失` : ""
        }。`,
      );
      toast.success(`已同步 ${list.length} 个模型`);
      await onChanged();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "拉取失败，已保留原目录");
    } finally {
      setFetching(false);
    }
  };

  const addModel = async () => {
    if (!account || !newUpstream.trim()) return;
    setBusy("add");
    try {
      await api.addManualModel(account.id, newUpstream.trim(), newAlias.trim());
      setNewUpstream("");
      setNewAlias("");
      setAdding(false);
      await load();
      toast.success("模型已加入");
      await onChanged();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "添加失败");
    } finally {
      setBusy(null);
    }
  };

  const syncManaged = async () => {
    if (!account) return;
    setBusy("sync");
    try {
      const result = await api.syncAccountModels(account.id);
      await load();
      toast.success(`已托管 ${result.managed_models} 个模型`);
      await onChanged();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "同步失败");
    } finally {
      setBusy(null);
    }
  };

  const close = () => {
    // 未到防抖窗口的改动在这里补交，关闭弹窗不会丢操作。
    void flush();
    onClose();
  };

  return (
    <>
      <Modal
        open={open}
        onClose={close}
        title={account ? `模型管理 · ${account.name}` : "模型管理"}
        className="model-selection-modal"
        footer={
          <>
            <span className="text-faint tabular" style={{ fontSize: 12 }}>
              {loading
                ? "加载中…"
                : `${rows.length} 个模型 · ${enabledCount} 个启用 · ${renamedCount} 个已设下游名`}
              {blockedCount > 0 && ` · ${blockedCount} 个未设下游名且被隐藏`}
              {saving && " · 保存中…"}
            </span>
            <div className="spacer" />
            <Button onClick={close}>关闭</Button>
          </>
        }
      >
        <div className="stack model-dialog-content">
          {account?.group_id === null && (
            <div className="callout callout-info">
              该账号还没有分配到分组：可以在这里整理模型目录、设好下游模型名，但
              <strong>不会生成调度目标</strong>，下游暂时用不到这些模型。去「编辑」
              里选一个分组，模型会按对外名一起迁过去并立即生效。
            </div>
          )}
          {managed && (
            <div className="callout callout-info">
              该账号已开启模型自动同步：上游全部模型由后台托管，删除 / 停用 /
              改名暂不可用。关闭自动同步后可回到手动选择集。
            </div>
          )}

          <div className="model-help">
            <strong>下游模型名怎么填？</strong>
            <span>
              留空就是使用上游原名；填写后，下游用这个名字调用。多个账号把同一个模型填成
              同一个名字，系统会自动合并成一个模型。
            </span>
          </div>

          <Switch
            checked={hideOriginal}
            onChange={(next) => void saveHideOriginal(next)}
            label="隐藏原始模型名（整个账号）"
            hint={
              hideOriginal
                ? "已打开：只暴露填写了下游模型名的模型；没填的模型下游无法获取。"
                : "关闭时：下游既能用下游模型名，也能用上游原模型名。"
            }
          />

          {notice && (
            <div className="callout callout-info" role="status">
              <span style={{ flex: 1 }}>{notice}</span>
              <button type="button" className="link-button" onClick={() => setNotice(null)}>
                知道了
              </button>
            </div>
          )}

          <div className="model-manager-toolbar">
            <div className="input-with-icon list-search">
              <IconSearch size={14} />
              <input
                className="input"
                value={query}
                placeholder="搜索模型名"
                aria-label="搜索模型"
                onChange={(event) => setQuery(event.target.value)}
              />
            </div>
            <label className="filter-toggle">
              <input
                type="checkbox"
                checked={onlyEnabled}
                onChange={(event) => setOnlyEnabled(event.target.checked)}
              />
              只看已启用
            </label>
            <span className="spacer" />
            {!managed && (
              <Button
                icon={<IconPlus size={13} />}
                onClick={() => setAdding((current) => !current)}
                disabled={adding || busy !== null}
              >
                添加模型
              </Button>
            )}
            <Button
              variant="primary"
              icon={
                fetching ? (
                  <span className="spinner spinner-sm" aria-hidden="true" />
                ) : (
                  <IconRefresh size={13} />
                )
              }
              onClick={() => void refreshUpstream()}
              disabled={fetching || busy !== null || saving}
            >
              {fetching ? "拉取中…" : "获取上游模型"}
            </Button>
            {managed && (
              <Button
                variant="primary"
                icon={
                  busy === "sync" ? (
                    <span className="spinner spinner-sm" aria-hidden="true" />
                  ) : (
                    <IconRefresh size={13} />
                  )
                }
                onClick={() => void syncManaged()}
                disabled={busy !== null}
              >
                {busy === "sync" ? "同步中…" : "立即同步"}
              </Button>
            )}
          </div>

          {adding && !managed && (
            <div className="model-add-row model-add-row-plain">
              <input
                className="input mono"
                value={newUpstream}
                placeholder="上游模型名，例如 gpt-5.6-sol-openai"
                aria-label="上游模型名"
                onChange={(event) => setNewUpstream(event.target.value)}
              />
              <input
                className="input mono"
                value={newAlias}
                placeholder="下游模型名（可留空）"
                aria-label="下游模型名"
                list="akhub-group-model-names"
                onChange={(event) => setNewAlias(event.target.value)}
              />
              <Button
                variant="primary"
                icon={
                  busy === "add" ? (
                    <span className="spinner spinner-sm" aria-hidden="true" />
                  ) : (
                    <IconPlus size={13} />
                  )
                }
                onClick={() => void addModel()}
                disabled={busy !== null || !newUpstream.trim()}
              >
                添加
              </Button>
              <Button variant="ghost" onClick={() => setAdding(false)}>
                取消
              </Button>
            </div>
          )}

          <datalist id="akhub-group-model-names">
            {existingNames.map((name) => (
              <option key={name} value={name} />
            ))}
          </datalist>

          {rows.length === 0 ? (
            <div className="empty" style={{ padding: 24 }}>
              <div className="empty-title">还没有模型</div>
              <p className="empty-desc">
                点「获取上游模型」从站点拉取；接口不可用时也可以手动添加上游模型名。
              </p>
              {!managed && (
                <Button variant="primary" icon={<IconPlus size={13} />} onClick={() => setAdding(true)}>
                  手动添加
                </Button>
              )}
            </div>
          ) : (
            <div className="table-wrap model-manager-wrap">
              <table className="data model-manager-table model-manager-table-wide">
                <thead>
                  <tr>
                    <th aria-label="启用" />
                    <th>上游模型名</th>
                    <th>下游模型名</th>
                    <th>下游可用名称</th>
                    <th>状态</th>
                    <th />
                  </tr>
                </thead>
                <tbody>
                  {visible.length === 0 ? (
                    <tr>
                      <td colSpan={6} className="table-empty-cell">
                        没有符合条件的模型
                      </td>
                    </tr>
                  ) : (
                    visible.map((row) => {
                      const isEditing = editing === row.upstream_model;
                      return (
                        <ModelRow
                          key={row.upstream_model}
                          row={row}
                          renamed={hasDownstreamName(row)}
                          names={exposedNames(row, hideOriginal)}
                          hiddenWithoutName={hideOriginal && !hasDownstreamName(row)}
                          hideOriginal={hideOriginal}
                          isEditing={isEditing}
                          managed={managed}
                          editAlias={isEditing ? editAlias : ""}
                          mergeSuggestions={isEditing ? mergeSuggestions : NO_NAMES}
                          existingNames={isEditing ? existingNames : NO_NAMES}
                          onEdit={beginEdit}
                          onCancelEdit={cancelEdit}
                          onAliasChange={setEditAlias}
                          onSave={saveEdit}
                          onToggle={toggleRow}
                          onDelete={setPendingDelete}
                        />
                      );
                    })
                  )}
                </tbody>
              </table>
            </div>
          )}
        </div>
      </Modal>

      <ConfirmDialog
        open={pendingDelete !== null}
        title="删除模型"
        danger
        confirmLabel="删除"
        message={
          pendingDelete ? (
            <>
              删除「{pendingDelete.upstream_model}」会移除它对应的 {deleteTargets} 个调度目标，
              下游将无法再用这个模型请求，在途请求会正常完成。
            </>
          ) : null
        }
        onClose={() => setPendingDelete(null)}
        onConfirm={() => pendingDelete && requestDelete(pendingDelete.upstream_model)}
      />

      <ConfirmDialog
        open={confirmWarnings !== null}
        title="这些模型最近 24 小时有流量"
        danger
        confirmLabel="仍然停用"
        message={
          <>
            {confirmWarnings?.map((warning) => (
              <span key={warning.public_name} style={{ display: "block" }}>
                「{warning.public_name}」最近 24 小时有 {warning.calls} 次调用。
              </span>
            ))}
            停用或删除后，下游再请求这些模型会直接失败。
          </>
        }
        onClose={() => {
          setConfirmWarnings(null);
          rejected.current = null;
          void load();
        }}
        onConfirm={() => void flush(true)}
      />
    </>
  );
}

/**
 * 表格里的一行（含展开的改名表单）。
 *
 * `memo` 是必需的：一个账号几百个模型时，勾一个复选框不该让整张表重渲染。
 * 因此回调都做成“接收上游模型名”的稳定函数，未处于编辑态的行拿到的是
 * 全等的空数组。
 */
const ModelRow = memo(function ModelRow({
  row,
  renamed,
  names,
  hiddenWithoutName,
  hideOriginal,
  isEditing,
  managed,
  editAlias,
  mergeSuggestions,
  existingNames,
  onEdit,
  onCancelEdit,
  onAliasChange,
  onSave,
  onToggle,
  onDelete,
}: {
  row: AccountModel;
  renamed: boolean;
  names: string[];
  hiddenWithoutName: boolean;
  hideOriginal: boolean;
  isEditing: boolean;
  managed: boolean;
  editAlias: string;
  mergeSuggestions: string[];
  existingNames: string[];
  onEdit: (upstreamModel: string) => void;
  onCancelEdit: () => void;
  onAliasChange: (value: string) => void;
  onSave: () => void;
  onToggle: (upstreamModel: string, selected: boolean) => void;
  onDelete: (row: AccountModel) => void;
}) {
  return (
    <>
      <tr className={row.missing ? "is-missing" : undefined}>
        <td className="model-checkbox-cell">
          <input
            type="checkbox"
            checked={row.selected}
            disabled={managed || row.missing}
            aria-label={`启用 ${row.upstream_model}`}
            title={row.missing ? "上游已消失，重新出现后会自动恢复" : "启用 / 停用该模型"}
            onChange={(event) => onToggle(row.upstream_model, event.target.checked)}
          />
        </td>
        <td>
          <div className="mono cell-strong cell-truncate" title={row.upstream_model}>
            {row.upstream_model}
          </div>
          <div className="row model-row-badges">
            {row.is_new && <Badge tone="info">本次新增</Badge>}
          </div>
        </td>
        <td>
          {renamed ? (
            <div className="mono cell-strong cell-truncate" title={row.public_name}>
              {row.public_name}
            </div>
          ) : (
            <span className="text-faint">未设置（使用上游原名）</span>
          )}
        </td>
        <td>
          {names.length === 0 ? (
            <Badge tone="warn">下游不可获取</Badge>
          ) : (
            <div className="model-exposed-names">
              {names.map((name) => (
                <span key={name} className="model-exposed-chip mono" title={`下游可用：${name}`}>
                  {name}
                </span>
              ))}
            </div>
          )}
          <div className="field-hint" style={{ marginTop: 4 }}>
            {hiddenWithoutName
              ? "没有填写下游模型名，且账号已打开“隐藏原始模型名”。"
              : hideOriginal
                ? "只暴露下游模型名。"
                : renamed
                  ? "下游模型名 + 上游原名都能用。"
                  : "下游使用上游原名。"}
          </div>
        </td>
        <td>
          <div className="row model-state-badges">
            {row.missing ? (
              <Badge tone="danger" dot>
                上游已消失
              </Badge>
            ) : row.selected ? (
              <Badge tone="success" dot>
                已启用
              </Badge>
            ) : (
              <Badge tone="neutral" dot>
                已停用
              </Badge>
            )}
            {row.excluded && !row.missing && <Badge tone="warn">你停用过</Badge>}
          </div>
        </td>
        <td>
          <div className="cell-actions">
            <Button
              size="sm"
              icon={<IconEdit size={13} />}
              onClick={() => onEdit(row.upstream_model)}
              disabled={managed}
            >
              改名
            </Button>
            <Button
              size="sm"
              variant="danger"
              icon={<IconTrash size={13} />}
              title="从目录删除并移除目标"
              aria-label={`删除模型 ${row.upstream_model}`}
              onClick={() => onDelete(row)}
              disabled={managed}
            />
          </div>
        </td>
      </tr>
      {isEditing && (
        <tr className="model-edit-row">
          <td />
          <td colSpan={5}>
            <div className="model-edit-form model-edit-form-plain">
              <div className="model-edit-field">
                <label className="field-label" htmlFor={`alias-${row.upstream_model}`}>
                  下游模型名
                </label>
                <input
                  id={`alias-${row.upstream_model}`}
                  className="input mono"
                  value={editAlias}
                  list="akhub-group-model-names"
                  placeholder={row.upstream_model}
                  maxLength={100}
                  onChange={(event) => onAliasChange(event.target.value)}
                />
                {existingNames.some((name) => name !== row.public_name) && (
                  <select
                    className="select"
                    value=""
                    aria-label="合并到已有模型"
                    onChange={(event) => {
                      if (event.target.value) onAliasChange(event.target.value);
                    }}
                  >
                    <option value="">合并到已有模型…</option>
                    {existingNames
                      .filter((name) => name !== row.public_name)
                      .map((name) => (
                        <option key={name} value={name}>
                          {name}
                        </option>
                      ))}
                  </select>
                )}
                <span className="field-hint">
                  留空 = 使用上游原名；填写后下游用这个名字调用。多个账号填成同一个名字会自动合并。
                </span>
              </div>
              <div className="model-edit-actions">
                <Button
                  variant="primary"
                  size="sm"
                  icon={<IconCheck size={13} />}
                  onClick={onSave}
                >
                  保存
                </Button>
                <Button size="sm" icon={<IconX size={13} />} onClick={onCancelEdit}>
                  取消
                </Button>
              </div>
            </div>
            {mergeSuggestions.length > 0 && (
              <div className="model-merge-suggestions">
                <span className="text-faint">可以合并到已有模型：</span>
                {mergeSuggestions.map((name) => (
                  <button
                    key={name}
                    type="button"
                    className="model-suggestion"
                    onClick={() => onAliasChange(name)}
                  >
                    用「{name}」
                  </button>
                ))}
              </div>
            )}
          </td>
        </tr>
      )}
    </>
  );
});
