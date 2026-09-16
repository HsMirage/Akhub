/**
 * 成本页（§6.8）：以逻辑模型为单位组织，绝不跨模型加总。
 *
 * 倍率是折扣不是价格——跨模型相加会被模型组合严重扭曲。全局区域只显示
 * 不失真的请求总数与账号占比；每个模型卡片再展开该模型内部的倍率口径。
 */
import { useCallback, useEffect, useState } from "react";
import type { Data } from "../lib/store";
import { api } from "../lib/api";
import type { CostModel, CostView } from "../lib/types";
import { Badge, Button, Card, EmptyState, Skeleton } from "../components/ui";
import { IconGauge, IconRefresh } from "../components/Icons";

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

  const groupName = (id: string) => data.groups.find((group) => group.id === id)?.name ?? id;

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
              disabled={loading && period === option.value}
            >
              {option.label}
            </button>
          ))}
          <Button
            size="sm"
            variant="ghost"
            icon={loading ? <span className="spinner spinner-sm" /> : <IconRefresh size={14} />}
            onClick={() => void load()}
            disabled={loading}
            title="刷新成本数据"
          >
            刷新
          </Button>
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
        <div className="cost-content">
          <CostOverview view={view} />
          <div className="cost-model-list">
            {view.models.map((model) => (
              <ModelCostCard
                key={`${model.group_id}/${model.logical_model}`}
                model={model}
                groupName={groupName(model.group_id)}
              />
            ))}
          </div>
        </div>
      )}
    </Card>
  );
}

function CostOverview({ view }: { view: CostView }) {
  return (
    <div className="cost-overview">
      <div className="cost-total">
        <div className="cost-label">成功请求总数</div>
        <div className="cost-total-value mono">{view.total_requests.toLocaleString()}</div>
      </div>
      <div className="cost-share-summary">
        <div className="cost-label">各账号占比</div>
        <div className="cost-share-chips">
          {view.account_shares.map((share) => (
            <span key={share.account_id} className="cost-share-chip">
              <span>{share.name}</span>
              <span className="mono">{(share.share * 100).toFixed(1)}%</span>
              <span className="cost-chip-count mono">{share.requests.toLocaleString()} 次</span>
            </span>
          ))}
        </div>
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
  const saving = model.saving_vs_cheapest;
  const savingValue = saving == null ? "—" : `${(saving * 100).toFixed(1)}%`;

  return (
    <article className="cost-model-card">
      <header className="cost-model-head">
        <div className="cost-model-heading">
          <span className="cost-model-name mono">{model.logical_model}</span>
          <span className="cost-model-group">{groupName}</span>
          <span className="cost-model-requests mono">{model.requests.toLocaleString()} 次</span>
          {model.single_target && <Badge tone="neutral">单目标</Badge>}
        </div>
      </header>

      <div className="cost-model-main">
        <div className="cost-metrics">
          <CostMetric label="加权均倍率" value={model.weighted_avg_multiplier ?? "—"} />
          <CostMetric
            label="最便宜 → 最贵"
            value={`${model.cheapest_multiplier ?? "—"} → ${model.dearest_multiplier ?? "—"}`}
          />
          <CostMetric label="可再省比例" value={savingValue} accent={saving != null && saving > 0} />
        </div>

        <div className="cost-account-list">
          <div className="cost-label">账号明细</div>
          {model.accounts.map((account) => (
            <CostAccountRow key={account.account_id} account={account} />
          ))}
        </div>
      </div>

      <p className="cost-footnote">
        口径：加权均倍率按本逻辑模型内的请求占比计算；可再省比例是假设全部请求走最便宜目标，始终不跨模型加总。
      </p>
    </article>
  );
}

function CostMetric({
  label,
  value,
  accent = false,
}: {
  label: string;
  value: string;
  accent?: boolean;
}) {
  return (
    <div className="cost-metric">
      <div className="cost-label">{label}</div>
      <div className={`cost-metric-value mono${accent ? " cost-metric-accent" : ""}`}>
        {value}
      </div>
    </div>
  );
}

function CostAccountRow({
  account,
}: {
  account: CostModel["accounts"][number];
}) {
  const percentage = Math.max(0, Math.min(100, account.share * 100));
  return (
    <div className="cost-account-row">
      <div className="cost-account-name">
        <span title={account.name}>{account.name}</span>
        <span className="cost-account-requests mono">{account.requests.toLocaleString()} 次</span>
      </div>
      <span className="cost-account-multiplier mono">
        {account.effective_multiplier ?? "—"}
      </span>
      <div
        className="cost-share-bar"
        role="img"
        aria-label={`${account.name} 占比 ${percentage.toFixed(1)}%`}
      >
        <span style={{ width: `${percentage}%` }} />
      </div>
      <span className="cost-account-percent mono">{percentage.toFixed(1)}%</span>
    </div>
  );
}
