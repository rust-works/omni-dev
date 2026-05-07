//! Serde-deserialisable view of YouTube's `playerResponse` envelope plus the
//! caption-track selector.
//!
//! Only the fields this crate actually consumes are modelled — everything
//! else is dropped on the floor. Most fields are wrapped in [`Option`] so
//! malformed or partial responses surface through
//! [`TranscriptError::ParseError`] at the call site, not as deserialisation
//! errors deep inside `serde_json`.

use serde::Deserialize;

use crate::transcript::error::{Result, TranscriptError};
use crate::transcript::source::{FetchOpts, LanguageInfo, MediaInfo, TrackKind};

/// Top-level `playerResponse` envelope.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayerResponse {
    /// Whether YouTube will serve the video and captions.
    pub playability_status: PlayabilityStatus,
    /// Per-video metadata. Absent for refused playback.
    #[serde(default)]
    pub video_details: Option<VideoDetails>,
    /// Caption-track listing. Absent for videos with no captions at all.
    #[serde(default)]
    pub captions: Option<Captions>,
}

/// Why YouTube will or will not play the video.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlayabilityStatus {
    /// `OK` for playable videos. Other values include `LOGIN_REQUIRED`,
    /// `AGE_VERIFICATION_REQUIRED`, `UNPLAYABLE`, `LIVE_STREAM_OFFLINE`.
    pub status: String,
    /// Optional human-readable reason, e.g. "Sign in to confirm your age".
    #[serde(default)]
    pub reason: Option<String>,
}

/// Metadata about the video itself.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoDetails {
    /// 11-character video ID.
    pub video_id: String,
    /// Display title.
    pub title: String,
    /// Duration in seconds, encoded as a numeric string.
    #[serde(default)]
    pub length_seconds: Option<String>,
    /// Channel / uploader name.
    #[serde(default)]
    pub author: Option<String>,
}

/// Wraps the actual tracklist renderer. YouTube nests one level deep here.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Captions {
    /// The renderer carrying the caption tracks and translation languages.
    pub player_captions_tracklist_renderer: TracklistRenderer,
}

/// The set of caption tracks plus the languages YouTube can translate into.
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TracklistRenderer {
    /// All caption tracks available on the video.
    #[serde(default)]
    pub caption_tracks: Vec<CaptionTrack>,
    /// Languages YouTube can machine-translate any translatable track into.
    #[serde(default)]
    pub translation_languages: Vec<TranslationLanguage>,
}

/// A single caption track.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptionTrack {
    /// Pre-signed URL for the timedtext endpoint. Append `&fmt=json3` when
    /// fetching to get the structured event format this crate parses.
    pub base_url: String,
    /// Display name of the track, e.g. `English (United States)`.
    #[serde(default)]
    pub name: Option<LocalizedText>,
    /// IETF-style language tag, e.g. `en`, `en-US`, `pt-BR`.
    pub language_code: String,
    /// Present and equal to `"asr"` for auto-generated tracks; absent
    /// for human-authored ones.
    #[serde(default)]
    pub kind: Option<String>,
    /// Whether YouTube allows machine-translating this track.
    #[serde(default)]
    pub is_translatable: Option<bool>,
}

/// A localised text payload, in either of YouTube's two text shapes.
///
/// `{ "simpleText": "..." }` for short labels, or
/// `{ "runs": [{ "text": "..." }, ...] }` for fields that mix runs of
/// styled text. The shape varies by client and by field; modern
/// `ANDROID_VR` `/player` responses typically use `runs` for caption
/// track names. Either is accepted; [`Self::text`] returns the
/// concatenation.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum LocalizedText {
    /// `{ "simpleText": "<label>" }`.
    Simple {
        /// The flat text payload.
        #[serde(rename = "simpleText")]
        simple_text: String,
    },
    /// `{ "runs": [{ "text": "..." }, ...] }`.
    Runs {
        /// The styled text fragments.
        runs: Vec<TextRun>,
    },
}

/// A single styled-text fragment within a [`LocalizedText::Runs`] payload.
#[derive(Clone, Debug, Deserialize)]
pub struct TextRun {
    /// The fragment's text. Only the text is consumed; YouTube also
    /// includes styling fields (`bold`, `italics`, `navigationEndpoint`,
    /// etc.) that are dropped by this crate.
    pub text: String,
}

