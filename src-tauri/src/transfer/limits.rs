//! Hot-adjustable concurrency + tuning shared by the engine and every task it
//! spawns (design §4.4).
//!
//! Before this module, [`crate::transfer::engine::TransferEngine`] read
//! `max_tasks`/`max_parts`/tuning once at construction (a fixed
//! `EngineConfig` and a fixed-size `Semaphore`): the settings page could
//! persist a new value, but it only took effect on the next app restart.
//! [`SharedLimits`] replaces both with state the running engine reads afresh
//! for every task it spawns, so a settings change (Task 8's command layer)
//! applies to the *next* task admitted -- no restart, and no task already
//! `Running` is disturbed.
//!
//! The task-level cap is a `tokio::sync::Semaphore`, which only grows
//! (`add_permits`) or shrinks (permits simply never returned to the pool) --
//! there is no `Semaphore::set_permits`. [`SharedLimits::set_max_tasks`]
//! layers a target on top:
//!
//! - **Growing** the target adds the difference as fresh permits, which
//!   immediately wakes any task parked in [`SharedLimits::acquire`] (tokio's
//!   semaphore is FIFO among registered waiters).
//! - **Shrinking** it removes the difference from the pool without touching
//!   any task that is already running: a permit that is free right now is
//!   acquired-and-forgotten synchronously; a permit that is not free yet
//!   (every slot is busy) is chased down by a background task that awaits it
//!   and forgets it the moment some running task's permit is returned.
//!   Nothing in the shrink path blocks the caller, and no running task is
//!   asked to stop early -- the pool simply drains down to the new target as
//!   permits are naturally returned.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore};

use crate::store::settings::{clamp_parts, clamp_tasks};
use crate::transfer::part::TransferTuning;

/// The engine's live concurrency + tuning state. Always lives behind an
/// `Arc` (see [`SharedLimits::new`]) so the engine, every in-flight task, and
/// (via [`crate::transfer::engine::TransferEngine::limits`]) the settings
/// command layer all observe and adjust the exact same state.
pub struct SharedLimits {
    /// The task-admission semaphore. Never rebuilt -- only ever grown or
    /// shrunk -- so a permit already handed out for a running task stays
    /// valid no matter how the target changes underneath it.
    tasks: Arc<Semaphore>,
    /// The `max_tasks` the pool is being steered towards. A plain `std`
    /// `Mutex`: every critical section is a handful of arithmetic
    /// instructions with no `.await` inside it.
    tasks_target: Mutex<usize>,
    part_limit: AtomicUsize,
    tuning: RwLock<TransferTuning>,
}

impl SharedLimits {
    /// Builds a fresh `SharedLimits`, clamping `max_tasks`/`max_parts` to
    /// their documented ranges ([`clamp_tasks`]/[`clamp_parts`]) exactly as
    /// the old `EngineConfig` construction path did. `tuning` is trusted
    /// as-is -- callers are expected to have gone through
    /// [`crate::store::settings::Settings::tuning`], which already clamps
    /// every field.
    pub fn new(max_tasks: usize, max_parts: usize, tuning: TransferTuning) -> Arc<Self> {
        let max_tasks = clamp_tasks(max_tasks);
        let max_parts = clamp_parts(max_parts);
        Arc::new(Self {
            tasks: Arc::new(Semaphore::new(max_tasks)),
            tasks_target: Mutex::new(max_tasks),
            part_limit: AtomicUsize::new(max_parts),
            tuning: RwLock::new(tuning),
        })
    }

    /// Waits for a task slot. Cancel-safe: dropping the returned future
    /// (e.g. losing a `tokio::select!` race against a cancellation token)
    /// releases any reservation back to the pool, exactly like
    /// `Semaphore::acquire_owned` itself.
    pub async fn acquire(&self) -> Result<OwnedSemaphorePermit, AcquireError> {
        self.tasks.clone().acquire_owned().await
    }

    /// Steers the task pool towards `n` (clamped to `[1, 5]`), without
    /// deadlocking and without disturbing any task already running.
    pub fn set_max_tasks(&self, n: usize) {
        let n = clamp_tasks(n);
        let mut cur = self.tasks_target.lock().unwrap();
        if n > *cur {
            // Growing: new permits wake any `acquire` already queued.
            self.tasks.add_permits(n - *cur);
        } else if n < *cur {
            // Shrinking: take the difference out of circulation. A permit
            // that is free right now is reclaimed immediately; one that
            // isn't (every slot busy) is chased down in the background so a
            // running task is never asked to stop early and the caller never
            // blocks.
            for _ in 0..(*cur - n) {
                match self.tasks.clone().try_acquire_owned() {
                    Ok(permit) => permit.forget(),
                    Err(_) => {
                        let sem = Arc::clone(&self.tasks);
                        tokio::spawn(async move {
                            if let Ok(permit) = sem.acquire_owned().await {
                                permit.forget();
                            }
                        });
                    }
                }
            }
        }
        *cur = n;
    }

