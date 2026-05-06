//! YouTube [`TranscriptSource`](crate::transcript::TranscriptSource).
//!
//! Walks [`client::DEFAULT_CHAIN`] (or a caller-supplied single client) when
//! resolving a video to a `playerResponse`: WEB first, then non-WEB clients
//! (Android VR, TV-embedded, iOS) on retryable refusals like `UNPLAYABLE`,
//! `LOGIN_REQUIRED`, `AGE_VERIFICATION_REQUIRED`, `CONTENT_CHECK_REQUIRED`.
//! Healthy videos are answered by WEB on the first request and pay no
//! extra request volume; only refused videos pay the cost of fanning out.

use std::time::Duration;

use async_trait::async_trait;
use tracing::debug;

use crate::transcript::error::{Result, TranscriptError};
use crate::transcript::source::{FetchOpts, LanguageInfo, MediaInfo, Transcript, TranscriptSource};

pub mod client;
pub mod innertube;
pub mod player_response;
pub mod timedtext;
pub mod url;

pub use client::{ClientContext, InnertubeClient, DEFAULT_CHAIN};
pub use player_response::{
    check_playability, extract_media_info, list_languages, parse as parse_player_response,
    select_track, CaptionTrack, PlayerResponse, SelectedTrack,
};
pub use timedtext::parse as parse_timedtext;
pub use url::extract_video_id;

/// Default origin for InnerTube and timedtext requests. Tests substitute
/// a `wiremock::MockServer::uri()` instead.
const DEFAULT_BASE_URL: &str = "https://www.youtube.com";

/// HTTP request timeout. Picked to match
/// [`crate::atlassian::client::AtlassianClient`]'s 30 s timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Whether `input` is recognised as a YouTube locator (URL or bare ID).
///
/// Used by the future `omni-dev transcript fetch <url>` auto-detection
/// path and by [`TranscriptSource::matches`].
pub fn matches_url(input: &str) -> bool {
    extract_video_id(input).is_ok()
}

/// Whether an error is worth retrying with a different InnerTube client.
///
/// Returns `true` for failures that may vary per-client:
///
/// * `PlayabilityRefused` (any status except `LIVE_STREAM_OFFLINE`) — WEB
///   enforces JS-derived signatures, sign-in checks, and clientVersion
///   staleness checks that the non-WEB clients skip. `ERROR` is included
///   because YouTube uses it as an ambiguous catch-all — covering both
///   genuinely-deleted videos *and* "client no longer supported" rejections
///   triggered by stale clientVersion strings — and the disambiguation
///   only happens once the chain has been walked.
/// * `Http` — different clients send subtly different request shapes;
///   IOS may receive 400 where ANDROID_VR receives a 200 with playability
///   refusal. yt-dlp retries across clients on HTTP errors too.
///
/// Returns `false` for `LIVE_STREAM_OFFLINE` (no client provides captions
/// for a stream that hasn't started), parse errors (next client will fail
/// the same way), and locator errors (purely client-side validation).
fn is_retryable_refusal(err: &TranscriptError) -> bool {
    match err {
        TranscriptError::PlayabilityRefused { status, .. } => {
            !matches!(status.as_str(), "LIVE_STREAM_OFFLINE")
        }
        TranscriptError::Http(_) => true,
        _ => false,
    }
}

/// YouTube [`TranscriptSource`].
///
/// Holds a single [`reqwest::Client`] reused across the InnerTube and
/// timedtext calls. Cheap to construct; in steady state it is fine to keep
/// one instance per process.
#[derive(Debug, Clone)]
pub struct Youtube {
    http: reqwest::Client,
    base_url: String,
    chain: Vec<InnertubeClient>,
}

