//! Private task registry and cancellation management for inert API bots.
//!
//! # Lock Hierarchy & Synchronization
//! 1. `ApiBotRegistry::slots`: Synchronous `std::sync::Mutex`.
//!    - Used ONLY for atomic slot reservation, cancellation token lookup, and removing slot.
//!    - MUST NEVER hold this lock across any `.await` point.
//! 2. Asynchronous task execution & joining:
//!    - Spawned supervisor task watches the runner task, saves the typed outcome, removes slot under lock, and broadcasts via watch channel.
//!    - Guaranteed order: `remove slot under lock -> outcome_tx.send()`. Callers awaiting outcome always see slot cleared.
//!    - Multiple concurrent `cancel_and_join` callers share the exact same completion outcome without detach races.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Identifies the ownership of an active runner task.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RunTurnKey {
    pub(crate) run_id: String,
    pub(crate) turn_id: String,
}

/// Typed outcome when a runner task finishes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TurnOutcome {
    Completed,
    Cancelled,
    Failed(String),
}

/// Explicit errors for registry operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RegistryError {
    AlreadyRunning { existing_turn_id: String },
    NotFound,
    TaskPanicked,
    JoinFailed,
    ChannelClosedEarly,
    IdentityMismatch,
}

#[derive(Clone)]
struct ActiveSlot {
    turn_id: String,
    cancel_token: CancellationToken,
    outcome_rx: watch::Receiver<Option<Result<TurnOutcome, RegistryError>>>,
}

/// Helper function to await outcome from a watch receiver, used by both production and tests.
pub(crate) async fn wait_outcome(
    rx: &mut watch::Receiver<Option<Result<TurnOutcome, RegistryError>>>,
) -> Result<TurnOutcome, RegistryError> {
    if let Some(ref res) = *rx.borrow() {
        return res.clone();
    }
    loop {
        if rx.changed().await.is_err() {
            return Err(RegistryError::ChannelClosedEarly);
        }
        if let Some(ref res) = *rx.borrow() {
            return res.clone();
        }
    }
}

impl ApiBotRegistry {
    pub(crate) fn new() -> Self {
        Self {
            slots: Mutex::new(HashMap::new()),
        }
    }

    /// Atomically checks for an empty slot for `key.run_id`, binds `key.turn_id`, spawns the runner task
    /// along with a dedicated join supervisor, and stores the slot before releasing the mutex.
    ///
    /// Synchronous and non-blocking: returns `CancellationToken` immediately.
    /// If an active turn already occupies the run, returns `Err(RegistryError::AlreadyRunning)`.
    pub(crate) fn spawn<F, Fut>(
        self: &Arc<Self>,
        key: RunTurnKey,
        task_fn: F,
    ) -> Result<CancellationToken, RegistryError>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<TurnOutcome, RegistryError>> + Send + 'static,
    {
        let mut guard = self.slots.lock().expect("registry mutex not poisoned");
        if let Some(existing) = guard.get(&key.run_id) {
            return Err(RegistryError::AlreadyRunning {
                existing_turn_id: existing.turn_id.clone(),
            });
        }

        let cancel_token = CancellationToken::new();
        let (outcome_tx, outcome_rx) = watch::channel(None);

        let cancel_child = cancel_token.clone();
        // Box::pin prevents debug stack frame overflow (AGENTS.md #1465)
        let runner_handle = tokio::spawn(Box::pin(async move {
            task_fn(cancel_child).await
        }));

        // Dedicated supervisor task ensures the runner JoinHandle is never lost/detached,
        // even if cancel_and_join callers drop their futures.
        let registry_weak = Arc::downgrade(self);
        let run_id_clone = key.run_id.clone();
        let turn_id_clone = key.turn_id.clone();

        tokio::spawn(Box::pin(async move {
            let join_res = runner_handle.await;
            let typed_outcome = match join_res {
                Ok(outcome) => outcome,
                Err(e) => {
                    if e.is_panic() {
                        Err(RegistryError::TaskPanicked)
                    } else {
                        Err(RegistryError::JoinFailed)
                    }
                }
            };

            // Order invariant: Atomically remove slot under lock BEFORE broadcasting outcome.
            // Guarantees that any awaiter waking up from the watch channel immediately observes `is_active == false`.
            if let Some(reg) = registry_weak.upgrade() {
                let mut guard = reg.slots.lock().expect("registry mutex not poisoned");
                if let Some(slot) = guard.get(&run_id_clone) {
                    if slot.turn_id == turn_id_clone {
                        guard.remove(&run_id_clone);
                    }
                }
            }

            // Broadcast the terminal outcome to all current and future watchers
            let _ = outcome_tx.send(Some(typed_outcome));
        }));

        guard.insert(
            key.run_id,
            ActiveSlot {
                turn_id: key.turn_id,
                cancel_token: cancel_token.clone(),
                outcome_rx,
            },
        );

        Ok(cancel_token)
    }

