//! 每目标独立队列与分组总容量（§13.6）。
//!
//! 队列按调度目标划分，不是分组单队列。分组单队列会产生队头阻塞：一个粘在
//! 忙账号上、愿意等 90 秒的大请求，会把后面两个本可以立刻走别的账号的小请求
//! 一起卡住 90 秒。
//!
//! 分组只负责一件事：**所有队列加起来的总容量上限**。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::health::{Capacity, CapacityPermit};

/// 等待的结果。
#[derive(Debug)]
pub enum WaitOutcome {
    /// 某个目标空出了名额：索引指向传入的候选列表，名额随结果一起交给调用方。
    ///
    /// 名额必须带走而不是在这里释放：tokio 的 Semaphore 按 FIFO 唤醒，带着它
    /// 进门才能保证等了 30 秒的请求不会在最后一步被刚到的新请求插队。
    Ready(usize, OwnedSemaphorePermit),
    /// 等待预算耗尽。粘性请求据此降级为无粘性请求重新选择（§10.3）。
    TimedOut,
}

/// 分组级的排队总容量。
///
/// 这是**唯一**的分组级队列语义：它不排序、不决定谁先走，只回答"现在还能不
/// 能再多一个人在等"。超出即 `queue_full`。
pub struct GroupQueues {
    inner: RwLock<HashMap<String, Arc<Semaphore>>>,
}

impl Default for GroupQueues {
    fn default() -> Self {
        Self::new()
    }
}

impl GroupQueues {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// 占一个排队名额。返回 `None` 表示分组队列已满。
    ///
    /// `capacity` 为 0 表示"不排队"：任何需要等待的请求直接失败。
    pub fn enter(&self, group_id: &str, capacity: u32) -> Option<QueueTicket> {
        if capacity == 0 {
            return None;
        }
        let semaphore = self.semaphore(group_id, capacity);
        semaphore
            .try_acquire_owned()
            .ok()
            .map(|permit| QueueTicket { _permit: permit })
    }

    /// 当前正在排队的请求数，供后台展示。
    pub fn waiting(&self, group_id: &str, capacity: u32) -> u32 {
        let semaphore = self.semaphore(group_id, capacity);
        capacity.saturating_sub(semaphore.available_permits() as u32)
    }

    fn semaphore(&self, group_id: &str, capacity: u32) -> Arc<Semaphore> {
        if let Ok(guard) = self.inner.read()
            && let Some(found) = guard.get(group_id)
        {
            return Arc::clone(found);
        }
        let mut guard = crate::sync::write(&self.inner);
        Arc::clone(
            guard
                .entry(group_id.to_string())
                .or_insert_with(|| Arc::new(Semaphore::new(capacity as usize))),
        )
    }

    /// 丢弃已不存在的分组。
    pub fn retain(&self, live_groups: &[String]) {
        if let Ok(mut guard) = self.inner.write() {
            guard.retain(|id, _| live_groups.iter().any(|live| live == id));
        }
    }
}

/// 排队名额。析构即归还，客户端断开时自然退出队列（§13.6）。
pub struct QueueTicket {
    _permit: OwnedSemaphorePermit,
}

/// 等待一组目标中**任意一个**空出并发名额。
///
/// 这正是"无粘性、当前层全满 → 进入任意目标队列；任何一个目标释放槽位即唤醒"
/// 的实现：`select_all` 在第一个 `acquire` 完成时返回，其余等待被丢弃。
///
/// 传入单个目标时它同时也是"粘性命中但目标忙"的等待路径。
pub async fn wait_for_any(permits: Vec<Arc<Semaphore>>, budget: Duration) -> WaitOutcome {
    if permits.is_empty() || budget.is_zero() {
        return WaitOutcome::TimedOut;
    }

    let acquisitions = permits.into_iter().enumerate().map(|(index, semaphore)| {
        Box::pin(async move {
            // 名额只是"门票"：真正的准入还要重做倍率与健康终检（§13.1）。
            semaphore
                .acquire_owned()
                .await
                .ok()
                .map(|permit| (index, permit))
        })
    });

    match tokio::time::timeout(budget, futures::future::select_all(acquisitions)).await {
        Ok((Some((index, permit)), _, _)) => WaitOutcome::Ready(index, permit),
        // 只有 Semaphore 被关闭才会拿到 None，本进程内不会发生。
        _ => WaitOutcome::TimedOut,
    }
}

