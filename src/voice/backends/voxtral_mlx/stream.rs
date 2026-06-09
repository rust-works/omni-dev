//! The streaming transcription session (#933 M3b) — a port of `mlx-audio`'s
//! `VoxtralStreamingSession`.
//!
//! Drives the same pipeline as the offline [`super::model::VoxtralMlxModel::transcribe`]
//! incrementally, emitting token text as audio arrives:
//!
//! - **Mel** ([`super::mel::mel_frames`]) — computed per newly-available frame over
//!   a buffer that begins with the offline left silence pad, so frame indices
//!   match the batch path.
//! - **Conv stem** ([`AudioEncoder::stream_conv_stem`]) — stateful causal convs.
//! - **Encoder** ([`AudioEncoder::stream_encode`]) — per-layer rotating KV cache.
//! - **Downsample/adapter** — complete 4-frame groups projected; the `<4`
//!   remainder carried across steps.
//! - **Decoder** — prefill once `prompt_len` adapter tokens exist, then one greedy
//!   step per new adapter token (`adapter[pos] + embed(prev)`), incrementally
//!   decoding bytes to UTF-8 text.
//!
//! Result parity: the concatenated streamed text equals the offline transcript
//! (validated by `streaming_matches_batch_*`), so streaming WER ≈ batch WER.

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use mlx_rs::ops::indexing::IndexOp;
use mlx_rs::ops::{concatenate, indexing::argmax};
use mlx_rs::Array;

use super::config::VoxtralMlxConfig;
use super::decoder::{Decoder, KvCache};
use super::encoder::{AudioEncoder, ConvStemState, StreamEncState};
use super::mel;
use super::model::{num_delay_tokens, BOS_TOKEN_ID, N_LEFT_PAD_TOKENS, STREAMING_PAD_TOKEN_ID};
use super::nn::Weights;
use super::tokenizer::TekkenTokenizer;

/// Greedy `argmax` over a `[.., vocab]` logits row → token id (shared with the
/// offline path).
pub(crate) fn argmax_token(logits: &Array) -> Result<i64> {
    let axis = (logits.ndim() - 1) as i32;
    let idx = argmax(logits, axis, false)?;
    idx.eval()?;
    Ok(i64::from(idx.item::<u32>()))
}

/// Buffers decoded bytes and releases the longest valid UTF-8 prefix, so a
/// multi-byte character split across tokens is never emitted half-formed.
#[derive(Default)]
struct IncrementalUtf8 {
    buf: Vec<u8>,
}

impl IncrementalUtf8 {
    /// Appends `bytes` and returns any newly-complete text.
    fn push(&mut self, bytes: &[u8]) -> Option<String> {
        if bytes.is_empty() {
            return None;
        }
        self.buf.extend_from_slice(bytes);
        let valid = match std::str::from_utf8(&self.buf) {
            Ok(s) => s.len(),
            Err(e) => e.valid_up_to(),
        };
        if valid == 0 {
            return None;
        }
        let s = String::from_utf8_lossy(&self.buf[..valid]).into_owned();
        self.buf.drain(..valid);
        Some(s)
    }

    /// Flushes any trailing bytes (lossily) at end of stream.
    fn flush(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        let s = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        Some(s)
    }
}

/// A stateful streaming transcription session borrowing a model's weights.
pub struct StreamSession<'a> {
    enc: AudioEncoder<'a>,
    dec: Decoder<'a>,
    tokenizer: &'a TekkenTokenizer,
    cfg: VoxtralMlxConfig,

    n_left: usize,
    n_delay: usize,
    prompt_len: usize,

    // Audio + front-end state.
    audio: Vec<f32>,
    real_samples: usize,
    mel_done: usize,
    conv_state: ConvStemState,
    enc_state: StreamEncState,
    norm_leftover: Option<Array>,

    // Adapter + decoder state.
    adapter: Option<Array>,
    ada: Vec<Array>,
    caches: Vec<KvCache>,
    prefilled: bool,
    prev_token: i64,
    decoded: usize,
    utf8: IncrementalUtf8,
    finished: bool,
}

