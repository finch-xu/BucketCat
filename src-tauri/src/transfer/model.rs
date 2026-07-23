//! Transfer task model and its state machine (design §5).

use serde::Serialize;

/// Which way the bytes flow. `Download` exists in the model from day one --
/// the panel, the DTO and the event payloads are direction-agnostic, so M4b
/// only has to add a runner, not reshape the wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Upload,
    Download,
}

/// Where a task is in its lifecycle.
///
/// `Failed` carries no payload; the reason lives in
/// [`TransferTaskDto::error_code`]. Keeping the reason out of the status
/// keeps the enum `Copy` and makes the transition table a plain value-to-value
/// mapping that can be exhaustively tested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferStatus {
    Queued,
    Running,
    Paused,
    Completed,
    Failed,
    Canceled,
}

impl TransferStatus {
    /// True for states no command can leave. See [`next_status`]'s docs for
    /// why `Completed`/`Canceled` are absorbing.
    pub fn is_terminal(self) -> bool {
        matches!(self, TransferStatus::Completed | TransferStatus::Canceled)
    }

    /// True while the engine still owns work for this task -- i.e. while a
    /// driver task is live for it. Used by
    /// [`crate::transfer::TransferEngine::cancel`] to tell apart the two
    /// cases: an *active* task's driver will observe the cancellation and
    /// apply the transition itself, whereas a `Paused` / `Failed` task has no
    /// driver, so `cancel` must apply it. (`clear_finished` uses
    /// [`TransferStatus::is_terminal`], not this.)
    pub fn is_active(self) -> bool {
        matches!(self, TransferStatus::Queued | TransferStatus::Running)
    }
}

/// The events that drive [`next_status`]. `Start` is issued by the scheduler
/// when a task wins a slot; the rest come from the user (`Pause`, `Resume`,
/// `Cancel`, `Retry`) or the runner (`Complete`, `Fail`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferCommand {
    Start,
    Pause,
    Resume,
    Retry,
    Cancel,
    Complete,
    Fail,
}

/// The whole state machine, as one total function.
///
/// ```text
/// Queued ──Start──▶ Running ──Complete──▶ Completed
///   │                  ├──Pause──▶ Paused ──Resume──▶ Queued
///   │                  └──Fail───▶ Failed ──Retry───▶ Queued
///   └──────────────── Cancel ────▶ Canceled
/// ```
///
/// `None` means "not a legal transition" and the caller must leave the status
/// alone. `Completed` and `Canceled` are absorbing: design §5 says a task can
/// be canceled "from any state", but cancelling something that already
/// finished would show the user a completed transfer regressing to canceled,
/// so this narrows that to "any state that isn't already terminal".
pub fn next_status(current: TransferStatus, cmd: TransferCommand) -> Option<TransferStatus> {
    use TransferCommand as C;
    use TransferStatus as S;

    match (current, cmd) {
        // Terminal first: this arm must precede the catch-all `Cancel` arm
        // below, or a completed task would still accept cancellation.
        (S::Completed | S::Canceled, _) => None,
        (S::Queued, C::Start) => Some(S::Running),
        (S::Running, C::Complete) => Some(S::Completed),
        (S::Running, C::Pause) => Some(S::Paused),
        (S::Running, C::Fail) => Some(S::Failed),
        (S::Paused, C::Resume) => Some(S::Queued),
        (S::Failed, C::Retry) => Some(S::Queued),
        (_, C::Cancel) => Some(S::Canceled),
        _ => None,
    }
}

