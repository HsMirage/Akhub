/**
 * 调度视图：只读地展示"分组 + 模型"下每个上游账号的评分、可用性与速度。
 *
 * 三级视觉必须能一眼分开：分组区块 → 下游模型卡片 → 账号优先级层。
 * 卡片头部只讲"下游模型名 + 下游还能用哪些名字"，表格里只有"上游账号 +
 * 上游模型名"；两者用颜色、字号、前缀明确区分，避免看串。
 *
 * 这里不再配置目标：目标由账号的「模型管理」自动生成。人工优先级只存在于
 * 账号上，数字相同即同层；层内按综合评分加权，跨层仍然严格阶梯。
 */
import { useEffect, useMemo, useState } from "react";
import type { Account, DispatchTarget, LogicalModel, TargetStatsSample } from "../lib/types";
import { PROTOCOL_LABELS, TARGET_STATUS_LABELS } from "../lib/types";
import { formatLimits } from "../lib/format";
import type { Data } from "../lib/store";
import { useRouteParams } from "../lib/store";
import type { Route } from "../routes";
import {
  Badge,
  Button,
  Card,
  EmptyState,
  InfoTip,
  ScoreMeter,
  useToast,
} from "../components/ui";
import { IconRoute, IconSearch, IconSettings } from "../components/Icons";

interface ResolvedTarget {
  target: DispatchTarget;
  account: Account | undefined;
  /** 同一模型内从高到低排出的层号（1 起）；数字相同为同层。 */
  layer: number;
  /** 层内按 score^8 归一化后的预计流量占比。 */
  share: number;
}

interface ModelEntry {
  model: LogicalModel;
  groupName: string;
  targets: ResolvedTarget[];
  /** 下游也能用来调用同一模型的其它名字（未隐藏的上游模型名）。 */
  aliases: string[];
}

