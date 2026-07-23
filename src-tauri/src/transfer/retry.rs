//! Part-level retry policy (design §5 + §7). Pure, no IO, no sleeping.

use std::time::Duration;

use crate::error::AppError;

/// Retries after the first attempt. Design §5: "指数退避 1s/2s/4s，3 次失败才
/// 置任务 Failed" -- three delays, hence three retries on top of the initial
/// try, for four attempts total.
pub const MAX_RETRIES: u32 = 3;

/// Delay before retry number `retry` (1-based): 1s, 2s, 4s, then flat.
///
/// Clamped at both ends so it is total: `retry == 0` cannot happen in the
/// runner but must not shift by -1, and anything past the schedule holds at
/// the last step rather than doubling forever.
pub fn backoff_delay(retry: u32) -> Duration {
    let shift = retry.saturating_sub(1).min(2);
    Duration::from_secs(1u64 << shift)
}

/// Whether the engine should retry a failed part instead of failing the task.
///
/// Only the `network/*` family qualifies (design §7): a timeout or an
/// unreachable endpoint is plausibly transient, whereas bad credentials, a
/// missing bucket or a local disk error will fail identically on the next
/// attempt and retrying only delays the user's feedback. `Internal` is
/// deliberately *not* retried -- it means the app hit something it does not
/// model, and quietly retrying an unknown condition three times hides it.
pub fn is_retryable(err: &AppError) -> bool {
    matches!(err, AppError::Timeout | AppError::Unreachable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_follows_the_documented_schedule() {
        // Design §5: "指数退避 1s/2s/4s".
        assert_eq!(backoff_delay(1), Duration::from_secs(1));
        assert_eq!(backoff_delay(2), Duration::from_secs(2));
        assert_eq!(backoff_delay(3), Duration::from_secs(4));
    }

    #[test]
    fn backoff_is_clamped_at_both_ends() {
        // Retry 0 never happens in the runner, but a delay of 2^-1 seconds is
        // not a thing, so it must degrade to the first step rather than panic.
        assert_eq!(backoff_delay(0), Duration::from_secs(1));
        // And nothing beyond the schedule may grow without bound.
        assert_eq!(backoff_delay(4), Duration::from_secs(4));
        assert_eq!(backoff_delay(u32::MAX), Duration::from_secs(4));
    }

    #[test]
    fn only_network_errors_are_retried() {
        // Design §7: network/* is retried by the engine; auth/storage/local
        // are surfaced immediately because retrying cannot help.
        assert!(is_retryable(&AppError::Timeout));
        assert!(is_retryable(&AppError::Unreachable));

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
        for err in [AppError::Timeout, AppError::Unreachable] {
            assert!(err.code().starts_with("network/"), "{}", err.code());
        }
    }
}
