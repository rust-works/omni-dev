//! YouTube [`TranscriptSource`](crate::transcript::TranscriptSource).
//!
//! Step 2 of [issue #687](https://github.com/rust-works/omni-dev/issues/687)
//! lands the *offline* parsing layers — URL → video ID, `playerResponse`
//! deserialisation and selector, and json3 timedtext parsing. The HTTP
//! layer (InnerTube `/player` POST and the WEB / Android / IosEmbedded
//! client fallback chain) and the [`TranscriptSource`] impl arrive in
//! step 3.

pub mod player_response;
pub mod timedtext;
pub mod url;

pub use player_response::{
    check_playability, extract_media_info, list_languages, parse as parse_player_response,
    select_track, CaptionTrack, PlayerResponse, SelectedTrack,
};
pub use timedtext::parse as parse_timedtext;
pub use url::extract_video_id;

/// Whether `input` is recognised as a YouTube URL by [`extract_video_id`].
///
/// Used by the future `omni-dev transcript fetch <url>` auto-detection
/// path and by the [`TranscriptSource::matches`] impl that lands with
/// step 3.
///
/// [`TranscriptSource::matches`]: crate::transcript::TranscriptSource::matches
pub fn matches_url(input: &str) -> bool {
    extract_video_id(input).is_ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! End-to-end offline pipeline: parse a checked-in `playerResponse`,
    //! select the requested track, parse a checked-in json3 transcript,
    //! render via the [`format::srt`] converter, and compare to a golden
    //! `.srt` fixture. This is the acceptance gate for step 2.
    //!
    //! [`format::srt`]: crate::transcript::format::srt

    use super::*;
    use crate::transcript::format::srt;
    use crate::transcript::source::{FetchOpts, TrackKind, Transcript};

    const PLAYER_RESPONSE: &str = include_str!("youtube/fixtures/player_response_basic.json");
    const TIMEDTEXT: &str = include_str!("youtube/fixtures/timedtext_basic.json");
    const EXPECTED_SRT: &str = include_str!("youtube/fixtures/expected_basic.srt");

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
    fn end_to_end_player_response_to_srt() {
        // 1. Parse the player response and verify playability.
        let response = parse_player_response(PLAYER_RESPONSE).unwrap();
        check_playability(&response).unwrap();

        // 2. Select a track for the requested language.
        let opts = FetchOpts::new("en-US");
        let selected = select_track(&response, &opts).unwrap();
        assert_eq!(selected.kind, TrackKind::Manual);
        assert_eq!(selected.language, "en-US");

        // 3. Parse the json3 fixture into cues.
        let cues = parse_timedtext(TIMEDTEXT).unwrap();
        assert_eq!(cues.len(), 3);

        // 4. Assemble a Transcript and render to SRT.
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

        // 5. The output must match the checked-in golden byte-for-byte.
        assert_eq!(rendered, EXPECTED_SRT);
    }

    #[test]
    fn end_to_end_translation_path_picks_target_language() {
        let response = parse_player_response(PLAYER_RESPONSE).unwrap();
        let mut opts = FetchOpts::new("ja"); // not present natively
        opts.translate_to = Some("fr".into()); // present in translationLanguages
        let selected = select_track(&response, &opts).unwrap();
        assert_eq!(selected.kind, TrackKind::Translated);
        assert_eq!(selected.language, "fr");
        assert!(selected.fetch_url.contains("tlang=fr"));
    }
}
