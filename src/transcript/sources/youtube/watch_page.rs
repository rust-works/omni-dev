//! Watch-page bootstrap: scrapes `INNERTUBE_CONTEXT.client.visitorData`
//! from a YouTube `/watch` HTML response.
//!
//! YouTube's anti-bot gating is keyed off whether the requesting session
//! carries a `visitorData` token, *not* the user-facing `clientName`.
//! Procedure: GET any `/watch?v=<stable-id>` page, regex out the
//! `"visitorData":"..."` substring from the inline `ytcfg.set({...})`
//! script block, and forward it on subsequent InnerTube `/player` POSTs
//! as a sibling of `clientName` under `context.client`.
//!
//! Requests carrying the token are treated as continuation requests from
//! a "real" session and bypass the bot challenge that otherwise refuses
//! every client variant with `LOGIN_REQUIRED`, `UNPLAYABLE`, etc.
//!
//! The token is per-process and rotates server-side; one scrape per
//! [`super::Youtube`] instance is sufficient (callers cache via
//! `tokio::sync::OnceCell`).

use std::sync::OnceLock;

use regex::Regex;

use crate::transcript::error::{Result, TranscriptError};

/// Stable, captioned video used as the bootstrap target. The `/watch`
/// response carries the same `ytcfg.set` block regardless of which video
/// the URL references, so a known-stable ID is preferable to whatever the
/// caller is actually after — it short-circuits the bootstrap if the
/// caller's video is itself unavailable.
const BOOTSTRAP_VIDEO_ID: &str = "dQw4w9WgXcQ";

/// User-Agent advertised on the watch-page GET. The watch page is a
/// public HTML page intended for browsers; YouTube serves a different
/// (Polymer-shaped) body to non-browser UAs that omits the
/// `INNERTUBE_CONTEXT` block. Browser-shaped UA is required.
///
/// Distinct from [`super::USER_AGENT`], which the InnerTube `/player`
/// POSTs use — that one must match `clientName: ANDROID_VR`.
///
/// Also reused by [`super::channel`] when scraping a channel page for its
/// `channelId` — same browser-vs-bot reasoning applies there.
pub(crate) const BROWSER_USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

/// Lazy-compiled extractor. `visitorData` strings don't contain raw `"`,
/// so a non-greedy character class up to the closing quote is sufficient
/// — no need for a real JSON parser. Anchored on the exact key to avoid
/// matching neighbouring `*Data` keys (`sessionData`, etc.).
fn visitor_data_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    #[allow(clippy::expect_used)]
    RE.get_or_init(|| {
        Regex::new(r#""visitorData":"([^"]+)""#).expect("visitor_data regex must compile")
    })
}

/// GET the YouTube watch page and extract `visitorData`.
///
/// `base_url` is normally `https://www.youtube.com`; tests inject a
/// `wiremock::MockServer::uri()` instead.
///
/// On a parseable response that lacks the token, returns
/// [`TranscriptError::MissingVisitorData`] — distinct from a generic
/// parse error so callers can surface a clear "watch-page format drifted"
/// signal rather than confusing it with a malformed `playerResponse`.
pub async fn fetch_visitor_data(http: &reqwest::Client, base_url: &str) -> Result<String> {
    let url = format!(
        "{base}/watch?v={video}",
        base = base_url.trim_end_matches('/'),
        video = BOOTSTRAP_VIDEO_ID,
    );
    let started = std::time::Instant::now();
    let result = http
        .get(&url)
        .header(reqwest::header::USER_AGENT, BROWSER_USER_AGENT)
        .send()
        .await;
    super::record_yt_http("GET", &url, started, &result);
    let body = result?.error_for_status()?.text().await?;

    extract_visitor_data(&body)
        .map(str::to_string)
        .ok_or(TranscriptError::MissingVisitorData { url })
}