impl LocalizedText {
    /// Resolve to a flat string, joining `runs[].text` if present.
    pub fn text(&self) -> String {
        match self {
            Self::Simple { simple_text } => simple_text.clone(),
            Self::Runs { runs } => runs.iter().map(|r| r.text.as_str()).collect(),
        }
    }
}

/// A target language for machine translation.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranslationLanguage {
    /// IETF language tag of the translation target.
    pub language_code: String,
    /// Display name of the translation target.
    #[serde(default)]
    pub language_name: Option<LocalizedText>,
}

impl CaptionTrack {
    /// Whether this track is the auto-generated (ASR) variant.
    pub fn is_asr(&self) -> bool {
        self.kind.as_deref() == Some("asr")
    }
}

/// The outcome of [`select_track`].
///
/// Carries the chosen track, the URL to fetch (with `&tlang=` appended
/// for translations), and the metadata that should appear on the
/// resulting [`Transcript`](crate::transcript::Transcript).
#[derive(Clone, Debug)]
pub struct SelectedTrack<'a> {
    /// Reference to the chosen track in the source response.
    pub track: &'a CaptionTrack,
    /// URL to fetch (base URL with `fmt=json3` appended; for translation
    /// flows, also `&tlang=<target>`).
    pub fetch_url: String,
    /// Effective language code for the returned transcript. For translation
    /// this is the target language; otherwise it is the track's own code.
    pub language: String,
    /// Whether the result is manual, asr-generated, or machine-translated.
    pub kind: TrackKind,
}

/// Parse a raw `playerResponse` JSON document. Map serde failures to
/// [`TranscriptError::ParseError`] for caller convenience.
pub fn parse(raw: &str) -> Result<PlayerResponse> {
    serde_json::from_str(raw)
        .map_err(|e| TranscriptError::ParseError(format!("playerResponse: {e}")))
}

/// Surface a non-`OK` playability status as a typed error.
pub fn check_playability(response: &PlayerResponse) -> Result<()> {
    if response.playability_status.status == "OK" {
        Ok(())
    } else {
        Err(TranscriptError::PlayabilityRefused {
            status: response.playability_status.status.clone(),
            reason: response.playability_status.reason.clone(),
        })
    }
}

/// Select the caption track that best matches `opts`.
///
/// Selection priority:
///
/// 1. Manual track whose `language_code` exactly equals `opts.language`.
/// 2. Manual track whose `language_code` *starts with* `opts.language`
///    (so `en` matches `en-US`).
/// 3. If `opts.allow_auto`: the same two passes against ASR tracks.
/// 4. If `opts.translate_to` is set and the target language is in the
///    response's `translationLanguages`, append `&tlang=<target>` to the
///    first translatable track.
/// 5. Otherwise [`TranscriptError::LanguageNotFound`], or
///    [`TranscriptError::AutoCaptionsRequireOptIn`] if the only matches
///    were ASR and the caller did not pass `allow_auto`.
pub fn select_track<'a>(
    response: &'a PlayerResponse,
    opts: &FetchOpts,
) -> Result<SelectedTrack<'a>> {
    let tracks: &[CaptionTrack] = response.captions.as_ref().map_or(&[][..], |c| {
        &c.player_captions_tracklist_renderer.caption_tracks
    });

    if let Some(track) = pick(tracks, &opts.language, /* asr = */ false) {
        return Ok(materialise_native(track));
    }
    if opts.allow_auto {
        if let Some(track) = pick(tracks, &opts.language, /* asr = */ true) {
            return Ok(materialise_native(track));
        }
    }

    if let Some(target) = opts.translate_to.as_deref() {
        if let Some(track) = pick_translation_base(response, target) {
            return Ok(materialise_translation(track, target));
        }
    }

    let asr_only = !opts.allow_auto && pick(tracks, &opts.language, /* asr = */ true).is_some();
    if asr_only {
        return Err(TranscriptError::AutoCaptionsRequireOptIn(
            opts.language.clone(),
        ));
    }

    let available = tracks
        .iter()
        .map(|t| t.language_code.clone())
        .collect::<Vec<_>>();
    Err(TranscriptError::LanguageNotFound {
        requested: opts.language.clone(),
        available,
    })
}

