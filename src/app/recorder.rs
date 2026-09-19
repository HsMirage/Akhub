//! 请求元数据的有界通道与批量落盘任务（§19.4）。
//!
//! 热路径只做一次 `try_send`。通道满时**丢弃记录并计数**，绝不阻塞推理请求：
//! 日志是可观测性，不是业务结果，用它拖慢真实调用是本末倒置。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::storage::Store;
use crate::storage::store::RequestRecord;

/// 通道容量。按每条记录约 200 字节计，占用可忽略，但上限必须存在。
const CHANNEL_CAPACITY: usize = 4096;
/// 单批最多写入多少条。
const BATCH_SIZE: usize = 128;
/// 即使不满一批，也至少这么久刷一次盘。
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

/// 请求元数据的写入端。克隆开销极低，可自由分发到各请求任务。
#[derive(Clone)]
pub struct RequestRecorder {
    sender: mpsc::Sender<RequestRecord>,
    dropped: Arc<AtomicU64>,
    /// 保留期设为 0 时改走内存汇总（§24.2）；由设置服务在运行时可改。
    settings: crate::app::SettingsService,
    live: Arc<crate::app::live_stats::LiveStats>,
}

impl RequestRecorder {
    /// 启动后台批量写入任务并返回写入端。
    pub fn spawn(
        store: Store,
        settings: crate::app::SettingsService,
        live: Arc<crate::app::live_stats::LiveStats>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
        let recorder = Self {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            settings,
            live,
        };
        tokio::spawn(drain(store, receiver));
        recorder
    }

    /// 记录一条请求元数据。永不阻塞，永不失败。
    ///
    /// 保留天数为 0 时**不落库**，只累加内存里的当日汇总（§3、§24.2）。
    /// 判断放在这里而不是调用点：写入路径只有这一处，不会漏。
    pub fn record(&self, record: RequestRecord) {
        if self.settings.get().retention_days == 0 {
            self.live.record(&record);
            return;
        }
        if self.sender.try_send(record).is_err() {
            let total = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            // 只在 2 的幂次上告警，避免故障时日志自身变成压力源。
            if total.is_power_of_two() {
                tracing::warn!(dropped = total, "请求元数据通道已满，正在丢弃记录");
            }
        }
    }

    /// 因通道满而被丢弃的记录总数，用于概览页诊断。
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// 后台任务：攒批写入，通道关闭后把剩余记录刷完再退出。
async fn drain(store: Store, mut receiver: mpsc::Receiver<RequestRecord>) {
    let mut batch = Vec::with_capacity(BATCH_SIZE);
    let mut ticker = tokio::time::interval(FLUSH_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            received = receiver.recv() => match received {
                Some(record) => {
                    batch.push(record);
                    if batch.len() >= BATCH_SIZE {
                        flush(&store, &mut batch).await;
                    }
                }
                // 发送端全部释放：刷完最后一批后退出。
                None => {
                    flush(&store, &mut batch).await;
                    return;
                }
            },
            _ = ticker.tick() => flush(&store, &mut batch).await,
        }
    }
}

