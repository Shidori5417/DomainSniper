//! Per-host rate limiting and retry with backoff for transient failures.

use std::future::Future;
use std::num::NonZeroU32;
use std::time::Duration;

use governor::{DefaultKeyedRateLimiter, Quota, RateLimiter};

use crate::check::CheckError;

const MAX_ATTEMPTS: u32 = 3;
const BASE_BACKOFF: Duration = Duration::from_millis(500);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);

/// Limits requests per remote host (RDAP / WHOIS server).
pub struct HostLimiter {
    inner: DefaultKeyedRateLimiter<String>,
}

impl HostLimiter {
    pub fn new(requests_per_sec: u32) -> Self {
        let rate = NonZeroU32::new(requests_per_sec).unwrap_or(NonZeroU32::MIN);
        Self {
            inner: RateLimiter::keyed(Quota::per_second(rate)),
        }
    }

    pub async fn until_ready(&self, host: &str) {
        self.inner.until_key_ready(&host.to_lowercase()).await;
    }
}

/// A failed attempt, optionally carrying the server's `Retry-After` hint.
pub struct Failure {
    pub error: CheckError,
    pub retry_after: Option<Duration>,
}

impl From<CheckError> for Failure {
    fn from(error: CheckError) -> Self {
        Self {
            error,
            retry_after: None,
        }
    }
}

/// Exponential backoff (0.5 s, 1 s, ...) with up to 50% added jitter.
fn backoff(attempt: u32) -> Duration {
    let base = BASE_BACKOFF * 2u32.pow(attempt - 1);
    base + base.mul_f64(rand::random_range(0.0..0.5))
}

/// Runs `op` up to three times, retrying only transient errors.
pub async fn retry<T, F, Fut>(mut op: F) -> Result<T, CheckError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Failure>>,
{
    let mut attempt = 1;
    loop {
        let failure = match op().await {
            Ok(value) => return Ok(value),
            Err(failure) => failure,
        };
        if !matches!(failure.error, CheckError::Transient(_)) || attempt >= MAX_ATTEMPTS {
            return Err(failure.error);
        }
        let delay = failure
            .retry_after
            .map_or_else(|| backoff(attempt), |d| d.min(MAX_RETRY_AFTER));
        tracing::debug!(attempt, ?delay, error = %failure.error, "retrying after transient failure");
        tokio::time::sleep(delay).await;
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn transient() -> Failure {
        Failure {
            error: CheckError::Transient("x".into()),
            retry_after: Some(Duration::ZERO),
        }
    }

    #[test]
    fn backoff_grows_with_bounded_jitter() {
        for attempt in 1..=3 {
            let base = BASE_BACKOFF * 2u32.pow(attempt - 1);
            let d = backoff(attempt);
            assert!(d >= base && d <= base.mul_f64(1.5), "{attempt}: {d:?}");
        }
    }

    #[tokio::test]
    async fn retries_transient_up_to_limit() {
        let calls = Cell::new(0);
        let result: Result<(), _> = retry(|| {
            calls.set(calls.get() + 1);
            async { Err(transient()) }
        })
        .await;
        assert!(matches!(result, Err(CheckError::Transient(_))));
        assert_eq!(calls.get(), 3);
    }

    #[tokio::test]
    async fn succeeds_after_transient_failure() {
        let calls = Cell::new(0);
        let result = retry(|| {
            calls.set(calls.get() + 1);
            let first = calls.get() == 1;
            async move {
                if first {
                    Err(transient())
                } else {
                    Ok(7)
                }
            }
        })
        .await;
        assert_eq!(result.unwrap(), 7);
        assert_eq!(calls.get(), 2);
    }

    #[tokio::test]
    async fn permanent_errors_are_not_retried() {
        let calls = Cell::new(0);
        let result: Result<(), _> = retry(|| {
            calls.set(calls.get() + 1);
            async { Err(CheckError::Permanent("no".into()).into()) }
        })
        .await;
        assert!(matches!(result, Err(CheckError::Permanent(_))));
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test]
    async fn limiter_allows_first_request_immediately() {
        let limiter = HostLimiter::new(1);
        limiter.until_ready("Example.com").await;
    }
}
