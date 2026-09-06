/**
 * 模型选择对话框（§16.2）：拉取上游列表、应用别名、勾选生成调度目标。
 *
 * 已选的默认勾选、明确排除的保持不勾、新出现的默认不勾——配合"只看新增"
 * 过滤器，8 账号 × 20 模型的常规维护变成几秒钟的事（§16.3）。
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api } from "../lib/api";
import type {
  Account,
  AccountModel,
  Alias,
  SelectionWarning,
} from "../lib/types";
import { Badge, Button, Modal, useToast } from "./ui";
import { IconPlus, IconRefresh, IconTrash } from "./Icons";

/** 一行的状态标签（§16.2 对话框示意）。 */
function stateLabel(
  model: AccountModel,
): { text: string; tone: "success" | "warn" | "info" | "danger" } | null {
  if (model.missing) return { text: "已消失", tone: "danger" };
  if (model.is_new) return { text: "新出现", tone: "info" };
  return model.selected ? { text: "已在用", tone: "success" } : { text: "你排除过", tone: "warn" };
}

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
  const [checked, setChecked] = useState<Set<string>>(new Set());
  const [onlyNew, setOnlyNew] = useState(false);
  const [busy, setBusy] = useState(false);
  const [fetching, setFetching] = useState(false);
  const [manual, setManual] = useState("");
  const [pendingRemove, setPendingRemove] = useState<SelectionWarning[] | null>(null);
  /** 记住已展示过"新出现"的模型：确认或拉取之后就不再是新的。 */
  const knownNew = useRef<Set<string>>(new Set());

  const accountId = account?.id;

  const load = useCallback(async () => {
    if (!accountId) return;
    try {
      const [catalog, aliasRows] = await Promise.all([
        api.accountModels(accountId),
        api.aliases(accountId),
      ]);
      setModels(catalog);
      setAliases(aliasRows);
      setChecked(new Set(catalog.filter((m) => m.selected).map((m) => m.public_name)));
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "读取模型目录失败");
    }
  }, [accountId, toast]);

  useEffect(() => {
    if (open) void load();
  }, [open, load]);

  const managed = account?.auto_sync ?? false;

  const apply = async (force: boolean) => {
    if (!accountId) return;
    setBusy(true);
    try {
      const result = await api.selectAccountModels(accountId, [...checked], force);
      if ("warnings" in result) {
        setPendingRemove(result.warnings);
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
      const entries = await api.refreshAccountModels(accountId);
      setModels(entries);
      // 选择集仍是权威状态；新出现的默认不勾（§16.2）。
      setChecked(new Set(entries.filter((m) => m.selected).map((m) => m.public_name)));
      knownNew.current = new Set(entries.filter((m) => m.is_new).map((m) => m.upstream_model));
      setOnlyNew(true);
      toast.success(`拉取到 ${entries.length} 个模型`);
    } catch (cause) {
      // §16.1：失败保留原目录，只提示错误，不弹新对话框。
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

  const toggleAlias = async (alias: Alias) => {
    if (!accountId) return;
    const next = aliases.some(
      (a) => a.upstream_model === alias.upstream_model && a.public_name === alias.public_name,
    )
      ? aliases.filter(
          (a) =>
            !(
              a.upstream_model === alias.upstream_model &&
              a.public_name === alias.public_name
            ),
        )
      : [...aliases, alias];
    try {
      await api.updateAliases(accountId, next);
      setAliases(next);
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "别名保存失败");
    }
  };

  const visible = useMemo(
    () => (onlyNew ? models.filter((m) => m.is_new) : models),
    [models, onlyNew],
  );
  const selectedCount = checked.size;

  return (
    <>
      <Modal
        open={open}
        onClose={onClose}
        title={account ? `模型 · ${account.name}` : "模型"}
        footer={
          managed ? undefined : (
            <>
              <span className="text-faint" style={{ fontSize: 12 }}>
                已选 {selectedCount} / 共 {models.length}
              </span>
              <div className="spacer" />
              <Button onClick={onClose}>取消</Button>
              <Button variant="primary" onClick={() => void apply(false)} disabled={busy}>
                {busy ? "应用中…" : "应用选择"}
              </Button>
            </>
          )
        }
      >
        {managed ? (
          <div className="stack" style={{ gap: 12 }}>
            <p style={{ margin: 0, lineHeight: 1.7 }}>
              该账号已开启<b>模型自动同步</b>，全部上游模型由后台托管，忽略选择集。
            </p>
            <Button
              variant="primary"
              icon={<IconRefresh size={13} />}
              onClick={async () => {
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
              }}
              disabled={busy}
            >
              立即同步
            </Button>
          </div>
        ) : (
          <div className="stack" style={{ gap: 12 }}>
            <div className="row" style={{ gap: 8 }}>
              <Button
                variant="primary"
                icon={<IconRefresh size={13} />}
                onClick={() => void fetchUpstream()}
                disabled={fetching}
              >
                {fetching ? "拉取中…" : "获取模型"}
              </Button>
              <label className="row" style={{ gap: 5, fontSize: 13 }}>
                <input
                  type="checkbox"
                  checked={onlyNew}
                  onChange={(e) => setOnlyNew(e.target.checked)}
                  disabled={models.every((m) => !m.is_new)}
                />
                只看新增
              </label>
            </div>

            {models.length === 0 ? (
              <p className="text-faint" style={{ margin: 0 }}>
                还没有模型目录。点击「获取模型」从上游拉取，或手动输入上游模型名。
              </p>
            ) : (
              <div className="table-wrap" style={{ maxHeight: 320, overflowY: "auto" }}>
                <table className="data">
                  <tbody>
                    {visible.map((model) => {
                      const tag = stateLabel(model);
                      const aliased = model.upstream_model !== model.public_name;
                      return (
                        <tr key={model.upstream_model}>
                          <td style={{ width: 30 }}>
                            <input
                              type="checkbox"
                              checked={checked.has(model.public_name)}
                              disabled={model.missing}
                              onChange={(e) => {
                                const next = new Set(checked);
                                if (e.target.checked) {
                                  next.add(model.public_name);
                                } else {
                                  next.delete(model.public_name);
                                }
                                setChecked(next);
                              }}
                            />
                          </td>
                          <td>
                            <div className="cell-strong mono" style={{ fontSize: 13 }}>
                              {model.public_name}
                            </div>
                            {aliased && (
                              <div className="text-faint mono" style={{ fontSize: 11.5 }}>
                                ← {model.upstream_model}
                              </div>
                            )}
                          </td>
                          <td style={{ width: 90 }}>
                            {tag && <Badge tone={tag.tone}>{tag.text}</Badge>}
                          </td>
                          <td style={{ width: 70 }}>
                            <AliasToggle model={model} onToggle={toggleAlias} />
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </div>
            )}

            <div className="row" style={{ gap: 8 }}>
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
          </div>
        )}
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
    </>
  );
}

/** 别名单元格：把对外名改成别名时写入账号别名表（§16.4）。 */
function AliasToggle({
  model,
  onToggle,
}: {
  model: AccountModel;
  onToggle: (alias: Alias) => Promise<void>;
}) {
  const [editing, setEditing] = useState(false);
  const [value, setValue] = useState("");
  const toast = useToast();

  if (!editing) {
    return (
      <button
        className="btn btn-ghost btn-sm"
        title="设置对外名别名"
        onClick={() => {
          setValue(model.public_name);
          setEditing(true);
        }}
      >
        别名
      </button>
    );
  }

  return (
    <div className="row" style={{ gap: 4 }}>
      <input
        className="input mono"
        style={{ height: 26, fontSize: 12 }}
        value={value}
        autoFocus
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            setEditing(false);
          }
        }}
      />
      <Button
        size="sm"
        variant="primary"
        onClick={() => {
          const name = value.trim();
          if (!name || name === model.upstream_model) {
            setEditing(false);
            return;
          }
          if (name !== model.public_name && name.length > 100) {
            toast.error("对外名不能超过 100 个字符");
            return;
          }
          void onToggle({ upstream_model: model.upstream_model, public_name: name }).then(
            () => setEditing(false),
          );
        }}
      >
        ✓
      </Button>
      <Button
        size="sm"
        variant="danger"
        icon={<IconTrash size={12} />}
        onClick={() => setEditing(false)}
      />
    </div>
  );
}
