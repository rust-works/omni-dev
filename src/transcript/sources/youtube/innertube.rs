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

use serde_json::{json, Value};

use crate::transcript::error::Result;

/// Path appended to the `base_url` for the `/player` POST.
pub(crate) const PLAYER_PATH: &str = "/youtubei/v1/player";

/// Path appended to the `base_url` for the `/browse` POST. Used to enumerate
/// a channel's uploads (see [`super::channel`]). Unlike `/player`, browse uses
/// the **WEB** client (see [`web_client_context`]): only the WEB grid is
/// paginated (continuation tokens) and carries the `richGridRenderer` /
/// `lockupViewModel` shape the channel parser reads. The `ANDROID_VR` client
/// returns an unpaginated `compactVideoRenderer` mobile shape instead.
pub(crate) const BROWSE_PATH: &str = "/youtubei/v1/browse";

/// Public InnerTube API key for the ANDROID-family clients (including
/// `ANDROID_VR`). Sent as the `X-Goog-Api-Key` header — YouTube returns
/// 400 for the legacy `?key=` query form on modern `/player` paths.
/// Embedded in the public Oculus YouTube app binary; not a credential.
pub(crate) const INNERTUBE_API_KEY: &str = "AIzaSyA8eiZmM1FaDVjRy-df2KTyQ_vz_yYM39w";

/// Public InnerTube API key for the `WEB` client, used by the channel
/// [`fetch_browse`] path. Like [`INNERTUBE_API_KEY`], it is embedded in the
/// public web app and is not a credential.
pub(crate) const WEB_INNERTUBE_API_KEY: &str = "AIzaSyAO_FJ2SlqU8Q4STEHLGCilw_Y9_11qcW8";

/// `client.clientName` for the WEB browse client.
pub(crate) const WEB_CLIENT_NAME: &str = "WEB";

/// `client.clientVersion` for the WEB browse client. Browse is not bot-gated
/// the way `/player` is, so this need only be recent enough to be accepted.
pub(crate) const WEB_CLIENT_VERSION: &str = "2.20240101.00.00";

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

/// The `context.client` object pinned to the `ANDROID_VR` fingerprint and
/// carrying the scraped `visitorData` token. Shared by every InnerTube POST
/// ([`fetch_player_response`], [`fetch_browse`]) so the device fingerprint
/// and bot-bypass token stay identical across endpoints.
pub(crate) fn client_context(visitor_data: &str) -> Value {
    json!({
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
    })
}

/// The `context.client` object for the **WEB** client used by channel browse.
/// Carries no device fingerprint or `visitorData` — browse is a public,
/// non-bot-gated endpoint, unlike `/player`.
pub(crate) fn web_client_context() -> Value {
    json!({
        "client": {
            "clientName": WEB_CLIENT_NAME,
            "clientVersion": WEB_CLIENT_VERSION,
            "hl": "en",
            "gl": "US",
        },
    })
}

/// POST `body` to the InnerTube `/browse` endpoint and return the raw body.
///
/// Callers build `body` with a WEB `context` (see [`web_client_context`]) plus
/// either a `browseId` (first page) or a `continuation` token (subsequent
/// pages); [`super::channel`] feeds the result to its `lockupViewModel` /
/// continuation parser. Sent with the WEB API key — the ANDROID key returns
/// the unpaginated mobile shape.
///
/// `base_url` is normally `https://www.youtube.com`; tests inject a
/// `wiremock::MockServer::uri()` instead.
pub async fn fetch_browse(http: &reqwest::Client, base_url: &str, body: &Value) -> Result<String> {
    let url = format!(
        "{base}{path}",
        base = base_url.trim_end_matches('/'),
        path = BROWSE_PATH,
    );
    let response = http
        .post(&url)
        .header(API_KEY_HEADER, WEB_INNERTUBE_API_KEY)
        .json(body)
        .send()
        .await?
        .error_for_status()?;
    Ok(response.text().await?)
}

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
        "context": client_context(visitor_data),
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

