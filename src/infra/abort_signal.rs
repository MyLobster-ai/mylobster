//! Unified abort lifecycle for channels and long-running tasks (v2026.2.25).
//!
//! Provides a cooperative cancellation pattern using `AtomicBool` + `Notify`.
//! Channels and background workers use `AbortHandle` to signal and wait for
//! graceful shutdown.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

/// A cooperative abort handle.
///
/// Clone-cheap (wraps `Arc`). Signal once, await many. Safe to call
/// `abort()` before any waiter has registered — the aborted flag is
/// checked on each `wait_for_abort()` call.
#[derive(Debug, Clone)]
pub struct AbortHandle {
    aborted: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl AbortHandle {
    /// Create a new abort handle.
    pub fn new() -> Self {
        Self {
            aborted: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Notify::new()),
        }
    }

    /// Signal all waiters that the operation should abort.
    pub fn abort(&self) {
        self.aborted.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Check whether abort has been signalled (non-blocking).
    pub fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::Acquire)
    }

    /// Wait until an abort signal is received.
    ///
    /// Returns immediately if `abort()` was already called.
    pub async fn wait_for_abort(&self) {
        // Register for notification first, then check the flag.
        // This avoids a race where abort() fires between checking
        // the flag and registering.
        loop {
            let notified = self.notify.notified();
            if self.aborted.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

impl Default for AbortHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Run a future with abort lifecycle monitoring.
///
/// If `abort_handle` fires before `task` completes, the task is dropped and
/// `Err("aborted")` is returned. If the task completes first, its result is
/// returned.
pub async fn monitor_with_abort_lifecycle<T, F>(
    task: F,
    abort_handle: &AbortHandle,
) -> Result<T, &'static str>
where
    F: Future<Output = T>,
{
    tokio::select! {
        result = task => Ok(result),
        _ = abort_handle.wait_for_abort() => Err("aborted"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn abort_handle_signals_waiters() {
        let handle = AbortHandle::new();
        let handle2 = handle.clone();

        let waiter = tokio::spawn(async move {
            handle2.wait_for_abort().await;
            true
        });

        // Give the waiter a moment to start.
        tokio::task::yield_now().await;
        handle.abort();

        assert!(waiter.await.unwrap());
    }

    #[tokio::test]
    async fn monitor_completes_normally() {
        let handle = AbortHandle::new();
        let result = monitor_with_abort_lifecycle(async { 42 }, &handle).await;
        assert_eq!(result, Ok(42));
    }

    #[tokio::test]
    async fn monitor_aborts_pending_task() {
        let handle = AbortHandle::new();
        // Pre-fire abort — wait_for_abort() should return immediately
        // thanks to the AtomicBool flag check.
        handle.abort();

        let result = monitor_with_abort_lifecycle(
            async {
                // This future would never complete on its own.
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                42
            },
            &handle,
        )
        .await;

        assert_eq!(result, Err("aborted"));
    }

    #[test]
    fn is_aborted_false_initially() {
        let handle = AbortHandle::new();
        assert!(!handle.is_aborted());
    }

    #[test]
    fn is_aborted_true_after_abort() {
        let handle = AbortHandle::new();
        handle.abort();
        assert!(handle.is_aborted());
    }
}
