//! The causal audio encoder — a direct port of `mlx-audio`'s
//! `voxtral_realtime/encoder.py`.
//!
//! Pipeline: log-mel `[128, frames]` → causal conv stem (128→1280 s1, 1280→1280
//! s2) → 32 transformer layers (interleaved RoPE θ=1M, sliding-window causal
//! attention, selective biases, SwiGLU FFN) → RMS norm → 4× downsample → adapter
//! MLP → `[seq/4, 3072]` ready for the decoder.
//!
//! The attention/FFN linears are INT4-quantized ([`Weights::qlinear`]); the conv
//! stem and adapter projections are full-precision F16 ([`Weights::linear`]).
//!
//! **Scope (M1.2):** the batch [`AudioEncoder::encode_full`] path — a single SDPA
//! over an explicit causal mask, valid when the conv output fits within the
//! sliding window (`encode.py`'s `seq_len <= sliding_window` branch; true for the
//! short clips M1.5 validates first). Long-audio chunking with a rotating KV
//! cache (`encode_chunks`) lands with streaming (M3).

use anyhow::{anyhow, bail, Result};
use mlx_rs::ops::concatenate;
use mlx_rs::ops::indexing::IndexOp;
use mlx_rs::Array;

use super::config::EncoderConfig;
use super::nn::{chunk_window_mask, sliding_window_mask, Weights, COMPUTE_DTYPE};

/// The audio encoder, borrowing the loaded weights for the duration of a forward
/// pass. Construct per-encode; holds no mutable state on the batch path.
pub struct AudioEncoder<'a> {
    w: Weights<'a>,
    cfg: EncoderConfig,
}

impl<'a> AudioEncoder<'a> {
    /// Wraps `weights` (the full model map) with the encoder config.
    pub fn new(weights: Weights<'a>, cfg: EncoderConfig) -> Self {
        Self { w: weights, cfg }
    }

    /// Causal 1D convolution with left-only padding `kernel - stride`, matching
    /// `CausalConv1d`. `x` is `[1, frames, in_ch]` (MLX NLC); weight is stored
    /// `[out_ch, kernel, in_ch]`; the F32 bias is added post-convolution.
    fn causal_conv1d(&self, x: &Array, prefix: &str, kernel: i32, stride: i32) -> Result<Array> {
        let pad = kernel - stride;
        let x = if pad > 0 {
            let shape = x.shape();
            let zeros =
                mlx_rs::ops::zeros::<f32>(&[shape[0], pad, shape[2]])?.as_dtype(COMPUTE_DTYPE)?;
            concatenate(&[zeros, x.clone()], 1).map_err(|e| anyhow!("pad {prefix}: {e}"))?
        } else {
            x.clone()
        };
        let weight = self
            .w
            .get(&format!("{prefix}.weight"))?
            .as_dtype(COMPUTE_DTYPE)?;
        let y = mlx_rs::ops::conv1d(&x, &weight, stride, 0, 1, 1)
            .map_err(|e| anyhow!("conv1d {prefix}: {e}"))?;
        let bias = self
            .w
            .get(&format!("{prefix}.bias"))?
            .as_dtype(COMPUTE_DTYPE)?;
        y.add(&bias).map_err(|e| anyhow!("conv bias {prefix}: {e}"))
    }

    /// Conv stem: `[128, frames]` mel → `[seq, 1280]`, truncated to a multiple of
    /// the downsample factor (mirrors `conv_stem`). The conv output frame count
    /// is `4 ×` the audio-token count consumed by the decoder.
    pub fn conv_stem(&self, mel: &Array) -> Result<Array> {
        // mel.T[None] : [128, frames] -> [1, frames, 128]
        let x = mel
            .transpose(&[1, 0])?
            .expand_dims(&[0])?
            .as_dtype(COMPUTE_DTYPE)?;
        let x =
            mlx_rs::nn::gelu(self.causal_conv1d(&x, "encoder.conv_layers_0_conv.conv", 3, 1)?)?;
        let x =
            mlx_rs::nn::gelu(self.causal_conv1d(&x, "encoder.conv_layers_1_conv.conv", 3, 2)?)?;
        let x = x.squeeze(&[0i32][..])?; // [seq, 1280]

        let seq = x.shape()[0];
        let trunc = seq % self.cfg.downsample_factor as i32;
        if trunc > 0 {
            Ok(x.index(trunc..seq))
        } else {
            Ok(x)
        }
    }

