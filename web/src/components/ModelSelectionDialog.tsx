/**
 * 账号模型管理对话框。
 *
 * 这是“下游能看到什么模型”的唯一配置入口：
 * - 「下游模型名」留空 = 使用上游原名；
 * - 填写后 = 下游用这个名称调用；
 * - 多个账号填成同一个名称，会自动合并为一个模型；
 * - 账号级「隐藏原始模型名」打开后，只暴露填写了下游模型名的模型。
 *
 * 交互上只有三条规矩，但都是必需的：
 * 1. 勾选 / 改名 / 删除只改**本地草稿**，点右下角「保存」才合并成**一次**
 *    `/models/apply`。逐行 POST 会让服务端每次勾选都重调和一遍目标、重建一遍
 *    配置快照，几百个模型时界面就会卡住（`/models/apply` 一次请求只调和一遍）。
 * 2. 行组件用 `memo` 包起来，勾一个复选框只重渲染那一行，而不是整张表。
 * 3. **最左列是选择列，不是启用列**：勾上它只进入“已选 N 项”，供批量启用 /
 *    批量停用 / 批量删除使用；单个模型的启用与停用由「状态」列里那个可点击的
 *    状态徽标承担。选择是纯界面状态、不进草稿——所以“全选”之后一定跟着一排
 *    批量操作，不会出现“勾完不知道能干什么”。
 *
 * 删除**不再弹二次确认**：它和勾选一样只是草稿，点「保存」之前那一行都还在
 * 服务端的目录里，反悔可以点「撤销移除」。真正会被拦下的是“最近 24 小时有
 * 流量的模型被停用 / 删除”，那是服务端 409 的安全阀，不是本地确认框。
 *
 * 为什么不做"防抖自动提交"（§16.8 的 2026-09-29 修订）：连点复选框时每隔
 * 350ms 就会发一次写请求，而每次写都会推进配置版本、重建配置快照。列表越长
 * 越卡之外，还会莫名弹出"配置已被其他会话修改"——那多半不是真有第二个人在改，
 * 而是自己上一次写还在途、或者后台任务（倍率刷新、托管同步）刚推进了版本。
 * 现在写只发生在用户明确点「保存」的那一刻，底部状态行也会明说"有几项未保存"。
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
  /** 保存进行中：底部用它显示“保存中…”，同时锁住行内操作。 */
  const [saving, setSaving] = useState(false);
  /** 草稿行数：0 表示没有未保存的改动（「保存」按钮与关闭确认都看它）。 */
  const [draftCount, setDraftCount] = useState(0);
  const [query, setQuery] = useState("");
  const [onlyEnabled, setOnlyEnabled] = useState(false);
  const [notice, setNotice] = useState<string | null>(null);

  const [editing, setEditing] = useState<string | null>(null);
  const [editAlias, setEditAlias] = useState("");
  const [adding, setAdding] = useState(false);
  const [newUpstream, setNewUpstream] = useState("");
  const [newAlias, setNewAlias] = useState("");
  /** 有流量模型被停用/删除时的二次确认（§16.3，服务端 409 驱动，不是本地确认框）。 */
  const [confirmWarnings, setConfirmWarnings] = useState<SelectionWarning[] | null>(null);
  /** 左列选择集：纯界面状态，只服务批量操作，**不进草稿**。 */
  const [selection, setSelection] = useState<ReadonlySet<string>>(() => new Set());
  /** 本次草稿里被移除的行，供「撤销移除」用（删除不再弹二次确认）。 */
  const [removedRows, setRemovedRows] = useState<AccountModel[]>([]);

  /** 本地草稿：上游模型名 → 待保存的改动。只有点「保存」时才提交。 */
  const pending = useRef<Map<string, PendingChange>>(new Map());
  const onChangedRef = useRef(onChanged);
  onChangedRef.current = onChanged;
  /** 当前账号 ID 的即时副本：慢响应回来时用它判断是否还有效。 */
  const accountIdRef = useRef(accountId);
  accountIdRef.current = accountId;
  /** 当前账号的即时副本：打开 / 换账号的副作用只认 ID，不认对象引用。 */
  const accountRef = useRef(account);
  accountRef.current = account;
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
    const current = accountRef.current;
    if (!current) return;
    // 未分配账号没有分组，也就没有"同组已有模型"可以挑（§4.2.3）。
    if (current.group_id === null) {
      setGroupModels([]);
      return;
    }
    try {
      const result = await api.availableGroupModels(current.group_id);
      setGroupModels(result.models);
    } catch {
      setGroupModels([]);
    }
    // 依赖留空：它只该在"打开弹窗 / 换账号"时跑一次。跟着 account 对象引用
    // 重跑会让数据刷新连带清掉用户正在改的草稿。
  }, []);

  /** 提交函数的即时副本：版本冲突的自动重试要重新走一遍 `commitDraft`。 */
  const commitDraftRef = useRef<(force?: boolean) => Promise<boolean>>(async () => true);

  /**
   * 把草稿合并成**一次** `/models/apply` 提交。返回是否成功（没有草稿也算成功）。
   *
   * 调用点只有三处：
   * 1. 用户点「保存」（`force = false`；停用有流量的模型由服务端 409 拦下再确认）；
   * 2. 二次确认之后带 `force = true` 重发；
   * 3. 「获取上游模型 / 添加模型 / 切换隐藏原始模型名」之前——这些动作会重写
   *    服务端目录，草稿不先落库就会被回来的新目录整体覆盖。它们拿到 `false`
   *    就中止，绝不带着"已经保存好了"的错觉继续。
   */
  const commitDraft = useCallback(
    async (force = false) => {
      const changes: ModelChange[] = [...pending.current.entries()].map(
        ([upstream_model, change]) => ({ upstream_model, ...change }),
      );
      if (changes.length === 0) return true;
      // 提交期间行内操作是锁住的（见 `rowBusy`），所以这里可以先清空草稿：
      // 失败时再整体放回去，不会出现"提交到一半又攒进新改动"的交错。
      pending.current.clear();
      setDraftCount(0);
      // 请求发出后账号可能已经被切走：回来的目录只属于发出它的那个账号。
      const requestedFor = accountId;
      setSaving(true);
      try {
        const list = await api.applyAccountModels(requestedFor, changes, force);
        if (accountIdRef.current === requestedFor) setRows(list);
        if (confirmWarningsRef.current !== null) setConfirmWarnings(null);
        setRemovedRows([]);
        toast.success(`已保存 ${changes.length} 项模型改动`);
        // 写已经成功了：刷新数据失败不能算作这次保存失败，否则下面的 catch 会把
        // 存好的改动又塞回草稿，用户再点一次「保存」就是重复提交。
        //
        // 而且这里**不等**它：全局数据（调度目标 / 概览）在后台追上即可，
        // 让「保存中…」一直挂到 6 个列表请求回来，正是"保存模型卡一下"的
        // 来源。对话框自己的目录已经用写接口返回的真值更新过了。
        void onChangedRef.current().catch(() => undefined);
        return true;
      } catch (cause) {
        // 失败一律把改动放回草稿：用户刚点的是「保存」，界面不能在这个时候
        // 把他做的选择悄悄抹掉——那正是旧版本最糟的一种表现。
        if (accountIdRef.current === requestedFor) {
          for (const change of changes) {
            pending.current.set(change.upstream_model, {
              ...(pending.current.get(change.upstream_model) ?? {}),
              ...change,
            });
          }
          setDraftCount(pending.current.size);
        }
        // 409 有两种完全不同的含义，必须分开：
        //
        // - `config_conflict`：乐观锁拦下的并发写（多半是后台任务刚推进过配置
        //   版本，或者上一次自己的写在途）。它不是"有流量的模型要确认"，
        //   把它当成后者会弹出一个**一条警告都没有**的确认框——用户只能反复
        //   点"仍然停用"，而每次都会再撞一次。正确做法是重新拉一次目录拿到
        //   新版本，然后自动重试一次；这条路径自带重试，所以它不触发全局的
        //   "配置已被其他会话修改"弹窗（见 `api.applyAccountModels`）。
        // - 其余 409 才是"以下模型最近 24 小时有流量"的二次确认。
        if (cause instanceof ApiError && cause.status === 409 && cause.configConflict) {
          await load();
          if (!conflictRetried.current) {
            conflictRetried.current = true;
            // 等这次提交的 `setSaving(false)` 落定再重试，避免紧挨着的又一次
            // 请求仍带着同一个过期版本号。
            window.setTimeout(() => void commitDraftRef.current(force), 0);
          } else {
            toast.error("配置刚被其他会话改过，你的改动还留在这个弹窗里；请确认目录后再点「保存」");
          }
          return false;
        }
        if (cause instanceof ApiError && cause.status === 409) {
          // 停用有流量的模型：先让用户确认。改动已经在草稿里，确认后原样重发。
          const payload = cause.payload as
            | { warnings?: SelectionWarning[]; needs_confirm?: SelectionWarning[] }
            | null;
          const warnings = payload?.warnings ?? payload?.needs_confirm ?? [];
          // 409 不只有"有流量要确认"一种：托管账号（auto_sync）也返回 409，
          // 但**没有 warnings**。把它也当成二次确认，用户就会看到一个一条警告
          // 都没有的确认框，点"仍然停用"再撞一次同样的 409，永远出不去。
          if (warnings.length > 0) {
            setConfirmWarnings(warnings);
          } else {
            toast.error(cause.message);
          }
          return false;
        }
        toast.error(cause instanceof Error ? cause.message : "保存失败");
        return false;
      } finally {
        setSaving(false);
      }
    },
    [accountId, load, toast],
  );
  commitDraftRef.current = commitDraft;

  /**
   * 记下一行本地草稿。**不发任何请求**：写只发生在点「保存」的那一刻（§16.8）。
   */
  const queueDraft = useCallback((upstreamModel: string, change: PendingChange) => {
    pending.current.set(upstreamModel, {
      ...(pending.current.get(upstreamModel) ?? {}),
      ...change,
    });
    setDraftCount(pending.current.size);
    // 用户又动手了：这一批改动值得再获得一次自动重试。
    conflictRetried.current = false;
  }, []);

  /**
   * 打开 / 换账号：清空草稿并从服务端读一份新目录。
   *
   * 草稿是按上游模型名索引的，串到别的账号就是一次错误的写入，所以这里必须
   * 先丢掉它。**依赖里只有账号 ID，没有 account 对象**：数据刷新会给父级一个
   * 新的对象引用，跟着它重跑会把用户正在改的草稿清掉。
   *
   * 关闭时同样清空：Modal 的 `dirty` 守卫已经在放弃之前问过用户了。
   */
  const forgetDraft = useCallback(() => {
    pending.current.clear();
    setDraftCount(0);
    setSelection(new Set());
    setRemovedRows([]);
  }, []);

  useEffect(() => {
    forgetDraft();
    if (!open) return;
    setQuery("");
    setOnlyEnabled(false);
    setEditing(null);
    setEditAlias("");
    setAdding(false);
    setNewUpstream("");
    setNewAlias("");
    setNotice(null);
    setConfirmWarnings(null);
    conflictRetried.current = false;
    setHideOriginal(accountRef.current?.hide_original ?? false);
    void load();
    void loadGroupNames();
  }, [open, accountId, forgetDraft, load, loadGroupNames]);

  /**
   * 弹窗还在、但组件被整个卸载时（浏览器后退 / ⌘K 跳转 / 切页）的兜底补交。
   *
   * `Modal` 的 dirty 守卫只覆盖它自己的关闭动作（X / Esc / 点遮罩）。hash 路由
   * 一变化，`Accounts` 连同这个弹窗一起被卸载，`pending` ref 随之销毁——既没有
   * 弹窗也没有补交，用户的勾选与改名就无声消失了。v1.1.14 在这里是"尽力补交"，
   * 不能反而退化成"保证不保存"。
   *
   * 只在**卸载**时补交，正常关闭仍走「保存」/脏数据确认：那时组件还活着，
   * 走这里会把用户明确放弃的改动又写回去。
   */
  useEffect(() => {
    return () => {
      const changes: ModelChange[] = [...pending.current.entries()].map(
        ([upstream_model, change]) => ({ upstream_model, ...change }),
      );
      if (changes.length === 0) return;
      pending.current.clear();
      // 卸载后没有人能看 toast，失败只能记日志：这条路已经是"无路可退"的兜底。
      void api
        .applyAccountModels(accountIdRef.current, changes, false)
        .catch((cause) => {
          console.warn("卸载时补交模型改动失败", cause);
        });
    };
  }, []);

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

  /** 当前列表里可操作的行：上游已消失的行不能选、也不能批量改。 */
  const selectable = useMemo(() => visible.filter((row) => !row.missing), [visible]);
  /**
   * 选择列的实际内容。
   *
   * 选择集按上游模型名存，但渲染与批量操作都只认“现在还在列表里”的行——
   * 被删掉的行即使名字还留在选择集里也不该再算数。
   */
  const selectedRows = useMemo(
    () => selectable.filter((row) => selection.has(row.upstream_model)),
    [selectable, selection],
  );
  const selectedNames = useMemo(
    () => new Set(selectedRows.map((row) => row.upstream_model)),
    [selectedRows],
  );
  const allVisibleSelected = selectable.length > 0 && selectedRows.length === selectable.length;
  /**
   * 有服务端动作在途：行内的勾选 / 改名 / 删除一律锁住。
   *
   * 一次「保存」必须是原子的——提交的内容和草稿不能交错，否则回来的是服务端的
   * 新目录，用户在这中间点的几下就会被悄悄吞掉。
   */
  const rowBusy = saving || fetching || busy !== null;

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
  /** 被移除的行数：底部状态行与「撤销移除」都看它。 */
  const removedCount = removedRows.length;
  /** 这些移除会连带撤下多少个调度目标：状态行用它把影响说清楚。 */
  const removedTargets = useMemo(() => {
    if (removedRows.length === 0) return 0;
    const names = new Set(removedRows.map((row) => row.upstream_model));
    return data.targets.filter(
      (target) => target.account_id === accountId && names.has(target.upstream_model),
    ).length;
  }, [removedRows, data.targets, accountId]);

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
    queueDraft(editing, { alias });
    cancelEdit();
    setNotice(
      alias
        ? `已把「${editing}」的下游模型名设为「${alias}」，点「保存」后生效。`
        : `已清空「${editing}」的下游模型名，这个模型会使用上游原名；点「保存」后生效。`,
    );
  }, [account, editing, editAlias, hideOriginal, queueDraft, cancelEdit]);

  const saveHideOriginal = async (next: boolean) => {
    if (!account) return;
    setBusy("hide");
    try {
      // 这个开关是账号级设置，单独立即生效；但账号目录必须先落库——下面会用
      // 服务端返回的目录整体替换本地列表，草稿不落库就等于被丢掉。
      if (!(await commitDraft())) return;
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
      queueDraft(upstreamModel, { selected });
    },
    [account, queueDraft],
  );

  /**
   * 批量把若干行设成同一个启用状态。
   *
   * 走的是和单个状态切换完全相同的草稿路径（同一个 pending 映射），点「保存」
   * 才提交；一次批量动作只推进一次草稿计数，不额外发任何请求。
   */
  const setRowsEnabled = useCallback(
    (names: ReadonlySet<string>, selected: boolean) => {
      if (names.size === 0) return;
      setRows((current) =>
        current.map((row) => (names.has(row.upstream_model) ? { ...row, selected } : row)),
      );
      let touched = false;
      for (const row of rows) {
        if (!names.has(row.upstream_model) || row.selected === selected) continue;
        pending.current.set(row.upstream_model, {
          ...(pending.current.get(row.upstream_model) ?? {}),
          selected,
        });
        touched = true;
      }
      if (!touched) return;
      setDraftCount(pending.current.size);
      conflictRetried.current = false;
    },
    [rows],
  );

  /** 单独勾选左列的选择框。纯界面状态，不进草稿、不推进配置版本。 */
  const toggleSelected = useCallback((upstreamModel: string, on: boolean) => {
    setSelection((current) => {
      const next = new Set(current);
      if (on) next.add(upstreamModel);
      else next.delete(upstreamModel);
      return next;
    });
  }, []);

  /** 表头复选框：全选 / 取消全选**当前列表里的行**（受搜索与筛选影响）。 */
  const selectAllVisible = useCallback(
    (on: boolean) => {
      setSelection((current) => {
        const next = new Set(current);
        for (const row of selectable) {
          if (on) next.add(row.upstream_model);
          else next.delete(row.upstream_model);
        }
        return next;
      });
    },
    [selectable],
  );

  const clearSelection = useCallback(() => setSelection(new Set()), []);

  /**
   * 移除若干行。**只记草稿、不弹二次确认**：点「保存」之前这些行都还在服务端
   * 的目录里，反悔可以点「撤销移除」，或者直接关掉弹窗放弃整批改动。
   */
  const removeRows = useCallback((rowsToRemove: AccountModel[]) => {
    if (rowsToRemove.length === 0) return;
    const names = new Set(rowsToRemove.map((row) => row.upstream_model));
    setRows((current) => current.filter((row) => !names.has(row.upstream_model)));
    setRemovedRows((current) => [
      ...current.filter((row) => !names.has(row.upstream_model)),
      ...rowsToRemove,
    ]);
    setSelection((current) => {
      const next = new Set(current);
      for (const name of names) next.delete(name);
      return next;
    });
    for (const name of names) {
      pending.current.set(name, {
        ...(pending.current.get(name) ?? {}),
        delete: true,
      });
    }
    setDraftCount(pending.current.size);
    conflictRetried.current = false;
    setNotice(
      rowsToRemove.length === 1
        ? `已从目录移除「${rowsToRemove[0]!.upstream_model}」，点「保存」后生效；反悔可以点「撤销移除」。`
        : `已从目录移除 ${rowsToRemove.length} 个模型，点「保存」后生效；反悔可以点「撤销移除」。`,
    );
  }, []);

  /** 撤销本次草稿里的全部移除：行回到列表，草稿里的 delete 标记一并清掉。 */
  const undoRemove = useCallback(() => {
    if (removedRows.length === 0) return;
    const restored = removedRows;
    setRows((current) => [...current, ...restored]);
    for (const row of restored) {
      const change = pending.current.get(row.upstream_model);
      if (!change) continue;
      if (change.alias === undefined && change.selected === undefined) {
        pending.current.delete(row.upstream_model);
      } else {
        delete change.delete;
        pending.current.set(row.upstream_model, change);
      }
    }
    setRemovedRows([]);
    setDraftCount(pending.current.size);
    setNotice(`已恢复 ${restored.length} 个模型。`);
  }, [removedRows]);

  const refreshUpstream = async () => {
    if (!account) return;
    setFetching(true);
    setNotice(null);
    try {
      // 拉取会用上游目录 + 服务端的选择集重新合并出一份目录，草稿必须先落库。
      if (!(await commitDraft())) return;
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
      if (!(await commitDraft())) return;
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
        // 关闭前不补交草稿：写只发生在「保存」。带着未保存的改动关窗时，
        // Modal 的 dirty 守卫会先问一次"放弃未保存的修改？"。
        onClose={onClose}
        // 草稿之外，"正在输入但还没点「加入待保存」"的内容同样算脏：
        // 行内改名框与添加模型表单填了一半就关窗，输入会无声消失。
        dirty={
          draftCount > 0 ||
          (editing !== null && editAlias.trim() !== "") ||
          (adding && (newUpstream.trim() !== "" || newAlias.trim() !== ""))
        }
        title={account ? `模型管理 · ${account.name}` : "模型管理"}
        className="model-selection-modal"
        footer={
          <>
            <span className="text-faint tabular" style={{ fontSize: 12 }}>
              {loading
                ? "加载中…"
                : `${rows.length} 个模型 · ${enabledCount} 个启用 · ${renamedCount} 个已设下游名`}
              {blockedCount > 0 && ` · ${blockedCount} 个未设下游名且被隐藏`}
              {removedCount > 0 &&
                ` · 已移除 ${removedCount} 个模型（连带撤下 ${removedTargets} 个调度目标）`}
            </span>
            {removedCount > 0 && (
              <Button size="sm" variant="ghost" onClick={undoRemove} disabled={rowBusy}>
                撤销移除
              </Button>
            )}
            {draftCount > 0 && <span className="dirty-badge">{draftCount} 项未保存</span>}
            <div className="spacer" />
            <Button
              variant="primary"
              onClick={() => void commitDraft()}
              // managed 时服务端一律拒绝（自动同步接管了目录），按钮可以点
              // 只是让用户白撞一次 409——账号可能在弹窗开着的这段时间里被
              // 另一处打开自动同步，所以这个判断必须放在渲染里而不是打开时。
              disabled={draftCount === 0 || rowBusy || managed}
              title={draftCount === 0 ? "没有未保存的改动" : "保存本次改动"}
            >
              {saving ? "保存中…" : "保存"}
            </Button>
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

          {/* 选择列的落点：勾了行之后**一定**有动作可做，这正是「全选」的意义。
              批量动作和单个动作一样只改草稿，点「保存」才提交。 */}
          {selectedRows.length > 0 && (
            <div className="model-batch-bar" role="toolbar" aria-label="批量操作">
              <span className="tabular model-batch-count">已选 {selectedRows.length} 个模型</span>
              <Button
                size="sm"
                onClick={() => setRowsEnabled(selectedNames, true)}
                disabled={managed || rowBusy}
                title="把这批模型设为启用；点「保存」后生效"
              >
                批量启用
              </Button>
              <Button
                size="sm"
                onClick={() => setRowsEnabled(selectedNames, false)}
                disabled={managed || rowBusy}
                title="把这批模型设为停用；点「保存」后生效"
              >
                批量停用
              </Button>
              <Button
                size="sm"
                variant="danger"
                icon={<IconTrash size={13} />}
                onClick={() => removeRows(selectedRows)}
                disabled={managed || rowBusy}
                title="把这批模型从目录移除会连带撤下它们的调度目标；点「保存」后生效，可撤销"
              >
                批量删除
              </Button>
              <span className="spacer" />
              <button type="button" className="link-button" onClick={clearSelection}>
                清除选择
              </button>
            </div>
          )}

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
                    <th className="model-checkbox-cell">
                      <input
                        type="checkbox"
                        checked={allVisibleSelected}
                        disabled={managed || rowBusy || selectable.length === 0}
                        aria-label="全选当前列表"
                        title={`全选 / 取消全选当前列表里的 ${selectable.length} 个模型（受搜索与「只看已启用」筛选影响）`}
                        onChange={(event) => selectAllVisible(event.target.checked)}
                      />
                    </th>
                    <th>上游模型名</th>
                    <th>下游模型名</th>
                    <th>下游可用名称</th>
                    <th>状态（点击切换）</th>
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
                          locked={rowBusy}
                          editAlias={isEditing ? editAlias : ""}
                          mergeSuggestions={isEditing ? mergeSuggestions : NO_NAMES}
                          existingNames={isEditing ? existingNames : NO_NAMES}
                          onEdit={beginEdit}
                          onCancelEdit={cancelEdit}
                          onAliasChange={setEditAlias}
                          onSave={saveEdit}
                          onToggle={toggleRow}
                          selected={selectedNames.has(row.upstream_model)}
                          onSelect={toggleSelected}
                          onRemove={removeRows}
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
          // 用户不愿意停用这些有流量的模型：丢掉这批草稿，回到服务端的目录真相。
          setConfirmWarnings(null);
          forgetDraft();
          void load();
        }}
        onConfirm={() => void commitDraft(true)}
      />
    </>
  );
}

