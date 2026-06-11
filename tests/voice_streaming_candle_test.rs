//! `whisper-candle-streaming` backend end-to-end against the 5-minute
//! monologue fixture — the #974 LCD-envelope gates, with the #969 spike
//! numbers as the no-regression baseline (RTF 0.34 / WER 9.2 % /
//! time-to-final 0.73/1.42 s mean/max / peak RSS ~429 MB / partial-latency
//! P95 2.41 s, Apple-Silicon).
//!
//! `#[ignore]`-by-default because the tests need the Whisper tiny.en model
//! staged on disk and minutes of CPU. **Run under `--release`** — the RTF
//! gates assume an optimized build (debug candle inference is an order of
//! magnitude slower):
//!
//! ```text
//! omni-dev voice install-model
//! cargo test --release --test voice_streaming_candle_test -- --ignored --nocapture
//! ```
//!
//! Or point at a pre-staged install via `OMNI_DEV_VOICE_WHISPER_MODEL`.
//!
//! The paced test replays the fixture at 1× wall-clock via a
//! **deadline-based** driver ([`PacedAudioInput`]): chunk `i` is released
//! at `(i+1) × 100 ms` from stream start, with no sleep when inference
//! made the consumer late (catch-up). Pacing lives in the test driver, not
//! the transcriber — the #969/#826 spikes showed a naïve fixed-sleep
//! driver accumulates lag unbounded even at RTF < 1.
//!
//! Peak RSS is reported on Linux (`/proc/self/status` `VmHWM`, no `unsafe`
//! needed) and additionally **gated** at ≤ 500 MB when
//! `OMNI_DEV_STREAMING_RSS_GATE=1` — set by the CI keep-up workflow, which
//! runs this binary in isolation so the process high-water mark belongs to
//! one pipeline rather than several tests sharing the process.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::cast_precision_loss)] // metric math on sample counts well below 2^52

use std::path::PathBuf;
use std::time::{Duration, Instant};

use omni_dev::voice::backends::candle_streaming::{CandleStreamingTranscriber, StreamingConfig};
use omni_dev::voice::det::CountingUlidRng;
use omni_dev::voice::models::{default_whisper_model_dir, ensure_model_present};
use omni_dev::voice::transcriber::{
    AudioChunk, AudioInput, EndpointKind, Transcriber, TranscriptEvent, VecAudioInput,
};

/// 100 ms of 16 kHz audio — the chunk size the #969 envelope was measured at.
const CHUNK_SAMPLES: usize = 1600;

/// Silence budget of [`StreamingConfig::default`], used to convert an
/// endpoint timestamp back to the end-of-utterance it detected.
const DEFAULT_SILENCE_SECS: f64 = 0.3;

fn fixture_wav() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/voice/monologue_5min.wav")
}

fn fixture_expected() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/voice/monologue_5min.expected.txt")
}

fn fixture_words() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/voice/monologue_5min.words.jsonl")
}

fn resolve_model_dir() -> Option<PathBuf> {
    if let Ok(env) = std::env::var("OMNI_DEV_VOICE_WHISPER_MODEL") {
        if !env.is_empty() {
            return Some(PathBuf::from(env));
        }
    }
    default_whisper_model_dir().filter(|d| ensure_model_present(d).is_ok())
}

fn require_model_dir() -> PathBuf {
    resolve_model_dir().expect(
        "Whisper model not found. Run `omni-dev voice install-model` or set \
         OMNI_DEV_VOICE_WHISPER_MODEL=<path> to point at a pre-staged install.",
    )
}

/// One ground-truth word from `monologue_5min.words.jsonl` (forced
/// alignment from #969; latency ground truth, not a transcription
/// reference).
#[derive(Debug, serde::Deserialize)]
struct RefWord {
    word: String,
    #[allow(dead_code)]
    start_s: f64,
    end_s: f64,
}

fn load_ref_words() -> Vec<RefWord> {
    std::fs::read_to_string(fixture_words())
        .expect("words.jsonl fixture should exist")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("words.jsonl line should parse"))
        .collect()
}

