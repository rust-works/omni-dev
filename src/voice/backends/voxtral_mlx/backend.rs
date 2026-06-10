//! `VoxtralMlxBackend` — the batch [`Transcriber`] over the real-time INT4 MLX
//! port (ADR-0039).
//!
//! Selected by `--backend voxtral-mlx`. Compiled only with the off-by-default
//! `voxtral-mlx` feature on macOS Apple Silicon (the whole `voxtral_mlx` module
//! is so gated). Like [`crate::voice::backends::voxtral::VoxtralBackend`] it
//! implements the **batch** contract — drain the audio, run one offline
//! transcription pass, emit one `Final` plus a terminal `Endpoint`. Streaming
//! (`Partial` segmentation) lands in M3.
//!
//! [`mlx_rs::Array`] is `Send` but `!Sync`, so the model lives behind a
//! [`Mutex`] (the backend must be `Send + Sync` to be a `Transcriber`); the lock
//! also serialises the per-call decoder KV state, the same reason
//! `VoxtralBackend` and candle lock their engines.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures::channel::mpsc;
use ulid::Ulid;

use super::model::VoxtralMlxModel;
use crate::voice::idle::IdleDetector;
use crate::voice::segment::StreamSegmenter;
use crate::voice::transcriber::{
    AsyncAudioInput, AudioChunk, AudioInput, EndpointKind, EventStream, StreamingTranscriber,
    Transcriber, TranscriptEvent, TranscriptEventStream,
};

/// Voxtral's fixed input sample rate (16 kHz mono).
const SAMPLE_RATE: f64 = 16_000.0;

/// Consecutive silent windows that close a segment (mirrors the `voxtral` backend).
const SILENCE_GAP_WINDOWS: u32 = 7;

/// Default decoder delay (lookahead) in ms — the #930 accuracy/latency sweet spot.
pub const DEFAULT_VOXTRAL_MLX_DELAY_MS: u32 = 480;

/// Segment confidence reported for every `Final`. The greedy decoder exposes no
/// calibrated per-token probability, so `1.0` is a placeholder meaning "not
/// provided", not a certainty (mirrors the `voxtral` backend).
const CONFIDENCE: f32 = 1.0;

/// Real-time INT4 Voxtral backend (MLX). Holds the loaded model behind an
/// `Arc<Mutex<_>>` so a handle can move into the streaming inference thread.
///
/// The mutex also serialises inference: concurrent `transcribe` / streaming calls
/// on one backend queue behind it, and MLX itself drives a process-global Metal
/// device (so *independent* concurrent inferences across backends are unsafe).
/// Normal usage runs a single backend per command, so this is not a constraint
/// in practice.
pub struct VoxtralMlxBackend {
    model: Arc<Mutex<VoxtralMlxModel>>,
}

impl VoxtralMlxBackend {
    /// Loads the INT4 model + tokenizer from `model_dir` and applies `delay_ms`.
    ///
    /// Verifies the model files are present up front so a missing model carries
    /// the install hint (mirroring the other backends).
    pub fn new(model_dir: &Path, delay_ms: u32) -> Result<Self> {
        crate::voice::models::ensure_voxtral_mlx_model_present(model_dir)?;
        let mut model = VoxtralMlxModel::from_model_dir(model_dir)?;
        model.set_delay_ms(delay_ms);
        Ok(Self {
            model: Arc::new(Mutex::new(model)),
        })
    }
}

