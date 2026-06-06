//! Channel video enumeration — the one capability the per-video fetcher
//! lacks. Two no-auth strategies, both reusing the existing HTTP plumbing:
//!
//! - **RSS** ([`fetch_recent_videos`]): GET `feeds/videos.xml?channel_id=…`,
//!   regex out the ~15 newest `<yt:videoId>`s plus their `<published>` dates.
//!   Trivial and robust; ideal for incremental "keep up to date" syncs.
//! - **InnerTube `/browse`** ([`fetch_all_video_ids`]): page through the
//!   channel's *Videos* tab via continuation tokens for the full upload
//!   history. Reuses [`super::innertube::fetch_browse`] (same host, same
//!   client context as `/player`). This depends on YouTube's internal JSON
//!   shape — the same accepted fragility the `/player` parsing already lives
//!   with.
//!
//! Both return newest-first, which lets a sync stop paging once it hits a
//! contiguous run of already-synced IDs.
//!
//! Channel locators (`@handle`, `/c/Name`, `/user/Name`, channel URLs) are
//! resolved to a canonical `UC…` ID by [`resolve_channel_id`] — a raw `UC…`
//! and `/channel/UC…` URLs short-circuit without any HTTP; everything else is
//! scraped from the channel page (same watch-page-bootstrap pattern as
//! [`super::watch_page`]).

use std::collections::HashSet;
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{json, Value};
use url::Url;

use crate::transcript::error::{Result, TranscriptError};

use super::innertube::{fetch_browse, web_client_context};
use super::watch_page::BROWSER_USER_AGENT;

/// Length of a `UC…` channel ID (the `UC` prefix plus 22 base64-ish chars).
const CHANNEL_ID_LEN: usize = 24;

/// Opaque, version-stable `params` selecting a channel's *Videos* tab on the
/// InnerTube `/browse` endpoint. A base64-encoded protobuf; treated as a
/// constant token (the same way the `/player` request shape is pinned).
const VIDEOS_TAB_PARAMS: &str = "EgZ2aWRlb3PyBgQKAjoA";

/// A single video enumerated from a channel, newest-first.
///
/// `published` is present for the RSS path (which carries `<published>` per
/// entry) and absent for the browse path (the upload grid has no per-item
/// dates).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoEntry {
    /// 11-character YouTube video ID.
    pub id: String,
    /// Publish timestamp, when the enumeration source provides one.
    pub published: Option<DateTime<Utc>>,
}

/// Resolve a channel locator to its canonical `UC…` ID.
///
/// Short-circuits (no HTTP) for a bare `UC…` ID and for `/channel/UC…` URLs.
/// For `@handle`, `/c/Name`, `/user/Name`, a bare handle/name, or any other
/// channel URL, GETs the page with a browser UA and scrapes the `channelId`
/// token out of the HTML. Returns [`TranscriptError::ChannelNotFound`] when
/// the input is unusable or the token is absent.
pub async fn resolve_channel_id(
    http: &reqwest::Client,
    base_url: &str,
    input: &str,
) -> Result<String> {
    if is_channel_id(input) {
        return Ok(input.to_string());
    }

    // Decide which page to scrape. A `/channel/UC…` URL is resolved inline.
    let page_url = match Url::parse(input) {
        Ok(url) => {
            if let Some(id) = channel_id_from_path(url.path()) {
                return Ok(id);
            }
            input.to_string()
        }
        Err(_) => format!(
            "{base}/{path}",
            base = base_url.trim_end_matches('/'),
            path = input.trim_start_matches('/'),
        ),
    };

    let body = http
        .get(&page_url)
        .header(reqwest::header::USER_AGENT, BROWSER_USER_AGENT)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    extract_channel_id(&body)
        .map(str::to_string)
        .ok_or_else(|| TranscriptError::ChannelNotFound {
            input: input.to_string(),
        })
}

/// RSS enumeration: the ~15 most recent uploads, newest-first. One GET, no
/// continuation logic. Ideal for incremental syncs.
pub async fn fetch_recent_videos(
    http: &reqwest::Client,
    base_url: &str,
    channel_id: &str,
) -> Result<Vec<VideoEntry>> {
    let url = format!(
        "{base}/feeds/videos.xml?channel_id={id}",
        base = base_url.trim_end_matches('/'),
        id = channel_id,
    );
    let body = http
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    Ok(parse_rss(&body))
}

