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
}

impl RequestRecorder {
    /// 启动后台批量写入任务并返回写入端。
    pub fn spawn(store: Store) -> Self {
        let (sender, receiver) = mpsc::channel(CHANNEL_CAPACITY);
        let recorder = Self {
            sender,
            dropped: Arc::new(AtomicU64::new(0)),
        };
        tokio::spawn(drain(store, receiver));
        recorder
    }

    /// 记录一条请求元数据。永不阻塞，永不失败。
    pub fn record(&self, record: RequestRecord) {
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
            queued_ms: 0,
            sticky_hit: false,
        }
    }

    #[tokio::test]
    async fn records_reach_the_database_through_the_channel() {
        let store = Store::new(crate::storage::open_in_memory().await.unwrap());
        let recorder = RequestRecorder::spawn(store.clone());
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
        };
        let _ = store;

        // 接收端不消费，通道只有 2 个位置：后续调用必须立即返回而不是挂起。
        for i in 0..10 {
            recorder.record(record(&format!("req_{i}")));
        }
        assert_eq!(recorder.dropped(), 8);
    }
}
