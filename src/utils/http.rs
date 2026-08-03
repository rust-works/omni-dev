//! Shared HTTP helpers for the REST clients.
//!
//! [`retry_429`] is the literal-429-only driver behind the Atlassian and
//! Datadog clients — a thin wrapper over [`retry_if`], the general driver
//! that also lets Gmail retry its own quota-exhaustion signal (HTTP 403 with
//! a `reason` Atlassian/Datadog never emit). Both rebuild the request per
//! attempt, log every attempt, and on a retryable response wait per
//! `Retry-After`, then `X-RateLimit-Reset`, then exponential backoff.
//! Consolidating the previously per-verb loops also unified the
//! `X-RateLimit-Reset` awareness that used to live only in Datadog (#1152).

use std::time::{Duration, Instant};

use reqwest::{Response, ResponseBuilderExt as _};

/// Standard HTTP request timeout shared by the REST clients.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Maximum number of retries on a retryable response (attempts =
/// `MAX_RETRIES` + 1).
const MAX_RETRIES: u32 = 3;

/// Base (seconds) for exponential backoff when neither `Retry-After` nor
/// `X-RateLimit-Reset` is present: `DEFAULT_RETRY_DELAY_SECS ^ (attempt + 1)`.
const DEFAULT_RETRY_DELAY_SECS: u64 = 2;

/// Drives an HTTP request through the shared literal-429 retry loop.
///
/// A thin [`retry_if`] wrapper retrying only `status == 429` — Atlassian and
/// Datadog never emit anything else worth retrying, so this keeps their call
/// sites unchanged.
pub(crate) async fn retry_429<B, L>(build: B, log: L) -> reqwest::Result<Response>
where
    B: Fn() -> reqwest::RequestBuilder,
    L: Fn(Instant, &reqwest::Result<Response>),
{
    retry_if(build, log, |status, _body| status == 429).await
}

/// Drives an HTTP request through a retry loop with a caller-supplied
/// retryability predicate.
///
/// `build` is called once per attempt to produce a fresh [`RequestBuilder`],
/// so bodies are always replayable; `log` receives the send result of every
/// attempt for the request log, called before any body is read. Transport
/// errors are returned to the caller without retry. A successful response is
/// returned untouched, without ever reading its body. On a non-success
/// response, the body is buffered once (needed either way — every caller
/// already reads a non-2xx body via its own `response_to_error`-equivalent
/// downstream) and passed to `is_retryable` alongside the status; a `true`
/// verdict below the retry ceiling waits per [`wait_for_retry`] and retries.
/// Otherwise the response is reconstructed from its captured status,
/// version, headers, URL, and buffered body and returned — callers see an
/// ordinary [`Response`] whose body reads exactly as it would have
/// unbuffered.
///
/// [`RequestBuilder`]: reqwest::RequestBuilder
pub(crate) async fn retry_if<B, L, P>(
    build: B,
    log: L,
    is_retryable: P,
) -> reqwest::Result<Response>
where
    B: Fn() -> reqwest::RequestBuilder,
    L: Fn(Instant, &reqwest::Result<Response>),
    P: Fn(u16, &[u8]) -> bool,
{
    let mut attempt = 0;
    loop {
        let started = Instant::now();
        let result = build().send().await;
        log(started, &result);
        let response = result?;
        if response.status().is_success() {
            return Ok(response);
        }

        let status = response.status();
        let version = response.version();
        let url = response.url().clone();
        let headers = response.headers().clone();
        let body = response.bytes().await?;

        if is_retryable(status.as_u16(), &body) && attempt < MAX_RETRIES {
            wait_for_retry(&headers, status.as_u16(), attempt).await;
            attempt += 1;
            continue;
        }

        let mut builder = http::Response::builder()
            .status(status)
            .version(version)
            .url(url);
        if let Some(header_map) = builder.headers_mut() {
            header_map.extend(
                headers
                    .iter()
                    .map(|(name, value)| (name.clone(), value.clone())),
            );
        }
        // Rebuilding from status/version/headers/url the HTTP library already
        // parsed successfully out of a real response cannot fail.
        #[allow(clippy::expect_used)]
        let rebuilt = builder
            .body(body)
            .expect("rebuilding a response from its own already-valid parts cannot fail");
        return Ok(Response::from(rebuilt));
    }
}

/// Waits before retrying a retryable (429, or a caller-recognised
/// equivalent) response.
///
/// Consults, in order: `Retry-After`, then Datadog's `X-RateLimit-Reset`, then
/// exponential backoff (`DEFAULT_RETRY_DELAY_SECS ^ (attempt + 1)`).
async fn wait_for_retry(headers: &reqwest::header::HeaderMap, status: u16, attempt: u32) {
    let delay = header_u64(headers, "Retry-After")
        .or_else(|| header_u64(headers, "X-RateLimit-Reset"))
        .unwrap_or_else(|| DEFAULT_RETRY_DELAY_SECS.pow(attempt + 1));

    eprintln!(
        "Rate limited ({status}). Retrying in {delay}s (attempt {})...",
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

    // ── retry_if: caller-supplied predicate (the Gmail 403 case) ──────

    #[tokio::test]
    async fn retry_if_retries_a_custom_status_when_predicate_says_so() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(403).set_body_string("quota exceeded"))
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
        let resp = retry_if(
            || client.get(&url),
            |_s, _r| {},
            |status, _body| status == 403,
        )
        .await
        .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
    }

    #[tokio::test]
    async fn retry_if_does_not_retry_when_predicate_says_no() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(403).set_body_string("insufficientPermissions"))
            .expect(1)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/x", server.uri());
        let resp = retry_if(
            || client.get(&url),
            |_s, _r| {},
            |status, body| status == 403 && body == b"rateLimitExceeded",
        )
        .await
        .unwrap();
        assert_eq!(resp.status().as_u16(), 403);
    }

    #[tokio::test]
    async fn retry_if_preserves_headers_and_body_through_reconstruction_on_give_up() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(
                ResponseTemplate::new(429)
                    .append_header("X-RateLimit-Remaining", "0")
                    .set_body_string("too many requests"),
            )
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/x", server.uri());
        let resp = retry_429(|| client.get(&url), |_s, _r| {}).await.unwrap();
        assert_eq!(resp.status().as_u16(), 429);
        assert_eq!(resp.headers().get("X-RateLimit-Remaining").unwrap(), "0");
        let body = resp.text().await.unwrap();
        assert_eq!(body, "too many requests");
    }

    #[tokio::test]
    async fn retry_if_preserves_body_on_first_attempt_give_up() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/x"))
            .respond_with(ResponseTemplate::new(403).set_body_string("insufficientPermissions"))
            .expect(1)
            .mount(&server)
            .await;

        let client = reqwest::Client::new();
        let url = format!("{}/x", server.uri());
        let resp = retry_if(|| client.get(&url), |_s, _r| {}, |_s, _b| false)
            .await
            .unwrap();
        let body = resp.text().await.unwrap();
        assert_eq!(body, "insufficientPermissions");
    }
}
