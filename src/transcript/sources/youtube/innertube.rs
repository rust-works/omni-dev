//! HTTP wrapper around YouTube's InnerTube `/player` endpoint.
//!
//! Step 3 of [issue #687](https://github.com/rust-works/omni-dev/issues/687)
//! pins the **WEB** client only. The Android / IosEmbedded fallback chain
//! used for age-gated content lands in step 5; that work introduces a
//! dedicated `client.rs` with an enum, at which point the constants below
//! migrate. Until then they live next to the only HTTP call that uses them.

use serde_json::json;

use crate::transcript::error::Result;

/// Path appended to the `base_url` for the `/player` POST. YouTube also
/// accepts this without the API key, but we forward the public WEB key for
/// parity with browsers.
pub(crate) const PLAYER_PATH: &str = "/youtubei/v1/player";

/// Public WEB-client API key. Long-stable across years, embedded in the
/// YouTube watch page's bootstrapped config — this is not a credential.
pub(crate) const WEB_API_KEY: &str = "AIzaSyAO_FJ2SlqU8Q4STEHLGCilw_Y9_11qcW8";

/// `client.clientName` for the WEB context.
pub(crate) const CLIENT_NAME: &str = "WEB";

/// `client.clientVersion` for the WEB context. YouTube treats stale
/// versions leniently, but the value drifts over months — refresh if
/// `/player` starts returning empty `playerResponse` envelopes.
pub(crate) const CLIENT_VERSION: &str = "2.20250101.00.00";

/// POST `videoId` to the InnerTube `/player` endpoint at `base_url` and
/// return the raw response body. Callers feed the body to
/// [`super::player_response::parse`].
///
/// `base_url` is normally `https://www.youtube.com`; tests inject a
/// `wiremock::MockServer::uri()` instead.
pub async fn fetch_player_response(
    http: &reqwest::Client,
    base_url: &str,
    video_id: &str,
) -> Result<String> {
    let url = format!(
        "{base}{path}?key={key}",
        base = base_url.trim_end_matches('/'),
        path = PLAYER_PATH,
        key = WEB_API_KEY,
    );
    let body = json!({
        "context": {
            "client": {
                "clientName": CLIENT_NAME,
                "clientVersion": CLIENT_VERSION,
                "hl": "en",
                "gl": "US",
            },
        },
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true,
    });

    let response = http
        .post(&url)
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
    Ok(response.text().await?)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::Value;
    use wiremock::matchers::{body_partial_json, method, path, query_param};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const VIDEO_ID: &str = "dQw4w9WgXcQ";
    const FIXTURE_BASIC: &str = include_str!("fixtures/player_response_basic.json");

    fn http() -> reqwest::Client {
        reqwest::Client::builder().build().unwrap()
    }

    #[tokio::test]
    async fn posts_to_player_endpoint_with_web_context_and_video_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PLAYER_PATH))
            .and(query_param("key", WEB_API_KEY))
            .and(body_partial_json(json!({
                "videoId": VIDEO_ID,
                "context": { "client": { "clientName": CLIENT_NAME } },
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_BASIC))
            .expect(1)
            .mount(&server)
            .await;

        let body = fetch_player_response(&http(), &server.uri(), VIDEO_ID)
            .await
            .unwrap();
        assert_eq!(body, FIXTURE_BASIC);
    }

    #[tokio::test]
    async fn forwards_pinned_client_version() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PLAYER_PATH))
            .and(body_partial_json(json!({
                "context": { "client": { "clientVersion": CLIENT_VERSION } },
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;

        let _ = fetch_player_response(&http(), &server.uri(), VIDEO_ID)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn surfaces_non_2xx_as_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PLAYER_PATH))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let err = fetch_player_response(&http(), &server.uri(), VIDEO_ID)
            .await
            .unwrap_err();
        assert!(matches!(err, crate::transcript::TranscriptError::Http(_)));
        assert!(err.to_string().contains("500"));
    }

    #[tokio::test]
    async fn body_includes_check_flags() {
        // Capture the inbound JSON to assert the playability flags are set.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PLAYER_PATH))
            .respond_with(|req: &Request| {
                let parsed: Value = serde_json::from_slice(&req.body).unwrap();
                assert_eq!(parsed["contentCheckOk"], Value::Bool(true));
                assert_eq!(parsed["racyCheckOk"], Value::Bool(true));
                ResponseTemplate::new(200).set_body_string("{}")
            })
            .expect(1)
            .mount(&server)
            .await;

        let _ = fetch_player_response(&http(), &server.uri(), VIDEO_ID)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn trailing_slash_in_base_url_is_normalised() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PLAYER_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;

        let with_slash = format!("{}/", server.uri());
        let _ = fetch_player_response(&http(), &with_slash, VIDEO_ID)
            .await
            .unwrap();
    }
}
