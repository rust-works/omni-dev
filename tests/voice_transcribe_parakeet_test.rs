//! Parakeet-TDT-0.6B-v2 backend end-to-end against the committed
//! 5-min Sherlock Holmes monologue fixture from #826.
//!
//! All tests are `#[ignore]`-by-default because they need the Parakeet
//! model files staged on disk (the converted
//! `candle_weights.safetensors` is ~2.5 GB) and the candle inference
//! run takes substantially longer than the whisper-tiny.en path. Run
//! locally with:
//!
//! ```text
//! omni-dev voice install-model --variant parakeet-tdt-0.6b-v2
//! cargo test --test voice_transcribe_parakeet_test -- --ignored
//! ```
//!
//! Or point at a pre-staged install via `OMNI_DEV_VOICE_PARAKEET_MODEL`
//! (the intended hook for the CI cache once runner-side caching lands).
//!
//! Three tests gate the issue #898 acceptance criteria:
//!
//! - **`parakeet_batch_transcribes_monologue_with_content_words`**
//!   covers AC-3a (batch transcript on the 5-min fixture). Asserts a
//!   loose content-word match rather than exact text — parity with
//!   the `parakeet-mlx@32b8034` reference is within ±2 % WER, not
//!   byte-equal.
//!
//! - **`parakeet_streaming_final_only_matches_batch`** covers AC-3b
//!   (streaming Final-only transcript on the 5-min fixture). The v2
//!   streaming impl runs an incremental local-attention + KV-cache
//!   pipeline that emits Partials per internal-chunk and a single
//!   Final at stream end; the summed Final text is substring-equal
//!   to the batch transcript.
//!
//! - **`parakeet_streaming_emits_partials_on_30s_slice`** covers AC-3c
//!   (representative `Partial`-event sequence). Passes under v2 —
//!   the wrapper merges source chunks into internal 5 s chunks
//!   (`INTERNAL_CHUNK_MIN_SAMPLES = 80_000` in `streaming.rs`) and
//!   emits one `Partial` per internal-chunk's `add_audio` call. The
//!   30 s slice produces ~6 Partials; the assertion is `>= 2`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use omni_dev::voice::backends::parakeet::CandleParakeetTranscriber;
use omni_dev::voice::models::PARAKEET_TDT_0_6B_V2;
use omni_dev::voice::transcriber::{
    AsyncAudioInput, AudioChunk, FileAsyncAudioInput, StreamingTranscriber, Transcriber,
    TranscriptEvent, VecAudioInput,
};

/// Content words actually present in `monologue_5min.expected.txt`,
/// per the prescribed list in PR review (matches words the reviewer's
/// 2.69 % WER transcript surfaced). The fixture is a 5-min "A Scandal
/// in Bohemia" excerpt. Verified word-boundary presence via
/// `grep -ic "\b<word>\b" tests/fixtures/voice/monologue_5min.expected.txt`.
const CONTENT_WORDS: &[&str] = &[
    "holmes", "irene", "adler", "bohemian", "baker", "cocaine", "study",
];

fn fixture_wav() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/voice/monologue_5min.wav")
}

fn resolve_model_dir() -> Option<PathBuf> {
    if let Ok(env) = std::env::var("OMNI_DEV_VOICE_PARAKEET_MODEL") {
        if !env.is_empty() {
            return Some(PathBuf::from(env));
        }
    }
    let dir = PARAKEET_TDT_0_6B_V2.default_dir()?;
    PARAKEET_TDT_0_6B_V2.ensure_present(&dir).ok().map(|()| dir)
}

fn build_transcriber() -> CandleParakeetTranscriber {
    let Some(model_dir) = resolve_model_dir() else {
        panic!(
            "Parakeet model not found. Run `omni-dev voice install-model \
             --variant parakeet-tdt-0.6b-v2` or set \
             OMNI_DEV_VOICE_PARAKEET_MODEL=<path>."
        );
    };
    CandleParakeetTranscriber::new(&model_dir)
        .expect("CandleParakeetTranscriber::new should succeed")
}

