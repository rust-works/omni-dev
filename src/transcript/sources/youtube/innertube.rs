//! HTTP wrapper around YouTube's InnerTube `/player` endpoint.
//!
//! Pins the `ANDROID_VR` client. YouTube's anti-bot gating cross-checks
//! `clientName`, `clientVersion`, the per-endpoint API key, the device
//! fingerprint fields (`deviceMake` / `deviceModel` / `osVersion`) and the
//! request `User-Agent`; mismatches are flagged. The constants below match
//! what the real Oculus YouTube app emits at time of writing.
//!
//! Sessions also carry a `visitorData` token scraped from the watch page
//! (see [`super::watch_page`]); requests without it are treated as
//! unauthenticated bot traffic and refused on most videos.
//!
//! ## Refresh signal
//!
//! These values drift over months as YouTube tightens per-client checks.
//! When `/player` starts returning empty or refused responses for known-
//! healthy videos, bump `CLIENT_VERSION` (and the matching `User-Agent`
//! token in `super::USER_AGENT`) to the value currently shipped by the
//! Oculus YouTube app, and refresh `INNERTUBE_API_KEY` if the
//! ANDROID-family key starts being rejected.

use serde_json::json;

use crate::transcript::error::Result;

/// Path appended to the `base_url` for the `/player` POST.
pub(crate) const PLAYER_PATH: &str = "/youtubei/v1/player";

/// Public InnerTube API key for the ANDROID-family clients (including
/// `ANDROID_VR`). Sent as the `X-Goog-Api-Key` header — YouTube returns
/// 400 for the legacy `?key=` query form on modern `/player` paths.
/// Embedded in the public Oculus YouTube app binary; not a credential.
pub(crate) const INNERTUBE_API_KEY: &str = "AIzaSyA8eiZmM1FaDVjRy-df2KTyQ_vz_yYM39w";

/// `client.clientName`.
pub(crate) const CLIENT_NAME: &str = "ANDROID_VR";

/// `client.clientVersion` — version published by the Oculus YouTube app.
pub(crate) const CLIENT_VERSION: &str = "1.62.27";

/// `client.androidSdkVersion` — API level for Android 12L.
pub(crate) const ANDROID_SDK_VERSION: u32 = 32;

/// `client.deviceMake`.
pub(crate) const DEVICE_MAKE: &str = "Oculus";

/// `client.deviceModel`.
pub(crate) const DEVICE_MODEL: &str = "Quest 3";

/// `client.osName`.
pub(crate) const OS_NAME: &str = "Android";

/// `client.osVersion`. Quest 3 specifically reports `12L` (literal), not
/// `12` — a mismatch is one of the bot-detection signals.
pub(crate) const OS_VERSION: &str = "12L";

/// Header carrying the InnerTube API key. Replaces the legacy `?key=` query
/// param; modern YouTube `/player` paths reject the latter with HTTP 400.
const API_KEY_HEADER: &str = "X-Goog-Api-Key";

/// POST `videoId` to the InnerTube `/player` endpoint at `base_url` and
/// return the raw response body. Callers feed the body to
/// [`super::player_response::parse`].
///
/// `base_url` is normally `https://www.youtube.com`; tests inject a
/// `wiremock::MockServer::uri()` instead.
///
/// `visitor_data` is the opaque token scraped from the watch page by
/// [`super::watch_page::fetch_visitor_data`]. Requests without it are
/// treated as unauthenticated bot traffic and refused.
pub async fn fetch_player_response(
    http: &reqwest::Client,
    base_url: &str,
    video_id: &str,
    visitor_data: &str,
) -> Result<String> {
    let url = format!(
        "{base}{path}",
        base = base_url.trim_end_matches('/'),
        path = PLAYER_PATH,
    );
    let body = json!({
        "context": {
            "client": {
                "clientName": CLIENT_NAME,
                "clientVersion": CLIENT_VERSION,
                "androidSdkVersion": ANDROID_SDK_VERSION,
                "deviceMake": DEVICE_MAKE,
                "deviceModel": DEVICE_MODEL,
                "osName": OS_NAME,
                "osVersion": OS_VERSION,
                "hl": "en",
                "gl": "US",
                "visitorData": visitor_data,
            },
        },
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true,
    });

    let response = http
        .post(&url)
        .header(API_KEY_HEADER, INNERTUBE_API_KEY)
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
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const VIDEO_ID: &str = "dQw4w9WgXcQ";
    const VISITOR_DATA: &str = "test-visitor-data";
    const FIXTURE_BASIC: &str = include_str!("fixtures/player_response_basic.json");

    fn http() -> reqwest::Client {
        reqwest::Client::builder().build().unwrap()
    }

    #[tokio::test]
    async fn posts_to_player_endpoint_with_android_vr_context_and_video_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PLAYER_PATH))
            .and(header(API_KEY_HEADER, INNERTUBE_API_KEY))
            .and(body_partial_json(json!({
                "videoId": VIDEO_ID,
                "context": { "client": { "clientName": CLIENT_NAME } },
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_BASIC))
            .expect(1)
            .mount(&server)
            .await;

        let body = fetch_player_response(&http(), &server.uri(), VIDEO_ID, VISITOR_DATA)
            .await
            .unwrap();
        assert_eq!(body, FIXTURE_BASIC);
    }

    #[tokio::test]
    async fn body_pins_full_quest_device_fingerprint() {
        // Catch a regression to "12" (correct value is the literal "12L"),
        // a bumped clientVersion that misses the device fields, etc.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PLAYER_PATH))
            .and(body_partial_json(json!({
                "context": {
                    "client": {
                        "clientName":        CLIENT_NAME,
                        "clientVersion":     CLIENT_VERSION,
                        "androidSdkVersion": ANDROID_SDK_VERSION,
                        "deviceMake":        DEVICE_MAKE,
                        "deviceModel":       DEVICE_MODEL,
                        "osName":            OS_NAME,
                        "osVersion":         OS_VERSION,
                        "hl":                "en",
                        "gl":                "US",
                    }
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;

        let _ = fetch_player_response(&http(), &server.uri(), VIDEO_ID, VISITOR_DATA)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn body_includes_visitor_data_under_client() {
        // Visitor data is the bot-bypass token; without it on the request
        // YouTube refuses with bot-shaped errors. Pin its placement.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PLAYER_PATH))
            .and(body_partial_json(json!({
                "context": { "client": { "visitorData": VISITOR_DATA } },
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;

        let _ = fetch_player_response(&http(), &server.uri(), VIDEO_ID, VISITOR_DATA)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn url_no_longer_carries_legacy_key_query() {
        // Modern /player rejects ?key= with HTTP 400. Capture the inbound
        // request and assert no query string is present.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PLAYER_PATH))
            .respond_with(|req: &Request| {
                assert!(
                    req.url.query().is_none(),
                    "request URL must not carry a query string; got {:?}",
                    req.url.query()
                );
                ResponseTemplate::new(200).set_body_string("{}")
            })
            .expect(1)
            .mount(&server)
            .await;

        let _ = fetch_player_response(&http(), &server.uri(), VIDEO_ID, VISITOR_DATA)
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

        let err = fetch_player_response(&http(), &server.uri(), VIDEO_ID, VISITOR_DATA)
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

        let _ = fetch_player_response(&http(), &server.uri(), VIDEO_ID, VISITOR_DATA)
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
        let _ = fetch_player_response(&http(), &with_slash, VIDEO_ID, VISITOR_DATA)
            .await
            .unwrap();
    }
}
