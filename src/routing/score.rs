//! 性能 EWMA 与层内综合评分（§9.3、§9.4、§9.5）。
//!
//! 归一化一律使用**比值**，不使用 min-max：`0.50 / 0.50 / 0.51` 这组倍率用
//! min-max 会把第三个打成 0 分，用比值得到 0.98；同值时 min-max 又会让整个
//! 维度失效，让分配给它的权重形同虚设。

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use crate::domain::{Multiplier, Protocol, SchedulingWeights};
use crate::storage::store::PerfSnapshotRow;

/// EWMA 的平滑系数。0.2 大约相当于"最近 10 次请求主导当前值"。
const ALPHA: f64 = 0.2;
/// 样本数低于此值时性能三维使用保守中性分（§9.4 的冷启动规则）。
pub const MIN_SAMPLES: u64 = 20;
/// 冷启动与缺失数据时的保守中性分。
///
/// 取 0.6 而不是 0 或 1：新目标既不该凭空压过已证明很好的老目标，也不该被
/// 打成必然选不中——它得先拿到流量才能产生样本。
pub const NEUTRAL: f64 = 0.6;
/// 倍率已过期（`multiplier_stale`）时施加的降权系数（§11.4）。
const STALE_PENALTY: f64 = 0.8;
/// 加权随机的次幂。
///
/// 取 8 可以复现 §9.5 的示例：0.96 对 0.84 得到 74% / 26%，0.95 对 0.94 得到
/// 约 52% / 48%（分差小时接近均分），0.95 对 0.55 时弱者仍保有约 1% 的保命
/// 口粮——够让它的 EWMA 保持新鲜，这正是留这份口粮的目的。
const POWER: i32 = 8;
/// 评分下限。0 分会让权重变成 0，合格目标就再也拿不到任何流量。
const MIN_SCORE: f64 = 0.01;

/// 一次真实用户请求的性能采样。
///
/// 测试按钮与后台任务不进入样本（§9.3）——它们的延迟特征和真实请求不同，
/// 混进来只会污染判断。
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub success: bool,
    /// 首字或首个语义事件延迟。非流式请求没有这一项。
    pub first_token: Option<Duration>,
    /// 端到端总耗时。
    pub total: Duration,
    /// 上游报告的输出 Token 数，用于算每秒输出速度。
    pub output_tokens: Option<u64>,
}

/// 一个目标在某个协议、某种流式模式下的 EWMA 当前值。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    pub samples: u64,
    pub success_rate: f64,
    pub first_token_ms: f64,
    pub total_ms: f64,
    pub output_tps: f64,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            samples: 0,
            // 没有任何样本时先假设一切正常，让新目标有机会拿到第一批流量。
            success_rate: 1.0,
            first_token_ms: 0.0,
            total_ms: 0.0,
            output_tps: 0.0,
        }
    }
}

impl Stats {
    /// 样本是否已经多到可以信任性能三维（§9.4）。
    pub fn is_warm(&self) -> bool {
        self.samples >= MIN_SAMPLES
    }

    fn observe(&mut self, sample: &Sample) {
        self.samples = self.samples.saturating_add(1);
        self.success_rate = ewma(self.success_rate, if sample.success { 1.0 } else { 0.0 });

        // 失败请求的延迟没有意义：一个 0.2 秒就 500 的目标不该因此显得"很快"。
        if !sample.success {
            return;
        }
        if let Some(first_token) = sample.first_token {
            let value = first_token.as_secs_f64() * 1000.0;
            self.first_token_ms = ewma_or_seed(self.first_token_ms, value);
        }
        let total_ms = sample.total.as_secs_f64() * 1000.0;
        self.total_ms = ewma_or_seed(self.total_ms, total_ms);
        if let Some(tokens) = sample.output_tokens
            && sample.total > Duration::ZERO
        {
            let tps = tokens as f64 / sample.total.as_secs_f64();
            self.output_tps = ewma_or_seed(self.output_tps, tps);
        }
    }
}

