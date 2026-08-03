//! Progress aggregation: a 5s sliding-window speed estimate, batched and
//! throttled to one event per 150ms (design §5, and §3 principle 2's "avoid
//! flooding IPC").

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::mpsc;

/// How often the aggregator flushes. Design §5.
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);

/// Span of the speed estimate's sliding window. Design §5.
pub const SPEED_WINDOW: Duration = Duration::from_secs(5);

/// A sliding window of recent byte deltas, used to estimate throughput.
///
/// The clock is a parameter rather than read internally, which is what makes
/// this testable without sleeping: callers pass `Instant::now()` in
/// production and synthetic instants in tests.
#[derive(Debug, Default)]
pub struct SpeedWindow {
    samples: VecDeque<(Instant, u64)>,
}

impl SpeedWindow {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&mut self, at: Instant, bytes: u64) {
        self.samples.push_back((at, bytes));
    }

    /// Bytes per second over the window, or `0.0` when there is nothing to
    /// divide by.
    ///
    /// The denominator is the span from the *oldest surviving sample* to
    /// `now`, not the full 5s: right after a transfer starts there is only
    /// half a second of history, and dividing by 5 would under-report the
    /// speed by 10x for the first few seconds.
    ///
    /// The trim below drops samples strictly older than the cutoff, so a
    /// sample landing exactly on the `SPEED_WINDOW` boundary is retained for
    /// one extra round. That is deliberate, not an off-by-one: an inclusive
    /// boundary keeps the estimate marginally smoother and can never cause a
    /// divide-by-zero.
    pub fn speed(&mut self, now: Instant) -> f64 {
        let cutoff = now.checked_sub(SPEED_WINDOW);
        if let Some(cutoff) = cutoff {
            while self.samples.front().is_some_and(|(at, _)| *at < cutoff) {
                self.samples.pop_front();
            }
        }

        let Some((oldest, _)) = self.samples.front().copied() else {
            return 0.0;
        };
        let elapsed = now.saturating_duration_since(oldest).as_secs_f64();
        if elapsed <= 0.0 {
            return 0.0;
        }
        let total: u64 = self.samples.iter().map(|(_, bytes)| bytes).sum();
        total as f64 / elapsed
    }
}

/// Seconds remaining, or `None` when the number would be meaningless: a
/// stalled transfer (speed 0) would divide by zero, and an already-complete
/// one has nothing to wait for. Rounded up so a nearly-finished transfer
/// shows "1s" rather than "0s" while still running.
///
/// The `speed.is_nan()` check is deliberate and not redundant: IEEE-754
/// makes every ordered comparison against NaN `false`, so a bare
/// `speed <= 0.0` guard would let a NaN speed slip through and produce a
/// nonsensical `Some(0)`. (Clippy's `neg_cmp_op_on_partial_ord` lint also
/// rejects the equivalent `!(speed > 0.0)` form, so the explicit `is_nan`
/// check is both the clearer and the lint-clean spelling.)
///
/// The final `as u64` cast is a saturating conversion (Rust's `f64 as u64`
/// semantics, not UB), so an absurdly small speed yields `u64::MAX` rather
/// than wrapping or panicking -- no redundant range check is needed here.
pub fn eta_secs(total: u64, transferred: u64, speed: f64) -> Option<u64> {
    if speed.is_nan() || speed <= 0.0 || transferred >= total {
        return None;
    }
    Some((((total - transferred) as f64) / speed).ceil() as u64)
}

/// One task's progress, as pushed to the frontend.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProgressPayload {
    pub task_id: String,
    pub transferred: u64,
    pub total: u64,
    /// Bytes per second over the last [`SPEED_WINDOW`].
    pub speed: f64,
    pub eta_secs: Option<u64>,
}