    /// Checks if a run currently has an active runner slot.
    pub(crate) fn is_active(&self, run_id: &str) -> bool {
        let guard = self.slots.lock().expect("registry mutex not poisoned");
        guard.contains_key(run_id)
    }

    /// Signals cancellation and waits for the actual runner task to genuinely finish.
    ///
    /// Conservative semantics: If `run_id` is not found, returns `Err(RegistryError::NotFound)`.
    /// Once a task has finished and its slot is cleaned up, subsequent calls return `NotFound`.
    /// Callers (e.g. C2 lifecycle) MUST NOT treat NotFound as success.
    pub(crate) async fn cancel_and_join(&self, run_id: &str) -> Result<TurnOutcome, RegistryError> {
        let (token, mut rx) = {
            let guard = self.slots.lock().expect("registry mutex not poisoned");
            let slot = guard.get(run_id).ok_or(RegistryError::NotFound)?;
            (slot.cancel_token.clone(), slot.outcome_rx.clone())
        };

        // Fire cancellation token without holding registry lock
        token.cancel();

        wait_outcome(&mut rx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use tokio::sync::{oneshot, Notify};

    // Helper to poll once without sleeping or busy spinning
    fn poll_once<F: Future>(mut fut: Pin<&mut F>) -> Poll<F::Output> {
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        fut.as_mut().poll(&mut cx)
    }

    #[tokio::test]
    async fn test_slot_remains_occupied_until_real_join() {
        let registry = Arc::new(ApiBotRegistry::new());
        let key = RunTurnKey {
            run_id: "r1".into(),
            turn_id: "t1".into(),
        };

        let notify = Arc::new(Notify::new());
        let notify_clone = notify.clone();

        let _token = registry
            .spawn(key.clone(), move |_cancel| async move {
                notify_clone.notified().await;
                Ok(TurnOutcome::Completed)
            })
            .unwrap();

        assert!(registry.is_active("r1"));

        // Duplicate registration on same run rejected atomically
        let key_dup = RunTurnKey {
            run_id: "r1".into(),
            turn_id: "t2".into(),
        };
        match registry.spawn(key_dup, |_| async { Ok(TurnOutcome::Completed) }) {
            Err(RegistryError::AlreadyRunning { existing_turn_id }) => {
                assert_eq!(existing_turn_id, "t1");
            }
            other => panic!("expected AlreadyRunning, got {:?}", other),
        }

        // Release task and wait for completion
        notify.notify_one();

        let res = registry.cancel_and_join("r1").await;
        assert_eq!(res.unwrap(), TurnOutcome::Completed);
        // Guaranteed false immediately on return due to removal before broadcast
        assert!(!registry.is_active("r1"));

        // Once completed and removed, subsequent join returns conservative NotFound
        let after_res = registry.cancel_and_join("r1").await;
        assert_eq!(after_res, Err(RegistryError::NotFound));
    }

    #[tokio::test]
    async fn test_not_found_is_conservative_error() {
        let registry = Arc::new(ApiBotRegistry::new());
        let res = registry.cancel_and_join("nonexistent_run").await;
        assert_eq!(res, Err(RegistryError::NotFound));
    }

    #[tokio::test]
    async fn test_panic_is_typed_as_task_panicked() {
        let registry = Arc::new(ApiBotRegistry::new());
        let key = RunTurnKey {
            run_id: "r_panic".into(),
            turn_id: "t_panic".into(),
        };

        let _token = registry
            .spawn(key.clone(), |_| async move {
                panic!("controlled simulation panic for test");
                #[allow(unreachable_code)]
                Ok(TurnOutcome::Completed)
            })
            .unwrap();

        let res = registry.cancel_and_join("r_panic").await;
        assert_eq!(res, Err(RegistryError::TaskPanicked));
        assert!(!registry.is_active("r_panic"));
    }

    #[tokio::test]
    async fn test_concurrent_cancel_and_join_share_outcome() {
        let registry = Arc::new(ApiBotRegistry::new());
        let key = RunTurnKey {
            run_id: "r_conc".into(),
            turn_id: "t_conc".into(),
        };

        let notify = Arc::new(Notify::new());
        let notify_clone = notify.clone();

        let _token = registry
            .spawn(key.clone(), move |_| async move {
                notify_clone.notified().await;
                Ok(TurnOutcome::Completed)
            })
            .unwrap();

        let reg1 = registry.clone();
        let reg2 = registry.clone();

        let mut fut1 = Box::pin(reg1.cancel_and_join("r_conc"));
        let mut fut2 = Box::pin(reg2.cancel_and_join("r_conc"));

        // Verify both futures have been polled to Pending and subscribed
        assert!(poll_once(fut1.as_mut()).is_pending());
        assert!(poll_once(fut2.as_mut()).is_pending());

        // Now release task
        notify.notify_one();

        let res1 = fut1.await;
        let res2 = fut2.await;

        assert_eq!(res1, Ok(TurnOutcome::Completed));
        assert_eq!(res2, Ok(TurnOutcome::Completed));
        assert!(!registry.is_active("r_conc"));
    }

    #[tokio::test]
    async fn test_first_waiter_dropped_second_waiter_still_joins_cleanly() {
        let registry = Arc::new(ApiBotRegistry::new());
        let key = RunTurnKey {
            run_id: "r_two_waiters".into(),
            turn_id: "t_two_waiters".into(),
        };

        let (tx_done, rx_done) = oneshot::channel();

        let _token = registry
            .spawn(key.clone(), move |_cancel| async move {
                let _ = rx_done.await;
                Ok(TurnOutcome::Completed)
            })
            .unwrap();

        // Start first waiter and verify it polls to Pending, then drop it
        {
            let mut fut1 = Box::pin(registry.cancel_and_join("r_two_waiters"));
            assert!(poll_once(fut1.as_mut()).is_pending());
            // Drops fut1
        }

        // Slot must still be active and protected
        assert!(registry.is_active("r_two_waiters"));

        // Second waiter starts and polls to Pending
        let mut fut2 = Box::pin(registry.cancel_and_join("r_two_waiters"));
        assert!(poll_once(fut2.as_mut()).is_pending());

        // Release the runner task
        let _ = tx_done.send(());

        let res2 = fut2.await;
        assert_eq!(res2, Ok(TurnOutcome::Completed));
        assert!(!registry.is_active("r_two_waiters"));
    }

    #[tokio::test]
    async fn test_channel_closed_early_when_supervisor_drops_prematurely() {
        let (tx, mut rx) = watch::channel::<Option<Result<TurnOutcome, RegistryError>>>(None);
        drop(tx);

        let res = wait_outcome(&mut rx).await;
        assert_eq!(res, Err(RegistryError::ChannelClosedEarly));
    }

    #[tokio::test]
    async fn test_precise_complete_versus_cancel_task_interleaving_32_rounds() {
        let total_iterations = 32;
        let complete_witness = Arc::new(AtomicUsize::new(0));

        for i in 0..total_iterations {
            let registry = Arc::new(ApiBotRegistry::new());
            let key = RunTurnKey {
                run_id: format!("run_{}", i),
                turn_id: format!("turn_{}", i),
            };

            let witness = complete_witness.clone();
            let (tx_start_signal, rx_start_signal) = oneshot::channel();

            let _token = registry
                .spawn(key.clone(), move |cancel| async move {
                    if i % 2 == 0 {
                        // Cancel-first branch
                        cancel.cancelled().await;
                        Ok(TurnOutcome::Cancelled)
                    } else {
                        // Complete-first branch
                        let _ = rx_start_signal.await;
                        witness.fetch_add(1, Ordering::SeqCst);
                        Ok(TurnOutcome::Completed)
                    }
                })
                .unwrap();

            if i % 2 == 0 {
                let outcome = registry.cancel_and_join(&key.run_id).await.unwrap();
                assert_eq!(outcome, TurnOutcome::Cancelled);
            } else {
                let mut fut = Box::pin(registry.cancel_and_join(&key.run_id));
                assert!(poll_once(fut.as_mut()).is_pending());

                // Signal completion to runner
                let _ = tx_start_signal.send(());

                let outcome = fut.await.unwrap();
                assert_eq!(outcome, TurnOutcome::Completed);
            }

            assert!(!registry.is_active(&key.run_id));
        }

        // Exactly 16 iterations performed task completion
        assert_eq!(complete_witness.load(Ordering::SeqCst), 16);
    }
}