/// Browse enumeration: the full upload history, newest-first.
///
/// Pages the *Videos* tab through continuation tokens using the WEB client
/// (see [`super::innertube::fetch_browse`]). No `visitorData` is needed —
/// browse is a public, non-bot-gated endpoint.
pub async fn fetch_all_video_ids(
    http: &reqwest::Client,
    base_url: &str,
    channel_id: &str,
) -> Result<Vec<String>> {
    let mut ids: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut seen_tokens: HashSet<String> = HashSet::new();

    // First page keys off `browseId` + the Videos-tab `params`; subsequent
    // pages key off the continuation token returned by the previous page.
    let mut body = json!({
        "context": web_client_context(),
        "browseId": channel_id,
        "params": VIDEOS_TAB_PARAMS,
    });

    loop {
        let raw = fetch_browse(http, base_url, &body).await?;
        let value: Value = serde_json::from_str(&raw).map_err(|e| {
            TranscriptError::ParseError(format!("browse response was not valid JSON: {e}"))
        })?;
        let (page_ids, token) = parse_browse_page(&value);

        let mut added_any = false;
        for id in page_ids {
            if seen.insert(id.clone()) {
                ids.push(id);
                added_any = true;
            }
        }

        // Stop when the page advances nothing new or the token loops/ends —
        // each guard alone prevents a runaway pagination loop.
        match token {
            Some(t) if added_any && seen_tokens.insert(t.clone()) => {
                body = json!({
                    "context": web_client_context(),
                    "continuation": t,
                });
            }
            _ => break,
        }
    }

    Ok(ids)
}

/// Whether `s` is a bare canonical channel ID (`UC` + 22 ID chars).
fn is_channel_id(s: &str) -> bool {
    s.len() == CHANNEL_ID_LEN
        && s.starts_with("UC")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Pull a `UC…` ID out of a URL path like `/channel/UC…/videos`.
fn channel_id_from_path(path: &str) -> Option<String> {
    let rest = path.trim_start_matches('/').strip_prefix("channel/")?;
    let candidate = rest.split('/').next().unwrap_or(rest);
    is_channel_id(candidate).then(|| candidate.to_string())
}

/// Lazy-compiled `channelId` / `externalId` extractor. The token is a JSON
/// string value in an inline script block; anchored on the exact keys so it
/// doesn't match neighbouring `*Id` fields. Same regex-over-HTML approach as
/// [`super::watch_page`]'s `visitorData` scrape.
fn channel_id_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    #[allow(clippy::expect_used)]
    RE.get_or_init(|| {
        Regex::new(r#""(?:channelId|externalId)":"(UC[0-9A-Za-z_-]{22})""#)
            .expect("channel_id regex must compile")
    })
}

/// Pure helper: pull the `UC…` channel ID out of a channel-page body.
fn extract_channel_id(body: &str) -> Option<&str> {
    channel_id_regex()
        .captures(body)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str())
}

/// Lazy-compiled extractor for a single RSS `<entry>` block's video ID.
fn rss_video_id_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    #[allow(clippy::expect_used)]
    RE.get_or_init(|| {
        Regex::new(r"<yt:videoId>([0-9A-Za-z_-]{11})</yt:videoId>")
            .expect("rss video id regex must compile")
    })
}

/// Lazy-compiled extractor for a single RSS `<entry>` block's publish date.
fn rss_published_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    #[allow(clippy::expect_used)]
    RE.get_or_init(|| {
        Regex::new(r"<published>([^<]+)</published>").expect("rss published regex must compile")
    })
}

/// Lazy-compiled splitter for RSS `<entry>…</entry>` blocks.
fn rss_entry_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    #[allow(clippy::expect_used)]
    RE.get_or_init(|| {
        Regex::new(r"(?s)<entry>(.*?)</entry>").expect("rss entry regex must compile")
    })
}

/// Pure helper: parse a YouTube channel RSS feed into newest-first entries.
/// Entries without a parseable video ID are skipped; an unparseable
/// `<published>` degrades to `None` rather than dropping the entry.
fn parse_rss(xml: &str) -> Vec<VideoEntry> {
    rss_entry_regex()
        .captures_iter(xml)
        .filter_map(|entry| {
            let block = entry.get(1)?.as_str();
            let id = rss_video_id_regex()
                .captures(block)
                .and_then(|c| c.get(1))?
                .as_str()
                .to_string();
            let published = rss_published_regex()
                .captures(block)
                .and_then(|c| c.get(1))
                .and_then(|m| DateTime::parse_from_rfc3339(m.as_str()).ok())
                .map(|dt| dt.with_timezone(&Utc));
            Some(VideoEntry { id, published })
        })
        .collect()
}

