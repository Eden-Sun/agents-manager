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
//!    - `RunTicket` holds a private clone of `outcome_rx` captured before supervisor completion, guaranteeing genuine `JoinHandle` completion even if joined after slot removal.

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

/// Private registry managing the lifecycle of inert Claude API bot runner tasks.
pub(crate) struct ApiBotRegistry {
    slots: Mutex<HashMap<String, ActiveSlot>>,
}

/// Single-use ticket representing an actively tracked run turn.
///
/// All fields are private to `registry.rs`. It does NOT implement `Clone` and has NO public constructor.
/// A ticket is obtained exclusively via `ApiBotRegistry::spawn_tracked`.
/// Dropping the ticket does NOT cancel or abort the runner task, and does NOT detach the supervisor.
pub(crate) struct RunTicket {
    key: RunTurnKey,
    cancel_token: CancellationToken,
    outcome_rx: watch::Receiver<Option<Result<TurnOutcome, RegistryError>>>,
}

impl RunTicket {
    /// Returns a reference to the turn identification key.
    pub(crate) fn key(&self) -> &RunTurnKey {
        &self.key
    }

    /// Signals cancellation via the turn's cancellation token and waits for genuine task completion.
    /// Consumes the ticket.
    pub(crate) async fn cancel_and_join(self) -> Result<TurnOutcome, RegistryError> {
        self.cancel_token.cancel();
        let mut rx = self.outcome_rx;
        wait_outcome(&mut rx).await
    }

    /// Waits for genuine task completion without signaling cancellation.
    /// Consumes the ticket.
    pub(crate) async fn join(self) -> Result<TurnOutcome, RegistryError> {
        let mut rx = self.outcome_rx;
        wait_outcome(&mut rx).await
    }
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
    /// along with a dedicated join supervisor, stores the slot, and returns an owned `RunTicket`.
    ///
    /// Synchronous and non-blocking.
    /// If an active turn already occupies the run, returns `Err(RegistryError::AlreadyRunning)`.
    pub(crate) fn spawn_tracked<F, Fut>(
        self: &Arc<Self>,
        key: RunTurnKey,
        task_fn: F,
    ) -> Result<RunTicket, RegistryError>
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
        let runner_handle = tokio::spawn(Box::pin(async move { task_fn(cancel_child).await }));