/**
 * 表格里的一行（含展开的改名表单）。
 *
 * `memo` 是必需的：一个账号几百个模型时，勾一个选择框不该让整张表重渲染。
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
  locked,
  selected,
  editAlias,
  mergeSuggestions,
  existingNames,
  onEdit,
  onCancelEdit,
  onAliasChange,
  onSave,
  onToggle,
  onSelect,
  onRemove,
}: {
  row: AccountModel;
  renamed: boolean;
  names: string[];
  hiddenWithoutName: boolean;
  hideOriginal: boolean;
  isEditing: boolean;
  managed: boolean;
  /** 有服务端动作在途（保存 / 拉取 / 添加）：行内操作暂时锁住。 */
  locked: boolean;
  /** 是否被左列的选择框选中。选择只服务批量操作，与「启用 / 停用」是两回事。 */
  selected: boolean;
  editAlias: string;
  mergeSuggestions: string[];
  existingNames: string[];
  onEdit: (upstreamModel: string) => void;
  onCancelEdit: () => void;
  onAliasChange: (value: string) => void;
  onSave: () => void;
  onToggle: (upstreamModel: string, selected: boolean) => void;
  onSelect: (upstreamModel: string, on: boolean) => void;
  onRemove: (rows: AccountModel[]) => void;
}) {
  return (
    <>
      <tr className={row.missing ? "is-missing" : undefined}>
        <td className="model-checkbox-cell">
          <input
            type="checkbox"
            checked={selected}
            disabled={managed || row.missing || locked}
            aria-label={`选择 ${row.upstream_model}`}
            title={
              row.missing
                ? "上游已消失，重新出现后会自动恢复"
                : "选中它，再用上方的「批量启用 / 批量停用 / 批量删除」"
            }
            onChange={(event) => onSelect(row.upstream_model, event.target.checked)}
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
            ) : (
              <button
                type="button"
                className={`model-state-toggle ${row.selected ? "is-on" : "is-off"}`}
                aria-pressed={row.selected}
                disabled={managed || locked}
                title={
                  locked
                    ? "正在与后台交互，稍后再改"
                    : "点击切换启用 / 停用（改完点右下角「保存」）"
                }
                onClick={() => onToggle(row.upstream_model, !row.selected)}
              >
                <span className="model-state-dot" aria-hidden="true" />
                {row.selected ? "已启用" : "已停用"}
              </button>
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
              disabled={managed || locked}
            >
              改名
            </Button>
            <Button
              size="sm"
              variant="danger"
              icon={<IconTrash size={13} />}
              title="从目录删除并移除目标"
              aria-label={`删除模型 ${row.upstream_model}`}
              onClick={() => onRemove([row])}
              disabled={managed || locked}
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
                  disabled={locked}
                >
                  加入待保存
                </Button>
                <Button size="sm" icon={<IconX size={13} />} onClick={onCancelEdit} disabled={locked}>
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