/// Pure helper: extract `(video_ids, next_continuation_token)` from one
/// `/browse` response page.
///
/// IDs are collected by a tolerant tree-walk in document order (newest-first
/// for the Videos tab). Two item shapes are recognised:
///
/// - `lockupViewModel` with `contentType == "LOCKUP_CONTENT_TYPE_VIDEO"` →
///   `contentId` (the current WEB grid shape).
/// - `videoRenderer.videoId` (the legacy shape, still emitted on some tabs).
///
/// The continuation token is taken **only** from the array that also holds the
/// video items — the grid's "load more". A response carries other
/// `continuationItemRenderer`s too (e.g. one in the channel header) that page
/// unrelated content; scoping to the video array avoids paging the wrong list
/// (which would stop enumeration after the first ~30 uploads).
///
/// The `contentType` guard avoids playlist / channel lockups, and keying only
/// off these wrappers avoids the duplicate IDs that appear under
/// `watchEndpoint` / `addToPlaylist` link targets.
fn parse_browse_page(value: &Value) -> (Vec<String>, Option<String>) {
    let mut ids = Vec::new();
    let mut seen = HashSet::new();
    collect_video_ids(value, &mut ids, &mut seen);
    let token = find_grid_continuation(value);
    (ids, token)
}

fn collect_video_ids(value: &Value, ids: &mut Vec<String>, seen: &mut HashSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(id) = video_id_from_item(map) {
                if seen.insert(id.to_string()) {
                    ids.push(id.to_string());
                }
            }
            for child in map.values() {
                collect_video_ids(child, ids, seen);
            }
        }
        Value::Array(arr) => {
            for child in arr {
                collect_video_ids(child, ids, seen);
            }
        }
        _ => {}
    }
}

/// Find the continuation token belonging to the *video grid*: the
/// `continuationItemRenderer` token that sits in the same array as the video
/// items. Returns `None` once the grid has no further pages.
fn find_grid_continuation(value: &Value) -> Option<String> {
    match value {
        Value::Array(arr) => {
            // The video items in this array are wrapped (e.g. in
            // `richItemRenderer`), so test the subtree, not the item directly.
            let has_video = arr.iter().any(contains_video);
            if has_video {
                if let Some(token) = arr
                    .iter()
                    .filter_map(Value::as_object)
                    .find_map(continuation_token_from_item)
                {
                    return Some(token);
                }
            }
            arr.iter().find_map(find_grid_continuation)
        }
        Value::Object(map) => map.values().find_map(find_grid_continuation),
        _ => None,
    }
}

/// Whether `value`'s subtree contains at least one video item.
fn contains_video(value: &Value) -> bool {
    match value {
        Value::Object(map) => video_id_from_item(map).is_some() || map.values().any(contains_video),
        Value::Array(arr) => arr.iter().any(contains_video),
        _ => false,
    }
}

/// Pull a video ID out of a single item renderer if `map` is one. Recognises
/// the current `lockupViewModel` shape and the legacy `videoRenderer` shape;
/// returns `None` for any other object.
fn video_id_from_item(map: &serde_json::Map<String, Value>) -> Option<&str> {
    if let Some(lockup) = map.get("lockupViewModel") {
        if lockup.get("contentType").and_then(Value::as_str) == Some("LOCKUP_CONTENT_TYPE_VIDEO") {
            return lockup.get("contentId").and_then(Value::as_str);
        }
    }
    map.get("videoRenderer")
        .and_then(|vr| vr.get("videoId"))
        .and_then(Value::as_str)
}