    /// One encoder layer's attention with interleaved RoPE and an explicit mask,
    /// threading an optional left-context KV block for chunked encoding.
    ///
    /// `x` is the current chunk `[seq, dim]`; q/k/v are RoPE'd at absolute
    /// positions starting at `rope_offset`. When `prev_kv` is `Some`, those
    /// (already-RoPE'd) keys/values are prepended before SDPA so a query attends
    /// across the chunk boundary. Returns the attention output **and the current
    /// chunk's RoPE'd `(k, v)`** for the next chunk to use as left context.
    /// Biases are selective (wq/wv/wo yes, wk no).
    fn attention_kv(
        &self,
        x: &Array,
        layer: usize,
        rope_offset: i32,
        prev_kv: Option<&(Array, Array)>,
        mask: &Array,
    ) -> Result<(Array, (Array, Array))> {
        let prefix = format!("encoder.transformer_layers.{layer}.attention");
        let seq = x.shape()[0];
        let nh = self.cfg.n_heads as i32;
        let hd = self.cfg.head_dim as i32;

        let q = self.w.qlinear(x, &format!("{prefix}.wq"), true)?;
        let k = self.w.qlinear(x, &format!("{prefix}.wk"), false)?;
        let v = self.w.qlinear(x, &format!("{prefix}.wv"), true)?;

        // [seq, nh*hd] -> [1, nh, seq, hd]
        let to_heads = |t: &Array| -> Result<Array> {
            Ok(t.reshape(&[1, seq, nh, hd])?.transpose(&[0, 2, 1, 3])?)
        };
        let q = to_heads(&q)?;
        let k = to_heads(&k)?;
        let v = to_heads(&v)?;

        // Fused interleaved RoPE (traditional=true) at absolute positions.
        let theta = self.cfg.rope_theta;
        let q = mlx_rs::fast::rope(&q, hd, true, theta, 1.0, rope_offset, None)?;
        let k = mlx_rs::fast::rope(&k, hd, true, theta, 1.0, rope_offset, None)?;

        // Prepend the previous chunk's KV (already RoPE'd at its own offsets).
        let (ck, cv) = match prev_kv {
            None => (k.clone(), v.clone()),
            Some((pk, pv)) => (
                concatenate(&[pk, &k], 2).map_err(|e| anyhow!("kv k concat layer {layer}: {e}"))?,
                concatenate(&[pv, &v], 2).map_err(|e| anyhow!("kv v concat layer {layer}: {e}"))?,
            ),
        };

        let scale = 1.0 / (self.cfg.head_dim as f32).sqrt();
        let out = mlx_rs::fast::scaled_dot_product_attention(&q, &ck, &cv, scale, mask, None)
            .map_err(|e| anyhow!("sdpa layer {layer}: {e}"))?;

        // [1, nh, seq, hd] -> [seq, nh*hd]
        let out = out.transpose(&[0, 2, 1, 3])?.reshape(&[seq, nh * hd])?;
        let out = self.w.qlinear(&out, &format!("{prefix}.wo"), true)?;
        Ok((out, (k, v)))
    }

    /// The SwiGLU FFN sub-block (pre-norm + gated MLP + residual).
    fn ffn_block(&self, x: &Array, layer: usize) -> Result<Array> {
        let lp = format!("encoder.transformer_layers.{layer}");
        let h = self
            .w
            .rms_norm(x, &format!("{lp}.ffn_norm"), self.cfg.norm_eps)?;
        let gate = mlx_rs::nn::silu(self.w.qlinear(
            &h,
            &format!("{lp}.feed_forward_w1"),
            false,
        )?)?;
        let up = self
            .w
            .qlinear(&h, &format!("{lp}.feed_forward_w3"), false)?;
        let ff = self
            .w
            .qlinear(&gate.multiply(&up)?, &format!("{lp}.feed_forward_w2"), true)?;
        x.add(&ff)
            .map_err(|e| anyhow!("ffn residual layer {layer}: {e}"))
    }

