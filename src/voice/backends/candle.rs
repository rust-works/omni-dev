//! `CandleTranscriber` — pure-Rust Whisper backend on `candle`.
//!
//! Loads `openai/whisper-tiny.en` (config, BPE tokenizer, safetensors
//! weights) from a model directory chosen by
//! [`crate::voice::models::resolve_whisper_model_dir`] and runs greedy,
//! English-only, no-timestamps decode segment-by-segment.
//!
//! The model loading and greedy decode live in the crate-internal
//! [`WhisperEngine`] so the streaming backend
//! ([`crate::voice::backends::candle_streaming`]) reuses the exact same
//! inference path instead of re-implementing it (#974). Inference state
//! lives behind a [`Mutex`] because
//! [`candle_transformers::models::whisper::model::Whisper`]'s encoder and
//! decoder methods take `&mut self` (the decoder owns a per-segment KV
//! cache that two concurrent calls would corrupt). The
//! [`crate::voice::Transcriber`] trait exposes `&self`, so the lock is
//! correctness-critical, not just `Send + Sync` ceremony.
//!
//! The runtime choice is documented in ADR-0033 and was validated by the
//! spike in #813.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use byteorder::{ByteOrder, LittleEndian};
use candle_core::{Device, IndexOp, Tensor};
use candle_nn::{ops::softmax, VarBuilder};
use candle_transformers::models::whisper::{self as m, audio, Config};
use tokenizers::Tokenizer;
use ulid::Ulid;

use crate::voice::models::{ensure_model_present, REQUIRED_FILES};
use crate::voice::transcriber::{
    AudioInput, EndpointKind, EventStream, Transcriber, TranscriptEvent,
};

/// 80-bin mel-filter coefficients precomputed for the Whisper front-end,
/// vendored from the candle spike's `melfilters.bytes`. Little-endian f32.
const MEL_FILTERS_80: &[u8] = include_bytes!("candle_melfilters.bytes");

/// Lower bound used when taking the natural log of a chosen-token
/// probability — avoids `-inf` when softmax rounds a tail probability to
/// zero. `1e-20` is well below any value greedy decode would actually
/// select, so it never affects realistic confidences.
const LOG_PROB_FLOOR: f32 = 1e-20;

/// Shared candle-Whisper inference engine: model files, tokenizer, mel
/// filters, suppress mask, and the greedy decode loop.
///
/// Crate-internal seam between the batch [`CandleTranscriber`] and the
/// streaming [`crate::voice::backends::candle_streaming`] backend — both
/// hold an `Arc<WhisperEngine>` and run the identical mel → encoder →
/// greedy-decode pipeline.
pub(crate) struct WhisperEngine {
    model: Mutex<m::model::Whisper>,
    config: Config,
    tokenizer: Tokenizer,
    mel_filters: Vec<f32>,
    suppress: Tensor,
    device: Device,
    sot: u32,
    eot: u32,
    transcribe: u32,
    no_timestamps: u32,
}

impl WhisperEngine {
    /// Loads the three Whisper files from `model_dir`. Verifies all
    /// required files are present up front so missing-model errors carry
    /// the install hint specified by #802.
    pub(crate) fn load(model_dir: &Path) -> Result<Self> {
        ensure_model_present(model_dir)?;
        let config_path = model_dir.join(REQUIRED_FILES[0]);
        let tokenizer_path = model_dir.join(REQUIRED_FILES[1]);
        let weights_path = model_dir.join(REQUIRED_FILES[2]);

        let device = Device::Cpu;
        let config: Config = serde_json::from_str(
            &std::fs::read_to_string(&config_path)
                .with_context(|| format!("read Whisper config from {}", config_path.display()))?,
        )
        .with_context(|| format!("parse Whisper config at {}", config_path.display()))?;
        let tokenizer = Tokenizer::from_file(&tokenizer_path).map_err(|e| {
            anyhow!(
                "load Whisper tokenizer at {}: {e}",
                tokenizer_path.display()
            )
        })?;
        // `from_mmaped_safetensors` is unsafe because mmap'd files can be
        // mutated under us by another process. The weights are inside a
        // user-owned `~/.omni-dev/voice/models/` install directory; the
        // failure mode is "model file changed mid-load → tensors corrupt",
        // which we accept (same trust model as candle's own example).
        #[allow(unsafe_code)]
        let vb = unsafe {
            VarBuilder::from_mmaped_safetensors(&[&weights_path], m::DTYPE, &device)
                .with_context(|| format!("mmap Whisper weights at {}", weights_path.display()))?
        };
        let model = m::model::Whisper::load(&vb, config.clone())
            .with_context(|| "load Whisper model from safetensors")?;

        let mel_filters = load_mel_filters(config.num_mel_bins)?;
        let suppress = build_suppress_tensor(&config, &device)?;

        let sot = token_id(&tokenizer, m::SOT_TOKEN)?;
        let eot = token_id(&tokenizer, m::EOT_TOKEN)?;
        let transcribe = token_id(&tokenizer, m::TRANSCRIBE_TOKEN)?;
        let no_timestamps = token_id(&tokenizer, m::NO_TIMESTAMPS_TOKEN)?;

        Ok(Self {
            model: Mutex::new(model),
            config,
            tokenizer,
            mel_filters,
            suppress,
            device,
            sot,
            eot,
            transcribe,
            no_timestamps,
        })
    }

