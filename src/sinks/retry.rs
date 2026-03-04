use std::time::{Duration, SystemTime};

use crate::errors::{Report, Result};
use retry_policies::{RetryDecision, RetryPolicy};

pub(crate) fn default_total_duration_of_retries() -> Duration {
    Duration::from_secs(30 * 60)
}

/// Executes `operation` repeatedly until it succeeds, encounters a non-transient error,
/// or the retry budget (governed by `policy`) is exhausted.
///
/// Only errors classified as transient by `is_transient` are eligible for retry.
pub(crate) async fn retry_on_transient<P, F, Fut>(
    policy: &P,
    is_transient: fn(&Report) -> bool,
    mut operation: F,
) -> Result<()>
where
    P: RetryPolicy,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
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