    /// One full encoder transformer layer (pre-norm attention + SwiGLU FFN),
    /// threading the chunk KV cache. Returns `(output, current-chunk (k, v))`.
    fn layer_kv(
        &self,
        x: &Array,
        layer: usize,
        rope_offset: i32,
        prev_kv: Option<&(Array, Array)>,
        mask: &Array,
    ) -> Result<(Array, (Array, Array))> {
        let lp = format!("encoder.transformer_layers.{layer}");
        let h = self
            .w
            .rms_norm(x, &format!("{lp}.attention_norm"), self.cfg.norm_eps)?;
        let (h, kv) = self.attention_kv(&h, layer, rope_offset, prev_kv, mask)?;
        let x = x.add(&h)?;
        Ok((self.ffn_block(&x, layer)?, kv))
    }

    /// One full encoder transformer layer for the single-pass path (no cache).
    fn layer(&self, x: &Array, layer: usize, mask: &Array) -> Result<Array> {
        Ok(self.layer_kv(x, layer, 0, None, mask)?.0)
    }

    /// 4× downsample the encoder output and project to decoder dim via the
    /// adapter MLP (mirrors `downsample_and_project`).
    fn downsample_and_project(&self, encoded: &Array) -> Result<Array> {
        let ds = self.cfg.downsample_factor as i32;
        let dim = self.cfg.dim as i32;
        let seq = encoded.shape()[0];
        let ds_len = seq / ds;
        if ds_len == 0 {
            let decoder_dim = self
                .w
                .get("encoder.audio_language_projection_2.weight")?
                .shape()[0];
            return Ok(mlx_rs::ops::zeros::<f32>(&[0, decoder_dim])?.as_dtype(COMPUTE_DTYPE)?);
        }
        let x = encoded.index(0..ds_len * ds).reshape(&[ds_len, dim * ds])?;
        let x = mlx_rs::nn::gelu(self.w.linear(
            &x,
            "encoder.audio_language_projection_0",
            false,
        )?)?;
        self.w
            .linear(&x, "encoder.audio_language_projection_2", false)
    }

    /// Largest conv-output length [`encode_full`] will run in a single SDPA. The
    /// `[seq, seq]` attention scores grow quadratically (≈ `seq² · n_heads · 2 B`
    /// in F16), so a cap keeps short clips on the memory-safe single-pass path;
    /// longer audio routes to [`Self::encode_chunked`] (via [`Self::encode_conv`]).
    /// At 2048 (≈ 41 s) the scores are ≈ 256 MB — comfortable on unified memory.
    pub const MAX_FULL_FRAMES: usize = 2048;

    /// Encodes a full conv output `[seq, 1280]` into the adapter output
    /// `[seq/4, 3072]` in a single pass: 32 transformer layers under one shared
    /// sliding-window causal mask, then RMS norm, downsample, and the adapter
    /// projection. Equivalent to [`Self::encode_chunked`] over the whole sequence,
    /// capped at [`Self::MAX_FULL_FRAMES`] (callers should use
    /// [`Self::encode_conv`], which dispatches to the chunked path above the cap).
    pub fn encode_full(&self, conv_out: &Array) -> Result<Array> {
        let seq = conv_out.shape()[0] as usize;
        if seq > Self::MAX_FULL_FRAMES {
            bail!(
                "encoder.encode_full: {seq} conv frames exceeds the single-pass cap {} — \
                 use encode_conv/encode_chunked for long audio",
                Self::MAX_FULL_FRAMES
            );
        }
        let mask = sliding_window_mask(seq, self.cfg.sliding_window)?;
        let mut x = conv_out.clone();
        for layer in 0..self.cfg.n_layers {
            x = self.layer(&x, layer, &mask)?;
        }
        let x = self
            .w
            .rms_norm(&x, "encoder.transformer_norm", self.cfg.norm_eps)?;
        self.downsample_and_project(&x)
    }

