//! Transient-failure retries for provider HTTP calls.
//!
//! Unattended runs (CI, cron) must survive rate limits and provider blips.
//! Retries happen at the request level, strictly before any response body
//! is consumed, so streaming is unaffected: a request either fails before
//! streaming starts (retryable) or during the stream (not retried).

use crate::LlmError;
use std::future::Future;
use std::time::Duration;

const MAX_ATTEMPTS: u32 = 3;
const BASE_DELAY_MS: u64 = 1000;

/// Run `op` with up to two retries on transient failures, honoring
/// Retry-After when the provider sent one.
pub async fn with_retries<F, Fut>(op: F) -> Result<reqwest::Response, LlmError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<reqwest::Response, LlmError>>,
{
    let mut attempt: u32 = 0;
    loop {
        match op().await {
            Err(error) if attempt + 1 < MAX_ATTEMPTS && is_transient(&error) => {
                let delay = delay_for(attempt, retry_after_secs(&error));
                // WARN so default filters surface it: a Retry-After backoff
                // can stall a turn for a minute, which reads as a hang if
                // the operator can't see the retry.
                tracing::warn!(
                    attempt = attempt + 1,
                    delay_ms = delay.as_millis() as u64,
                    error = %error,
                    "transient provider error; retrying"
                );
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            other => return other,
        }
    }
}

fn is_transient(error: &LlmError) -> bool {
    match error {
        LlmError::Api { status, .. } => {
            matches!(status, 429 | 500 | 502 | 503 | 504 | 529)
        }
        LlmError::Http(e) => e.is_connect() || e.is_timeout(),
        _ => false,
    }
}

fn retry_after_secs(error: &LlmError) -> Option<u64> {
    match error {
        LlmError::Api { retry_after, .. } => *retry_after,
        _ => None,
    }
}

/// Exponential backoff (1s, 2s), overridden upward by Retry-After.
fn delay_for(attempt: u32, retry_after: Option<u64>) -> Duration {
    let backoff = Duration::from_millis(BASE_DELAY_MS * (1 << attempt));
    match retry_after {
        Some(secs) => backoff.max(Duration::from_secs(secs.min(30))),
        None => backoff,
    }
}

/// Build the `Api` error for a non-success response, capturing Retry-After.
pub async fn api_error(response: reqwest::Response) -> LlmError {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let body = response.text().await.unwrap_or_default();
    LlmError::Api {
        status,
        body,
        retry_after,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_transience() {
        let transient = LlmError::Api {
            status: 429,
            body: String::new(),
            retry_after: None,
        };
        let permanent = LlmError::Api {
            status: 400,
            body: String::new(),
            retry_after: None,
        };
        assert!(is_transient(&transient));
        assert!(is_transient(&LlmError::Api {
            status: 529,
            body: String::new(),
            retry_after: None
        }));
        assert!(!is_transient(&permanent));
        assert!(!is_transient(&LlmError::Parse("x".into())));
    }

    #[test]
    fn backoff_grows_and_respects_retry_after() {
        assert_eq!(delay_for(0, None), Duration::from_secs(1));
        assert_eq!(delay_for(1, None), Duration::from_secs(2));
        assert_eq!(delay_for(0, Some(5)), Duration::from_secs(5));
        assert_eq!(
            delay_for(1, Some(1)),
            Duration::from_secs(2),
            "backoff wins when larger"
        );
        assert_eq!(
            delay_for(0, Some(600)),
            Duration::from_secs(30),
            "retry-after capped"
        );
    }
}