async fn flush(store: &Store, batch: &mut Vec<RequestRecord>) {
    if batch.is_empty() {
        return;
    }
    if let Err(error) = store.insert_request_records(batch).await {
        // 写盘失败不能影响热路径，也不该无限重试堆积内存：记录并丢弃这一批。
        tracing::error!(%error, count = batch.len(), "写入请求元数据失败，本批已丢弃");
    }
    batch.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Protocol;

    fn record(id: &str) -> RequestRecord {
        RequestRecord {
            request_id: id.into(),
            started_at: 1_000,
            duration_ms: 5,
            protocol: Protocol::OpenAiChat,
            streaming: false,
            group_id: None,
            logical_model: Some("glm-4.6".into()),
            target_id: None,
            account_id: None,
            upstream_model: None,
            request_bytes: 128,
            upstream_status: Some(200),
            http_status: 200,
            error_code: None,
            endpoint: None,
            degraded: None,
            effective_multiplier: None,
            cheapest_multiplier: None,
            dearest_multiplier: None,
            attempts: 1,
            first_token_ms: None,
            input_tokens: None,
            output_tokens: None,
            config_version: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            sticky_wait_ms: None,
            sticky_freshness: None,
            output_tps: None,
            multiplier_source: None,
            quota_status: None,
            filter_summary: None,
            selected_layer: None,
            attempts_detail: Vec::new(),
            queued_ms: 0,
            sticky_hit: false,
        }
    }

    /// 保留期非 0 时的记录器（默认 30 天）。
    fn recorder_for(
        store: Store,
    ) -> (
        RequestRecorder,
        Arc<crate::app::live_stats::LiveStats>,
        crate::app::SettingsService,
    ) {
        let live = Arc::new(crate::app::live_stats::LiveStats::new());
        let settings = crate::app::SettingsService::new(crate::app::Settings::default());
        (
            RequestRecorder::spawn(store, settings.clone(), Arc::clone(&live)),
            live,
            settings,
        )
    }

    #[tokio::test]
    async fn records_reach_the_database_through_the_channel() {
        let store = Store::new(crate::storage::open_in_memory().await.unwrap());
        let (recorder, _, _) = recorder_for(store.clone());
        for i in 0..5 {
            recorder.record(record(&format!("req_{i}")));
        }
        drop(recorder);

        // 通道关闭后后台任务会刷完剩余记录；给它一个调度机会。
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if store.list_request_records(10, 0).await.unwrap().len() == 5 {
                return;
            }
        }
        panic!("记录未在预期时间内落盘");
    }

    #[tokio::test]
    async fn a_full_channel_drops_records_instead_of_blocking() {
        let store = Store::new(crate::storage::open_in_memory().await.unwrap());
        let (sender, _receiver) = mpsc::channel(2);
        let recorder = RequestRecorder {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
            settings: crate::app::SettingsService::new(crate::app::Settings::default()),
            live: Arc::new(crate::app::live_stats::LiveStats::new()),
        };
        let _ = store;

        // 接收端不消费，通道只有 2 个位置：后续调用必须立即返回而不是挂起。
        for i in 0..10 {
            recorder.record(record(&format!("req_{i}")));
        }
        assert_eq!(recorder.dropped(), 8);
    }

    /// 保留期为 0 时不落库，只进内存汇总；改回非 0 后恢复落库（§3、§24.2）。
    #[tokio::test]
    async fn zero_retention_keeps_records_in_memory_only() {
        let store = Store::new(crate::storage::open_in_memory().await.unwrap());
        let live = Arc::new(crate::app::live_stats::LiveStats::new());
        let settings = crate::app::SettingsService::new(crate::app::Settings::default());
        let recorder = RequestRecorder::spawn(store.clone(), settings.clone(), Arc::clone(&live));

        // 先把保留期设为 0：记录只进内存。
        settings.replace(crate::app::Settings {
            retention_days: 0,
            ..Default::default()
        });
        let now = crate::storage::now_unix();
        for i in 0..3 {
            let mut item = record(&format!("mem_{i}"));
            item.started_at = now;
            recorder.record(item);
        }
        // 给后台任务一点时间；这段时间里数据库必须一直是空的。
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(
            store.list_request_records(10, 0).await.unwrap().is_empty(),
            "保留期为 0 时不该写明细"
        );
        let snapshot = live.snapshot(crate::storage::now_unix());
        assert_eq!(snapshot.requests, 3, "内存汇总要收到这 3 条");
        assert_eq!(snapshot.success, 3);

        // 改回 30 天：后续记录重新落库。
        settings.replace(crate::app::Settings::default());
        recorder.record(record("req_persisted"));
        drop(recorder);
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            if !store.list_request_records(10, 0).await.unwrap().is_empty() {
                return;
            }
        }
        panic!("恢复保留期后应当重新落库");
    }

    /// 内存汇总跨天清零，但小时桶保留，概览的趋势图不会断（§24.2）。
    #[tokio::test]
    async fn live_stats_roll_over_at_the_day_boundary() {
        let live = crate::app::live_stats::LiveStats::new();
        let base = 1_700_000_000 - (1_700_000_000 % 86_400);
        let mut first = record("day1");
        first.started_at = base + 10;
        live.record(&first);
        assert_eq!(live.snapshot(base + 100).requests, 1);

        let mut second = record("day2");
        second.started_at = base + 86_400 + 10;
        live.record(&second);
        let snapshot = live.snapshot(base + 86_400 + 100);
        assert_eq!(snapshot.requests, 1, "跨天后当日计数应当从 1 重新开始");
        assert_eq!(snapshot.buckets.len(), 2, "小时桶要保留，趋势才连续");
    }

    /// 没有目标 ID 的记录不进目标维度，但会计入总量（与读库口径一致）。
    #[tokio::test]
    async fn live_stats_keep_per_model_usage_for_the_cost_page() {
        let live = crate::app::live_stats::LiveStats::new();
        let mut first = record("cost1");
        first.started_at = crate::storage::now_unix();
        first.group_id = Some("g1".into());
        first.logical_model = Some("glm-4.6".into());
        first.account_id = Some("acc1".into());
        first.effective_multiplier = Some(crate::domain::Multiplier::parse("0.5").unwrap());
        live.record(&first);

        let usage = live.usage(crate::storage::now_unix());
        let entry = usage
            .get(&("g1".to_string(), "glm-4.6".to_string()))
            .unwrap();
        assert_eq!(entry.requests, 1);
        assert_eq!(entry.per_account.get("acc1").unwrap().0, 1);

        let samples = live.samples(crate::storage::now_unix());
        let by_multiplier = &samples
            .get(&("g1".to_string(), "glm-4.6".to_string()))
            .unwrap()
            .by_multiplier;
        assert_eq!(by_multiplier.len(), 1);
    }
}
