/**
 * 成本页（§6.8）：以逻辑模型为单位组织，绝不跨模型加总。
 *
 * 倍率是折扣不是价格——跨模型相加会被模型组合严重扭曲。全局区域只显示
 * 不失真的量：请求总数与各账号占比。每个模型块同时给出"全用最便宜目标
 * 还能再省多少"：不中断服务的代价必须看得见。
 */
import { useCallback, useEffect, useState } from "react";
import { api } from "../lib/api";
import type { CostModel, CostView } from "../lib/types";
import type { Data } from "../lib/store";
import { Card, EmptyState, Skeleton } from "../components/ui";
import { IconGauge } from "../components/Icons";

const PERIODS: { value: "day" | "month"; label: string }[] = [
  { value: "day", label: "近 24 小时" },
  { value: "month", label: "本月" },
];

export function Cost({ data }: { data: Data }) {
  const [period, setPeriod] = useState<"day" | "month">("day");
  const [view, setView] = useState<CostView | null>(null);
  const [loading, setLoading] = useState(true);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      setView(await api.cost(period));
    } finally {
      setLoading(false);
    }
  }, [period]);

  useEffect(() => {
    void load();
  }, [load]);

  const groupName = (id: string) => data.groups.find((g) => g.id === id)?.name ?? id;

  return (
    <Card
      title="成本视图"
      description="倍率 × 用量只在同一个逻辑模型内部与真实花销成正比，所以这里不做跨模型加总。"
      actions={
        <div className="row" style={{ gap: 6 }}>
          {PERIODS.map((option) => (
            <button
              key={option.value}
              className={`btn btn-sm ${period === option.value ? "btn-primary" : "btn-ghost"}`}
              onClick={() => setPeriod(option.value)}
            >
              {option.label}
            </button>
          ))}
          <button className="btn btn-ghost btn-sm" onClick={() => void load()} title="刷新">
            ↻
          </button>
        </div>
      }
    >
      {loading ? (
        <Skeleton rows={4} />
      ) : view === null || view.models.length === 0 ? (
        <EmptyState
          icon={<IconGauge size={19} />}
          title="区间内没有成功请求"
          description="成本页只统计成功请求：失败请求没有产生上游消耗。跑一些真实流量后再来看。"
        />
      ) : (
        <div className="stack" style={{ gap: 16 }}>
          {/* 全局区域：只显示不失真的量（§6.8）。 */}
          <div className="card-body" style={{ padding: 0 }}>
            <div className="row" style={{ gap: 16, flexWrap: "wrap" }}>
              <Stat label="成功请求" value={view.total_requests.toLocaleString()} />
              <div>
                <div className="text-faint" style={{ fontSize: 11.5 }}>
                  各账号请求占比
                </div>
                <div className="row" style={{ gap: 8, flexWrap: "wrap", marginTop: 2 }}>
                  {view.account_shares.map((share) => (
                    <span key={share.account_id} style={{ fontSize: 12.5 }}>
                      <b>{share.name}</b>{" "}
                      <span className="mono text-faint">
                        {(share.share * 100).toFixed(1)}%
                      </span>
                    </span>
                  ))}
                </div>
              </div>
            </div>
          </div>

          {view.models.map((model) => (
            <ModelCostCard key={`${model.group_id}/${model.logical_model}`} model={model} groupName={groupName(model.group_id)} />
          ))}
        </div>
      )}
    </Card>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div>
      <div className="text-faint" style={{ fontSize: 11.5 }}>
        {label}
      </div>
      <div className="mono cell-strong" style={{ fontSize: 18 }}>
        {value}
      </div>
    </div>
  );
}

function ModelCostCard({
  model,
  groupName,
}: {
  model: CostModel;
  groupName: string;
}) {
  return (
    <div className="card-body" style={{ border: "1px solid var(--border)", borderRadius: 10 }}>
      <div className="row" style={{ gap: 8, flexWrap: "wrap" }}>
        <span className="mono cell-strong">{model.logical_model}</span>
        <span className="text-faint" style={{ fontSize: 12 }}>
          {groupName} · {model.requests.toLocaleString()} 次成功请求
        </span>
        {model.single_target && (
          <span className="badge badge-neutral">只有一个目标，无调度空间</span>
        )}
      </div>

      <div className="table-wrap" style={{ marginTop: 8 }}>
        <table className="data">
          <thead>
            <tr>
              <th>账号</th>
              <th>有效倍率</th>
              <th>流量占比</th>
              <th>请求数</th>
            </tr>
          </thead>
          <tbody>
            {model.accounts.map((account) => (
              <tr key={account.account_id}>
                <td className="cell-strong">{account.name}</td>
                <td className="mono">{account.effective_multiplier ?? "—"}</td>
                <td className="mono">{(account.share * 100).toFixed(1)}%</td>
                <td className="mono">{account.requests.toLocaleString()}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      <div
        className="row"
        style={{ gap: 16, flexWrap: "wrap", marginTop: 10, fontSize: 12.5 }}
      >
        {model.weighted_avg_multiplier && (
          <span>
            加权均倍率{" "}
            <b className="mono">{model.weighted_avg_multiplier}</b>
          </span>
        )}
        {model.cheapest_multiplier && (
          <span>
            最便宜目标 <b className="mono">{model.cheapest_multiplier}</b>
            {model.dearest_multiplier && model.dearest_multiplier !== model.cheapest_multiplier && (
              <>
                {" · "}
                最贵 <b className="mono">{model.dearest_multiplier}</b>
              </>
            )}
          </span>
        )}
        {model.saving_vs_cheapest != null && model.saving_vs_cheapest > 0.0005 && (
          <span className="text-faint">
            只用它会更省 {Math.round(model.saving_vs_cheapest * 100)}%，但没有备份——这是不中断的代价
          </span>
        )}
      </div>
    </div>
  );
}