    /// Encodes a conv output of **any length** by processing it in
    /// sliding-window-sized chunks, each carrying the previous chunk's KV as
    /// left context. Memory is bounded by one chunk's attention scores
    /// regardless of total length; the result is numerically identical to
    /// [`Self::encode_full`] (each query attends the same key window at the same
    /// absolute RoPE positions). Used for audio beyond [`Self::MAX_FULL_FRAMES`].
    pub fn encode_chunked(&self, conv_out: &Array) -> Result<Array> {
        let sw = self.cfg.sliding_window;
        let n = conv_out.shape()[0] as usize;
        let n_layers = self.cfg.n_layers;

        let mut prev_kv: Vec<Option<(Array, Array)>> = (0..n_layers).map(|_| None).collect();
        let mut prev_len = 0usize;
        let mut chunk_outputs: Vec<Array> = Vec::new();

        let mut start = 0usize;
        while start < n {
            let end = (start + sw).min(n);
            let chunk_len = end - start;
            let mask = chunk_window_mask(chunk_len, prev_len, sw)?;
            let mut x = conv_out.index((start as i32)..(end as i32));
            for (layer, kv_slot) in prev_kv.iter_mut().enumerate() {
                let (out, kv) = self.layer_kv(&x, layer, start as i32, kv_slot.as_ref(), &mask)?;
                x = out;
                *kv_slot = Some(kv);
            }
            chunk_outputs.push(self.w.rms_norm(
                &x,
                "encoder.transformer_norm",
                self.cfg.norm_eps,
            )?);
            prev_len = chunk_len;
            start = end;
        }

        let refs: Vec<&Array> = chunk_outputs.iter().collect();
        let encoded = concatenate(&refs, 0).map_err(|e| anyhow!("concat chunk outputs: {e}"))?;
        self.downsample_and_project(&encoded)
    }

    /// Encodes a conv output of any length, dispatching to the single-pass
    /// [`Self::encode_full`] when it fits the memory cap, else the memory-bounded
    /// [`Self::encode_chunked`]. Both yield the same adapter output.
    pub fn encode_conv(&self, conv_out: &Array) -> Result<Array> {
        if conv_out.shape()[0] as usize <= Self::MAX_FULL_FRAMES {
            self.encode_full(conv_out)
        } else {
            self.encode_chunked(conv_out)
        }
    }

    /// Convenience: conv stem + [`Self::encode_conv`] from a `[128, frames]` mel.
    pub fn encode(&self, mel: &Array) -> Result<Array> {
        let conv_out = self.conv_stem(mel)?;
        self.encode_conv(&conv_out)
    }

    /// Downsamples + projects complete `downsample_factor`-frame groups of
    /// `encoded`, exposed for the streaming session (which carries the `<4`-frame
    /// remainder across steps). See [`Self::downsample_and_project`].
    pub(crate) fn project_downsampled(&self, encoded: &Array) -> Result<Array> {
        self.downsample_and_project(encoded)
    }

    // ── Streaming primitives (M3b) ───────────────────────────────────────────

    /// One streaming causal Conv1d step (port of `StreamingCausalConv1d.step`):
    /// feeds `x_new` `[n_new, C_in]`, returns `[n_out, C_out]`, carrying the
    /// `kernel - stride` left-context frames in `st` so concatenated step outputs
    /// equal the batch [`Self::causal_conv1d`] over the concatenated input.
    fn stream_conv1d(
        &self,
        st: &mut StreamConvState,
        x_new: &Array,
        prefix: &str,
        kernel: i32,
        stride: i32,
    ) -> Result<Array> {
        let weight = self
            .w
            .get(&format!("{prefix}.weight"))?
            .as_dtype(COMPUTE_DTYPE)?;
        let c_out = weight.shape()[0];
        if x_new.shape()[0] == 0 {
            return Ok(mlx_rs::ops::zeros::<f32>(&[0, c_out])?.as_dtype(COMPUTE_DTYPE)?);
        }
        let keep = kernel - stride;
        let context = if !st.initialized {
            st.initialized = true;
            if keep > 0 {
                let pad = mlx_rs::ops::zeros::<f32>(&[keep, x_new.shape()[1]])?
                    .as_dtype(COMPUTE_DTYPE)?;
                concatenate(&[&pad, x_new], 0).map_err(|e| anyhow!("conv pad {prefix}: {e}"))?
            } else {
                x_new.clone()
            }
        } else {
            match &st.state {
                Some(s) => concatenate(&[s, x_new], 0)
                    .map_err(|e| anyhow!("conv state concat {prefix}: {e}"))?,
                None => x_new.clone(),
            }
        };

        let len = context.shape()[0];
        if len < kernel {
            st.state = Some(context);
            return Ok(mlx_rs::ops::zeros::<f32>(&[0, c_out])?.as_dtype(COMPUTE_DTYPE)?);
        }

        let out = mlx_rs::ops::conv1d(&context.expand_dims(&[0])?, &weight, stride, 0, 1, 1)
            .map_err(|e| anyhow!("stream conv1d {prefix}: {e}"))?;
        let bias = self
            .w
            .get(&format!("{prefix}.bias"))?
            .as_dtype(COMPUTE_DTYPE)?;
        let out = out.add(&bias)?.squeeze(&[0i32][..])?; // [n_out, C_out]
        let n_out = out.shape()[0];

        if keep > 0 {
            let leftover = len - n_out * stride;
            st.state = if leftover <= 0 {
                None
            } else {
                let take = keep.min(leftover);
                Some(context.index((len - take)..len))
            };
        } else {
            st.state = None;
        }
        Ok(out)
    }

