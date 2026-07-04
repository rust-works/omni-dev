//! Parser for YouTube's `json3` (a.k.a. `srv3`) timedtext format.
//!
//! The endpoint at `https://www.youtube.com/api/timedtext?...&fmt=json3`
//! returns a document with an `events` array. Each event has a start time,
//! duration, and a list of `seg` entries whose `utf8` payloads are
//! concatenated to form the cue text. Events without `segs` are styling /
//! window markers and are skipped here.

use serde::Deserialize;

use crate::transcript::cue::Cue;
use crate::transcript::error::{Result, TranscriptError};

/// Top-level json3 document.
#[derive(Clone, Debug, Deserialize, Default)]
struct Json3 {
    #[serde(default)]
    events: Vec<Event>,
}

/// A single event entry. Most fields are optional because YouTube emits
/// styling-only events that have no timing or text.
#[derive(Clone, Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Event {
    #[serde(default, rename = "tStartMs")]
    t_start_ms: Option<u64>,
    #[serde(default, rename = "dDurationMs")]
    d_duration_ms: Option<u64>,
    #[serde(default)]
    segs: Option<Vec<Segment>>,
}

/// A single text segment within an event.
#[derive(Clone, Debug, Deserialize, Default)]
struct Segment {
    #[serde(default)]
    utf8: Option<String>,
}

/// GET a fully-prepared timedtext URL and return the response body.
///
/// `url` is consumed as-is. Callers normally obtain it from
/// [`super::player_response::SelectedTrack::fetch_url`], which already
/// carries the signed signature, `fmt=json3`, and any `tlang=` parameter.
pub async fn fetch(http: &reqwest::Client, url: &str) -> Result<String> {
    let started = std::time::Instant::now();
    let result = http.get(url).send().await;
    super::record_yt_http("GET", url, started, &result);
    let response = result?.error_for_status()?;
    Ok(response.text().await?)
}

