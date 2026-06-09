//! The offline transcription driver — ties the mel front-end, encoder, decoder,
//! and Tekken tokenizer into a single audio → text pass.
//!
//! Port of `mlx-audio`'s `Model.generate` (non-streaming branch). The algorithm:
//!
//! 1. Pad audio (`n_left` = 32 left tokens, `n_right` = `n_delay + 11`), compute
//!    the log-mel, run the conv stem and the encoder → `adapter_out` (one audio
//!    embedding per `12.5 Hz` token).
//! 2. Build the prompt `[BOS] + [STREAMING_PAD] × (n_left + n_delay)` and prefill
//!    the decoder on `adapter_out[:prompt_len] + embed(prompt_ids)` (the
//!    audio-embedding and the token-embedding are **added**, not concatenated).
//! 3. Greedily decode one token per audio position: at `pos`, the decoder input
//!    is `adapter_out[pos] + embed(prev_token)`. Stop at EOS or when the audio
//!    runs out. Decode the collected ids to text.
//!
//! This is the offline driver; long audio is handled by
//! [`AudioEncoder::encode_conv`]'s chunked path (M3a), and the incremental
//! streaming counterpart lives in [`super::stream`] (M3b).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use mlx_rs::ops::indexing::IndexOp;
use mlx_rs::Array;

use super::config::VoxtralMlxConfig;
use super::decoder::Decoder;
use super::encoder::AudioEncoder;
use super::mel;
use super::nn::Weights;
use super::tokenizer::TekkenTokenizer;
use super::weights::load_safetensors;

// Stable special-token ids + streaming constants (config.py `AudioEncodingConfig`).
pub(crate) const BOS_TOKEN_ID: i64 = 1;
pub(crate) const EOS_TOKEN_ID: i64 = 2;
pub(crate) const STREAMING_PAD_TOKEN_ID: i32 = 32;
pub(crate) const N_LEFT_PAD_TOKENS: usize = 32;
const HOP_LENGTH: usize = 160;
const AUDIO_LENGTH_PER_TOK: usize = 8; // RAW_AUDIO_PER_TOK / HOP_LENGTH
const MAX_TOKENS: usize = 4096;

/// `_num_audio_tokens`: hop frames spanned by `audio_len`, then `÷ 8` (the
/// downsample to the 12.5 Hz token rate), rounding up.
fn num_audio_tokens(audio_len: usize) -> usize {
    let hops = if audio_len % HOP_LENGTH != 0 {
        ((audio_len as f64 / HOP_LENGTH as f64) - 1.0).ceil() as usize
    } else {
        audio_len / HOP_LENGTH
    };
    hops.div_ceil(AUDIO_LENGTH_PER_TOK)
}

/// `_num_delay_tokens`: decoder lag for a given delay in ms (480 ms → 6 tokens).
pub(crate) fn num_delay_tokens(delay_ms: u32) -> usize {
    let delay_len = (f64::from(delay_ms) / 1000.0 * 16_000.0) as usize;
    num_audio_tokens(delay_len)
}

/// A loaded Voxtral MLX model: the weight map, tokenizer, and config. The encoder
/// and decoder borrow the weights per transcription (they hold no owned state).
pub struct VoxtralMlxModel {
    weights: HashMap<String, Array>,
    tokenizer: TekkenTokenizer,
    cfg: VoxtralMlxConfig,
    delay_ms: u32,
}

impl VoxtralMlxModel {
    /// Loads the INT4 weights (`model.safetensors`) and `tekken.json` from a model
    /// directory, using the default decoder delay.
    pub fn from_model_dir(dir: &Path) -> Result<Self> {
        let cfg = VoxtralMlxConfig::voxtral_realtime_mini_4b();
        let weights = load_safetensors(&dir.join("model.safetensors"))?;
        let tokenizer = TekkenTokenizer::from_model_dir(dir)?;
        let delay_ms = cfg.default_delay_ms as u32;
        Ok(Self {
            weights,
            tokenizer,
            cfg,
            delay_ms,
        })
    }