/// 等待一个候选的账号共享容量与目标局部容量同时可用。
///
/// `shutdown` 置位时立即返回 `ShuttingDown`：关闭流程不该被排队请求拖住
/// （§25.3 第 2 步）。
pub async fn wait_for_any_capacity(
    capacities: Vec<Capacity>,
    budget: Duration,
    shutdown: &mut tokio::sync::watch::Receiver<bool>,
) -> CapacityWaitOutcome {
    if capacities.is_empty() || budget.is_zero() {
        return CapacityWaitOutcome::TimedOut;
    }
    // 信号可能在订阅之前就置位；先查一次，避免 select 永远等不到变化。
    if *shutdown.borrow() {
        return CapacityWaitOutcome::ShuttingDown;
    }
    let acquisitions = capacities.into_iter().enumerate().map(|(index, capacity)| {
        Box::pin(async move { capacity.acquire().await.map(|permit| (index, permit)) })
    });
    tokio::select! {
        result = tokio::time::timeout(budget, futures::future::select_all(acquisitions)) => {
            match result {
                Ok((Some((index, permit)), _, _)) => CapacityWaitOutcome::Ready(index, permit),
                _ => CapacityWaitOutcome::TimedOut,
            }
        }
        _ = shutdown.changed() => CapacityWaitOutcome::ShuttingDown,
    }
}

#[derive(Debug)]
pub enum CapacityWaitOutcome {
    Ready(usize, CapacityPermit),
    TimedOut,
    /// 服务正在关闭：排队中的请求立即取消，而不是等到宽限期结束。
    ShuttingDown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_full_group_queue_refuses_new_waiters() {
        let queues = GroupQueues::new();
        let _first = queues.enter("g1", 2).unwrap();
        let second = queues.enter("g1", 2).unwrap();
        assert!(queues.enter("g1", 2).is_none(), "超出总容量必须 queue_full");
        assert_eq!(queues.waiting("g1", 2), 2);

        drop(second);
        assert!(queues.enter("g1", 2).is_some());
    }

    #[tokio::test]
    async fn a_zero_capacity_group_never_queues() {
        let queues = GroupQueues::new();
        assert!(queues.enter("g1", 0).is_none());
    }

    #[tokio::test]
    async fn releasing_any_target_wakes_the_waiter() {
        let busy = Arc::new(Semaphore::new(1));
        let idle = Arc::new(Semaphore::new(1));
        let held_busy = Arc::clone(&busy).try_acquire_owned().unwrap();
        let held_idle = Arc::clone(&idle).try_acquire_owned().unwrap();

        let waiter = tokio::spawn(wait_for_any(
            vec![Arc::clone(&busy), Arc::clone(&idle)],
            Duration::from_secs(5),
        ));
        // 第二个目标先空出来，等待者就该被它唤醒，而不是死等第一个。
        drop(held_idle);
        let outcome = waiter.await.unwrap();
        assert!(matches!(outcome, WaitOutcome::Ready(1, _)), "{outcome:?}");
        // 名额随结果带走：在它被释放前，空闲目标对别人仍然是满的。
        assert!(Arc::clone(&idle).try_acquire_owned().is_err());
        drop(outcome);
        assert!(Arc::clone(&idle).try_acquire_owned().is_ok());
        drop(held_busy);
    }

