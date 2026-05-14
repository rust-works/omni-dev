//! Library-level integration tests for `voice reflect`.
//!
//! Exercises the end-to-end reflection pipeline (transcript → prompt →
//! mocked AI call → schema validation → events.jsonl) via the public
//! [`run_reflect`] entry point. The AI client is mocked in-process so
//! the test is deterministic and cheap; the CLI smoke test
//! ([`tests/voice_reflect_cli_test.rs`]) covers the subprocess /
//! argument-parsing layer separately.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Mutex;

use anyhow::Result;
use omni_dev::claude::ai::{AiClient, AiClientMetadata};
use omni_dev::voice::clock::FixedClock;
use omni_dev::voice::det::CountingUlidRng;
use omni_dev::voice::reflect::{run_reflect, ReflectOptions, TranscriptSource};

/// Minimal `AiClient` that returns a single canned response. Defined
/// inline rather than reusing `src/claude/test_utils.rs` because that
/// module is `pub(crate)` — integration tests can't see it.
struct CannedAiClient {
    response: Mutex<Option<String>>,
    metadata: AiClientMetadata,
}

impl CannedAiClient {
    fn new(response: String) -> Self {
        Self {
            response: Mutex::new(Some(response)),
            metadata: AiClientMetadata {
                provider: "Mock".to_string(),
                model: "mock-model".to_string(),
                max_context_length: 200_000,
                max_response_length: 8_192,
                active_beta: None,
            },
        }
    }
}

impl AiClient for CannedAiClient {
    fn send_request<'a>(
        &'a self,
        _system: &'a str,
        _user: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        let body = self
            .response
            .lock()
            .unwrap()
            .take()
            .unwrap_or_else(|| String::from("events: []"));
        Box::pin(async move { Ok(body) })
    }

    fn get_metadata(&self) -> AiClientMetadata {
        self.metadata.clone()
    }
}

fn fixture(name: &str) -> PathBuf {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.join("tests/fixtures/voice").join(name)
}

fn redact_filters() -> Vec<(&'static str, &'static str)> {
    vec![
        // Any 26-char Crockford base32 ULID inside quotes (covers
        // event_id, reflection_id, transcript_span endpoints, item_id,
        // decision_id, note_id, superseded_by). Liberal — `[0-9A-Z]`
        // because that's already established in the transcribe snapshot.
        (r#""[0-9A-Z]{26}""#, r#""<ULID>""#),
        // The fixed-clock RFC3339 timestamp varies in `+00:00` vs `Z`
        // suffix between chrono versions; normalise either way.
        (r"\+00:00", "Z"),
    ]
}

fn build_opts(transcript: PathBuf, canned_response: String) -> ReflectOptions {
    ReflectOptions {
        source: TranscriptSource::Path(transcript),
        ulid_rng: Box::new(CountingUlidRng::new()),
        clock: Box::new(FixedClock::from_rfc3339("2026-01-01T00:00:00Z")),
        ai: Box::new(CannedAiClient::new(canned_response)),
        session_root_override: None,
    }
}

#[tokio::test]
async fn reflect_short_en_transcript_against_canned_response() {
    let transcript = fixture("short_en_transcript.jsonl");
    let canned = std::fs::read_to_string(fixture("short_en_reflection_response.yaml")).unwrap();
    let opts = build_opts(transcript, canned);
    let mut out: Vec<u8> = Vec::new();
    run_reflect(opts, &mut out).await.unwrap();
    let body = String::from_utf8(out).unwrap();

    insta::with_settings!({ filters => redact_filters() }, {
        insta::assert_snapshot!("voice_reflect_short_en_events", body);
    });
}

#[tokio::test]
async fn malformed_response_yields_one_reflection_error() {
    let transcript = fixture("short_en_transcript.jsonl");
    let canned = std::fs::read_to_string(fixture("malformed_reflection_response.yaml")).unwrap();
    let opts = build_opts(transcript, canned);
    let mut out: Vec<u8> = Vec::new();
    run_reflect(opts, &mut out).await.unwrap();
    let body = String::from_utf8(out).unwrap();

    // Exactly one event line.
    assert_eq!(body.lines().count(), 1, "got: {body}");
    // It is a reflection.error.
    let parsed: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
    assert_eq!(parsed["event_type"], "reflection.error");

    insta::with_settings!({ filters => redact_filters() }, {
        insta::assert_snapshot!("voice_reflect_short_en_malformed", body);
    });
}

#[tokio::test]
async fn two_runs_with_same_seed_produce_byte_equal_output() {
    let transcript = fixture("short_en_transcript.jsonl");
    let canned = std::fs::read_to_string(fixture("short_en_reflection_response.yaml")).unwrap();

    let mut out1: Vec<u8> = Vec::new();
    run_reflect(build_opts(transcript.clone(), canned.clone()), &mut out1)
        .await
        .unwrap();

    let mut out2: Vec<u8> = Vec::new();
    run_reflect(build_opts(transcript, canned), &mut out2)
        .await
        .unwrap();

    assert_eq!(
        out1, out2,
        "FixedClock + CountingUlidRng + canned AI response should yield byte-stable output"
    );
}