/// Pull a continuation token out of a `continuationItemRenderer` item.
fn continuation_token_from_item(map: &serde_json::Map<String, Value>) -> Option<String> {
    map.get("continuationItemRenderer")?
        .get("continuationEndpoint")?
        .get("continuationCommand")?
        .get("token")?
        .as_str()
        .map(str::to_string)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const CHANNEL_PAGE: &str = include_str!("fixtures/channel_page.html");
    const RSS_FEED: &str = include_str!("fixtures/channel_rss.xml");
    const BROWSE_PAGE1: &str = include_str!("fixtures/browse_videos_page1.json");
    const BROWSE_PAGE2: &str = include_str!("fixtures/browse_videos_page2.json");

    const CHANNEL_ID: &str = "UC_x5XG1OV2P6uZZ5FSM9Ttw";

    fn http() -> reqwest::Client {
        reqwest::Client::builder().build().unwrap()
    }

    // ── pure helpers ──

    #[test]
    fn is_channel_id_accepts_canonical() {
        assert!(is_channel_id(CHANNEL_ID));
        assert!(!is_channel_id("UCshort"));
        assert!(!is_channel_id("AB_x5XG1OV2P6uZZ5FSM9Ttw")); // wrong prefix
    }

    #[test]
    fn channel_id_from_path_extracts_uc() {
        assert_eq!(
            channel_id_from_path(&format!("/channel/{CHANNEL_ID}/videos")).as_deref(),
            Some(CHANNEL_ID)
        );
        assert_eq!(channel_id_from_path("/@handle"), None);
    }

    #[test]
    fn extract_channel_id_from_fixture() {
        assert_eq!(extract_channel_id(CHANNEL_PAGE), Some(CHANNEL_ID));
    }

    #[test]
    fn extract_channel_id_ignores_other_id_keys() {
        let body = r#"{"clientId":"x","sessionId":"y"}"#;
        assert_eq!(extract_channel_id(body), None);
    }

    #[test]
    fn parse_rss_returns_entries_newest_first_with_dates() {
        let entries = parse_rss(RSS_FEED);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].id, "aaaaaaaaaaa");
        assert_eq!(entries[2].id, "ccccccccccc");
        assert!(entries[0].published.unwrap() > entries[2].published.unwrap());
    }

    #[test]
    fn parse_browse_page1_yields_ids_and_token() {
        let value: Value = serde_json::from_str(BROWSE_PAGE1).unwrap();
        let (ids, token) = parse_browse_page(&value);
        // The playlist lockup is filtered out by the contentType guard.
        assert_eq!(ids, vec!["vid00000001", "vid00000002"]);
        assert_eq!(token.as_deref(), Some("CONT_TOKEN_1"));
    }

    #[test]
    fn parse_browse_page_accepts_legacy_video_renderer() {
        // Older shape: `videoRenderer.videoId` rather than `lockupViewModel`.
        let value = json!({
            "contents": {
                "richGridRenderer": {
                    "contents": [
                        { "richItemRenderer": { "content": {
                            "videoRenderer": { "videoId": "legacyvid01" } } } },
                        { "continuationItemRenderer": { "continuationEndpoint": {
                            "continuationCommand": { "token": "LEGACY_TOK" } } } }
                    ]
                }
            }
        });
        let (ids, token) = parse_browse_page(&value);
        assert_eq!(ids, vec!["legacyvid01"]);
        assert_eq!(token.as_deref(), Some("LEGACY_TOK"));
    }

    #[test]
    fn find_grid_continuation_recurses_past_video_less_arrays() {
        // Arrays without any video → `contains_video` false → recurse → None.
        let empty = json!({ "a": [{ "x": 1 }], "b": { "c": [] } });
        assert_eq!(find_grid_continuation(&empty), None);

        // Outer array's items are wrappers (no direct continuation); the grid
        // token sits deeper, forcing the recursion fall-through branch.
        let nested = json!({
            "outer": [
                { "wrap": { "richGridRenderer": { "contents": [
                    { "richItemRenderer": { "content": { "lockupViewModel": {
                        "contentId": "vidxxxxxxx1",
                        "contentType": "LOCKUP_CONTENT_TYPE_VIDEO" } } } },
                    { "continuationItemRenderer": { "continuationEndpoint": {
                        "continuationCommand": { "token": "DEEP_TOK" } } } }
                ] } } }
            ]
        });
        assert_eq!(find_grid_continuation(&nested).as_deref(), Some("DEEP_TOK"));
    }

    #[test]
    fn parse_browse_page2_is_final() {
        let value: Value = serde_json::from_str(BROWSE_PAGE2).unwrap();
        let (ids, token) = parse_browse_page(&value);
        assert_eq!(ids, vec!["vid00000003"]);
        assert_eq!(token, None);
    }

    // ── HTTP layer ──

    #[tokio::test]
    async fn resolve_channel_id_passthrough_skips_http() {
        // No mock server: a bare UC id must not trigger any request.
        let id = resolve_channel_id(&http(), "http://127.0.0.1:1", CHANNEL_ID)
            .await
            .unwrap();
        assert_eq!(id, CHANNEL_ID);
    }

    #[tokio::test]
    async fn resolve_channel_id_from_channel_url_skips_http() {
        let id = resolve_channel_id(
            &http(),
            "http://127.0.0.1:1",
            &format!("https://www.youtube.com/channel/{CHANNEL_ID}"),
        )
        .await
        .unwrap();
        assert_eq!(id, CHANNEL_ID);
    }

    #[tokio::test]
    async fn resolve_channel_id_scrapes_handle() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/@google"))
            .respond_with(ResponseTemplate::new(200).set_body_string(CHANNEL_PAGE))
            .expect(1)
            .mount(&server)
            .await;

        let id = resolve_channel_id(&http(), &server.uri(), "@google")
            .await
            .unwrap();
        assert_eq!(id, CHANNEL_ID);
    }

    #[tokio::test]
    async fn resolve_channel_id_scrapes_full_url() {
        // A full URL whose path isn't `/channel/UC…` is scraped as-is.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/c/SomeName"))
            .respond_with(ResponseTemplate::new(200).set_body_string(CHANNEL_PAGE))
            .expect(1)
            .mount(&server)
            .await;

        let input = format!("{}/c/SomeName", server.uri());
        let id = resolve_channel_id(&http(), &server.uri(), &input)
            .await
            .unwrap();
        assert_eq!(id, CHANNEL_ID);
    }

    #[tokio::test]
    async fn resolve_channel_id_surfaces_missing_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/@nobody"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>no id</html>"))
            .mount(&server)
            .await;

        let err = resolve_channel_id(&http(), &server.uri(), "@nobody")
            .await
            .unwrap_err();
        assert!(matches!(err, TranscriptError::ChannelNotFound { .. }));
    }

    #[tokio::test]
    async fn fetch_recent_videos_parses_feed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/feeds/videos.xml"))
            .and(query_param("channel_id", CHANNEL_ID))
            .respond_with(ResponseTemplate::new(200).set_body_string(RSS_FEED))
            .expect(1)
            .mount(&server)
            .await;

        let entries = fetch_recent_videos(&http(), &server.uri(), CHANNEL_ID)
            .await
            .unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].id, "aaaaaaaaaaa");
    }

    #[tokio::test]
    async fn fetch_all_video_ids_pages_through_continuation() {
        let server = MockServer::start().await;
        // First page: keyed by browseId.
        Mock::given(method("POST"))
            .and(path(super::super::innertube::BROWSE_PATH))
            .and(body_partial_json(json!({ "browseId": CHANNEL_ID })))
            .respond_with(ResponseTemplate::new(200).set_body_string(BROWSE_PAGE1))
            .mount(&server)
            .await;
        // Continuation page: keyed by the token from page 1.
        Mock::given(method("POST"))
            .and(path(super::super::innertube::BROWSE_PATH))
            .and(body_partial_json(json!({ "continuation": "CONT_TOKEN_1" })))
            .respond_with(ResponseTemplate::new(200).set_body_string(BROWSE_PAGE2))
            .mount(&server)
            .await;

        let ids = fetch_all_video_ids(&http(), &server.uri(), CHANNEL_ID)
            .await
            .unwrap();
        assert_eq!(ids, vec!["vid00000001", "vid00000002", "vid00000003"]);
    }

    #[tokio::test]
    async fn fetch_all_video_ids_surfaces_invalid_json() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(super::super::innertube::BROWSE_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_string("{ not json"))
            .mount(&server)
            .await;

        let err = fetch_all_video_ids(&http(), &server.uri(), CHANNEL_ID)
            .await
            .unwrap_err();
        assert!(matches!(err, TranscriptError::ParseError(_)));
    }

    // ── Online integration test ──
    //
    // Hits real YouTube — gated behind the `online_tests` custom cfg (see the
    // note in `super`'s tests). Run manually with
    // `RUSTFLAGS='--cfg online_tests' cargo test online_resolve_and_enumerate`.
    #[cfg(online_tests)]
    #[tokio::test]
    async fn online_resolve_and_enumerate() {
        const BASE_URL: &str = "https://www.youtube.com";
        // Google Developers — stable, high-volume, captioned channel.
        let id = resolve_channel_id(&http(), BASE_URL, "@GoogleDevelopers")
            .await
            .unwrap();
        assert!(is_channel_id(&id));

        let recent = fetch_recent_videos(&http(), BASE_URL, &id).await.unwrap();
        assert!(!recent.is_empty());
        assert!(recent.iter().all(|e| e.id.len() == 11));

        // Browse should return at least as many as RSS (full history ≥ recent
        // 15) and page past the first ~30 via continuation tokens.
        let all = fetch_all_video_ids(&http(), BASE_URL, &id).await.unwrap();
        assert!(all.len() >= recent.len());
        assert!(all.iter().all(|v| v.len() == 11));
    }
}
