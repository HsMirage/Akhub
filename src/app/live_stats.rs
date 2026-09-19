//! 保留期为 0 时的内存实时汇总（§3、§24.2）。
//!
//! 保留天数设为 0 的意思是"不新增历史明细"，而不是"什么都看不见"：概览与成本页
//! 仍然要能显示**当天**的情况。所以走这条路径的请求不进 SQLite，只在这里累加。
//!
//! 内存占用必须有界——否则"保护隐私"会变成"把内存吃光"：
//! - 延迟样本只保留最近 LATENCY_SAMPLES 个，够算分位即可；
//! - 最近错误只保留 RECENT_ERRORS 条；
//! - 其余全是定长计数器，按天滚动。

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use crate::storage::store::RequestRecord;

/// 用于分位数的延迟样本上限。
const LATENCY_SAMPLES: usize = 4_096;
/// 概览页展示的最近错误条数上限。
const RECENT_ERRORS: usize = 20;
/// 一天的秒数。
const DAY: i64 = 86_400;
/// 小时桶宽度。
const HOUR: i64 = 3_600;
/// 小时桶保留数量（24 小时 + 当前这个不满的）。
const BUCKETS: usize = 25;

/// 一条内存里的最近错误，字段是请求记录的子集。
#[derive(Debug, Clone)]
pub struct LiveError {
    pub request_id: String,
    pub started_at: i64,
    pub logical_model: Option<String>,
    pub target_id: Option<String>,
    pub http_status: i64,
    pub error_code: Option<String>,
}

/// 一个（分组, 逻辑模型）的当日用量。
#[derive(Debug, Clone, Default)]
pub struct LiveModelUsage {
    pub requests: i64,
    pub tokens: i64,
    /// 账号 ID → （请求数, token 数）。
    pub per_account: HashMap<String, (i64, i64)>,
}

/// 一个（分组, 逻辑模型）的倍率样本：定点倍率原值 → 请求数。
#[derive(Debug, Clone, Default)]
pub struct LiveMultiplierSample {
    pub by_multiplier: HashMap<i64, i64>,
}

#[derive(Debug, Default)]
struct Inner {
    /// 当前统计的自然日（unix 秒的日起点）；跨天自动清零。
    day_start: i64,
    requests: i64,
    success: i64,
    queue_timeouts: i64,
    tokens: i64,
    latencies: VecDeque<i64>,
    recent_errors: VecDeque<LiveError>,
    usage: HashMap<(String, String), LiveModelUsage>,
    samples: HashMap<(String, String), LiveMultiplierSample>,
    /// 小时桶：(bucket_start, 请求数, 成功数)。
    buckets: VecDeque<(i64, i64, i64)>,
}

/// 保留期为 0 时的内存汇总表。
#[derive(Debug, Default)]
pub struct LiveStats {
    inner: Mutex<Inner>,
}

/// 概览用的当日快照。
#[derive(Debug, Clone)]
pub struct LiveSnapshot {
    pub day_start: i64,
    pub requests: i64,
    pub success: i64,
    pub queue_timeouts: i64,
    pub tokens: i64,
    pub p50_latency_ms: Option<i64>,
    pub p95_latency_ms: Option<i64>,
    pub avg_latency_ms: Option<i64>,
    pub recent_errors: Vec<LiveError>,
    /// （bucket_start, 请求数, 成功数）
    pub buckets: Vec<(i64, i64, i64)>,
}

impl LiveStats {
    pub fn new() -> Self {
        Self::default()
    }

