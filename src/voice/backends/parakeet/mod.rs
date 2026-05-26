//! Parakeet-TDT-0.6B-v2 backend on `candle`.
//!
//! Pure-Rust port of `mlx-community/parakeet-tdt-0.6b-v2` against the
//! `candle 0.10.x` runtime — FastConformer encoder + TDT decoder + joiner,
//! 600 M params, English-only ASR. The public surface is
//! [`CandleParakeetTranscriber`], implementing
//! [`crate::voice::Transcriber`] (batch). The streaming
//! `StreamingTranscriber` impl lives in [`streaming`] (lands in a
//! subsequent commit).
//!
//! Architecture rationale: ADR-0033 (candle for ASR), and the #871
//! feasibility spike's GO recommendation.

pub mod attention;
pub mod audio;
pub mod conformer_block;
pub mod conv_module;
pub mod decoder;
pub mod encoder;
pub mod streaming;
pub mod tokenizer;
pub mod weights;

use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use candle_core::{Device, Tensor};
use ulid::Ulid;

use crate::voice::transcriber::{
    AudioInput, EndpointKind, EventStream, Transcriber, TranscriptEvent,
};

use self::audio::{ParakeetMel, SAMPLE_RATE};
use self::decoder::{TdtDecoder, PARAKEET_TDT_0_6B_V2};
use self::encoder::{FastConformerEncoder, PARAKEET_0_6B_V2};
use self::tokenizer::ParakeetTokenizer;
use self::weights::open_safetensors;

/// Standard filenames the install pipeline writes into the model
/// directory. The order here is documentation only; the loader picks
/// files by name.
pub const REQUIRED_FILES: &[&str] = &[
    "config.json",
    "tokenizer.json",
    "candle_weights.safetensors",
    "ATTRIBUTION.txt",
];

/// Pure-Rust batch Parakeet backend.
///
/// Loads the encoder + decoder + tokenizer + mel front-end once at
/// construction. Per-call inference state (LSTM hidden, encoder output)
/// is short-lived and rebuilt from scratch each `transcribe` call —
/// streaming-style state threading is the [`crate::voice::StreamingTranscriber`]
/// impl's job.
///
/// Inference state lives behind a [`Mutex`] for parity with
/// [`crate::voice::backends::candle::CandleTranscriber`] — the
/// [`Transcriber`] trait exposes `&self` but we want the option to
/// extend the backend with mutable per-call state without changing the
/// trait. Today the encoder and decoder use `&self` internally, so the
/// lock is a no-op; keeping it makes the streaming-state migration in
/// the next commit a one-line addition.
pub struct CandleParakeetTranscriber {
    encoder: Mutex<FastConformerEncoder>,
    decoder: Mutex<TdtDecoder>,
    tokenizer: ParakeetTokenizer,
    mel: ParakeetMel,
    device: Device,
}

impl CandleParakeetTranscriber {
    /// Loads the model from `model_dir`. Expects the four files in
    /// [`REQUIRED_FILES`] under that directory (set up by
    /// `voice install-model parakeet-tdt-0.6b-v2`).
    pub fn new(model_dir: &Path) -> Result<Self> {
        for f in REQUIRED_FILES {
            let p = model_dir.join(f);
            if !p.is_file() {
                return Err(anyhow!(
                    "no Parakeet model found at {} (missing {}); \
                     run `omni-dev voice install-model --variant parakeet-tdt-0.6b-v2` \
                     or pass --model <path>",
                    model_dir.display(),
                    f
                ));
            }
        }
        let weights_path = model_dir.join("candle_weights.safetensors");
        let tokenizer_path = model_dir.join("tokenizer.json");

        let device = Device::Cpu;
        let vb = open_safetensors(&weights_path, &device).context("open Parakeet weights")?;

        let encoder_cfg = PARAKEET_0_6B_V2;
        let decoder_cfg = PARAKEET_TDT_0_6B_V2;
        let encoder = FastConformerEncoder::load(vb.pp("encoder"), &encoder_cfg, &device)
            .context("load Parakeet encoder")?;
        let decoder =
            TdtDecoder::load(vb.clone(), &decoder_cfg).context("load Parakeet decoder")?;
        let tokenizer = ParakeetTokenizer::from_file(&tokenizer_path)?;
        let mel = ParakeetMel::new().context("build Parakeet mel front-end")?;

        Ok(Self {
            encoder: Mutex::new(encoder),
            decoder: Mutex::new(decoder),
            tokenizer,
            mel,
            device,
        })
    }
}

impl Transcriber for CandleParakeetTranscriber {
    fn transcribe(&self, mut audio: Box<dyn AudioInput>) -> Result<Box<dyn EventStream>> {
        // Drain i16 chunks; convert to f32 in [-1, 1].
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
        let total_duration = Duration::from_secs_f64(total_samples as f64 / f64::from(SAMPLE_RATE));

        if pcm.is_empty() {
            let events = vec![Ok(TranscriptEvent::Endpoint {
                at: total_duration,
                kind: EndpointKind::StreamEnd,
            })];
            return Ok(Box::new(events.into_iter()));
        }

        // Mel front-end: (n_frames, n_mels=80) -> (1, T, 80) tensor.
        let mel_frames = self.mel.batch(&pcm).context("mel front-end")?;
        let mel_tensor = Tensor::from_vec(
            mel_frames.data,
            (1, mel_frames.n_frames, mel_frames.n_mels),
            &self.device,
        )
        .context("build mel tensor")?;

        // Encoder: (1, T, 80) -> (1, T', d_model=1024).
        let encoder_out = {
            let encoder = self
                .encoder
                .lock()
                .map_err(|e| anyhow!("Parakeet encoder mutex poisoned: {e}"))?;
            encoder.forward(&mel_tensor).context("encoder forward")?
        };

        // Decoder: greedy TDT over the encoder output -> token ids.
        let tokens = {
            let decoder = self
                .decoder
                .lock()
                .map_err(|e| anyhow!("Parakeet decoder mutex poisoned: {e}"))?;
            decoder
                .decode_greedy(&encoder_out)
                .context("decoder greedy")?
        };

        let text = self.tokenizer.decode(&tokens).context("decode tokens")?;

        // Confidence reporting: MLX uses an entropy-based score per
        // emission; the candle port could match it once the joiner
        // surfaces logits. Until then, report 1.0. Issue #898's
        // acceptance criteria don't gate on per-segment confidence.
        let events: Vec<Result<TranscriptEvent>> = vec![
            Ok(TranscriptEvent::Final {
                event_id: Ulid::new(),
                text,
                start: Duration::ZERO,
                end: total_duration,
                confidence: 1.0,
                words: None,
                speaker: None,
                revisable: false,
            }),
            Ok(TranscriptEvent::Endpoint {
                at: total_duration,
                kind: EndpointKind::StreamEnd,
            }),
        ];
        Ok(Box::new(events.into_iter()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn candle_parakeet_transcriber_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CandleParakeetTranscriber>();
    }

    #[test]
    fn new_errors_with_install_hint_when_dir_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let Err(err) = CandleParakeetTranscriber::new(tmp.path()) else {
            panic!("empty model dir should fail");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("no Parakeet model found"), "got: {msg}");
        assert!(msg.contains("voice install-model"), "got: {msg}");
        assert!(msg.contains("parakeet-tdt-0.6b-v2"), "got: {msg}");
    }
}
