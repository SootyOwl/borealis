use std::time::Duration;

use rand::Rng;
use tracing::{debug, warn};

/// Retry configuration with exponential backoff.
const BASE_DELAY: Duration = Duration::from_secs(1);
const MULTIPLIER: f64 = 2.0;
const MAX_DELAY: Duration = Duration::from_secs(30);
const JITTER_FRACTION: f64 = 0.25;

/// Whether an HTTP status code is retryable (408 request timeout, 429 rate
/// limit, or 5xx server error).
pub fn is_retryable_status(status: u16) -> bool {
    status == 408 || status == 429 || (500..600).contains(&status)
}

/// Calculate the delay for a given retry attempt (0-indexed).
/// Applies exponential backoff with jitter: base * multiplier^attempt +/- 25%.
pub fn backoff_delay(attempt: u32) -> Duration {
    let base_secs = BASE_DELAY.as_secs_f64() * MULTIPLIER.powi(attempt as i32);
    let capped_secs = base_secs.min(MAX_DELAY.as_secs_f64());

    let mut rng = rand::thread_rng();
    let jitter_range = capped_secs * JITTER_FRACTION;
    let jitter = rng.gen_range(-jitter_range..=jitter_range);
    let final_secs = (capped_secs + jitter).max(0.0);

    Duration::from_secs_f64(final_secs)
}

/// Parse the integer-seconds form of a `Retry-After` header.
///
/// The HTTP-date form is intentionally ignored — providers we talk to send
/// integer seconds, and a wrong clock would make date parsing worse than
/// falling back to our own backoff.
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

/// Delay before the next retry: the larger of the computed exponential backoff
/// and any server-provided `Retry-After`, capped at [`MAX_DELAY`].
///
/// When `Retry-After` wins, a small positive jitter is added on top so that
/// clients rate-limited at the same instant don't all wake simultaneously.
fn retry_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    let backoff = backoff_delay(attempt);
    let delay = match retry_after {
        Some(ra) if ra >= backoff => {
            let jitter = ra.as_secs_f64() * JITTER_FRACTION * rand::thread_rng().gen_range(0.0..=1.0);
            ra + Duration::from_secs_f64(jitter)
        }
        _ => backoff,
    };
    delay.min(MAX_DELAY)
}

/// Execute an HTTP request with retry logic.
///
/// Retries on 408, 429, and 5xx status codes up to `max_retries` times with
/// exponential backoff, honouring a `Retry-After` header (integer-seconds
/// form) when it asks for a longer wait than the computed backoff. Returns
/// the successful response or the last error.
pub async fn with_retry<F, Fut>(
    provider_name: &str,
    max_retries: u32,
    make_request: F,
) -> Result<reqwest::Response, RetryError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    for attempt in 0..=max_retries {
        match make_request().await {
            Ok(response) => {
                if response.status().is_success() {
                    debug!(provider = provider_name, attempt, "Request succeeded");
                    return Ok(response);
                }

                let status = response.status().as_u16();
                let retry_after = parse_retry_after(response.headers());
                // Read the error body now so it is logged before any backoff
                // sleep — operators shouldn't lose 429/5xx bodies on retried
                // attempts.
                let body = response.text().await.unwrap_or_default();

                if !is_retryable_status(status) || attempt >= max_retries {
                    return Err(RetryError::HttpStatus { status, body });
                }

                let delay = retry_delay(attempt, retry_after);
                warn!(
                    provider = provider_name,
                    status,
                    attempt = attempt + 1,
                    max_retries,
                    delay_ms = delay.as_millis() as u64,
                    retry_after_secs = retry_after.map(|d| d.as_secs()),
                    body = %body,
                    "Retryable status, backing off"
                );
                tokio::time::sleep(delay).await;
            }
            Err(e) => {
                if attempt >= max_retries {
                    return Err(RetryError::Network(e));
                }
                let delay = backoff_delay(attempt);
                warn!(
                    provider = provider_name,
                    attempt = attempt + 1,
                    max_retries,
                    error = %e,
                    delay_ms = delay.as_millis() as u64,
                    "Request error, backing off"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }

    unreachable!("with_retry always returns on the final attempt")
}

/// Errors that can occur during retry.
#[derive(Debug, thiserror::Error)]
pub enum RetryError {
    #[error("HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },

    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),
}

impl RetryError {
    /// Returns the HTTP status code if this is an `HttpStatus` error.
    pub fn status_code(&self) -> Option<u16> {
        match self {
            Self::HttpStatus { status, .. } => Some(*status),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, RETRY_AFTER};

    #[test]
    fn retryable_statuses_include_408_429_and_5xx() {
        assert!(is_retryable_status(408));
        assert!(is_retryable_status(429));
        assert!(is_retryable_status(500));
        assert!(is_retryable_status(503));
        assert!(is_retryable_status(599));
        assert!(!is_retryable_status(200));
        assert!(!is_retryable_status(400));
        assert!(!is_retryable_status(401));
        assert!(!is_retryable_status(404));
        assert!(!is_retryable_status(600));
    }

    #[test]
    fn parse_retry_after_integer_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, HeaderValue::from_static("7"));
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(7)));
    }

    #[test]
    fn parse_retry_after_ignores_http_date_form() {
        let mut headers = HeaderMap::new();
        headers.insert(
            RETRY_AFTER,
            HeaderValue::from_static("Wed, 21 Oct 2026 07:28:00 GMT"),
        );
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn parse_retry_after_missing_header() {
        assert_eq!(parse_retry_after(&HeaderMap::new()), None);
    }

    #[test]
    fn retry_delay_uses_retry_after_when_larger_than_backoff() {
        // Backoff for attempt 0 is ~1s (±25%), so a 10s Retry-After must win.
        // The server-requested wait is honoured as a floor, with up to
        // +JITTER_FRACTION added on top to desynchronize retry herds.
        let delay = retry_delay(0, Some(Duration::from_secs(10)));
        assert!(delay >= Duration::from_secs(10));
        assert!(delay <= Duration::from_secs_f64(10.0 * (1.0 + JITTER_FRACTION)));
    }

    #[test]
    fn retry_delay_uses_backoff_when_retry_after_smaller() {
        // Backoff for attempt 4 is ~16s (±25%), well above a 1s Retry-After.
        let delay = retry_delay(4, Some(Duration::from_secs(1)));
        assert!(delay >= Duration::from_secs_f64(16.0 * 0.75));
    }

    #[test]
    fn retry_delay_caps_retry_after_at_max_delay() {
        let delay = retry_delay(0, Some(Duration::from_secs(120)));
        assert_eq!(delay, MAX_DELAY);
    }

    #[test]
    fn retry_delay_without_retry_after_falls_back_to_backoff() {
        let delay = retry_delay(0, None);
        assert!(delay >= Duration::from_secs_f64(0.75));
        assert!(delay <= Duration::from_secs_f64(1.25));
    }
}