/// Parse a json3 timedtext document into a list of cues, dropping events
/// that carry no text (styling / window markers).
pub fn parse(raw: &str) -> Result<Vec<Cue>> {
    let doc: Json3 = serde_json::from_str(raw)
        .map_err(|e| TranscriptError::ParseError(format!("timedtext json3: {e}")))?;

    let mut cues = Vec::with_capacity(doc.events.len());
    for event in doc.events {
        let Some(segs) = event.segs else {
            continue;
        };
        let text = segs.into_iter().filter_map(|s| s.utf8).collect::<String>();
        if text.is_empty() {
            continue;
        }
        let start_ms = event.t_start_ms.unwrap_or(0);
        let end_ms = start_ms.saturating_add(event.d_duration_ms.unwrap_or(0));
        cues.push(Cue::new(start_ms, end_ms, text));
    }
    Ok(cues)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const FIXTURE_BASIC: &str = include_str!("fixtures/timedtext_basic.json");

    #[test]
    fn parse_basic_fixture() {
        let cues = parse(FIXTURE_BASIC).unwrap();
        assert_eq!(cues.len(), 3);
        assert_eq!(cues[0], Cue::new(0, 1500, "Hello, world."));
        assert_eq!(cues[1], Cue::new(2000, 3000, "This is a test."));
        assert_eq!(cues[2], Cue::new(4000, 6000, "Final cue\nwith newline."));
    }

    #[test]
    fn parse_empty_events_array() {
        let cues = parse(r#"{"events": []}"#).unwrap();
        assert!(cues.is_empty());
    }

    #[test]
    fn parse_missing_events_key_is_empty() {
        let cues = parse(r"{}").unwrap();
        assert!(cues.is_empty());
    }

    #[test]
    fn parse_skips_event_without_segs() {
        let raw = r#"{
            "events": [
                { "tStartMs": 0, "dDurationMs": 1000 },
                { "tStartMs": 1000, "dDurationMs": 1000, "segs": [{"utf8": "kept"}] }
            ]
        }"#;
        let cues = parse(raw).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "kept");
    }

    #[test]
    fn parse_skips_event_with_empty_text() {
        let raw = r#"{
            "events": [
                { "tStartMs": 0, "dDurationMs": 1000, "segs": [{}] },
                { "tStartMs": 1000, "dDurationMs": 1000, "segs": [{"utf8": ""}] },
                { "tStartMs": 2000, "dDurationMs": 1000, "segs": [{"utf8": "kept"}] }
            ]
        }"#;
        let cues = parse(raw).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].text, "kept");
    }

    #[test]
    fn parse_concatenates_multiple_segs() {
        let raw = r#"{
            "events": [
                {
                    "tStartMs": 0,
                    "dDurationMs": 500,
                    "segs": [
                        {"utf8": "a "},
                        {"utf8": "b "},
                        {"utf8": "c"}
                    ]
                }
            ]
        }"#;
        let cues = parse(raw).unwrap();
        assert_eq!(cues, vec![Cue::new(0, 500, "a b c")]);
    }

    #[test]
    fn parse_uses_zero_when_start_missing() {
        let raw = r#"{
            "events": [
                { "dDurationMs": 1000, "segs": [{"utf8": "x"}] }
            ]
        }"#;
        let cues = parse(raw).unwrap();
        assert_eq!(cues, vec![Cue::new(0, 1000, "x")]);
    }

    #[test]
    fn parse_uses_zero_when_duration_missing() {
        let raw = r#"{
            "events": [
                { "tStartMs": 1500, "segs": [{"utf8": "instant"}] }
            ]
        }"#;
        let cues = parse(raw).unwrap();
        assert_eq!(cues, vec![Cue::new(1500, 1500, "instant")]);
    }

    #[test]
    fn parse_invalid_json_errors() {
        let err = parse("{ not json").unwrap_err();
        assert!(matches!(err, TranscriptError::ParseError(_)));
        assert!(err.to_string().contains("timedtext json3"));
    }

    #[test]
    fn parse_ignores_unknown_event_fields() {
        let raw = r#"{
            "events": [
                {
                    "tStartMs": 0,
                    "dDurationMs": 100,
                    "wWinId": 1,
                    "wpWinPosId": 2,
                    "segs": [{"utf8": "x", "tOffsetMs": 0, "acAsrConf": 256}]
                }
            ]
        }"#;
        let cues = parse(raw).unwrap();
        assert_eq!(cues, vec![Cue::new(0, 100, "x")]);
    }

    #[test]
    fn parse_preserves_event_order() {
        let raw = r#"{
            "events": [
                { "tStartMs": 0,    "dDurationMs": 100, "segs": [{"utf8": "first"}] },
                { "tStartMs": 200,  "dDurationMs": 100, "segs": [{"utf8": "second"}] },
                { "tStartMs": 1000, "dDurationMs": 100, "segs": [{"utf8": "third"}] }
            ]
        }"#;
        let cues = parse(raw).unwrap();
        let texts: Vec<_> = cues.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, vec!["first", "second", "third"]);
    }

    #[test]
    fn parse_handles_unicode_text() {
        let raw = r#"{
            "events": [
                { "tStartMs": 0, "dDurationMs": 100, "segs": [{"utf8": "こんにちは "}, {"utf8": "🌍"}] }
            ]
        }"#;
        let cues = parse(raw).unwrap();
        assert_eq!(cues, vec![Cue::new(0, 100, "こんにちは 🌍")]);
    }

    #[tokio::test]
    async fn fetch_returns_body_for_2xx() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/timedtext"))
            .and(query_param("fmt", "json3"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FIXTURE_BASIC))
            .expect(1)
            .mount(&server)
            .await;

        let http = reqwest::Client::builder().build().unwrap();
        let url = format!("{}/api/timedtext?lang=en&fmt=json3", server.uri());
        let body = fetch(&http, &url).await.unwrap();
        assert_eq!(body, FIXTURE_BASIC);
    }

    #[tokio::test]
    async fn fetch_surfaces_non_2xx_as_http_error() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/timedtext"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let http = reqwest::Client::builder().build().unwrap();
        let url = format!("{}/api/timedtext?lang=en&fmt=json3", server.uri());
        let err = fetch(&http, &url).await.unwrap_err();
        assert!(matches!(err, TranscriptError::Http(_)));
    }

    #[test]
    fn parse_saturates_when_duration_overflows() {
        let raw = format!(
            r#"{{ "events": [ {{ "tStartMs": {start}, "dDurationMs": {dur}, "segs": [{{"utf8":"x"}}] }} ] }}"#,
            start = u64::MAX - 100,
            dur = 1000,
        );
        let cues = parse(&raw).unwrap();
        assert_eq!(cues.len(), 1);
        assert_eq!(cues[0].end_ms, u64::MAX);
    }
}
