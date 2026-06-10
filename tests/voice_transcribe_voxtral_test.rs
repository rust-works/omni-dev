//! Voxtral backend end-to-end on the committed 5-minute monologue fixture —
//! the in-tree reproduction of the #930 spike: **batch** offline numbers
//! (#933 Phase 7) and **streaming** numbers (#933 Phase 8). Both share the WER
//! helper, model resolution, and fixture loading.
//!
//! `#[ignore]`-by-default and `--features voxtral`-gated: it needs the ~8.9 GB
//! Voxtral model staged on disk (so it cannot run in CI) and a native build of
//! the engine. Run locally on macOS/Linux with:
//!
//! ```text
//! omni-dev voice install-model --variant voxtral-mini-4b-realtime
//! cargo test --features voxtral --test voice_transcribe_voxtral_test -- --ignored
//! ```
//!
//! Or point at a pre-staged install via `OMNI_DEV_VOICE_VOXTRAL_MODEL`.
//!
//! Asserts: a non-empty transcript with distinctive content words, **offline
//! WER** vs the committed reference ≤ a threshold (measured 4.12 % batch /
//! 3.49 % streaming in-tree, near the spike's ~3 %), the batch event shape
//! (exactly one `Final { revisable: false }` + a trailing `StreamEnd`), and
//! **RTF** (see [`MAX_RTF`]). The streaming test (Phase 8) drives the same
//! fixture "as live" through `transcribe_stream` and asserts first-Partial
//! latency ([`MAX_FIRST_PARTIAL_SECS`]), ≥ N Partials, ≥ 1 SilenceGap endpoint,
//! a terminal StreamEnd, and streaming WER ≤ the same threshold.
//!
//! Perf bars reflect `voxtral.c`'s **BF16** path measured on Apple-Silicon
//! Metal (RTF ≈ 1.25, first-Partial ≈ 2.69 s) — **not** the spike's MLX-INT4
//! numbers (RTF 0.44–0.53), which a heavier BF16 engine does not meet. The
//! streaming test replays at 1× wall-clock and, at RTF > 1, lags behind, so it
//! takes longer than the 5-minute audio (~7–8 min).

#![cfg(all(feature = "voxtral", not(target_os = "windows")))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use futures::StreamExt;
use omni_dev::voice::backends::voxtral::{VoxtralBackend, DEFAULT_VOXTRAL_DELAY_MS};
use omni_dev::voice::models::{ensure_voxtral_model_present, VOXTRAL_MINI_4B};
use omni_dev::voice::transcriber::{
    EndpointKind, StreamingTranscriber, Transcriber, TranscriptEvent, VecAudioInput,
};
use omni_dev::voice::{FileAsyncAudioInput, STREAM_CHUNK_SAMPLES};

/// 16 kHz mono — one sample is 1/16000 s.
const SAMPLE_RATE: f64 = 16_000.0;

/// Maximum tolerated offline word error rate. The spike measured ~3 %; 8 %
/// leaves headroom for text-normalisation and proper-noun spelling differences
/// while still failing loudly on a broken backend.
const MAX_WER: f64 = 0.08;

/// Real-time-factor ceiling.
///
/// The #930 spike's 0.44–0.53 was the **MLX INT4** path; `voxtral.c`'s heavier
/// **BF16** path (no INT4 — ADR-0037) is slower. Measured RTF ≈ 1.25 on
/// Apple-Silicon Metal (in-tree, 2026-06-07); this bound carries headroom for
/// host load. It is **not** real-time (> 1.0) — a known BF16 trade-off, tracked
/// for an INT4 path / further optimisation. Hardware-dependent; opt-in test.
const MAX_RTF: f64 = 1.5;

/// Distinctive words from "A Scandal in Bohemia" that a correct transcript must
/// surface (case-insensitive). Kept to robust, central vocabulary rather than
/// rare proper nouns that the model may spell differently.
const CONTENT_WORDS: &[&str] = &["holmes", "bohemian", "reasoning", "woman"];

/// First-`Partial` latency ceiling. The spike's 0.64–0.91 s was the MLX INT4
/// path; the BF16 `voxtral.c` path measured ≈ 2.69 s here (in-tree, 2026-06-07),
/// so the bound is set above that with headroom (same MLX-vs-BF16 caveat as
/// [`MAX_RTF`]).
const MAX_FIRST_PARTIAL_SECS: f64 = 3.5;

/// Minimum `Partial` events over a 5-minute monologue — a streaming backend
/// must emit hypotheses continuously, not just one final per utterance.
const MIN_PARTIALS: usize = 5;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/voice")
        .join(name)
}