impl Youtube {
    /// Construct a YouTube source with default HTTP settings (30 s timeout)
    /// targeting the public YouTube origin and the full
    /// [`DEFAULT_CHAIN`] fallback. The User-Agent is set per-request based
    /// on the active client.
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            base_url: DEFAULT_BASE_URL.to_string(),
            chain: DEFAULT_CHAIN.to_vec(),
        })
    }

    /// Construct a YouTube source pointed at an alternate origin. Used by
    /// tests to inject a `wiremock::MockServer::uri()`.
    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.into(),
            chain: DEFAULT_CHAIN.to_vec(),
        })
    }

    /// Replace the InnerTube client fallback chain with `chain`.
    ///
    /// `chain` must be non-empty. Useful for `--client web` (single rung
    /// for debugging) or for tests pinning a deterministic order.
    #[must_use]
    pub fn with_chain(mut self, chain: Vec<InnertubeClient>) -> Self {
        debug_assert!(!chain.is_empty(), "Youtube chain must be non-empty");
        self.chain = chain;
        self
    }

    /// One rung of the fallback chain: POST to InnerTube as `client`,
    /// parse the response, surface any non-`OK` `playabilityStatus` as
    /// [`TranscriptError::PlayabilityRefused`].
    async fn try_one_client(
        &self,
        video_id: &str,
        client: &ClientContext,
    ) -> Result<PlayerResponse> {
        let raw =
            innertube::fetch_player_response(&self.http, &self.base_url, video_id, client).await?;
        let response = parse_player_response(&raw)?;
        check_playability(&response)?;
        Ok(response)
    }

    /// Common preamble: locator → video ID → InnerTube POST loop →
    /// `playerResponse` parse → playability check.
    ///
    /// Walks `self.chain` in order: returns the first response whose
    /// `playabilityStatus.status == "OK"`. On a retryable refusal
    /// ([`is_retryable_refusal`]), continues to the next client. On a
    /// non-retryable refusal or any other error (HTTP / parse / locator),
    /// short-circuits with that error. If every client refuses, the last
    /// refusal is propagated with `attempted` populated so callers can see
    /// the full chain that was tried.
    async fn load_player_response(&self, locator: &str) -> Result<PlayerResponse> {
        let video_id = extract_video_id(locator)?;
        // Track two errors:
        //   * `last_err` — the most recent failure, used as a fallback
        //     when no other client produced anything more informative.
        //   * `best_refusal` — the most informative refusal seen so far
        //     (a `PlayabilityRefused`), preferred over opaque HTTP errors
        //     from later clients when we surface to the caller.
        let mut last_err: Option<TranscriptError> = None;
        let mut best_refusal: Option<TranscriptError> = None;
        let mut attempted: Vec<String> = Vec::with_capacity(self.chain.len());

        for variant in &self.chain {
            let ctx = variant.context();
            attempted.push(ctx.name.to_string());

            let attempt = self.try_one_client(&video_id, &ctx).await;
            match attempt {
                Ok(response) => {
                    if attempted.len() > 1 {
                        debug!(
                            client = ctx.name,
                            attempted = ?attempted,
                            "InnerTube client recovered playback after fallback"
                        );
                    }
                    return Ok(response);
                }
                Err(err) if is_retryable_refusal(&err) => {
                    debug!(
                        client = ctx.name,
                        error = %err,
                        "InnerTube client failed; falling through"
                    );
                    if matches!(err, TranscriptError::PlayabilityRefused { .. })
                        && best_refusal.is_none()
                    {
                        // Capture the *first* PlayabilityRefused — typically
                        // from the WEB client, which gives the most
                        // contextual reason ("Video unavailable",
                        // "Sign in to confirm…").
                        best_refusal = Some(clone_refusal(&err));
                    }
                    last_err = Some(err);
                }
                Err(err) => return Err(err),
            }
        }

        // Every client failed. Prefer the first PlayabilityRefused over
        // any HTTP error, since the refusal carries the platform's reason
        // string. Fall back to the last error otherwise. Either way,
        // attach the full chain so users can see what was tried.
        let surfaced = best_refusal.or(last_err).unwrap_or_else(|| {
            TranscriptError::ParseError("InnerTube fallback chain was empty".to_string())
        });
        Err(annotate_attempted(surfaced, attempted))
    }
}

/// Replace the `attempted` field on a `PlayabilityRefused` with the supplied
/// chain. Other variants are returned unchanged.
fn annotate_attempted(err: TranscriptError, attempted: Vec<String>) -> TranscriptError {
    match err {
        TranscriptError::PlayabilityRefused { status, reason, .. } => {
            TranscriptError::PlayabilityRefused {
                status,
                reason,
                attempted,
            }
        }
        other => other,
    }
}

