//! HTTP wrapper around YouTube's InnerTube `/player` endpoint.
//!
//! Client identity (WEB / ANDROID_VR / TVHTML5_SIMPLY_EMBEDDED_PLAYER / IOS)
//! is supplied by the caller via [`ClientContext`] so the same code path
//! services every rung of [`super::client::DEFAULT_CHAIN`]. See
//! [`super::client`] for the variant list and the rationale behind the
//! fallback ordering.

use serde_json::{json, Map, Value};

use crate::transcript::error::Result;
use crate::transcript::sources::youtube::client::ClientContext;

/// Path appended to the `base_url` for the `/player` POST.
pub(crate) const PLAYER_PATH: &str = "/youtubei/v1/player";

/// Header carrying the InnerTube API key. Modern YouTube rejects requests
/// that supply the key as the `?key=` URL parameter (returns 400), so we
/// send it as a header instead — same as the real YouTube apps.
const API_KEY_HEADER: &str = "X-Goog-Api-Key";

/// POST `videoId` to the InnerTube `/player` endpoint at `base_url`,
/// identifying as `client`, and return the raw response body. Callers feed
/// the body to [`super::player_response::parse`].
///
/// `base_url` is normally `https://www.youtube.com`; tests inject a
/// `wiremock::MockServer::uri()` instead.
pub async fn fetch_player_response(
    http: &reqwest::Client,
    base_url: &str,
    video_id: &str,
    client: &ClientContext,
) -> Result<String> {
    let url = format!(
        "{base}{path}",
        base = base_url.trim_end_matches('/'),
        path = PLAYER_PATH,
    );

    let mut client_obj = Map::new();
    client_obj.insert("clientName".into(), Value::String(client.name.into()));
    client_obj.insert("clientVersion".into(), Value::String(client.version.into()));
    client_obj.insert("hl".into(), Value::String("en".into()));
    client_obj.insert("gl".into(), Value::String("US".into()));
    if let Some(extra) = client.extra_context.as_ref().and_then(Value::as_object) {
        for (k, v) in extra {
            client_obj.insert(k.clone(), v.clone());
        }
    }

    let body = json!({
        "context": { "client": Value::Object(client_obj) },
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true,
    });

    let response = http
        .post(&url)
        .header(reqwest::header::USER_AGENT, client.user_agent)
        .header(API_KEY_HEADER, client.api_key)
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
    use crate::transcript::sources::youtube::client::InnertubeClient;
    use serde_json::Value;
    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    const VIDEO_ID: &str = "dQw4w9WgXcQ";
    const FIXTURE_BASIC: &str = include_str!("fixtures/player_response_basic.json");

    fn http() -> reqwest::Client {
        reqwest::Client::builder().build().unwrap()
    }

    #[tokio::test]
    async fn posts_to_player_endpoint_with_web_context_and_video_id() {
        let server = MockServer::start().await;
        let web = InnertubeClient::Web.context();
        Mock::given(method("POST"))
            .and(path(PLAYER_PATH))
            .and(header(API_KEY_HEADER, web.api_key))
            .and(body_partial_json(json!({
                "videoId": VIDEO_ID,
                "context": { "client": { "clientName": web.name } },
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_BASIC))
            .expect(1)
            .mount(&server)
            .await;

        let body = fetch_player_response(&http(), &server.uri(), VIDEO_ID, &web)
            .await
            .unwrap();
        assert_eq!(body, FIXTURE_BASIC);
    }

    #[tokio::test]
    async fn forwards_pinned_client_version() {
        let server = MockServer::start().await;
        let web = InnertubeClient::Web.context();
        Mock::given(method("POST"))
            .and(path(PLAYER_PATH))
            .and(body_partial_json(json!({
                "context": { "client": { "clientVersion": web.version } },
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;

        let _ = fetch_player_response(&http(), &server.uri(), VIDEO_ID, &web)
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

        let err = fetch_player_response(
            &http(),
            &server.uri(),
            VIDEO_ID,
            &InnertubeClient::Web.context(),
        )
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

        let _ = fetch_player_response(
            &http(),
            &server.uri(),
            VIDEO_ID,
            &InnertubeClient::Web.context(),
        )
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
        let _ = fetch_player_response(
            &http(),
            &with_slash,
            VIDEO_ID,
            &InnertubeClient::Web.context(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn user_agent_matches_client() {
        let server = MockServer::start().await;
        let android_vr = InnertubeClient::AndroidVr.context();
        Mock::given(method("POST"))
            .and(path(PLAYER_PATH))
            .and(header("user-agent", android_vr.user_agent))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;

        let _ = fetch_player_response(&http(), &server.uri(), VIDEO_ID, &android_vr)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn android_vr_body_includes_extra_context() {
        let server = MockServer::start().await;
        let android_vr = InnertubeClient::AndroidVr.context();
        Mock::given(method("POST"))
            .and(path(PLAYER_PATH))
            .and(body_partial_json(json!({
                "context": {
                    "client": {
                        "clientName": "ANDROID_VR",
                        "androidSdkVersion": 32,
                        "deviceMake": "Oculus",
                    }
                }
            })))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .expect(1)
            .mount(&server)
            .await;

        let _ = fetch_player_response(&http(), &server.uri(), VIDEO_ID, &android_vr)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn each_chain_client_can_be_dispatched() {
        // Smoke test: every variant produces a valid request the mock accepts.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(PLAYER_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        for variant in [
            InnertubeClient::Web,
            InnertubeClient::AndroidVr,
            InnertubeClient::TvEmbedded,
            InnertubeClient::Ios,
        ] {
            let ctx = variant.context();
            let _ = fetch_player_response(&http(), &server.uri(), VIDEO_ID, &ctx)
                .await
                .unwrap();
        }
    }
}
