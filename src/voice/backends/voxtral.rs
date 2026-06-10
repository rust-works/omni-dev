//! `VoxtralBackend` — native Voxtral Realtime ASR via the `voxtral-sys` FFI
//! engine (vendored `antirez/voxtral.c`).
//!
//! Selected by `--backend voxtral`. Compiled only with the off-by-default
//! `voxtral` feature on `cfg(not(target_os = "windows"))` (ADR-0037). All the
//! `unsafe` lives in `voxtral-sys`; this module is safe Rust.
//!
//! Like [`crate::voice::backends::candle::CandleTranscriber`], this implements
//! the **batch** [`Transcriber`] contract — it drives Voxtral's streaming C
//! API internally (feed PCM, drain token strings, finish) and returns one
//! `Final` plus a terminal `Endpoint`. The richer streaming surface
//! (`Partial` segmentation, `voice listen`) belongs to #806.
//!
//! Inference state lives behind a [`Mutex`] because the engine holds
//! per-stream decoder KV state that two concurrent `transcribe` calls would
//! corrupt — the same correctness reason candle locks its model.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures::channel::mpsc;
use ulid::Ulid;
use voxtral_sys::{VoxCtx, VoxStream};

use crate::voice::idle::IdleDetector;
use crate::voice::models::ensure_voxtral_model_present;
use crate::voice::segment::StreamSegmenter;
use crate::voice::transcriber::{
    AsyncAudioInput, AudioChunk, AudioInput, EndpointKind, EventStream, StreamingTranscriber,
    Transcriber, TranscriptEvent, TranscriptEventStream,
};

/// Voxtral's fixed input sample rate (16 kHz mono), used to convert sample
/// counts to stream-relative durations.
const SAMPLE_RATE: f64 = 16_000.0;

/// Default decoder delay (lookahead) in milliseconds. The #930 spike found
/// 240–480 ms to be the accuracy/latency sweet spot; 480 ms is the engine's
/// own default. A CLI override lands in #933 Phase 4.
pub const DEFAULT_VOXTRAL_DELAY_MS: i32 = 480;

/// PCM is fed to the engine in ~1 s windows, draining tokens between windows,
/// so the pending-token queue never grows unbounded on long audio.
const FEED_WINDOW_SAMPLES: usize = 16_000;

/// Pointers requested per `vox_stream_get` drain call.
const TOKENS_PER_DRAIN: usize = 256;

/// Streaming: minimum wall-clock between encoder runs. Lower than the engine's
/// 2 s default for more responsive `Partial`s; tuned against the model in #933
/// Phase 8.
const STREAM_PROCESSING_INTERVAL_SECS: f32 = 0.5;

/// Streaming: consecutive silent 100 ms windows that commit an utterance with a
/// `SilenceGap` endpoint (~700 ms). Tuned against the model in #933 Phase 8.
const SILENCE_GAP_WINDOWS: u32 = 7;

/// Segment confidence reported for every `Final`.
///
/// Voxtral's `vox_stream_get` returns only token *strings* — the C API exposes
/// no per-token log-probabilities (unlike candle, which derives a real
/// confidence). `1.0` is a placeholder meaning "not provided by this engine",
/// not a genuine certainty measure. A real value would need a richer upstream
/// API.
const VOXTRAL_CONFIDENCE: f32 = 1.0;

/// Native Voxtral Realtime backend.
///
/// Implements both the batch [`Transcriber`] and the streaming
/// [`StreamingTranscriber`] (ADR-0038). The context is `Arc<Mutex<_>>` so a
/// handle can move into the blocking inference thread the streaming path spawns.
#[derive(Debug)]
pub struct VoxtralBackend {
    ctx: Arc<Mutex<VoxCtx>>,
}

impl VoxtralBackend {
    /// Loads the Voxtral model from `model_dir` and applies `delay_ms`.
    ///
    /// Verifies the model files are present up front so a missing model
    /// carries the install hint (mirroring `CandleTranscriber::new`), then
    /// loads the engine and sets the decoder delay.
    pub fn new(model_dir: &Path, delay_ms: i32) -> Result<Self> {
        ensure_voxtral_model_present(model_dir)?;
        let mut ctx = VoxCtx::load(model_dir)
            .map_err(|e| anyhow!("load Voxtral model from {}: {e}", model_dir.display()))?;
        ctx.set_delay(delay_ms);
        Ok(Self {
            ctx: Arc::new(Mutex::new(ctx)),
        })
    }
}