/// Clone-equivalent for the `PlayabilityRefused` variant. `TranscriptError`
/// is not `Clone` because of the `reqwest::Error` variant; we only need to
/// copy the refusal so we can keep the original around alongside it.
fn clone_refusal(err: &TranscriptError) -> TranscriptError {
    match err {
        TranscriptError::PlayabilityRefused {
            status,
            reason,
            attempted,
        } => TranscriptError::PlayabilityRefused {
            status: status.clone(),
            reason: reason.clone(),
            attempted: attempted.clone(),
        },
        // Should never be called with a non-refusal error; return a
        // sentinel rather than panic.
        other => TranscriptError::ParseError(format!("clone_refusal called on {other:?}")),
    }
}

#[async_trait]
impl TranscriptSource for Youtube {
    fn name(&self) -> &'static str {
        "youtube"
    }

    fn matches(url: &str) -> bool {
        matches_url(url)
    }

    async fn fetch(&self, locator: &str, opts: &FetchOpts) -> Result<Transcript> {
        let response = self.load_player_response(locator).await?;
        let selected = select_track(&response, opts)?;
        let body = timedtext::fetch(&self.http, &selected.fetch_url).await?;
        let cues = timedtext::parse(&body)?;
        let locator_id = response
            .video_details
            .as_ref()
            .map(|d| d.video_id.clone())
            .unwrap_or_default();
        Ok(Transcript {
            source: self.name().to_string(),
            locator_id,
            language: selected.language.clone(),
            kind: selected.kind,
            cues,
        })
    }

    async fn list_languages(&self, locator: &str) -> Result<Vec<LanguageInfo>> {
        let response = self.load_player_response(locator).await?;
        Ok(list_languages(&response))
    }

    async fn info(&self, locator: &str) -> Result<MediaInfo> {
        let response = self.load_player_response(locator).await?;
        Ok(extract_media_info(&response))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! Three layers:
    //!
    //! 1. Offline acceptance gate — parse a checked-in `playerResponse`,
    //!    select the requested track, parse a checked-in json3 transcript,
    //!    render via [`format::srt`], and compare to a golden `.srt`.
    //!    Carried over from step 2.
    //! 2. HTTP-driven `TranscriptSource` impl tested against a
    //!    `wiremock::MockServer` serving both the InnerTube `/player`
    //!    endpoint and the timedtext URL the player response points at.
    //! 3. Multi-client fallback behaviour: verifies the chain advances on
    //!    `UNPLAYABLE` / `LOGIN_REQUIRED`, short-circuits on transport or
    //!    parse errors, and surfaces the full `attempted` list when every
    //!    rung refuses.
    //!
    //! [`format::srt`]: crate::transcript::format::srt

    use super::*;
    use crate::transcript::error::TranscriptError;
    use crate::transcript::format::srt;
    use crate::transcript::source::{FetchOpts, TrackKind};
    use serde_json::Value;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const PLAYER_RESPONSE: &str = include_str!("youtube/fixtures/player_response_basic.json");
    const PLAYER_RESPONSE_AGE_GATED: &str =
        include_str!("youtube/fixtures/player_response_age_gated.json");
    const PLAYER_RESPONSE_UNPLAYABLE: &str =
        include_str!("youtube/fixtures/player_response_unplayable.json");
    const TIMEDTEXT: &str = include_str!("youtube/fixtures/timedtext_basic.json");
    const EXPECTED_SRT: &str = include_str!("youtube/fixtures/expected_basic.srt");

    const VIDEO_ID: &str = "dQw4w9WgXcQ";

    // ── Offline acceptance gate (carried from step 2) ──

    #[test]
    fn matches_url_accepts_canonical_forms() {
        assert!(matches_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ"));
        assert!(matches_url("https://youtu.be/dQw4w9WgXcQ"));
    }

    #[test]
    fn matches_url_rejects_other_hosts() {
        assert!(!matches_url("https://vimeo.com/123456"));
        assert!(!matches_url("not a url"));
    }

    #[test]
    fn matches_url_accepts_bare_video_id() {
        assert!(matches_url(VIDEO_ID));
    }

    #[test]
    fn end_to_end_player_response_to_srt() {
        let response = parse_player_response(PLAYER_RESPONSE).unwrap();
        check_playability(&response).unwrap();

        let opts = FetchOpts::new("en-US");
        let selected = select_track(&response, &opts).unwrap();
        assert_eq!(selected.kind, TrackKind::Manual);
        assert_eq!(selected.language, "en-US");

        let cues = parse_timedtext(TIMEDTEXT).unwrap();
        assert_eq!(cues.len(), 3);

        let video_id = response
            .video_details
            .as_ref()
            .map(|d| d.video_id.clone())
            .unwrap_or_default();
        let transcript = Transcript {
            source: "youtube".to_string(),
            locator_id: video_id,
            language: selected.language.clone(),
            kind: selected.kind,
            cues,
        };
        let rendered = srt::render(&transcript.cues);
        assert_eq!(rendered, EXPECTED_SRT);
    }

    #[test]
    fn end_to_end_translation_path_picks_target_language() {
        let response = parse_player_response(PLAYER_RESPONSE).unwrap();
        let mut opts = FetchOpts::new("ja");
        opts.translate_to = Some("fr".into());
        let selected = select_track(&response, &opts).unwrap();
        assert_eq!(selected.kind, TrackKind::Translated);
        assert_eq!(selected.language, "fr");
        assert!(selected.fetch_url.contains("tlang=fr"));
    }

    // ── HTTP-driven TranscriptSource impl ──

    /// Take the checked-in `player_response_basic.json` fixture and rewrite
    /// every caption track's `baseUrl` to point at the mock server, so
    /// `select_track` produces a URL the same mock will answer for the
    /// timedtext GET.
    fn fixture_with_rewritten_caption_urls(mock_uri: &str) -> String {
        let mut value: Value = serde_json::from_str(PLAYER_RESPONSE).unwrap();
        let tracks = value["captions"]["playerCaptionsTracklistRenderer"]["captionTracks"]
            .as_array_mut()
            .unwrap();
        for track in tracks {
            let lang = track["languageCode"].as_str().unwrap().to_string();
            track["baseUrl"] = Value::String(format!("{mock_uri}/api/timedtext?lang={lang}"));
        }
        serde_json::to_string(&value).unwrap()
    }

    async fn mock_server_with_basic_video() -> MockServer {
        let server = MockServer::start().await;
        let player_response = fixture_with_rewritten_caption_urls(&server.uri());

        Mock::given(method("POST"))
            .and(path(innertube::PLAYER_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string(player_response))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/timedtext"))
            .respond_with(ResponseTemplate::new(200).set_body_string(TIMEDTEXT))
            .mount(&server)
            .await;

        server
    }

    /// Build a Youtube source with a single-client chain, so the test only
    /// has to mock one POST and the mock server's expected request count
    /// matches the call count exactly.
    fn yt_web_only(server: &MockServer) -> Youtube {
        Youtube::with_base_url(server.uri())
            .unwrap()
            .with_chain(vec![InnertubeClient::Web])
    }

    #[tokio::test]
    async fn fetch_returns_transcript_assembled_from_both_endpoints() {
        let server = mock_server_with_basic_video().await;
        let yt = yt_web_only(&server);
        let opts = FetchOpts::new("en-US");

        let transcript = yt
            .fetch(
                &format!("https://www.youtube.com/watch?v={VIDEO_ID}"),
                &opts,
            )
            .await
            .unwrap();

        assert_eq!(transcript.source, "youtube");
        assert_eq!(transcript.locator_id, VIDEO_ID);
        assert_eq!(transcript.language, "en-US");
        assert_eq!(transcript.kind, TrackKind::Manual);
        assert_eq!(transcript.cues.len(), 3);
        // Render and compare to the golden SRT to catch any divergence
        // between the HTTP and offline pipelines.
        assert_eq!(srt::render(&transcript.cues), EXPECTED_SRT);
    }

    #[tokio::test]
    async fn fetch_accepts_bare_video_id_as_locator() {
        let server = mock_server_with_basic_video().await;
        let yt = yt_web_only(&server);
        let opts = FetchOpts::new("en-US");

        let transcript = yt.fetch(VIDEO_ID, &opts).await.unwrap();
        assert_eq!(transcript.locator_id, VIDEO_ID);
    }

    #[tokio::test]
    async fn fetch_propagates_language_not_found() {
        let server = mock_server_with_basic_video().await;
        let yt = yt_web_only(&server);
        let opts = FetchOpts::new("zz");

        let err = yt.fetch(VIDEO_ID, &opts).await.unwrap_err();
        assert!(matches!(err, TranscriptError::LanguageNotFound { .. }));
    }

    #[tokio::test]
    async fn fetch_surfaces_age_gated_when_chain_exhausted() {
        // Every rung returns LOGIN_REQUIRED. The chain walks them all and
        // surfaces the last refusal with the full attempted list.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(innertube::PLAYER_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string(PLAYER_RESPONSE_AGE_GATED))
            .mount(&server)
            .await;

        let yt = Youtube::with_base_url(server.uri()).unwrap();
        let err = yt.fetch(VIDEO_ID, &FetchOpts::new("en")).await.unwrap_err();
        match err {
            TranscriptError::PlayabilityRefused {
                status, attempted, ..
            } => {
                assert_eq!(status, "LOGIN_REQUIRED");
                assert_eq!(attempted.len(), DEFAULT_CHAIN.len());
                assert_eq!(attempted.first().map(String::as_str), Some("WEB"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_invalid_locator_short_circuits_before_http() {
        // No mock server needed — the call should fail at URL parsing.
        let yt = Youtube::with_base_url("http://127.0.0.1:1").unwrap();
        let err = yt
            .fetch("not-a-url", &FetchOpts::new("en"))
            .await
            .unwrap_err();
        assert!(matches!(err, TranscriptError::InvalidLocator(_)));
    }

    #[tokio::test]
    async fn fetch_prefers_playability_refusal_over_later_http_errors() {
        // First two rungs return refusals; remaining rungs return 500. The
        // surfaced error should be the *first* refusal (most informative)
        // with the full attempted chain, not the trailing HTTP error.
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path(innertube::PLAYER_PATH))
            .and(body_partial_json(json_clientname("WEB")))
            .respond_with(ResponseTemplate::new(200).set_body_string(PLAYER_RESPONSE_UNPLAYABLE))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(innertube::PLAYER_PATH))
            .and(body_partial_json(json_clientname("ANDROID_VR")))
            .respond_with(ResponseTemplate::new(200).set_body_string(PLAYER_RESPONSE_AGE_GATED))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(innertube::PLAYER_PATH))
            .and(body_partial_json(json_clientname(
                "TVHTML5_SIMPLY_EMBEDDED_PLAYER",
            )))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path(innertube::PLAYER_PATH))
            .and(body_partial_json(json_clientname("IOS")))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&server)
            .await;

        let yt = Youtube::with_base_url(server.uri()).unwrap();
        let err = yt.fetch(VIDEO_ID, &FetchOpts::new("en")).await.unwrap_err();
        match err {
            TranscriptError::PlayabilityRefused {
                status, attempted, ..
            } => {
                // First refusal was UNPLAYABLE on WEB.
                assert_eq!(status, "UNPLAYABLE");
                assert_eq!(attempted.len(), DEFAULT_CHAIN.len());
                assert_eq!(attempted.first().map(String::as_str), Some("WEB"));
            }
            other => panic!("expected PlayabilityRefused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_walks_chain_on_http_errors_then_surfaces_last() {
        // Every rung gets a 500. The chain walks them all (HTTP failures
        // can be per-client request-shape rejections), then surfaces the
        // last HTTP error.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(innertube::PLAYER_PATH))
            .respond_with(ResponseTemplate::new(500))
            .expect(DEFAULT_CHAIN.len() as u64)
            .mount(&server)
            .await;

        let yt = Youtube::with_base_url(server.uri()).unwrap();
        let err = yt.fetch(VIDEO_ID, &FetchOpts::new("en")).await.unwrap_err();
        assert!(matches!(err, TranscriptError::Http(_)));
    }

    #[tokio::test]
    async fn list_languages_projects_caption_tracks() {
        let server = mock_server_with_basic_video().await;
        let yt = yt_web_only(&server);

        let langs = yt.list_languages(VIDEO_ID).await.unwrap();
        let codes: Vec<_> = langs.iter().map(|l| l.code.as_str()).collect();
        assert!(codes.contains(&"en-US"));
        assert!(codes.contains(&"es"));
        assert!(codes.contains(&"en"));
    }

    #[tokio::test]
    async fn info_returns_video_metadata() {
        let server = mock_server_with_basic_video().await;
        let yt = yt_web_only(&server);

        let info = yt.info(VIDEO_ID).await.unwrap();
        assert_eq!(info.source, "youtube");
        assert_eq!(info.locator_id, VIDEO_ID);
        assert_eq!(info.title, "Sample Video");
        assert_eq!(info.duration_ms, Some(212_000));
        assert_eq!(info.languages.len(), 3);
    }

    #[tokio::test]
    async fn matches_static_dispatch_through_trait() {
        // Object-safety / static-method routing sanity check.
        assert!(<Youtube as TranscriptSource>::matches(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        ));
        assert!(!<Youtube as TranscriptSource>::matches(
            "https://vimeo.com/1"
        ));
    }

    #[tokio::test]
    async fn name_is_lowercase_youtube() {
        let server = mock_server_with_basic_video().await;
        let yt = yt_web_only(&server);
        assert_eq!(yt.name(), "youtube");
    }

    #[test]
    fn new_constructs_default_client() {
        // Smoke test for the production constructor — exercises the
        // reqwest::Client::builder() path with the pinned timeout.
        let yt = Youtube::new().unwrap();
        assert_eq!(yt.base_url, DEFAULT_BASE_URL);
        assert_eq!(yt.chain, DEFAULT_CHAIN);
    }

    #[tokio::test]
    async fn fetch_surfaces_malformed_innertube_json_as_parse_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(innertube::PLAYER_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string("{ not json"))
            .expect(1) // parse errors short-circuit; no fallback
            .mount(&server)
            .await;

        let yt = Youtube::with_base_url(server.uri()).unwrap();
        let err = yt.fetch(VIDEO_ID, &FetchOpts::new("en")).await.unwrap_err();
        assert!(matches!(err, TranscriptError::ParseError(_)));
    }

    // ── Multi-client fallback ──

    #[tokio::test]
    async fn fetch_falls_back_to_android_vr_when_web_unplayable() {
        let server = MockServer::start().await;
        let player_response_ok = fixture_with_rewritten_caption_urls(&server.uri());

        // WEB rung: refuse with UNPLAYABLE.
        Mock::given(method("POST"))
            .and(path(innertube::PLAYER_PATH))
            .and(body_partial_json(json_clientname("WEB")))
            .respond_with(ResponseTemplate::new(200).set_body_string(PLAYER_RESPONSE_UNPLAYABLE))
            .expect(1)
            .mount(&server)
            .await;

        // ANDROID_VR rung: succeed.
        Mock::given(method("POST"))
            .and(path(innertube::PLAYER_PATH))
            .and(body_partial_json(json_clientname("ANDROID_VR")))
            .respond_with(ResponseTemplate::new(200).set_body_string(player_response_ok))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/timedtext"))
            .respond_with(ResponseTemplate::new(200).set_body_string(TIMEDTEXT))
            .mount(&server)
            .await;

        let yt = Youtube::with_base_url(server.uri())
            .unwrap()
            .with_chain(vec![InnertubeClient::Web, InnertubeClient::AndroidVr]);
        let opts = FetchOpts::new("en-US");
        let transcript = yt.fetch(VIDEO_ID, &opts).await.unwrap();

        assert_eq!(transcript.source, "youtube");
        assert_eq!(transcript.cues.len(), 3);
        assert_eq!(srt::render(&transcript.cues), EXPECTED_SRT);
    }

    #[tokio::test]
    async fn fetch_chain_exhausted_surfaces_last_refusal_with_attempted() {
        let server = MockServer::start().await;
        // Every POST returns UNPLAYABLE regardless of which client asked.
        Mock::given(method("POST"))
            .and(path(innertube::PLAYER_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string(PLAYER_RESPONSE_UNPLAYABLE))
            .expect(DEFAULT_CHAIN.len() as u64)
            .mount(&server)
            .await;

        let yt = Youtube::with_base_url(server.uri()).unwrap();
        let err = yt.fetch(VIDEO_ID, &FetchOpts::new("en")).await.unwrap_err();
        match err {
            TranscriptError::PlayabilityRefused {
                status,
                attempted,
                reason,
            } => {
                assert_eq!(status, "UNPLAYABLE");
                assert_eq!(reason.as_deref(), Some("Video unavailable"));
                let want: Vec<String> = DEFAULT_CHAIN
                    .iter()
                    .map(|c| c.context().name.to_string())
                    .collect();
                assert_eq!(attempted, want);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_falls_back_on_error_status() {
        // `ERROR` is retryable — YouTube uses it for both deleted videos
        // *and* "client no longer supported" rejections, so the chain
        // walks every rung before surfacing a refusal.
        let server = MockServer::start().await;
        let response = serde_json::json!({
            "playabilityStatus": { "status": "ERROR", "reason": "Video unavailable" }
        })
        .to_string();
        Mock::given(method("POST"))
            .and(path(innertube::PLAYER_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string(response))
            .expect(DEFAULT_CHAIN.len() as u64)
            .mount(&server)
            .await;

        let yt = Youtube::with_base_url(server.uri()).unwrap();
        let err = yt.fetch(VIDEO_ID, &FetchOpts::new("en")).await.unwrap_err();
        match err {
            TranscriptError::PlayabilityRefused {
                status, attempted, ..
            } => {
                assert_eq!(status, "ERROR");
                assert_eq!(attempted.len(), DEFAULT_CHAIN.len());
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_short_circuits_on_live_stream_offline() {
        // Live streams that haven't started will never have captions, so
        // we don't waste requests fanning out.
        let server = MockServer::start().await;
        let response = serde_json::json!({
            "playabilityStatus": {
                "status": "LIVE_STREAM_OFFLINE",
                "reason": "Premieres in 1 hour"
            }
        })
        .to_string();
        Mock::given(method("POST"))
            .and(path(innertube::PLAYER_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string(response))
            .expect(1)
            .mount(&server)
            .await;

        let yt = Youtube::with_base_url(server.uri()).unwrap();
        let err = yt.fetch(VIDEO_ID, &FetchOpts::new("en")).await.unwrap_err();
        match err {
            TranscriptError::PlayabilityRefused { status, .. } => {
                assert_eq!(status, "LIVE_STREAM_OFFLINE");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    fn json_clientname(name: &str) -> serde_json::Value {
        serde_json::json!({
            "context": { "client": { "clientName": name } }
        })
    }

    #[test]
    fn is_retryable_refusal_matches_expected_statuses() {
        let mk = |s: &str| TranscriptError::PlayabilityRefused {
            status: s.to_string(),
            reason: None,
            attempted: Vec::new(),
        };
        // All ambiguous refusals retry: any one of these may be a
        // client-specific quirk that another rung will resolve.
        assert!(is_retryable_refusal(&mk("UNPLAYABLE")));
        assert!(is_retryable_refusal(&mk("LOGIN_REQUIRED")));
        assert!(is_retryable_refusal(&mk("AGE_VERIFICATION_REQUIRED")));
        assert!(is_retryable_refusal(&mk("CONTENT_CHECK_REQUIRED")));
        assert!(is_retryable_refusal(&mk("ERROR")));
        // Live streams that haven't started will never have captions on
        // any client.
        assert!(!is_retryable_refusal(&mk("LIVE_STREAM_OFFLINE")));
        // Parse / locator errors short-circuit — same input yields the
        // same parse failure on every client.
        assert!(!is_retryable_refusal(&TranscriptError::ParseError(
            "x".into()
        )));
        assert!(!is_retryable_refusal(&TranscriptError::InvalidLocator(
            "x".into()
        )));
    }

    // ── Online integration test ──
    //
    // Hits real YouTube — gated behind the `online_tests` custom cfg
    // (declared in `Cargo.toml`'s `[lints.rust]`), *not* a cargo feature,
    // so `cargo test --all-features` does not compile or run it. CI never
    // sets the cfg; run manually with
    // `RUSTFLAGS='--cfg online_tests' cargo test online_fetch_against_public_video`.
    // Note that YouTube blocks well-known cloud / CI IPs with
    // `LOGIN_REQUIRED`, so this test passes only from a residential
    // network — it is intentionally manual-only.
    #[cfg(online_tests)]
    #[tokio::test]
    async fn online_fetch_against_public_video() {
        // "Me at the zoo" — the first YouTube video, captioned, stable.
        const STABLE_VIDEO_ID: &str = "jNQXAC9IVRw";
        let yt = Youtube::new().unwrap();
        let opts = FetchOpts::new("en");
        let transcript = yt.fetch(STABLE_VIDEO_ID, &opts).await.unwrap();
        assert_eq!(transcript.source, "youtube");
        assert_eq!(transcript.locator_id, STABLE_VIDEO_ID);
        assert!(!transcript.cues.is_empty());
    }
}
