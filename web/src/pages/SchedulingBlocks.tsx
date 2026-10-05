/**
 * 调度屏蔽页（§16.7、§23.5）。
 *
 * 这里列的是**进程内存里**的两类屏蔽：上游明确拒绝过的能力，以及被证实不存在
 * 的端点。两者都有 24 小时级别的存续期，都会让整类请求直接没有候选目标，却
 * 既不落库也不进配置——不摊开来看，管理员只能看到一句"模型没有可用的调度目标"。
 *
 * 存续期不等于"一定会挂满"：任何一次配置写入都会整体清空这两张表（见
 * `app::Runtime::retain`），进程重启同样清空。所以「放行」是给"配置没动、
 * 限制却卡着流量"这种情形准备的出口，只清内存状态、不动配置：下一次请求会
 * 重新向上游取证，真的不行还会再记一次。
 */
import { useCallback, useEffect, useState } from "react";
import { api } from "../lib/api";
import type { SchedulingBlocks as BlocksView } from "../lib/types";
import {
  Badge,
  Button,
  Card,
  ConfirmDialog,
  EmptyState,
  InfoTip,
  Skeleton,
  useToast,
} from "../components/ui";
import { IconAlert, IconRefresh } from "../components/Icons";
import { humanizeSeconds } from "../lib/format";

/** 能力词表的中文标签，与后端 `capability::specs` 对齐；未知名字原样显示。 */
const CAPABILITY_LABELS: Record<string, string> = {
  function_calling: "工具调用",
  forced_tool_choice: "强制工具选择",
  vision: "图片 / 文档输入",
  response_schema: "结构化输出",
  reasoning: "思考",
};

function capabilityLabel(name: string): string {
  return CAPABILITY_LABELS[name] ?? name;
}