/// What runners send to the aggregator.
#[derive(Debug, Clone)]
pub enum ProgressMsg {
    /// `bytes` more of this task have been transferred.
    Delta {
        task_id: String,
        bytes: u64,
        total: u64,
    },
    /// Stop tracking this task: any later `Delta` for the same `task_id`
    /// starts its accounting from zero.
    ///
    /// A post-`Forget` `Delta` is *not* rejected or merged into the old
    /// history -- it silently starts a brand-new `TaskProgress`, so
    /// `transferred` becomes that delta's byte count rather than resuming the
    /// pre-`Forget` total. Both of the engine's callers rely on exactly that:
    ///
    /// - **Finishing** (`Completed` / `Canceled` / `Failed`): the task is over,
    ///   its final numbers travel on the unthrottled state event instead, and
    ///   nothing more will ever be reported for it.
    /// - **Restarting** (`resume` / `retry`): a resumed runner re-reports work
    ///   it already reported, so the aggregator's counter is zeroed at the same
    ///   instant as the engine's own, and the replayed bytes land on a fresh
    ///   entry instead of doubling the total.
    ///
    /// The one case that must *not* send it is `Paused`: keeping the entry is
    /// what lets the panel go on showing the progress the pause froze.
    Forget { task_id: String },
    /// Undo `bytes` of a task's `transferred` -- the aggregator-side half of
    /// [`crate::transfer::ProgressHandle::retract`]'s atomic rollback, so a
    /// task that keeps running after a retract (a retried chunk, Task 3's
    /// upload case) does not leave the IPC payload permanently over-counted.
    ///
    /// Deliberately *not* a throughput sample: a retract does not touch the
    /// entry's `SpeedWindow`, so the reported speed can briefly overshoot
    /// during a retry burst rather than dip -- accepted per the design brief.
    /// An unknown `task_id` is silently ignored (no entry is created for it),
    /// the same as any message racing a `Forget`.
    Retract { task_id: String, bytes: u64 },
}

/// Where flushed batches go. The Tauri implementation lives in `engine.rs`;
/// tests substitute a collecting one.
pub trait ProgressSink: Send + Sync + 'static {
    fn flush(&self, batch: Vec<ProgressPayload>);
}

#[derive(Debug)]
struct TaskProgress {
    transferred: u64,
    total: u64,
    window: SpeedWindow,
    dirty: bool,
}