/// Joins every committed `Final` text in stream order.
fn committed_transcript(events: &[TranscriptEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            TranscriptEvent::Final { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Deadline-based 1× pacing wrapper: chunk `i` is released at wall-clock
/// `(i+1) × 100 ms` measured from the first pull. If inference made the
/// consumer late, no sleep happens (catch-up) — lag stays bounded while
/// during-speech RTF < 1.
struct PacedAudioInput {
    inner: VecAudioInput,
    start: Option<Instant>,
    chunk_idx: u32,
    chunk_duration: Duration,
}

impl PacedAudioInput {
    fn new(inner: VecAudioInput) -> Self {
        Self {
            inner,
            start: None,
            chunk_idx: 0,
            chunk_duration: Duration::from_millis(100),
        }
    }
}

impl AudioInput for PacedAudioInput {
    fn next_chunk(&mut self) -> Option<AudioChunk> {
        let start = *self.start.get_or_insert_with(Instant::now);
        let deadline = self.chunk_duration * (self.chunk_idx + 1);
        let elapsed = start.elapsed();
        if elapsed < deadline {
            std::thread::sleep(deadline.saturating_sub(elapsed));
        }
        self.chunk_idx += 1;
        self.inner.next_chunk()
    }
}

/// Forward-only alignment of hypothesis words to reference words with a
/// small lookahead, tolerating substitutions/deletions. Returns
/// `(hyp_index, ref_index)` pairs.
fn match_words(hyp: &[String], reference: &[String]) -> Vec<(usize, usize)> {
    const LOOKAHEAD: usize = 5;
    let mut matches = Vec::new();
    let mut j = 0usize;
    for (i, word) in hyp.iter().enumerate() {
        let limit = (j + LOOKAHEAD).min(reference.len());
        if let Some(k) = (j..limit).find(|&k| &reference[k] == word) {
            matches.push((i, k));
            j = k + 1;
        }
        if j >= reference.len() {
            break;
        }
    }
    matches
}

fn normalise_words(text: &str) -> Vec<String> {
    wer::tokenize(text)
}

/// Peak RSS in bytes from `/proc/self/status` (`VmHWM`), Linux only.
fn peak_rss_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

/// Fast smoke test over the real backend on the 11.7 s `short_en.wav`
/// fixture — this is the gated test the **coverage CI job** runs (the
/// 5-minute envelope tests stay manual / keep-up CI): it executes the
/// production path end-to-end (`CandleStreamingTranscriber::new` →
/// `WhisperEngine::decode_pcm`) so the inference code is covered.
/// Config trades cadence for speed (~3 decodes total).
#[test]
#[ignore = "requires Whisper tiny.en model on disk; run `omni-dev voice install-model` first"]
fn streaming_smoke_short_en_transcribes_with_real_backend() {
    const CONTENT_WORDS: &[&str] = &[
        "wizards",
        "tempers",
        "universal",
        "flaw",
        "species",
        "fighting",
        "rely",
    ];
    let model_dir = require_model_dir();
    let transcriber = CandleStreamingTranscriber::with_config_and_rng(
        &model_dir,
        StreamingConfig {
            cadence_secs: 2.0,
            ..StreamingConfig::default()
        },
        Box::new(CountingUlidRng::new()),
    )
    .expect("CandleStreamingTranscriber should build");
    let wav = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/voice/short_en.wav");
    let input = VecAudioInput::from_wav_path(wav, CHUNK_SAMPLES).expect("fixture should load");

    let events: Vec<TranscriptEvent> = transcriber
        .transcribe(Box::new(input))
        .expect("transcribe should succeed")
        .collect::<anyhow::Result<Vec<_>>>()
        .expect("backend should not error mid-stream");

    assert!(
        matches!(
            events.last(),
            Some(TranscriptEvent::Endpoint {
                kind: EndpointKind::StreamEnd,
                ..
            })
        ),
        "last event must be Endpoint(StreamEnd), got: {:?}",
        events.last()
    );
    let transcript = committed_transcript(&events).to_lowercase();
    assert!(!transcript.is_empty(), "expected committed text");
    let hits = CONTENT_WORDS
        .iter()
        .filter(|w| transcript.contains(**w))
        .count();
    assert!(
        hits >= 4,
        "expected most content words to survive streaming commit, got {hits}/7 in: {transcript:?}"
    );
    // Real confidence (not the spike's hard-coded 1.0) on every Final.
    for event in &events {
        if let TranscriptEvent::Final { confidence, .. } = event {
            assert!(
                *confidence > 0.0 && *confidence <= 1.0,
                "confidence must be in (0, 1], got {confidence}"
            );
        }
    }
}

/// LCD-envelope gates that don't need pacing: WER ≤ 15 %, total-pipeline
/// RTF ≤ 0.5 (an upper bound on inference RTF — chunk pulls are instant
/// when unpaced), and event-stream shape.
#[test]
#[ignore = "requires Whisper tiny.en model on disk; run `omni-dev voice install-model` first"]
fn streaming_unpaced_meets_wer_and_rtf_envelope() {
    let model_dir = require_model_dir();
    let transcriber = CandleStreamingTranscriber::new(&model_dir)
        .expect("CandleStreamingTranscriber::new should succeed");
    let input =
        VecAudioInput::from_wav_path(fixture_wav(), CHUNK_SAMPLES).expect("fixture should load");

    let run_start = Instant::now();
    let events: Vec<TranscriptEvent> = transcriber
        .transcribe(Box::new(input))
        .expect("transcribe should succeed")
        .collect::<anyhow::Result<Vec<_>>>()
        .expect("backend should not error mid-stream");
    let wall_secs = run_start.elapsed().as_secs_f64();

    // Shape: partials present, several finals, terminal StreamEnd.
    let partials = events
        .iter()
        .filter(|e| matches!(e, TranscriptEvent::Partial { .. }))
        .count();
    let finals = events
        .iter()
        .filter(|e| matches!(e, TranscriptEvent::Final { .. }))
        .count();
    assert!(partials >= 1, "expected Partial events, got {partials}");
    assert!(finals >= 2, "expected several Final events, got {finals}");
    let Some(TranscriptEvent::Endpoint {
        at,
        kind: EndpointKind::StreamEnd,
    }) = events.last()
    else {
        panic!(
            "last event must be Endpoint(StreamEnd), got: {:?}",
            events.last()
        );
    };

    // The terminal endpoint's `at` is the audio frontier — assert it
    // matches the fixture length (5 min) and use it as the RTF denominator.
    let audio_secs = at.as_secs_f64();
    assert!(
        (audio_secs - 300.0).abs() < 2.0,
        "frontier should be ~300 s, got {audio_secs}"
    );
    let rtf = wall_secs / audio_secs;
    eprintln!("unpaced: wall={wall_secs:.1}s audio={audio_secs:.1}s rtf={rtf:.3}");
    assert!(rtf <= 0.5, "RTF gate: {rtf:.3} > 0.5 (baseline 0.34)");

    // WER against the hand-corrected reference transcript.
    let reference =
        std::fs::read_to_string(fixture_expected()).expect("expected.txt fixture should exist");
    let report = wer::wer(&reference, &committed_transcript(&events));
    eprintln!("unpaced: {report:?}");
    assert!(
        report.wer <= 0.15,
        "WER gate: {:.3} > 0.15 (baseline 0.092); report: {report:?}",
        report.wer
    );
}

/// Three runs over the first 60 s of the fixture with a pinned
/// [`CountingUlidRng`] must produce byte-identical serialized event
/// streams (the #969 determinism gate; per-host).
#[test]
#[ignore = "requires Whisper tiny.en model on disk; run `omni-dev voice install-model` first"]
fn streaming_is_deterministic_across_runs() {
    let model_dir = require_model_dir();

    // First 60 s of the fixture: enough for several segments, a third of
    // the wall-clock cost of the full file per run.
    let mut full =
        VecAudioInput::from_wav_path(fixture_wav(), CHUNK_SAMPLES).expect("fixture should load");
    let mut samples: Vec<i16> = Vec::new();
    while let Some(chunk) = full.next_chunk() {
        samples.extend_from_slice(&chunk);
        if samples.len() >= 60 * 16_000 {
            break;
        }
    }

    let run = || -> Vec<String> {
        let transcriber = CandleStreamingTranscriber::with_config_and_rng(
            &model_dir,
            StreamingConfig::default(),
            Box::new(CountingUlidRng::new()),
        )
        .expect("transcriber should build");
        let input = VecAudioInput::from_samples(samples.clone(), CHUNK_SAMPLES);
        transcriber
            .transcribe(Box::new(input))
            .expect("transcribe should succeed")
            .map(|e| serde_json::to_string(&e.expect("no mid-stream error")).unwrap())
            .collect()
    };

    let a = run();
    let b = run();
    let c = run();
    assert!(!a.is_empty(), "expected events from the 60 s slice");
    assert_eq!(a, b, "runs 1 and 2 must be byte-identical");
    assert_eq!(b, c, "runs 2 and 3 must be byte-identical");
}

/// As-live envelope at 1× pacing: time-to-final ≤ 2.5 s (mean & max),
/// display lag bounded and non-drifting; partial-latency P50/P95 and peak
/// RSS reported (RSS gated only under `OMNI_DEV_STREAMING_RSS_GATE=1`).
/// Wall-clock runtime ≈ the fixture length (5 min).
#[test]
#[ignore = "requires Whisper tiny.en model on disk and ~5 min wall clock; run `omni-dev voice install-model` first"]
fn streaming_paced_time_to_final_and_lag_bounded() {
    let model_dir = require_model_dir();
    let transcriber = CandleStreamingTranscriber::new(&model_dir)
        .expect("CandleStreamingTranscriber::new should succeed");
    let input = PacedAudioInput::new(
        VecAudioInput::from_wav_path(fixture_wav(), CHUNK_SAMPLES).expect("fixture should load"),
    );

    let run_start = Instant::now();
    let timeline: Vec<(f64, TranscriptEvent)> = transcriber
        .transcribe(Box::new(input))
        .expect("transcribe should succeed")
        .map(|e| {
            (
                run_start.elapsed().as_secs_f64(),
                e.expect("backend should not error mid-stream"),
            )
        })
        .collect();
    assert!(
        matches!(
            timeline.last(),
            Some((
                _,
                TranscriptEvent::Endpoint {
                    kind: EndpointKind::StreamEnd,
                    ..
                }
            ))
        ),
        "last event must be Endpoint(StreamEnd)"
    );

    // ── Time-to-final per utterance endpoint (the spike's definition):
    // wall of the last Final committed for the segment, minus the
    // end-of-utterance the endpoint detected (`at` − silence budget).
    let mut ttf: Vec<f64> = Vec::new();
    let mut last_final_wall: Option<f64> = None;
    for (wall, event) in &timeline {
        match event {
            TranscriptEvent::Final { .. } => last_final_wall = Some(*wall),
            TranscriptEvent::Endpoint {
                at,
                kind: EndpointKind::SilenceGap,
            } => {
                if let Some(w) = last_final_wall.take() {
                    ttf.push(w - (at.as_secs_f64() - DEFAULT_SILENCE_SECS));
                }
            }
            _ => {}
        }
    }
    assert!(!ttf.is_empty(), "expected utterance endpoints with finals");
    let ttf_mean = ttf.iter().sum::<f64>() / ttf.len() as f64;
    let ttf_max = ttf.iter().copied().fold(f64::MIN, f64::max);
    eprintln!(
        "paced: time-to-final mean={ttf_mean:.2}s max={ttf_max:.2}s over {} endpoints",
        ttf.len()
    );
    assert!(
        ttf_mean <= 2.5,
        "time-to-final mean gate: {ttf_mean:.2} > 2.5 (baseline 0.73)"
    );
    assert!(
        ttf_max <= 2.5,
        "time-to-final max gate: {ttf_max:.2} > 2.5 (baseline 1.42)"
    );

    // ── Display lag: wall − audio frontier at emission, per Partial/Final.
    // Bounded absolutely and non-drifting (last quarter vs first quarter).
    let lags: Vec<f64> = timeline
        .iter()
        .filter_map(|(wall, event)| match event {
            TranscriptEvent::Partial { end, .. } | TranscriptEvent::Final { end, .. } => {
                Some(wall - end.as_secs_f64())
            }
            TranscriptEvent::Endpoint { .. } => None,
        })
        .collect();
    assert!(lags.len() >= 8, "expected a meaningful lag sample");
    let max_lag = lags.iter().copied().fold(f64::MIN, f64::max);
    let quarter = lags.len() / 4;
    let mean = |s: &[f64]| s.iter().sum::<f64>() / s.len() as f64;
    let first_q = mean(&lags[..quarter.max(1)]);
    let last_q = mean(&lags[lags.len() - quarter.max(1)..]);
    eprintln!("paced: lag max={max_lag:.2}s first-quarter mean={first_q:.2}s last-quarter mean={last_q:.2}s");
    assert!(max_lag <= 5.0, "lag bound gate: {max_lag:.2} > 5.0");
    assert!(
        last_q <= first_q + 1.0,
        "lag drift gate: last-quarter mean {last_q:.2} > first-quarter mean {first_q:.2} + 1.0"
    );

    // ── Partial latency (reported, not gated — the LCD tier explicitly
    // fails the strict interactive ≤ 1 s gate; baseline P95 2.41 s).
    // First-emission wall per committed word position, aligned to the
    // forced-alignment ground truth.
    let mut first_emit: Vec<f64> = Vec::new(); // by committed-word position
    let mut committed = 0usize;
    for (wall, event) in &timeline {
        match event {
            TranscriptEvent::Partial { text, .. } => {
                let n = normalise_words(text).len();
                while first_emit.len() < committed + n {
                    first_emit.push(*wall);
                }
            }
            TranscriptEvent::Final { text, .. } => {
                let n = normalise_words(text).len();
                while first_emit.len() < committed + n {
                    first_emit.push(*wall);
                }
                committed += n;
            }
            TranscriptEvent::Endpoint { .. } => {}
        }
    }
    let events_only: Vec<TranscriptEvent> = timeline.iter().map(|(_, e)| e.clone()).collect();
    let hyp_words = normalise_words(&committed_transcript(&events_only));
    let ref_words_raw = load_ref_words();
    let ref_norm: Vec<String> = ref_words_raw
        .iter()
        .map(|w| normalise_words(&w.word).join(""))
        .collect();
    let matches = match_words(&hyp_words, &ref_norm);
    let coverage = matches.len() as f64 / ref_norm.len() as f64;
    assert!(
        coverage >= 0.6,
        "word alignment coverage {coverage:.2} too low for latency metrics to be meaningful"
    );
    let mut latencies: Vec<f64> = matches
        .iter()
        .filter_map(|&(hyp_idx, ref_idx)| {
            first_emit
                .get(hyp_idx)
                .map(|w| w - ref_words_raw[ref_idx].end_s)
        })
        .filter(|l| *l >= 0.0)
        .collect();
    latencies.sort_by(f64::total_cmp);
    if !latencies.is_empty() {
        let p = |q: f64| latencies[((latencies.len() - 1) as f64 * q) as usize];
        eprintln!(
            "paced: partial-latency P50={:.2}s P95={:.2}s over {} matched words (coverage {coverage:.2}) — reported, not gated",
            p(0.5),
            p(0.95),
            latencies.len()
        );
    }

    // ── Peak RSS: reported on Linux; gated only when the CI keep-up
    // workflow runs this test in isolation.
    match peak_rss_bytes() {
        Some(bytes) => {
            let mb = bytes as f64 / (1024.0 * 1024.0);
            eprintln!("paced: peak RSS {mb:.0} MB (baseline ~429 MB)");
            if std::env::var("OMNI_DEV_STREAMING_RSS_GATE").as_deref() == Ok("1") {
                assert!(mb <= 500.0, "peak RSS gate: {mb:.0} MB > 500 MB");
            }
        }
        None => eprintln!("paced: peak RSS unavailable on this platform (Linux-only metric)"),
    }
}

/// Word error rate via Levenshtein edit distance, ported from the #969
/// spike's `baseline/src/wer.rs` so the gate measures the same thing the
/// baseline numbers were measured with.
mod wer {
    /// Edit-distance breakdown over normalized word tokens.
    #[derive(Debug, Clone, Copy)]
    pub struct WerReport {
        pub edits: usize,
        pub substitutions: usize,
        pub insertions: usize,
        pub deletions: usize,
        #[allow(dead_code)] // populated for the {:?} report in test output
        pub reference_words: usize,
        #[allow(dead_code)] // populated for the {:?} report in test output
        pub hypothesis_words: usize,
        pub wer: f64,
    }

    pub fn wer(reference: &str, hypothesis: &str) -> WerReport {
        let r = tokenize(reference);
        let h = tokenize(hypothesis);
        let (edits, subs, ins, dels) = levenshtein_with_breakdown(&r, &h);
        let wer = if r.is_empty() {
            if h.is_empty() {
                0.0
            } else {
                1.0
            }
        } else {
            edits as f64 / r.len() as f64
        };
        WerReport {
            edits,
            substitutions: subs,
            insertions: ins,
            deletions: dels,
            reference_words: r.len(),
            hypothesis_words: h.len(),
            wer,
        }
    }

    /// Lowercases and strips everything but alphanumerics and apostrophes,
    /// then splits on whitespace.
    pub fn tokenize(s: &str) -> Vec<String> {
        s.to_lowercase()
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '\'' || c.is_whitespace() {
                    c
                } else {
                    ' '
                }
            })
            .collect::<String>()
            .split_whitespace()
            .map(str::to_string)
            .collect()
    }

    #[allow(clippy::many_single_char_names)] // standard edit-distance DP notation (r/h, n/m, i/j)
    fn levenshtein_with_breakdown(r: &[String], h: &[String]) -> (usize, usize, usize, usize) {
        let n = r.len();
        let m = h.len();
        if n == 0 {
            return (m, 0, m, 0);
        }
        if m == 0 {
            return (n, 0, 0, n);
        }

        // Op type per cell so we can reconstruct the path for the s/i/d
        // split. 0 = match, 1 = substitution, 2 = insertion (H),
        // 3 = deletion (R).
        let mut cost = vec![vec![0usize; m + 1]; n + 1];
        let mut op = vec![vec![0u8; m + 1]; n + 1];
        for i in 0..=n {
            cost[i][0] = i;
            op[i][0] = 3;
        }
        for j in 0..=m {
            cost[0][j] = j;
            op[0][j] = 2;
        }
        op[0][0] = 0;

        for i in 1..=n {
            for j in 1..=m {
                if r[i - 1] == h[j - 1] {
                    cost[i][j] = cost[i - 1][j - 1];
                    op[i][j] = 0;
                } else {
                    let sub = cost[i - 1][j - 1] + 1;
                    let ins = cost[i][j - 1] + 1;
                    let del = cost[i - 1][j] + 1;
                    let m_cost = sub.min(ins).min(del);
                    cost[i][j] = m_cost;
                    op[i][j] = if m_cost == sub {
                        1
                    } else if m_cost == ins {
                        2
                    } else {
                        3
                    };
                }
            }
        }

        let mut i = n;
        let mut j = m;
        let mut subs = 0;
        let mut ins = 0;
        let mut dels = 0;
        while i > 0 || j > 0 {
            match op[i][j] {
                0 => {
                    i -= 1;
                    j -= 1;
                }
                1 => {
                    subs += 1;
                    i -= 1;
                    j -= 1;
                }
                2 => {
                    ins += 1;
                    j -= 1;
                }
                3 => {
                    dels += 1;
                    i -= 1;
                }
                _ => unreachable!(),
            }
        }
        (subs + ins + dels, subs, ins, dels)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn identical_strings_have_zero_wer() {
            let r = wer("hello world", "hello world");
            assert_eq!(r.edits, 0);
            assert!(r.wer.abs() < f64::EPSILON);
        }

        #[test]
        fn one_substitution() {
            let r = wer("the quick brown fox", "the quick brown dog");
            assert_eq!(r.substitutions, 1);
            assert_eq!(r.insertions, 0);
            assert_eq!(r.deletions, 0);
            assert!((r.wer - 0.25).abs() < 1e-9);
        }

        #[test]
        fn punctuation_and_case_are_normalised() {
            let r = wer("Hello, world!", "hello world");
            assert_eq!(r.edits, 0);
        }

        #[test]
        fn empty_reference_with_hypothesis_words() {
            let r = wer("", "hello world");
            assert_eq!(r.edits, 2);
            assert_eq!(r.insertions, 2);
            assert!((r.wer - 1.0).abs() < f64::EPSILON);
        }
    }
}

#[cfg(test)]
mod driver_tests {
    use super::*;

    /// The matcher is forward-only with bounded lookahead: substituted
    /// hypothesis words are skipped, reference order is preserved.
    #[test]
    fn match_words_skips_substitutions_and_keeps_order() {
        let hyp: Vec<String> = ["the", "quick", "browne", "fox"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let reference: Vec<String> = ["the", "quick", "brown", "fox"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let m = match_words(&hyp, &reference);
        assert_eq!(m, vec![(0, 0), (1, 1), (3, 3)]);
    }

    /// The paced driver releases chunk `i` no earlier than its deadline
    /// and passes audio through unchanged.
    #[test]
    fn paced_input_respects_deadlines_and_preserves_audio() {
        let samples: Vec<i16> = (0..4800).map(|i| i as i16).collect();
        let mut paced = PacedAudioInput::new(VecAudioInput::from_samples(samples.clone(), 1600));
        let start = Instant::now();
        let mut collected: Vec<i16> = Vec::new();
        while let Some(chunk) = paced.next_chunk() {
            collected.extend_from_slice(&chunk);
        }
        // 3 chunks × 100 ms deadlines ⇒ at least 300 ms of wall clock.
        assert!(
            start.elapsed() >= Duration::from_millis(300),
            "pacing should enforce deadlines, took {:?}",
            start.elapsed()
        );
        assert_eq!(collected, samples);
    }
}