impl Transcriber for VoxtralMlxBackend {
    fn transcribe(&self, mut audio: Box<dyn AudioInput>) -> Result<Box<dyn EventStream>> {
        // Drain i16 chunks; convert to f32 in [-1, 1] (the front-end's input).
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

        let total_duration = Duration::from_secs_f64(total_samples as f64 / SAMPLE_RATE);

        // No audio at all → just the terminal endpoint (matches the others).
        if pcm.is_empty() {
            let events = vec![Ok(TranscriptEvent::Endpoint {
                at: total_duration,
                kind: EndpointKind::StreamEnd,
            })];
            return Ok(Box::new(events.into_iter()));
        }

        let text = {
            let model = self
                .model
                .lock()
                .map_err(|e| anyhow!("VoxtralMlxBackend model mutex poisoned: {e}"))?;
            model.transcribe(&pcm)?
        };

        let mut events: Vec<Result<TranscriptEvent>> = Vec::new();
        if !text.is_empty() {
            events.push(Ok(TranscriptEvent::Final {
                event_id: Ulid::new(),
                text,
                start: Duration::ZERO,
                end: total_duration,
                confidence: CONFIDENCE,
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

impl StreamingTranscriber for VoxtralMlxBackend {
    /// Drives the streaming session incrementally (ADR-0038). Same async↔blocking
    /// bridge as `VoxtralBackend`: an async **feeder** pulls chunks and a
    /// `spawn_blocking` **inference** task runs the (blocking, Metal) session,
    /// feeding decoded token text into a [`StreamSegmenter`] and emitting events.
    /// Must be called within a tokio runtime.
    fn transcribe_stream(&self, mut audio: Box<dyn AsyncAudioInput>) -> TranscriptEventStream {
        let (event_tx, event_rx) = mpsc::unbounded::<Result<TranscriptEvent>>();
        let (audio_tx, audio_rx) = std::sync::mpsc::channel::<AudioChunk>();
        let model = Arc::clone(&self.model);

        tokio::spawn(async move {
            while let Some(chunk) = audio.next_chunk().await {
                if audio_tx.send(chunk).is_err() {
                    break; // inference thread gone
                }
            }
        });

        tokio::task::spawn_blocking(move || run_stream(&model, &audio_rx, &event_tx));

        Box::pin(event_rx)
    }
}

/// The streaming inference loop, run on a blocking thread. Locks the model for
/// the stream's lifetime, builds a [`StreamSession`](super::stream::StreamSession),
/// and feeds decoded token text into a [`StreamSegmenter`], emitting Partials as
/// they form, a `Final` + `SilenceGap` at each silence boundary, and a final
/// `Final` + `Endpoint` at end of audio. Errors are forwarded and stop the stream.
fn run_stream(
    model: &Arc<Mutex<VoxtralMlxModel>>,
    audio_rx: &std::sync::mpsc::Receiver<AudioChunk>,
    event_tx: &mpsc::UnboundedSender<Result<TranscriptEvent>>,
) {
    let guard = match model.lock() {
        Ok(g) => g,
        Err(e) => {
            let _ = event_tx.unbounded_send(Err(anyhow!("VoxtralMlxBackend mutex poisoned: {e}")));
            return;
        }
    };
    let mut session = guard.stream_session();

    let mut idle = IdleDetector::new(1);
    let mut segmenter = StreamSegmenter::new(SILENCE_GAP_WINDOWS);
    let mut samples_fed: usize = 0;
    let now = |samples: usize| Duration::from_secs_f64(samples as f64 / SAMPLE_RATE);

    while let Ok(chunk) = audio_rx.recv() {
        let pcm: Vec<f32> = chunk.iter().map(|&s| f32::from(s) / 32768.0).collect();
        let classes = idle.push(&pcm);

        let tokens = match session.feed(&pcm) {
            Ok(t) => t,
            Err(e) => {
                let _ = event_tx.unbounded_send(Err(anyhow!("voxtral-mlx stream feed: {e}")));
                return;
            }
        };
        samples_fed += chunk.len();
        let t = now(samples_fed);

        if let Some(partial) = segmenter.push_tokens(&tokens, t) {
            if event_tx.unbounded_send(Ok(partial)).is_err() {
                return;
            }
        }
        // The silence audio itself flushes the decoder's delay window (the trailing
        // tokens are decoded as the silence flows in), so no extra flush is needed.
        if segmenter.observe_silence(&classes) {
            for ev in segmenter.commit_silence_gap(&[], t, CONFIDENCE) {
                if event_tx.unbounded_send(Ok(ev)).is_err() {
                    return;
                }
            }
        }
    }

    // Audio ended: drain the delay window and commit the tail.
    let tail = match session.finish() {
        Ok(t) => t,
        Err(e) => {
            let _ = event_tx.unbounded_send(Err(anyhow!("voxtral-mlx stream finish: {e}")));
            return;
        }
    };
    let t = now(samples_fed);
    for ev in segmenter.commit_end(&tail, t, CONFIDENCE) {
        if event_tx.unbounded_send(Ok(ev)).is_err() {
            return;
        }
    }
}
