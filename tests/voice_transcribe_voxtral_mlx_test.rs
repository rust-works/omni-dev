//! Model-gated integration tests for the real-time INT4 Voxtral **MLX** backend
//! (`--backend voxtral-mlx`, ADR-0039 / #933 M3b).
//!
//! `#[ignore]`-by-default and gated to `--features voxtral-mlx` on macOS Apple
//! Silicon: they need the ~2.6 GB INT4 model (`voice install-model --variant
//! voxtral-mlx-int4`, or `OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL=<dir>`). The batch test
//! checks the `Transcriber` event shape + a correct transcript; the streaming
//! test drives `short_en.wav` "as live" through `transcribe_stream` and asserts
//! first-Partial latency, ≥ 1 Partial, the terminal `Final` + `StreamEnd`, and a
//! correct transcript — so streaming reproduces the batch result (validated
//! byte-identical in the unit tests; here through the full async event path).
//!
//! **Run single-threaded** (`--test-threads=1`): MLX drives a *process-global*
//! Metal device, so two independent inferences running concurrently (the batch
//! and streaming tests in parallel) crash. Normal usage runs one backend per
//! command, so this only affects this test file.

#![cfg(all(feature = "voxtral-mlx", target_os = "macos", target_arch = "aarch64"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use omni_dev::voice::backends::voxtral_mlx::{VoxtralMlxBackend, DEFAULT_VOXTRAL_MLX_DELAY_MS};
use omni_dev::voice::models::{ensure_voxtral_mlx_model_present, VOXTRAL_MLX_INT4};
use omni_dev::voice::transcriber::{
    EndpointKind, StreamingTranscriber, Transcriber, TranscriptEvent, VecAudioInput,
};
use omni_dev::voice::{FileAsyncAudioInput, STREAM_CHUNK_SAMPLES};

/// Words known to appear in the `short_en.wav` transcript ("Dark wizards cannot
/// keep their tempers … of the species … learns to rely on it.").
const EXPECTED_WORDS: &[&str] = &["wizards", "tempers", "species", "rely"];

/// First-`Partial` latency ceiling (s). INT4/MLX is ≈ 5× real-time, so a Partial
/// should land quickly once enough audio + the decoder delay window have arrived.
const MAX_FIRST_PARTIAL_SECS: f64 = 4.0;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/voice")
        .join(name)
}

/// Resolves the INT4 MLX model dir (env var → default), or `None` to skip.
fn resolve_model_dir() -> Option<PathBuf> {
    let dir = VOXTRAL_MLX_INT4.resolve_dir(None).ok()?;
    ensure_voxtral_mlx_model_present(&dir).ok().map(|()| dir)
}

fn assert_has_expected_words(transcript: &str) {
    let lower = transcript.to_lowercase();
    for word in EXPECTED_WORDS {
        assert!(
            lower.contains(word),
            "transcript missing {word:?}: {transcript:?}"
        );
    }
}

#[test]
#[ignore = "requires the INT4 Voxtral MLX model; run `omni-dev voice install-model --variant voxtral-mlx-int4` first"]
fn voxtral_mlx_batch_transcribes_short_en() {
    let Some(model_dir) = resolve_model_dir() else {
        eprintln!("skipping: no voxtral-mlx-int4 model installed");
        return;
    };

    let backend = VoxtralMlxBackend::new(&model_dir, DEFAULT_VOXTRAL_MLX_DELAY_MS)
        .expect("construct backend");
    let audio = VecAudioInput::from_wav_path(fixture("short_en.wav"), STREAM_CHUNK_SAMPLES)
        .expect("load short_en.wav");
    let events: Vec<_> = backend
        .transcribe(Box::new(audio))
        .expect("transcribe")
        .collect::<anyhow::Result<Vec<_>>>()
        .expect("event stream");

    // Exactly one non-revisable Final, terminal StreamEnd.
    let finals: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            TranscriptEvent::Final {
                text, revisable, ..
            } => Some((text.clone(), *revisable)),
            _ => None,
        })
        .collect();
    assert_eq!(finals.len(), 1, "batch backend emits exactly one Final");
    assert!(!finals[0].1, "batch Final must not be revisable");
    assert!(
        matches!(
            events.last(),
            Some(TranscriptEvent::Endpoint {
                kind: EndpointKind::StreamEnd,
                ..
            })
        ),
        "stream must end with a StreamEnd endpoint"
    );
    assert_has_expected_words(&finals[0].0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the INT4 Voxtral MLX model; run `omni-dev voice install-model --variant voxtral-mlx-int4` first"]
async fn voxtral_mlx_streaming_emits_partials_and_correct_transcript() {
    let Some(model_dir) = resolve_model_dir() else {
        eprintln!("skipping: no voxtral-mlx-int4 model installed");
        return;
    };

    let backend = VoxtralMlxBackend::new(&model_dir, DEFAULT_VOXTRAL_MLX_DELAY_MS)
        .expect("construct backend");
    // Replay "as live" (realtime: true) so first-Partial latency is meaningful.
    let audio =
        FileAsyncAudioInput::from_wav_path(fixture("short_en.wav"), STREAM_CHUNK_SAMPLES, true)
            .expect("load short_en.wav");

    let start = std::time::Instant::now();
    let mut stream = backend.transcribe_stream(Box::new(audio));

    let mut partials = 0usize;
    let mut first_partial_at: Option<Duration> = None;
    let mut finals: Vec<String> = Vec::new();
    let mut saw_stream_end = false;

    while let Some(ev) = stream.next().await {
        match ev.expect("stream event") {
            TranscriptEvent::Partial { .. } => {
                partials += 1;
                first_partial_at.get_or_insert_with(|| start.elapsed());
            }
            TranscriptEvent::Final { text, .. } => finals.push(text),
            TranscriptEvent::Endpoint { kind, .. } => {
                if kind == EndpointKind::StreamEnd {
                    saw_stream_end = true;
                }
            }
        }
    }

    assert!(partials >= 1, "expected at least one Partial");
    assert!(saw_stream_end, "expected a terminal StreamEnd");
    let first = first_partial_at.expect("first Partial latency");
    assert!(
        first.as_secs_f64() < MAX_FIRST_PARTIAL_SECS,
        "first Partial at {:.2}s exceeds {MAX_FIRST_PARTIAL_SECS}s",
        first.as_secs_f64()
    );

    let transcript = finals.join(" ");
    assert!(
        !transcript.trim().is_empty(),
        "streaming produced no Final text"
    );
    assert_has_expected_words(&transcript);
}
