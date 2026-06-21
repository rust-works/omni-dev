//! Per-video metadata sidecar: the serde view of the WEB `/player` response
//! and its projection to the [`VideoMetadata`] domain struct written to
//! `<out>/<channel-id>/<video-id>.meta.yaml`.
//!
//! The transcript path pins the `ANDROID_VR` client (see [`super::innertube`]),
//! whose `/player` response carries `videoDetails` but **no `microformat`** —
//! so it lacks publish date, like count, and category. A plain WEB-client
//! `/player` call (un-gated, no `visitorData`) supplies both `videoDetails`
//! and `microformat.playerMicroformatRenderer`, so metadata fetching is fully
//! independent of the bot-gated transcript path.
//!
//! ## Refresh signal
//!
//! The WEB `microformat` shape can drift independently of the `ANDROID_VR`
//! constants. If [`parse`] starts failing or returning empty metadata for
//! known-healthy videos, the field paths below (`publishDate`, `likeCount`,
//! `microformat.playerMicroformatRenderer`, …) are the place to re-check.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::transcript::error::{Result, TranscriptError};

/// Current sidecar schema version. Bumped only on a breaking field change;
/// new optional fields can be added under the same version.
pub const SCHEMA_VERSION: u32 = 1;

/// Per-video metadata sidecar, serialised to `<video-id>.meta.yaml`.
///
/// `view_count` / `like_count` are point-in-time snapshots; [`Self::fetched_at`]
/// is what makes them honest and is the refresh-staleness key. Fields sourced
/// from `microformat` are [`Option`] because that block is absent for
/// private/removed videos, in which case the sidecar is written from
/// `videoDetails` alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoMetadata {
    /// Sidecar schema version (always [`SCHEMA_VERSION`] when written).
    #[serde(default = "default_schema")]
    pub schema: u32,
    /// 11-character video ID.
    pub video_id: String,
    /// Display title.
    pub title: String,
    /// Channel / uploader name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// `UC…` channel ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    /// Owner profile URL (the `@handle` URL). From `microformat`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_url: Option<String>,
    /// Video category (e.g. `Music`). From `microformat`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Publish timestamp, full ISO 8601 with original offset preserved
    /// (e.g. `2009-10-24T23:57:33-07:00`). From `microformat.publishDate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_at: Option<String>,
    /// Duration in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<u64>,
    /// Full description (`videoDetails.shortDescription`, despite the name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Free-text keywords/tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    /// View count at fetch time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_count: Option<u64>,
    /// Like count at fetch time. Absent when ratings are disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub like_count: Option<u64>,
    /// Whether this is (or was) a live broadcast.
    #[serde(default)]
    pub is_live_content: bool,
    /// Whether the video is unlisted. From `microformat`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_unlisted: Option<bool>,
    /// Best available thumbnail URL (maxres when present).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbnail_url: Option<String>,
    /// When this sidecar was fetched (UTC). The refresh-staleness key.
    pub fetched_at: DateTime<Utc>,
}

fn default_schema() -> u32 {
    SCHEMA_VERSION
}

/// Project a raw WEB `/player` response into a [`VideoMetadata`].
///
/// `fetched_at` is supplied by the caller (the fetch timestamp) rather than
/// read from a clock here, keeping the projection a pure, unit-testable
/// function. Tolerant of an absent `microformat` block: the sidecar is then
/// written from `videoDetails` alone (microformat-only fields stay `None`).
/// Fails with [`TranscriptError::ParseError`] only if `videoDetails` itself is
/// missing (a removed/blocked video the WEB client won't describe).
pub fn parse(raw: &str, fetched_at: DateTime<Utc>) -> Result<VideoMetadata> {
    let web: WebPlayerResponse = serde_json::from_str(raw)
        .map_err(|e| TranscriptError::ParseError(format!("WEB playerResponse: {e}")))?;

    let details = web.video_details.ok_or_else(|| {
        TranscriptError::ParseError("WEB playerResponse: missing videoDetails".to_string())
    })?;
    let micro = web.microformat.and_then(|m| m.player_microformat_renderer);

    // viewCount appears in both blocks; videoDetails is the live snapshot.
    let view_count = details
        .view_count
        .as_ref()
        .and_then(Count::to_u64)
        .or_else(|| {
            micro
                .as_ref()
                .and_then(|m| m.view_count.as_ref())
                .and_then(Count::to_u64)
        });

    let thumbnail_url = micro
        .as_ref()
        .and_then(|m| m.thumbnail.as_ref())
        .and_then(ThumbnailList::best)
        .or_else(|| details.thumbnail.as_ref().and_then(ThumbnailList::best));

    let description = details.short_description.filter(|s| !s.is_empty());

    Ok(VideoMetadata {
        schema: SCHEMA_VERSION,
        video_id: details.video_id,
        title: details.title,
        channel: details
            .author
            .or_else(|| micro.as_ref().and_then(|m| m.owner_channel_name.clone())),
        channel_id: details
            .channel_id
            .or_else(|| micro.as_ref().and_then(|m| m.external_channel_id.clone())),
        channel_url: micro.as_ref().and_then(|m| m.owner_profile_url.clone()),
        category: micro.as_ref().and_then(|m| m.category.clone()),
        published_at: micro.as_ref().and_then(|m| m.publish_date.clone()),
        duration_seconds: details.length_seconds.as_ref().and_then(Count::to_u64),
        description,
        keywords: details.keywords,
        view_count,
        like_count: micro
            .as_ref()
            .and_then(|m| m.like_count.as_ref())
            .and_then(Count::to_u64),
        is_live_content: details.is_live_content.unwrap_or(false),
        is_unlisted: micro.as_ref().and_then(|m| m.is_unlisted),
        thumbnail_url,
        fetched_at,
    })
}