fn ewma(current: f64, observation: f64) -> f64 {
    current * (1.0 - ALPHA) + observation * ALPHA
}

/// 第一次观测直接作为初值，避免所有延迟从 0 慢慢爬上来。
fn ewma_or_seed(current: f64, observation: f64) -> f64 {
    if current <= 0.0 {
        observation
    } else {
        ewma(current, observation)
    }
}

/// 统计维度的键：逻辑模型由目标唯一确定，所以只需要目标、协议与是否流式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Dimension {
    pub protocol: Protocol,
    pub streaming: bool,
}

/// 一个目标在各维度上的统计，按目标独立加锁。
type TargetStats = Arc<Mutex<HashMap<Dimension, Stats>>>;

/// 性能统计表的内存上限（§19.4）。
///
/// 正常路径由 \`retain\` 按"当前配置里还存在的目标"清理；这个上限是兜底：
/// 万一清理没被调用（例如某种没走配置重载的路径），表也不能无限涨。
const MAX_TRACKED_TARGETS: usize = 5_000;

/// 全进程的性能统计表。
#[derive(Default)]
pub struct Registry {
    inner: RwLock<HashMap<String, TargetStats>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记录一次真实请求的采样。
    pub fn observe(&self, target_id: &str, dimension: Dimension, sample: &Sample) {
        let entry = self.entry(target_id);
        let mut stats = crate::sync::lock(&entry);
        stats.entry(dimension).or_default().observe(sample);
    }

    /// 读取一个目标在某个维度上的当前统计。
    pub fn stats(&self, target_id: &str, dimension: Dimension) -> Stats {
        let entry = self.entry(target_id);
        let stats = crate::sync::lock(&entry);
        stats.get(&dimension).copied().unwrap_or_default()
    }

    fn entry(&self, target_id: &str) -> TargetStats {
        if let Ok(guard) = self.inner.read()
            && let Some(found) = guard.get(target_id)
        {
            return Arc::clone(found);
        }
        let mut guard = crate::sync::write(&self.inner);
        if guard.len() >= MAX_TRACKED_TARGETS && !guard.contains_key(target_id) {
            // 兜底淘汰：这些条目本来就该由 retain 清掉，走到这里说明清理漏了。
            // 丢一个已有目标比无界增长好，但要在日志里留下痕迹。
            if let Some(victim) = guard.keys().next().cloned() {
                guard.remove(&victim);
                tracing::warn!(
                    limit = MAX_TRACKED_TARGETS,
                    evicted = %victim,
                    "性能统计表达到上限，已淘汰一个条目（retain 可能漏了）"
                );
            }
        }
        Arc::clone(guard.entry(target_id.to_string()).or_default())
    }

    /// 导出全部统计，供 60 秒快照任务落盘。
    pub fn export(&self, now: i64) -> Vec<PerfSnapshotRow> {
        let guard = crate::sync::read(&self.inner);
        let mut rows = Vec::new();
        for (target_id, entry) in guard.iter() {
            let stats = crate::sync::lock(entry);
            for (dimension, value) in stats.iter() {
                rows.push(PerfSnapshotRow {
                    target_id: target_id.clone(),
                    protocol: dimension.protocol,
                    streaming: dimension.streaming,
                    samples: value.samples as i64,
                    success_rate: value.success_rate,
                    first_token_ms: value.first_token_ms,
                    total_ms: value.total_ms,
                    output_tps: value.output_tps,
                    updated_at: now,
                });
            }
        }
        rows
    }

    /// 启动时从快照恢复，让评分不从零开始（§20.1）。
    pub fn restore(&self, rows: &[PerfSnapshotRow]) {
        let mut guard = crate::sync::write(&self.inner);
        for row in rows {
            let entry = guard.entry(row.target_id.clone()).or_default();
            let mut stats = crate::sync::lock(entry);
            stats.insert(
                Dimension {
                    protocol: row.protocol,
                    streaming: row.streaming,
                },
                Stats {
                    samples: row.samples.max(0) as u64,
                    success_rate: row.success_rate,
                    first_token_ms: row.first_token_ms,
                    total_ms: row.total_ms,
                    output_tps: row.output_tps,
                },
            );
        }
    }

