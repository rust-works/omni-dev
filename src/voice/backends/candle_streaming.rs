//! `CandleStreamingTranscriber` — pure-Rust streaming Whisper on `candle`
//! with VAD-driven chunking and LocalAgreement-2 (#974, algorithm validated
//! by the #969 spike; decision recorded in ADR-0040).
//!
//! This is the **latency-tolerant, lowest-common-denominator** streaming
//! backend: it runs on every platform the batch backend runs on (pure Rust,
//! no native toolchain) and keeps up with real-time speech, but the
//! displayed transcript trails the speaker by ~1.5 s typical / up to ~3 s
//! (bounded, non-drifting while during-speech RTF < 1). Sub-second
//! interactive latency is a **non-goal** — candle Whisper's fixed-cost
//! ~0.5–0.6 s per inference makes it structurally unreachable; see
//! ADR-0040 and #936 for the low-latency tier.
//!
//! ## Algorithm
//!
//! Per pulled audio chunk:
//!
//! 1. **VAD silence-gating** — [`VadGate`] scores 16 ms windows; only
//!    voiced chunks enter the decode window, so the decoder never
//!    transcribes silence (the RTF lever).
//! 2. **Cadence inference** — once the window reaches `min_window_secs`,
//!    re-decode it every `cadence_secs` of new audio, but only if new
//!    voiced audio actually entered the window (the redundant-inference
//!    skip).
//! 3. **LocalAgreement-2 commit** — the longest common word prefix of the
//!    two most recent hypotheses is stable: words beyond the already
//!    committed count are emitted as `Final`; the volatile tail is emitted
//!    as `Partial`.
//! 4. **Finalize on endpoint/cap** — a VAD silence endpoint flushes the
//!    uncommitted tail of the last hypothesis *without* re-decoding (the
//!    window is unchanged during trailing silence); the hard window cap
//!    (`max_window_secs`) re-decodes the full window first so trailing
//!    audio since the last cadence pass is not lost (lossless cap).
//!
//! ## Event mapping
//!
//! The decode window holds voiced-only samples, so per-word timing does
//! not exist. Events carry segment-granularity times on a single clock —
//! the input-audio frontier (total samples consumed, including silence):
//! `Partial`/`Final` `start` is the frontier when the current utterance's
//! first voiced chunk arrived, `end` is the frontier at emission. Ranges
//! from one utterance overlap; they identify the source region, not word
//! alignment. Deduplicate `Final`s by `event_id`.
//!
//! Unlike the spike (which hard-coded 1.0), each `Final` carries the real
//! average-logprob confidence of the inference that produced its words.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Result};

use crate::voice::backends::candle::WhisperEngine;
use crate::voice::det::{SystemUlidRng, UlidRng};
use crate::voice::transcriber::{
    AudioInput, EndpointKind, EventStream, Transcriber, TranscriptEvent,
};
use crate::voice::vad::VadGate;
use crate::voice::wav::TARGET_SAMPLE_RATE;

/// Minimum voiced window to bother transcribing at a *boundary*
/// (endpoint/cap/stream-end). Lower than `min_window_secs` so short
/// phrase-level segments (the point of VAD-driven chunking) are still
/// committed rather than silently dropped.
const FINALIZE_MIN_SECS: f32 = 0.4;

