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
/// 有效样本量低于此值时性能三维使用保守中性分（§9.4 的冷启动规则）。
///
/// 它数的是**时间衰减后的**样本权重，不是累计条数：一个几百条样本但全是
/// 上周的账号，有效样本量会衰减到阈值以下，于是自动退出"可信"状态、回到
/// 中性分，并把参照系让给当下有数据的账号（§9.4 修订）。
///
/// 取 10 而不是 20：加上时间衰减之后，这个数字**只影响首次接入**——一旦有稳定
/// 流量，有效样本量会在远高于阈值的地方达到稳态（现场主力约 610，慢账号约 20），
/// 10 与 20 的差别只剩"新账号多快被采信"。EWMA 的 α=0.2 意味着约 5 次观测就
/// 主导当前值，10 条已经足够覆盖两个完整的主导窗口。
pub const MIN_SAMPLES: u64 = 10;

/// 样本权重的半衰期（秒）。
///
/// 有效样本量按 `0.5^(闲置时长 / 半衰期)` 衰减。24 小时这个取值由**真实观测速率**
/// 反推出来，两端都要满足：
///
/// - **下界**（慢账号不能被时间衰减踢出参照系）：稳态下 `权重 ≈ 速率 × 半衰期/ln2`，
///   门槛 10 对应的最低速率是 `10 × ln2 / 半衰期`。现场（gpt-boom / gpt-5.6-sol）
///   除主力外四个账号的实测速率是 0.71 ~ 2.30 条/小时，半衰期必须 ≥ 10 小时才能让
///   最慢的那个仍然算"有持续证据"。取 24 小时即门槛 0.29 条/小时，留了约 2.4 倍
///   余量，同时仍能把"超过一天没有任何观测"判为证据过期。
/// - **上界**（陈旧证据必须真的过期）：300 条样本的账号闲置约 5 天后跌到门槛以下，
///   不再以"上周很快"的身份定义参照系、压住当下正常的账号。
///
/// 两个约束把取值夹在 10~120 小时之间；24 小时居中，且"最后采样超过一天"本身
/// 就是值得复核的信号。
const SAMPLE_HALF_LIFE_SECS: f64 = 24.0 * 3600.0;
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
/// 层内抽签的**探索口粮**：每个候选在 `score^k` 之外额外分到的固定底权（§9.5）。
///
/// 只用 `score^k` 时冷目标永远拿不到样本，拿不到样本又永远是保守中性分，
/// 于是它永远排在热目标之后。这里还有一层**正反馈**：一个目标只要被抽中一次
/// 就变热、权重再涨一档，"越被选中越被选中"。现场（gpt-boom / gpt-5.6-sol，
/// 5 个同层候选）的期望分布退化成了单一账号：
///
/// | 目标 | 综合评分 | score^8 | 原始占比 |
/// |---|---|---|---|
/// | 唯一热目标 | 0.982 | 0.851 | 63.6% |
/// | 冷目标（中性 0.6） | 0.802 | 0.168 | 12.6% |
/// | 冷目标（最贵） | 0.669 | 0.040 | 3.0% |
///
/// 这份底权把每个候选抬到同一量级的下限。它不是平均分配（上表里 63.6% →
/// 56.4%）：分数仍是主导项，只是不再是唯一的项。改动后的效果是最差的目标
/// 也有约 3% 的期望流量，于是它能在可接受的时间内攒够 §9.4 要求的 20 个样本，
/// 真正进入评分——这正是 §9.5 所说的"保命口粮"在数量级差距下真正生效。
const EXPLORATION: f64 = 0.03;
/// 目标多久没被采样就算"陈旧"，探索口粮按这个尺度放大（§9.5）。
const STALE_UNIT_SECS: f64 = 600.0;
/// 陈旧换来的探索口粮上限倍数（相对 `EXPLORATION`）。
///
/// 取 8 而不是更小：口粮要能把一个"分低但只是很久没测过"的目标拉回到能攒够
/// `MIN_SAMPLES` 的量级。按 300 请求/小时、5 个目标估算，4 倍（0.12）在对手
/// 分数 0.95 时只够 4% 上下，攒 20 个样本要两个多小时；8 倍（0.24）能到 10%
/// 量级，二十多分钟就重新有数。
///
/// 不抬高基础值 `EXPLORATION` 本身：那是所有目标（含主力）都吃的水位，抬高它
/// 会把"主力仍然拿走绝大多数流量"这条不变量一起改掉。
const MAX_STALE_FACTOR: f64 = 8.0;
/// 定义性能参照系所需的最少热目标数（§9.4）。
///
/// 只有一个热目标时，它就是参照系里"最快的""吞吐最高的"，三项性能分自动
/// 拉满——而这与它实际有多慢无关。它因此永远比拼分只有中性 0.6 的冷目标高
/// 一截，形成第二个正反馈：最先拿到样本的目标从此赢家通吃。
/// 一个数据点不构成参照系；不足两个热目标时全体退回中性分，让流量先按倍率
/// 与探索口粮铺开，等样本够了再让性能说话。
const MIN_FRAME_TARGETS: usize = 2;