/// Drains all currently-pending token strings into `text`.
fn drain_tokens(stream: &mut VoxStream<'_>, text: &mut String) {
    loop {
        let tokens = stream.get(TOKENS_PER_DRAIN);
        if tokens.is_empty() {
            break;
        }
        for token in tokens {
            text.push_str(&token);
        }
    }
}

/// Drains all currently-pending token strings as a `Vec` (streaming path).
fn drain_token_strings(stream: &mut VoxStream<'_>) -> Vec<String> {
    let mut out = Vec::new();
    loop {
        let tokens = stream.get(TOKENS_PER_DRAIN);
        if tokens.is_empty() {
            break;
        }
        out.extend(tokens);
    }
    out
}

impl Transcriber for VoxtralBackend {
    fn transcribe(&self, mut audio: Box<dyn AudioInput>) -> Result<Box<dyn EventStream>> {
        // Drain i16 chunks; convert to f32 in [-1, 1] as the engine expects.
        let mut samples_i16: Vec<i16> = Vec::new();
        while let Some(chunk) = audio.next_chunk() {
            samples_i16.extend_from_slice(&chunk);
        }
        let total_samples = samples_i16.len();
        let pcm: Vec<f32> = samples_i16
            .iter()
            .map(|&s| f32::from(s) / 32768.0)
            .collect();
        drop(samples_i16);

        #[allow(clippy::cast_precision_loss)]
        let total_duration = Duration::from_secs_f64(total_samples as f64 / SAMPLE_RATE);

        // No audio at all → just the terminal endpoint (matches candle).
        if pcm.is_empty() {
            let events = vec![Ok(TranscriptEvent::Endpoint {
                at: total_duration,
                kind: EndpointKind::StreamEnd,
            })];
            return Ok(Box::new(events.into_iter()));
        }

        let ctx = self
            .ctx
            .lock()
            .map_err(|e| anyhow!("VoxtralBackend context mutex poisoned: {e}"))?;
        let mut stream = ctx
            .stream()
            .map_err(|e| anyhow!("open Voxtral stream: {e}"))?;

        // Feed in windows, draining tokens between them, then finish and drain
        // whatever the delay window held back.
        let mut text = String::new();
        for window in pcm.chunks(FEED_WINDOW_SAMPLES) {
            stream
                .feed(window)
                .map_err(|e| anyhow!("feed audio to Voxtral stream: {e}"))?;
            drain_tokens(&mut stream, &mut text);
        }
        stream
            .finish()
            .map_err(|e| anyhow!("finish Voxtral stream: {e}"))?;
        drain_tokens(&mut stream, &mut text);

        drop(stream);
        drop(ctx);

        let text = text.trim().to_string();
        let mut events: Vec<Result<TranscriptEvent>> = Vec::new();
        if !text.is_empty() {
            events.push(Ok(TranscriptEvent::Final {
                event_id: Ulid::new(),
                text,
                start: Duration::ZERO,
                end: total_duration,
                confidence: VOXTRAL_CONFIDENCE,
                words: None,
                speaker: None,
                revisable: false,
            }));
        }
        events.push(Ok(TranscriptEvent::Endpoint {
            at: total_duration,
            kind: EndpointKind::StreamEnd,
        }));
        Ok(Box::new(events.into_iter()))
    }
}

impl StreamingTranscriber for VoxtralBackend {
    /// Drives the engine incrementally and returns a live event stream
    /// (ADR-0038). Wires an async↔blocking bridge: an async **feeder** task
    /// pulls chunks from `audio` and hands them to a `spawn_blocking`
    /// **inference** task that drives the blocking C engine, emitting events
    /// over a channel.
    ///
    /// Must be called within a tokio runtime (it uses `spawn`/`spawn_blocking`);
    /// the streaming consumers (`voice listen`, #807) are async by design.
    fn transcribe_stream(&self, mut audio: Box<dyn AsyncAudioInput>) -> TranscriptEventStream {
        let (event_tx, event_rx) = mpsc::unbounded::<Result<TranscriptEvent>>();
        let (audio_tx, audio_rx) = std::sync::mpsc::channel::<AudioChunk>();
        let ctx = Arc::clone(&self.ctx);

        // Feeder: pull async audio chunks; dropping `audio_tx` at the end
        // signals end-of-audio to the blocking thread's `recv()`.
        tokio::spawn(async move {
            while let Some(chunk) = audio.next_chunk().await {
                if audio_tx.send(chunk).is_err() {
                    break; // inference thread gone
                }
            }
        });

        // Inference: drive the blocking C engine off the async runtime.
        tokio::task::spawn_blocking(move || run_stream(&ctx, &audio_rx, &event_tx));

        Box::pin(event_rx)
    }
}

