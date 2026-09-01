use super::*;

#[derive(Default)]
struct CancellationState {
    cancelled: AtomicBool,
    waiters: StdMutex<Vec<Waker>>,
}

/// 可在 runtime、核心状态机和工具适配器之间传递的轻量取消信号。
#[derive(Clone, Default)]
pub struct CancellationToken {
    state: Shared<CancellationState>,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if self.state.cancelled.swap(true, Ordering::AcqRel) {
            return;
        }

        let waiters = {
            let mut waiters = self
                .state
                .waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *waiters)
        };
        for waiter in waiters {
            waiter.wake();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        futures_util::future::poll_fn(|context| {
            if self.is_cancelled() {
                return Poll::Ready(());
            }

            let mut waiters = self
                .state
                .waiters
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if self.is_cancelled() {
                return Poll::Ready(());
            }
            if !waiters
                .iter()
                .any(|waiter| waiter.will_wake(context.waker()))
            {
                waiters.push(context.waker().clone());
            }
            Poll::Pending
        })
        .await
    }
}

impl fmt::Debug for CancellationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CancellationToken")
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}
