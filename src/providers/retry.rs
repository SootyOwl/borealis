use std::time::Duration;

use rand::Rng;
use tracing::{debug, warn};

/// Retry configuration with exponential backoff.
const BASE_DELAY: Duration = Duration::from_secs(1);
const MULTIPLIER: f64 = 2.0;
const MAX_DELAY: Duration = Duration::from_secs(30);
const JITTER_FRACTION: f64 = 0.25;

/// Whether an HTTP status code is retryable (429 rate limit or 5xx server error).
pub fn is_retryable_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
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

/// Execute an HTTP request with retry logic.
///
/// Retries on 429 and 5xx status codes up to `max_retries` times with exponential backoff.
/// Returns the successful response or the last error.
pub async fn with_retry<F, Fut>(
    provider_name: &str,
    max_retries: u32,
    make_request: F,
) -> Result<reqwest::Response, RetryError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
{
    let mut last_error = None;

    for attempt in 0..=max_retries {
        match make_request().await {
            Ok(response) => {
                let status = response.status().as_u16();
                if is_retryable_status(status) && attempt < max_retries {
                    let delay = backoff_delay(attempt);
                    warn!(
                        provider = provider_name,
                        status,
                        attempt = attempt + 1,
                        max_retries,
                        delay_ms = delay.as_millis() as u64,
                        "Retryable status, backing off"
                    );
                    tokio::time::sleep(delay).await;
                    last_error = Some(RetryError::HttpStatus {
                        status,
                        body: response.text().await.unwrap_or_default(),
                    });
                    continue;
                }

                if !response.status().is_success() {
                    let status = response.status().as_u16();
                    let body = response.text().await.unwrap_or_default();
                    return Err(RetryError::HttpStatus { status, body });
                }

                debug!(provider = provider_name, attempt, "Request succeeded");
                return Ok(response);
            }
            Err(e) => {
                if attempt < max_retries {
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
                    last_error = Some(RetryError::Network(e));
                    continue;
                }
                return Err(RetryError::Network(e));
            }
        }
    }

    Err(last_error.unwrap_or(RetryError::Exhausted))
}

/// Errors that can occur during retry.
#[derive(Debug, thiserror::Error)]
pub enum RetryError {
    #[error("HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },

    #[error("Network error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("All retries exhausted")]
    Exhausted,
}