/// Pure helper: pull `visitorData` out of a watch-page body. Split out so
/// the regex contract can be exercised without spinning up a mock server.
fn extract_visitor_data(body: &str) -> Option<&str> {
    visitor_data_regex()
        .captures(body)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const FIXTURE: &str = include_str!("fixtures/watch_page_with_visitor_data.html");
    const EXPECTED_TOKEN: &str = "CgtkUTQyOFR3aV9NSSjFoYvBBjIKCgJVUxIEGgAgPg%3D%3D";

    fn http() -> reqwest::Client {
        reqwest::Client::builder().build().unwrap()
    }

    // ── pure regex layer ──

    #[test]
    fn extracts_visitor_data_from_fixture() {
        let token = extract_visitor_data(FIXTURE).unwrap();
        assert_eq!(token, EXPECTED_TOKEN);
    }

    #[test]
    fn extracts_first_match_when_multiple_present() {
        // The fixture also contains a `VISITOR_DATA` (uppercase) key with
        // the same value; the regex is anchored on the lowercase
        // `visitorData` so it picks the right one regardless.
        let body = r#"{"foo":"bar","visitorData":"first","visitorData":"second"}"#;
        assert_eq!(extract_visitor_data(body), Some("first"));
    }

    #[test]
    fn ignores_neighbouring_data_keys() {
        // `sessionData`, `userData`, etc. must not match — only the exact
        // key `visitorData` should.
        let body = r#"{"sessionData":"S","userData":"U","fooData":"F"}"#;
        assert_eq!(extract_visitor_data(body), None);
    }

    #[test]
    fn returns_none_when_token_missing() {
        let body = "<html><body>no ytcfg here</body></html>";
        assert_eq!(extract_visitor_data(body), None);
    }

    #[test]
    fn returns_none_for_empty_body() {
        assert_eq!(extract_visitor_data(""), None);
    }

    // ── HTTP layer ──

    fn watch_page_mock(body: &'static str) -> Mock {
        Mock::given(method("GET"))
            .and(path("/watch"))
            .and(query_param("v", BOOTSTRAP_VIDEO_ID))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
    }

    #[tokio::test]
    async fn fetch_returns_extracted_token() {
        let server = MockServer::start().await;
        watch_page_mock(FIXTURE).mount(&server).await;

        let token = fetch_visitor_data(&http(), &server.uri()).await.unwrap();
        assert_eq!(token, EXPECTED_TOKEN);
    }

    #[tokio::test]
    async fn fetch_sends_browser_user_agent() {
        // The watch page omits INNERTUBE_CONTEXT for non-browser UAs,
        // so the bootstrap UA must be the desktop string — not the
        // ANDROID_VR UA used by the InnerTube call. Capture inbound
        // request and assert directly so a UA mismatch surfaces as a
        // clear assertion rather than a 404 from a non-matching mock.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/watch"))
            .respond_with(|req: &Request| {
                let ua = req
                    .headers
                    .get("user-agent")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                assert_eq!(ua, BROWSER_USER_AGENT);
                ResponseTemplate::new(200).set_body_string(FIXTURE)
            })
            .expect(1)
            .mount(&server)
            .await;

        let _ = fetch_visitor_data(&http(), &server.uri()).await.unwrap();
    }

    #[tokio::test]
    async fn fetch_targets_bootstrap_video_id() {
        // expect(1) on the mock proves the requested ?v= matched the
        // bootstrap constant; if it didn't the mock wouldn't fire.
        let server = MockServer::start().await;
        watch_page_mock(FIXTURE).expect(1).mount(&server).await;

        let _ = fetch_visitor_data(&http(), &server.uri()).await.unwrap();
    }

    #[tokio::test]
    async fn fetch_surfaces_missing_token_as_typed_error() {
        let server = MockServer::start().await;
        let body = "<html><body>no ytcfg here</body></html>";
        Mock::given(method("GET"))
            .and(path("/watch"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;

        let err = fetch_visitor_data(&http(), &server.uri())
            .await
            .unwrap_err();
        match err {
            TranscriptError::MissingVisitorData { url } => {
                assert!(url.contains("/watch?v=dQw4w9WgXcQ"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_surfaces_non_2xx_as_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/watch"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let err = fetch_visitor_data(&http(), &server.uri())
            .await
            .unwrap_err();
        assert!(matches!(err, TranscriptError::Http(_)));
    }

    #[tokio::test]
    async fn fetch_normalises_trailing_slash_in_base_url() {
        let server = MockServer::start().await;
        watch_page_mock(FIXTURE).expect(1).mount(&server).await;

        let with_slash = format!("{}/", server.uri());
        let _ = fetch_visitor_data(&http(), &with_slash).await.unwrap();
    }
}