fn pick<'a>(tracks: &'a [CaptionTrack], lang: &str, asr: bool) -> Option<&'a CaptionTrack> {
    tracks
        .iter()
        .find(|t| t.is_asr() == asr && t.language_code == lang)
        .or_else(|| {
            tracks
                .iter()
                .find(|t| t.is_asr() == asr && t.language_code.starts_with(lang))
        })
}

fn pick_translation_base<'a>(
    response: &'a PlayerResponse,
    target: &str,
) -> Option<&'a CaptionTrack> {
    let renderer = &response
        .captions
        .as_ref()?
        .player_captions_tracklist_renderer;
    let target_supported = renderer
        .translation_languages
        .iter()
        .any(|l| l.language_code == target);
    if !target_supported {
        return None;
    }
    renderer
        .caption_tracks
        .iter()
        .find(|t| !t.is_asr() && t.is_translatable.unwrap_or(true))
        .or_else(|| {
            renderer
                .caption_tracks
                .iter()
                .find(|t| t.is_translatable.unwrap_or(true))
        })
}

fn materialise_native(track: &CaptionTrack) -> SelectedTrack<'_> {
    SelectedTrack {
        track,
        fetch_url: timedtext_url(&track.base_url, None),
        language: track.language_code.clone(),
        kind: if track.is_asr() {
            TrackKind::Auto
        } else {
            TrackKind::Manual
        },
    }
}

fn materialise_translation<'a>(track: &'a CaptionTrack, target: &str) -> SelectedTrack<'a> {
    SelectedTrack {
        track,
        fetch_url: timedtext_url(&track.base_url, Some(target)),
        language: target.to_string(),
        kind: TrackKind::Translated,
    }
}

/// Build the timedtext fetch URL from a caption track's `baseUrl`.
///
/// YouTube embeds a default `fmt=` (typically `srv3`, the legacy XML
/// format) in the `baseUrl` it ships in `playerResponse`. A naive
/// `&fmt=json3` append produces a URL with two `fmt=` params; YouTube
/// honours the *first* one and returns SRV3, which the json3 parser
/// rejects with a 1:1 "expected value" error. Replacing rather than
/// appending is required.
///
/// Same treatment for `tlang=` on the translation path.
fn timedtext_url(base_url: &str, tlang: Option<&str>) -> String {
    let Ok(mut url) = url::Url::parse(base_url) else {
        // Defensive: if YouTube ever returns a non-URL baseUrl, fall
        // back to a naive append so the caller still gets *some*
        // reachable URL instead of a hard error here.
        let mut s = base_url.to_string();
        s.push(if s.contains('?') { '&' } else { '?' });
        s.push_str("fmt=json3");
        if let Some(t) = tlang {
            s.push_str("&tlang=");
            s.push_str(t);
        }
        return s;
    };
    let preserved: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "fmt" && k != "tlang")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    url.query_pairs_mut().clear();
    {
        let mut q = url.query_pairs_mut();
        for (k, v) in &preserved {
            q.append_pair(k, v);
        }
        q.append_pair("fmt", "json3");
        if let Some(t) = tlang {
            q.append_pair("tlang", t);
        }
    }
    url.to_string()
}

