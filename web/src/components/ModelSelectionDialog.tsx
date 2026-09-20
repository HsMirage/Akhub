/**
 * 账号模型管理对话框。
 *
 * 这是“下游能看到什么模型”的唯一配置入口：
 * - 「下游模型名」留空 = 使用上游原名；
 * - 填写后 = 下游用这个名称调用；
 * - 多个账号填成同一个名称，会自动合并为一个模型；
 * - 账号级「隐藏原始模型名」打开后，只暴露填写了下游模型名的模型。
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "../lib/api";
import type { Account, AccountModel, DispatchTarget } from "../lib/types";
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
  const [query, setQuery] = useState("");
  const [onlyEnabled, setOnlyEnabled] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);

  const [editing, setEditing] = useState<string | null>(null);
  const [editAlias, setEditAlias] = useState("");
  const [adding, setAdding] = useState(false);
  const [newUpstream, setNewUpstream] = useState("");
  const [newAlias, setNewAlias] = useState("");
  const [pendingDelete, setPendingDelete] = useState<AccountModel | null>(null);

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
    try {
      const result = await api.availableGroupModels(account.group_id);
      setGroupModels(result.models);
    } catch {
      setGroupModels([]);
    }
  }, [account]);

  useEffect(() => {
    if (!open) return;
    setQuery("");
    setOnlyEnabled(false);
    setEditing(null);
    setAdding(false);
    setNotice(null);
    setHideOriginal(account?.hide_original ?? false);
    void load();
    void loadGroupNames();
  }, [open, account, load, loadGroupNames]);

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

  const beginEdit = (row: AccountModel) => {
    setEditing(row.upstream_model);
    setEditAlias(hasDownstreamName(row) ? row.public_name : "");
  };

  const cancelEdit = () => {
    setEditing(null);
    setEditAlias("");
  };

  const saveEdit = async () => {
    if (!account || !editing) return;
    const row = rows.find((item) => item.upstream_model === editing);
    if (!row) return;
    const alias = editAlias.trim();
    setBusy(`save:${editing}`);
    try {
      const updated = await api.updateAccountModel(account.id, {
        upstream_model: editing,
        alias,
      });
      setRows(updated);
      cancelEdit();
      setNotice(
        alias
          ? `已保存：下游用「${alias}」调用这个模型。`
          : "已清空下游模型名，这个模型会使用上游原名。",
      );
      toast.success("模型名称已保存");
      await onChanged();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "保存失败");
    } finally {
      setBusy(null);
    }
  };

  const saveHideOriginal = async (next: boolean) => {
    if (!account) return;
    setBusy("hide");
    try {
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

  const toggleRow = async (row: AccountModel, selected: boolean) => {
    if (!account) return;
    setBusy(`toggle:${row.upstream_model}`);
    try {
      const updated = await api.updateAccountModel(account.id, {
        upstream_model: row.upstream_model,
        selected,
      });
      setRows(updated);
      toast.success(selected ? "模型已启用" : "模型已停用");
      await onChanged();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "操作失败");
    } finally {
      setBusy(null);
    }
  };

  const removeRow = async (row: AccountModel) => {
    if (!account) return;
    setBusy(`delete:${row.upstream_model}`);
    try {
      await api.deleteAccountModel(account.id, row.upstream_model);
      setPendingDelete(null);
      await load();
      toast.success(`已删除 ${row.upstream_model}`);
      await onChanged();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "删除失败");
    } finally {
      setBusy(null);
    }
  };

  const refreshUpstream = async () => {
    if (!account) return;
    setFetching(true);
    setNotice(null);
    try {
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

  return (
    <>
      <Modal
        open={open}
        onClose={onClose}
        title={account ? `模型管理 · ${account.name}` : "模型管理"}
        className="model-selection-modal"
        footer={
          <>
            <span className="text-faint tabular" style={{ fontSize: 12 }}>
              {loading
                ? "加载中…"
                : `${rows.length} 个模型 · ${enabledCount} 个启用 · ${renamedCount} 个已设下游名`}
              {blockedCount > 0 && ` · ${blockedCount} 个未设下游名且被隐藏`}
            </span>
            <div className="spacer" />
            <Button onClick={onClose}>关闭</Button>
          </>
        }
      >
        <div className="stack model-dialog-content">
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
              disabled={fetching || busy !== null}
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
                      const renamed = hasDownstreamName(row);
                      const names = exposedNames(row, hideOriginal);
                      const isEditing = editing === row.upstream_model;
                      const rowBusy =
                        busy === `toggle:${row.upstream_model}` ||
                        busy === `save:${row.upstream_model}` ||
                        busy === `delete:${row.upstream_model}`;
                      return (
                        <FragmentRow
                          key={row.upstream_model}
                          row={row}
                          renamed={renamed}
                          names={names}
                          hiddenWithoutName={hideOriginal && !renamed}
                          hideOriginal={hideOriginal}
                          isEditing={isEditing}
                          rowBusy={rowBusy}
                          managed={managed}
                          editAlias={editAlias}
                          mergeSuggestions={mergeSuggestions}
                          existingNames={existingNames}
                          onEdit={() => beginEdit(row)}
                          onCancelEdit={cancelEdit}
                          onAliasChange={setEditAlias}
                          onSave={() => void saveEdit()}
                          onToggle={(selected) => void toggleRow(row, selected)}
                          onDelete={() => setPendingDelete(row)}
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
        onConfirm={() => pendingDelete && void removeRow(pendingDelete)}
      />
    </>
  );
}

function FragmentRow({
  row,
  renamed,
  names,
  hiddenWithoutName,
  hideOriginal,
  isEditing,
  rowBusy,
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
  rowBusy: boolean;
  managed: boolean;
  editAlias: string;
  mergeSuggestions: string[];
  existingNames: string[];
  onEdit: () => void;
  onCancelEdit: () => void;
  onAliasChange: (value: string) => void;
  onSave: () => void;
  onToggle: (selected: boolean) => void;
  onDelete: () => void;
}) {
  return (
    <>
      <tr className={row.missing ? "is-missing" : undefined}>
        <td className="model-checkbox-cell">
          <input
            type="checkbox"
            checked={row.selected}
            disabled={managed || row.missing || rowBusy}
            aria-label={`启用 ${row.upstream_model}`}
            title={row.missing ? "上游已消失，重新出现后会自动恢复" : "启用 / 停用该模型"}
            onChange={(event) => onToggle(event.target.checked)}
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
              onClick={onEdit}
              disabled={managed || rowBusy}
            >
              改名
            </Button>
            <Button
              size="sm"
              variant="danger"
              icon={<IconTrash size={13} />}
              title="从目录删除并移除目标"
              aria-label={`删除模型 ${row.upstream_model}`}
              onClick={onDelete}
              disabled={managed || rowBusy}
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
                  icon={
                    rowBusy ? (
                      <span className="spinner spinner-sm" aria-hidden="true" />
                    ) : (
                      <IconCheck size={13} />
                    )
                  }
                  onClick={onSave}
                  disabled={rowBusy}
                >
                  保存
                </Button>
                <Button size="sm" icon={<IconX size={13} />} onClick={onCancelEdit} disabled={rowBusy}>
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
}