/// Tuning knobs for the streaming state machine.
///
/// [`StreamingConfig::default`] is the **recommended LCD operating point**
/// measured in #969 (`tiny.en`, paced 1×, Apple-Silicon CPU): RTF 0.34,
/// WER 9.2 %, time-to-final 0.73/1.42 s mean/max, peak RSS ~429 MB. The
/// defaults maximise keep-up headroom, not minimise lag. `silence_secs`
/// is the one knob that may need per-deployment tuning (0.5 cuts more
/// conservatively).
#[derive(Debug, Clone)]
pub struct StreamingConfig {
    /// VAD speech-score threshold in `[0, 1]` (aggressiveness); lower =
    /// more permissive (more audio treated as speech).
    pub vad_threshold: f32,
    /// Seconds of consecutive VAD silence before an utterance endpoint
    /// fires. `0` disables auto-endpointing (cap and stream-end still
    /// flush).
    pub silence_secs: f32,
    /// Minimum voiced window (s) before the first cadence inference of a
    /// segment fires.
    pub min_window_secs: f32,
    /// Seconds of new audio between re-inferences.
    pub cadence_secs: f32,
    /// Hard voiced-window cap (s) before a forced flush, bounding decoder
    /// iterations during continuous speech.
    pub max_window_secs: f32,
    /// Whether to emit `Partial` events for the volatile hypothesis tail
    /// in addition to committed `Final`s.
    pub emit_partials: bool,
}

impl Default for StreamingConfig {
    fn default() -> Self {
        Self {
            vad_threshold: 0.5,
            silence_secs: 0.3,
            min_window_secs: 2.0,
            cadence_secs: 1.0,
            max_window_secs: 5.0,
            emit_partials: true,
        }
    }
}

impl StreamingConfig {
    /// Validates the knobs, failing at construction time rather than
    /// mid-stream.
    pub fn validate(&self) -> Result<()> {
        if !(0.0..=1.0).contains(&self.vad_threshold) {
            bail!(
                "vad_threshold must be in [0, 1], got {}",
                self.vad_threshold
            );
        }
        if !self.silence_secs.is_finite() || self.silence_secs < 0.0 {
            bail!(
                "silence_secs must be finite and >= 0, got {}",
                self.silence_secs
            );
        }
        for (name, value) in [
            ("min_window_secs", self.min_window_secs),
            ("cadence_secs", self.cadence_secs),
            ("max_window_secs", self.max_window_secs),
        ] {
            if !value.is_finite() || value <= 0.0 {
                bail!("{name} must be finite and > 0, got {value}");
            }
        }
        if self.min_window_secs > self.max_window_secs {
            bail!(
                "min_window_secs ({}) must not exceed max_window_secs ({})",
                self.min_window_secs,
                self.max_window_secs
            );
        }
        Ok(())
    }
}

/// Seam between the streaming state machine and the Whisper inference.
///
/// Production uses [`WhisperEngine`]; unit tests script the decoder so the
/// LA-2 / finalize logic is testable without a model on disk.
pub(crate) trait WindowDecoder: Send + Sync {
    /// Transcribes one PCM window (f32 in `[-1, 1]` at 16 kHz), returning
    /// `(text, confidence)`.
    fn decode(&self, pcm: &[f32]) -> Result<(String, f32)>;
}

impl WindowDecoder for WhisperEngine {
    fn decode(&self, pcm: &[f32]) -> Result<(String, f32)> {
        self.decode_pcm(pcm)
    }
}

/// Streaming Whisper backend (`--backend whisper-candle-streaming`).
///
/// Implements [`Transcriber`] by returning a lazy [`EventStream`]: each
/// `next()` pulls chunks from the [`AudioInput`] and runs the
/// VAD/cadence/LocalAgreement-2 state machine, yielding
/// `Partial`/`Final`/`Endpoint` events as they materialise. Pacing is the
/// *input's* concern — a live source paces naturally; replayed fixtures
/// are consumed as fast as inference allows.
pub struct CandleStreamingTranscriber {
    decoder: Arc<dyn WindowDecoder>,
    config: StreamingConfig,
    rng: Arc<Mutex<Box<dyn UlidRng>>>,
}

impl CandleStreamingTranscriber {
    /// Builds a streaming transcriber with the recommended defaults,
    /// loading the Whisper model from `model_dir`.
    pub fn new(model_dir: &Path) -> Result<Self> {
        Self::with_config(model_dir, StreamingConfig::default())
    }

    /// Builds a streaming transcriber with explicit knobs.
    pub fn with_config(model_dir: &Path, config: StreamingConfig) -> Result<Self> {
        Self::with_config_and_rng(model_dir, config, Box::new(SystemUlidRng))
    }

