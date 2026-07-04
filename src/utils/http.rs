//! Shared HTTP helpers for the REST clients.
//!
//! [`retry_429`] is the single 429-retry driver behind the Atlassian and
//! Datadog clients: it rebuilds the request per attempt, logs every attempt,
//! and on a `429` waits per `Retry-After`, then `X-RateLimit-Reset`, then
//! exponential backoff before retrying. Consolidating the previously per-verb
//! loops also unified the `X-RateLimit-Reset` awareness that used to live only
//! in Datadog (#1152).

use std::time::{Duration, Instant};

/// Standard HTTP request timeout shared by the REST clients.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of retries on HTTP 429 (attempts = `MAX_RETRIES` + 1).
const MAX_RETRIES: u32 = 3;

/// Base (seconds) for exponential backoff when neither `Retry-After` nor
/// `X-RateLimit-Reset` is present: `DEFAULT_RETRY_DELAY_SECS ^ (attempt + 1)`.
const DEFAULT_RETRY_DELAY_SECS: u64 = 2;

/// Drives an HTTP request through the shared 429-retry loop.
///
/// `build` is called once per attempt to produce a fresh [`RequestBuilder`],
/// so bodies are always replayable; `log` receives the send result of every
/// attempt for the request log. Transport errors are returned to the caller
/// without retry — only a `429` below the retry ceiling is retried, waiting per
/// [`wait_for_retry`].
///
/// [`RequestBuilder`]: reqwest::RequestBuilder
pub(crate) async fn retry_429<B, L>(build: B, log: L) -> reqwest::Result<reqwest::Response>
where
    B: Fn() -> reqwest::RequestBuilder,
    L: Fn(Instant, &reqwest::Result<reqwest::Response>),
{
    for attempt in 0..=MAX_RETRIES {
        let started = Instant::now();
        let result = build().send().await;
        log(started, &result);
        match result {
            Ok(response) if response.status().as_u16() == 429 && attempt < MAX_RETRIES => {
                wait_for_retry(&response, attempt).await;
            }
            other => return other,
        }
    }
    unreachable!("loop returns on the final (MAX_RETRIES) attempt")
}

/// Waits before retrying a rate-limited (`429`) request.
///
/// Consults, in order: `Retry-After`, then Datadog's `X-RateLimit-Reset`, then
/// exponential backoff (`DEFAULT_RETRY_DELAY_SECS ^ (attempt + 1)`).
async fn wait_for_retry(response: &reqwest::Response, attempt: u32) {
    let headers = response.headers();
    let delay = header_u64(headers, "Retry-After")
        .or_else(|| header_u64(headers, "X-RateLimit-Reset"))
        .unwrap_or_else(|| DEFAULT_RETRY_DELAY_SECS.pow(attempt + 1));

    eprintln!(
        "Rate limited (429). Retrying in {delay}s (attempt {})...",
        attempt + 1
    );
    tokio::time::sleep(Duration::from_secs(delay)).await;
}

/// Parses a header value as a `u64`, if present and numeric.
fn header_u64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<u64> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn retries_429_then_succeeds_and_logs_each_attempt() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(429).append_header("Retry-After", "0"))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(200))
            .with_priority(2)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/x", server.uri());
        let calls = AtomicUsize::new(0);
        let resp = retry_429(
            || client.get(&url),
            |_started, _result| {
                calls.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await
        .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        // Logged both the 429 attempt and the successful retry.
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn returns_429_after_max_retries() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(429).append_header("Retry-After", "0"))
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/x", server.uri());
        let calls = AtomicUsize::new(0);
        let resp = retry_429(
            || client.get(&url),
            |_s, _r| {
                calls.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await
        .unwrap();
        assert_eq!(resp.status().as_u16(), 429);
        assert_eq!(calls.load(Ordering::SeqCst), (MAX_RETRIES + 1) as usize);
    }

    #[tokio::test]
    async fn honours_x_ratelimit_reset() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(429).append_header("X-RateLimit-Reset", "0"))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(200))
            .with_priority(2)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/x", server.uri());
        let resp = retry_429(|| client.get(&url), |_s, _r| {}).await.unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn does_not_retry_non_429() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/x", server.uri());
        let resp = retry_429(|| client.get(&url), |_s, _r| {}).await.unwrap();
        assert_eq!(resp.status().as_u16(), 500);
    }

    #[tokio::test]
    async fn transport_error_is_returned_without_retry() {
        // Port 1 refuses immediately; the send fails at the transport layer.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();
        let url = "http://127.0.0.1:1/x".to_string();
        let calls = AtomicUsize::new(0);
        let result = retry_429(
            || client.get(&url),
            |_s, _r| {
                calls.fetch_add(1, Ordering::SeqCst);
            },
        )
        .await;
        assert!(result.is_err());
        // A transport error is not retried.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