impl<'a> StreamSession<'a> {
    /// Builds a session over `weights`/`cfg`/`tokenizer` with the decoder delay
    /// `delay_ms`. The audio buffer is seeded with the offline left pad so frame
    /// indices (and therefore the prompt alignment) match the batch path.
    pub fn new(
        weights: &'a HashMap<String, Array>,
        cfg: VoxtralMlxConfig,
        tokenizer: &'a TekkenTokenizer,
        delay_ms: u32,
    ) -> Self {
        let enc = AudioEncoder::new(
            Weights::new(weights, cfg.quant.group_size, cfg.quant.bits),
            cfg.encoder,
        );
        let dec = Decoder::new(
            Weights::new(weights, cfg.quant.group_size, cfg.quant.bits),
            cfg.decoder,
        );
        let n_left = N_LEFT_PAD_TOKENS;
        let n_delay = num_delay_tokens(delay_ms);
        let prompt_len = 1 + n_left + n_delay;
        let enc_state = StreamEncState::new(cfg.encoder.n_layers);
        let caches = dec.make_cache();
        // Left silence pad (offline `n_left` tokens) + mel front pad, so mel frame
        // 0 is centered exactly as in the offline path.
        let audio = vec![0.0_f32; mel::MEL_FRONT_PAD + n_left * mel::RAW_AUDIO_PER_TOK];

        Self {
            enc,
            dec,
            tokenizer,
            cfg,
            n_left,
            n_delay,
            prompt_len,
            audio,
            real_samples: 0,
            mel_done: 0,
            conv_state: ConvStemState::default(),
            enc_state,
            norm_leftover: None,
            adapter: None,
            ada: Vec::new(),
            caches,
            prefilled: false,
            prev_token: 0,
            decoded: 0,
            utf8: IncrementalUtf8::default(),
            finished: false,
        }
    }

    /// Feeds 16 kHz mono `samples`, returning any newly decoded text.
    pub fn feed(&mut self, samples: &[f32]) -> Result<Vec<String>> {
        self.audio.extend_from_slice(samples);
        self.real_samples += samples.len();
        let mut out = Vec::new();
        self.advance(&mut out)?;
        Ok(out)
    }

    /// Ends the stream: appends the offline right pad so the decoder can drain the
    /// delay window, decodes the remainder, and returns the final text.
    pub fn finish(&mut self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        if !self.finished {
            self.finished = true;
            let align = (mel::RAW_AUDIO_PER_TOK - self.real_samples % mel::RAW_AUDIO_PER_TOK)
                % mel::RAW_AUDIO_PER_TOK;
            let n_right = (self.n_delay + 1) + 10;
            let pad = align + n_right * mel::RAW_AUDIO_PER_TOK;
            self.audio.extend(std::iter::repeat(0.0_f32).take(pad));
            self.advance(&mut out)?;
            // The final pending token (the reference's `for/else` tail).
            if self.prefilled {
                if let Some(s) = self.utf8.push(self.tokenizer.token_bytes(self.prev_token)) {
                    out.push(s);
                }
            }
            if let Some(s) = self.utf8.flush() {
                out.push(s);
            }
        }
        Ok(out)
    }

    /// Runs the front-end + decoder over whatever new audio is now available.
    fn advance(&mut self, out: &mut Vec<String>) -> Result<()> {
        let mel_avail = mel::mel_frames_available(self.audio.len());
        if mel_avail > self.mel_done {
            let raw = mel::mel_frames(
                &self.audio,
                self.mel_done,
                mel_avail,
                self.cfg.audio.global_log_mel_max,
            );
            let n = mel_avail - self.mel_done;
            self.mel_done = mel_avail;
            let mel_arr = Array::from_slice(&raw, &[n as i32, self.cfg.audio.num_mel_bins as i32]);
            let conv = self.enc.stream_conv_stem(&mut self.conv_state, &mel_arr)?;
            if conv.shape()[0] > 0 {
                let normed = self.enc.stream_encode(&mut self.enc_state, &conv)?;
                self.ingest_normed(&normed)?;
            }
        }
        self.decode_available(out)
    }

    /// Accumulates encoder-normed frames, projecting complete 4-frame groups into
    /// adapter tokens and carrying the `<4`-frame remainder.
    fn ingest_normed(&mut self, normed: &Array) -> Result<()> {
        let combined = match self.norm_leftover.take() {
            Some(l) => concatenate(&[&l, normed], 0).map_err(|e| anyhow!("norm concat: {e}"))?,
            None => normed.clone(),
        };
        let ds = self.cfg.encoder.downsample_factor as i32;
        let k = combined.shape()[0];
        let groups = k / ds;
        if groups > 0 {
            let new = self
                .enc
                .project_downsampled(&combined.index(0..groups * ds))?;
            self.adapter = Some(match self.adapter.take() {
                Some(a) => {
                    concatenate(&[&a, &new], 0).map_err(|e| anyhow!("adapter concat: {e}"))?
                }
                None => new,
            });
        }
        if groups * ds < k {
            self.norm_leftover = Some(combined.index((groups * ds)..k));
        }
        Ok(())
    }

