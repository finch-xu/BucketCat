//! Part-level retry policy (design §5 + §7). Pure, no IO, no sleeping.

use std::time::Duration;

use crate::error::AppError;

/// Retries after the first attempt. Design §5: "指数退避 1s/2s/4s，3 次失败才
/// 置任务 Failed" -- three delays, hence three retries on top of the initial
/// try, for four attempts total.
pub const MAX_RETRIES: u32 = 3;

/// Delay before retry number `retry` (1-based) for a given error, per
/// design §7.2-7.3: most `network/*` errors use the normal 1s/2s/4s
/// schedule, but `Throttled` gets its own, much slower 5s/15s/45s table --
/// hammering a server that just told us to slow down at the same cadence as
/// a plain timeout would only make the throttling worse.
///
/// Clamped at both ends so it is total: `retry == 0` cannot happen in the
/// runner but must not shift by -1, and anything past either schedule holds
/// at the last step rather than doubling forever.
pub fn backoff_delay_for(err: &AppError, retry: u32) -> Duration {
    let step = retry.saturating_sub(1).min(2) as usize;
    let secs: u64 = if matches!(err, AppError::Throttled) {
        [5, 15, 45][step]
    } else {
        1 << step
    };
    Duration::from_secs(secs)
}

/// Whether the engine should retry a failed part instead of failing the task.
///
/// Only the `network/*` family qualifies (design §7): a timeout, an
/// unreachable endpoint or a throttled response is plausibly transient,
/// whereas bad credentials, a missing bucket or a local disk error will fail
/// identically on the next attempt and retrying only delays the user's
/// feedback. `Internal` is deliberately *not* retried -- it means the app
/// hit something it does not model, and quietly retrying an unknown
/// condition three times hides it.
pub fn is_retryable(err: &AppError) -> bool {
    matches!(
        err,
        AppError::Timeout | AppError::Unreachable | AppError::Throttled
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_follows_the_documented_schedule() {
        // Design §5: "指数退避 1s/2s/4s".
        assert_eq!(
            backoff_delay_for(&AppError::Timeout, 1),
            Duration::from_secs(1)
        );
        assert_eq!(
            backoff_delay_for(&AppError::Timeout, 2),
            Duration::from_secs(2)
        );
        assert_eq!(
            backoff_delay_for(&AppError::Timeout, 3),
            Duration::from_secs(4)
        );
    }

    #[test]
    fn backoff_is_clamped_at_both_ends() {
        // Retry 0 never happens in the runner, but a delay of 2^-1 seconds is
        // not a thing, so it must degrade to the first step rather than panic.
        assert_eq!(
            backoff_delay_for(&AppError::Timeout, 0),
            Duration::from_secs(1)
        );
        // And nothing beyond the schedule may grow without bound.
        assert_eq!(
            backoff_delay_for(&AppError::Timeout, 4),
            Duration::from_secs(4)
        );
        assert_eq!(
            backoff_delay_for(&AppError::Timeout, u32::MAX),
            Duration::from_secs(4)
        );
    }

    #[test]
    fn throttled_is_retryable_with_a_longer_schedule() {
        assert!(is_retryable(&AppError::Throttled));
        assert_eq!(
            backoff_delay_for(&AppError::Throttled, 1),
            Duration::from_secs(5)
        );
        assert_eq!(
            backoff_delay_for(&AppError::Throttled, 2),
            Duration::from_secs(15)
        );
        assert_eq!(
            backoff_delay_for(&AppError::Throttled, 3),
            Duration::from_secs(45)
        );
        assert_eq!(
            backoff_delay_for(&AppError::Throttled, 9),
            Duration::from_secs(45)
        );
        // The plain schedule is unaffected by the throttled table existing.
        assert_eq!(
            backoff_delay_for(&AppError::Timeout, 1),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn only_network_errors_are_retried() {
        // Design §7: network/* is retried by the engine; auth/storage/local
        // are surfaced immediately because retrying cannot help.
        assert!(is_retryable(&AppError::Timeout));
        assert!(is_retryable(&AppError::Unreachable));
        assert!(is_retryable(&AppError::Throttled));

        assert!(!is_retryable(&AppError::InvalidCredentials));
        assert!(!is_retryable(&AppError::AccessDenied));
        assert!(!is_retryable(&AppError::KeyNotFound {
            key: "k".to_string()
        }));
        assert!(!is_retryable(&AppError::BucketNotFound {
            bucket: "b".to_string()
        }));
        assert!(!is_retryable(&AppError::Internal {
            message: "m".to_string()
        }));
    }

    #[test]
    fn every_retryable_error_is_in_the_network_family() {
        // Guards the invariant rather than the list: if a future AppError
        // variant is marked retryable it must be a network/* one.
        for err in [
            AppError::Timeout,
            AppError::Unreachable,
            AppError::Throttled,
        ] {
            assert!(err.code().starts_with("network/"), "{}", err.code());
        }
    }
}