    /// Builds a streaming transcriber with explicit knobs and a pluggable
    /// [`UlidRng`] — tests inject
    /// [`CountingUlidRng`](crate::voice::det::CountingUlidRng) for
    /// byte-identical event streams across runs.
    pub fn with_config_and_rng(
        model_dir: &Path,
        config: StreamingConfig,
        rng: Box<dyn UlidRng>,
    ) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            decoder: Arc::new(WhisperEngine::load(model_dir)?),
            config,
            rng: Arc::new(Mutex::new(rng)),
        })
    }

    /// Test-only constructor over an arbitrary [`WindowDecoder`].
    #[cfg(test)]
    fn from_decoder(
        decoder: Arc<dyn WindowDecoder>,
        config: StreamingConfig,
        rng: Box<dyn UlidRng>,
    ) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            decoder,
            config,
            rng: Arc::new(Mutex::new(rng)),
        })
    }
}

impl Transcriber for CandleStreamingTranscriber {
    fn transcribe(&self, audio: Box<dyn AudioInput>) -> Result<Box<dyn EventStream>> {
        Ok(Box::new(CandleStreamingStream::new(
            Arc::clone(&self.decoder),
            self.config.clone(),
            Arc::clone(&self.rng),
            audio,
        )))
    }
}

/// Converts a sample count on the input-audio frontier to stream time.
#[allow(clippy::cast_precision_loss)] // sample counts are far below 2^52
fn frontier_duration(samples: u64) -> Duration {
    Duration::from_secs_f64(samples as f64 / f64::from(TARGET_SAMPLE_RATE))
}

/// Converts whole seconds of audio to a sample count.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // validated finite, non-negative, small
fn secs_to_samples(secs: f32) -> usize {
    (secs * TARGET_SAMPLE_RATE as f32) as usize
}

/// Word-level longest common prefix — the LocalAgreement-2 stability test.
fn longest_common_prefix(a: &[String], b: &[String]) -> usize {
    a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count()
}

/// The lazy event stream: owns the audio input, the VAD, the decode
/// window, and the LocalAgreement-2 state. See the module docs for the
/// algorithm.
struct CandleStreamingStream {
    decoder: Arc<dyn WindowDecoder>,
    rng: Arc<Mutex<Box<dyn UlidRng>>>,
    audio: Box<dyn AudioInput>,
    vad: VadGate,
    emit_partials: bool,
    // Derived sample-count thresholds (all comparisons stay in exact
    // sample units; no float/ms rounding drift).
    min_window_samples: usize,
    cadence_samples: u64,
    max_window_samples: usize,
    finalize_min_samples: usize,
    /// Voiced-only samples of the current utterance.
    window: VecDeque<f32>,
    /// Hypothesis words from the most recent inference over `window`.
    hyp_prev_words: Vec<String>,
    /// LocalAgreement-2 commit point: words before this index in
    /// `hyp_prev_words` have been emitted as `Final`.
    committed: usize,
    /// Confidence of the most recent inference (for the no-reinfer flush).
    last_confidence: f32,
    /// Input-audio frontier: total samples consumed, including silence.
    samples_pushed: u64,
    /// Frontier at the last inference — the cadence clock.
    last_inference_samples: u64,
    /// Window length at the last inference — the redundant-inference-skip
    /// marker (unchanged window ⇒ identical audio ⇒ skip the re-decode).
    last_inference_window_len: usize,
    was_idle: bool,
    /// Frontier when the current utterance's first voiced chunk arrived.
    utterance_start: Duration,
    /// Events materialised by the state machine but not yet yielded.
    pending: VecDeque<TranscriptEvent>,
    /// Fused after the terminal endpoint or the first error.
    done: bool,
}

