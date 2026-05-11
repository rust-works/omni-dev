//! Extract a YouTube video ID from a public URL or a bare ID.
//!
//! Recognised forms:
//!
//! - A bare 11-character ID like `dQw4w9WgXcQ`
//! - `https://www.youtube.com/watch?v=<id>` (any query parameters)
//! - `https://youtu.be/<id>` (any trailing query / fragment)
//! - `https://www.youtube.com/shorts/<id>`
//! - `https://www.youtube.com/embed/<id>`
//!
//! `m.` and `www.` subdomain prefixes are stripped before host matching.

use url::Url;

use crate::transcript::error::{Result, TranscriptError};

/// Length of a YouTube video ID. Stable since 2008.
const VIDEO_ID_LEN: usize = 11;

/// Extract the 11-character YouTube video ID from a public URL or a bare ID.
///
/// Accepts the watch, share, shorts, and embed forms listed in the module
/// docs, plus a bare 11-character ID that already matches the YouTube ID
/// shape. Returns
/// [`TranscriptError::InvalidLocator`]
/// if `input` is neither.
pub fn extract_video_id(input: &str) -> Result<String> {
    if is_valid_video_id(input) {
        return Ok(input.to_string());
    }

    let parsed =
        Url::parse(input).map_err(|_| TranscriptError::InvalidLocator(input.to_string()))?;

    let host = parsed
        .host_str()
        .ok_or_else(|| TranscriptError::InvalidLocator(input.to_string()))?;

    let host = host.strip_prefix("www.").unwrap_or(host);
    let host = host.strip_prefix("m.").unwrap_or(host);

    let id = match host {
        "youtu.be" => first_path_segment(&parsed),
        "youtube.com" => match parsed.path() {
            "/watch" => parsed
                .query_pairs()
                .find(|(k, _)| k == "v")
                .map(|(_, v)| v.into_owned()),
            path if path.starts_with("/shorts/") => path
                .trim_start_matches("/shorts/")
                .split('/')
                .next()
                .map(str::to_string),
            path if path.starts_with("/embed/") => path
                .trim_start_matches("/embed/")
                .split('/')
                .next()
                .map(str::to_string),
            _ => None,
        },
        _ => None,
    };

    match id {
        Some(id) if is_valid_video_id(&id) => Ok(id),
        _ => Err(TranscriptError::InvalidLocator(input.to_string())),
    }
}

fn first_path_segment(url: &Url) -> Option<String> {
    let path = url.path().trim_start_matches('/');
    if path.is_empty() {
        None
    } else {
        path.split('/').next().map(str::to_string)
    }
}

