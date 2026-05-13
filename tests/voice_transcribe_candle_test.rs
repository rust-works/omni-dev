//! Whisper-candle backend end-to-end against `tests/fixtures/voice/short_en.wav`.
//!
//! `#[ignore]`-by-default because the test needs the Whisper tiny.en model
//! files staged on disk and the candle inference run costs ~1 s of CPU.
//! Run locally with:
//!
//! ```text
//! omni-dev voice install-model
//! cargo test --test voice_transcribe_candle_test -- --ignored
//! ```
//!
//! Or point at a pre-staged install via `OMNI_DEV_VOICE_WHISPER_MODEL` (the
//! intended hook for the CI cache once the runner-side caching lands).
//!
//! Assertion is **case-insensitive substring match** on the seven content
//! words from `baseline/content_words.txt` (issue #813): Whisper output
//! varies subtly across model versions and we want tolerance.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use omni_dev::voice::backends::candle::CandleTranscriber;
use omni_dev::voice::models::{default_whisper_model_dir, ensure_model_present};
use omni_dev::voice::transcriber::{Transcriber, TranscriptEvent, VecAudioInput};

/// Content words captured from the whisper.cpp baseline in #813; the
/// candle backend must surface every one of them (case-insensitive).
const CONTENT_WORDS: &[&str] = &[
    "wizards",
    "tempers",
    "universal",
    "flaw",
    "species",
    "fighting",
    "rely",
];

fn fixture_wav() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/voice/short_en.wav")
}

fn resolve_model_dir() -> Option<PathBuf> {
    if let Ok(env) = std::env::var("OMNI_DEV_VOICE_WHISPER_MODEL") {
        if !env.is_empty() {
            return Some(PathBuf::from(env));
        }
    }
    default_whisper_model_dir().filter(|d| ensure_model_present(d).is_ok())
}

#[test]
#[ignore = "requires Whisper tiny.en model on disk; run `omni-dev voice install-model` first"]
fn whisper_candle_transcribes_short_en_with_content_words() {
    let Some(model_dir) = resolve_model_dir() else {
        panic!(
            "Whisper model not found. Run `omni-dev voice install-model` or set \
             OMNI_DEV_VOICE_WHISPER_MODEL=<path> to point at a pre-staged install."
        );
    };

    let transcriber =
        CandleTranscriber::new(&model_dir).expect("CandleTranscriber::new should succeed");
    let input = VecAudioInput::from_wav_path(fixture_wav(), 1024).expect("fixture wav should load");
    let stream = transcriber
        .transcribe(Box::new(input))
        .expect("transcribe should succeed");

    let events: Vec<TranscriptEvent> = stream
        .collect::<anyhow::Result<Vec<_>>>()
        .expect("backend should not error mid-stream");

    let finals: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            TranscriptEvent::Final { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        !finals.is_empty(),
        "expected at least one Final event, got events: {events:?}"
    );

    let endpoint_at_end = matches!(events.last(), Some(TranscriptEvent::Endpoint { .. }));
    assert!(
        endpoint_at_end,
        "last event must be Endpoint, got: {:?}",
        events.last()
    );

    let transcript_lower = finals.join(" ").to_lowercase();
    for word in CONTENT_WORDS {
        assert!(
            transcript_lower.contains(word),
            "expected content word {word:?} in transcript: {transcript_lower:?}"
        );
    }
}