impl CandleStreamingStream {
    fn new(
        decoder: Arc<dyn WindowDecoder>,
        config: StreamingConfig,
        rng: Arc<Mutex<Box<dyn UlidRng>>>,
        audio: Box<dyn AudioInput>,
    ) -> Self {
        Self {
            decoder,
            rng,
            audio,
            vad: VadGate::new(config.vad_threshold, config.silence_secs),
            emit_partials: config.emit_partials,
            min_window_samples: secs_to_samples(config.min_window_secs),
            cadence_samples: secs_to_samples(config.cadence_secs) as u64,
            max_window_samples: secs_to_samples(config.max_window_secs),
            finalize_min_samples: secs_to_samples(FINALIZE_MIN_SECS),
            window: VecDeque::new(),
            hyp_prev_words: Vec::new(),
            committed: 0,
            last_confidence: 0.0,
            samples_pushed: 0,
            last_inference_samples: 0,
            last_inference_window_len: 0,
            was_idle: false,
            utterance_start: Duration::ZERO,
            pending: VecDeque::new(),
            done: false,
        }
    }

    /// Runs one chunk through the VAD → cap → endpoint → cadence pipeline,
    /// queueing any events it produces.
    fn step(&mut self, chunk: &[i16]) -> Result<()> {
        let chunk_f32: Vec<f32> = chunk.iter().map(|&s| f32::from(s) / 32768.0).collect();
        let frontier_before = self.samples_pushed;
        self.samples_pushed += chunk.len() as u64;

        // Only voiced audio enters the decode window, so the decoder never
        // transcribes silence. The endpoint clock (vad.is_idle) runs on the
        // VAD's own silent-window counter, unaffected by what we append.
        let chunk_has_voice = self.vad.push(&chunk_f32).iter().any(|&(voiced, _)| voiced);
        if chunk_has_voice {
            if self.window.is_empty() {
                self.utterance_start = frontier_duration(frontier_before);
            }
            self.window.extend(chunk_f32.iter().copied());
        }

        // Hard cap: bound the decode window (and thus decoder iterations).
        // The cap is a real segmentation boundary, so finalize re-decodes
        // the full window (lossless cap) before flushing.
        if self.window.len() > self.max_window_samples {
            self.finalize(EndpointKind::SilenceGap)?;
            self.reset_segment();
            return Ok(());
        }

        // VAD endpoint: on the not-idle → idle edge, commit the segment.
        // Suppressed when no voice has been seen since the last boundary —
        // long silences must not emit an endpoint every `silence_secs`.
        let now_idle = self.vad.is_idle();
        if !self.was_idle && now_idle && self.vad.has_any_voice() {
            self.finalize(EndpointKind::SilenceGap)?;
            self.reset_segment();
            return Ok(());
        }
        self.was_idle = now_idle;

        // Cadence inference + LocalAgreement-2 commit. Re-infer only when
        // the cadence has elapsed AND new voiced audio entered the window
        // (a VAD-dropped silence gap leaves it unchanged — skip).
        if self.window.len() >= self.min_window_samples
            && self.samples_pushed - self.last_inference_samples >= self.cadence_samples
            && self.window.len() > self.last_inference_window_len
        {
            let (text, confidence) = self.decode_window()?;
            let words: Vec<String> = text.split_whitespace().map(String::from).collect();

            let lcp = longest_common_prefix(&self.hyp_prev_words, &words);
            if lcp > self.committed {
                self.queue_final(words[self.committed..lcp].join(" "), confidence)?;
                self.committed = lcp;
            }
            if self.emit_partials && words.len() > self.committed {
                self.pending.push_back(TranscriptEvent::Partial {
                    text: words[self.committed..].join(" "),
                    start: self.utterance_start,
                    end: frontier_duration(self.samples_pushed),
                    words: None,
                    speaker: None,
                });
            }
            self.hyp_prev_words = words;
            self.last_confidence = confidence;
            self.last_inference_samples = self.samples_pushed;
            self.last_inference_window_len = self.window.len();
        }
        Ok(())
    }