fn is_valid_video_id(id: &str) -> bool {
    id.len() == VIDEO_ID_LEN
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const ID: &str = "dQw4w9WgXcQ";

    #[test]
    fn watch_url_basic() {
        let id = extract_video_id(&format!("https://www.youtube.com/watch?v={ID}")).unwrap();
        assert_eq!(id, ID);
    }

    #[test]
    fn watch_url_with_extra_query_params() {
        let id = extract_video_id(&format!(
            "https://www.youtube.com/watch?v={ID}&t=42s&list=foo"
        ))
        .unwrap();
        assert_eq!(id, ID);
    }

    #[test]
    fn watch_url_v_param_not_first() {
        let id =
            extract_video_id(&format!("https://www.youtube.com/watch?list=foo&v={ID}")).unwrap();
        assert_eq!(id, ID);
    }

    #[test]
    fn watch_url_without_www() {
        let id = extract_video_id(&format!("https://youtube.com/watch?v={ID}")).unwrap();
        assert_eq!(id, ID);
    }

    #[test]
    fn watch_url_mobile_subdomain() {
        let id = extract_video_id(&format!("https://m.youtube.com/watch?v={ID}")).unwrap();
        assert_eq!(id, ID);
    }

    #[test]
    fn youtu_be_short_url() {
        let id = extract_video_id(&format!("https://youtu.be/{ID}")).unwrap();
        assert_eq!(id, ID);
    }

    #[test]
    fn youtu_be_with_trailing_query() {
        let id = extract_video_id(&format!("https://youtu.be/{ID}?t=42")).unwrap();
        assert_eq!(id, ID);
    }

    #[test]
    fn youtu_be_with_fragment() {
        let id = extract_video_id(&format!("https://youtu.be/{ID}#chapter-2")).unwrap();
        assert_eq!(id, ID);
    }

    #[test]
    fn shorts_url() {
        let id = extract_video_id(&format!("https://www.youtube.com/shorts/{ID}")).unwrap();
        assert_eq!(id, ID);
    }

    #[test]
    fn shorts_url_with_trailing_path() {
        let id = extract_video_id(&format!("https://www.youtube.com/shorts/{ID}/foo")).unwrap();
        assert_eq!(id, ID);
    }

    #[test]
    fn embed_url() {
        let id = extract_video_id(&format!("https://www.youtube.com/embed/{ID}")).unwrap();
        assert_eq!(id, ID);
    }

    #[test]
    fn http_scheme_accepted() {
        let id = extract_video_id(&format!("http://www.youtube.com/watch?v={ID}")).unwrap();
        assert_eq!(id, ID);
    }

    #[test]
    fn missing_v_param_errors() {
        let err = extract_video_id("https://www.youtube.com/watch").unwrap_err();
        assert!(matches!(err, TranscriptError::InvalidLocator(_)));
    }

    #[test]
    fn empty_v_param_errors() {
        let err = extract_video_id("https://www.youtube.com/watch?v=").unwrap_err();
        assert!(matches!(err, TranscriptError::InvalidLocator(_)));
    }

    #[test]
    fn id_too_short_errors() {
        let err = extract_video_id("https://www.youtube.com/watch?v=short").unwrap_err();
        assert!(matches!(err, TranscriptError::InvalidLocator(_)));
    }

    #[test]
    fn id_too_long_errors() {
        let err =
            extract_video_id("https://www.youtube.com/watch?v=waytoolongforavideo").unwrap_err();
        assert!(matches!(err, TranscriptError::InvalidLocator(_)));
    }

    #[test]
    fn id_with_invalid_character_errors() {
        // 11 chars but contains `!` which is outside [A-Za-z0-9_-]
        let err = extract_video_id("https://www.youtube.com/watch?v=abcd!fghijk").unwrap_err();
        assert!(matches!(err, TranscriptError::InvalidLocator(_)));
    }

    #[test]
    fn non_youtube_host_errors() {
        let err = extract_video_id(&format!("https://vimeo.com/watch?v={ID}")).unwrap_err();
        assert!(matches!(err, TranscriptError::InvalidLocator(_)));
    }

    #[test]
    fn unsupported_youtube_path_errors() {
        let err = extract_video_id("https://www.youtube.com/feed/trending").unwrap_err();
        assert!(matches!(err, TranscriptError::InvalidLocator(_)));
    }

    #[test]
    fn malformed_url_errors() {
        let err = extract_video_id("not a url at all").unwrap_err();
        assert!(matches!(err, TranscriptError::InvalidLocator(_)));
    }

    #[test]
    fn ids_containing_dashes_and_underscores_accepted() {
        let id = "abc-def_GHI";
        assert_eq!(id.len(), 11);
        let extracted = extract_video_id(&format!("https://www.youtube.com/watch?v={id}")).unwrap();
        assert_eq!(extracted, id);
    }

    #[test]
    fn error_message_contains_input() {
        let err = extract_video_id("https://example.com/foo").unwrap_err();
        assert!(err.to_string().contains("example.com"));
    }

    #[test]
    fn youtu_be_with_empty_path_errors() {
        let err = extract_video_id("https://youtu.be/").unwrap_err();
        assert!(matches!(err, TranscriptError::InvalidLocator(_)));
    }

    #[test]
    fn youtu_be_with_root_path_errors() {
        let err = extract_video_id("https://youtu.be").unwrap_err();
        assert!(matches!(err, TranscriptError::InvalidLocator(_)));
    }

    #[test]
    fn bare_video_id_accepted() {
        let id = extract_video_id(ID).unwrap();
        assert_eq!(id, ID);
    }

    #[test]
    fn bare_id_with_dashes_and_underscores_accepted() {
        let id = "abc-def_GHI";
        let extracted = extract_video_id(id).unwrap();
        assert_eq!(extracted, id);
    }

    #[test]
    fn bare_id_wrong_length_falls_through_to_url_parse() {
        // 10 chars looks neither like an ID nor a URL.
        let err = extract_video_id("abcdefghij").unwrap_err();
        assert!(matches!(err, TranscriptError::InvalidLocator(_)));
    }
}