/// 一次真实用户请求的性能采样。
///
/// 测试按钮与后台任务不进入样本（§9.3）——它们的延迟特征和真实请求不同，
/// 混进来只会污染判断。
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    pub success: bool,
    /// 这次采样是否应该影响**目标质量**的判断。
    ///
    /// 客户端中途断开（`client_gone`）时它是 `false`：这次请求是下游自己
    /// 放弃的，上游没有做错任何事。若把它当成失败喂进成功率 EWMA，一次
    /// 掉线就按 `ALPHA` 扣掉两成可靠性，要连着十次成功才爬得回来——那等于
    /// 因为调用方掉线而惩罚一个健康账号（§9.3、§12.3）。
    ///
    /// 与健康状态机的 `Neutral` 是同一个口径：不进统计。
    pub counts: bool,
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
    /// 累计采样条数（只增不减）。用于展示"采了多少条"，不再是可信判据。
    pub samples: u64,
    /// **带时间衰减的样本权重**：最后一次观测那一刻的有效样本量。
    ///
    /// 读取时必须经 `effective_samples` 按闲置时长继续衰减，否则一个几小时
    /// 没被采样的账号会带着旧权重装作新鲜（§9.4 修订）。
    pub weight: f64,
    pub success_rate: f64,
    pub first_token_ms: f64,
    pub total_ms: f64,
    pub output_tps: f64,
    /// 最近一次**进统计**的采样时刻（Unix 秒）。0 表示从来没采样过。
    ///
    /// 它同时是权重的衰减起点（见 `weight`）与探索口粮的陈旧度依据（§9.5）。
    pub last_sample_at: i64,
}

/// 闲置 `idle_secs` 之后样本权重的残留比例（半衰期见 `SAMPLE_HALF_LIFE_SECS`）。
fn decay_factor(idle_secs: f64) -> f64 {
    if idle_secs <= 0.0 {
        return 1.0;
    }
    0.5f64.powf(idle_secs / SAMPLE_HALF_LIFE_SECS)
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            samples: 0,
            weight: 0.0,
            // 没有任何样本时先假设一切正常，让新目标有机会拿到第一批流量。
            success_rate: 1.0,
            first_token_ms: 0.0,
            total_ms: 0.0,
            output_tps: 0.0,
            last_sample_at: 0,
        }
    }
}

impl Stats {
    /// 到 `now` 为止的**有效样本量**：按闲置时长对 `weight` 继续衰减。
    ///
    /// 从没被采样过时是 0——这是"没有证据"，与"证据过期"区分开（后者仍可能
    /// 略大于 0，只是不够可信）。
    pub fn effective_samples(&self, now: i64) -> f64 {
        if self.weight <= 0.0 {
            return 0.0;
        }
        // 采样时刻未知（老库快照）时按"完全过期"处理，而不是拿当前时间当基准：
        // 那会让一个几天没请求的目标看起来刚被采样过，正是要修的那个毛病。
        if self.last_sample_at <= 0 {
            return 0.0;
        }
        self.weight * decay_factor((now - self.last_sample_at).max(0) as f64)
    }

    /// 有效样本量是否够信任性能三维（§9.4 修订）。
    ///
    /// 判据是**时间衰减后的**证据量，不是累计条数：一个几百条样本但全是上周的
    /// 账号会失去可信状态，不再定义参照系、也不再压住当下正常的账号。
    pub fn is_warm(&self, now: i64) -> bool {
        self.effective_samples(now) >= MIN_SAMPLES as f64
    }