/// Starts the aggregator loop and returns its sender.
///
/// Unbounded on purpose: a runner reporting progress must never block on a
/// slow consumer, and the messages are tiny and strictly bounded by the number
/// of parts in flight.
pub fn spawn_aggregator(sink: Arc<dyn ProgressSink>) -> mpsc::UnboundedSender<ProgressMsg> {
    let (tx, mut rx) = mpsc::unbounded_channel::<ProgressMsg>();

    tokio::spawn(async move {
        let mut tasks: HashMap<String, TaskProgress> = HashMap::new();
        // The first deadline is offset one full period out: there is no
        // reason to flush at t=0, and `interval`'s default of ticking
        // immediately would race the first batch of deltas -- both the
        // queued messages and the immediate tick are ready at the first
        // `select!`, and `select!` polls its branches in random order, so
        // the tick could be taken part-way through draining the deltas and
        // flush a partial batch instead of one clean batch per interval.
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + PROGRESS_INTERVAL,
            PROGRESS_INTERVAL,
        );
        // Skipping missed ticks keeps a stalled executor from firing a burst
        // of catch-up flushes once it gets scheduled again.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                msg = rx.recv() => {
                    match msg {
                        // Every sender dropped: the engine is gone, so is the
                        // reason to keep ticking.
                        None => break,
                        Some(ProgressMsg::Forget { task_id }) => {
                            tasks.remove(&task_id);
                        }
                        Some(ProgressMsg::Retract { task_id, bytes }) => {
                            // `get_mut` rather than `entry(..).or_insert`: an
                            // unknown task_id must be a silent no-op, not
                            // spawn a zeroed entry that then reports garbage
                            // on the next tick.
                            if let Some(entry) = tasks.get_mut(&task_id) {
                                entry.transferred = entry.transferred.saturating_sub(bytes);
                                entry.dirty = true;
                            }
                        }
                        Some(ProgressMsg::Delta { task_id, bytes, total }) => {
                            let entry = tasks.entry(task_id).or_insert_with(|| TaskProgress {
                                transferred: 0,
                                total,
                                window: SpeedWindow::new(),
                                dirty: false,
                            });
                            entry.transferred = entry.transferred.saturating_add(bytes);
                            entry.total = total;
                            // Read via tokio's clock (`.into_std()`) rather than
                            // `std::time::Instant::now()` directly: under
                            // `start_paused` tests the two do not agree, and a
                            // speed estimate built from the wrong clock is
                            // meaningless.
                            entry.window.record(tokio::time::Instant::now().into_std(), bytes);
                            entry.dirty = true;
                        }
                    }
                }
                _ = ticker.tick() => {
                    let now = tokio::time::Instant::now().into_std();
                    let batch: Vec<ProgressPayload> = tasks
                        .iter_mut()
                        .filter(|(_, p)| p.dirty)
                        .map(|(id, p)| {
                            p.dirty = false;
                            let speed = p.window.speed(now);
                            ProgressPayload {
                                task_id: id.clone(),
                                transferred: p.transferred,
                                total: p.total,
                                speed,
                                eta_secs: eta_secs(p.total, p.transferred, speed),
                            }
                        })
                        .collect();
                    // An empty batch is not an event: a quiet engine should
                    // produce zero IPC traffic, not 6.7 empty messages a second.
                    if !batch.is_empty() {
                        sink.flush(batch);
                    }
                }
            }
        }
    });

    tx
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ---- SpeedWindow (pure, clock injected) ----

    #[test]
    fn empty_window_reports_zero() {
        let mut w = SpeedWindow::new();
        assert_eq!(w.speed(Instant::now()), 0.0);
    }

    #[test]
    fn single_sample_at_now_reports_zero_not_infinity() {
        // Elapsed time is zero here; dividing by it must not produce inf/NaN.
        let mut w = SpeedWindow::new();
        let t0 = Instant::now();
        w.record(t0, 1024);
        assert_eq!(w.speed(t0), 0.0);
    }

    #[test]
    fn speed_is_bytes_over_the_window_span() {
        let mut w = SpeedWindow::new();
        let t0 = Instant::now();
        w.record(t0, 1_000);
        w.record(t0 + Duration::from_secs(1), 1_000);
        // 2000 bytes spread over the 2s from the oldest sample to now.
        assert!((w.speed(t0 + Duration::from_secs(2)) - 1_000.0).abs() < 1.0);
    }

    #[test]
    fn samples_older_than_the_window_are_dropped() {
        let mut w = SpeedWindow::new();
        let t0 = Instant::now();
        w.record(t0, 1_000_000); // ancient burst, must not inflate the reading
        w.record(t0 + Duration::from_secs(20), 1_000);
        w.record(t0 + Duration::from_secs(21), 1_000);
        let speed = w.speed(t0 + Duration::from_secs(22));
        assert!(
            speed < 2_000.0,
            "stale burst leaked into the reading: {speed}"
        );
    }

    #[test]
    fn a_stalled_transfer_decays_to_zero() {
        let mut w = SpeedWindow::new();
        let t0 = Instant::now();
        w.record(t0, 5_000_000);
        // Nothing recorded for well past the window: the UI must stop
        // claiming megabytes per second for a transfer that is not moving.
        assert_eq!(w.speed(t0 + SPEED_WINDOW + Duration::from_secs(1)), 0.0);
    }

    // ---- eta ----

    #[test]
    fn eta_is_remaining_over_speed_rounded_up() {
        assert_eq!(eta_secs(1000, 0, 100.0), Some(10));
        assert_eq!(eta_secs(1000, 500, 100.0), Some(5));
        assert_eq!(eta_secs(1000, 999, 100.0), Some(1)); // ceil, never 0
    }

    #[test]
    fn eta_is_absent_when_it_would_be_meaningless() {
        assert_eq!(eta_secs(1000, 0, 0.0), None); // stalled
        assert_eq!(eta_secs(1000, 1000, 100.0), None); // finished
        assert_eq!(eta_secs(1000, 1200, 100.0), None); // over-reported
        assert_eq!(eta_secs(0, 0, 100.0), None); // empty file
    }

    #[test]
    fn eta_is_absent_for_nan_speed() {
        // IEEE-754: every ordered comparison against NaN is false, so a
        // naive `speed <= 0.0` guard would let this slip through and return
        // Some(0). The guard must be written to catch NaN explicitly.
        assert_eq!(eta_secs(1000, 0, f64::NAN), None);
    }

    // ---- aggregator ----

    #[derive(Default)]
    struct RecordingSink {
        batches: Mutex<Vec<Vec<ProgressPayload>>>,
    }

    impl ProgressSink for RecordingSink {
        fn flush(&self, batch: Vec<ProgressPayload>) {
            self.batches.lock().unwrap().push(batch);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn many_deltas_collapse_into_one_batch_per_interval() {
        let sink = Arc::new(RecordingSink::default());
        let tx = spawn_aggregator(sink.clone());

        // 50 deltas inside a single 150ms window must produce ONE event, not
        // 50 -- that is the whole point of the throttle (design §3).
        for _ in 0..50 {
            tx.send(ProgressMsg::Delta {
                task_id: "t1".to_string(),
                bytes: 10,
                total: 1000,
            })
            .unwrap();
        }
        tokio::task::yield_now().await;
        tokio::time::advance(PROGRESS_INTERVAL * 2).await;
        tokio::task::yield_now().await;

        let batches = sink.batches.lock().unwrap();
        assert_eq!(batches.len(), 1, "expected exactly one flush");
        assert_eq!(batches[0].len(), 1, "expected one task in the batch");
        assert_eq!(batches[0][0].task_id, "t1");
        assert_eq!(batches[0][0].transferred, 500);
        assert_eq!(batches[0][0].total, 1000);
    }

    #[tokio::test(start_paused = true)]
    async fn an_idle_interval_emits_nothing() {
        let sink = Arc::new(RecordingSink::default());
        let _tx = spawn_aggregator(sink.clone());

        tokio::time::advance(PROGRESS_INTERVAL * 10).await;
        tokio::task::yield_now().await;

        assert!(
            sink.batches.lock().unwrap().is_empty(),
            "a quiet engine must not emit empty progress events"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_tasks_share_one_batch() {
        let sink = Arc::new(RecordingSink::default());
        let tx = spawn_aggregator(sink.clone());

        for id in ["t1", "t2", "t3"] {
            tx.send(ProgressMsg::Delta {
                task_id: id.to_string(),
                bytes: 7,
                total: 100,
            })
            .unwrap();
        }
        tokio::task::yield_now().await;
        tokio::time::advance(PROGRESS_INTERVAL * 2).await;
        tokio::task::yield_now().await;

        let batches = sink.batches.lock().unwrap();
        assert_eq!(batches.len(), 1, "one flush covering all three tasks");
        assert_eq!(batches[0].len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn forget_stops_reporting_a_finished_task() {
        let sink = Arc::new(RecordingSink::default());
        let tx = spawn_aggregator(sink.clone());

        tx.send(ProgressMsg::Delta {
            task_id: "t1".to_string(),
            bytes: 10,
            total: 10,
        })
        .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(PROGRESS_INTERVAL * 2).await;
        tokio::task::yield_now().await;

        tx.send(ProgressMsg::Forget {
            task_id: "t1".to_string(),
        })
        .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(PROGRESS_INTERVAL * 4).await;
        tokio::task::yield_now().await;

        // The one flush from before Forget, and nothing in between: a
        // completed task must not keep occupying the progress stream.
        assert_eq!(sink.batches.lock().unwrap().len(), 1);

        // The above alone cannot tell "removed" apart from "merely
        // quiescent": by this point the task's `dirty` flag is already
        // false, so no further flush happens either way. Discriminate by
        // sending a Delta for the *same* task_id after the Forget: if the
        // entry was truly removed, this starts a fresh TaskProgress and
        // `transferred` is just this delta's 5 bytes; if Forget was a
        // no-op, the old entry (transferred = 10) survived and this delta
        // would land on top of it, reporting 15.
        tx.send(ProgressMsg::Delta {
            task_id: "t1".to_string(),
            bytes: 5,
            total: 10,
        })
        .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(PROGRESS_INTERVAL * 2).await;
        tokio::task::yield_now().await;

        let batches = sink.batches.lock().unwrap();
        assert_eq!(batches.len(), 2, "the post-Forget delta must flush again");
        assert_eq!(
            batches[1][0].transferred, 5,
            "Forget must have removed the entry so the post-Forget delta \
             starts a fresh TaskProgress, not resume the old total"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retract_reduces_transferred_without_touching_speed() {
        let sink = Arc::new(RecordingSink::default());
        let tx = spawn_aggregator(sink.clone());

        tx.send(ProgressMsg::Delta {
            task_id: "t1".to_string(),
            bytes: 8_000_000,
            total: 10_000_000,
        })
        .unwrap();
        tx.send(ProgressMsg::Retract {
            task_id: "t1".to_string(),
            bytes: 3_000_000,
        })
        .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(PROGRESS_INTERVAL * 2).await;
        tokio::task::yield_now().await;

        let batches = sink.batches.lock().unwrap();
        assert_eq!(batches.len(), 1, "expected exactly one flush");
        let payload = &batches[0][0];
        assert_eq!(payload.task_id, "t1");
        assert_eq!(
            payload.transferred, 5_000_000,
            "retract must reduce transferred: 8MB delta minus a 3MB retract"
        );
        // The window only records Delta bytes, so the speed a retract-adjusted
        // entry reports must be identical to what the 8MB Delta alone would
        // have produced -- a retract is not a throughput sample and must not
        // shrink or otherwise perturb the window (brief retry-period
        // overshoot is accepted).
        let expected_speed = 8_000_000.0 / (PROGRESS_INTERVAL * 2).as_secs_f64();
        assert!(
            (payload.speed - expected_speed).abs() < 1.0,
            "speed {} strayed from the Delta-only expectation {expected_speed}",
            payload.speed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn retract_for_an_unknown_task_is_ignored() {
        let sink = Arc::new(RecordingSink::default());
        let tx = spawn_aggregator(sink.clone());

        tx.send(ProgressMsg::Retract {
            task_id: "ghost".to_string(),
            bytes: 100,
        })
        .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(PROGRESS_INTERVAL * 2).await;
        tokio::task::yield_now().await;

        assert!(
            sink.batches.lock().unwrap().is_empty(),
            "a Retract for a task nobody Delta'd yet must not create an entry, let alone flush \
             one"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn deltas_accumulate_across_intervals() {
        let sink = Arc::new(RecordingSink::default());
        let tx = spawn_aggregator(sink.clone());

        for _ in 0..2 {
            tx.send(ProgressMsg::Delta {
                task_id: "t1".to_string(),
                bytes: 100,
                total: 1000,
            })
            .unwrap();
            tokio::task::yield_now().await;
            tokio::time::advance(PROGRESS_INTERVAL * 2).await;
            tokio::task::yield_now().await;
        }

        let batches = sink.batches.lock().unwrap();
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0][0].transferred, 100);
        assert_eq!(batches[1][0].transferred, 200, "transferred is cumulative");
    }
}
