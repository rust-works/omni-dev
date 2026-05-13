//! End-to-end snapshot of the `Transcriber` trait wired through the
//! factory with the mock backend.
//!
//! `MockTranscriber` is the only backend in #801 — the real ASR backend
//! was deferred — so this test pins the mock's JSONL output. When a real
//! backend lands, a separate `#[ignore]`'d integration test will pin its
//! output against the committed `tests/fixtures/voice/short_en.wav`
//! fixture.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::Duration;

use omni_dev::voice::backends::mock::{CountingUlidRng, MockSegment, MockTranscriber};
use omni_dev::voice::{Transcriber, TranscriptEvent, VecAudioInput};

fn fixture_path() -> PathBuf {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.join("tests/fixtures/voice/short_en.wav")
}

fn deterministic_script() -> Vec<MockSegment> {
    vec![
        MockSegment {
            text: "hello world".into(),
            start: Duration::from_millis(0),
            end: Duration::from_secs(2),
            confidence: 0.95,
        },
        MockSegment {
            text: "this is the mock transcriber".into(),
            start: Duration::from_secs(2),
            end: Duration::from_millis(5_500),
            confidence: 0.92,
        },
        MockSegment {
            text: "emitting a deterministic event sequence".into(),
            start: Duration::from_millis(5_500),
            end: Duration::from_secs(10),
            confidence: 0.97,
        },
    ]
}

fn transcribe_to_jsonl() -> String {
    let transcriber =
        MockTranscriber::with_rng(deterministic_script(), Box::new(CountingUlidRng::new()));
    let input = VecAudioInput::from_wav_path(fixture_path(), 1024).unwrap();
    let events: Vec<TranscriptEvent> = transcriber
        .transcribe(Box::new(input))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    events
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn mock_transcriber_emits_deterministic_jsonl() {
    insta::assert_snapshot!(transcribe_to_jsonl());
}

#[test]
fn output_is_byte_stable_across_runs() {
    // Belt-and-braces: confirm the same script + fixture + RNG seed
    // produce identical bytes on two independent calls. Catches latent
    // sources of non-determinism (HashMap ordering, time-based entropy
    // leaking in) before the snapshot test catches them.
    assert_eq!(transcribe_to_jsonl(), transcribe_to_jsonl());
}
