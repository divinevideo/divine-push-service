//! Liveness tracking for the service's critical background tasks.
//!
//! The delivery pipeline runs in detached Tokio tasks. A panic inside one of
//! them unwinds only that task: the process survives, the HTTP server keeps
//! answering, and nothing else observes the death. This module gives the health
//! endpoint and the shutdown path a shared view of which tasks are still alive,
//! so a dead pipeline surfaces as an unhealthy pod instead of a silent outage.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// A background task whose death means the service can no longer deliver
/// notifications, and whose liveness `/health` therefore reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CriticalTask {
    NostrListener,
    EventHandler,
}

impl CriticalTask {
    pub const ALL: [CriticalTask; 2] = [CriticalTask::NostrListener, CriticalTask::EventHandler];

    /// Stable identifier used in log fields and in the `/health` body.
    pub fn name(self) -> &'static str {
        match self {
            CriticalTask::NostrListener => "nostr_listener",
            CriticalTask::EventHandler => "event_handler",
        }
    }
}

/// Shared liveness state for the critical tasks.
#[derive(Debug)]
pub struct TaskHealth {
    nostr_listener: AtomicBool,
    event_handler: AtomicBool,
    unexpected_exit: AtomicBool,
}

impl TaskHealth {
    pub fn new() -> Self {
        Self {
            nostr_listener: AtomicBool::new(true),
            event_handler: AtomicBool::new(true),
            unexpected_exit: AtomicBool::new(false),
        }
    }

    fn flag(&self, task: CriticalTask) -> &AtomicBool {
        match task {
            CriticalTask::NostrListener => &self.nostr_listener,
            CriticalTask::EventHandler => &self.event_handler,
        }
    }

    pub fn mark_dead(&self, task: CriticalTask) {
        self.flag(task).store(false, Ordering::SeqCst);
    }

    pub fn is_alive(&self, task: CriticalTask) -> bool {
        self.flag(task).load(Ordering::SeqCst)
    }

    pub fn all_alive(&self) -> bool {
        CriticalTask::ALL.iter().all(|task| self.is_alive(*task))
    }

    /// Records that a task ended without a shutdown having been requested. The
    /// process exits non-zero on this, so the restart is visible in pod state
    /// rather than looking like a clean shutdown.
    pub fn mark_unexpected_exit(&self) {
        self.unexpected_exit.store(true, Ordering::SeqCst);
    }

    pub fn had_unexpected_exit(&self) -> bool {
        self.unexpected_exit.load(Ordering::SeqCst)
    }
}

impl Default for TaskHealth {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether a task returning normally counts as a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnReturn {
    /// The task is expected to run for the lifetime of the process, so any
    /// return before shutdown means something broke.
    Fatal,
    /// The task may legitimately finish on its own — the cleanup service
    /// returns `Ok(())` immediately when disabled or misconfigured. Treating
    /// that as failure would crash-loop the whole service over an optional
    /// feature being switched off.
    Tolerated,
}

/// Fires when a tracked task ends, whether it returned normally or panicked.
///
/// A panic unwinds through the task's locals, so dropping this guard is the one
/// signal that catches both. Without it a panicking task is invisible: the
/// `JoinError` is only observable at join time, which happens after shutdown has
/// already been triggered.
struct TaskExitGuard {
    name: &'static str,
    critical: Option<CriticalTask>,
    on_return: OnReturn,
    health: Arc<TaskHealth>,
    token: CancellationToken,
}

impl Drop for TaskExitGuard {
    fn drop(&mut self) {
        // True only while this thread unwinds, which is exactly the case where
        // the task did not choose to end. It separates "finished" from "died"
        // without the task having to report anything.
        let panicked = std::thread::panicking();

        if !panicked {
            if self.token.is_cancelled() {
                tracing::info!(task = self.name, "Task finished during shutdown.");
                return;
            }

            if self.on_return == OnReturn::Tolerated {
                tracing::info!(
                    task = self.name,
                    "Optional task finished on its own; leaving the rest of the service running."
                );
                return;
            }
        }

        if let Some(task) = self.critical {
            self.health.mark_dead(task);
        }

        tracing::error!(
            task = self.name,
            panicked,
            "Task exited unexpectedly - cancelling remaining tasks so the process exits and is restarted."
        );
        self.health.mark_unexpected_exit();
        self.token.cancel();
    }
}

/// Spawns and supervises the service's long-lived tasks.
///
/// Any task ending before shutdown is requested cancels the shared token, which
/// unblocks `main`'s shutdown path so the process exits and the pod restarts.
pub struct TaskTracker {
    handles: Vec<tokio::task::JoinHandle<()>>,
    health: Arc<TaskHealth>,
    token: CancellationToken,
}

impl TaskTracker {
    pub fn new(health: Arc<TaskHealth>, token: CancellationToken) -> Self {
        Self {
            handles: Vec::new(),
            health,
            token,
        }
    }

    /// Spawns a supervised task. `critical` names the task in `/health`; pass
    /// `None` for tasks whose death should still stop the process but which the
    /// health endpoint does not report individually. `on_return` says whether
    /// finishing normally is a failure — a panic is always a failure regardless.
    pub fn spawn<F>(
        &mut self,
        name: &'static str,
        critical: Option<CriticalTask>,
        on_return: OnReturn,
        future: F,
    ) where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let guard = TaskExitGuard {
            name,
            critical,
            on_return,
            health: Arc::clone(&self.health),
            token: self.token.clone(),
        };