    /// Builds the `(1, num_mel_bins, frames)` mel tensor for `pcm`.
    fn mel_tensor(&self, pcm: &[f32]) -> Result<Tensor> {
        let mel = audio::pcm_to_mel(&self.config, pcm, &self.mel_filters);
        let mel_len = mel.len();
        Tensor::from_vec(
            mel,
            (
                1,
                self.config.num_mel_bins,
                mel_len / self.config.num_mel_bins,
            ),
            &self.device,
        )
        .context("build mel tensor")
    }

    /// Acquires the model lock, mapping poisoning to an error.
    fn lock_model(&self) -> Result<MutexGuard<'_, m::model::Whisper>> {
        self.model
            .lock()
            .map_err(|e| anyhow!("candle Whisper mutex poisoned: {e}"))
    }

    /// Greedy-decodes one ≤[`m::N_FRAMES`] mel segment. Returns the segment
    /// text plus `(sum_logprob, n_decoded)` so callers can aggregate
    /// confidence across segments.
    fn decode_segment(
        &self,
        model: &mut m::model::Whisper,
        mel_segment: &Tensor,
    ) -> Result<(String, f64, usize)> {
        let audio_features = model
            .encoder
            .forward(mel_segment, true)
            .context("encoder forward")?;
        let mut tokens: Vec<u32> = vec![self.sot, self.transcribe, self.no_timestamps];
        let sample_len = self.config.max_target_positions / 2;
        let mut sum_logprob: f64 = 0.0;
        let mut n_decoded: usize = 0;

        for i in 0..sample_len {
            let tokens_t = Tensor::new(tokens.as_slice(), &self.device)
                .context("build tokens tensor")?
                .unsqueeze(0)
                .context("unsqueeze tokens tensor")?;
            let ys = model
                .decoder
                .forward(&tokens_t, &audio_features, i == 0)
                .context("decoder forward")?;
            let (_, seq_len, _) = ys.dims3().context("decoder output dims")?;
            let logits = model
                .decoder
                .final_linear(
                    &ys.i((..1, seq_len - 1..))
                        .context("slice last decoder step")?,
                )
                .context("decoder final_linear")?
                .i(0)
                .context("strip batch dim")?
                .i(0)
                .context("strip seq dim")?;
            let logits = logits
                .broadcast_add(&self.suppress)
                .context("apply suppress mask")?;
            let probs = softmax(&logits, candle_core::D::Minus1).context("softmax logits")?;
            let probs_v: Vec<f32> = probs.to_vec1().context("probs to host")?;
            #[allow(clippy::cast_possible_truncation)]
            let (next_idx, next_prob) = probs_v
                .iter()
                .enumerate()
                .max_by(|(_, a), (_, b)| a.total_cmp(b))
                .map(|(i, p)| (i as u32, *p))
                .ok_or_else(|| anyhow!("empty probability distribution"))?;
            sum_logprob += f64::from(next_prob.max(LOG_PROB_FLOOR).ln());
            n_decoded += 1;
            if next_idx == self.eot {
                break;
            }
            tokens.push(next_idx);
            if tokens.len() > self.config.max_target_positions {
                break;
            }
        }

        let segment_tokens = &tokens[3..];
        let text = self
            .tokenizer
            .decode(segment_tokens, true)
            .map_err(|e| anyhow!("decode segment tokens: {e}"))?;
        Ok((text, sum_logprob, n_decoded))
    }

    /// Transcribes a whole PCM window (f32 samples in `[-1, 1]` at 16 kHz)
    /// and returns `(text, confidence)`. The streaming backend's per-window
    /// entry point: locks the model for the duration of the window's
    /// segment loop (one segment in practice — streaming windows are capped
    /// well below 30 s — but the loop defends against larger caps).
    pub(crate) fn decode_pcm(&self, pcm: &[f32]) -> Result<(String, f32)> {
        if pcm.is_empty() {
            return Ok((String::new(), 0.0));
        }
        let mel = self.mel_tensor(pcm)?;
        let (_, _, content_frames) = mel.dims3().context("mel tensor dims")?;

        let mut model = self.lock_model()?;
        let mut full_text = String::new();
        let mut total_logprob: f64 = 0.0;
        let mut total_decoded: usize = 0;
        let mut seek = 0usize;
        while seek < content_frames {
            let segment_size = usize::min(content_frames - seek, m::N_FRAMES);
            let mel_segment = mel
                .narrow(2, seek, segment_size)
                .context("narrow mel to segment window")?;
            seek += segment_size;
            let (text, sum_logprob, n_decoded) = self.decode_segment(&mut model, &mel_segment)?;
            if !full_text.is_empty() && !text.is_empty() {
                full_text.push(' ');
            }
            full_text.push_str(&text);
            total_logprob += sum_logprob;
            total_decoded += n_decoded;
        }
        drop(model);

        Ok((
            full_text.trim().to_string(),
            confidence_from(total_logprob, total_decoded),
        ))
    }
}