    /// Commits the current segment at a boundary (cap, silence endpoint,
    /// or stream end) and queues the `Endpoint`.
    ///
    /// When new voiced audio entered the window since the last cadence
    /// inference (e.g. the cap fired mid-phrase), re-decode the FULL window
    /// so trailing audio isn't lost, and commit every word beyond the
    /// commit point. Otherwise — the common silence-endpoint case, where
    /// the window stopped growing during the trailing (dropped) silence so
    /// the last hypothesis already covers it — just flush the uncommitted
    /// tail, avoiding a redundant re-decode of identical audio (the
    /// dominant RTF waste).
    fn finalize(&mut self, kind: EndpointKind) -> Result<()> {
        let grew = self.window.len() > self.last_inference_window_len;
        if grew && self.window.len() >= self.finalize_min_samples {
            let (text, confidence) = self.decode_window()?;
            let words: Vec<String> = text.split_whitespace().map(String::from).collect();
            if words.len() > self.committed {
                self.queue_final(words[self.committed..].join(" "), confidence)?;
            }
        } else if self.hyp_prev_words.len() > self.committed {
            let tail = self.hyp_prev_words[self.committed..].join(" ");
            self.queue_final(tail, self.last_confidence)?;
        }
        self.pending.push_back(TranscriptEvent::Endpoint {
            at: frontier_duration(self.samples_pushed),
            kind,
        });
        Ok(())
    }

    /// Clears per-segment state after a committed boundary.
    fn reset_segment(&mut self) {
        self.window.clear();
        self.hyp_prev_words.clear();
        self.committed = 0;
        self.last_inference_samples = self.samples_pushed;
        self.last_inference_window_len = 0;
        self.vad.reset();
        self.was_idle = false;
    }

    /// Runs the decoder over the full current window.
    fn decode_window(&self) -> Result<(String, f32)> {
        let pcm: Vec<f32> = self.window.iter().copied().collect();
        self.decoder.decode(&pcm)
    }

    /// Queues a `Final` for committed words (no-op for empty text).
    fn queue_final(&mut self, text: String, confidence: f32) -> Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        let event_id = self
            .rng
            .lock()
            .map_err(|e| anyhow!("streaming UlidRng mutex poisoned: {e}"))?
            .next_ulid();
        self.pending.push_back(TranscriptEvent::Final {
            event_id,
            text,
            start: self.utterance_start,
            end: frontier_duration(self.samples_pushed),
            confidence,
            words: None,
            speaker: None,
            revisable: false,
        });
        Ok(())
    }

    /// End-of-input flush: commits whatever the window still holds and
    /// queues the terminal `Endpoint`.
    fn finish(&mut self) -> Result<()> {
        self.finalize(EndpointKind::StreamEnd)
    }
}