    /// 把所有观测按闲置时长一次性衰减掉，然后按观测更新其余 EWMA。
    ///
    /// 只在 `observe` 里调用：这样 `Stats` 的其它读取路径全部是纯函数，
    /// 不需要 `&mut self`，也就不会在"评分时顺手改写状态"这种地方引入竞态。
    fn observe(&mut self, sample: &Sample, now: i64) {
        // 客户端断开连样本都不算：它既不代表目标成功，也不代表目标失败，
        // 混进样本数还会让 `is_warm` 提前成立（§9.3）。
        if !sample.counts {
            return;
        }
        // 先把已有权重按"距上次采样过了多久"衰减，再补上这一次观测。
        // 递减式：稳定状态下权重收敛到观测速率的量级（见 SAMPLE_HALF_LIFE_SECS）。
        //
        // 走 effective_samples 而不是就地乘衰减因子：这样"不知道有多旧"
        // （last_sample_at == 0，例如老库快照）会自然归零——它必须重新采样
        // 才能回到可信状态，而不是靠一次观测就把几百条旧权重全部复活。
        self.weight = self.effective_samples(now) + 1.0;
        self.samples = self.samples.saturating_add(1);
        self.last_sample_at = now;
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
    pub fn observe(&self, target_id: &str, dimension: Dimension, sample: &Sample, now: i64) {
        let entry = self.entry(target_id);
        let mut stats = crate::sync::lock(&entry);
        stats.entry(dimension).or_default().observe(sample, now);
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
                    weight: value.weight,
                    success_rate: value.success_rate,
                    first_token_ms: value.first_token_ms,
                    total_ms: value.total_ms,
                    output_tps: value.output_tps,
                    last_sample_at: value.last_sample_at,
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
                    // 权重与采样时刻都按真实值恢复，重启才不会凭空"洗白"一个
                    // 陈旧账号：`last_sample_at` 是**真正的采样时刻**（不是快照的
                    // `updated_at`——那个每 60 秒就被刷成当前时间，拿它当采样时刻
                    // 会让几天没请求的目标看起来刚被采样过，陈旧判定与探索口粮
                    // 一起失效）。
                    //
                    // 老库没有 weight 时用累计条数兜底（等价于旧语义）。注意
                    // last_sample_at 为 0 的行走 effective_samples 会直接归零：
                    // 兜底的 weight 只用于展示，不会让旧分数重新变得可信。
                    weight: if row.weight > 0.0 {
                        row.weight
                    } else {
                        row.samples.max(0) as f64
                    },
                    success_rate: row.success_rate,
                    first_token_ms: row.first_token_ms,
                    total_ms: row.total_ms,
                    output_tps: row.output_tps,
                    last_sample_at: row.last_sample_at,
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
    /// 这个候选此刻的探索口粮，已经含陈旧度放大（§9.5）。
    pub exploration: f64,
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
    fn of(candidates: &[Candidate], cheapest_in_group: Option<Multiplier>, now: i64) -> Self {
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
        //
        // "够多"是**时间衰减后**的够多：一个上周很快、此后没人打过的目标不算
        // 参照系成员，否则它会把"全组最快"这个位置长期占住，让所有当下正常的
        // 账号得分被压到 0.2 量级（§9.4 修订）。
        let warm = || candidates.iter().filter(|c| c.stats.is_warm(now));
        // 但**一个**热目标同样不构成参照系：它自动成为"最快"与"吞吐最高"，
        // 三项性能分全部拉满，而冷目标一律中性 0.6——差距与它实际快慢无关。
        // 现场表现就是"最先被抽中的那个账号从此赢家通吃"。不足两个热目标时
        // 放弃参照系，全体按中性分处理，等样本攒够再让性能说话（§9.4）。
        if warm().count() < MIN_FRAME_TARGETS {
            return Self {
                cheapest,
                fastest_first_token: f64::INFINITY,
                highest_tps: 0.0,
            };
        }
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

/// 这个目标此刻的探索口粮（§9.5）。
///
/// 底权的本意是"每个候选都有一份与分数无关的口粮"，让弱者攒得到样本回到评分里。
/// 但固定底权有个漏洞：**越久没被采样，越说明我们对它的判断已经过时**。一个
/// 两小时前的 0.9 只是历史，不是现在的证据；而它没被采样，恰恰是因为某个赢家
/// 把流量全吃掉了。
///
/// 所以口粮随"距上次采样多久"放大，被采样一次就立刻回落。真正差的目标不会因此
/// 长期占流量：采到样本、分数掉下去，口粮就恢复成基础值。
pub fn exploration_floor(stats: &Stats, now: i64) -> f64 {
    if stats.last_sample_at == 0 {
        // 从没被采样过：口粮直接给满，否则新账号永远攒不到 `MIN_SAMPLES`。
        return EXPLORATION * MAX_STALE_FACTOR;
    }
    let idle = (now - stats.last_sample_at).max(0) as f64;
    let factor = (1.0 + idle / STALE_UNIT_SECS).min(MAX_STALE_FACTOR);
    EXPLORATION * factor
}

/// 按 §9.4 给一组候选打分。
///
/// `cheapest_in_group` 是整个分组内最低的有效倍率；倍率是账号级属性，用分组
/// 作参照系才能让"同一个账号在不同模型下的倍率得分一致"。
pub fn score_all(
    candidates: &[Candidate],
    weights: SchedulingWeights,
    cheapest_in_group: Option<Multiplier>,
    now: i64,
) -> Vec<Score> {
    let reference = Reference::of(candidates, cheapest_in_group, now);
    candidates
        .iter()
        .map(|candidate| score_one(candidate, &reference, weights, now))
        .collect()
}

fn score_one(
    candidate: &Candidate,
    reference: &Reference,
    weights: SchedulingWeights,
    now: i64,
) -> Score {
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

    // 冷启动：有效样本不足时性能三维用保守中性分，倍率维正常参与（§9.4）。
    let (reliability, first_token, throughput) = if candidate.stats.is_warm(now) {
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
        exploration: exploration_floor(&candidate.stats, now),
    }
}

/// 按 `score^k` 加权随机排出层内的尝试顺序（§9.5）。
///
/// `boost` 与 `scores` 等长，是**逐候选的权重倍数**（软粘性用，见 §10.1 修订）。
/// 它必须作用在**权重**上而不是分数上：分数被 `total.clamp(.., 1.0)` 夹在 1.0，
/// 若先把分数乘以倍数再夹，任何大于 1 的分数都会撞到同一个上限，倍数形同虚设，
/// 甚至会让一个已经变差的绑定目标与健康目标并列。
///
/// 返回的是**顺序**而不是单个选择：第一个是本次抽中的目标，后面是它失败后
/// 依次尝试的备选。不放回抽样天然满足"层内先耗尽再降层"（§13.1）。
pub fn weighted_order(scores: &[Score], random: &mut impl FnMut() -> f64) -> Vec<usize> {
    weighted_order_with(scores, &[], random)
}

/// 带逐候选权重倍数的加权随机排序（软粘性用）。
///
/// `boost` 为空表示没有倾斜；否则 `boost[i]` 是 `scores[i]` 的权重倍数。
pub fn weighted_order_with(
    scores: &[Score],
    boost: &[f64],
    random: &mut impl FnMut() -> f64,
) -> Vec<usize> {
    // 底权在幂次**之外**相加：它保证的是"每个候选都有一份与分数无关的口粮"，
    // 而不是把分数拉平。
    let mut weights: Vec<f64> = scores
        .iter()
        .enumerate()
        .map(|(index, score)| {
            let base = score.total.max(MIN_SCORE).powi(POWER) + score.exploration;
            base * boost.get(index).copied().unwrap_or(1.0).max(0.0)
        })
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

    /// 测试统一的时间基准：样本都"刚刚采到"，于是时间衰减不起作用，
    /// 各用例只考察它本来要考察的那条规则。
    const NOW: i64 = 1_000;

    fn warm(multiplier_raw: &str, success: f64, first_token_ms: f64, tps: f64) -> Candidate {
        Candidate {
            target_id: multiplier_raw.into(),
            multiplier: multiplier(multiplier_raw),
            multiplier_stale: false,
            stats: Stats {
                samples: MIN_SAMPLES,
                // 权重给足以跨过门槛；采样时刻就是测试用的 NOW，于是此刻
                // （now = NOW）判定为新鲜。last_sample_at 为 0 在新语义里是
                // "采样时刻未知"，等价于完全过期，不能拿来当"刚采过"。
                weight: MIN_SAMPLES as f64,
                success_rate: success,
                first_token_ms,
                total_ms: first_token_ms * 4.0,
                output_tps: tps,
                last_sample_at: NOW,
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
        let scores = score_all(&candidates, SchedulingWeights::default(), None, NOW);
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
        let scores = score_all(&candidates, SchedulingWeights::default(), None, NOW);
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
        let scores = score_all(&candidates, SchedulingWeights::default(), None, NOW);
        assert!(
            (scores[1].multiplier - 0.625).abs() < 0.01,
            "{:?}",
            scores[1]
        );
    }

    #[test]
    fn cold_targets_use_a_neutral_performance_score_but_a_real_multiplier_score() {
        let mut cold = warm("0.1", 1.0, 100.0, 100.0);
        // 可信判据是**有效样本量**（时间衰减后的 weight），不是累计条数。
        cold.stats.weight = (MIN_SAMPLES - 1) as f64;
        let candidates = vec![cold, warm("0.2", 1.0, 900.0, 40.0)];
        let scores = score_all(&candidates, SchedulingWeights::default(), None, NOW);

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
        lucky.stats.weight = 2.0;
        // 另有两个成熟目标，参照系才成立（只有一个热目标时全体中性，见下一个测试）。
        let candidates = vec![
            lucky,
            warm("0.5", 1.0, 1000.0, 50.0),
            warm("0.5", 1.0, 2000.0, 25.0),
        ];
        let scores = score_all(&candidates, SchedulingWeights::default(), None, NOW);
        assert_eq!(scores[1].first_token, 1.0, "成熟目标仍是参照系里最快的");
        assert_eq!(scores[1].throughput, 1.0);
        // 跑过两次的那个目标（10ms、500tps）没有资格参与参照系。
        assert!(scores[0].first_token < 1.0);
    }

    /// **一个**热目标不构成参照系（§9.4 修订）。
    ///
    /// 只把自己算成"全组最快"，三项性能分就会自动拉满，与它实际快慢无关；
    /// 这会把"最先拿到样本的目标"永久钉在榜首，形成赢家通吃。不足两个热目标
    /// 时全体退回中性分，先按倍率与探索口粮铺开流量，等样本够了再让性能说话。
    #[test]
    fn a_single_warm_target_does_not_become_the_whole_reference_frame() {
        // 唯一的热目标速度其实很平庸（5 秒首字、5 tps），冷目标连样本都没有。
        let only_warm = warm("0.5", 1.0, 5000.0, 5.0);
        let cold = Candidate {
            stats: Stats::default(),
            ..only_warm.clone()
        };

        let scores = score_all(&[only_warm, cold], SchedulingWeights::default(), None, NOW);
        // 它是参照系里唯一的点，但一个点不构成参照系：不给它满分。
        assert_eq!(
            scores[0].first_token, NEUTRAL,
            "一个热目标不足以定义最快的参照系"
        );
        assert_eq!(scores[0].throughput, NEUTRAL);
        // 冷目标同样中性：两者在性能维度上被拉平，只剩可靠性与倍率说话。
        assert_eq!(scores[1].first_token, NEUTRAL);
        assert_eq!(scores[1].throughput, NEUTRAL);
        assert_eq!(scores[0].first_token, scores[1].first_token);
        assert_eq!(scores[0].throughput, scores[1].throughput);
    }

    /// 两个热目标时参照系恢复正常：快的那一个拿到性能满分。
    #[test]
    fn two_warm_targets_restore_the_reference_frame() {
        let candidates = vec![
            warm("0.5", 1.0, 10.0, 500.0),
            warm("0.5", 1.0, 1000.0, 50.0),
        ];
        let scores = score_all(&candidates, SchedulingWeights::default(), None, NOW);
        assert_eq!(scores[0].first_token, 1.0);
        assert_eq!(scores[0].throughput, 1.0);
        assert!(scores[1].first_token < 1.0);
    }

    /// 有效样本量随时间衰减：半衰期处恰好剩一半。
    #[test]
    fn evidence_decays_with_a_half_life() {
        let mut stats = Stats::default();
        // 一瞬间喂 10 条（此刻 weight = 10）。
        for _ in 0..10 {
            stats.observe(
                &Sample {
                    success: true,
                    counts: true,
                    first_token: Some(Duration::from_millis(100)),
                    total: Duration::from_millis(400),
                    output_tokens: Some(30),
                },
                NOW,
            );
        }
        assert!((stats.effective_samples(NOW) - 10.0).abs() < 1e-9);
        let half = NOW + SAMPLE_HALF_LIFE_SECS as i64;
        assert!(
            (stats.effective_samples(half) - 5.0).abs() < 1e-6,
            "半衰期处应剩一半，实际 {}",
            stats.effective_samples(half)
        );
        // 两个半衰期后剩四分之一。
        let two = NOW + 2 * SAMPLE_HALF_LIFE_SECS as i64;
        assert!((stats.effective_samples(two) - 2.5).abs() < 1e-6);
        // 累计条数不受衰减影响（它只是"采过多少条"的展示值）。
        assert_eq!(stats.samples, 10);
    }

    /// 持续观测会收敛到一个与阈值无关的高位：门槛只影响"多久被采信"，
    /// 不影响稳定态。这正是"10 与 20 差别不大"的量化依据。
    #[test]
    fn steady_state_evidence_dwarfs_the_threshold() {
        let mut stats = Stats::default();
        // 每 60 秒一次，跑 30 分钟。
        let mut at = NOW;
        for _ in 0..30 {
            stats.observe(
                &Sample {
                    success: true,
                    counts: true,
                    first_token: Some(Duration::from_millis(100)),
                    total: Duration::from_millis(400),
                    output_tokens: Some(30),
                },
                at,
            );
            at += 60;
        }
        let effective = stats.effective_samples(at);
        assert!(
            effective > 25.0,
            "每分钟一次时稳态有效样本量应当远高于门槛，实际 {effective}"
        );
        assert!(stats.is_warm(at));
    }

    /// **核心回归**：一个上周很快、此后没被采样的账号，必须退出参照系，
    /// 不能继续把当下正常的账号压成低分（§9.4 修订）。
    #[test]
    fn a_stale_target_stops_defining_the_reference_frame() {
        let week = 7 * 24 * 3600;
        // 三个账号：一个"陈旧很快"（从未参与本次评分的时间窗口），
        // 两个当下正常。
        let stale = Candidate {
            target_id: "stale".into(),
            multiplier: multiplier("0.5"),
            multiplier_stale: false,
            stats: Stats {
                samples: 300,
                weight: 300.0,
                success_rate: 1.0,
                first_token_ms: 200.0,
                total_ms: 1_000.0,
                output_tps: 80.0,
                // 一周前采样过。
                last_sample_at: NOW - week,
            },
        };
        let normal = |id: &str, first_ms: f64| Candidate {
            target_id: id.into(),
            multiplier: multiplier("0.5"),
            multiplier_stale: false,
            stats: Stats {
                samples: 300,
                weight: 300.0,
                success_rate: 1.0,
                first_token_ms: first_ms,
                total_ms: 4_000.0,
                output_tps: 30.0,
                last_sample_at: NOW,
            },
        };

        let candidates = vec![stale, normal("b", 800.0), normal("c", 1000.0)];
        let scores = score_all(&candidates, SchedulingWeights::default(), None, NOW);

        // 陈旧账号退回中性分，不再以"全组最快"的身份出现。
        assert_eq!(
            scores[0].first_token, NEUTRAL,
            "一周前的证据不得继续定义参照系"
        );
        assert_eq!(scores[0].throughput, NEUTRAL);
        // 当下正常的账号之间仍然正常比较：最快的拿满分。
        assert_eq!(scores[1].first_token, 1.0, "当下最快的就是参照系里最快的");
        assert!(scores[2].first_token < 1.0);
        assert!(
            scores[1].first_token > scores[0].first_token,
            "正常账号必须高于陈旧账号，否则旧分数会一直压住它们"
        );
    }

    /// **现场回归**：慢账号不能被时间衰减踢出参照系。
    ///
    /// 现场（gpt-boom / gpt-5.6-sol）除主力外四个账号的实测速率是
    /// 0.71~2.30 条/小时；半衰期如果取得太短（例如最初写的 3 小时，门槛速率
    /// 2.31 条/小时），三个慢账号会被判成"冷"，参照系重新退化回"只有主力一个
    /// 热目标"——那正是要修的那个病，而且会被这个参数变相放大。
    ///
    /// 这里按现场最慢的实际速率（0.71 条/小时 ≈ 每 85 分钟一条）喂一整天的
    /// 样本，断言它仍然算"热"。
    #[test]
    fn the_half_life_keeps_a_slow_but_steady_target_warm() {
        // 0.71 条/小时 —— 现场最慢账号的实测速率。
        let interval = (3600.0 / 0.71) as i64;
        let mut stats = Stats::default();
        let mut at = NOW;
        // 连续观测 48 小时（远超一个半衰期，进入稳态）。
        let mut fed = 0;
        while at < NOW + 48 * 3600 {
            stats.observe(
                &Sample {
                    success: true,
                    counts: true,
                    first_token: Some(Duration::from_millis(900)),
                    total: Duration::from_millis(4_000),
                    output_tokens: Some(30),
                },
                at,
            );
            fed += 1;
            at += interval;
        }
        let effective = stats.effective_samples(at);
        assert!(
            fed > 30,
            "48 小时里按 0.71/小时应当采到 30 条以上，实际 {fed}"
        );
        assert!(
            stats.is_warm(at),
            "0.71 条/小时的稳定账号必须仍然算热，否则它会被踢出参照系；             稳态有效样本量 = {effective}（门槛 {MIN_SAMPLES}）"
        );
    }

    /// 反面：真正的闲置必须让证据过期。
    #[test]
    fn sustained_idleness_does_expire_the_evidence() {
        let mut stats = Stats::default();
        for _ in 0..300 {
            stats.observe(
                &Sample {
                    success: true,
                    counts: true,
                    first_token: Some(Duration::from_millis(200)),
                    total: Duration::from_millis(1_000),
                    output_tokens: Some(80),
                },
                NOW,
            );
        }
        assert!(stats.is_warm(NOW));
        // 一个半衰期后还剩一半（150），仍然可信——这是有意的容忍度。
        assert!(stats.is_warm(NOW + SAMPLE_HALF_LIFE_SECS as i64));
        // 五天之后彻底过期：不再以旧分数占住参照系。
        assert!(!stats.is_warm(NOW + 5 * 24 * 3600));
    }

    /// 反面：同一个账号只要**重新被采样**，就立刻回到参照系。
    ///
    /// 这保证衰减不会变成"永久惩罚"——它只是要求证据保持新鲜。
    #[test]
    fn a_refreshed_target_rejoins_the_reference_frame() {
        let week = 7 * 24 * 3600;
        let mut stats = Stats {
            samples: 300,
            weight: 300.0,
            success_rate: 1.0,
            first_token_ms: 200.0,
            total_ms: 1_000.0,
            output_tps: 80.0,
            last_sample_at: NOW - week,
        };
        assert!(!stats.is_warm(NOW), "一周没采样时不可信");

        // 采一次：权重从 0 重新累积（旧权重已经完全过期）。
        stats.observe(
            &Sample {
                success: true,
                counts: true,
                first_token: Some(Duration::from_millis(200)),
                total: Duration::from_millis(1_000),
                output_tokens: Some(80),
            },
            NOW,
        );
        assert!(
            (stats.effective_samples(NOW) - 1.0).abs() < 1e-9,
            "过期权重不得复活"
        );
        assert!(!stats.is_warm(NOW), "一条新样本还不够回到可信");

        // 继续采样直到重新可信。注意不能断言"恰好 N 条就够"：有效样本量是
        // **带衰减累加**的量，观测之间总要衰减掉一点，因此跨时间的 N 条会
        // 略小于 N。门槛量的是"当下证据有多厚"，不是"采过多少条"。
        let mut at = NOW;
        let mut fed = 1;
        while !stats.is_warm(at) {
            at += 60;
            fed += 1;
            stats.observe(
                &Sample {
                    success: true,
                    counts: true,
                    first_token: Some(Duration::from_millis(200)),
                    total: Duration::from_millis(1_000),
                    output_tokens: Some(80),
                },
                at,
            );
            assert!(fed < 30, "重新采样后应当很快回到可信，实际喂了 {fed} 条");
        }
        assert!(
            fed >= MIN_SAMPLES as usize,
            "至少要有门槛那么多条证据，实际 {fed}"
        );
    }

    /// 采样时刻未知（老库快照）按完全过期处理，不得靠一次观测复活旧权重。
    #[test]
    fn unknown_sample_time_never_claims_freshness() {
        let stats = Stats {
            samples: 500,
            weight: 500.0,
            success_rate: 1.0,
            first_token_ms: 100.0,
            total_ms: 400.0,
            output_tps: 50.0,
            last_sample_at: 0,
        };
        assert_eq!(stats.effective_samples(1_000_000), 0.0);
        assert!(!stats.is_warm(1_000_000));
    }

    /// 门槛本身是 10（§9.4 修订）：滑动证据下它只影响首次接入。
    #[test]
    fn the_warm_threshold_is_the_documented_value() {
        assert_eq!(MIN_SAMPLES, 10);
    }

    /// 恢复快照不能把"快照写入时刻"冒充成"最近采样时刻"（§9.5）。
    ///
    /// `export()` 每 60 秒把所有行的 `updated_at` 刷成当前时间，所以一个几天没
    /// 请求的目标也会看起来刚更新过。如果拿它当采样时刻，陈旧目标在重启后既
    /// 拿不到放大的口粮，又会带着旧分数继续占着参照系（§9.4 修订）。
    #[test]
    fn a_restored_snapshot_counts_as_stale_not_fresh() {
        let registry = Registry::new();
        registry.restore(&[PerfSnapshotRow {
            target_id: "t1".into(),
            protocol: crate::domain::Protocol::OpenAiChat,
            streaming: false,
            samples: 500,
            // 老库没有 weight 列，兜底用累计条数。
            weight: 0.0,
            success_rate: 1.0,
            first_token_ms: 100.0,
            total_ms: 400.0,
            output_tps: 50.0,
            // 老库也没有"真正的采样时刻"。
            last_sample_at: 0,
            updated_at: 1_000_000,
        }]);
        let stats = registry.stats(
            "t1",
            Dimension {
                protocol: crate::domain::Protocol::OpenAiChat,
                streaming: false,
            },
        );
        assert_eq!(stats.samples, 500);
        assert_eq!(stats.last_sample_at, 0, "快照没有采样时刻，必须按陈旧处理");
        assert!(
            !stats.is_warm(1_000_000),
            "采样时刻未知时不得声称可信，否则它会带着旧分数占住参照系"
        );
        assert_eq!(
            stats.effective_samples(1_000_000),
            0.0,
            "采样时刻未知等于完全过期"
        );
        assert!(
            exploration_floor(&stats, 1_000_000) > EXPLORATION,
            "陈旧目标必须拿到放大的口粮"
        );
    }

    /// ⑤ 探索口粮随"距上次采样多久"放大，被采样一次就回落（§9.5）。
    #[test]
    fn a_stale_target_earns_a_bigger_exploration_allowance() {
        let stats = Stats {
            samples: MIN_SAMPLES,
            last_sample_at: 1_000,
            ..Stats::default()
        };
        // 刚刚采过样：只有基础口粮。
        assert!((exploration_floor(&stats, 1_000) - EXPLORATION).abs() < 1e-12);
        // 十分钟没采样：翻倍。
        assert!((exploration_floor(&stats, 1_600) - EXPLORATION * 2.0).abs() < 1e-12);
        // 再久就封顶，不能让一个老目标把流量全吸走。
        assert!(
            (exploration_floor(&stats, 100_000) - EXPLORATION * MAX_STALE_FACTOR).abs() < 1e-12
        );
        // 从没采样过的新目标直接给满，否则永远攒不到 MIN_SAMPLES。
        assert!(
            (exploration_floor(&Stats::default(), 1_000) - EXPLORATION * MAX_STALE_FACTOR).abs()
                < 1e-12
        );
    }

    /// 上半条的效果：陈旧目标的份额确实被抬起来，但仍然抢不走主力。
    #[test]
    fn the_stale_allowance_shifts_share_toward_the_unmeasured_target() {
        let hot = score(0.95);
        let stale_but_untouched = score(0.55);
        let stale_with_allowance = Score {
            exploration: EXPLORATION * MAX_STALE_FACTOR,
            ..stale_but_untouched
        };
        let without = simulate(&[hot, stale_but_untouched], 40_000);
        let with = simulate(&[hot, stale_with_allowance], 40_000);
        assert!(
            with[1] > without[1] * 1.5,
            "陈旧目标必须拿到明显更多口粮：{without:?} -> {with:?}"
        );
        assert!(with[1] < 0.5, "但主力仍然拿走大多数流量：{with:?}");
    }

    #[test]
    fn the_exploration_floor_keeps_every_candidate_fed() {
        let share = simulate(&[score(0.95), score(0.55)], 40_000);
        // 主力仍然拿走绝大多数流量：底权不是平均分配。
        assert!(share[0] > 0.9, "主力应当拿到绝大多数流量：{share:?}");
        // 但弱者必须拿得到足够的样本量级：>3% 意味着 600 次请求就能攒到 20 个。
        assert!(
            share[1] > 0.03,
            "弱者必须攒得到样本，否则永远回不到评分里：{share:?}"
        );
    }

    #[test]
    fn reliability_is_scored_not_merely_left_to_the_breaker() {
        // 稳定 85% 成功率永远不会被熔断，但每 7 次就要重试一次，必须降分。
        let candidates = vec![
            warm("0.5", 1.0, 900.0, 50.0),
            warm("0.50", 0.85, 900.0, 50.0),
        ];
        let scores = score_all(&candidates, SchedulingWeights::default(), None, NOW);
        assert!(scores[1].total < scores[0].total);
    }

    #[test]
    fn a_stale_multiplier_is_penalised_but_still_selectable() {
        let mut stale = warm("0.5", 1.0, 900.0, 50.0);
        stale.multiplier_stale = true;
        let candidates = vec![warm("0.5", 1.0, 900.0, 50.0), stale];
        let scores = score_all(&candidates, SchedulingWeights::default(), None, NOW);
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
            0,
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
            0,
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
            0,
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

    /// 软粘性的倍数必须作用在**权重**上，不能作用在分数上（§10.1 修订）。
    ///
    /// 这条测试钉的是这个修复里最危险的一个坑。分数被 `total.clamp(MIN_SCORE, 1.0)`
    /// 夹在 1.0，把倍数乘在**分数**上再夹会得到一个**非单调**的有效倍数：
    ///
    /// | 绑定目标分数 | 乘分数再夹的有效倍数 |
    /// |---|---|
    /// | 0.90 | 2.3x |
    /// | 0.80 | 6.0x |
    /// | 0.40 | **1526x** |
    /// | ≤0.25 | 65536x（撞满分） |
    ///
    /// 也就是说**越差的绑定目标反而被放大得越狠**：一个已经掉到 0.40 的账号会
    /// 从健康的 0.90 手里抢走约七成流量——正是本修复要解决的问题换了个形式复发。
    /// 正确做法是让倍数与分数无关，这里用两个不同分数段验证有效倍数恒定。
    #[test]
    fn the_soft_affinity_boost_scales_weights_not_scores() {
        // 有效倍数 = (权重比) / (分数^k 比)，应当恒等于声明的 4 倍。
        let effective = |bound_score: f64, other_score: f64| {
            let scores = vec![score(other_score), score(bound_score)];
            let plain = simulate_with_boost(&scores, &[], 200_000);
            let boosted = simulate_with_boost(&scores, &[1.0, 4.0], 200_000);
            (boosted[1] / plain[1]) * (plain[0] / boosted[0])
        };

        // 分数高低完全不影响有效倍数——这正是"作用在权重上"的定义。
        for (bound, other) in [(0.85, 0.80), (0.60, 0.90), (0.40, 0.90), (0.30, 0.95)] {
            let ratio = effective(bound, other);
            assert!(
                (ratio - 4.0).abs() < 0.6,
                "分数 {bound}/{other} 的有效倍数应当恒为 4 倍，实际 {ratio:.2}"
            );
        }

        // 同分候选：约八成流量留在绑定目标（4 倍权重的直观含义）。
        let equal = simulate_with_boost(&[score(0.8), score(0.8)], &[1.0, 4.0], 40_000);
        assert!(
            (equal[1] - 0.8).abs() < 0.05,
            "同分时绑定目标应当拿到约八成：{equal:?}"
        );

        // 一个**已经变差**的绑定目标不该靠倍数反超健康目标。
        let degraded = simulate_with_boost(&[score(0.90), score(0.40)], &[1.0, 4.0], 40_000);
        assert!(
            degraded[0] > degraded[1],
            "健康目标必须仍然占优，变差的绑定目标不能反超：{degraded:?}"
        );
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
                exploration: EXPLORATION,
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
            exploration: EXPLORATION,
        }
    }

    /// 同 `simulate`，但可以给每个候选一个权重倍数（软粘性）。
    fn simulate_with_boost(scores: &[Score], boost: &[f64], rounds: usize) -> Vec<f64> {
        let mut counts = vec![0usize; scores.len()];
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut random = || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 11) as f64 / (1u64 << 53) as f64
        };
        for _ in 0..rounds {
            let order = weighted_order_with(scores, boost, &mut random);
            counts[order[0]] += 1;
        }
        counts
            .into_iter()
            .map(|count| count as f64 / rounds as f64)
            .collect()
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
            stats.observe(
                &Sample {
                    success: true,
                    counts: true,
                    first_token: Some(Duration::from_millis(1000)),
                    total: Duration::from_millis(4000),
                    output_tokens: Some(400),
                },
                0,
            );
        }
        assert!((stats.first_token_ms - 1000.0).abs() < 1.0);
        assert!((stats.output_tps - 100.0).abs() < 1.0);

        // 变快之后要在几十个样本内跟上，而不是被历史拖住。
        for _ in 0..20 {
            stats.observe(
                &Sample {
                    success: true,
                    counts: true,
                    first_token: Some(Duration::from_millis(200)),
                    total: Duration::from_millis(1000),
                    output_tokens: Some(400),
                },
                0,
            );
        }
        assert!(stats.first_token_ms < 400.0, "{}", stats.first_token_ms);
    }

    #[test]
    fn a_failed_request_lowers_reliability_without_faking_speed() {
        let mut stats = Stats::default();
        stats.observe(
            &Sample {
                success: true,
                counts: true,
                first_token: Some(Duration::from_millis(1000)),
                total: Duration::from_millis(4000),
                output_tokens: Some(400),
            },
            0,
        );
        let fast_first_token = stats.first_token_ms;

        // 0.2 秒就 500 的失败请求不该让这个目标显得"很快"。
        stats.observe(
            &Sample {
                success: false,
                counts: true,
                first_token: Some(Duration::from_millis(1)),
                total: Duration::from_millis(200),
                output_tokens: None,
            },
            0,
        );
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
        // 模拟半小时内每分钟一次的真实流量（30 条），而不是把 30 条挤在同一
        // 瞬间——`weight` 是**速率**量纲的量（稳态约等于 观测速率 × 半衰期 /
        // ln2），同一瞬间堆出来的条数并不代表持续的观测速率。
        let mut at = 1_000;
        for _ in 0..30 {
            registry.observe(
                "tgt",
                dimension,
                &Sample {
                    success: true,
                    counts: true,
                    first_token: Some(Duration::from_millis(700)),
                    total: Duration::from_millis(3000),
                    output_tokens: Some(300),
                },
                at,
            );
            at += 60;
        }
        let exported = registry.export(at);
        assert_eq!(exported.len(), 1);

        // 重启：新表从快照恢复，评分不从零开始（§26.7）。
        let restored = Registry::new();
        restored.restore(&exported);
        let stats = restored.stats("tgt", dimension);
        assert!(
            stats.is_warm(at + 60),
            "刚导出的快照紧接着恢复，仍然新鲜：{}",
            stats.effective_samples(at + 60)
        );
        assert!((stats.first_token_ms - 700.0).abs() < 1.0);

        // 一周不采样：证据彻底过期，必须重新采样才能回到可信状态。
        //
        // 不能只放一天——一天恰好是一个半衰期，30 条样本衰减到 15 条仍在门槛
        // 之上，那正是"半衰期要能容忍慢账号"的设计意图（见
        // `SAMPLE_HALF_LIFE_SECS` 的下界推导）。
        assert!(
            !stats.is_warm(at + 7 * 24 * 3600),
            "一周不采样的证据不得继续声称可信：{}",
            stats.effective_samples(at + 7 * 24 * 3600)
        );

        restored.retain(&[]);
        assert!(!restored.stats("tgt", dimension).is_warm(1_000));
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
                    counts: true,
                    first_token: None,
                    total: Duration::from_millis(10),
                    output_tokens: None,
                },
                0,
            );
        }
        assert!(
            registry.len() <= MAX_TRACKED_TARGETS + 1,
            "统计表不该无界增长：{}",
            registry.len()
        );
    }
}