/// Project a [`PlayerResponse`] to the [`LanguageInfo`] list expected by
/// `omni-dev transcript youtube list-langs`.
pub fn list_languages(response: &PlayerResponse) -> Vec<LanguageInfo> {
    response
        .captions
        .as_ref()
        .map(|c| {
            c.player_captions_tracklist_renderer
                .caption_tracks
                .iter()
                .map(|t| LanguageInfo {
                    code: t.language_code.clone(),
                    name: t
                        .name
                        .as_ref()
                        .map_or_else(|| t.language_code.clone(), LocalizedText::text),
                    kind: if t.is_asr() {
                        TrackKind::Auto
                    } else {
                        TrackKind::Manual
                    },
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Project a [`PlayerResponse`] to the [`MediaInfo`] expected by
/// `omni-dev transcript youtube info`.
pub fn extract_media_info(response: &PlayerResponse) -> MediaInfo {
    let details = response.video_details.as_ref();
    MediaInfo {
        source: "youtube".to_string(),
        locator_id: details.map(|d| d.video_id.clone()).unwrap_or_default(),
        title: details.map(|d| d.title.clone()).unwrap_or_default(),
        author: details.and_then(|d| d.author.clone()),
        duration_ms: details
            .and_then(|d| d.length_seconds.as_deref())
            .and_then(|s| s.parse::<u64>().ok())
            .map(|s| s * 1_000),
        languages: list_languages(response),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const FIXTURE_BASIC: &str = include_str!("fixtures/player_response_basic.json");
    const FIXTURE_AGE_GATED: &str = include_str!("fixtures/player_response_age_gated.json");

    fn opts(lang: &str) -> FetchOpts {
        FetchOpts::new(lang)
    }

    #[test]
    fn parse_basic_fixture() {
        let r = parse(FIXTURE_BASIC).unwrap();
        assert_eq!(r.playability_status.status, "OK");
        let details = r.video_details.as_ref().unwrap();
        assert_eq!(details.video_id, "dQw4w9WgXcQ");
        assert_eq!(details.title, "Sample Video");
        assert_eq!(details.length_seconds.as_deref(), Some("212"));
        let renderer = &r
            .captions
            .as_ref()
            .unwrap()
            .player_captions_tracklist_renderer;
        assert_eq!(renderer.caption_tracks.len(), 3);
        assert_eq!(renderer.translation_languages.len(), 2);
    }

    #[test]
    fn parse_invalid_json_errors() {
        let err = parse("{ not valid json").unwrap_err();
        assert!(matches!(err, TranscriptError::ParseError(_)));
    }

    #[test]
    fn parse_missing_required_field_errors() {
        let err = parse("{}").unwrap_err();
        assert!(matches!(err, TranscriptError::ParseError(_)));
    }

    #[test]
    fn check_playability_passes_for_ok() {
        let r = parse(FIXTURE_BASIC).unwrap();
        assert!(check_playability(&r).is_ok());
    }

    #[test]
    fn check_playability_surfaces_login_required() {
        let r = parse(FIXTURE_AGE_GATED).unwrap();
        let err = check_playability(&r).unwrap_err();
        match err {
            TranscriptError::PlayabilityRefused { status, reason } => {
                assert_eq!(status, "LOGIN_REQUIRED");
                assert_eq!(reason.as_deref(), Some("Sign in to confirm your age"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn caption_track_is_asr_detects_kind() {
        let r = parse(FIXTURE_BASIC).unwrap();
        let tracks = &r
            .captions
            .as_ref()
            .unwrap()
            .player_captions_tracklist_renderer
            .caption_tracks;
        let asr_count = tracks.iter().filter(|t| t.is_asr()).count();
        assert_eq!(asr_count, 1);
    }

    #[test]
    fn select_exact_manual_match() {
        let r = parse(FIXTURE_BASIC).unwrap();
        let s = select_track(&r, &opts("es")).unwrap();
        assert_eq!(s.language, "es");
        assert_eq!(s.kind, TrackKind::Manual);
        assert!(s.fetch_url.contains("lang=es"));
        assert!(s.fetch_url.contains("fmt=json3"));
    }

    #[test]
    fn select_prefix_falls_back_to_longer_code() {
        let r = parse(FIXTURE_BASIC).unwrap();
        // "en" is not present as a manual track; "en-US" is.
        let s = select_track(&r, &opts("en")).unwrap();
        assert_eq!(s.language, "en-US");
        assert_eq!(s.kind, TrackKind::Manual);
    }

    #[test]
    fn select_excludes_asr_by_default() {
        // Build a response with only an ASR track for `de`.
        let mut r = parse(FIXTURE_BASIC).unwrap();
        let renderer = &mut r
            .captions
            .as_mut()
            .unwrap()
            .player_captions_tracklist_renderer;
        renderer.caption_tracks.retain(|t| t.language_code == "en");
        // The remaining `en` track is asr.
        let err = select_track(&r, &opts("en")).unwrap_err();
        assert!(matches!(err, TranscriptError::AutoCaptionsRequireOptIn(_)));
    }

    #[test]
    fn select_includes_asr_when_allow_auto() {
        let mut r = parse(FIXTURE_BASIC).unwrap();
        r.captions
            .as_mut()
            .unwrap()
            .player_captions_tracklist_renderer
            .caption_tracks
            .retain(|t| t.language_code == "en");
        let mut o = opts("en");
        o.allow_auto = true;
        let s = select_track(&r, &o).unwrap();
        assert_eq!(s.kind, TrackKind::Auto);
        assert_eq!(s.language, "en");
    }

    #[test]
    fn select_manual_takes_precedence_over_asr() {
        let r = parse(FIXTURE_BASIC).unwrap();
        let mut o = opts("en");
        o.allow_auto = true;
        // Both an `en-US` manual (prefix-match) and `en` asr (exact) exist.
        // Manual must win even when the asr match is "better" by exactness.
        let s = select_track(&r, &o).unwrap();
        assert_eq!(s.kind, TrackKind::Manual);
        assert_eq!(s.language, "en-US");
    }

    #[test]
    fn select_unknown_language_errors_with_available_list() {
        let r = parse(FIXTURE_BASIC).unwrap();
        let err = select_track(&r, &opts("ja")).unwrap_err();
        match err {
            TranscriptError::LanguageNotFound {
                requested,
                available,
            } => {
                assert_eq!(requested, "ja");
                assert!(available.iter().any(|c| c == "en-US"));
                assert!(available.iter().any(|c| c == "es"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn select_translation_synthesises_track() {
        let r = parse(FIXTURE_BASIC).unwrap();
        let mut o = opts("ja"); // no native ja track
        o.translate_to = Some("fr".into());
        let s = select_track(&r, &o).unwrap();
        assert_eq!(s.kind, TrackKind::Translated);
        assert_eq!(s.language, "fr");
        assert!(s.fetch_url.contains("tlang=fr"));
        assert!(s.fetch_url.contains("fmt=json3"));
    }

    #[test]
    fn select_translation_skipped_when_target_unsupported() {
        let r = parse(FIXTURE_BASIC).unwrap();
        let mut o = opts("ja");
        o.translate_to = Some("zz".into()); // not in translationLanguages
        let err = select_track(&r, &o).unwrap_err();
        assert!(matches!(err, TranscriptError::LanguageNotFound { .. }));
    }

    #[test]
    fn select_native_match_skips_translation_path() {
        let r = parse(FIXTURE_BASIC).unwrap();
        let mut o = opts("es");
        o.translate_to = Some("fr".into());
        let s = select_track(&r, &o).unwrap();
        // Native `es` was available, so the translation flag is ignored.
        assert_eq!(s.kind, TrackKind::Manual);
        assert_eq!(s.language, "es");
    }

    #[test]
    fn select_translation_falls_back_to_asr_when_no_manual_translatable() {
        // No manual track exists at all; only an ASR `en` track. Translation
        // must still synthesise a target track from the ASR base.
        let mut r = parse(FIXTURE_BASIC).unwrap();
        let renderer = &mut r
            .captions
            .as_mut()
            .unwrap()
            .player_captions_tracklist_renderer;
        renderer.caption_tracks.retain(CaptionTrack::is_asr);
        let mut o = opts("ja");
        o.translate_to = Some("fr".into());
        let s = select_track(&r, &o).unwrap();
        assert_eq!(s.kind, TrackKind::Translated);
        assert_eq!(s.language, "fr");
        assert!(s.fetch_url.contains("tlang=fr"));
    }

    #[test]
    fn select_translation_skips_non_translatable_manual() {
        // Mark the only manual track non-translatable; selector must skip it
        // and either pick an ASR base or yield no candidate.
        let mut r = parse(FIXTURE_BASIC).unwrap();
        let renderer = &mut r
            .captions
            .as_mut()
            .unwrap()
            .player_captions_tracklist_renderer;
        renderer.caption_tracks.retain(|t| t.language_code != "es");
        for t in &mut renderer.caption_tracks {
            if !t.is_asr() {
                t.is_translatable = Some(false);
            }
        }
        let mut o = opts("ja");
        o.translate_to = Some("fr".into());
        // The ASR `en` track is translatable by default, so selector falls
        // through to it.
        let s = select_track(&r, &o).unwrap();
        assert_eq!(s.kind, TrackKind::Translated);
        assert_eq!(s.language, "fr");
        assert!(s.track.is_asr());
    }

    #[test]
    fn fetch_url_uses_question_mark_when_base_has_none() {
        // Synthetic — real YouTube URLs always have a query string, but
        // the helper must still produce a valid URL if not.
        let track = CaptionTrack {
            base_url: "https://example.com/captions".to_string(),
            name: None,
            language_code: "en".to_string(),
            kind: None,
            is_translatable: Some(true),
        };
        let s = materialise_native(&track);
        assert_eq!(s.fetch_url, "https://example.com/captions?fmt=json3");
    }

    #[test]
    fn fetch_url_replaces_existing_fmt_param() {
        // Real ANDROID_VR /player responses ship caption URLs with an
        // embedded `fmt=srv3` (legacy XML). A naive append would leave
        // both `fmt` params; YouTube honours the first and serves SRV3,
        // which the json3 parser rejects. Pin the replacement contract.
        let track = CaptionTrack {
            base_url: "https://example.com/timedtext?lang=en&fmt=srv3&signature=ABC".to_string(),
            name: None,
            language_code: "en".to_string(),
            kind: None,
            is_translatable: Some(true),
        };
        let s = materialise_native(&track);
        assert!(s.fetch_url.contains("fmt=json3"));
        assert!(!s.fetch_url.contains("fmt=srv3"));
        assert!(s.fetch_url.contains("signature=ABC"));
        assert!(s.fetch_url.contains("lang=en"));
    }

    #[test]
    fn translation_url_replaces_existing_fmt_and_tlang() {
        let track = CaptionTrack {
            base_url: "https://example.com/timedtext?lang=en&fmt=srv3&tlang=de&signature=X"
                .to_string(),
            name: None,
            language_code: "en".to_string(),
            kind: None,
            is_translatable: Some(true),
        };
        let s = materialise_translation(&track, "fr");
        assert!(s.fetch_url.contains("fmt=json3"));
        assert!(!s.fetch_url.contains("fmt=srv3"));
        assert!(s.fetch_url.contains("tlang=fr"));
        assert!(!s.fetch_url.contains("tlang=de"));
        assert!(s.fetch_url.contains("signature=X"));
    }

    #[test]
    fn fetch_url_handles_non_url_base_defensively() {
        // If YouTube ever returns a baseUrl that isn't parseable as a
        // URL, the helper falls back to a naive append rather than
        // panicking — the caller still gets a reachable string.
        let track = CaptionTrack {
            base_url: "not a url".to_string(),
            name: None,
            language_code: "en".to_string(),
            kind: None,
            is_translatable: Some(true),
        };
        let s = materialise_native(&track);
        assert!(s.fetch_url.contains("fmt=json3"));
    }

    #[test]
    fn translation_url_handles_non_url_base_defensively() {
        // Same fallback, exercised through the translation path so the
        // `&tlang=` branch of the non-URL fallback is covered.
        let track = CaptionTrack {
            base_url: "not a url".to_string(),
            name: None,
            language_code: "en".to_string(),
            kind: None,
            is_translatable: Some(true),
        };
        let s = materialise_translation(&track, "fr");
        assert!(s.fetch_url.contains("fmt=json3"));
        assert!(s.fetch_url.contains("&tlang=fr"));
    }

    #[test]
    fn fallback_appends_with_ampersand_when_base_already_has_query() {
        // `?` vs `&` separator branch in the non-URL fallback. The
        // string contains `?` so the next param is joined with `&`.
        let track = CaptionTrack {
            base_url: "broken url with ?existing=q".to_string(),
            name: None,
            language_code: "en".to_string(),
            kind: None,
            is_translatable: Some(true),
        };
        let s = materialise_native(&track);
        assert!(s.fetch_url.ends_with("&fmt=json3"));
    }

    #[test]
    fn list_languages_falls_back_to_language_code_when_name_absent() {
        // Track without a `name` field: `LanguageInfo.name` falls back
        // to the language code rather than panicking. Exercises the
        // `None` branch of `t.name.as_ref().map_or_else(...)`.
        let mut r = parse(FIXTURE_BASIC).unwrap();
        for t in &mut r
            .captions
            .as_mut()
            .unwrap()
            .player_captions_tracklist_renderer
            .caption_tracks
        {
            t.name = None;
        }
        let langs = list_languages(&r);
        assert!(langs.iter().any(|l| l.code == "en-US" && l.name == "en-US"));
        assert!(langs.iter().any(|l| l.code == "es" && l.name == "es"));
    }

    #[test]
    fn list_languages_projects_all_tracks() {
        let r = parse(FIXTURE_BASIC).unwrap();
        let langs = list_languages(&r);
        assert_eq!(langs.len(), 3);
        let by_code: std::collections::HashMap<_, _> =
            langs.iter().map(|l| (l.code.clone(), l)).collect();
        assert_eq!(by_code["en-US"].kind, TrackKind::Manual);
        assert_eq!(by_code["en"].kind, TrackKind::Auto);
        assert_eq!(by_code["es"].kind, TrackKind::Manual);
        assert_eq!(by_code["en-US"].name, "English (United States)");
    }

    #[test]
    fn localized_text_deserialises_simple_shape() {
        let json = r#"{ "simpleText": "English (United States)" }"#;
        let lt: LocalizedText = serde_json::from_str(json).unwrap();
        assert!(matches!(lt, LocalizedText::Simple { .. }));
        assert_eq!(lt.text(), "English (United States)");
    }

    #[test]
    fn localized_text_deserialises_runs_shape() {
        // Real-world ANDROID_VR /player responses use this shape for
        // caption-track names; the previous struct-only deserialiser
        // failed with `missing field simpleText` on these payloads.
        let json = r#"{ "runs": [{ "text": "English (auto-generated)" }] }"#;
        let lt: LocalizedText = serde_json::from_str(json).unwrap();
        assert!(matches!(lt, LocalizedText::Runs { .. }));
        assert_eq!(lt.text(), "English (auto-generated)");
    }

    #[test]
    fn localized_text_concatenates_multiple_runs() {
        let json = r#"{ "runs": [{ "text": "English" }, { "text": " " }, { "text": "(auto)" }] }"#;
        let lt: LocalizedText = serde_json::from_str(json).unwrap();
        assert_eq!(lt.text(), "English (auto)");
    }

    #[test]
    fn localized_text_handles_empty_runs() {
        let json = r#"{ "runs": [] }"#;
        let lt: LocalizedText = serde_json::from_str(json).unwrap();
        assert_eq!(lt.text(), "");
    }

    #[test]
    fn caption_track_accepts_runs_name_shape() {
        // End-to-end: a player_response carrying `name: { runs: [...] }`
        // must deserialise and project to LanguageInfo.name correctly.
        let json = r#"{
            "playabilityStatus": { "status": "OK" },
            "captions": {
                "playerCaptionsTracklistRenderer": {
                    "captionTracks": [{
                        "baseUrl": "https://www.youtube.com/api/timedtext?lang=en",
                        "name": { "runs": [{ "text": "English (auto-generated)" }] },
                        "languageCode": "en",
                        "kind": "asr"
                    }],
                    "translationLanguages": []
                }
            }
        }"#;
        let r = parse(json).unwrap();
        let langs = list_languages(&r);
        assert_eq!(langs.len(), 1);
        assert_eq!(langs[0].code, "en");
        assert_eq!(langs[0].name, "English (auto-generated)");
        assert_eq!(langs[0].kind, TrackKind::Auto);
    }

    #[test]
    fn list_languages_handles_no_captions() {
        let r = parse(FIXTURE_AGE_GATED).unwrap();
        assert_eq!(list_languages(&r), vec![]);
    }

    #[test]
    fn extract_media_info_populates_fields() {
        let r = parse(FIXTURE_BASIC).unwrap();
        let info = extract_media_info(&r);
        assert_eq!(info.source, "youtube");
        assert_eq!(info.locator_id, "dQw4w9WgXcQ");
        assert_eq!(info.title, "Sample Video");
        assert_eq!(info.author.as_deref(), Some("Sample Channel"));
        assert_eq!(info.duration_ms, Some(212_000));
        assert_eq!(info.languages.len(), 3);
    }

    #[test]
    fn extract_media_info_tolerates_missing_details() {
        let r = parse(FIXTURE_AGE_GATED).unwrap();
        let info = extract_media_info(&r);
        assert_eq!(info.source, "youtube");
        assert_eq!(info.locator_id, "");
        assert_eq!(info.title, "");
        assert!(info.languages.is_empty());
    }
}
