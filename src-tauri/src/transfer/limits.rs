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
//! - **Growing** the target adds the difference as fresh permits (after
//!   first cancelling out any outstanding shrink debt -- see below), which
//!   immediately wakes any task parked in [`SharedLimits::acquire`] (tokio's
//!   semaphore is FIFO among registered waiters).
//! - **Shrinking** it removes the difference from the pool without touching
//!   any task that is already running: a permit that is free right now is
//!   acquired-and-forgotten synchronously; a permit that is *not* free yet
//!   (every slot busy) cannot be reclaimed synchronously, since reclaiming it
//!   means waiting for a running task to finish. That remainder is recorded
//!   as **debt** -- a plain counter, not a background task -- and
//!   [`SharedLimits::acquire`] settles it the next time a permit becomes
//!   available: it repays one unit of debt (forgetting the permit) and loops
//!   around to acquire another, rather than handing that permit to the task
//!   that called `acquire`. This is why shrinking a busy pool never
//!   deadlocks and never disturbs a running task, but a *new* admission can
//!   be delayed past the moment its permit re-forms in the pool -- it is
//!   paying down debt on the way in.
//!
//! An earlier version of the shrink path used `tokio::spawn` to chase down a
//! permit that was not free yet (a background task that awaited it and
//! forgot it the moment some running task's permit was returned). That is
//! exactly the hazard `lib.rs`'s `setup` comments on for `spawn_aggregator`:
//! `tokio::spawn` panics without a live Tokio runtime driving the current
//! thread. Unlike `spawn_aggregator` (called once, from `setup`, where the
//! caller controls whether a runtime is entered), `SharedLimits::set_max_tasks`
//! is reached from *synchronous* `#[tauri::command]`s
//! (`commands::settings::set_max_tasks`/`set_transfer_preset`), which Tauri v2
//! runs inline on the IPC thread with **no** Tokio runtime at all. Calling
//! `tokio::spawn` there panics ("there is no reactor running"), and because
//! the panic unwinds while this module's `tasks_target` mutex is held, it
//! poisons that mutex -- every later call to any of these setters then
//! panics too, on a `.lock().unwrap()` against a poisoned lock, for the rest
//! of the process's life. The debt-counter design never spawns anything, so
//! it has no such runtime dependency: every method here is synchronous
//! arithmetic under a `std::sync::Mutex`, safe to call from any thread,
//! runtime or none.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tokio::sync::{AcquireError, OwnedSemaphorePermit, Semaphore};

use crate::store::settings::{clamp_parts, clamp_tasks};
use crate::transfer::part::TransferTuning;

/// `tasks_target`'s payload: the `max_tasks` the pool is being steered
/// towards, plus how many permits are still owed back to the pool from a
/// shrink that could not be completed synchronously (every slot was busy at
/// the time). Both fields live under the same lock because every update
/// that touches one often needs to touch the other atomically (e.g. growing
/// while debt is outstanding must settle debt before adding fresh permits).
struct TargetState {
    target: usize,
    debt: usize,
}

