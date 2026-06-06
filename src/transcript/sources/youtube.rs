//! YouTube [`TranscriptSource`].
//!
//! Wires the offline parsers ([`url`], [`player_response`], [`timedtext`])
//! into a concrete [`TranscriptSource`] backed by an HTTP client. The
//! request shape is pinned to the `ANDROID_VR` InnerTube client (see
//! [`innertube`]); a `visitorData` token is scraped from the watch page
//! on first use ([`watch_page`]) and cached for the lifetime of the
//! [`Youtube`] instance.

use std::time::Duration;

use async_trait::async_trait;

use crate::transcript::error::Result;
use crate::transcript::source::{FetchOpts, LanguageInfo, MediaInfo, Transcript, TranscriptSource};

pub mod channel;
pub mod innertube;
pub mod player_response;
pub mod timedtext;
pub mod url;
pub mod watch_page;

pub use channel::VideoEntry;

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

/// User-Agent advertised to YouTube on InnerTube `/player` calls. Must
/// match the `clientName` / `clientVersion` constants in [`innertube`] —
/// YouTube cross-checks UA against `clientName` as part of bot detection
/// and a mismatch is one of the flagged signals. The trailing `gzip`
/// token isn't decorative; that's what the real Quest YouTube app emits.
///
/// The watch-page bootstrap in [`watch_page`] uses a separate
/// browser-shaped UA — it scrapes a public HTML page, not InnerTube.
const USER_AGENT: &str = "com.google.android.apps.youtube.vr.oculus/1.62.27 \
     (Linux; U; Android 12; Quest 3) gzip";

/// Whether `input` is recognised as a YouTube locator (URL or bare ID).
///
/// Used by the future `omni-dev transcript fetch <url>` auto-detection
/// path and by [`TranscriptSource::matches`].
pub fn matches_url(input: &str) -> bool {
    extract_video_id(input).is_ok()
}

/// YouTube [`TranscriptSource`].
///
/// Holds a single [`reqwest::Client`] reused across the watch-page,
/// InnerTube, and timedtext calls. Cheap to construct; in steady state
/// it is fine to keep one instance per process.
///
/// On first use, a `visitorData` token is scraped from the watch page
/// and cached in [`tokio::sync::OnceCell`]. Concurrent first-callers
/// serialise on a single fetch rather than double-fetching, and every
/// subsequent InnerTube `/player` POST forwards the cached token.
#[derive(Debug, Clone)]
pub struct Youtube {
    http: reqwest::Client,
    base_url: String,
    visitor_data: tokio::sync::OnceCell<String>,
}

