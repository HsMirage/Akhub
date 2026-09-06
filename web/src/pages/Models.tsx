/** 逻辑模型：下游看到的名称。 */
import { useState } from "react";
import { api } from "../lib/api";
import type { LogicalModel } from "../lib/types";
import type { Data } from "../lib/store";
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
  Drawer,
  EmptyState,
  Field,
  useToast,
} from "../components/ui";
import { IconCube, IconPlus, IconTrash } from "../components/Icons";

export function Models({ data, refresh }: { data: Data; refresh: () => Promise<void> }) {
  const toast = useToast();
  const [creating, setCreating] = useState(false);
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
            description="填一个对外暴露的名字，例如 claude-sonnet-4-5。下一步把它绑定到「账号 + 具体上游模型」，就能对外服务了。"
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
                    <td className="cell-strong mono">{model.name}</td>
                    <td className="cell-dim">{groupName(model.group_id)}</td>
                    <td className="cell-dim">
                      {model.origin === "auto" ? "自动创建" : "手工创建"}
                    </td>
                    <td>
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

      <ModelDrawer
        data={data}
        open={creating}
        onClose={() => setCreating(false)}
        onCreated={async () => {
          setCreating(false);
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
            删除「{confirm?.name}」会同时移除它的 {confirm?.dispatch_targets ?? 0}{" "}
            个调度目标，下游再请求该模型名会得到 404 model_not_found。
          </>
        }
        onClose={() => setConfirm(null)}
        onConfirm={() => void remove(confirm!)}
      />
    </>
  );
}

function ModelDrawer({
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
  const [busy, setBusy] = useState(false);

  const duplicate = data.models.some(
    (model) => model.group_id === groupId && model.name === name.trim(),
  );

  const submit = async () => {
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
      description="手工创建的模型在零目标时保留记录，但会从 /v1/models 移除。"
      footer={
        <>
          <Button onClick={onClose}>取消</Button>
          <Button
            variant="primary"
            onClick={submit}
            disabled={busy || !name.trim() || duplicate}
          >
            {busy ? "创建中…" : "创建"}
          </Button>
        </>
      }
    >
      <div className="stack">
        <Field label="所属分组">
          {(id) => (
            <select
              id={id}
              className="select"
              value={groupId}
              onChange={(e) => setGroupId(e.target.value)}
            >
              {data.groups.map((group) => (
                <option key={group.id} value={group.id}>
                  {group.name}
                </option>
              ))}
            </select>
          )}
        </Field>

        <Field
          label="对外模型名"
          error={duplicate ? "该分组内已存在同名逻辑模型" : undefined}
          hint="下游客户端在请求体的 model 字段里填的就是这个名字。"
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