impl Iterator for CandleStreamingStream {
    type Item = Result<TranscriptEvent>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(Ok(event));
            }
            if self.done {
                return None;
            }
            if let Some(chunk) = self.audio.next_chunk() {
                if let Err(e) = self.step(&chunk) {
                    self.done = true;
                    return Some(Err(e));
                }
            } else {
                let result = self.finish();
                self.done = true;
                if let Err(e) = result {
                    return Some(Err(e));
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::voice::det::CountingUlidRng;
    use crate::voice::transcriber::VecAudioInput;

    /// Scripted [`WindowDecoder`]: returns canned `(text, confidence)`
    /// responses in order and counts calls. Errs when the script runs dry.
    struct ScriptedDecoder {
        responses: Mutex<VecDeque<Result<(String, f32)>>>,
        calls: AtomicUsize,
    }

    impl ScriptedDecoder {
        fn new(responses: Vec<Result<(String, f32)>>) -> Arc<Self> {
            Arc::new(Self {
                responses: Mutex::new(responses.into_iter().collect()),
                calls: AtomicUsize::new(0),
            })
        }

        fn ok(texts: &[(&str, f32)]) -> Arc<Self> {
            Self::new(texts.iter().map(|&(t, c)| Ok((t.to_string(), c))).collect())
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl WindowDecoder for ScriptedDecoder {
        fn decode(&self, _pcm: &[f32]) -> Result<(String, f32)> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(anyhow!("scripted decoder exhausted")))
        }
    }

    /// Config where every VAD window counts as voiced (threshold 0 ⇒
    /// `score >= 0` always) and auto-endpointing is off — isolates the
    /// cadence/LA-2/cap logic from earshot's GMM scoring.
    fn all_voiced_config() -> StreamingConfig {
        StreamingConfig {
            vad_threshold: 0.0,
            silence_secs: 0.0,
            min_window_secs: 0.1,
            cadence_secs: 0.1,
            max_window_secs: 100.0,
            emit_partials: true,
        }
    }

    fn transcribe_chunks(
        decoder: &Arc<ScriptedDecoder>,
        config: StreamingConfig,
        samples: Vec<i16>,
        chunk_samples: usize,
    ) -> Vec<Result<TranscriptEvent>> {
        let t = CandleStreamingTranscriber::from_decoder(
            Arc::clone(decoder) as Arc<dyn WindowDecoder>,
            config,
            Box::new(CountingUlidRng::new()),
        )
        .unwrap();
        let input = VecAudioInput::from_samples(samples, chunk_samples);
        t.transcribe(Box::new(input)).unwrap().collect()
    }

    fn texts(events: &[Result<TranscriptEvent>]) -> Vec<String> {
        events
            .iter()
            .map(|e| match e.as_ref().unwrap() {
                TranscriptEvent::Partial { text, .. } => format!("P:{text}"),
                TranscriptEvent::Final { text, .. } => format!("F:{text}"),
                TranscriptEvent::Endpoint { kind, .. } => format!("E:{kind:?}"),
            })
            .collect()
    }

    /// A test stream over a scripted decoder with no input — used to drive
    /// `finalize` directly against a hand-built state.
    fn bare_stream(
        decoder: &Arc<ScriptedDecoder>,
        config: StreamingConfig,
    ) -> CandleStreamingStream {
        CandleStreamingStream::new(
            Arc::clone(decoder) as Arc<dyn WindowDecoder>,
            config,
            Arc::new(Mutex::new(
                Box::new(CountingUlidRng::new()) as Box<dyn UlidRng>
            )),
            Box::new(VecAudioInput::from_samples(vec![], 1)),
        )
    }

    #[test]
    fn all_silence_yields_only_stream_end_endpoint_and_never_decodes() {
        let decoder = ScriptedDecoder::ok(&[]);
        // Default config: zeros are silent at threshold 0.5 (proven by the
        // VadGate unit tests). 5 s of silence crosses the 0.3 s endpoint
        // budget repeatedly — none of those edges may emit events.
        let events = transcribe_chunks(
            &decoder,
            StreamingConfig::default(),
            vec![0i16; 80_000],
            1600,
        );
        assert_eq!(texts(&events), vec!["E:StreamEnd"]);
        assert_eq!(decoder.calls(), 0, "silence must never reach the decoder");
        if let TranscriptEvent::Endpoint { at, .. } = events[0].as_ref().unwrap() {
            assert!((at.as_secs_f64() - 5.0).abs() < 1e-9, "got: {at:?}");
        }
    }

    #[test]
    fn la2_commits_stable_prefix_and_emits_volatile_tail_as_partial() {
        // Two cadence inferences: hypothesis grows "hello there" →
        // "hello there world". LA-2 commits the agreed prefix as Final on
        // the second pass; tails ride as Partials. End-of-stream flushes
        // the last uncommitted word without re-decoding (window unchanged
        // after the final inference).
        let decoder = ScriptedDecoder::ok(&[("hello there", 0.9), ("hello there world", 0.8)]);
        let events = transcribe_chunks(&decoder, all_voiced_config(), vec![1000i16; 3200], 1600);
        assert_eq!(
            texts(&events),
            vec![
                "P:hello there",
                "F:hello there",
                "P:world",
                "F:world",
                "E:StreamEnd"
            ]
        );
        assert_eq!(
            decoder.calls(),
            2,
            "stream end must not re-decode an unchanged window"
        );
        // Confidence threading: the committed Final carries the confidence
        // of the inference that produced it; the flushed tail carries the
        // last inference's confidence.
        let finals: Vec<f32> = events
            .iter()
            .filter_map(|e| match e.as_ref().unwrap() {
                TranscriptEvent::Final { confidence, .. } => Some(*confidence),
                _ => None,
            })
            .collect();
        assert_eq!(finals, vec![0.8, 0.8]);
    }

    #[test]
    fn silence_endpoint_flushes_tail_without_reinference() {
        let decoder = ScriptedDecoder::ok(&[]);
        let mut stream = bare_stream(&decoder, StreamingConfig::default());
        // Hand-built state: a 2 s window already decoded ("hello world",
        // first word committed), unchanged since the last inference.
        stream.window = std::iter::repeat(0.1f32).take(32_000).collect();
        stream.last_inference_window_len = stream.window.len();
        stream.hyp_prev_words = vec!["hello".into(), "world".into()];
        stream.committed = 1;
        stream.last_confidence = 0.7;
        stream.samples_pushed = 40_000;

        stream.finalize(EndpointKind::SilenceGap).unwrap();
        assert_eq!(
            decoder.calls(),
            0,
            "unchanged window must skip the re-decode"
        );
        let events: Vec<TranscriptEvent> = stream.pending.drain(..).collect();
        assert_eq!(events.len(), 2);
        match &events[0] {
            TranscriptEvent::Final {
                text, confidence, ..
            } => {
                assert_eq!(text, "world");
                assert!((confidence - 0.7).abs() < f32::EPSILON);
            }
            other => panic!("expected Final, got {other:?}"),
        }
        assert!(matches!(
            &events[1],
            TranscriptEvent::Endpoint {
                kind: EndpointKind::SilenceGap,
                ..
            }
        ));
    }

    #[test]
    fn window_cap_reinfers_full_window_before_flush() {
        let decoder = ScriptedDecoder::ok(&[("hello brave new world", 0.6)]);
        let mut stream = bare_stream(&decoder, StreamingConfig::default());
        // The window grew since the last inference (cap fired mid-phrase):
        // finalize must re-decode the FULL window so the trailing audio is
        // not lost, and commit everything beyond the commit point.
        stream.window = std::iter::repeat(0.1f32).take(80_000).collect();
        stream.last_inference_window_len = 48_000;
        stream.hyp_prev_words = vec!["hello".into(), "brave".into()];
        stream.committed = 1;
        stream.last_confidence = 0.9;
        stream.samples_pushed = 96_000;

        stream.finalize(EndpointKind::SilenceGap).unwrap();
        assert_eq!(
            decoder.calls(),
            1,
            "grown window must be re-decoded losslessly"
        );
        let events: Vec<TranscriptEvent> = stream.pending.drain(..).collect();
        match &events[0] {
            TranscriptEvent::Final {
                text, confidence, ..
            } => {
                assert_eq!(text, "brave new world");
                assert!((confidence - 0.6).abs() < f32::EPSILON);
            }
            other => panic!("expected Final, got {other:?}"),
        }
    }

    #[test]
    fn window_cap_via_stream_resets_segment_state() {
        // max_window 0.5 s (8000 samples), all voiced. The cap fires once
        // the window exceeds it, forcing a flush + endpoint mid-stream.
        let config = StreamingConfig {
            max_window_secs: 0.5,
            min_window_secs: 0.2,
            cadence_secs: 0.2,
            ..all_voiced_config()
        };
        let decoder = ScriptedDecoder::ok(&[("a b", 0.9), ("a b c", 0.9), ("a b c d", 0.9)]);
        let events = transcribe_chunks(&decoder, config, vec![1000i16; 9600], 1600);
        let rendered = texts(&events);
        assert!(
            rendered.contains(&"E:SilenceGap".to_string()),
            "cap must emit a mid-stream endpoint, got: {rendered:?}"
        );
        assert_eq!(
            rendered.last().unwrap(),
            "E:StreamEnd",
            "stream must still terminate with StreamEnd, got: {rendered:?}"
        );
        // All committed text, in order, with no duplication.
        let committed: Vec<&str> = rendered
            .iter()
            .filter_map(|t| t.strip_prefix("F:"))
            .collect();
        assert_eq!(committed.join(" "), "a b c d");
    }

    #[test]
    fn decoder_error_is_yielded_once_then_stream_fuses() {
        let decoder = ScriptedDecoder::new(vec![Err(anyhow!("boom"))]);
        let t = CandleStreamingTranscriber::from_decoder(
            Arc::clone(&decoder) as Arc<dyn WindowDecoder>,
            all_voiced_config(),
            Box::new(CountingUlidRng::new()),
        )
        .unwrap();
        let input = VecAudioInput::from_samples(vec![1000i16; 3200], 1600);
        let mut stream = t.transcribe(Box::new(input)).unwrap();
        let first = stream.next().expect("expected an item");
        assert!(first.is_err(), "got: {first:?}");
        assert!(stream.next().is_none(), "stream must fuse after an error");
        assert!(stream.next().is_none());
    }

    #[test]
    fn event_ids_deterministic_with_counting_rng() {
        let make_events = || {
            let decoder = ScriptedDecoder::ok(&[("hello there", 0.9), ("hello there world", 0.8)]);
            transcribe_chunks(&decoder, all_voiced_config(), vec![1000i16; 3200], 1600)
                .into_iter()
                .map(|e| serde_json::to_string(&e.unwrap()).unwrap())
                .collect::<Vec<_>>()
        };
        let a = make_events();
        let b = make_events();
        assert_eq!(a, b, "two runs with CountingUlidRng must be byte-identical");
    }

    #[test]
    fn partial_and_final_carry_utterance_start_and_frontier() {
        let decoder = ScriptedDecoder::ok(&[("hi", 0.9), ("hi there", 0.8)]);
        // 0.2 s of audio, all voiced from the first chunk: utterance_start
        // is 0; ends are the frontier at emission. Two chunks ⇒ two
        // cadence inferences (0.1 s min-window/cadence).
        let events = transcribe_chunks(&decoder, all_voiced_config(), vec![1000i16; 3200], 1600);
        for event in &events {
            match event.as_ref().unwrap() {
                TranscriptEvent::Partial { start, end, .. }
                | TranscriptEvent::Final { start, end, .. } => {
                    assert_eq!(*start, Duration::ZERO);
                    assert!(*end <= Duration::from_millis(200), "got: {end:?}");
                    assert!(*end > Duration::ZERO);
                }
                TranscriptEvent::Endpoint { at, .. } => {
                    assert!((at.as_secs_f64() - 0.2).abs() < 1e-9, "got: {at:?}");
                }
            }
        }
    }

    #[test]
    fn config_validation_rejects_bad_values() {
        for config in [
            StreamingConfig {
                vad_threshold: 1.5,
                ..StreamingConfig::default()
            },
            StreamingConfig {
                silence_secs: -0.1,
                ..StreamingConfig::default()
            },
            StreamingConfig {
                cadence_secs: 0.0,
                ..StreamingConfig::default()
            },
            StreamingConfig {
                min_window_secs: 10.0,
                max_window_secs: 5.0,
                ..StreamingConfig::default()
            },
        ] {
            assert!(config.validate().is_err(), "should reject: {config:?}");
        }
        assert!(StreamingConfig::default().validate().is_ok());
    }

    #[test]
    fn stream_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<CandleStreamingStream>();
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CandleStreamingTranscriber>();
    }
}
