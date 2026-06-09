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
use super::nn::{causal_mask, Weights, COMPUTE_DTYPE};

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
    /// the downsample factor (mirrors `conv_stem`).
    fn conv_stem(&self, mel: &Array) -> Result<Array> {
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

    /// One encoder layer's attention with interleaved RoPE and an explicit mask.
    /// `x` is `[seq, dim]`; biases are selective (wq/wv/wo yes, wk no).
    fn attention(&self, x: &Array, layer: usize, mask: &Array) -> Result<Array> {
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

        // Fused interleaved RoPE (traditional=true), offset 0 on the batch path.
        let theta = self.cfg.rope_theta;
        let q = mlx_rs::fast::rope(&q, hd, true, theta, 1.0, 0, None)?;
        let k = mlx_rs::fast::rope(&k, hd, true, theta, 1.0, 0, None)?;

        let scale = 1.0 / (self.cfg.head_dim as f32).sqrt();
        let out = mlx_rs::fast::scaled_dot_product_attention(&q, &k, &v, scale, mask, None)
            .map_err(|e| anyhow!("sdpa layer {layer}: {e}"))?;

        // [1, nh, seq, hd] -> [seq, nh*hd]
        let out = out.transpose(&[0, 2, 1, 3])?.reshape(&[seq, nh * hd])?;
        self.w.qlinear(&out, &format!("{prefix}.wo"), true)
    }

    /// One full encoder transformer layer (pre-norm attention + SwiGLU FFN).
    fn layer(&self, x: &Array, layer: usize, mask: &Array) -> Result<Array> {
        let lp = format!("encoder.transformer_layers.{layer}");
        let h = self
            .w
            .rms_norm(x, &format!("{lp}.attention_norm"), self.cfg.norm_eps)?;
        let h = self.attention(&h, layer, mask)?;
        let x = x.add(&h)?;

        let h = self
            .w
            .rms_norm(&x, &format!("{lp}.ffn_norm"), self.cfg.norm_eps)?;
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

    /// Non-chunked encode: conv stem + 32 layers (one shared causal mask) + RMS
    /// norm + downsample + project. Valid when the conv output fits within the
    /// sliding window; errors otherwise (long-audio chunking is M3).
    pub fn encode(&self, mel: &Array) -> Result<Array> {
        let conv_out = self.conv_stem(mel)?;
        let seq = conv_out.shape()[0] as usize;
        if seq > self.cfg.sliding_window {
            bail!(
                "encoder.encode: conv output {seq} frames exceeds sliding window {} — \
                 long-audio chunked encoding lands with streaming (#933 M3)",
                self.cfg.sliding_window
            );
        }
        let mask = causal_mask(seq)?;
        let mut x = conv_out;
        for layer in 0..self.cfg.n_layers {
            x = self.layer(&x, layer, &mask)?;
        }
        let x = self
            .w
            .rms_norm(&x, "encoder.transformer_norm", self.cfg.norm_eps)?;
        self.downsample_and_project(&x)
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
            *v = ((i % 17) as f32 / 17.0) * 3.0 - 1.5;
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
}