    /// One streaming conv-stem step: mel frames `[n, 128]` (frame-major) →
    /// conv output `[m, dim]` (port of `StreamingConvStem.step`; no front-trunc —
    /// the standard padding keeps the running total ÷ `downsample_factor`).
    pub(crate) fn stream_conv_stem(
        &self,
        st: &mut ConvStemState,
        mel_frames: &Array,
    ) -> Result<Array> {
        let x = mel_frames.as_dtype(COMPUTE_DTYPE)?;
        let x = mlx_rs::nn::gelu(self.stream_conv1d(
            &mut st.c0,
            &x,
            "encoder.conv_layers_0_conv.conv",
            3,
            1,
        )?)?;
        let x = mlx_rs::nn::gelu(self.stream_conv1d(
            &mut st.c1,
            &x,
            "encoder.conv_layers_1_conv.conv",
            3,
            2,
        )?)?;
        Ok(x)
    }

    /// One streaming encoder step over a conv chunk `[n, dim]` → post-norm output
    /// `[n, dim]` (port of `StreamingEncoder.step`): runs all 32 layers with a
    /// per-layer rotating KV cache (≤ sliding_window frames) at the running RoPE
    /// position. Equivalent to [`Self::encode_chunked`] for any chunking.
    pub(crate) fn stream_encode(
        &self,
        st: &mut StreamEncState,
        conv_chunk: &Array,
    ) -> Result<Array> {
        let chunk_len = conv_chunk.shape()[0] as usize;
        if chunk_len == 0 {
            return Ok(conv_chunk.clone());
        }
        let sw = self.cfg.sliding_window;
        let prev_len = st.prev_kv[0]
            .as_ref()
            .map_or(0, |(k, _)| k.shape()[2] as usize);
        let mask = chunk_window_mask(chunk_len, prev_len, sw)?;

        let mut x = conv_chunk.clone();
        for (layer, slot) in st.prev_kv.iter_mut().enumerate() {
            let (out, (k, v)) = self.layer_kv(&x, layer, st.pos, slot.as_ref(), &mask)?;
            x = out;
            // Append the new chunk's KV and keep only the last `sw` frames.
            let (mut nk, mut nv) = match slot.take() {
                None => (k, v),
                Some((pk, pv)) => (
                    concatenate(&[&pk, &k], 2).map_err(|e| anyhow!("enc kv k: {e}"))?,
                    concatenate(&[&pv, &v], 2).map_err(|e| anyhow!("enc kv v: {e}"))?,
                ),
            };
            let t = nk.shape()[2];
            if t as usize > sw {
                let idx: Vec<i32> = ((t - sw as i32)..t).collect();
                let idx = Array::from_slice(&idx, &[sw as i32]);
                nk = nk.take(&idx, 2)?;
                nv = nv.take(&idx, 2)?;
            }
            *slot = Some((nk, nv));
        }
        let normed = self
            .w
            .rms_norm(&x, "encoder.transformer_norm", self.cfg.norm_eps)?;
        st.pos += chunk_len as i32;
        Ok(normed)
    }
}

/// Per-conv carried left-context state for [`AudioEncoder::stream_conv1d`].
#[derive(Default)]
pub(crate) struct StreamConvState {
    state: Option<Array>,
    initialized: bool,
}

/// The two streaming conv states of the conv stem.
#[derive(Default)]
pub(crate) struct ConvStemState {
    c0: StreamConvState,
    c1: StreamConvState,
}

/// Streaming encoder state: a per-layer rotating KV cache and the running RoPE
/// position.
pub(crate) struct StreamEncState {
    prev_kv: Vec<Option<(Array, Array)>>,
    pos: i32,
}