        // Dedicated supervisor task ensures the runner JoinHandle is never lost/detached,
        // even if callers or tickets drop their futures.
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
            key.run_id.clone(),
            ActiveSlot {
                turn_id: key.turn_id.clone(),
                cancel_token: cancel_token.clone(),
                outcome_rx: outcome_rx.clone(),
            },
        );

        Ok(RunTicket {
            key,
            cancel_token,
            outcome_rx,
        })
    }

    /// Backwards-compatible spawn: delegates to `spawn_tracked` and returns `CancellationToken`.
    pub(crate) fn spawn<F, Fut>(
        self: &Arc<Self>,
        key: RunTurnKey,
        task_fn: F,
    ) -> Result<CancellationToken, RegistryError>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<TurnOutcome, RegistryError>> + Send + 'static,
    {
        let ticket = self.spawn_tracked(key, task_fn)?;
        Ok(ticket.cancel_token)
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

    #[cfg(test)]
    pub(crate) fn slot_count(&self) -> usize {
        let guard = self.slots.lock().expect("registry mutex not poisoned");
        guard.len()
    }

    #[cfg(test)]
    pub(crate) fn subscribe_observer(
        &self,
        run_id: &str,
    ) -> Option<watch::Receiver<Option<Result<TurnOutcome, RegistryError>>>> {
        let guard = self.slots.lock().expect("registry mutex not poisoned");
        guard.get(run_id).map(|slot| slot.outcome_rx.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::sync::{oneshot, Notify};
    use tokio::time::timeout;

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

    #[tokio::test]
    async fn test_late_join_after_actual_slot_removal() {
        timeout(Duration::from_secs(10), async {
            let registry = Arc::new(ApiBotRegistry::new());
            let key = RunTurnKey {
                run_id: "r_late".into(),
                turn_id: "t_late".into(),
            };

            let (tx_done, rx_done) = oneshot::channel();
            let ticket = registry
                .spawn_tracked(key.clone(), move |_cancel| async move {
                    let _ = rx_done.await;
                    Ok(TurnOutcome::Completed)
                })
                .unwrap();

            assert_eq!(ticket.key(), &key);
            assert!(registry.is_active("r_late"));

            // Poll legacy join to Pending before releasing runner
            let mut legacy_fut = Box::pin(registry.cancel_and_join("r_late"));
            assert!(poll_once(legacy_fut.as_mut()).is_pending());

            // Release runner task
            let _ = tx_done.send(());

            // Await legacy join to completion
            let outcome_reg = legacy_fut.await.unwrap();
            assert_eq!(outcome_reg, TurnOutcome::Completed);

            // Verify slot is definitely removed
            assert!(!registry.is_active("r_late"));
            assert_eq!(registry.cancel_and_join("r_late").await, Err(RegistryError::NotFound));

            // Even though slot is completely removed and registry cancel_and_join gives NotFound,
            // the owned ticket still yields the genuine JoinHandle TurnOutcome!
            let outcome_ticket = ticket.join().await.unwrap();
            assert_eq!(outcome_ticket, TurnOutcome::Completed);
        })
        .await
        .expect("test_late_join_after_actual_slot_removal hit deadlock timeout");
    }

    #[tokio::test]
    async fn test_old_turn_ticket_cannot_affect_new_turn_same_run() {
        timeout(Duration::from_secs(10), async {
            let registry = Arc::new(ApiBotRegistry::new());
            let key1 = RunTurnKey {
                run_id: "r_turns".into(),
                turn_id: "t1".into(),
            };

            let (tx1_done, rx1_done) = oneshot::channel();
            let ticket1 = registry
                .spawn_tracked(key1.clone(), move |_cancel| async move {
                    let _ = rx1_done.await;
                    Ok(TurnOutcome::Completed)
                })
                .unwrap();

            // Get an independent observer for turn 1 before finishing
            let mut obs1 = registry
                .subscribe_observer("r_turns")
                .expect("slot exists");

            // Finish turn 1 runner
            let _ = tx1_done.send(());
            let res1 = wait_outcome(&mut obs1).await.unwrap();
            assert_eq!(res1, TurnOutcome::Completed);

            // Verify slot is removed (slot count 0)
            assert_eq!(registry.slot_count(), 0);
            assert!(!registry.is_active("r_turns"));

            // Now start turn 2 on same run, holding gate so it stays running
            let key2 = RunTurnKey {
                run_id: "r_turns".into(),
                turn_id: "t2".into(),
            };
            let (tx2_done, rx2_done) = oneshot::channel();
            let (tx2_started, rx2_started) = oneshot::channel();
            let cancel_token_clone = Arc::new(Mutex::new(None));
            let cancel_token_capture = cancel_token_clone.clone();

            let ticket2 = registry
                .spawn_tracked(key2.clone(), move |cancel| {
                    *cancel_token_capture.lock().unwrap() = Some(cancel.clone());
                    let _ = tx2_started.send(());
                    async move {
                        let _ = rx2_done.await;
                        Ok(TurnOutcome::Completed)
                    }
                })
                .unwrap();

            let _ = rx2_started.await;
            assert!(registry.is_active("r_turns"));

            // Old ticket 1 executes cancel_and_join
            // Because turn 1 already finished, it consumes ticket1 and returns turn 1 outcome
            let old_outcome = ticket1.cancel_and_join().await.unwrap();
            assert_eq!(old_outcome, TurnOutcome::Completed);

            // Verify turn 2 token was NOT cancelled and slot remains active
            let t2_cancel = cancel_token_clone.lock().unwrap().as_ref().unwrap().clone();
            assert!(!t2_cancel.is_cancelled());
            assert!(registry.is_active("r_turns"));

            // Now release turn 2 and join cleanly
            let _ = tx2_done.send(());
            let outcome2 = ticket2.join().await.unwrap();
            assert_eq!(outcome2, TurnOutcome::Completed);
            assert!(!registry.is_active("r_turns"));
            assert_eq!(registry.slot_count(), 0);
        })
        .await
        .expect("test_old_turn_ticket_cannot_affect_new_turn_same_run hit deadlock timeout");
    }

    #[tokio::test]
    async fn test_ticket_cancel_and_join_stops_runner() {
        timeout(Duration::from_secs(10), async {
            let registry = Arc::new(ApiBotRegistry::new());
            let key = RunTurnKey {
                run_id: "r_cancel_t".into(),
                turn_id: "t_cancel_t".into(),
            };

            let ticket = registry
                .spawn_tracked(key.clone(), move |cancel| async move {
                    cancel.cancelled().await;
                    Ok(TurnOutcome::Cancelled)
                })
                .unwrap();

            let outcome = ticket.cancel_and_join().await.unwrap();
            assert_eq!(outcome, TurnOutcome::Cancelled);
            assert!(!registry.is_active("r_cancel_t"));
        })
        .await
        .expect("test_ticket_cancel_and_join_stops_runner hit deadlock timeout");
    }

    #[tokio::test]
    async fn test_ticket_dropped_does_not_cancel_or_abort_runner_and_supervision_intact() {
        timeout(Duration::from_secs(10), async {
            let registry = Arc::new(ApiBotRegistry::new());
            let key = RunTurnKey {
                run_id: "r_drop_t".into(),
                turn_id: "t_drop_t".into(),
            };

            let (tx_started, rx_started) = oneshot::channel();
            let (tx_done, rx_done) = oneshot::channel();
            let (tx_checked_not_cancelled, rx_checked_not_cancelled) = oneshot::channel();

            let ticket = registry
                .spawn_tracked(key.clone(), move |cancel| async move {
                    let _ = tx_started.send(());
                    let _ = rx_checked_not_cancelled.await;
                    assert!(!cancel.is_cancelled());
                    let _ = rx_done.await;
                    Ok(TurnOutcome::Completed)
                })
                .unwrap();

            // Wait until task starts
            let _ = rx_started.await;

            // Subscribe an observer receiver to monitor supervisor outcome without calling cancel_and_join
            let mut obs_rx = registry
                .subscribe_observer("r_drop_t")
                .expect("slot exists");

            // Dropping ticket must NOT cancel runner
            drop(ticket);

            // Slot must still be active and cancel token not cancelled
            assert!(registry.is_active("r_drop_t"));
            let _ = tx_checked_not_cancelled.send(());

            // Release runner task (do NOT call cancel_and_join, which would signal cancel)
            let _ = tx_done.send(());

            // Await genuine completion via observer
            let outcome = wait_outcome(&mut obs_rx).await.unwrap();
            assert_eq!(outcome, TurnOutcome::Completed);
            assert!(!registry.is_active("r_drop_t"));
            assert_eq!(registry.slot_count(), 0);
        })
        .await
        .expect("test_ticket_dropped_does_not_cancel_or_abort_runner_and_supervision_intact hit deadlock timeout");
    }

    #[tokio::test]
    async fn test_waiter_drop_does_not_break_subsequent_ticket_join() {
        timeout(Duration::from_secs(10), async {
            let registry = Arc::new(ApiBotRegistry::new());
            let key = RunTurnKey {
                run_id: "r_wait_drop".into(),
                turn_id: "t_wait_drop".into(),
            };

            let (tx_done, rx_done) = oneshot::channel();
            let ticket = registry
                .spawn_tracked(key.clone(), move |_cancel| async move {
                    let _ = rx_done.await;
                    Ok(TurnOutcome::Completed)
                })
                .unwrap();

            // Start a cancel_and_join waiter from registry, poll it, then drop it
            {
                let mut fut = Box::pin(registry.cancel_and_join("r_wait_drop"));
                assert!(poll_once(fut.as_mut()).is_pending());
            }

            // Runner is released
            let _ = tx_done.send(());

            // Ticket can still join cleanly and witness the genuine completion
            let res = ticket.join().await.unwrap();
            assert_eq!(res, TurnOutcome::Completed);
            assert!(!registry.is_active("r_wait_drop"));
        })
        .await
        .expect("test_waiter_drop_does_not_break_subsequent_ticket_join hit deadlock timeout");
    }

    #[tokio::test]
    async fn test_ticket_panic_is_typed_as_task_panicked() {
        timeout(Duration::from_secs(10), async {
            let registry = Arc::new(ApiBotRegistry::new());
            let key = RunTurnKey {
                run_id: "r_ticket_panic".into(),
                turn_id: "t_ticket_panic".into(),
            };

            let ticket = registry
                .spawn_tracked(key.clone(), |_| async move {
                    panic!("controlled panic for ticket join");
                    #[allow(unreachable_code)]
                    Ok(TurnOutcome::Completed)
                })
                .unwrap();

            let res = ticket.join().await;
            assert_eq!(res, Err(RegistryError::TaskPanicked));
            assert!(!registry.is_active("r_ticket_panic"));
        })
        .await
        .expect("test_ticket_panic_is_typed_as_task_panicked hit deadlock timeout");
    }

    #[tokio::test]
    async fn test_repeat_duplicate_spawn_rejection_and_drop_slots_remain_zero_n64() {
        timeout(Duration::from_secs(10), async {
            let registry = Arc::new(ApiBotRegistry::new());
            let n = 64;

            for i in 0..n {
                let key = RunTurnKey {
                    run_id: format!("run_cycle_{}", i),
                    turn_id: format!("turn_init_{}", i),
                };

                let (tx_release, rx_release) = oneshot::channel();
                let ticket = registry
                    .spawn_tracked(key.clone(), move |_cancel| async move {
                        let _ = rx_release.await;
                        Ok(TurnOutcome::Completed)
                    })
                    .unwrap();

                // Duplicate spawn with another turn_id on same run MUST be rejected
                let dup_key = RunTurnKey {
                    run_id: format!("run_cycle_{}", i),
                    turn_id: format!("turn_dup_{}", i),
                };
                let dup_res = registry.spawn_tracked(dup_key, |_| async { Ok(TurnOutcome::Completed) });
                match dup_res {
                    Err(RegistryError::AlreadyRunning { ref existing_turn_id }) => {
                        assert_eq!(existing_turn_id, &key.turn_id);
                    }
                    Err(_) => panic!("expected AlreadyRunning"),
                    Ok(_) => panic!("expected AlreadyRunning, got Ok"),
                }

                // Set up test observer before dropping ticket
                let mut obs = registry
                    .subscribe_observer(&key.run_id)
                    .expect("slot exists");

                // Drop ticket (must not cancel or abort runner or supervisor)
                drop(ticket);

                // Release task and wait for supervisor result via observer
                let _ = tx_release.send(());
                let outcome = wait_outcome(&mut obs).await.unwrap();
                assert_eq!(outcome, TurnOutcome::Completed);

                // Drop observer
                drop(obs);

                // Stable barrier: slot count must be 0 after join and supervisor cleanup
                assert_eq!(registry.slot_count(), 0);
            }

            // Verify after 64 cycles slots map has not grown
            assert_eq!(registry.slot_count(), 0);
        })
        .await
        .expect("test_repeat_duplicate_spawn_rejection_and_drop_slots_remain_zero_n64 hit deadlock timeout");
    }

    #[tokio::test]
    async fn test_watch_receiver_reclaimed_after_ticket_and_slot_drop() {
        timeout(Duration::from_secs(10), async {
            let registry = Arc::new(ApiBotRegistry::new());
            let key = RunTurnKey {
                run_id: "r_watch_recycle".into(),
                turn_id: "t_watch_recycle".into(),
            };

            let (tx_release, rx_release) = oneshot::channel();
            let ticket = registry
                .spawn_tracked(key.clone(), move |_cancel| async move {
                    let _ = rx_release.await;
                    Ok(TurnOutcome::Completed)
                })
                .unwrap();

            // Subscribe observer to monitor when sender is dropped/closed
            let mut obs = registry
                .subscribe_observer(&key.run_id)
                .expect("slot exists");

            // Release task and join via ticket
            let _ = tx_release.send(());
            let outcome = ticket.join().await.unwrap();
            assert_eq!(outcome, TurnOutcome::Completed);

            // Slot is dropped from registry
            assert_eq!(registry.slot_count(), 0);

            // Observer receives terminal outcome
            let res = wait_outcome(&mut obs).await.unwrap();
            assert_eq!(res, TurnOutcome::Completed);

            // Explicitly mark terminal update as seen so next changed() observes sender closure
            obs.borrow_and_update();

            // Verify supervisor dropped sender and channel is closed
            assert!(obs.changed().await.is_err());
            drop(obs);

            // Now test dropping ticket before task completion:
            let key2 = RunTurnKey {
                run_id: "r_watch_recycle2".into(),
                turn_id: "t_watch_recycle2".into(),
            };
            let (tx_release2, rx_release2) = oneshot::channel();
            let ticket2 = registry
                .spawn_tracked(key2.clone(), move |_cancel| async move {
                    let _ = rx_release2.await;
                    Ok(TurnOutcome::Completed)
                })
                .unwrap();

            let mut obs2 = registry
                .subscribe_observer(&key2.run_id)
                .expect("slot exists");

            // Drop ticket
            drop(ticket2);

            // Release runner task
            let _ = tx_release2.send(());

            // Observer receives terminal outcome
            let res2 = wait_outcome(&mut obs2).await.unwrap();
            assert_eq!(res2, TurnOutcome::Completed);

            // Slot is removed
            assert_eq!(registry.slot_count(), 0);

            // Explicitly mark terminal update as seen so next changed() observes sender closure
            obs2.borrow_and_update();

            // Sender was dropped by supervisor, so changed() returns Err
            assert!(obs2.changed().await.is_err());
            drop(obs2);

            // Both observers and slots dropped, resources fully reclaimed
            assert_eq!(registry.slot_count(), 0);
        })
        .await
        .expect("test_watch_receiver_reclaimed_after_ticket_and_slot_drop hit deadlock timeout");
    }
}