    /// Sets the per-task part/chunk concurrency limit (clamped to `[1, 8]`).
    /// Only the *next* task an engine spawns reads this -- a task already
    /// running captured its `part_limit` when it started (design §4.4).
    pub fn set_max_parts(&self, n: usize) {
        self.part_limit.store(clamp_parts(n), Ordering::Relaxed);
    }

    /// Replaces the tuning snapshot new tasks are handed. Not clamped here:
    /// callers are expected to pass an already-clamped
    /// [`crate::store::settings::Settings::tuning`] result.
    pub fn set_tuning(&self, tuning: TransferTuning) {
        *self.tuning.write().unwrap() = tuning;
    }

    /// The current per-task part/chunk limit.
    pub fn part_limit(&self) -> usize {
        self.part_limit.load(Ordering::Relaxed)
    }

    /// The current tuning snapshot.
    pub fn tuning(&self) -> TransferTuning {
        *self.tuning.read().unwrap()
    }

    /// The `max_tasks` value [`SharedLimits::set_max_tasks`] is steering the
    /// pool towards -- visible immediately, even mid-shrink while the
    /// background reclaimers are still chasing down busy permits.
    pub fn max_tasks_target(&self) -> usize {
        *self.tasks_target.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn raising_max_tasks_unblocks_a_queued_acquire() {
        let l = SharedLimits::new(1, 4, TransferTuning::balanced());
        let _held = l.acquire().await;
        let waiter = tokio::spawn({
            let l = l.clone();
            async move { l.acquire().await }
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        l.set_max_tasks(2);
        let _permit = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("add_permits must wake the queued acquire")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn lowering_max_tasks_takes_effect_without_deadlock() {
        let l = SharedLimits::new(3, 4, TransferTuning::balanced());
        l.set_max_tasks(1);
        let _p = l.acquire().await; // still gets the first (and only) slot
        assert_eq!(l.max_tasks_target(), 1); // the target is visible immediately
    }

    #[tokio::test]
    async fn shrinking_below_running_tasks_reclaims_the_freed_permit_instead_of_deadlocking() {
        // Two tasks running under a pool of 3; shrink the target to 1 while
        // both permits are held. Neither running task should be disturbed
        // (their permits stay valid), but once one of them releases, the
        // background reclaimer must catch that permit rather than letting it
        // return to the pool -- otherwise a third `acquire` could sneak in
        // even though the target is now 1.
        let l = SharedLimits::new(3, 4, TransferTuning::balanced());
        let p1 = l.acquire().await.unwrap();
        let p2 = l.acquire().await.unwrap();
        l.set_max_tasks(1);
        drop(p1); // one running task finishes; its permit must be reclaimed, not reused

        // Give the background reclaimer every chance to run.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // The pool must now be fully exhausted: p2 still holds the sole
        // remaining permit, and the freed one from p1 was reclaimed, so a
        // fresh `try_acquire` must fail.
        assert!(
            l.tasks.clone().try_acquire_owned().is_err(),
            "the reclaimed permit must not be available for a new acquire"
        );
        drop(p2);
    }

    #[test]
    fn setters_clamp_out_of_range_values() {
        let l = SharedLimits::new(3, 4, TransferTuning::balanced());
        l.set_max_tasks(0);
        assert_eq!(l.max_tasks_target(), 1);
        l.set_max_tasks(999);
        assert_eq!(l.max_tasks_target(), 5);
        l.set_max_parts(0);
        assert_eq!(l.part_limit(), 1);
        l.set_max_parts(999);
        assert_eq!(l.part_limit(), 8);
    }

    #[test]
    fn new_clamps_out_of_range_construction_args() {
        let l = SharedLimits::new(0, 0, TransferTuning::balanced());
        assert_eq!(l.max_tasks_target(), 1);
        assert_eq!(l.part_limit(), 1);
    }

    #[test]
    fn tuning_round_trips_through_set_and_get() {
        let l = SharedLimits::new(3, 4, TransferTuning::balanced());
        assert_eq!(l.tuning(), TransferTuning::balanced());
        l.set_tuning(TransferTuning::aggressive());
        assert_eq!(l.tuning(), TransferTuning::aggressive());
    }
}