        self.handles.push(tokio::spawn(async move {
            // Held across the await so it drops on panic as well as on return.
            let _guard = guard;
            future.await;
        }));
    }

    pub async fn wait(self) {
        for handle in self.handles {
            let _ = handle.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_all_alive() {
        let health = TaskHealth::new();
        assert!(health.all_alive());
        assert!(!health.had_unexpected_exit());
    }

    #[test]
    fn marking_one_task_dead_fails_the_aggregate() {
        let health = TaskHealth::new();
        health.mark_dead(CriticalTask::EventHandler);

        assert!(!health.is_alive(CriticalTask::EventHandler));
        assert!(health.is_alive(CriticalTask::NostrListener));
        assert!(!health.all_alive());
    }

    #[test]
    fn tracks_unexpected_exit_independently_of_liveness() {
        let health = TaskHealth::new();
        health.mark_unexpected_exit();

        assert!(health.had_unexpected_exit());
        // A task can exit unexpectedly without any *critical* task dying, e.g.
        // the cleanup service, so liveness must not be inferred from it.
        assert!(health.all_alive());
    }

    #[test]
    fn task_names_are_stable() {
        assert_eq!(CriticalTask::NostrListener.name(), "nostr_listener");
        assert_eq!(CriticalTask::EventHandler.name(), "event_handler");
    }

    /// The outage this guards against: an FCM 5xx panicked the event handler,
    /// the `JoinError` was discarded, the token was never cancelled, and the
    /// pod served 200 for 59 hours with a dead delivery pipeline.
    #[tokio::test]
    async fn panicking_task_marks_itself_dead_and_triggers_shutdown() {
        let health = Arc::new(TaskHealth::new());
        let token = CancellationToken::new();
        let mut tracker = TaskTracker::new(Arc::clone(&health), token.clone());

        tracker.spawn(
            "event_handler",
            Some(CriticalTask::EventHandler),
            OnReturn::Fatal,
            async {
                panic!("simulated FCM panic");
            },
        );
        tracker.wait().await;

        assert!(!health.is_alive(CriticalTask::EventHandler));
        assert!(!health.all_alive());
        assert!(health.had_unexpected_exit());
        assert!(token.is_cancelled(), "shutdown must be triggered");
    }

    #[tokio::test]
    async fn task_returning_early_also_triggers_shutdown() {
        let health = Arc::new(TaskHealth::new());
        let token = CancellationToken::new();
        let mut tracker = TaskTracker::new(Arc::clone(&health), token.clone());

        // A clean `return` is just as fatal as a panic: the listener exits
        // normally once its event channel closes.
        tracker.spawn(
            "nostr_listener",
            Some(CriticalTask::NostrListener),
            OnReturn::Fatal,
            async {},
        );
        tracker.wait().await;

        assert!(!health.is_alive(CriticalTask::NostrListener));
        assert!(health.had_unexpected_exit());
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn task_finishing_during_shutdown_is_not_an_unexpected_exit() {
        let health = Arc::new(TaskHealth::new());
        let token = CancellationToken::new();
        let mut tracker = TaskTracker::new(Arc::clone(&health), token.clone());

        token.cancel();
        tracker.spawn(
            "event_handler",
            Some(CriticalTask::EventHandler),
            OnReturn::Fatal,
            async {},
        );
        tracker.wait().await;

        assert!(
            !health.had_unexpected_exit(),
            "a clean shutdown must exit zero"
        );
    }

    #[tokio::test]
    async fn non_critical_task_death_stops_the_process_without_failing_liveness() {
        let health = Arc::new(TaskHealth::new());
        let token = CancellationToken::new();
        let mut tracker = TaskTracker::new(Arc::clone(&health), token.clone());

        tracker.spawn("cleanup_service", None, OnReturn::Fatal, async {});
        tracker.wait().await;

        assert!(health.all_alive(), "no critical task died");
        assert!(health.had_unexpected_exit());
        assert!(token.is_cancelled());
    }

    /// `run_cleanup_service` returns `Ok(())` immediately when cleanup is
    /// disabled or misconfigured. Treating that as failure would crash-loop the
    /// entire push service because an optional feature was switched off.
    #[tokio::test]
    async fn tolerated_task_finishing_normally_does_not_stop_the_service() {
        let health = Arc::new(TaskHealth::new());
        let token = CancellationToken::new();
        let mut tracker = TaskTracker::new(Arc::clone(&health), token.clone());

        tracker.spawn("cleanup_service", None, OnReturn::Tolerated, async {});
        tracker.wait().await;

        assert!(!health.had_unexpected_exit(), "must not force a restart");
        assert!(
            !token.is_cancelled(),
            "the rest of the service must keep running"
        );
        assert!(health.all_alive());
    }

    /// Tolerating a normal return must not tolerate a crash: a panic is never
    /// something the task chose.
    #[tokio::test]
    async fn tolerated_task_that_panics_still_stops_the_service() {
        let health = Arc::new(TaskHealth::new());
        let token = CancellationToken::new();
        let mut tracker = TaskTracker::new(Arc::clone(&health), token.clone());

        tracker.spawn("cleanup_service", None, OnReturn::Tolerated, async {
            panic!("simulated cleanup panic");
        });
        tracker.wait().await;

        assert!(health.had_unexpected_exit(), "a panic is always fatal");
        assert!(token.is_cancelled());
    }
}
