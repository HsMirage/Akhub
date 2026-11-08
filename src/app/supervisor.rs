//! 后台任务监督（§19.4、§22）。
//!
//! 后台循环里的一次 panic 不该让它永远停掉：清理、快照、请求记录落盘停了
//! 之后不会有任何报错，只会让数据库悄悄长大、统计悄悄断档。这里把任务放进
//! 子任务里跑，panic 时记日志并按退避重启；任务正常返回（例如 `AppState`
//! 已释放）就结束监督。

use std::future::Future;
use std::time::Duration;

/// 首次重启前的等待。
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
/// 退避上限。
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// 在监督下运行 `make` 产生的任务；panic 后用新的实例重启。
pub fn supervise<F, Fut>(name: &'static str, make: F)
where
    F: Fn() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let mut backoff = INITIAL_BACKOFF;
        loop {
            match tokio::spawn(make()).await {
                Ok(()) => return,
                Err(error) if error.is_panic() => {
                    tracing::error!(
                        task = name,
                        retry_in_secs = backoff.as_secs(),
                        "后台任务 panic，稍后重启"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
                // 被取消：运行时正在关闭，不再重启。
                Err(_) => return,
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[tokio::test(start_paused = true)]
    async fn a_panicking_task_is_restarted_until_it_finishes() {
        let runs = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&runs);
        supervise("测试", move || {
            let counter = Arc::clone(&counter);
            async move {
                if counter.fetch_add(1, Ordering::SeqCst) < 2 {
                    panic!("故意的");
                }
            }
        });
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        assert_eq!(runs.load(Ordering::SeqCst), 3, "panic 两次后第三次正常结束");
    }
}