    /// Overrides the decoder delay (lookahead) in milliseconds; the next
    /// transcription recomputes the ada-norm gains for it.
    pub fn set_delay_ms(&mut self, delay_ms: u32) {
        self.delay_ms = delay_ms;
    }

    /// Builds a streaming session borrowing this model's weights, config, and
    /// tokenizer for the lifetime of the stream (M3b).
    pub fn stream_session(&self) -> super::stream::StreamSession<'_> {
        super::stream::StreamSession::new(&self.weights, self.cfg, &self.tokenizer, self.delay_ms)
    }

    /// Transcribes 16 kHz mono `samples` to text (offline, greedy).
    pub fn transcribe(&self, samples: &[f32]) -> Result<String> {
        let n_delay = num_delay_tokens(self.delay_ms);
        let n_left = N_LEFT_PAD_TOKENS;
        let n_right = (n_delay + 1) + 10;
        let prompt_len = 1 + n_left + n_delay;

        // Mel front-end.
        let mel = mel::prepare_mel(samples, &self.cfg.audio, n_left, n_right);
        let mel_array = Array::from_slice(
            &mel.data,
            &[self.cfg.audio.num_mel_bins as i32, mel.frames as i32],
        );

        // Encoder → adapter embeddings (one per audio token).
        let enc = AudioEncoder::new(
            Weights::new(
                &self.weights,
                self.cfg.quant.group_size,
                self.cfg.quant.bits,
            ),
            self.cfg.encoder,
        );
        let conv_out = enc.conv_stem(&mel_array)?;
        let n_audio = (conv_out.shape()[0] / self.cfg.encoder.downsample_factor as i32) as usize;
        let adapter_out = enc.encode_conv(&conv_out)?; // [n_audio, dim]
        let adapter_len = adapter_out.shape()[0] as usize;

        if prompt_len > adapter_len {
            bail!(
                "audio too short: {adapter_len} audio tokens < prompt length {prompt_len} \
                 (needs ≳ {:.1}s of audio)",
                prompt_len as f32 / self.cfg.audio.frame_rate
            );
        }

        // Decoder setup.
        let dec = Decoder::new(
            Weights::new(
                &self.weights,
                self.cfg.quant.group_size,
                self.cfg.quant.bits,
            ),
            self.cfg.decoder,
        );
        let ada = dec.precompute_ada_gains(n_delay as f32)?;
        let mut caches = dec.make_cache();

        // Prefill: prefix_embeds = adapter_out[:prompt_len] + embed(prompt_ids).
        let mut prompt_ids = vec![BOS_TOKEN_ID as i32];
        prompt_ids.extend(std::iter::repeat(STREAMING_PAD_TOKEN_ID).take(n_left + n_delay));
        let text_embeds = dec.embed_tokens(&prompt_ids)?;
        let prefix = adapter_out.index(0..prompt_len as i32).add(&text_embeds)?;
        let h = dec.forward(&prefix, 0, &ada, &mut caches)?;
        let last = h.index((prompt_len as i32 - 1)..prompt_len as i32); // [1, dim]
        let mut token =
            crate::voice::backends::voxtral_mlx::stream::argmax_token(&dec.logits(&last)?)?;

        // Greedy decode: one token per audio position in [prompt_len, n_audio).
        let mut generated: Vec<i64> = Vec::new();
        let mut completed = true;
        for pos in prompt_len..n_audio {
            generated.push(token);
            if token == EOS_TOKEN_ID || generated.len() > MAX_TOKENS {
                completed = false;
                break;
            }
            let audio = adapter_out.index((pos as i32)..(pos as i32 + 1)); // [1, dim]
            let tok_embed = dec.embed_tokens(&[token as i32])?; // [1, dim]
            let embed = audio.add(&tok_embed)?;
            let h = dec.forward(&embed, pos as i32, &ada, &mut caches)?;
            token = crate::voice::backends::voxtral_mlx::stream::argmax_token(&dec.logits(&h)?)?;
        }
        if completed {
            // Loop ran to the end of the audio without an early stop: the last
            // pending token is still real output (the reference's `for/else`).
            generated.push(token);
        }
        if generated.last() == Some(&EOS_TOKEN_ID) {
            generated.pop();
        }

        Ok(self.tokenizer.decode(&generated).trim().to_string())
    }
}