/// The streaming inference loop, run on a blocking thread. Locks the context
/// for the stream's lifetime (the guard and `VoxStream` stay thread-local, so
/// no lifetime escapes), then feeds audio, drains tokens into a
/// [`StreamSegmenter`], and emits events over `event_tx`. Engine errors are
/// forwarded as `Err` and stop the stream; a dropped receiver also stops it.
fn run_stream(
    ctx: &Arc<Mutex<VoxCtx>>,
    audio_rx: &std::sync::mpsc::Receiver<AudioChunk>,
    event_tx: &mpsc::UnboundedSender<Result<TranscriptEvent>>,
) {
    let guard = match ctx.lock() {
        Ok(g) => g,
        Err(e) => {
            let _ = event_tx.unbounded_send(Err(anyhow!("VoxtralBackend mutex poisoned: {e}")));
            return;
        }
    };
    let mut stream = match guard.stream() {
        Ok(s) => s,
        Err(e) => {
            let _ = event_tx.unbounded_send(Err(anyhow!("open Voxtral stream: {e}")));
            return;
        }
    };
    stream.set_processing_interval(STREAM_PROCESSING_INTERVAL_SECS);

    // `IdleDetector` is used only as the RMS window classifier here (we read the
    // per-window classes from `push`); its idle-after threshold is irrelevant.
    let mut idle = IdleDetector::new(1);
    let mut segmenter = StreamSegmenter::new(SILENCE_GAP_WINDOWS);
    let mut samples_fed: usize = 0;

    let now = |samples: usize| -> Duration {
        #[allow(clippy::cast_precision_loss)]
        Duration::from_secs_f64(samples as f64 / SAMPLE_RATE)
    };

    while let Ok(chunk) = audio_rx.recv() {
        let pcm: Vec<f32> = chunk.iter().map(|&s| f32::from(s) / 32768.0).collect();
        let classes = idle.push(&pcm);

        if let Err(e) = stream.feed(&pcm) {
            let _ = event_tx.unbounded_send(Err(anyhow!("feed Voxtral stream: {e}")));
            return;
        }
        samples_fed += chunk.len();
        let t = now(samples_fed);

        let tokens = drain_token_strings(&mut stream);
        if let Some(partial) = segmenter.push_tokens(&tokens, t) {
            if event_tx.unbounded_send(Ok(partial)).is_err() {
                return;
            }
        }

        if segmenter.observe_silence(&classes) {
            if let Err(e) = stream.flush() {
                let _ = event_tx.unbounded_send(Err(anyhow!("flush Voxtral stream: {e}")));
                return;
            }
            let flushed = drain_token_strings(&mut stream);
            for ev in segmenter.commit_silence_gap(&flushed, t, VOXTRAL_CONFIDENCE) {
                if event_tx.unbounded_send(Ok(ev)).is_err() {
                    return;
                }
            }
        }
    }

    // Audio ended: finish, drain the delay window, and commit the tail.
    if let Err(e) = stream.finish() {
        let _ = event_tx.unbounded_send(Err(anyhow!("finish Voxtral stream: {e}")));
        return;
    }
    let t = now(samples_fed);
    let tail = drain_token_strings(&mut stream);
    for ev in segmenter.commit_end(&tail, t, VOXTRAL_CONFIDENCE) {
        if event_tx.unbounded_send(Ok(ev)).is_err() {
            return;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // Real inference needs the staged ~8.9 GB model and is uncovered-by-design
    // (ADR-0037), exactly as CandleTranscriber's inference path is. The
    // construction/error path is coverable without the model:

    #[test]
    fn new_missing_model_dir_errors_with_install_hint() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = VoxtralBackend::new(tmp.path(), DEFAULT_VOXTRAL_DELAY_MS).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no Voxtral model found"), "got: {msg}");
        assert!(
            msg.contains("--variant voxtral-mini-4b-realtime"),
            "got: {msg}"
        );
    }
}