/// The engine's live concurrency + tuning state. Always lives behind an
/// `Arc` (see [`SharedLimits::new`]) so the engine, every in-flight task, and
/// (via [`crate::transfer::engine::TransferEngine::limits`]) the settings
/// command layer all observe and adjust the exact same state.
pub struct SharedLimits {
    /// The task-admission semaphore. Never rebuilt -- only ever grown or
    /// shrunk -- so a permit already handed out for a running task stays
    /// valid no matter how the target changes underneath it.
    tasks: Arc<Semaphore>,
    /// The `max_tasks` target plus outstanding shrink debt. A plain `std`
    /// `Mutex`: every critical section is a handful of arithmetic
    /// instructions with no `.await` inside it, so it is safe to lock from a
    /// thread with no Tokio runtime at all (see the module doc comment).
    tasks_target: Mutex<TargetState>,
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
            tasks_target: Mutex::new(TargetState {
                target: max_tasks,
                debt: 0,
            }),
            part_limit: AtomicUsize::new(max_parts),
            tuning: RwLock::new(tuning),
        })
    }

    /// Waits for a task slot. Cancel-safe: dropping the returned future
    /// (e.g. losing a `tokio::select!` race against a cancellation token)
    /// releases any reservation back to the pool, exactly like
    /// `Semaphore::acquire_owned` itself.
    ///
    /// Loops rather than returning the first permit it gets: if a shrink
    /// left outstanding debt (see the module doc comment), the permit this
    /// call just received might be one still owed back to the pool. In that
    /// case it repays one unit of debt -- forgetting the permit rather than
    /// handing it to the caller -- and goes around again. This is what makes
    /// a shrink "stick": debt is settled before any *new* task is admitted,
    /// even though the permit that pays it off might arrive well after
    /// `set_max_tasks` returned.
    pub async fn acquire(&self) -> Result<OwnedSemaphorePermit, AcquireError> {
        loop {
            let permit = self.tasks.clone().acquire_owned().await?;
            // Lock only after the acquire completes -- never hold this
            // `std::sync::Mutex` across an `.await`.
            let mut state = self.tasks_target.lock().unwrap();
            if state.debt > 0 {
                state.debt -= 1;
                drop(state);
                permit.forget();
                continue;
            }
            return Ok(permit);
        }
    }

    /// Steers the task pool towards `n` (clamped to `[1, 5]`), without
    /// deadlocking, without disturbing any task already running, and without
    /// ever spawning (safe to call from a thread with no Tokio runtime --
    /// see the module doc comment).
    pub fn set_max_tasks(&self, n: usize) {
        let n = clamp_tasks(n);
        let mut state = self.tasks_target.lock().unwrap();
        let cur = state.target;
        if n > cur {
            // Growing: first cancel out any outstanding shrink debt (a
            // permit that would have paid down debt no longer needs to --
            // the target it was shrinking towards just moved back up), then
            // add whatever's left as fresh permits. Fresh permits wake any
            // `acquire` already queued.
            let delta = n - cur;
            let settle = delta.min(state.debt);
            state.debt -= settle;
            let grow = delta - settle;
            state.target = n;
            drop(state);
            if grow > 0 {
                self.tasks.add_permits(grow);
            }
        } else if n < cur {
            // Shrinking: take the difference out of circulation. A permit
            // that is free right now is reclaimed immediately (forgotten,
            // never handed back to the pool); whatever remains -- every
            // slot was busy -- becomes debt for `acquire` to settle as
            // permits are naturally returned. Nothing here blocks the
            // caller and no running task is asked to stop early.
            let mut remaining = cur - n;
            while remaining > 0 {
                match self.tasks.clone().try_acquire_owned() {
                    Ok(permit) => {
                        permit.forget();
                        remaining -= 1;
                    }
                    Err(_) => break, // every remaining slot is busy
                }
            }
            state.debt += remaining;
            state.target = n;
        }
        // n == cur: nothing to do.
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
    /// pool towards -- visible immediately, even mid-shrink while debt is
    /// still outstanding.
    pub fn max_tasks_target(&self) -> usize {
        self.tasks_target.lock().unwrap().target
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
    async fn shrinking_below_running_tasks_settles_debt_before_admitting_a_new_task() {
        // Two tasks running under a pool of 3; shrink the target to 1 while
        // both permits are held. Neither running task should be disturbed
        // (their permits stay valid), but once one of them releases, that
        // freed permit must settle the outstanding debt -- not be handed
        // straight to a new task queued behind it -- otherwise a third task
        // could sneak in even though the target is now 1.
        let l = SharedLimits::new(3, 4, TransferTuning::balanced());
        let p1 = l.acquire().await.unwrap();
        let p2 = l.acquire().await.unwrap();
        l.set_max_tasks(1);
        drop(p1); // one running task finishes; its permit is freed

        let waiter = tokio::spawn({
            let l = l.clone();
            async move { l.acquire().await }
        });
        // Give the spawned acquire every chance to run: it must observe the
        // freed permit, consume it as debt repayment, and loop back around
        // to wait -- not be granted a slot -- while `p2` is still held.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            !waiter.is_finished(),
            "the freed permit must settle debt, not admit a new task while p2 still runs"
        );

        drop(p2);
        let _p3 = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("acquire must succeed once debt is fully settled and p2's permit frees up")
            .unwrap()
            .unwrap();
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

    /// Regression for the Critical finding: `set_max_tasks` is reached from
    /// *synchronous* `#[tauri::command]`s, which Tauri v2 runs inline on the
    /// IPC thread with no Tokio runtime driving it at all -- not even
    /// `tauri::async_runtime::block_on`'s runtime, since the command itself
    /// never enters one. The old shrink path called `tokio::spawn` in that
    /// situation, which panics ("there is no reactor running") and, because
    /// the panic unwound while `tasks_target` was locked, poisoned the mutex
    /// for every later call. This test calls `set_max_tasks` from a plain
    /// `#[test]` thread -- deliberately outside `rt.block_on`, mirroring the
    /// sync command's thread exactly -- while every permit is held inside a
    /// manually-built runtime, and proves both that it does not panic and
    /// that the resulting debt is honored: permits freed one at a time repay
    /// debt before a queued `acquire` is ever granted.
    #[test]
    fn set_max_tasks_shrink_off_runtime_does_not_panic_and_settles_debt() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let l = SharedLimits::new(3, 4, TransferTuning::balanced());

        // Acquire all 3 permits inside the runtime -- three tasks "running".
        let (p1, p2, p3) = rt.block_on(async {
            let p1 = l.acquire().await.unwrap();
            let p2 = l.acquire().await.unwrap();
            let p3 = l.acquire().await.unwrap();
            (p1, p2, p3)
        });

        // Shrink from this plain test thread: no `#[tokio::test]`, no
        // `rt.block_on`, no runtime context entered here at all -- exactly
        // the sync `#[tauri::command]` scenario. The old `tokio::spawn`
        // shrink path panics here; the debt-counter design must not.
        l.set_max_tasks(1);
        assert_eq!(l.max_tasks_target(), 1);

        // 3 -> 1 while all 3 permits are held means 2 units of debt: no
        // permit was free to reclaim synchronously.
        rt.block_on(async {
            let waiter = tokio::spawn({
                let l = l.clone();
                async move { l.acquire().await }
            });

            // Freeing one permit must pay down exactly one unit of debt --
            // not admit the queued acquire.
            drop(p1);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(
                !waiter.is_finished(),
                "one freed permit must only settle one unit of debt"
            );

            // Freeing the second permit settles the last unit of debt, but
            // the pool is still fully subscribed (p3 still held), so the
            // queued acquire must still not be granted.
            drop(p2);
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert!(
                !waiter.is_finished(),
                "debt must be fully settled before any new task is admitted"
            );

            // Freeing the last permit finally brings the pool down to the
            // target with nothing held: the queued acquire must now succeed.
            drop(p3);
            let _p = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
                .await
                .expect("acquire must succeed once the pool is actually back at target")
                .unwrap()
                .unwrap();
        });
    }
}
