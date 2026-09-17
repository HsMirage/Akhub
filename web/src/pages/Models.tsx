/** 逻辑模型：下游看到的名称。 */
import { useMemo, useState } from "react";
import { api } from "../lib/api";
import type { AvailableModel, LogicalModel } from "../lib/types";
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
import { IconCube, IconEdit, IconPlus, IconRefresh, IconTrash } from "../components/Icons";

export function Models({ data, refresh }: { data: Data; refresh: () => Promise<void> }) {
  const toast = useToast();
  const [creating, setCreating] = useState(false);
  const [editing, setEditing] = useState<LogicalModel | null>(null);
  const [confirm, setConfirm] = useState<LogicalModel | null>(null);

  const groupName = (id: string) =>
    data.groups.find((group) => group.id === id)?.name ?? id;

  const remove = async (model: LogicalModel) => {
    try {
      await api.deleteModel(model.id);
      await refresh();
      toast.success(`逻辑模型「${model.name}」已删除`);
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "删除失败");
    }
  };

  const toggle = async (model: LogicalModel) => {
    try {
      await api.updateModel(model.id, { enabled: !model.enabled });
      await refresh();
      toast.success(model.enabled ? "已停用" : "已启用");
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "操作失败");
    }
  };

  return (
    <>
      <Card
        title="逻辑模型"
        description="下游只使用这里的名称，看不到真实上游、账号与倍率。"
        actions={
          <Button
            variant="primary"
            icon={<IconPlus />}
            onClick={() => setCreating(true)}
            disabled={data.groups.length === 0}
          >
            新建逻辑模型
          </Button>
        }
      >
        {data.groups.length === 0 ? (
          <EmptyState
            icon={<IconCube size={19} />}
            title="请先创建分组"
            description="逻辑模型属于某个分组，只对该分组的下游 Key 可见。"
          />
        ) : data.models.length === 0 ? (
          <EmptyState
            icon={<IconCube size={19} />}
            title="还没有逻辑模型"
            description="从分组的上游目录挑一个模型，或输入自定义对外名。下一步把它绑定到「账号 + 具体上游模型」，就能对外服务了。"
            action={
              <Button variant="primary" icon={<IconPlus />} onClick={() => setCreating(true)}>
                新建逻辑模型
              </Button>
            }
          />
        ) : (
          <div className="table-wrap">
            <table className="data">
              <thead>
                <tr>
                  <th>模型名</th>
                  <th>分组</th>
                  <th>来源</th>
                  <th>调度目标</th>
                  <th>/v1/models</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {data.models.map((model) => (
                  <tr key={model.id}>
                    <td className="cell-strong mono cell-truncate" title={model.name}>
                      {model.name}
                    </td>
                    <td className="cell-dim">{groupName(model.group_id)}</td>
                    <td className="cell-dim">
                      {model.origin === "auto" ? "自动创建" : "手工创建"}
                    </td>
                    <td className="tabular">
                      <Badge tone={model.dispatch_targets > 0 ? "success" : "warn"}>
                        {model.dispatch_targets}
                      </Badge>
                    </td>
                    <td>
                      {model.listed ? (
                        <Badge tone="success" dot>
                          已上架
                        </Badge>
                      ) : (
                        <Badge tone="neutral" dot>
                          {model.enabled ? "零目标未上架" : "已停用"}
                        </Badge>
                      )}
                    </td>
                    <td>
                      <div className="cell-actions">
                        <Button
                          size="sm"
                          icon={<IconEdit size={13} />}
                          onClick={() => setEditing(model)}
                        >
                          编辑
                        </Button>
                        <Button size="sm" onClick={() => void toggle(model)}>
                          {model.enabled ? "停用" : "启用"}
                        </Button>
                        <Button
                          size="sm"
                          variant="danger"
                          icon={<IconTrash size={13} />}
                          title="删除逻辑模型"
                          onClick={() => setConfirm(model)}
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

      <ModelCreateDrawer
        key={creating ? "create-open" : "create-closed"}
        data={data}
        open={creating}
        onClose={() => setCreating(false)}
        onCreated={async () => {
          setCreating(false);
          await refresh();
        }}
      />

      <ModelEditDrawer
        key={editing?.id ?? "edit-closed"}
        model={editing}
        data={data}
        open={editing !== null}
        onClose={() => setEditing(null)}
        onSaved={async () => {
          setEditing(null);
          await refresh();
        }}
      />

      <ConfirmDialog
        open={confirm !== null}
        title="删除逻辑模型"
        danger
        confirmLabel="删除"
        message={
          <>
            删除「{confirm?.name}」会同时移除它的 {confirm?.dispatch_targets ?? 0} 个调度目标，下游再请求该模型名会得到 404 model_not_found。
          </>
        }
        onClose={() => setConfirm(null)}
        onConfirm={() => void remove(confirm!)}
      />
    </>
  );
}

function ModelCreateDrawer({
  data,
  open,
  onClose,
  onCreated,
}: {
  data: Data;
  open: boolean;
  onClose: () => void;
  onCreated: () => void | Promise<void>;
}) {
  const toast = useToast();
  const [groupId, setGroupId] = useState(data.groups[0]?.id ?? "");
  const [name, setName] = useState("");
  const [available, setAvailable] = useState<AvailableModel[]>([]);
  const [search, setSearch] = useState("");
  const [loadingAvailable, setLoadingAvailable] = useState(false);
  const [availableMessage, setAvailableMessage] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const duplicate = data.models.some(
    (model) => model.group_id === groupId && model.name === name.trim(),
  );
  const filteredAvailable = useMemo(() => {
    const needle = search.trim().toLowerCase();
    if (!needle) return available;
    return available.filter(
      (model) =>
        model.public_name.toLowerCase().includes(needle) ||
        model.accounts.some((account) => account.toLowerCase().includes(needle)),
    );
  }, [available, search]);

  const changeGroup = (value: string) => {
    setGroupId(value);
    setAvailable([]);
    setSearch("");
    setAvailableMessage(null);
  };

  const fetchAvailable = async () => {
    if (!groupId) return;
    setLoadingAvailable(true);
    setAvailableMessage(null);
    try {
      const result = await api.availableGroupModels(groupId);
      setAvailable(result.models);
      setSearch("");
      setAvailableMessage(
        result.models.length === 0
          ? "这个分组当前没有可选模型，请手动输入自定义对外名。"
          : null,
      );
    } catch (cause) {
      setAvailable([]);
      setAvailableMessage(
        cause instanceof Error
          ? `拉取失败：${cause.message}。请直接手动输入自定义对外名。`
          : "拉取失败，请直接手动输入自定义对外名。",
      );
    } finally {
      setLoadingAvailable(false);
    }
  };

  const submit = async () => {
    if (!name.trim() || duplicate || !groupId) return;
    setBusy(true);
    try {
      await api.createModel({ group_id: groupId, name: name.trim() });
      setName("");
      toast.success("逻辑模型已创建");
      await onCreated();
    } catch (cause) {
      toast.error(cause instanceof Error ? cause.message : "创建失败");
    } finally {
      setBusy(false);
    }
  };

  return (
    <Drawer
      open={open}
      onClose={onClose}
      title="新建逻辑模型"
      description="先从分组的上游目录挑选公开名，也可以直接输入自定义对外名。"
      footer={
        <>
          <Button onClick={onClose}>取消</Button>
          <Button
            variant="primary"
            onClick={() => void submit()}
            disabled={busy || !name.trim() || duplicate || !groupId}
          >
            {busy ? <><span className="spinner spinner-sm" aria-hidden="true" /> 创建中…</> : "创建"}
          </Button>
        </>
      }
    >
      <div className="stack">
        <Field label="所属分组" hint="目录只会展示这个分组下账号提供的模型。">
          {(id) => (
            <select id={id} className="select" value={groupId} onChange={(e) => changeGroup(e.target.value)}>
              {data.groups.map((group) => (
                <option key={group.id} value={group.id}>{group.name}</option>
              ))}
            </select>
          )}
        </Field>

        <div className="model-picker">
          <div className="row model-picker-head">
            <div>
              <div className="field-label">上游目录</div>
              <div className="field-hint">点击模型会自动填入对外名。</div>
            </div>
            <Button
              variant="secondary"
              icon={loadingAvailable ? <span className="spinner spinner-sm" aria-hidden="true" /> : <IconRefresh size={13} />}
              onClick={() => void fetchAvailable()}
              disabled={loadingAvailable || !groupId}
            >
              {loadingAvailable ? "拉取中…" : "拉取可选模型"}
            </Button>
          </div>
          {available.length > 0 && (
            <div className="stack model-picker-list" style={{ gap: 8 }}>
              <input
                className="input"
                value={search}
                onChange={(e) => setSearch(e.target.value)}
                placeholder="搜索模型名或账号"
                aria-label="搜索可选模型"
              />
              <div className="model-picker-options" role="listbox" aria-label="可选模型">
                {filteredAvailable.length === 0 ? (
                  <div className="table-empty">没有匹配的模型</div>
                ) : (
                  filteredAvailable.map((model) => (
                    <button
                      type="button"
                      className={`model-picker-option${name === model.public_name ? " is-selected" : ""}`}
                      key={model.public_name}
                      onClick={() => setName(model.public_name)}
                      role="option"
                      aria-selected={name === model.public_name}
                    >
                      <span className="mono cell-truncate" title={model.public_name}>{model.public_name}</span>
                      <span className="model-picker-accounts" title={model.accounts.join("、")}>
                        {model.accounts.join("、")}
                      </span>
                    </button>
                  ))
                )}
              </div>
            </div>
          )}
          {availableMessage && <div className="callout callout-warn">{availableMessage}</div>}
        </div>

        <Field
          label="对外模型名"
          error={duplicate ? "该分组内已存在同名逻辑模型" : undefined}
          hint="下游客户端在请求体的 model 字段里填的就是这个名字；可不与上游名相同。"
        >
          {(id) => (
            <input
              id={id}
              className="input mono"
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="claude-sonnet-4-5"
            />
          )}
        </Field>
      </div>
    </Drawer>
  );
}

function ModelEditDrawer({
  model,
  data,
  open,
  onClose,
  onSaved,
}: {
  model: LogicalModel | null;
  data: Data;
  open: boolean;
  onClose: () => void;
  onSaved: () => void | Promise<void>;
}) {
  const toast = useToast();
  const [name, setName] = useState(model?.name ?? "");
  const [enabled, setEnabled] = useState(model?.enabled ?? true);
  const [busy, setBusy] = useState(false);
  const duplicate = model
    ? data.models.some(
        (candidate) =>
          candidate.id !== model.id &&
          candidate.group_id === model.group_id &&
          candidate.name === name.trim(),
      )
    : false;
  const renamed = Boolean(model && name.trim() !== model.name);

  const submit = async () => {
    if (!model || !name.trim() || duplicate) return;
    setBusy(true);
    try {
      await api.updateModel(model.id, { name: name.trim(), enabled });
      toast.success("逻辑模型已更新");
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
      title={model ? `编辑「${model.name}」` : "编辑逻辑模型"}
      description="名称和启用状态会在保存后对新请求立即生效。"
      footer={
        <>
          <Button onClick={onClose}>取消</Button>
          <Button variant="primary" onClick={() => void submit()} disabled={busy || !model || !name.trim() || duplicate}>
            {busy ? <><span className="spinner spinner-sm" aria-hidden="true" /> 保存中…</> : "保存"}
          </Button>
        </>
      }
    >
      <div className="stack">
        <Field label="对外模型名" error={duplicate ? "该分组内已存在同名逻辑模型" : undefined}>
          {(id) => (
            <input id={id} className="input mono" value={name} onChange={(e) => setName(e.target.value)} />
          )}
        </Field>
        {renamed && (
          <div className="callout callout-warn">
            改名后旧名字立即失效，客户端要改用新名字。
          </div>
        )}
        <Switch
          checked={enabled}
          onChange={setEnabled}
          label="启用逻辑模型"
          hint="停用后不会出现在 /v1/models，也不会接收新请求。"
        />
      </div>
    </Drawer>
  );
}