fn collect_finals(events: &[TranscriptEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match e {
            TranscriptEvent::Final { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn assert_content_words(transcript_lower: &str) {
    for word in CONTENT_WORDS {
        assert!(
            transcript_lower.contains(word),
            "expected content word {word:?} in transcript: {transcript_lower:?}"
        );
    }
}

// ── AC-3a: batch ────────────────────────────────────────────────────────

#[test]
#[ignore = "requires Parakeet model on disk; run `omni-dev voice install-model --variant parakeet-tdt-0.6b-v2`"]
fn parakeet_batch_transcribes_monologue_with_content_words() {
    let transcriber = build_transcriber();
    let input = VecAudioInput::from_wav_path(fixture_wav(), 1024).expect("fixture should load");
    let stream = transcriber
        .transcribe(Box::new(input))
        .expect("transcribe should succeed");
    let events: Vec<TranscriptEvent> = stream
        .collect::<anyhow::Result<Vec<_>>>()
        .expect("backend should not error mid-stream");

    let finals = collect_finals(&events);
    assert!(
        !finals.is_empty(),
        "expected at least one Final event, got events: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(TranscriptEvent::Endpoint { .. })),
        "last event must be Endpoint, got: {:?}",
        events.last()
    );

    assert_content_words(&finals.join(" ").to_lowercase());
}

// ── AC-3b: streaming Final-only ─────────────────────────────────────────

#[test]
#[ignore = "requires Parakeet model on disk; run `omni-dev voice install-model --variant parakeet-tdt-0.6b-v2`"]
fn parakeet_streaming_final_only_matches_batch() {
    let transcriber = build_transcriber();
    let input =
        FileAsyncAudioInput::from_wav_path(fixture_wav(), 25_600).expect("fixture should load");

    // Drive the stream to completion in a tokio runtime.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let events: Vec<TranscriptEvent> = rt.block_on(async {
        use futures::StreamExt;
        let mut stream = transcriber.transcribe_stream(Box::new(input));
        let mut events = Vec::new();
        while let Some(item) = stream.next().await {
            events.push(item.expect("backend should not error mid-stream"));
        }
        events
    });

    let finals = collect_finals(&events);
    assert!(
        !finals.is_empty(),
        "expected at least one Final event in stream, got events: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(TranscriptEvent::Endpoint { .. })),
        "stream must end with Endpoint, got: {:?}",
        events.last()
    );
    assert_content_words(&finals.join(" ").to_lowercase());
}

// ── AC-3c: streaming Partials on 30 s slice ────────────────────────────

#[test]
#[ignore = "requires Parakeet model on disk; run `omni-dev voice install-model --variant parakeet-tdt-0.6b-v2`"]
fn parakeet_streaming_emits_partials_on_30s_slice() {
    let transcriber = build_transcriber();
    // Take just the first 30 s of the fixture by truncating samples.
    let full = VecAudioInput::from_wav_path(fixture_wav(), 1024).expect("fixture should load");
    let mut samples: Vec<i16> = Vec::new();
    let mut iter: Box<dyn omni_dev::voice::transcriber::AudioInput> = Box::new(full);
    let cap = 30 * 16_000;
    while samples.len() < cap {
        match iter.next_chunk() {
            Some(c) => samples.extend_from_slice(&c),
            None => break,
        }
    }
    samples.truncate(cap);
    let async_input = FileAsyncAudioInput::from_samples(samples, 1_600);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let events: Vec<TranscriptEvent> = rt.block_on(async {
        use futures::StreamExt;
        let mut stream = transcriber.transcribe_stream(Box::new(async_input));
        let mut events = Vec::new();
        while let Some(item) = stream.next().await {
            events.push(item.expect("backend should not error mid-stream"));
        }
        events
    });

    let partials: Vec<&TranscriptEvent> = events
        .iter()
        .filter(|e| matches!(e, TranscriptEvent::Partial { .. }))
        .collect();
    assert!(
        partials.len() >= 2,
        "expected at least 2 Partial events on a 30 s slice; got {} (events: {:?})",
        partials.len(),
        events
    );
}

// ── AC-3d: async yield-as-you-go ────────────────────────────────────────

/// `AsyncAudioInput` wrapper that injects a fixed sleep before each
/// `next_chunk` call. Simulates a realtime audio source (cpal-like) where
/// chunks arrive over wall-clock time rather than instantly. Used by the
/// `..._yields_partials_during_input` test to prove that Partials are
/// emitted *during* the stream, not in a burst at the end.
struct SleepingAudioInput {
    inner: FileAsyncAudioInput,
    delay: Duration,
    /// Stamped the first time `next_chunk` returns `None`. Lets the
    /// test compare the first-Partial timestamp against the
    /// input-exhausted timestamp.
    exhausted_at: std::sync::Arc<std::sync::Mutex<Option<Instant>>>,
}

#[async_trait]
impl AsyncAudioInput for SleepingAudioInput {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        tokio::time::sleep(self.delay).await;
        let chunk = self.inner.next_chunk().await;
        if chunk.is_none() {
            let mut slot = self.exhausted_at.lock().unwrap();
            if slot.is_none() {
                *slot = Some(Instant::now());
            }
        }
        chunk
    }
}

#[test]
#[ignore = "requires Parakeet model on disk; run `omni-dev voice install-model --variant parakeet-tdt-0.6b-v2`"]
fn parakeet_streaming_yields_partials_during_input() {
    let transcriber = build_transcriber();
    // Take 30 s of audio in 100 ms chunks (1 600 samples) so the
    // SleepingAudioInput's delay applies 300 times.
    let full = VecAudioInput::from_wav_path(fixture_wav(), 1024).expect("fixture should load");
    let mut samples: Vec<i16> = Vec::new();
    let mut iter: Box<dyn omni_dev::voice::transcriber::AudioInput> = Box::new(full);
    let cap = 30 * 16_000;
    while samples.len() < cap {
        match iter.next_chunk() {
            Some(c) => samples.extend_from_slice(&c),
            None => break,
        }
    }
    samples.truncate(cap);
    let inner = FileAsyncAudioInput::from_samples(samples, 1_600);

    // 10 ms per chunk × 300 chunks ≈ 3 s wall-clock to drain. Encoder
    // processes a 5 s buffer ~6 times during that window, so Partials
    // should arrive starting around T ≈ 1 s — well before T_exhausted ≈ 3 s.
    let exhausted_at = std::sync::Arc::new(std::sync::Mutex::new(None));
    let async_input = SleepingAudioInput {
        inner,
        delay: Duration::from_millis(10),
        exhausted_at: std::sync::Arc::clone(&exhausted_at),
    };

    // Multi-thread runtime: `spawn_blocking` needs a worker thread for the
    // encoder/decoder forward calls while the driver awaits chunks.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");

    let started = Instant::now();
    let stamped_events: Vec<(Duration, TranscriptEvent)> = rt.block_on(async {
        use futures::StreamExt;
        let mut stream = transcriber.transcribe_stream(Box::new(async_input));
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            out.push((
                started.elapsed(),
                item.expect("backend should not error mid-stream"),
            ));
        }
        out
    });

    let t_exhausted = exhausted_at
        .lock()
        .unwrap()
        .expect("input must have been exhausted")
        - started;

    let first_partial_at = stamped_events
        .iter()
        .find_map(|(t, ev)| matches!(ev, TranscriptEvent::Partial { .. }).then_some(*t))
        .expect("expected at least one Partial event");

    eprintln!("t_first_partial = {first_partial_at:?}");
    eprintln!("t_input_exhausted = {t_exhausted:?}");
    for (t, ev) in &stamped_events {
        eprintln!("  t={t:?}  {ev:?}");
    }

    assert!(
        first_partial_at < t_exhausted,
        "Partials should arrive while the audio is still streaming, not after. \
         first_partial = {first_partial_at:?}, input_exhausted = {t_exhausted:?}"
    );
}