    /// 当前跟踪的目标数，供测试与诊断确认表没有无界增长。
    pub fn len(&self) -> usize {
        self.inner.read().map(|guard| guard.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 丢弃已经不在配置里的目标。
    pub fn retain(&self, live_targets: &[String]) {
        if let Ok(mut guard) = self.inner.write() {
            guard.retain(|id, _| live_targets.iter().any(|live| live == id));
        }
    }
}

/// 参与评分的一个候选目标。
#[derive(Debug, Clone)]
pub struct Candidate {
    pub target_id: String,
    /// 此刻的有效倍率。
    pub multiplier: Multiplier,
    /// 倍率是否已过期（宽限期内），过期要降权（§11.4）。
    pub multiplier_stale: bool,
    pub stats: Stats,
}

/// 一个目标的分维得分与综合评分，后台可以直接列成一列做诊断（§6.9）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Score {
    pub multiplier: f64,
    pub reliability: f64,
    pub first_token: f64,
    pub throughput: f64,
    pub total: f64,
}

/// 归一化的参照系。
///
/// §9.4 要求参照系比"当前层"更宽，好让分数跨层可比。这里取**同一逻辑模型
/// 的全部候选**：首字延迟和输出速度是模型属性，把 haiku 和 opus 放进同一个
/// 参照系会让慢模型的所有目标一起趋近 0 分，那个维度就再次失效——正是比值
/// 归一化想避免的毛病。倍率是账号属性，与模型无关，因此调用方传入的参照系
/// 可以覆盖整个分组。
#[derive(Debug, Clone, Copy)]
struct Reference {
    cheapest: f64,
    fastest_first_token: f64,
    highest_tps: f64,
}

impl Reference {
    fn of(candidates: &[Candidate], cheapest_in_group: Option<Multiplier>) -> Self {
        let cheapest = cheapest_in_group
            .map(|m| m.to_f64())
            .or_else(|| {
                candidates
                    .iter()
                    .map(|c| c.multiplier.to_f64())
                    .fold(None, |acc: Option<f64>, v| {
                        Some(acc.map_or(v, |a| a.min(v)))
                    })
            })
            .unwrap_or(0.0);

        // 只有样本够多的目标才有资格定义参照系：拿一个跑过两次的目标当
        // "全组最快"，会把所有成熟目标都压成低分。
        let warm = || candidates.iter().filter(|c| c.stats.is_warm());
        let fastest_first_token = warm()
            .map(|c| c.stats.first_token_ms)
            .filter(|v| *v > 0.0)
            .fold(f64::INFINITY, f64::min);
        let highest_tps = warm().map(|c| c.stats.output_tps).fold(0.0, f64::max);

        Self {
            cheapest,
            fastest_first_token,
            highest_tps,
        }
    }
}

/// 按 §9.4 给一组候选打分。
///
/// `cheapest_in_group` 是整个分组内最低的有效倍率；倍率是账号级属性，用分组
/// 作参照系才能让"同一个账号在不同模型下的倍率得分一致"。
pub fn score_all(
    candidates: &[Candidate],
    weights: SchedulingWeights,
    cheapest_in_group: Option<Multiplier>,
) -> Vec<Score> {
    let reference = Reference::of(candidates, cheapest_in_group);
    candidates
        .iter()
        .map(|candidate| score_one(candidate, &reference, weights))
        .collect()
}

fn score_one(candidate: &Candidate, reference: &Reference, weights: SchedulingWeights) -> Score {
    let own = candidate.multiplier.to_f64();
    // 免费或倍率为 0 时没有比值可言，直接给满分。
    let multiplier = if own <= 0.0 {
        1.0
    } else if reference.cheapest <= 0.0 {
        // 参照系里有一个免费目标：自己不免费就只能算最低分档，但不至于 0。
        MIN_SCORE
    } else {
        (reference.cheapest / own).clamp(0.0, 1.0)
    };

    // 冷启动：真实样本不足时性能三维用保守中性分，倍率维正常参与（§9.4）。
    let (reliability, first_token, throughput) = if candidate.stats.is_warm() {
        let reliability = candidate.stats.success_rate.clamp(0.0, 1.0);
        let first_token =
            if candidate.stats.first_token_ms > 0.0 && reference.fastest_first_token.is_finite() {
                (reference.fastest_first_token / candidate.stats.first_token_ms).clamp(0.0, 1.0)
            } else {
                NEUTRAL
            };
        let throughput = if candidate.stats.output_tps > 0.0 && reference.highest_tps > 0.0 {
            (candidate.stats.output_tps / reference.highest_tps).clamp(0.0, 1.0)
        } else {
            NEUTRAL
        };
        (reliability, first_token, throughput)
    } else {
        (NEUTRAL, NEUTRAL, NEUTRAL)
    };

    let total = (multiplier * f64::from(weights.multiplier)
        + reliability * f64::from(weights.reliability)
        + first_token * f64::from(weights.first_token)
        + throughput * f64::from(weights.throughput))
        / f64::from(SchedulingWeights::TOTAL);
    let total = if candidate.multiplier_stale {
        total * STALE_PENALTY
    } else {
        total
    };

    Score {
        multiplier,
        reliability,
        first_token,
        throughput,
        total: total.clamp(MIN_SCORE, 1.0),
    }
}

/// 按 `score^k` 加权随机排出层内的尝试顺序（§9.5）。
///
/// 返回的是**顺序**而不是单个选择：第一个是本次抽中的目标，后面是它失败后
/// 依次尝试的备选。不放回抽样天然满足"层内先耗尽再降层"（§13.1）。
pub fn weighted_order(scores: &[Score], random: &mut impl FnMut() -> f64) -> Vec<usize> {
    let mut weights: Vec<f64> = scores
        .iter()
        .map(|score| score.total.max(MIN_SCORE).powi(POWER))
        .collect();
    let mut order = Vec::with_capacity(scores.len());
    let mut remaining: Vec<usize> = (0..scores.len()).collect();

    while !remaining.is_empty() {
        let total: f64 = remaining.iter().map(|i| weights[*i]).sum();
        if total <= 0.0 || total.is_nan() {
            order.append(&mut remaining);
            break;
        }
        let mut ticket = random().clamp(0.0, 1.0) * total;
        let mut chosen = remaining.len() - 1;
        for (position, index) in remaining.iter().enumerate() {
            ticket -= weights[*index];
            if ticket <= 0.0 {
                chosen = position;
                break;
            }
        }
        let index = remaining.remove(chosen);
        weights[index] = 0.0;
        order.push(index);
    }
    order
}

/// 默认的随机源。
pub fn random_unit() -> f64 {
    let mut bytes = [0u8; 8];
    // 失败时退回 0.0：等价于总是选权重最高的那个，比 panic 好得多。
    if getrandom::fill(&mut bytes).is_err() {
        return 0.0;
    }
    (u64::from_le_bytes(bytes) >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn multiplier(raw: &str) -> Multiplier {
        Multiplier::parse(raw).unwrap()
    }

    fn warm(multiplier_raw: &str, success: f64, first_token_ms: f64, tps: f64) -> Candidate {
        Candidate {
            target_id: multiplier_raw.into(),
            multiplier: multiplier(multiplier_raw),
            multiplier_stale: false,
            stats: Stats {
                samples: MIN_SAMPLES,
                success_rate: success,
                first_token_ms,
                total_ms: first_token_ms * 4.0,
                output_tps: tps,
            },
        }
    }

    /// 固定序列的伪随机源，让加权抽样的测试可复现。
    fn sequence(values: Vec<f64>) -> impl FnMut() -> f64 {
        let mut iter = values.into_iter().cycle();
        move || iter.next().unwrap_or(0.0)
    }

    #[test]
    fn ratio_normalisation_does_not_kill_a_marginally_worse_target() {
        // §9.4 的核心例子：0.50 / 0.50 / 0.51 用 min-max 会把第三个打成 0 分。
        let candidates = vec![
            warm("0.5", 1.0, 1000.0, 50.0),
            warm("0.50", 1.0, 1000.0, 50.0),
            warm("0.51", 1.0, 1000.0, 50.0),
        ];
        let scores = score_all(&candidates, SchedulingWeights::default(), None);
        assert!(
            (scores[2].multiplier - 0.98).abs() < 0.01,
            "{:?}",
            scores[2]
        );
        assert!(scores[2].total > 0.9);
    }

    #[test]
    fn identical_values_keep_the_dimension_alive() {
        // 同值时 min-max 会让整个维度失效；比值归一化仍给出满分。
        let candidates = vec![
            warm("0.5", 1.0, 800.0, 60.0),
            warm("0.50", 1.0, 800.0, 60.0),
        ];
        let scores = score_all(&candidates, SchedulingWeights::default(), None);
        assert_eq!(scores[0].multiplier, 1.0);
        assert_eq!(scores[1].multiplier, 1.0);
        assert_eq!(scores[0].first_token, 1.0);
    }

    #[test]
    fn a_large_multiplier_gap_is_reflected_proportionally() {
        // 0.05 / 0.08：min-max 给 1.0 / 0.0，比值给 1.0 / 0.63。
        let candidates = vec![
            warm("0.05", 1.0, 900.0, 40.0),
            warm("0.08", 1.0, 900.0, 40.0),
        ];
        let scores = score_all(&candidates, SchedulingWeights::default(), None);
        assert!(
            (scores[1].multiplier - 0.625).abs() < 0.01,
            "{:?}",
            scores[1]
        );
    }

    #[test]
    fn cold_targets_use_a_neutral_performance_score_but_a_real_multiplier_score() {
        let mut cold = warm("0.1", 1.0, 100.0, 100.0);
        cold.stats.samples = MIN_SAMPLES - 1;
        let candidates = vec![cold, warm("0.2", 1.0, 900.0, 40.0)];
        let scores = score_all(&candidates, SchedulingWeights::default(), None);

        assert_eq!(scores[0].reliability, NEUTRAL);
        assert_eq!(scores[0].first_token, NEUTRAL);
        assert_eq!(scores[0].throughput, NEUTRAL);
        // 倍率维照常参与：新目标便宜就是便宜。
        assert_eq!(scores[0].multiplier, 1.0);
        assert!(scores[1].multiplier < 1.0);
    }

    #[test]
    fn a_cold_target_cannot_define_the_reference_frame() {
        // 一个只跑过两次、恰好很快的目标不该把所有成熟目标压成低分。
        let mut lucky = warm("0.5", 1.0, 10.0, 500.0);
        lucky.stats.samples = 2;
        let candidates = vec![lucky, warm("0.5", 1.0, 1000.0, 50.0)];
        let scores = score_all(&candidates, SchedulingWeights::default(), None);
        assert_eq!(scores[1].first_token, 1.0, "成熟目标仍是参照系里最快的");
        assert_eq!(scores[1].throughput, 1.0);
    }

    #[test]
    fn reliability_is_scored_not_merely_left_to_the_breaker() {
        // 稳定 85% 成功率永远不会被熔断，但每 7 次就要重试一次，必须降分。
        let candidates = vec![
            warm("0.5", 1.0, 900.0, 50.0),
            warm("0.50", 0.85, 900.0, 50.0),
        ];
        let scores = score_all(&candidates, SchedulingWeights::default(), None);
        assert!(scores[1].total < scores[0].total);
    }

    #[test]
    fn a_stale_multiplier_is_penalised_but_still_selectable() {
        let mut stale = warm("0.5", 1.0, 900.0, 50.0);
        stale.multiplier_stale = true;
        let candidates = vec![warm("0.5", 1.0, 900.0, 50.0), stale];
        let scores = score_all(&candidates, SchedulingWeights::default(), None);
        assert!(scores[1].total < scores[0].total);
        assert!(scores[1].total > 0.0, "宽限期内仍可参与调度");
    }

    #[test]
    fn the_group_wide_frame_keeps_multiplier_scores_comparable_across_models() {
        // 分组里最便宜的是 0.1，但这个模型的候选最低只有 0.5：倍率得分必须
        // 按分组算，否则同一个账号在不同模型下会得到不同的倍率分。
        let candidates = vec![warm("0.5", 1.0, 900.0, 50.0)];
        let scores = score_all(
            &candidates,
            SchedulingWeights::default(),
            Some(multiplier("0.1")),
        );
        assert!((scores[0].multiplier - 0.2).abs() < 1e-9);
    }

    #[test]
    fn weights_actually_shift_the_ranking() {
        // 便宜但慢 vs 贵但快：把权重全给倍率，便宜的应当胜出；全给首字，反之。
        let candidates = vec![
            warm("0.2", 1.0, 3000.0, 20.0),
            warm("0.8", 1.0, 300.0, 20.0),
        ];
        let cost_first = score_all(
            &candidates,
            SchedulingWeights {
                multiplier: 100,
                reliability: 0,
                first_token: 0,
                throughput: 0,
            },
            None,
        );
        assert!(cost_first[0].total > cost_first[1].total);

        let latency_first = score_all(
            &candidates,
            SchedulingWeights {
                multiplier: 0,
                reliability: 0,
                first_token: 100,
                throughput: 0,
            },
            None,
        );
        assert!(latency_first[1].total > latency_first[0].total);
    }

    #[test]
    fn a_small_score_gap_splits_traffic_almost_evenly() {
        let scores = vec![score(0.95), score(0.94)];
        let share = simulate(&scores, 20_000);
        assert!(
            (share[0] - 0.5).abs() < 0.1,
            "分差小时应当接近均分，实际 {share:?}"
        );
    }

    #[test]
    fn a_large_score_gap_still_leaves_the_backup_some_rations() {
        let scores = vec![score(0.95), score(0.55)];
        let share = simulate(&scores, 40_000);
        assert!(share[0] > 0.9, "主力应当拿到绝大多数流量：{share:?}");
        assert!(
            share[1] > 0.0005,
            "备胎必须留有保命口粮，否则它的 EWMA 会一直停在三天前：{share:?}"
        );
        assert!(share[1] < 0.08, "备胎不该拿到接近均分的流量：{share:?}");
    }

    #[test]
    fn the_documented_example_reproduces() {
        // §9.5：A 0.96 / B 0.84 → 74% / 26%
        let share = simulate(&[score(0.96), score(0.84)], 40_000);
        assert!((share[0] - 0.74).abs() < 0.03, "{share:?}");
    }

    #[test]
    fn every_candidate_appears_exactly_once_in_the_attempt_order() {
        let scores = vec![score(0.9), score(0.5), score(0.1)];
        let mut random = sequence(vec![0.99, 0.5, 0.0]);
        let mut order = weighted_order(&scores, &mut random);
        order.sort_unstable();
        assert_eq!(order, vec![0, 1, 2], "不放回抽样必须覆盖全部候选");
    }

    #[test]
    fn a_zero_scoring_candidate_is_still_reachable() {
        // 分数被夹到下限而不是 0：合格目标不能因为一时的坏分数彻底消失。
        let scores = vec![
            Score {
                multiplier: 0.0,
                reliability: 0.0,
                first_token: 0.0,
                throughput: 0.0,
                total: 0.0,
            },
            score(0.9),
        ];
        let mut random = sequence(vec![0.999999]);
        let order = weighted_order(&scores, &mut random);
        assert_eq!(order.len(), 2);
    }

    fn score(total: f64) -> Score {
        Score {
            multiplier: total,
            reliability: total,
            first_token: total,
            throughput: total,
            total,
        }
    }

    /// 用均匀随机源模拟多轮抽签，返回每个候选被抽为第一名的比例。
    fn simulate(scores: &[Score], rounds: usize) -> Vec<f64> {
        let mut counts = vec![0usize; scores.len()];
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut random = || {
            // xorshift64*：测试内自带确定性随机源，不依赖外部 crate。
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
        };
        for _ in 0..rounds {
            let order = weighted_order(scores, &mut random);
            counts[order[0]] += 1;
        }
        counts
            .into_iter()
            .map(|count| count as f64 / rounds as f64)
            .collect()
    }

    #[test]
    fn ewma_weights_recent_requests_more_heavily() {
        let mut stats = Stats::default();
        for _ in 0..50 {
            stats.observe(&Sample {
                success: true,
                first_token: Some(Duration::from_millis(1000)),
                total: Duration::from_millis(4000),
                output_tokens: Some(400),
            });
        }
        assert!((stats.first_token_ms - 1000.0).abs() < 1.0);
        assert!((stats.output_tps - 100.0).abs() < 1.0);

        // 变快之后要在几十个样本内跟上，而不是被历史拖住。
        for _ in 0..20 {
            stats.observe(&Sample {
                success: true,
                first_token: Some(Duration::from_millis(200)),
                total: Duration::from_millis(1000),
                output_tokens: Some(400),
            });
        }
        assert!(stats.first_token_ms < 400.0, "{}", stats.first_token_ms);
    }

    #[test]
    fn a_failed_request_lowers_reliability_without_faking_speed() {
        let mut stats = Stats::default();
        stats.observe(&Sample {
            success: true,
            first_token: Some(Duration::from_millis(1000)),
            total: Duration::from_millis(4000),
            output_tokens: Some(400),
        });
        let fast_first_token = stats.first_token_ms;

        // 0.2 秒就 500 的失败请求不该让这个目标显得"很快"。
        stats.observe(&Sample {
            success: false,
            first_token: Some(Duration::from_millis(1)),
            total: Duration::from_millis(200),
            output_tokens: None,
        });
        assert_eq!(stats.first_token_ms, fast_first_token);
        assert!(stats.success_rate < 1.0);
    }

    #[test]
    fn snapshots_round_trip_through_the_registry() {
        let registry = Registry::new();
        let dimension = Dimension {
            protocol: Protocol::AnthropicMessages,
            streaming: true,
        };
        for _ in 0..MIN_SAMPLES {
            registry.observe(
                "tgt",
                dimension,
                &Sample {
                    success: true,
                    first_token: Some(Duration::from_millis(700)),
                    total: Duration::from_millis(3000),
                    output_tokens: Some(300),
                },
            );
        }
        let exported = registry.export(1_000);
        assert_eq!(exported.len(), 1);

        // 重启：新表从快照恢复，评分不从零开始（§26.7）。
        let restored = Registry::new();
        restored.restore(&exported);
        let stats = restored.stats("tgt", dimension);
        assert!(stats.is_warm());
        assert!((stats.first_token_ms - 700.0).abs() < 1.0);

        restored.retain(&[]);
        assert!(!restored.stats("tgt", dimension).is_warm());
    }

    #[test]
    fn the_default_random_source_stays_in_range() {
        for _ in 0..64 {
            let value = random_unit();
            assert!((0.0..1.0).contains(&value), "{value}");
        }
    }

    /// 性能统计表有兜底上限，即使 retain 没被调用也不会无界增长（§19.4）。
    #[test]
    fn the_registry_stops_growing_at_its_cap() {
        use crate::domain::Protocol;
        let registry = Registry::new();
        let dimension = Dimension {
            protocol: Protocol::OpenAiChat,
            streaming: false,
        };
        for i in 0..MAX_TRACKED_TARGETS + 50 {
            registry.observe(
                &format!("t{i}"),
                dimension,
                &Sample {
                    success: true,
                    first_token: None,
                    total: Duration::from_millis(10),
                    output_tokens: None,
                },
            );
        }
        assert!(
            registry.len() <= MAX_TRACKED_TARGETS + 1,
            "统计表不该无界增长：{}",
            registry.len()
        );
    }
}