impl StreamEncState {
    /// A fresh state for an `n_layers`-deep encoder.
    pub(crate) fn new(n_layers: usize) -> Self {
        Self {
            prev_kv: (0..n_layers).map(|_| None).collect(),
            pos: 0,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::config::VoxtralMlxConfig;
    use super::super::{load_safetensors, Weights};
    use super::AudioEncoder;
    use mlx_rs::{Array, Dtype};

    fn model_dir() -> Option<std::path::PathBuf> {
        std::env::var("OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
    }

    /// Runs the full encoder forward on a synthetic in-window mel and asserts the
    /// adapter output has the expected `[seq/4, decoder_dim]` shape and is finite
    /// — proving the conv stem + 32 INT4 transformer layers + adapter execute on
    /// Metal without NaN/Inf (the core M1.2 deliverable).
    #[test]
    #[ignore = "requires the INT4 Voxtral model; set OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL=<dir> (#933 M1.2)"]
    fn encoder_forward_runs_on_metal() {
        let Some(dir) = model_dir() else {
            panic!("set OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL=<dir with model.safetensors>");
        };
        let map = load_safetensors(&dir.join("model.safetensors")).expect("load weights");
        let cfg = VoxtralMlxConfig::voxtral_realtime_mini_4b();
        let enc = AudioEncoder::new(
            Weights::new(&map, cfg.quant.group_size, cfg.quant.bits),
            cfg.encoder,
        );

        // Synthetic log-mel [128, 300] in the model's ~[-1.5, 1.5] range. 300
        // frames → conv stride-2 → 150 → trunc-to-÷4 → 148 (≤ 750 window).
        let (bins, frames) = (128usize, 300usize);
        let mut data = vec![0.0_f32; bins * frames];
        for (i, v) in data.iter_mut().enumerate() {
            *v = ((i % 17) as f32 / 17.0).mul_add(3.0, -1.5);
        }
        let mel = Array::from_slice(&data, &[bins as i32, frames as i32]);

        let out = enc.encode(&mel).expect("encoder forward");
        assert_eq!(out.shape(), &[37, 3072], "adapter output shape");

        let out = out.as_dtype(Dtype::Float32).unwrap();
        out.eval().unwrap();
        assert!(
            out.as_slice::<f32>().iter().all(|x| x.is_finite()),
            "encoder output must be finite (no NaN/Inf)"
        );
    }

    /// Chunked encoding must be numerically equivalent to the single-pass path
    /// (same key window + absolute RoPE per query). Builds a multi-chunk conv
    /// output (> sliding_window) and asserts the two adapter outputs match to
    /// F16 tolerance — the M3 long-audio correctness guarantee.
    #[test]
    #[ignore = "requires the INT4 Voxtral model; set OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL=<dir> (#933 M3)"]
    fn chunked_encoding_matches_single_pass() {
        let Some(dir) = model_dir() else {
            panic!("set OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL=<dir>");
        };
        let map = load_safetensors(&dir.join("model.safetensors")).expect("load weights");
        let cfg = VoxtralMlxConfig::voxtral_realtime_mini_4b();
        let enc = AudioEncoder::new(
            Weights::new(&map, cfg.quant.group_size, cfg.quant.bits),
            cfg.encoder,
        );

        // 1600 mel frames → conv ≈ 800 frames > 750 window → 2 chunks chunked,
        // still ≤ 2048 so single-pass runs too. Compare the two.
        let (bins, frames) = (128usize, 1600usize);
        let mut data = vec![0.0_f32; bins * frames];
        for (i, v) in data.iter_mut().enumerate() {
            *v = ((i % 23) as f32 / 23.0).mul_add(3.0, -1.5);
        }
        let mel = Array::from_slice(&data, &[bins as i32, frames as i32]);
        let conv_out = enc.conv_stem(&mel).expect("conv stem");
        assert!(
            conv_out.shape()[0] > cfg.encoder.sliding_window as i32,
            "test needs a multi-chunk conv output"
        );

        let full = enc.encode_full(&conv_out).expect("encode_full");
        let chunked = enc.encode_chunked(&conv_out).expect("encode_chunked");
        assert_eq!(full.shape(), chunked.shape());

        let diff = full
            .subtract(&chunked)
            .unwrap()
            .abs()
            .unwrap()
            .max(None, None)
            .unwrap();
        diff.eval().unwrap();
        let max_abs = diff.item::<f32>();
        assert!(
            max_abs < 0.05,
            "chunked vs single-pass max abs diff {max_abs}"
        );
    }
}