fn resolve_model_dir() -> Option<PathBuf> {
    if let Ok(env) = std::env::var("OMNI_DEV_VOICE_VOXTRAL_MODEL") {
        if !env.is_empty() {
            return Some(PathBuf::from(env));
        }
    }
    VOXTRAL_MINI_4B
        .default_dir()
        .filter(|d| ensure_voxtral_model_present(d).is_ok())
}

/// Lowercases and reduces to alphanumeric/apostrophe word tokens.
fn normalize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '\'' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .map(String::from)
        .collect()
}

/// Word-level Levenshtein distance (substitution/insertion/deletion = 1).
fn word_edit_distance(a: &[String], b: &[String]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0usize; b.len() + 1];
    for (i, wa) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, wb) in b.iter().enumerate() {
            let cost = usize::from(wa != wb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

/// Offline word error rate = edit distance / reference word count.
fn word_error_rate(reference: &str, hypothesis: &str) -> f64 {
    let r = normalize(reference);
    let h = normalize(hypothesis);
    if r.is_empty() {
        return if h.is_empty() { 0.0 } else { 1.0 };
    }
    word_edit_distance(&r, &h) as f64 / r.len() as f64
}

#[test]
#[ignore = "requires the ~8.9 GB Voxtral model; run `omni-dev voice install-model --variant voxtral-mini-4b-realtime` first"]
fn voxtral_batch_reproduces_spike_offline_metrics_on_5min_monologue() {
    let Some(model_dir) = resolve_model_dir() else {
        panic!(
            "Voxtral model not found. Run `omni-dev voice install-model --variant \
             voxtral-mini-4b-realtime` or set OMNI_DEV_VOICE_VOXTRAL_MODEL=<path>."
        );
    };

    // Load the fixture as 16 kHz mono i16 and note its duration for RTF.
    let mut reader =
        hound::WavReader::open(fixture("monologue_5min.wav")).expect("open monologue_5min.wav");
    let samples: Vec<i16> = reader
        .samples::<i16>()
        .collect::<Result<Vec<_>, _>>()
        .expect("decode i16 samples");
    let audio_secs = samples.len() as f64 / SAMPLE_RATE;
    let reference = std::fs::read_to_string(fixture("monologue_5min.expected.txt"))
        .expect("read reference transcript");

    let backend = VoxtralBackend::new(&model_dir, DEFAULT_VOXTRAL_DELAY_MS)
        .expect("VoxtralBackend::new should succeed with a staged model");
    let input = VecAudioInput::from_samples(samples, 16_000); // ~1 s chunks

    let start = Instant::now();
    let events: Vec<TranscriptEvent> = backend
        .transcribe(Box::new(input))
        .expect("transcribe should start")
        .collect::<anyhow::Result<Vec<_>>>()
        .expect("backend should not error mid-stream");
    let elapsed = start.elapsed();

    // Batch event shape: exactly one non-revisable Final, terminal StreamEnd.
    let finals: Vec<(&str, bool)> = events
        .iter()
        .filter_map(|e| match e {
            TranscriptEvent::Final {
                text, revisable, ..
            } => Some((text.as_str(), *revisable)),
            _ => None,
        })
        .collect();
    assert_eq!(finals.len(), 1, "batch backend emits exactly one Final");
    assert!(!finals[0].1, "batch Final must not be revisable");
    assert!(
        matches!(
            events.last(),
            Some(TranscriptEvent::Endpoint {
                kind: omni_dev::voice::transcriber::EndpointKind::StreamEnd,
                ..
            })
        ),
        "last event must be a StreamEnd endpoint, got {:?}",
        events.last()
    );

    let transcript = finals[0].0;
    assert!(
        !transcript.trim().is_empty(),
        "transcript must be non-empty"
    );

    let lower = transcript.to_lowercase();
    for word in CONTENT_WORDS {
        assert!(
            lower.contains(word),
            "expected content word {word:?} in transcript: {transcript:?}"
        );
    }

    let wer = word_error_rate(&reference, transcript);
    let rtf = elapsed.as_secs_f64() / audio_secs;
    eprintln!(
        "voxtral batch 5-min: WER={wer:.4} (max {MAX_WER}), RTF={rtf:.3} (max {MAX_RTF}), \
         {audio_secs:.0}s audio in {:.1}s",
        elapsed.as_secs_f64()
    );
    assert!(wer <= MAX_WER, "offline WER {wer:.4} exceeds {MAX_WER}");
    assert!(
        rtf < MAX_RTF,
        "RTF {rtf:.3} exceeds {MAX_RTF} (hardware-dependent)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires the ~8.9 GB Voxtral model and replays 5 min at 1x (~5 min); run `omni-dev voice install-model --variant voxtral-mini-4b-realtime` first"]
async fn voxtral_streaming_reproduces_spike_metrics_on_5min_monologue() {
    let Some(model_dir) = resolve_model_dir() else {
        panic!(
            "Voxtral model not found. Run `omni-dev voice install-model --variant \
             voxtral-mini-4b-realtime` or set OMNI_DEV_VOICE_VOXTRAL_MODEL=<path>."
        );
    };
    let reference = std::fs::read_to_string(fixture("monologue_5min.expected.txt"))
        .expect("read reference transcript");

    let backend = VoxtralBackend::new(&model_dir, DEFAULT_VOXTRAL_DELAY_MS)
        .expect("VoxtralBackend::new should succeed with a staged model");
    // `realtime = true`: replay the fixture on the wall clock, the way a live
    // mic would, so first-Partial latency is meaningful.
    let audio = FileAsyncAudioInput::from_wav_path(
        fixture("monologue_5min.wav"),
        STREAM_CHUNK_SAMPLES,
        true,
    )
    .expect("load fixture as async audio input");

    let start = Instant::now();
    let mut stream = backend.transcribe_stream(Box::new(audio));

    let mut first_partial_at: Option<Duration> = None;
    let mut partials = 0usize;
    let mut silence_gaps = 0usize;
    let mut saw_stream_end = false;
    let mut finals: Vec<String> = Vec::new();

    while let Some(event) = stream.next().await {
        match event.expect("streaming backend should not error mid-stream") {
            TranscriptEvent::Partial { .. } => {
                if first_partial_at.is_none() {
                    first_partial_at = Some(start.elapsed());
                }
                partials += 1;
            }
            TranscriptEvent::Final { text, .. } => finals.push(text),
            TranscriptEvent::Endpoint { kind, .. } => match kind {
                EndpointKind::SilenceGap => silence_gaps += 1,
                EndpointKind::StreamEnd => saw_stream_end = true,
                EndpointKind::UtteranceEnd => {}
            },
        }
    }

    let first = first_partial_at.expect("expected at least one Partial event");
    let transcript = finals.join(" ");
    let wer = word_error_rate(&reference, &transcript);
    eprintln!(
        "voxtral streaming 5-min: first_partial={:.3}s (max {MAX_FIRST_PARTIAL_SECS}), \
         partials={partials} (min {MIN_PARTIALS}), silence_gaps={silence_gaps}, \
         WER={wer:.4} (max {MAX_WER})",
        first.as_secs_f64()
    );

    assert!(
        first.as_secs_f64() < MAX_FIRST_PARTIAL_SECS,
        "first-Partial latency {:.3}s exceeds {MAX_FIRST_PARTIAL_SECS}s",
        first.as_secs_f64()
    );
    assert!(
        partials >= MIN_PARTIALS,
        "only {partials} Partials (min {MIN_PARTIALS})"
    );
    assert!(
        silence_gaps >= 1,
        "expected ≥1 SilenceGap endpoint, got {silence_gaps}"
    );
    assert!(
        saw_stream_end,
        "stream must terminate with a StreamEnd endpoint"
    );
    assert!(
        !transcript.trim().is_empty(),
        "streaming transcript must be non-empty"
    );
    assert!(wer <= MAX_WER, "streaming WER {wer:.4} exceeds {MAX_WER}");
}

#[cfg(test)]
mod wer_unit {
    //! The WER helper itself is pure and *does* run in CI (under --features
    //! voxtral) — it needs no model.
    use super::{normalize, word_error_rate};

    #[test]
    fn wer_is_zero_for_identical_text() {
        assert!(word_error_rate("The quick brown fox", "the quick, brown fox!") < 1e-9);
    }

    #[test]
    fn wer_counts_substitution_insertion_deletion() {
        // ref: a b c d (4 words); hyp: a x c (sub b→x, delete d) → 2/4 = 0.5
        assert!((word_error_rate("a b c d", "a x c") - 0.5).abs() < 1e-9);
    }

    #[test]
    fn normalize_strips_punctuation_and_lowercases() {
        assert_eq!(
            normalize("Holmes, the BOHEMIAN!"),
            vec!["holmes", "the", "bohemian"]
        );
    }
}