    #[tokio::test(start_paused = true)]
    async fn an_exhausted_budget_times_out_instead_of_hanging() {
        let full = Arc::new(Semaphore::new(1));
        let _held = Arc::clone(&full).try_acquire_owned().unwrap();
        assert!(matches!(
            wait_for_any(vec![full], Duration::from_secs(3)).await,
            WaitOutcome::TimedOut
        ));
    }

    #[tokio::test]
    async fn a_zero_budget_switches_immediately() {
        // 小请求的等待预算是 0：重建缓存几乎不花钱，等待纯属浪费（§10.3）。
        let full = Arc::new(Semaphore::new(1));
        let _held = Arc::clone(&full).try_acquire_owned().unwrap();
        assert!(matches!(
            wait_for_any(vec![full], Duration::ZERO).await,
            WaitOutcome::TimedOut
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_long_sticky_wait_does_not_block_a_ready_request() {
        // §26.3：粘性请求长等待不阻塞可立即执行的无粘性请求。
        let sticky_target = Arc::new(Semaphore::new(1));
        let free_target = Arc::new(Semaphore::new(1));
        let _held = Arc::clone(&sticky_target).try_acquire_owned().unwrap();

        // 粘性请求死等自己那个已满的目标，预算 90 秒。
        let sticky = tokio::spawn(wait_for_any(
            vec![Arc::clone(&sticky_target)],
            Duration::from_secs(90),
        ));
        // 无粘性请求根本不排队——它的目标是空的，立刻拿到名额。
        let free = Arc::clone(&free_target).try_acquire_owned();
        assert!(free.is_ok(), "空闲目标必须立即可用，不能被粘性请求卡住");

        tokio::time::advance(Duration::from_secs(91)).await;
        assert!(matches!(sticky.await.unwrap(), WaitOutcome::TimedOut));
    }

    #[tokio::test]
    async fn a_dropped_ticket_leaves_the_queue() {
        // 客户端断开时 QueueTicket 随请求任务一起析构，名额立刻归还。
        let queues = GroupQueues::new();
        {
            let _ticket = queues.enter("g1", 1).unwrap();
            assert!(queues.enter("g1", 1).is_none());
        }
        assert!(queues.enter("g1", 1).is_some());
    }

    /// 关闭信号必须打断"等容量"，而不是让排队请求干等到宽限期结束。
    #[tokio::test]
    async fn the_shutdown_signal_cancels_a_capacity_wait() {
        use crate::health::Capacity;

        // 容量被占满：两个信号量都没有空闲名额。
        let account = Arc::new(tokio::sync::Semaphore::new(1));
        let target = Arc::new(tokio::sync::Semaphore::new(1));
        let _held_account = account.clone().try_acquire_owned().unwrap();
        let _held_target = target.clone().try_acquire_owned().unwrap();
        let capacity = Capacity { account, target };

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let waiting = tokio::spawn(async move {
            wait_for_any_capacity(vec![capacity], Duration::from_secs(600), &mut shutdown_rx).await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown_tx.send(true).unwrap();

        let outcome = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .expect("关闭信号必须唤醒等待")
            .unwrap();
        assert!(
            matches!(outcome, CapacityWaitOutcome::ShuttingDown),
            "关闭时必须返回 ShuttingDown"
        );
    }

    /// 信号在订阅之前就已置位时也不能漏判。
    #[tokio::test]
    async fn an_already_shutting_down_signal_is_not_missed() {
        use crate::health::Capacity;

        let account = Arc::new(tokio::sync::Semaphore::new(0));
        let target = Arc::new(tokio::sync::Semaphore::new(0));
        let capacity = Capacity { account, target };
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        shutdown_tx.send(true).unwrap();

        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            wait_for_any_capacity(vec![capacity], Duration::from_secs(600), &mut shutdown_rx),
        )
        .await
        .expect("不能卡住");
        assert!(matches!(outcome, CapacityWaitOutcome::ShuttingDown));
    }
}
