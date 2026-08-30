use std::time::{Duration, SystemTime};

use retry_policies::{RetryDecision, RetryPolicy};

pub(crate) fn default_total_duration_of_retries() -> Duration {
    Duration::from_mins(30)
}

/// Executes `operation` repeatedly until it succeeds, encounters a non-transient error,
/// or the retry budget (governed by `policy`) is exhausted.
///
/// Only errors classified as transient by `is_transient` are eligible for retry.
///
/// `is_transient` is classified on the operation's own error type `E` (e.g. `sqlx::Error`),
/// not on a `miette::Report` — wrapping into a `Report` before classifying would erase the
/// concrete type and make every `downcast_ref` classifier always return `false`, since
/// `Report::downcast_ref` matches the outer wrapper, not its `source()` chain.
pub(crate) async fn retry_on_transient<P, E, F, Fut>(
    policy: &P,
    is_transient: fn(&E) -> bool,
    mut operation: F,
) -> Result<(), E>
where
    P: RetryPolicy,
    E: std::fmt::Debug,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), E>>,
{
    let request_start_time = SystemTime::now();
    let mut n_past_retries: u32 = 0;
    loop {
        match operation().await {
            Ok(()) => return Ok(()),
            Err(err) => {
                if !is_transient(&err) {
                    return Err(err);
                }
                match policy.should_retry(request_start_time, n_past_retries) {
                    RetryDecision::DoNotRetry => return Err(err),
                    RetryDecision::Retry { execute_after } => {
                        let delay =
                            execute_after.duration_since(SystemTime::now()).unwrap_or_default();
                        tracing::warn!(
                            error = ?err,
                            sleep = ?delay,
                            n_past_retries,
                            "Transient error, retrying"
                        );
                        n_past_retries += 1;
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use retry_policies::policies::ExponentialBackoff;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn always_transient(_: &String) -> bool {
        true
    }

    fn never_transient(_: &String) -> bool {
        false
    }

    fn fast_policy() -> impl RetryPolicy {
        ExponentialBackoff::builder()
            .retry_bounds(Duration::from_millis(1), Duration::from_millis(50))
            .build_with_total_retry_duration(Duration::from_millis(500))
    }

    #[tokio::test]
    async fn succeeds_on_first_try_without_retrying() {
        let calls = AtomicU32::new(0);
        let result: Result<(), String> =
            retry_on_transient(&fast_policy(), always_transient, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
            .await;
        assert!(result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn non_transient_error_returns_immediately() {
        let calls = AtomicU32::new(0);
        let result = retry_on_transient(&fast_policy(), never_transient, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err("boom".to_string())
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn retries_transient_error_then_succeeds() {
        let calls = AtomicU32::new(0);
        let result = retry_on_transient(&fast_policy(), always_transient, || async {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            if n < 2 { Err("boom".to_string()) } else { Ok(()) }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_once_retry_budget_exhausted() {
        let calls = AtomicU32::new(0);
        let result = retry_on_transient(&fast_policy(), always_transient, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err("boom".to_string())
        })
        .await;
        assert!(result.is_err());
        assert!(calls.load(Ordering::SeqCst) > 1, "should have retried at least once");
    }
}