export function Targets({
  data,
  navigate,
}: {
  data: Data;
  navigate: (route: Route, params?: Record<string, string>) => void;
}) {
  const toast = useToast();
  const [query, setQuery] = useState("");
  const [groupFilter, setGroupFilter] = useState("all");
  const [onlyIssues, setOnlyIssues] = useState(false);
  const params = useRouteParams();
  const highlightId = params.get("target");
  const [highlight, setHighlight] = useState<string | null>(highlightId);

  useEffect(() => {
    if (!highlightId) {
      setHighlight(null);
      return;
    }
    if (!data.targets.some((target) => target.id === highlightId)) {
      toast.error("这条请求记录里的调度目标已经不存在了");
      setHighlight(null);
      return;
    }
    setHighlight(highlightId);
    const scrollTimer = window.setTimeout(() => {
      document
        .querySelector(`tr[data-target-id="${highlightId}"]`)
        ?.scrollIntoView({ block: "center", behavior: "smooth" });
    }, 80);
    const clearTimer = window.setTimeout(() => setHighlight(null), 2600);
    return () => {
      window.clearTimeout(scrollTimer);
      window.clearTimeout(clearTimer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [highlightId, data.targets.length]);

  const entries = useMemo<ModelEntry[]>(() => {
    const accounts = new Map(data.accounts.map((account) => [account.id, account]));
    const groupNames = new Map(data.groups.map((group) => [group.id, group.name]));

    return data.models
      .map((model) => {
        const raw = data.targets
          .filter((target) => target.logical_model_id === model.id)
          .map<Omit<ResolvedTarget, "layer" | "share">>((target) => ({
            target,
            account: accounts.get(target.account_id),
          }))
          .sort((left, right) => {
            if (right.target.priority !== left.target.priority) {
              return right.target.priority - left.target.priority;
            }
            const leftScore = left.target.score?.total ?? 0;
            const rightScore = right.target.score?.total ?? 0;
            return rightScore - leftScore;
          });

        // 同优先级构成一层；层内权重取 score^8，与调度器的加权随机一致。
        const layerPriorities = [...new Set(raw.map((item) => item.target.priority))].sort(
          (a, b) => b - a,
        );
        const shares = new Map<number, number>();
        for (const priority of layerPriorities) {
          const layer = raw.filter((item) => item.target.priority === priority);
          const weights = layer.map((item) =>
            Math.max(item.target.score?.total ?? 1, 0.01) ** 8,
          );
          const total = weights.reduce((sum, value) => sum + value, 0) || 1;
          layer.forEach((item, index) => {
            shares.set(
              raw.indexOf(item),
              (weights[index] ?? 0) / total,
            );
          });
        }

        const resolved: ResolvedTarget[] = raw.map((item, index) => ({
          ...item,
          layer: layerPriorities.indexOf(item.target.priority) + 1,
          share: shares.get(index) ?? 0,
        }));
        const aliases = [
          ...new Set(
            resolved
              .filter(
                ({ target, account }) =>
                  target.enabled &&
                  account?.enabled !== false &&
                  account?.hide_original !== true &&
                  target.upstream_model !== model.name,
              )
              .map(({ target }) => target.upstream_model),
          ),
        ].sort((a, b) => a.localeCompare(b, "zh-CN"));

        return {
          model,
          groupName: groupNames.get(model.group_id) ?? model.group_id,
          targets: resolved,
          aliases,
        };
      })
      .filter((entry) => entry.targets.length > 0);
  }, [data]);

  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return entries.filter((entry) => {
      if (groupFilter !== "all" && entry.model.group_id !== groupFilter) return false;
      if (
        needle &&
        !entry.model.name.toLowerCase().includes(needle) &&
        !entry.aliases.some((alias) => alias.toLowerCase().includes(needle)) &&
        !entry.targets.some(
          ({ account, target }) =>
            account?.name.toLowerCase().includes(needle) ||
            target.upstream_model.toLowerCase().includes(needle),
        )
      ) {
        return false;
      }
      if (!onlyIssues) return true;
      return entry.targets.some(({ target, account }) => targetHasIssue(target, account));
    });
  }, [entries, groupFilter, onlyIssues, query]);

  const targetTotal = entries.reduce((sum, entry) => sum + entry.targets.length, 0);
  const visibleGroups = data.groups.filter((group) =>
    visible.some((entry) => entry.model.group_id === group.id),
  );

  return (
    <>
      {highlight && (
        <div className="locate-banner">
          <span>
            已从请求记录定位到目标 <span className="mono">{highlight}</span>
          </span>
          <button
            type="button"
            className="link-button"
            onClick={() =>
              document
                .querySelector(`tr[data-target-id="${highlight}"]`)
                ?.scrollIntoView({ block: "center", behavior: "smooth" })
            }
          >
            再次定位
          </button>
          <span className="spacer" />
          <button type="button" className="link-button" onClick={() => setHighlight(null)}>
            关闭
          </button>
        </div>
      )}

      <Card
        title="调度视图"
        description="按分组与模型查看上游账号的综合评分、可用性与速度。目标是自动生成的；人工优先级只在账号上，数字相同即同层，同层按评分加权。"
        actions={
          <Button
            icon={<IconSettings size={13} />}
            onClick={() => navigate("accounts")}
          >
            去账号配置模型
          </Button>
        }
      >
        {targetTotal === 0 ? (
          <EmptyState
            icon={<IconRoute size={19} />}
            title="还没有可调度的模型"
            description={
              data.accounts.length === 0
                ? "先创建上游账号，再从账号的「模型管理」里获取或添加模型。"
                : "打开账号的「模型管理」，获取上游模型并启用，就会自动生成这里的调度目标。"
            }
            action={
              <Button
                variant="primary"
                icon={<IconSettings size={13} />}
                onClick={() => navigate("accounts")}
              >
                去账号模型管理
              </Button>
            }
          />
        ) : (
          <>
            <div className="list-toolbar">
              <div className="input-with-icon list-search">
                <IconSearch size={14} />
                <input
                  className="input"
                  value={query}
                  placeholder="搜索模型名或账号名"
                  aria-label="搜索调度视图"
                  onChange={(event) => setQuery(event.target.value)}
                />
              </div>
              <select
                className="select toolbar-select"
                value={groupFilter}
                aria-label="按分组筛选"
                onChange={(event) => setGroupFilter(event.target.value)}
              >
                <option value="all">全部分组</option>
                {data.groups.map((group) => (
                  <option key={group.id} value={group.id}>
                    {group.name}
                  </option>
                ))}
              </select>
              <label className="filter-toggle">
                <input
                  type="checkbox"
                  checked={onlyIssues}
                  onChange={(event) => setOnlyIssues(event.target.checked)}
                />
                只看异常
              </label>
              <span className="spacer" />
              {/* 调度视图是只读的全局视图：这里必须能看出"手上拿的是不是全量"。
                  列表在数据层已翻页取全，所以正常显示"已全部加载"；
                  万一服务端没给全，立刻退化成"已加载 N/共 M"。 */}
              <span className="table-filter-summary tabular">
                {visible.length} / {entries.length} 个模型 · {targetTotal} 个目标 ·{" "}
                {data.targets.length >= data.totals.targets
                  ? "已全部加载"
                  : `已加载 ${data.targets.length}/${data.totals.targets}`}
              </span>
            </div>

            {visible.length === 0 ? (
              <div className="table-empty">没有符合条件的模型</div>
            ) : (
              visibleGroups.map((group) => (
                <section key={group.id} className="routing-group-block">
                  <header className="routing-group-head">
                    <span className="routing-group-mark" aria-hidden="true" />
                    <span className="routing-group-label">分组</span>
                    <span className="routing-group-name">{group.name}</span>
                    <span className="text-faint">
                      {
                        visible.filter((entry) => entry.model.group_id === group.id).length
                      }{" "}
                      个模型
                    </span>
                  </header>
                  <div className="routing-model-list">
                    {visible
                      .filter((entry) => entry.model.group_id === group.id)
                      .map((entry) => (
                        <ModelBlock
                          key={entry.model.id}
                          entry={entry}
                          highlight={highlight}
                          navigate={navigate}
                        />
                      ))}
                  </div>
                </section>
              ))
            )}
          </>
        )}
      </Card>
    </>
  );
}

function targetHasIssue(target: DispatchTarget, account: Account | undefined): boolean {
  return (
    !target.enabled ||
    account?.enabled === false ||
    target.status !== "active" ||
    account?.multiplier_status !== "known" ||
    target.pause_reason !== null
  );
}

/**
 * 「首字 / 速度」两列的悬停说明（§6.5）。
 *
 * 这两个数是 EWMA，可信度取决于"哪一批请求"与"有多少条样本"。维度必须
 * 说出来：同一个账号的流式与非流式是两份互不相干的统计，分不清就会拿 A 批
 * 的样本去解释 B 批的体感。样本不足 20 条时明说是冷的，评分也还没采信它。
 */
function statsHint(stats: TargetStatsSample | null): string | undefined {
  if (!stats) return undefined;
  const source = `${PROTOCOL_LABELS[stats.protocol]} · ${stats.streaming ? "流式" : "非流式"}`;
  const base = `来自 ${source} 的 ${stats.samples} 个样本`;
  return stats.warm
    ? base
    : `${base}，不足 20 条：只作参考，评分仍按中性分`;
}

function ModelBlock({
  entry,
  highlight,
  navigate,
}: {
  entry: ModelEntry;
  highlight: string | null;
  navigate: (route: Route, params?: Record<string, string>) => void;
}) {
  const { model, aliases, targets } = entry;
  const issueCount = targets.filter(({ target, account }) => targetHasIssue(target, account)).length;
  const layers = [...new Set(targets.map((item) => item.layer))]
    .sort((a, b) => a - b)
    .map((layer) => ({
      layer,
      priority: targets.find((item) => item.layer === layer)?.target.priority ?? 0,
      targets: targets.filter((item) => item.layer === layer),
    }));

  return (
    <section className="routing-model-block">
      <header className="routing-model-head">
        <div className="routing-model-headline">
          <span className="routing-model-kicker">下游模型名</span>
          <span className="routing-model-name mono" title={model.name}>
            {model.name}
          </span>
          <Badge tone={model.listed ? "success" : "warn"}>
            {model.listed ? "已上架" : "未上架"}
          </Badge>
          {issueCount > 0 && <Badge tone="warn">{issueCount} 个异常</Badge>}
        </div>
        {aliases.length > 0 && (
          <div className="routing-model-alias-row">
            <span className="routing-model-kicker">下游也能用这些名字</span>
            {aliases.map((alias) => (
              <span key={alias} className="model-exposed-chip mono" title={`下游也可用：${alias}`}>
                {alias}
              </span>
            ))}
          </div>
        )}
        <div className="spacer" />
        <div className="routing-model-stats">
          <span className="routing-model-stat">
            <b>{layers.length}</b>
            <span>层</span>
          </span>
          <span className="routing-model-stat">
            <b>{targets.length}</b>
            <span>个上游账号</span>
          </span>
        </div>
      </header>

      <div className="routing-layers">
        {layers.map((layer, index) => (
          <div className="routing-layer" key={layer.layer}>
            <div className={`routing-layer-head${index === 0 ? " is-primary" : ""}`}>
              <span className="routing-layer-index">第 {layer.layer} 层</span>
              <span className="routing-layer-priority">账号优先级 {layer.priority}</span>
              <span className="routing-layer-note">
                {index === 0
                  ? "日常流量都走这一层；同一层内按综合评分分配"
                  : "上一层全部不可用（停用 / 冷却 / 额度耗尽）时才会使用"}
              </span>
            </div>
            <div className="table-wrap">
              <table className="data routing-table" style={{ tableLayout: "fixed" }}>
                <colgroup>
                  <col style={{ width: "26%" }} />
                  <col style={{ width: "9%" }} />
                  <col style={{ width: "19%" }} />
                  <col style={{ width: "14%" }} />
                  <col style={{ width: "13%" }} />
                  <col style={{ width: "11%" }} />
                  <col style={{ width: "8%" }} />
                </colgroup>
                <thead>
                  <tr>
                    <th>上游账号 / 上游模型名</th>
                    <th>有效倍率</th>
                    <th>综合评分</th>
                    <th>可用性</th>
                    <th>首字 / 速度</th>
                    <th>在途 / 限制</th>
                    <th>预计分配</th>
                  </tr>
                </thead>
                <tbody>
                  {layer.targets.map(({ target, account, share }) => (
                    <tr
                      key={target.id}
                      data-target-id={target.id}
                      className={highlight === target.id ? "is-highlighted" : undefined}
                    >
                      <td title={`${account?.name ?? "账号已删除"} / ${target.upstream_model}`}>
                        <div className="routing-account-cell">
                          {account ? (
                            <button
                              type="button"
                              className="link-button routing-account-name cell-truncate"
                              title={`打开「${account.name}」的模型管理`}
                              onClick={() =>
                                navigate("accounts", { account: account.id, manage: "1" })
                              }
                            >
                              {account.name}
                            </button>
                          ) : (
                            <span className="routing-account-name">账号已删除</span>
                          )}
                          <span
                            className="routing-upstream-model mono cell-truncate"
                            title={`上游模型名：${target.upstream_model}`}
                          >
                            {target.upstream_model}
                          </span>
                        </div>
                      </td>
                      <td className="mono">{account?.effective_multiplier ?? "—"}</td>
                      <td>
                        {target.score ? (
                          <span className="row" style={{ gap: 4 }}>
                            <ScoreMeter score={target.score} />
                            <InfoTip label={`查看「${account?.name ?? ""}」的评分明细`}>
                              <span className="stack" style={{ gap: 4 }}>
                                <span>倍率 {target.score.multiplier.toFixed(2)}</span>
                                <span>可靠性 {target.score.reliability.toFixed(2)}</span>
                                <span>首字延迟 {target.score.first_token.toFixed(2)}</span>
                                <span>输出速度 {target.score.throughput.toFixed(2)}</span>
                                <span>
                                  综合 {target.score.total.toFixed(2)} · 样本 {target.score.samples}/20
                                  {target.score.warm ? "" : "（冷启动）"}
                                </span>
                              </span>
                            </InfoTip>
                          </span>
                        ) : (
                          <span className="text-faint">—</span>
                        )}
                      </td>
                      <td>
                        <TargetStatusBadge target={target} account={account} />
                        {target.pause_reason && (
                          <div className="text-faint" style={{ fontSize: 11, marginTop: 2 }}>
                            {target.pause_reason}
                          </div>
                        )}
                      </td>
                      <td
                        className="mono cell-dim"
                        style={{ fontSize: 12 }}
                        title={statsHint(target.stats)}
                      >
                        {target.first_token_ms == null && target.output_tps == null ? (
                          "—"
                        ) : (
                          <>
                            <div>
                              {target.first_token_ms == null
                                ? "—"
                                : `${Math.round(target.first_token_ms)} ms`}
                            </div>
                            <div className="text-faint" style={{ fontSize: 11 }}>
                              {target.output_tps == null
                                ? "—"
                                : `${target.output_tps.toFixed(1)} tok/s`}
                            </div>
                          </>
                        )}
                        {/* 样本不足 20 条的数字要明说是冷的，别让人当成稳定值。 */}
                        {target.stats && !target.stats.warm && (
                          <div className="text-faint" style={{ fontSize: 11 }}>
                            样本 {target.stats.samples}/20
                          </div>
                        )}
                      </td>
                      <td className="mono cell-dim" style={{ fontSize: 12 }}>
                        <div>{formatLimits(target.effective_limits)}</div>
                        <div className="text-faint" style={{ fontSize: 11 }}>
                          在途 {target.inflight}
                        </div>
                      </td>
                      <td className="tabular">{Math.round(share * 100)}%</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          </div>
        ))}
      </div>
    </section>
  );
}

function TargetStatusBadge({
  target,
  account,
}: {
  target: DispatchTarget;
  account?: Account;
}) {
  if (!target.enabled || account?.enabled === false) {
    return (
      <Badge tone="neutral" dot>
        停用
      </Badge>
    );
  }
  if (account && account.multiplier_status === "multiplier_unknown") {
    return (
      <Badge tone="danger" dot>
        倍率未知
      </Badge>
    );
  }
  switch (target.status) {
    case "active":
      return (
        <Badge tone={account?.multiplier_status === "multiplier_stale" ? "warn" : "success"} dot>
          {account?.multiplier_status === "multiplier_stale" ? "可用 · 倍率过期" : "可用"}
        </Badge>
      );
    case "cooldown":
      return (
        <Badge tone="warn" dot>
          冷却中{target.cooldown_secs !== null ? ` · ${target.cooldown_secs}s` : ""}
        </Badge>
      );
    case "half_open":
      return (
        <Badge tone="info" dot>
          {TARGET_STATUS_LABELS.half_open}
        </Badge>
      );
    case "quota_exhausted":
    case "key_invalid":
      return (
        <Badge tone="danger" dot>
          {TARGET_STATUS_LABELS[target.status]}
        </Badge>
      );
  }
}
