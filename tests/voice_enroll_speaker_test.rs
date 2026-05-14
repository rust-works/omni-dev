//! Wespeaker speaker-embedding backend end-to-end against
//! `tests/fixtures/voice/two_speakers.wav`.
//!
//! `#[ignore]`-by-default because the test needs the wespeaker ONNX
//! model file staged on disk. Run locally with:
//!
//! ```text
//! omni-dev voice install-model --variant speaker-wespeaker-en
//! cargo test --test voice_enroll_speaker_test -- --ignored
//! ```
//!
//! Or point at a pre-staged install via `OMNI_DEV_VOICE_SPEAKER_MODEL`
//! (the intended hook for the CI cache once the runner-side caching
//! lands).
//!
//! The test re-runs the same separability check the #805 spike did
//! (within-speaker mean ≈ 0.91, cross-speaker mean ≈ 0.07 on this
//! fixture). The gates here are conservative — generous slack on both
//! sides of the spike's measured numbers — so the test is a load-
//! bearing regression guard, not a precision benchmark.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use omni_dev::voice::models::SPEAKER_WESPEAKER_EN;
use omni_dev::voice::{cosine, WespeakerEmbedder};

/// Spike-aligned gates from
/// `SPIKE.md` on `issue-805-spike-tract-speaker`.
const WITHIN_MIN: f32 = 0.70;
const CROSS_MAX: f32 = 0.40;

fn fixture_wav() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/voice/two_speakers.wav")
}

fn resolve_speaker_model_path() -> Option<PathBuf> {
    if let Ok(env) = std::env::var("OMNI_DEV_VOICE_SPEAKER_MODEL") {
        if !env.is_empty() {
            let p = PathBuf::from(env);
            // The env var conventionally points at a directory.
            if p.is_dir() {
                let onnx = p.join(SPEAKER_WESPEAKER_EN.required_files[0]);
                if onnx.is_file() {
                    return Some(onnx);
                }
            } else if p.is_file() {
                return Some(p);
            }
        }
    }
    let dir = SPEAKER_WESPEAKER_EN.default_dir()?;
    let onnx = dir.join(SPEAKER_WESPEAKER_EN.required_files[0]);
    if onnx.is_file() {
        Some(onnx)
    } else {
        None
    }
}

fn read_pcm(path: &Path) -> Vec<i16> {
    let mut reader = hound::WavReader::open(path).expect("open two_speakers.wav");
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 16_000);
    assert_eq!(spec.channels, 1);
    assert_eq!(spec.bits_per_sample, 16);
    reader
        .samples::<i16>()
        .collect::<Result<Vec<_>, _>>()
        .expect("decode PCM")
}

fn slice(pcm: &[i16], start_s: f64, end_s: f64) -> &[i16] {
    let s = (start_s * 16_000.0) as usize;
    let e = (end_s * 16_000.0) as usize;
    &pcm[s..e.min(pcm.len())]
}

#[test]
#[ignore = "requires wespeaker ONNX on disk; run `omni-dev voice install-model --variant speaker-wespeaker-en` first"]
fn wespeaker_separates_speakers_in_two_speakers_fixture() {
    let Some(model_path) = resolve_speaker_model_path() else {
        panic!(
            "wespeaker model not found. Run \
             `omni-dev voice install-model --variant speaker-wespeaker-en` or set \
             OMNI_DEV_VOICE_SPEAKER_MODEL=<dir> to point at a pre-staged install."
        );
    };

    let embedder = WespeakerEmbedder::new(&model_path).expect("WespeakerEmbedder::new");
    let pcm = read_pcm(&fixture_wav());
    assert!(
        pcm.len() >= 24 * 16_000,
        "expected ≥24 s fixture, got {} samples",
        pcm.len()
    );

    // Same window plan as the spike: two windows per speaker, skipping the
    // leading silence and the silence gap.
    let a1 = embedder.embed(slice(&pcm, 1.0, 7.0)).expect("embed A1");
    let a2 = embedder.embed(slice(&pcm, 6.0, 12.0)).expect("embed A2");
    let b1 = embedder.embed(slice(&pcm, 13.5, 19.5)).expect("embed B1");
    let b2 = embedder.embed(slice(&pcm, 18.5, 24.5)).expect("embed B2");

    let aa = cosine(&a1, &a2);
    let bb = cosine(&b1, &b2);
    let ab11 = cosine(&a1, &b1);
    let ab12 = cosine(&a1, &b2);
    let ab21 = cosine(&a2, &b1);
    let ab22 = cosine(&a2, &b2);

    eprintln!(
        "within-speaker:  sim(A1,A2)={aa:.4}  sim(B1,B2)={bb:.4}\n\
         cross-speaker:   sim(A1,B1)={ab11:.4}  sim(A1,B2)={ab12:.4}  \
                          sim(A2,B1)={ab21:.4}  sim(A2,B2)={ab22:.4}"
    );

    assert!(
        aa >= WITHIN_MIN && bb >= WITHIN_MIN,
        "within-speaker cosines must be >= {WITHIN_MIN}; got A={aa:.4}, B={bb:.4}"
    );
    let cross_max = ab11.max(ab12).max(ab21).max(ab22);
    assert!(
        cross_max <= CROSS_MAX,
        "cross-speaker cosines must all be <= {CROSS_MAX}; max was {cross_max:.4}"
    );
}

#[test]
#[ignore = "requires wespeaker ONNX on disk; run `omni-dev voice install-model --variant speaker-wespeaker-en` first"]
fn wespeaker_default_threshold_picks_correct_speaker() {
    let Some(model_path) = resolve_speaker_model_path() else {
        panic!(
            "wespeaker model not found. Run \
             `omni-dev voice install-model --variant speaker-wespeaker-en` first."
        );
    };

    let embedder = WespeakerEmbedder::new(&model_path).expect("WespeakerEmbedder::new");
    let pcm = read_pcm(&fixture_wav());

    // Enroll on the first half of speaker A; verify the *second* half of
    // speaker A clears the 0.5 threshold and both halves of speaker B
    // fall below it. Mirrors the `voice transcribe --speaker` runtime
    // path without depending on the Whisper ASR backend being installed.
    const THRESHOLD: f32 = 0.5;
    let enrolled = embedder.embed(slice(&pcm, 1.0, 7.0)).expect("embed enroll");
    let a_query = embedder
        .embed(slice(&pcm, 6.0, 12.0))
        .expect("embed a_query");
    let b_query_early = embedder
        .embed(slice(&pcm, 13.5, 19.5))
        .expect("embed b_query_early");
    let b_query_late = embedder
        .embed(slice(&pcm, 18.5, 24.5))
        .expect("embed b_query_late");

    assert!(
        cosine(&enrolled, &a_query) >= THRESHOLD,
        "same-speaker cosine should clear default threshold"
    );
    assert!(
        cosine(&enrolled, &b_query_early) < THRESHOLD,
        "other-speaker cosine must fall below default threshold (early window)"
    );
    assert!(
        cosine(&enrolled, &b_query_late) < THRESHOLD,
        "other-speaker cosine must fall below default threshold (late window)"
    );
}
