/**
 * 模型选择与别名编辑对话框（§16）：拉取目录、批量选择、一次提交生成调度目标。
 * 选择集和别名分别保存，避免用户在筛选列表里逐条点击时产生半成品配置。
 */
import { useCallback, useEffect, useMemo, useState } from "react";
import { api } from "../lib/api";
import type { Account, AccountModel, Alias, SelectionWarning } from "../lib/types";
import { Badge, Button, ConfirmDialog, Modal, useToast } from "./ui";
import { IconPlus, IconRefresh } from "./Icons";

export function ModelSelectionDialog({
  account,
  open,
  onClose,
}: {
  account: Account | null;
  open: boolean;
  onClose: () => void;
}) {
  const toast = useToast();
  const [models, setModels] = useState<AccountModel[]>([]);
  const [aliases, setAliases] = useState<Alias[]>([]);
  const [aliasDraft, setAliasDraft] = useState<Record<string, string>>({});
  const [checked, setChecked] = useState<Set<string>>(new Set());
  const [onlyNew, setOnlyNew] = useState(false);
  const [query, setQuery] = useState("");
  const [busy, setBusy] = useState(false);
  const [fetching, setFetching] = useState(false);
  const [aliasBusy, setAliasBusy] = useState(false);
  const [manual, setManual] = useState("");
  const [pendingRemove, setPendingRemove] = useState<SelectionWarning[] | null>(null);
  const [clearAliasesConfirm, setClearAliasesConfirm] = useState(false);

  const accountId = account?.id;
  const managed = account?.auto_sync ?? false;

  const hydrate = useCallback((catalog: AccountModel[], aliasRows: Alias[]) => {
    const aliasByUpstream = new Map(
      aliasRows.map((alias) => [alias.upstream_model, alias.public_name]),
    );
    setModels(catalog);
    setAliases(aliasRows);
    setAliasDraft(
      Object.fromEntries(
        catalog.map((model) => [
          model.upstream_model,
          aliasByUpstream.get(model.upstream_model) ?? "",
        ]),
      ),
    );
    setChecked(new Set(catalog.filter((model) => model.selected).map((model) => model.public_name)));
  }, []);

  const load = useCallback(async () => {
    if (!accountId) return;
    try {
      const [catalog, aliasRows] = await Promise.all([
        api.accountModels(accountId),
        api.aliases(accountId),
      ]);
      hydrate(catalog, aliasRows);
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "读取模型目录失败");
    }
  }, [accountId, hydrate, toast]);

  useEffect(() => {
    if (!open) return;
    setQuery("");
    setOnlyNew(false);
    setPendingRemove(null);
    void load();
  }, [open, load]);

  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return models.filter((model) => {
      if (onlyNew && !model.is_new) return false;
      if (!needle) return true;
      return (
        model.upstream_model.toLowerCase().includes(needle) ||
        model.public_name.toLowerCase().includes(needle)
      );
    });
  }, [models, onlyNew, query]);

  const applyVisible = (mode: "all" | "none" | "new") => {
    setChecked((current) => {
      const next = new Set(current);
      visible.forEach((model) => {
        if (model.missing) {
          next.delete(model.public_name);
        } else if (mode === "all" || (mode === "new" && model.is_new)) {
          next.add(model.public_name);
        } else {
          next.delete(model.public_name);
        }
      });
      return next;
    });
  };

  const apply = async (force: boolean) => {
    if (!accountId) return;
    setBusy(true);
    try {
      const result = await api.selectAccountModels(accountId, [...checked], force);
      if ("needs_confirm" in result) {
        setPendingRemove(result.needs_confirm);
        return;
      }
      toast.success(
        `已更新：新建 ${result.created_targets} 个目标，移除 ${result.removed_targets} 个`,
      );
      setPendingRemove(null);
      await load();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "应用选择失败");
    } finally {
      setBusy(false);
    }
  };

  const fetchUpstream = async () => {
    if (!accountId) return;
    setFetching(true);
    try {
      const [catalog, aliasRows] = await Promise.all([
        api.refreshAccountModels(accountId),
        api.aliases(accountId),
      ]);
      hydrate(catalog, aliasRows);
      setOnlyNew(true);
      toast.success(`拉取到 ${catalog.length} 个模型`);
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "拉取失败，已保留原列表");
    } finally {
      setFetching(false);
    }
  };

  const addManual = async () => {
    if (!accountId || !manual.trim()) return;
    setBusy(true);
    try {
      await api.addManualModel(accountId, manual.trim());
      setManual("");
      toast.success("手动模型已加入调度");
      await load();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "添加失败");
    } finally {
      setBusy(false);
    }
  };

  const syncManaged = async () => {
    if (!accountId) return;
    setBusy(true);
    try {
      const result = await api.syncAccountModels(accountId);
      toast.success(`已托管 ${result.managed_models} 个模型`);
      await load();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "同步失败");
    } finally {
      setBusy(false);
    }
  };

  const saveAliases = async () => {
    if (!accountId) return;
    setAliasBusy(true);
    try {
      const rows: Alias[] = models
        .map((model) => ({
          upstream_model: model.upstream_model,
          public_name: (aliasDraft[model.upstream_model] ?? "").trim(),
        }))
        .filter((alias) => alias.public_name.length > 0 && alias.public_name !== alias.upstream_model);
      await api.updateAliases(accountId, rows);
      toast.success(`已保存 ${rows.length} 个别名`);
      await load();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "别名保存失败");
    } finally {
      setAliasBusy(false);
    }
  };

  const clearAliases = () => {
    setAliasDraft((current) =>
      Object.fromEntries(Object.keys(current).map((key) => [key, ""])),
    );
  };

  const selectedCount = checked.size;

  return (
    <>
      <Modal
        open={open}
        onClose={onClose}
        title={account ? `模型 · ${account.name}` : "模型"}
        className="model-selection-modal"
        footer={
          managed ? undefined : (
            <>
              <span className="text-faint tabular" style={{ fontSize: 12 }}>
                已选 {selectedCount} / 共 {models.length}
              </span>
              <div className="spacer" />
              <Button onClick={onClose}>取消</Button>
              <Button variant="primary" onClick={() => void apply(false)} disabled={busy}>
                {busy ? <><span className="spinner spinner-sm" aria-hidden="true" /> 应用中…</> : "应用选择"}
              </Button>
            </>
          )
        }
      >
        <div className="stack model-dialog-content">
          {managed ? (
            <div className="callout callout-info">
              该账号已开启模型自动同步，全部上游模型由后台托管，忽略选择集。
            </div>
          ) : (
            <>
              <div className="model-toolbar">
                <Button
                  variant="primary"
                  icon={fetching ? <span className="spinner spinner-sm" aria-hidden="true" /> : <IconRefresh size={13} />}
                  onClick={() => void fetchUpstream()}
                  disabled={fetching}
                >
                  {fetching ? "拉取中…" : "获取模型"}
                </Button>
                <input
                  className="input model-search"
                  value={query}
                  onChange={(e) => setQuery(e.target.value)}
                  placeholder="搜索 upstream_model / public_name"
                  aria-label="搜索模型"
                />
                <label className="row model-new-filter">
                  <input
                    type="checkbox"
                    checked={onlyNew}
                    onChange={(e) => setOnlyNew(e.target.checked)}
                    disabled={models.every((model) => !model.is_new)}
                  />
                  只看新增
                </label>
              </div>
              <div className="model-bulk-actions">
                <div className="row" style={{ gap: 6 }}>
                  <Button size="sm" onClick={() => applyVisible("all")} disabled={visible.length === 0}>
                    全选
                  </Button>
                  <Button size="sm" onClick={() => applyVisible("none")} disabled={visible.length === 0}>
                    全不选
                  </Button>
                  <Button size="sm" onClick={() => applyVisible("new")} disabled={visible.length === 0}>
                    只选新增
                  </Button>
                </div>
                <span className="text-faint tabular">已选 {selectedCount} / 共 {models.length}</span>
              </div>

              {models.length === 0 ? (
                <p className="text-faint model-empty-copy">
                  还没有模型目录。点击「获取模型」从上游拉取，或手动输入上游模型名。
                </p>
              ) : (
                <div className="table-wrap model-selection-table-wrap">
                  <table className="data model-selection-table">
                    <thead>
                      <tr>
                        <th aria-label="选择" />
                        <th>上游模型 / 对外名</th>
                        <th>状态</th>
                      </tr>
                    </thead>
                    <tbody>
                      {visible.length === 0 ? (
                        <tr><td colSpan={3} className="table-empty-cell">没有匹配的模型</td></tr>
                      ) : visible.map((model) => {
                        const aliased = model.upstream_model !== model.public_name;
                        return (
                          <tr key={model.upstream_model} className={model.missing ? "is-missing" : undefined}>
                            <td className="model-checkbox-cell">
                              <input
                                type="checkbox"
                                checked={checked.has(model.public_name)}
                                disabled={model.missing}
                                title={model.missing ? "上游已消失" : undefined}
                                aria-label={`选择 ${model.public_name}`}
                                onChange={(e) => {
                                  setChecked((current) => {
                                    const next = new Set(current);
                                    if (e.target.checked) next.add(model.public_name);
                                    else next.delete(model.public_name);
                                    return next;
                                  });
                                }}
                              />
                            </td>
                            <td>
                              <div className="mono model-upstream-name" title={model.upstream_model}>
                                {model.upstream_model}
                              </div>
                              <div className={`mono model-public-name${aliased ? " is-aliased" : ""}`} title={model.public_name}>
                                {aliased ? `对外名：${model.public_name}` : model.public_name}
                              </div>
                            </td>
                            <td>
                              <div className="row model-state-badges">
                                {model.is_new && <Badge tone="info">新增</Badge>}
                {/* "你排除过"（§16.2）：和"从没出现过"分开。没有这一条，
                    管理员会以为之前的取消勾选没生效，于是再点一次。 */}
                {model.excluded && !model.selected && <Badge tone="warn">你排除过</Badge>}
                                {model.missing && <Badge tone="danger">上游已消失</Badge>}
                                {!model.is_new && !model.missing && model.selected && <Badge tone="success">已在用</Badge>}
                              </div>
                            </td>
                          </tr>
                        );
                      })}
                    </tbody>
                  </table>
                </div>
              )}

              <div className="row model-manual-add">
                <input
                  className="input mono"
                  style={{ flex: 1 }}
                  value={manual}
                  placeholder="手动输入上游模型名（上游列表接口不可用时）"
                  onChange={(e) => setManual(e.target.value)}
                />
                <Button
                  icon={<IconPlus size={13} />}
                  onClick={() => void addManual()}
                  disabled={busy || !manual.trim()}
                >
                  手动添加
                </Button>
              </div>
            </>
          )}

          {managed && (
            <Button
              variant="primary"
              icon={busy ? <span className="spinner spinner-sm" aria-hidden="true" /> : <IconRefresh size={13} />}
              onClick={() => void syncManaged()}
              disabled={busy}
            >
              {busy ? "同步中…" : "立即同步"}
            </Button>
          )}

          <section className="alias-editor">
            <div className="alias-editor-head">
              <div>
                <h3 className="card-title">模型别名</h3>
                <p className="card-desc">留空表示跟随上游真名；一次保存当前目录里的全部改动。</p>
              </div>
              <Button
                size="sm"
                variant="danger"
                onClick={() => setClearAliasesConfirm(true)}
                disabled={models.length === 0 || aliasBusy}
              >
                清空所有别名
              </Button>
            </div>
            {models.length === 0 ? (
              <p className="text-faint">先获取或同步模型目录，再设置对外名。</p>
            ) : (
              <div className="table-wrap alias-table-wrap">
                <table className="data alias-table">
                  <thead>
                    <tr><th>上游真名</th><th>对外名</th><th>状态</th></tr>
                  </thead>
                  <tbody>
                    {models.map((model) => {
                      const value = aliasDraft[model.upstream_model] ?? "";
                      const changed = value.trim().length > 0 && value.trim() !== model.upstream_model;
                      return (
                        <tr key={model.upstream_model}>
                          <td className="mono cell-truncate" title={model.upstream_model}>{model.upstream_model}</td>
                          <td>
                            <input
                              className="input mono alias-input"
                              value={value}
                              placeholder={model.upstream_model}
                              maxLength={100}
                              onChange={(e) =>
                                setAliasDraft((current) => ({
                                  ...current,
                                  [model.upstream_model]: e.target.value,
                                }))
                              }
                            />
                          </td>
                          <td>{changed ? <Badge tone="accent">已改</Badge> : <span className="text-faint">跟随上游</span>}</td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            )}
            <div className="alias-editor-foot">
              <span className="text-faint">当前已加载 {aliases.length} 个已保存别名</span>
              <Button variant="secondary" onClick={() => void saveAliases()} disabled={aliasBusy || models.length === 0}>
                {aliasBusy ? <><span className="spinner spinner-sm" aria-hidden="true" /> 保存中…</> : "保存别名"}
              </Button>
            </div>
          </section>
        </div>
      </Modal>

      <Modal
        open={pendingRemove !== null}
        onClose={() => setPendingRemove(null)}
        title="这些模型最近 24 小时有流量"
        footer={
          <>
            <Button onClick={() => setPendingRemove(null)}>取消</Button>
            <Button variant="danger" onClick={() => void apply(true)} disabled={busy}>
              仍要移除
            </Button>
          </>
        }
      >
        <div className="stack" style={{ gap: 6 }}>
          <p style={{ margin: 0, lineHeight: 1.7 }}>
            取消勾选会移除对应的调度目标，在途请求不受影响。以下模型最近 24 小时仍有调用：
          </p>
          {(pendingRemove ?? []).map((warning) => (
            <div key={warning.public_name} className="row" style={{ gap: 8 }}>
              <span className="mono cell-strong">{warning.public_name}</span>
              <Badge tone="warn">{warning.calls} 次</Badge>
            </div>
          ))}
        </div>
      </Modal>

      <ConfirmDialog
        open={clearAliasesConfirm}
        title="清空所有别名"
        danger
        confirmLabel="清空"
        message="这会把当前目录里的所有对外名恢复为上游真名。确认后还需要点击「保存别名」才会提交。"
        onClose={() => setClearAliasesConfirm(false)}
        onConfirm={clearAliases}
      />
    </>
  );
}
