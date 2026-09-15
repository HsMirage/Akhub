//! `std::sync` 锁的毒化恢复辅助。
//!
//! 恢复后拿到的守卫数据可能处于部分更新状态，但这比让服务在一次持锁 panic
//! 后永久返回 500 更合适。为控制这一取舍的风险，锁内临界区必须只做不会
//! panic 的简单操作。

/// 获取互斥锁守卫；锁被毒化时记录错误并恢复其中的数据。
pub fn lock<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::error!("检测到 Mutex 已被毒化，已恢复并继续使用锁内数据");
            poisoned.into_inner()
        }
    }
}

/// 获取读锁守卫；锁被毒化时记录错误并恢复其中的数据。
pub fn read<T>(lock: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    match lock.read() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::error!("检测到 RwLock 已被毒化，已恢复读锁并继续使用锁内数据");
            poisoned.into_inner()
        }
    }
}

/// 获取写锁守卫；锁被毒化时记录错误并恢复其中的数据。
pub fn write<T>(lock: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    match lock.write() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::error!("检测到 RwLock 已被毒化，已恢复写锁并继续使用锁内数据");
            poisoned.into_inner()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, RwLock};
    use std::thread;

    use super::{lock, read, write};

    #[test]
    fn mutex_recovers_after_a_thread_panics_while_holding_the_lock() {
        let mutex = Arc::new(Mutex::new(1));
        let poisoned = Arc::clone(&mutex);

        assert!(
            thread::spawn(move || {
                let mut value = poisoned.lock().unwrap();
                *value = 2;
                panic!("poison mutex");
            })
            .join()
            .is_err()
        );

        let recovered = Arc::clone(&mutex);
        thread::spawn(move || {
            let mut value = lock(&recovered);
            assert_eq!(*value, 2);
            *value = 3;
        })
        .join()
        .unwrap();

        assert_eq!(*lock(&mutex), 3);
    }

    #[test]
    fn rwlock_read_and_write_recover_after_a_writer_panics() {
        let lock = Arc::new(RwLock::new(1));
        let poisoned = Arc::clone(&lock);

        assert!(
            thread::spawn(move || {
                let mut value = poisoned.write().unwrap();
                *value = 2;
                panic!("poison rwlock");
            })
            .join()
            .is_err()
        );

        let recovered = Arc::clone(&lock);
        thread::spawn(move || {
            assert_eq!(*read(&recovered), 2);
            *write(&recovered) = 3;
            assert_eq!(*read(&recovered), 3);
        })
        .join()
        .unwrap();
    }
}