/// One transfer task, as the frontend sees it.
///
/// `seq` is a monotonically increasing creation counter rather than a
/// timestamp: the panel only needs a stable newest-first ordering, and a
/// counter gives that without dragging a clock (and its testability problems)
/// into the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TransferTaskDto {
    pub id: String,
    pub seq: u64,
    pub direction: Direction,
    pub connection_id: String,
    pub bucket: String,
    /// Remote object key (upload target / download source).
    pub key: String,
    /// Absolute local filesystem path.
    pub local_path: String,
    /// Display name -- the file's basename, computed server-side so the
    /// frontend never has to guess the platform's path separator.
    pub file_name: String,
    pub total: u64,
    pub transferred: u64,
    pub status: TransferStatus,
    /// `AppError::code()`-style i18n key when `status == Failed`, else `None`.
    /// The frontend renders it through the same `errors.*` dictionary as
    /// top-level errors.
    pub error_code: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::TransferCommand::*;
    use super::TransferStatus::*;
    use super::*;

    #[test]
    fn happy_path_runs_to_completion() {
        assert_eq!(next_status(Queued, Start), Some(Running));
        assert_eq!(next_status(Running, Complete), Some(Completed));
    }

    #[test]
    fn pause_resume_returns_to_the_queue() {
        assert_eq!(next_status(Running, Pause), Some(Paused));
        // Resume re-queues rather than jumping straight to Running: the task
        // must wait for a scheduler slot like everyone else (design §5).
        assert_eq!(next_status(Paused, Resume), Some(Queued));
    }

    #[test]
    fn failure_can_be_retried() {
        assert_eq!(next_status(Running, Fail), Some(Failed));
        assert_eq!(next_status(Failed, Retry), Some(Queued));
    }

    #[test]
    fn cancel_is_accepted_from_every_non_terminal_state() {
        for from in [Queued, Running, Paused, Failed] {
            assert_eq!(next_status(from, Cancel), Some(Canceled), "from {from:?}");
        }
    }

    #[test]
    fn terminal_states_reject_everything() {
        // Deliberate narrowing of design §5's "cancel from any state": a
        // finished task going back to Canceled would be a visible regression
        // in the panel. See the plan's decision D6.
        for from in [Completed, Canceled] {
            for cmd in [Start, Pause, Resume, Retry, Cancel, Complete, Fail] {
                assert_eq!(next_status(from, cmd), None, "{from:?} + {cmd:?}");
            }
        }
    }

    #[test]
    fn nonsense_transitions_are_rejected() {
        assert_eq!(next_status(Queued, Pause), None);
        assert_eq!(next_status(Queued, Complete), None);
        assert_eq!(next_status(Paused, Pause), None);
        assert_eq!(next_status(Running, Start), None);
        assert_eq!(next_status(Running, Retry), None);
        assert_eq!(next_status(Failed, Fail), None);
    }

    #[test]
    fn dto_serializes_with_the_contract_field_names() {
        // Pins the wire shape `src/lib/api.ts` mirrors field-for-field.
        let task = TransferTaskDto {
            id: "t-1".to_string(),
            seq: 7,
            direction: Direction::Upload,
            connection_id: "c-1".to_string(),
            bucket: "b".to_string(),
            key: "docs/a.bin".to_string(),
            local_path: "/tmp/a.bin".to_string(),
            file_name: "a.bin".to_string(),
            total: 1024,
            transferred: 512,
            status: TransferStatus::Running,
            error_code: None,
        };
        let v = serde_json::to_value(&task).unwrap();
        assert_eq!(v["id"], "t-1");
        assert_eq!(v["seq"], 7);
        assert_eq!(v["direction"], "upload");
        assert_eq!(v["connection_id"], "c-1");
        assert_eq!(v["bucket"], "b");
        assert_eq!(v["key"], "docs/a.bin");
        assert_eq!(v["local_path"], "/tmp/a.bin");
        assert_eq!(v["file_name"], "a.bin");
        assert_eq!(v["total"], 1024);
        assert_eq!(v["transferred"], 512);
        assert_eq!(v["status"], "running");
        assert!(v["error_code"].is_null());
    }

    #[test]
    fn status_and_direction_serialize_as_snake_case_strings() {
        let cases = [
            (TransferStatus::Queued, "queued"),
            (TransferStatus::Running, "running"),
            (TransferStatus::Paused, "paused"),
            (TransferStatus::Completed, "completed"),
            (TransferStatus::Failed, "failed"),
            (TransferStatus::Canceled, "canceled"),
        ];
        for (status, wire) in cases {
            assert_eq!(serde_json::to_value(status).unwrap(), wire);
        }
        assert_eq!(
            serde_json::to_value(Direction::Download).unwrap(),
            "download"
        );
    }

    #[test]
    fn is_terminal_matches_the_transition_table() {
        for status in [
            TransferStatus::Queued,
            TransferStatus::Running,
            TransferStatus::Paused,
            TransferStatus::Completed,
            TransferStatus::Failed,
            TransferStatus::Canceled,
        ] {
            let has_any_transition = [Start, Pause, Resume, Retry, Cancel, Complete, Fail]
                .into_iter()
                .any(|cmd| next_status(status, cmd).is_some());
            assert_eq!(
                status.is_terminal(),
                !has_any_transition,
                "is_terminal disagrees with the table for {status:?}"
            );
        }
    }
}
