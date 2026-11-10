//! Retry policy with exponential backoff for unary gRPC calls.
//!
//! Only transport-level failures are retried (UNAVAILABLE, DEADLINE_
//! EXCEEDED, ABORTED); business rejections (auth, validation, not-found)
//! surface immediately.

use std::time::Duration;

use tonic::Status;

/// Retry behaviour for unary calls.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    max_attempts: u32,
    initial_backoff: Duration,
    max_backoff: Duration,
    retry_on: [tonic::Code; 3],
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_secs(2),
            retry_on: [
                tonic::Code::Unavailable,
                tonic::Code::DeadlineExceeded,
                tonic::Code::Aborted,
            ],
        }
    }
}

impl RetryPolicy {
    /// Builds a custom policy.
    pub fn new(max_attempts: u32, initial_backoff: Duration, max_backoff: Duration) -> Self {
        Self {
            max_attempts: max_attempts.max(1),
            initial_backoff,
            max_backoff,
            retry_on: [
                tonic::Code::Unavailable,
                tonic::Code::DeadlineExceeded,
                tonic::Code::Aborted,
            ],
        }
    }

    /// Runs `op` with retries; each attempt gets a fresh closure call.
    pub async fn run<T, F, Fut>(&self, mut op: F) -> Result<T, Status>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, Status>>,
    {
        let mut backoff = self.initial_backoff;
        let mut last: Option<Status> = None;
        for attempt in 1..=self.max_attempts {
            match op().await {
                Ok(value) => return Ok(value),
                Err(status) => {
                    if !self.retry_on.contains(&status.code()) || attempt == self.max_attempts {
                        return Err(status);
                    }
                    tracing::warn!(
                        attempt,
                        backoff = ?backoff,
                        code = ?status.code(),
                        "grpc call failed; retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(self.max_backoff);
                    last = Some(status);
                }
            }
        }
        Err(last.unwrap_or_else(|| Status::internal("retry loop exited without status")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn retries_unavailable_then_succeeds() {
        let policy = RetryPolicy::new(3, Duration::from_millis(1), Duration::from_millis(2));
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = calls.clone();
        let result = policy
            .run(move || {
                let calls = calls2.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    if n < 2 {
                        Err(Status::unavailable("try again"))
                    } else {
                        Ok(n)
                    }
                }
            })
            .await
            .unwrap();
        assert_eq!(result, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_not_found() {
        let policy = RetryPolicy::default();
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = calls.clone();
        let err = policy
            .run(move || {
                let calls = calls2.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err::<(), _>(Status::not_found("nope"))
                }
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exhausts_attempts() {
        let policy = RetryPolicy::new(3, Duration::from_millis(1), Duration::from_millis(1));
        let calls = Arc::new(AtomicU32::new(0));
        let calls2 = calls.clone();
        let err = policy
            .run(move || {
                let calls = calls2.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err::<(), _>(Status::unavailable("down"))
                }
            })
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Unavailable);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
}