/// Converts an accumulated decode-loop log-probability into a segment
/// confidence: `exp(avg logprob)` clamped to `[0, 1]`.
fn confidence_from(sum_logprob: f64, n_decoded: usize) -> f32 {
    if n_decoded > 0 {
        #[allow(clippy::cast_possible_truncation)]
        let avg = (sum_logprob / n_decoded as f64) as f32;
        avg.exp().clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Whisper backend built on the `candle` framework.
pub struct CandleTranscriber {
    engine: Arc<WhisperEngine>,
}

impl CandleTranscriber {
    /// Builds a transcriber by loading the three Whisper files from
    /// `model_dir`. Verifies all required files are present up front so
    /// missing-model errors carry the install hint specified by #802.
    pub fn new(model_dir: &Path) -> Result<Self> {
        Ok(Self {
            engine: Arc::new(WhisperEngine::load(model_dir)?),
        })
    }
}

impl Transcriber for CandleTranscriber {
    fn transcribe(&self, mut audio: Box<dyn AudioInput>) -> Result<Box<dyn EventStream>> {
        // Drain i16 chunks; convert to f32 in [-1, 1] as Whisper expects.
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
        let total_duration = Duration::from_secs_f64(total_samples as f64 / m::SAMPLE_RATE as f64);

        // If we got no audio at all, emit just the terminal endpoint.
        if pcm.is_empty() {
            let events = vec![Ok(TranscriptEvent::Endpoint {
                at: total_duration,
                kind: EndpointKind::StreamEnd,
            })];
            return Ok(Box::new(events.into_iter()));
        }

        let mel = self.engine.mel_tensor(&pcm)?;
        let mut model = self.engine.lock_model()?;

        let (_, _, content_frames) = mel.dims3().context("mel tensor dims")?;
        let mut events: Vec<Result<TranscriptEvent>> = Vec::new();
        let mut seek = 0usize;

        while seek < content_frames {
            let segment_start_seek = seek;
            let segment_size = usize::min(content_frames - seek, m::N_FRAMES);
            let mel_segment = mel
                .narrow(2, seek, segment_size)
                .context("narrow mel to segment window")?;
            seek += segment_size;

            let (text, sum_logprob, n_decoded) =
                self.engine.decode_segment(&mut model, &mel_segment)?;

            #[allow(clippy::cast_precision_loss)]
            let start = Duration::from_secs_f64(
                (segment_start_seek * m::HOP_LENGTH) as f64 / m::SAMPLE_RATE as f64,
            );
            #[allow(clippy::cast_precision_loss)]
            let end =
                Duration::from_secs_f64((seek * m::HOP_LENGTH) as f64 / m::SAMPLE_RATE as f64);

            events.push(Ok(TranscriptEvent::Final {
                event_id: Ulid::new(),
                text,
                start,
                end,
                confidence: confidence_from(sum_logprob, n_decoded),
                words: None,
                speaker: None,
                revisable: false,
            }));
        }

        // Lock dropped before pushing the terminal endpoint to keep the
        // critical section narrow.
        drop(model);

        events.push(Ok(TranscriptEvent::Endpoint {
            at: total_duration,
            kind: EndpointKind::StreamEnd,
        }));
        Ok(Box::new(events.into_iter()))
    }
}

fn load_mel_filters(num_mel_bins: usize) -> Result<Vec<f32>> {
    if num_mel_bins != 80 {
        bail!("whisper-candle ships 80-bin mel filters only (got {num_mel_bins})");
    }
    let mut filters = vec![0f32; MEL_FILTERS_80.len() / 4];
    LittleEndian::read_f32_into(MEL_FILTERS_80, &mut filters);
    Ok(filters)
}

fn build_suppress_tensor(config: &Config, device: &Device) -> Result<Tensor> {
    let mask: Vec<f32> = (0..config.vocab_size as u32)
        .map(|i| {
            if config.suppress_tokens.contains(&i) {
                f32::NEG_INFINITY
            } else {
                0f32
            }
        })
        .collect();
    Tensor::new(mask.as_slice(), device).context("build suppress-tokens tensor")
}

fn token_id(tokenizer: &Tokenizer, token: &str) -> Result<u32> {
    tokenizer
        .token_to_id(token)
        .ok_or_else(|| anyhow!("Whisper tokenizer is missing required token {token}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn candle_transcriber_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CandleTranscriber>();
        assert_send_sync::<WhisperEngine>();
    }

    #[test]
    fn load_mel_filters_returns_80_bin_blob() {
        let filters = load_mel_filters(80).unwrap();
        // The vendored blob is 64320 bytes = 16080 f32s. 16080 / 80 = 201
        // FFT bins per mel band — matches Whisper's n_fft=400 layout.
        assert_eq!(filters.len(), 16_080);
    }

    #[test]
    fn load_mel_filters_rejects_other_bin_counts() {
        let err = load_mel_filters(128).unwrap_err();
        assert!(err.to_string().contains("80-bin"), "got: {err}");
    }

    #[test]
    fn new_errors_with_install_hint_when_dir_empty() {
        let tmp = tempfile::TempDir::new().unwrap();
        let Err(err) = CandleTranscriber::new(tmp.path()) else {
            panic!("empty model dir should fail the ensure_model_present check");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("no Whisper model found"), "got: {msg}");
        assert!(msg.contains("voice install-model"), "got: {msg}");
    }

    fn resolve_model_dir() -> Option<std::path::PathBuf> {
        if let Ok(env) = std::env::var("OMNI_DEV_VOICE_WHISPER_MODEL") {
            if !env.is_empty() {
                return Some(std::path::PathBuf::from(env));
            }
        }
        crate::voice::models::default_whisper_model_dir()
            .filter(|d| ensure_model_present(d).is_ok())
    }

    fn load_engine() -> WhisperEngine {
        let dir = resolve_model_dir().expect(
            "Whisper model not found. Run `omni-dev voice install-model` or set \
             OMNI_DEV_VOICE_WHISPER_MODEL=<path>.",
        );
        WhisperEngine::load(&dir).expect("WhisperEngine::load should succeed")
    }

    #[test]
    #[ignore = "requires Whisper tiny.en model on disk; run `omni-dev voice install-model` first"]
    fn decode_pcm_empty_input_returns_empty_text() {
        let engine = load_engine();
        let (text, confidence) = engine.decode_pcm(&[]).unwrap();
        assert!(text.is_empty());
        assert!(confidence.abs() < f32::EPSILON);
    }

    #[test]
    #[ignore = "requires Whisper tiny.en model on disk; run `omni-dev voice install-model` first"]
    fn decode_pcm_joins_multi_segment_windows() {
        // A >30 s window spans two N_FRAMES segments, exercising the
        // segment loop and the space-join between segment texts (the
        // streaming backend's ≤5 s windows never reach this path).
        let engine = load_engine();
        let wav = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/voice/monologue_5min.wav");
        let mut input = crate::voice::transcriber::VecAudioInput::from_wav_path(wav, 16_000)
            .expect("fixture should load");
        let mut pcm: Vec<f32> = Vec::new();
        while let Some(chunk) = input.next_chunk() {
            pcm.extend(chunk.iter().map(|&s| f32::from(s) / 32768.0));
            if pcm.len() >= 35 * 16_000 {
                break;
            }
        }
        let (text, confidence) = engine.decode_pcm(&pcm).unwrap();
        assert!(
            text.len() > 200,
            "35 s of speech should decode to substantial text, got: {text:?}"
        );
        assert!(confidence > 0.0 && confidence <= 1.0, "got: {confidence}");
    }

    #[test]
    fn confidence_from_zero_decoded_is_zero() {
        assert!(confidence_from(0.0, 0).abs() < f32::EPSILON);
    }

    #[test]
    fn confidence_from_avg_logprob_is_clamped_unit_interval() {
        // avg logprob 0 → exp(0) = 1.0; very negative → near 0.
        assert!((confidence_from(0.0, 3) - 1.0).abs() < f32::EPSILON);
        let low = confidence_from(-100.0, 2);
        assert!((0.0..=0.001).contains(&low), "got: {low}");
    }
}