    /// Prefills once enough adapter tokens exist, then greedily decodes one token
    /// per available adapter position, emitting decoded text.
    fn decode_available(&mut self, out: &mut Vec<String>) -> Result<()> {
        let adapter_done = self.adapter.as_ref().map_or(0, |a| a.shape()[0] as usize);

        if !self.prefilled && adapter_done >= self.prompt_len {
            self.prefill()?;
        }
        if !self.prefilled {
            return Ok(());
        }

        let adapter = self
            .adapter
            .as_ref()
            .ok_or_else(|| anyhow!("decode_available: adapter missing after prefill"))?;
        while self.decoded < adapter_done {
            let token = self.prev_token;
            if let Some(s) = self.utf8.push(self.tokenizer.token_bytes(token)) {
                out.push(s);
            }
            // Decode step at position `decoded`: adapter[pos] + embed(prev).
            let pos = self.decoded as i32;
            let audio_emb = adapter.index(pos..(pos + 1));
            let tok_emb = self.dec.embed_tokens(&[token as i32])?;
            let embed = audio_emb.add(&tok_emb)?;
            let h = self.dec.forward(&embed, pos, &self.ada, &mut self.caches)?;
            self.prev_token = argmax_token(&self.dec.logits(&h)?)?;
            self.decoded += 1;
        }
        Ok(())
    }

    /// Decoder prefill on `adapter[:prompt_len] + embed([BOS] + [PAD]×…)`.
    fn prefill(&mut self) -> Result<()> {
        self.ada = self.dec.precompute_ada_gains(self.n_delay as f32)?;
        let mut prompt_ids = vec![BOS_TOKEN_ID as i32];
        prompt_ids
            .extend(std::iter::repeat(STREAMING_PAD_TOKEN_ID).take(self.n_left + self.n_delay));
        let text = self.dec.embed_tokens(&prompt_ids)?;
        let adapter = self
            .adapter
            .as_ref()
            .ok_or_else(|| anyhow!("prefill: adapter missing"))?;
        let prefix = adapter.index(0..self.prompt_len as i32).add(&text)?;
        let h = self.dec.forward(&prefix, 0, &self.ada, &mut self.caches)?;
        let last = h.index((self.prompt_len as i32 - 1)..self.prompt_len as i32);
        self.prev_token = argmax_token(&self.dec.logits(&last)?)?;
        self.prefilled = true;
        self.decoded = self.prompt_len;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn incremental_utf8_releases_only_complete_chars() {
        let mut u = IncrementalUtf8::default();
        // "é" is 0xC3 0xA9; feeding the first byte alone yields nothing.
        assert_eq!(u.push(&[0xC3]), None);
        assert_eq!(u.push(&[0xA9]).as_deref(), Some("é"));
        assert_eq!(u.push(b"hi").as_deref(), Some("hi"));
        assert_eq!(u.flush(), None);
    }

    fn model_dir() -> Option<std::path::PathBuf> {
        std::env::var("OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
    }

    /// Streaming the audio in small chunks must reproduce the offline transcript
    /// (the streaming front-end/encoder/decoder parity contract) — so streaming
    /// WER ≈ batch WER. Feeds `short_en.wav` in 100 ms chunks and asserts the
    /// streamed text equals the batch text. (#933 M3b)
    #[test]
    #[ignore = "requires the INT4 Voxtral model; set OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL=<dir> (#933 M3b)"]
    fn streaming_matches_batch_on_short_en() {
        use super::super::model::{load_wav_16k_mono, VoxtralMlxModel};

        let Some(dir) = model_dir() else {
            panic!("set OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL=<dir>");
        };
        let model = VoxtralMlxModel::from_model_dir(&dir).expect("load model");
        let wav = std::path::Path::new("tests/fixtures/voice/short_en.wav");
        let samples = load_wav_16k_mono(wav).expect("load short_en");

        let batch = model.transcribe(&samples).expect("batch");

        let mut session = model.stream_session();
        let mut streamed = String::new();
        for chunk in samples.chunks(1600) {
            for s in session.feed(chunk).expect("feed") {
                streamed.push_str(&s);
            }
        }
        for s in session.finish().expect("finish") {
            streamed.push_str(&s);
        }
        let streamed = streamed.trim().to_string();

        println!("\n=== batch    : {batch}\n=== streaming: {streamed}\n");
        assert_eq!(streamed, batch, "streamed transcript must equal batch");
    }
}
