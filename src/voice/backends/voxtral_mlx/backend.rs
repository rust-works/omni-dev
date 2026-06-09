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
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Result};
use ulid::Ulid;

use super::model::VoxtralMlxModel;
use crate::voice::transcriber::{
    AudioInput, EndpointKind, EventStream, Transcriber, TranscriptEvent,
};

/// Voxtral's fixed input sample rate (16 kHz mono).
const SAMPLE_RATE: f64 = 16_000.0;

/// Default decoder delay (lookahead) in ms — the #930 accuracy/latency sweet spot.
pub const DEFAULT_VOXTRAL_MLX_DELAY_MS: u32 = 480;

/// Segment confidence reported for every `Final`. The greedy decoder exposes no
/// calibrated per-token probability, so `1.0` is a placeholder meaning "not
/// provided", not a certainty (mirrors the `voxtral` backend).
const CONFIDENCE: f32 = 1.0;

/// Real-time INT4 Voxtral backend (MLX). Holds the loaded model behind a mutex.
pub struct VoxtralMlxBackend {
    model: Mutex<VoxtralMlxModel>,
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
            model: Mutex::new(model),
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