/// POST `videoId` to the InnerTube `/player` endpoint using the **WEB**
/// client and return the raw response body. Callers feed the body to
/// [`super::metadata::parse`].
///
/// Unlike [`fetch_player_response`], this carries no device fingerprint and
/// **no `visitorData`** — the WEB `/player` call is not bot-gated for the
/// metadata it returns. It reports `playabilityStatus: UNPLAYABLE` for
/// streaming purposes, but still includes both `videoDetails` and the
/// `microformat.playerMicroformatRenderer` block (publish date, like count,
/// category, …) that the `ANDROID_VR` transcript path lacks. That
/// independence is what lets metadata be refreshed for already-synced videos
/// without touching the gated transcript path.
///
/// `base_url` is normally `https://www.youtube.com`; tests inject a
/// `wiremock::MockServer::uri()` instead.
pub async fn fetch_player_response_web(
    http: &reqwest::Client,
    base_url: &str,
    video_id: &str,
) -> Result<String> {
    let url = format!(
        "{base}{path}",
        base = base_url.trim_end_matches('/'),
        path = PLAYER_PATH,
    );
    let body = json!({
        "context": web_client_context(),
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true,
    });

    let response = http
        .post(&url)
        .header(API_KEY_HEADER, WEB_INNERTUBE_API_KEY)
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
    async fn browse_posts_to_browse_endpoint_with_web_key_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(BROWSE_PATH))
            .and(header(API_KEY_HEADER, WEB_INNERTUBE_API_KEY))
            .and(body_partial_json(json!({
                "browseId": "UC_x5XG1OV2P6uZZ5FSM9Ttw",
                "context": { "client": { "clientName": WEB_CLIENT_NAME } },
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"ok":true}"#))
            .expect(1)
            .mount(&server)
            .await;

        let body = json!({
            "context": web_client_context(),
            "browseId": "UC_x5XG1OV2P6uZZ5FSM9Ttw",
        });
        let out = fetch_browse(&http(), &server.uri(), &body).await.unwrap();
        assert_eq!(out, r#"{"ok":true}"#);
    }

    #[tokio::test]
    async fn browse_surfaces_non_2xx_as_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(BROWSE_PATH))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let body = json!({ "context": client_context(VISITOR_DATA), "browseId": "UCabc" });
        let err = fetch_browse(&http(), &server.uri(), &body)
            .await
            .unwrap_err();
        assert!(matches!(err, crate::transcript::TranscriptError::Http(_)));
    }

    #[test]
    fn client_context_pins_full_quest_fingerprint() {
        let ctx = client_context("vd-token");
        let client = &ctx["client"];
        assert_eq!(client["clientName"], CLIENT_NAME);
        assert_eq!(client["clientVersion"], CLIENT_VERSION);
        assert_eq!(client["osVersion"], OS_VERSION);
        assert_eq!(client["visitorData"], "vd-token");
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

    #[tokio::test]
    async fn web_player_posts_with_web_key_and_web_client_and_no_visitor_data() {
        // The metadata path uses the WEB client and key (un-gated), and must
        // NOT carry a visitorData token — that is the ANDROID_VR-only signal.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PLAYER_PATH))
            .and(header(API_KEY_HEADER, WEB_INNERTUBE_API_KEY))
            .respond_with(|req: &Request| {
                let parsed: Value = serde_json::from_slice(&req.body).unwrap();
                assert_eq!(parsed["videoId"], VIDEO_ID);
                assert_eq!(parsed["context"]["client"]["clientName"], WEB_CLIENT_NAME);
                assert!(
                    parsed["context"]["client"]["visitorData"].is_null(),
                    "WEB metadata call must not carry visitorData"
                );
                ResponseTemplate::new(200).set_body_string("{}")
            })
            .expect(1)
            .mount(&server)
            .await;

        let _ = fetch_player_response_web(&http(), &server.uri(), VIDEO_ID)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn web_player_surfaces_non_2xx_as_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PLAYER_PATH))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let err = fetch_player_response_web(&http(), &server.uri(), VIDEO_ID)
            .await
            .unwrap_err();
        assert!(matches!(err, crate::transcript::TranscriptError::Http(_)));
    }
}