/// Read just the `fetched_at` stamp from an existing sidecar.
///
/// Used for staleness decisions during a directory scan. Returns `None` if the
/// YAML is corrupt or carries no parseable `fetched_at` — callers treat that as
/// "sidecar missing" and re-fetch.
pub fn read_fetched_at(yaml: &str) -> Option<DateTime<Utc>> {
    serde_yaml::from_str::<SidecarStamp>(yaml)
        .ok()
        .map(|s| s.fetched_at)
}

/// Minimal view used only to recover the staleness key from an on-disk
/// sidecar without requiring the rest of the document to be well-formed.
#[derive(Deserialize)]
struct SidecarStamp {
    fetched_at: DateTime<Utc>,
}

// ── Serde view of the WEB `/player` response ──

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebPlayerResponse {
    #[serde(default)]
    video_details: Option<WebVideoDetails>,
    #[serde(default)]
    microformat: Option<Microformat>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebVideoDetails {
    video_id: String,
    title: String,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    length_seconds: Option<Count>,
    #[serde(default)]
    short_description: Option<String>,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    view_count: Option<Count>,
    #[serde(default)]
    is_live_content: Option<bool>,
    #[serde(default)]
    thumbnail: Option<ThumbnailList>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Microformat {
    #[serde(default)]
    player_microformat_renderer: Option<PlayerMicroformatRenderer>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlayerMicroformatRenderer {
    #[serde(default)]
    publish_date: Option<String>,
    #[serde(default)]
    like_count: Option<Count>,
    #[serde(default)]
    view_count: Option<Count>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    owner_channel_name: Option<String>,
    #[serde(default)]
    owner_profile_url: Option<String>,
    #[serde(default)]
    external_channel_id: Option<String>,
    #[serde(default)]
    is_unlisted: Option<bool>,
    #[serde(default)]
    thumbnail: Option<ThumbnailList>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThumbnailList {
    #[serde(default)]
    thumbnails: Vec<Thumbnail>,
}

impl ThumbnailList {
    /// The highest-resolution thumbnail URL (YouTube lists ascending, so the
    /// last entry is the largest).
    fn best(&self) -> Option<String> {
        self.thumbnails.last().and_then(|t| t.url.clone())
    }
}

#[derive(Debug, Deserialize)]
struct Thumbnail {
    #[serde(default)]
    url: Option<String>,
}

/// A count field that YouTube encodes inconsistently as either a JSON string
/// (`"123"`) or a number (`123`) depending on client and block. Accepting both
/// keeps a string/number drift from failing the whole sidecar.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Count {
    Str(String),
    Num(u64),
}

impl Count {
    fn to_u64(&self) -> Option<u64> {
        match self {
            Self::Str(s) => s.parse().ok(),
            Self::Num(n) => Some(*n),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const WITH_MICROFORMAT: &str = include_str!("fixtures/player_response_web_metadata.json");
    const NO_MICROFORMAT: &str = include_str!("fixtures/player_response_web_no_microformat.json");

    fn fixed_fetched_at() -> DateTime<Utc> {
        "2026-06-11T03:12:45Z".parse().unwrap()
    }

    #[test]
    fn parse_projects_full_metadata_with_microformat() {
        let meta = parse(WITH_MICROFORMAT, fixed_fetched_at()).unwrap();
        assert_eq!(meta.schema, SCHEMA_VERSION);
        assert_eq!(meta.video_id, "dQw4w9WgXcQ");
        assert_eq!(meta.title, "Rick Astley - Never Gonna Give You Up");
        assert_eq!(meta.channel.as_deref(), Some("Rick Astley"));
        assert_eq!(meta.channel_id.as_deref(), Some("UCuAXFkgsw1L7xaCfnd5JJOw"));
        assert_eq!(
            meta.channel_url.as_deref(),
            Some("http://www.youtube.com/@RickAstleyYT")
        );
        assert_eq!(meta.category.as_deref(), Some("Music"));
        assert_eq!(
            meta.published_at.as_deref(),
            Some("2009-10-24T23:57:33-07:00")
        );
        assert_eq!(meta.duration_seconds, Some(213));
        assert!(meta
            .description
            .as_deref()
            .unwrap()
            .contains("official video"));
        assert_eq!(
            meta.keywords,
            vec!["rick astley", "Never Gonna Give You Up"]
        );
        assert_eq!(meta.view_count, Some(1_781_429_760));
        assert_eq!(meta.like_count, Some(19_148_727));
        assert!(!meta.is_live_content);
        assert_eq!(meta.is_unlisted, Some(false));
        assert_eq!(
            meta.thumbnail_url.as_deref(),
            Some("https://i.ytimg.com/vi/dQw4w9WgXcQ/maxresdefault.jpg")
        );
        assert_eq!(meta.fetched_at, fixed_fetched_at());
    }

    #[test]
    fn parse_tolerates_absent_microformat() {
        let meta = parse(NO_MICROFORMAT, fixed_fetched_at()).unwrap();
        assert_eq!(meta.video_id, "noMicro1234");
        assert_eq!(meta.title, "Private-ish video");
        assert_eq!(meta.channel.as_deref(), Some("Some Channel"));
        // microformat-only fields fall away rather than failing the parse
        assert_eq!(meta.published_at, None);
        assert_eq!(meta.like_count, None);
        assert_eq!(meta.category, None);
        assert_eq!(meta.is_unlisted, None);
        // videoDetails-sourced fields still populate
        assert_eq!(meta.view_count, Some(42));
        assert_eq!(meta.duration_seconds, Some(100));
    }

    #[test]
    fn parse_errors_when_video_details_missing() {
        let err = parse(
            r#"{"playabilityStatus":{"status":"ERROR"}}"#,
            fixed_fetched_at(),
        )
        .unwrap_err();
        assert!(matches!(err, TranscriptError::ParseError(_)));
    }

    #[test]
    fn parse_errors_on_malformed_json() {
        let err = parse("{ not json", fixed_fetched_at()).unwrap_err();
        assert!(matches!(err, TranscriptError::ParseError(_)));
    }

    #[test]
    fn count_accepts_string_or_number() {
        // viewCount as a bare number (microformat sometimes does this) still
        // projects to u64 rather than failing the sidecar.
        let raw = r#"{"videoDetails":{"videoId":"x","title":"t","viewCount":7}}"#;
        let meta = parse(raw, fixed_fetched_at()).unwrap();
        assert_eq!(meta.view_count, Some(7));
    }

    #[test]
    fn serialised_sidecar_round_trips_and_skips_empties() {
        let meta = parse(NO_MICROFORMAT, fixed_fetched_at()).unwrap();
        let yaml = serde_yaml::to_string(&meta).unwrap();
        // microformat-only fields are omitted when absent
        assert!(!yaml.contains("like_count"));
        assert!(!yaml.contains("category"));
        // required fields are present
        assert!(yaml.contains("schema: 1"));
        assert!(yaml.contains("video_id: noMicro1234"));
        assert!(yaml.contains("fetched_at:"));
        let back: VideoMetadata = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back, meta);
    }

    #[test]
    fn read_fetched_at_recovers_stamp() {
        let meta = parse(WITH_MICROFORMAT, fixed_fetched_at()).unwrap();
        let yaml = serde_yaml::to_string(&meta).unwrap();
        assert_eq!(read_fetched_at(&yaml), Some(fixed_fetched_at()));
    }

    #[test]
    fn read_fetched_at_returns_none_for_corrupt_or_missing() {
        assert_eq!(read_fetched_at("{ not yaml"), None);
        assert_eq!(read_fetched_at("schema: 1\nvideo_id: x\n"), None);
    }
}
