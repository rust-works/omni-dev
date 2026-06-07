//! Voxtral **batch** backend end-to-end on the committed 5-minute monologue
//! fixture — #933 Phase 7 (in-tree reproduction of the #930 spike's *offline*
//! numbers).
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
//! Asserts the spike's batch-validatable bars: a non-empty transcript with
//! distinctive content words, **offline WER** vs the committed reference ≤ a
//! threshold (the spike measured ~2.84–3.15 %; the bound carries headroom for
//! normalisation/proper-noun variance), the batch event shape (exactly one
//! `Final { revisable: false }` + a trailing `StreamEnd`), and **RTF** < 0.6.
//! The streaming bars (first-Partial, Partials, SilenceGap) are #933 Phase 8.

#![cfg(all(feature = "voxtral", not(target_os = "windows")))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::Instant;

use omni_dev::voice::backends::voxtral::{VoxtralBackend, DEFAULT_VOXTRAL_DELAY_MS};
use omni_dev::voice::models::{ensure_voxtral_model_present, VOXTRAL_MINI_4B};
use omni_dev::voice::transcriber::{Transcriber, TranscriptEvent, VecAudioInput};

/// 16 kHz mono — one sample is 1/16000 s.
const SAMPLE_RATE: f64 = 16_000.0;

/// Maximum tolerated offline word error rate. The spike measured ~3 %; 8 %
/// leaves headroom for text-normalisation and proper-noun spelling differences
/// while still failing loudly on a broken backend.
const MAX_WER: f64 = 0.08;

/// Real-time-factor ceiling from the spike (it measured 0.44–0.53 on Apple
/// Silicon). Hardware-dependent — this test is opt-in and run on capable hosts.
const MAX_RTF: f64 = 0.6;

/// Distinctive words from "A Scandal in Bohemia" that a correct transcript must
/// surface (case-insensitive). Kept to robust, central vocabulary rather than
/// rare proper nouns that the model may spell differently.
const CONTENT_WORDS: &[&str] = &["holmes", "bohemian", "reasoning", "woman"];

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