export function SchedulingBlocks() {
  const toast = useToast();
  const [view, setView] = useState<BlocksView | null>(null);
  const [loading, setLoading] = useState(true);
  const [refreshing, setRefreshing] = useState(false);
  const [clearing, setClearing] = useState(false);
  const [confirmAll, setConfirmAll] = useState(false);

  const load = useCallback(async () => {
    setRefreshing(true);
    try {
      setView(await api.schedulingBlocks());
    } catch (error) {
      toast.error(error instanceof Error ? error.message : "读取调度屏蔽失败");
    } finally {
      setLoading(false);
      setRefreshing(false);
    }
  }, [toast]);

  useEffect(() => {
    void load();
  }, [load]);

  const clear = useCallback(
    async (
      payload: Parameters<typeof api.clearSchedulingBlocks>[0],
      describe: string,
    ) => {
      setClearing(true);
      try {
        const result = await api.clearSchedulingBlocks(payload);
        const cleared = result.capabilities_cleared + result.evidence_cleared;
        toast.success(`已放行 ${describe}（${cleared} 条）：下一次请求会重新向上游取证`);
        await load();
      } catch (error) {
        toast.error(error instanceof Error ? error.message : "放行失败");
      } finally {
        setClearing(false);
      }
    },
    [load, toast],
  );

  const capabilities = view?.capabilities ?? [];
  const evidence = view?.evidence ?? [];
  const empty = capabilities.length === 0 && evidence.length === 0;

  return (
    <Card
      title="调度屏蔽"
      description="上游明确拒绝过的能力，以及被证实不存在的端点。它们只在内存里，重启即清空。"
      actions={
        <Button
          size="sm"
          variant="ghost"
          icon={refreshing ? <span className="spinner spinner-sm" /> : <IconRefresh size={14} />}
          onClick={() => void load()}
          disabled={refreshing}
          title="重新读取调度屏蔽"
        >
          刷新
        </Button>
      }
    >
      {loading ? (
        <Skeleton rows={3} />
      ) : empty ? (
        <EmptyState
          icon={<IconAlert size={19} />}
          title="当前没有被屏蔽的组合"
          description="这里为空是正常状态：说明没有账号因为上游明确拒绝而被打上能力限制，也没有端点被证实不存在。"
          action={
            <Button variant="secondary" onClick={() => void load()}>
              重新读取
            </Button>
          }
        />
      ) : (
        <div className="stack" style={{ gap: 16 }}>
          <div className="callout callout-warn">
            <span style={{ flex: 1 }}>
              被屏蔽的组合在存续期内会<b>直接不合格</b>：带这类能力的请求不会派给它，
              其余目标都不可用时就会得到"当前没有可用的调度目标"。放行只清这里的状态，
              不会改动账号或目标配置。
            </span>
            <InfoTip label="为什么会有屏蔽">
              上游明确回答"这个模型不支持某项能力"时，网关记下证据并按词表避开这个
              「账号 × 模型 × 能力」组合。归因取**最接近拒绝措辞**的那项能力，并按
              本次请求真正用到的能力过滤，所以"不支持强制工具选择"不会连坐普通工具
              调用。窄能力（如强制工具选择）只影响那一种请求形状，存续期也短；
              宽能力（如工具调用）影响整类流量。证据要攒够次数才生效。
            </InfoTip>
          </div>

          <div className="row" style={{ gap: 8, flexWrap: "wrap" }}>
            <Button
              variant="secondary"
              size="sm"
              disabled={clearing}
              onClick={() => setConfirmAll(true)}
            >
              全部放行（{capabilities.length + evidence.length} 条）
            </Button>
          </div>

          <section className="stack" style={{ gap: 8 }}>
            <div className="row" style={{ justifyContent: "space-between", alignItems: "baseline" }}>
              <h3 style={{ margin: 0, fontSize: 14 }}>能力限制（{capabilities.length}）</h3>
              <span className="card-desc" style={{ margin: 0 }}>
                证据攒够次数才生效；未生效的条目也会列出来，方便提前干预。
              </span>
            </div>
            {capabilities.length === 0 ? (
              <p className="card-desc" style={{ margin: 0 }}>没有能力限制。</p>
            ) : (
              <div className="table-wrap">
                <table className="data">
                  <thead>
                    <tr>
                      <th>账号</th>
                      <th>模型</th>
                      <th>能力</th>
                      <th>证据</th>
                      <th>状态</th>
                      <th>剩余</th>
                      <th />
                    </tr>
                  </thead>
                  <tbody>
                    {capabilities.map((block) => (
                      <tr key={`${block.account_id}|${block.model}|${block.capability}`}>
                        <td title={block.account_id}>{block.account_name}</td>
                        <td className="mono">{block.model}</td>
                        <td title={block.capability}>{capabilityLabel(block.capability)}</td>
                        <td className="mono">
                          {block.strikes}/{block.required_strikes}
                        </td>
                        <td>
                          {block.effective ? (
                            <Badge tone="danger">生效中</Badge>
                          ) : (
                            <Badge tone="warn">取证中</Badge>
                          )}
                        </td>
                        <td className="mono">
                          {block.effective ? humanizeSeconds(block.expires_in_secs) : "—"}
                        </td>
                        <td>
                          <Button
                            size="sm"
                            variant="secondary"
                            disabled={clearing}
                            onClick={() =>
                              void clear(
                                {
                                  scope: "capability",
                                  account_id: block.account_id,
                                  model: block.model,
                                  capability: block.capability,
                                },
                                `${block.account_name} · ${block.model} · ${capabilityLabel(block.capability)}`,
                              )
                            }
                          >
                            放行
                          </Button>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </section>

          <section className="stack" style={{ gap: 8 }}>
            <div className="row" style={{ justifyContent: "space-between", alignItems: "baseline" }}>
              <h3 style={{ margin: 0, fontSize: 14 }}>端点缺失证据（{evidence.length}）</h3>
              <span className="card-desc" style={{ margin: 0 }}>
                判据是上游明确回答"这条路由不存在"；普通 400 / 5xx / 超时不会记进来。
              </span>
            </div>
            {evidence.length === 0 ? (
              <p className="card-desc" style={{ margin: 0 }}>没有端点缺失证据。</p>
            ) : (
              <div className="table-wrap">
                <table className="data">
                  <thead>
                    <tr>
                      <th>账号</th>
                      <th>端点</th>
                      <th>剩余</th>
                      <th />
                    </tr>
                  </thead>
                  <tbody>
                    {evidence.map((block) => (
                      <tr key={`${block.account_id}|${block.endpoint}`}>
                        <td title={block.account_id}>{block.account_name}</td>
                        <td className="mono">{block.endpoint}</td>
                        <td className="mono">{humanizeSeconds(block.expires_in_secs)}</td>
                        <td>
                          <Button
                            size="sm"
                            variant="secondary"
                            disabled={clearing}
                            onClick={() =>
                              void clear(
                                { scope: "evidence", account_id: block.account_id, endpoint: block.endpoint },
                                `${block.account_name} · ${block.endpoint}`,
                              )
                            }
                          >
                            放行
                          </Button>
                        </td>
                      </tr>
                    ))}
                  </tbody>
                </table>
              </div>
            )}
          </section>
        </div>
      )}

      <ConfirmDialog
        open={confirmAll}
        title="放行全部调度屏蔽？"
        message="所有账号的能力限制与端点证据会被清空。下一次请求会重新向上游取证；如果上游仍然拒绝，限制会按同样的规则重新记上。这不会改动账号或目标配置。"
        confirmLabel="全部放行"
        onClose={() => setConfirmAll(false)}
        onConfirm={() => {
          void clear({ scope: "all" }, "全部屏蔽");
        }}
      />
    </Card>
  );
}