impl Youtube {
    /// Construct a YouTube source with default HTTP settings (30 s timeout,
    /// ANDROID_VR User-Agent) targeting the public YouTube origin.
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self {
            http,
            base_url: DEFAULT_BASE_URL.to_string(),
            visitor_data: tokio::sync::OnceCell::new(),
        })
    }

    /// Construct a YouTube source pointed at an alternate origin. Used by
    /// tests to inject a `wiremock::MockServer::uri()`. The HTTP client
    /// retains the production timeout and User-Agent so request shape
    /// matches the real client.
    pub fn with_base_url(base_url: impl Into<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.into(),
            visitor_data: tokio::sync::OnceCell::new(),
        })
    }

    /// Cached `visitorData` token. First call scrapes the watch page;
    /// concurrent first-callers serialise on a single in-flight scrape
    /// (`OnceCell::get_or_try_init`) rather than double-fetching.
    async fn visitor_data(&self) -> Result<&str> {
        self.visitor_data
            .get_or_try_init(|| watch_page::fetch_visitor_data(&self.http, &self.base_url))
            .await
            .map(String::as_str)
    }

    /// Common preamble: locator → video ID → watch-page bootstrap →
    /// InnerTube POST → `playerResponse` parse → playability check.
    ///
    /// `extract_video_id` runs first so an invalid locator short-circuits
    /// before any HTTP — lazy `visitor_data` fetch only happens on a
    /// validated locator.
    async fn load_player_response(&self, locator: &str) -> Result<PlayerResponse> {
        let video_id = extract_video_id(locator)?;
        let visitor_data = self.visitor_data().await?;
        let raw =
            innertube::fetch_player_response(&self.http, &self.base_url, &video_id, visitor_data)
                .await?;
        let response = parse_player_response(&raw)?;
        check_playability(&response)?;
        Ok(response)
    }

    /// Resolve a channel locator (`@handle`, `/c/Name`, channel URL, or a raw
    /// `UC…` ID) to its canonical `UC…` channel ID. See
    /// [`channel::resolve_channel_id`].
    pub async fn resolve_channel_id(&self, input: &str) -> Result<String> {
        channel::resolve_channel_id(&self.http, &self.base_url, input).await
    }

    /// Enumerate a channel's recent uploads via its RSS feed (newest-first,
    /// ~15 most recent). See [`channel::fetch_recent_videos`].
    pub async fn recent_channel_videos(&self, channel_id: &str) -> Result<Vec<VideoEntry>> {
        channel::fetch_recent_videos(&self.http, &self.base_url, channel_id).await
    }

    /// Enumerate a channel's full upload history via the InnerTube `/browse`
    /// endpoint (newest-first). Uses the WEB client; no `visitorData` bootstrap
    /// is needed (browse is a public endpoint). See
    /// [`channel::fetch_all_video_ids`].
    pub async fn all_channel_video_ids(&self, channel_id: &str) -> Result<Vec<String>> {
        channel::fetch_all_video_ids(&self.http, &self.base_url, channel_id).await
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
    //! Two layers:
    //!
    //! 1. Offline acceptance gate — parse a checked-in `playerResponse`,
    //!    select the requested track, parse a checked-in json3 transcript,
    //!    render via [`format::srt`], and compare to a golden `.srt`.
    //!    Carried over from step 2.
    //! 2. HTTP-driven `TranscriptSource` impl tested against a
    //!    `wiremock::MockServer` serving both the InnerTube `/player`
    //!    endpoint and the timedtext URL the player response points at.
    //!
    //! [`format::srt`]: crate::transcript::format::srt

    use super::*;
    use crate::transcript::error::TranscriptError;
    use crate::transcript::format::srt;
    use crate::transcript::source::{FetchOpts, TrackKind};
    use serde_json::Value;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const PLAYER_RESPONSE: &str = include_str!("youtube/fixtures/player_response_basic.json");
    const PLAYER_RESPONSE_AGE_GATED: &str =
        include_str!("youtube/fixtures/player_response_age_gated.json");
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

    /// Watch-page fixture used to satisfy the `visitorData` bootstrap in
    /// every wiremock-driven test below. The exact token value doesn't
    /// matter for these tests — only that the bootstrap returns *some*
    /// token so [`load_player_response`] can proceed.
    const WATCH_PAGE: &str = include_str!("youtube/fixtures/watch_page_with_visitor_data.html");

    /// Mount the watch-page bootstrap mock onto `server`. Every
    /// [`Youtube::fetch`] / `list_languages` / `info` call in the tests
    /// below triggers this on first use (cached thereafter via
    /// `OnceCell`).
    async fn mount_watch_page(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/watch"))
            .respond_with(ResponseTemplate::new(200).set_body_string(WATCH_PAGE))
            .mount(server)
            .await;
    }

    async fn mock_server_with_basic_video() -> MockServer {
        let server = MockServer::start().await;
        let player_response = fixture_with_rewritten_caption_urls(&server.uri());

        mount_watch_page(&server).await;

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

    #[tokio::test]
    async fn fetch_returns_transcript_assembled_from_both_endpoints() {
        let server = mock_server_with_basic_video().await;
        let yt = Youtube::with_base_url(server.uri()).unwrap();
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
        let yt = Youtube::with_base_url(server.uri()).unwrap();
        let opts = FetchOpts::new("en-US");

        let transcript = yt.fetch(VIDEO_ID, &opts).await.unwrap();
        assert_eq!(transcript.locator_id, VIDEO_ID);
    }

    #[tokio::test]
    async fn fetch_propagates_language_not_found() {
        let server = mock_server_with_basic_video().await;
        let yt = Youtube::with_base_url(server.uri()).unwrap();
        let opts = FetchOpts::new("zz");

        let err = yt.fetch(VIDEO_ID, &opts).await.unwrap_err();
        assert!(matches!(err, TranscriptError::LanguageNotFound { .. }));
    }

    #[tokio::test]
    async fn fetch_surfaces_age_gated_as_playability_refused() {
        let server = MockServer::start().await;
        mount_watch_page(&server).await;
        Mock::given(method("POST"))
            .and(path(innertube::PLAYER_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string(PLAYER_RESPONSE_AGE_GATED))
            .mount(&server)
            .await;

        let yt = Youtube::with_base_url(server.uri()).unwrap();
        let err = yt.fetch(VIDEO_ID, &FetchOpts::new("en")).await.unwrap_err();
        match err {
            TranscriptError::PlayabilityRefused { status, .. } => {
                assert_eq!(status, "LOGIN_REQUIRED");
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
    async fn fetch_surfaces_innertube_500_as_http_error() {
        let server = MockServer::start().await;
        mount_watch_page(&server).await;
        Mock::given(method("POST"))
            .and(path(innertube::PLAYER_PATH))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let yt = Youtube::with_base_url(server.uri()).unwrap();
        let err = yt.fetch(VIDEO_ID, &FetchOpts::new("en")).await.unwrap_err();
        assert!(matches!(err, TranscriptError::Http(_)));
    }

    #[tokio::test]
    async fn list_languages_projects_caption_tracks() {
        let server = mock_server_with_basic_video().await;
        let yt = Youtube::with_base_url(server.uri()).unwrap();

        let langs = yt.list_languages(VIDEO_ID).await.unwrap();
        let codes: Vec<_> = langs.iter().map(|l| l.code.as_str()).collect();
        assert!(codes.contains(&"en-US"));
        assert!(codes.contains(&"es"));
        assert!(codes.contains(&"en"));
    }

    #[tokio::test]
    async fn info_returns_video_metadata() {
        let server = mock_server_with_basic_video().await;
        let yt = Youtube::with_base_url(server.uri()).unwrap();

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
        let yt = Youtube::with_base_url(server.uri()).unwrap();
        assert_eq!(yt.name(), "youtube");
    }

    #[test]
    fn new_constructs_default_client() {
        // Smoke test for the production constructor — exercises the
        // reqwest::Client::builder() path with the pinned timeout / UA.
        let yt = Youtube::new().unwrap();
        assert_eq!(yt.base_url, DEFAULT_BASE_URL);
    }

    #[tokio::test]
    async fn fetch_threads_visitor_data_into_innertube_body() {
        // Pin the bootstrap → InnerTube wiring: the token scraped from the
        // watch page must appear under `context.client.visitorData` on the
        // outbound /player POST. Captures the inbound JSON and asserts on
        // the value the fixture publishes.
        const EXPECTED_TOKEN: &str = "CgtkUTQyOFR3aV9NSSjFoYvBBjIKCgJVUxIEGgAgPg%3D%3D";

        let server = MockServer::start().await;
        mount_watch_page(&server).await;
        let player_response = fixture_with_rewritten_caption_urls(&server.uri());

        Mock::given(method("POST"))
            .and(path(innertube::PLAYER_PATH))
            .respond_with(move |req: &wiremock::Request| {
                let parsed: Value = serde_json::from_slice(&req.body).unwrap();
                assert_eq!(
                    parsed["context"]["client"]["visitorData"],
                    Value::String(EXPECTED_TOKEN.to_string()),
                );
                ResponseTemplate::new(200).set_body_string(player_response.clone())
            })
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/api/timedtext"))
            .respond_with(ResponseTemplate::new(200).set_body_string(TIMEDTEXT))
            .mount(&server)
            .await;

        let yt = Youtube::with_base_url(server.uri()).unwrap();
        let _ = yt.fetch(VIDEO_ID, &FetchOpts::new("en-US")).await.unwrap();
    }

    #[tokio::test]
    async fn visitor_data_fetched_only_once_for_repeated_calls() {
        // Sequential calls via a single `Youtube` instance must hit the
        // watch page exactly once — `OnceCell` caches across calls.
        let server = MockServer::start().await;
        let player_response = fixture_with_rewritten_caption_urls(&server.uri());

        Mock::given(method("GET"))
            .and(path("/watch"))
            .respond_with(ResponseTemplate::new(200).set_body_string(WATCH_PAGE))
            .expect(1)
            .mount(&server)
            .await;

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

        let yt = Youtube::with_base_url(server.uri()).unwrap();
        let _ = yt.fetch(VIDEO_ID, &FetchOpts::new("en-US")).await.unwrap();
        let _ = yt.fetch(VIDEO_ID, &FetchOpts::new("en-US")).await.unwrap();
        // wiremock asserts expect(1) on server drop.
    }

    #[tokio::test]
    async fn visitor_data_fetched_only_once_under_concurrency() {
        // Concurrent first-callers must serialise on a single in-flight
        // scrape rather than each issuing their own watch-page GET.
        // `tokio::sync::OnceCell::get_or_try_init` is documented to
        // provide this guarantee; this test pins the contract.
        let server = MockServer::start().await;
        let player_response = fixture_with_rewritten_caption_urls(&server.uri());

        Mock::given(method("GET"))
            .and(path("/watch"))
            .respond_with(ResponseTemplate::new(200).set_body_string(WATCH_PAGE))
            .expect(1)
            .mount(&server)
            .await;

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

        let yt = Youtube::with_base_url(server.uri()).unwrap();
        let opts = FetchOpts::new("en-US");
        let (a, b, c) = tokio::join!(
            yt.fetch(VIDEO_ID, &opts),
            yt.fetch(VIDEO_ID, &opts),
            yt.fetch(VIDEO_ID, &opts),
        );
        a.unwrap();
        b.unwrap();
        c.unwrap();
        // wiremock asserts expect(1) on server drop.
    }

    #[tokio::test]
    async fn fetch_surfaces_missing_visitor_data_as_typed_error() {
        // Watch-page format has drifted (no visitorData token): the fetch
        // must propagate `MissingVisitorData` rather than fall through to
        // an unauthenticated /player call.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/watch"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("<html><body>no token</body></html>"),
            )
            .mount(&server)
            .await;

        let yt = Youtube::with_base_url(server.uri()).unwrap();
        let err = yt.fetch(VIDEO_ID, &FetchOpts::new("en")).await.unwrap_err();
        assert!(matches!(err, TranscriptError::MissingVisitorData { .. }));
    }

    #[tokio::test]
    async fn fetch_surfaces_malformed_innertube_json_as_parse_error() {
        let server = MockServer::start().await;
        mount_watch_page(&server).await;
        Mock::given(method("POST"))
            .and(path(innertube::PLAYER_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string("{ not json"))
            .mount(&server)
            .await;

        let yt = Youtube::with_base_url(server.uri()).unwrap();
        let err = yt.fetch(VIDEO_ID, &FetchOpts::new("en")).await.unwrap_err();
        assert!(matches!(err, TranscriptError::ParseError(_)));
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