    /// 记一条请求。只做内存操作、不碰磁盘，可以在热路径上直接调用。
    pub fn record(&self, record: &RequestRecord) {
        let mut inner = crate::sync::lock(&self.inner);
        inner.roll_day(record.started_at);
        let success = (200..300).contains(&record.http_status);
        inner.requests += 1;
        if success {
            inner.success += 1;
        }
        if record.error_code.as_deref() == Some("queue_timeout") {
            inner.queue_timeouts += 1;
        }
        let tokens = record.input_tokens.unwrap_or(0) + record.output_tokens.unwrap_or(0);
        inner.tokens += tokens;
        if record.duration_ms > 0 {
            if inner.latencies.len() == LATENCY_SAMPLES {
                inner.latencies.pop_front();
            }
            inner.latencies.push_back(record.duration_ms);
        }
        if !success {
            if inner.recent_errors.len() == RECENT_ERRORS {
                inner.recent_errors.pop_front();
            }
            inner.recent_errors.push_back(LiveError {
                request_id: record.request_id.clone(),
                started_at: record.started_at,
                logical_model: record.logical_model.clone(),
                target_id: record.target_id.clone(),
                http_status: record.http_status,
                error_code: record.error_code.clone(),
            });
        }
        let bucket = (record.started_at / HOUR) * HOUR;
        match inner.buckets.back_mut() {
            Some(last) if last.0 == bucket => {
                last.1 += 1;
                if success {
                    last.2 += 1;
                }
            }
            _ => {
                if inner.buckets.len() == BUCKETS {
                    inner.buckets.pop_front();
                }
                inner.buckets.push_back((bucket, 1, i64::from(success)));
            }
        }
        if let (Some(group), Some(model)) = (&record.group_id, &record.logical_model) {
            let key = (group.clone(), model.clone());
            let usage = inner.usage.entry(key.clone()).or_default();
            usage.requests += 1;
            usage.tokens += tokens;
            if let Some(account) = &record.account_id {
                let entry = usage.per_account.entry(account.clone()).or_default();
                entry.0 += 1;
                entry.1 += tokens;
            }
            // 与读库路径的口径一致：只有成功请求的有效倍率进样本，
            // 失败请求的倍率不参与加权均倍率（§6.8、§11.6）。
            if success && let Some(multiplier) = record.effective_multiplier {
                inner
                    .samples
                    .entry(key)
                    .or_default()
                    .by_multiplier
                    .entry(multiplier.raw())
                    .and_modify(|count| *count += 1)
                    .or_insert(1);
            }
        }
    }

    /// 当日快照，供概览页使用。
    pub fn snapshot(&self, now: i64) -> LiveSnapshot {
        let mut inner = crate::sync::lock(&self.inner);
        inner.roll_day(now);
        let mut latencies: Vec<i64> = inner.latencies.iter().copied().collect();
        latencies.sort_unstable();
        LiveSnapshot {
            day_start: inner.day_start,
            requests: inner.requests,
            success: inner.success,
            queue_timeouts: inner.queue_timeouts,
            tokens: inner.tokens,
            p50_latency_ms: percentile(&latencies, 0.50),
            p95_latency_ms: percentile(&latencies, 0.95),
            avg_latency_ms: (!latencies.is_empty())
                .then(|| latencies.iter().sum::<i64>() / latencies.len() as i64),
            recent_errors: inner.recent_errors.iter().cloned().collect(),
            buckets: inner.buckets.iter().copied().collect(),
        }
    }

    /// 当日每个（分组, 逻辑模型）的用量，供成本页使用。
    pub fn usage(&self, now: i64) -> HashMap<(String, String), LiveModelUsage> {
        let mut inner = crate::sync::lock(&self.inner);
        inner.roll_day(now);
        inner.usage.clone()
    }

    /// 当日每个（分组, 逻辑模型）的倍率样本。
    pub fn samples(&self, now: i64) -> HashMap<(String, String), LiveMultiplierSample> {
        let mut inner = crate::sync::lock(&self.inner);
        inner.roll_day(now);
        inner.samples.clone()
    }

    /// 清空（恢复备份后调用：账号与模型可能整批换了）。
    pub fn clear(&self) {
        let mut inner = crate::sync::lock(&self.inner);
        *inner = Inner::default();
    }
}

impl Inner {
    /// 跨天就把当日统计清零：保留期为 0 时只承诺"当日数据"（§24.2）。
    ///
    /// **只向前滚**。跨过午夜仍在跑的流式请求，结算时带的是昨天的 `started_at`，
    /// 如果按它回滚就会把今天已经累计的统计全部清空——那正好发生在一天里流量
    /// 最需要被看见的时候。时间戳倒退的记录直接计入当前这一天。
    fn roll_day(&mut self, now: i64) {
        let day = (now / DAY) * DAY;
        if day <= self.day_start {
            return;
        }
        // 趋势图跨天仍然连续，所以小时桶保留。
        let buckets = std::mem::take(&mut self.buckets);
        *self = Inner {
            day_start: day,
            buckets,
            ..Inner::default()
        };
    }
}

/// 最近邻分位，与 SQL 版本的口径一致（§6.2）。
fn percentile(sorted: &[i64], ratio: f64) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let index = ((sorted.len() as f64 - 1.0) * ratio).round() as usize;
    sorted.get(index).copied()
}