/// Loads 16 kHz mono f32 samples from a WAV file (test/CLI helper).
pub fn load_wav_16k_mono(path: &Path) -> Result<Vec<f32>> {
    let mut reader =
        hound::WavReader::open(path).with_context(|| format!("open wav {}", path.display()))?;
    let spec = reader.spec();
    if spec.sample_rate != 16_000 || spec.channels != 1 {
        bail!(
            "expected 16 kHz mono, got {} Hz / {} ch",
            spec.sample_rate,
            spec.channels
        );
    }
    let samples = reader
        .samples::<i16>()
        .map(|s| s.map(|v| f32::from(v) / 32768.0))
        .collect::<std::result::Result<Vec<f32>, _>>()
        .map_err(|e| anyhow!("read wav samples: {e}"))?;
    Ok(samples)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn delay_and_audio_token_math_matches_reference() {
        assert_eq!(num_delay_tokens(480), 6);
        assert_eq!(num_audio_tokens(7680), 6);
        assert_eq!(num_audio_tokens(1280), 1);
    }

    fn model_dir() -> Option<std::path::PathBuf> {
        std::env::var("OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
    }

    /// Normalises text for WER: lowercase, keep alphanumerics + spaces, split.
    fn norm_words(s: &str) -> Vec<String> {
        s.to_lowercase()
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .map(str::to_string)
            .collect()
    }

    /// Word-level edit distance ÷ reference length.
    fn wer(reference: &[String], hypothesis: &[String]) -> f64 {
        let (n, m) = (reference.len(), hypothesis.len());
        let mut prev: Vec<usize> = (0..=m).collect();
        let mut cur = vec![0usize; m + 1];
        for i in 1..=n {
            cur[0] = i;
            for j in 1..=m {
                let cost = usize::from(reference[i - 1] != hypothesis[j - 1]);
                cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            }
            std::mem::swap(&mut prev, &mut cur);
        }
        prev[m] as f64 / n.max(1) as f64
    }

    /// WER of `hyp` against the best-length prefix of `reference` (the audio slice
    /// is a prefix of the full recording, so the transcript should match a prefix
    /// of the reference text; search a window around `len(hyp)` to fairly handle
    /// the cut boundary).
    fn best_prefix_wer(reference: &[String], hyp: &[String]) -> f64 {
        let h = hyp.len();
        let lo = h.saturating_sub(12).max(1);
        let hi = (h + 12).min(reference.len());
        (lo..=hi)
            .map(|k| wer(&reference[..k], hyp))
            .fold(f64::INFINITY, f64::min)
    }

    /// Batch accuracy + real-time-factor on a cap-sized (~32 s) prefix of the
    /// 5-min fixture (the full clip needs M3 chunking). Reports WER vs the
    /// best-aligned reference prefix and RTF (transcribe wall-clock ÷ audio
    /// duration). Run with `--release` for a meaningful RTF.
    #[test]
    #[ignore = "requires the INT4 Voxtral model; set OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL=<dir> (#933 M1.5)"]
    fn batch_wer_and_rtf_on_monologue_prefix() {
        let Some(dir) = model_dir() else {
            panic!("set OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL=<dir>");
        };
        let model = VoxtralMlxModel::from_model_dir(&dir).expect("load model");
        let wav = std::path::Path::new("tests/fixtures/voice/monologue_5min.wav");
        let all = load_wav_16k_mono(wav).expect("load monologue");

        // First ~32 s keeps the conv output under AudioEncoder::MAX_FULL_FRAMES.
        let slice_secs = 32.0_f64;
        let n = (slice_secs * 16_000.0) as usize;
        let samples = &all[..n.min(all.len())];
        let dur = samples.len() as f64 / 16_000.0;

        let start = std::time::Instant::now();
        let text = model.transcribe(samples).expect("transcribe");
        let elapsed = start.elapsed().as_secs_f64();
        let rtf = elapsed / dur;

        let reference = std::fs::read_to_string("tests/fixtures/voice/monologue_5min.expected.txt")
            .expect("read expected");
        let ref_words = norm_words(&reference);
        let hyp_words = norm_words(&text);
        let wer = best_prefix_wer(&ref_words, &hyp_words);

        println!("\n=== monologue {dur:.1}s prefix ===");
        println!("transcript: {text}");
        println!(
            "WER (best prefix): {:.1}%  |  RTF: {rtf:.3}  ({elapsed:.2}s / {dur:.1}s)",
            wer * 100.0
        );
        println!("=================================");
        assert!(wer < 0.15, "WER {:.1}% too high", wer * 100.0);
        assert!(!hyp_words.is_empty(), "empty transcript");
    }

    /// Full 5-minute fixture transcription + WER/RTF — exercises the chunked
    /// long-audio encoder (M3) and closes the M1.5 batch validation directly
    /// comparable to the `voxtral.c` BF16 numbers (WER 4.12%, RTF 1.25). Run with
    /// `--release` for a meaningful RTF.
    #[test]
    #[ignore = "requires the INT4 Voxtral model; set OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL=<dir> (#933 M3)"]
    fn full_monologue_wer_and_rtf() {
        let Some(dir) = model_dir() else {
            panic!("set OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL=<dir>");
        };
        let model = VoxtralMlxModel::from_model_dir(&dir).expect("load model");
        let wav = std::path::Path::new("tests/fixtures/voice/monologue_5min.wav");
        let samples = load_wav_16k_mono(wav).expect("load monologue");
        let dur = samples.len() as f64 / 16_000.0;

        let start = std::time::Instant::now();
        let text = model.transcribe(&samples).expect("transcribe");
        let elapsed = start.elapsed().as_secs_f64();
        let rtf = elapsed / dur;

        let reference = std::fs::read_to_string("tests/fixtures/voice/monologue_5min.expected.txt")
            .expect("read expected");
        let ref_words = norm_words(&reference);
        let hyp_words = norm_words(&text);
        let wer = wer(&ref_words, &hyp_words);

        println!("\n=== monologue {dur:.1}s (full) ===");
        println!("transcript ({} words): {text}", hyp_words.len());
        println!(
            "WER: {:.1}%  |  RTF: {rtf:.3}  ({elapsed:.1}s / {dur:.1}s)",
            wer * 100.0
        );
        println!("==================================");
        assert!(wer < 0.10, "WER {:.1}% too high", wer * 100.0);
    }

    /// End-to-end offline transcription on the short English fixture. Prints the
    /// transcript (inspect with `--nocapture`) and asserts it is non-empty and
    /// looks like real text — the M1.4/M1.5 deliverable (a correct offline
    /// transcript from the real INT4 model).
    #[test]
    #[ignore = "requires the INT4 Voxtral model; set OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL=<dir> (#933 M1.4)"]
    fn transcribes_short_en_fixture() {
        let Some(dir) = model_dir() else {
            panic!(
                "set OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL=<dir with model.safetensors + tekken.json>"
            );
        };
        let model = VoxtralMlxModel::from_model_dir(&dir).expect("load model");
        let wav = std::path::Path::new("tests/fixtures/voice/short_en.wav");
        let samples = load_wav_16k_mono(wav).expect("load short_en.wav");
        let text = model.transcribe(&samples).expect("transcribe");
        println!("\n=== short_en.wav transcript ===\n{text}\n===============================");
        assert!(!text.is_empty(), "transcript should not be empty");
        let letters = text.chars().filter(char::is_ascii_alphabetic).count();
        assert!(
            letters > 10,
            "transcript should contain real words: {text:?}"
        );
    }
}
