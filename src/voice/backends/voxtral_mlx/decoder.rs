//! The LLM decoder — a direct port of `mlx-audio`'s `voxtral_realtime/decoder.py`.
//!
//! A 26-layer decoder-only transformer: GQA (32 query / 8 KV heads, head_dim
//! 128), interleaved RoPE (θ=1M), SwiGLU FFN, **no biases**, **adaptive RMS norm**
//! time-conditioning on the FFN branch, and **tied embeddings** (the F16
//! `tok_embeddings` serves as both input lookup and LM head). Attention/FFN
//! linears are INT4-quantized; the per-layer ada-norm MLPs are tiny F32.
//!
//! **Scope (M1.3):** the decoder forward + a simple **growing** KV cache (append
//! along the time axis), sufficient when the total decoded length stays within
//! the 8192 sliding window — true for the short clips M1.5 validates and for
//! prefill-then-greedy decode. The `RotatingKVCache` ring buffer (O(1) steady
//! state, long-audio) lands with streaming (M3). The audio↔text interleaving
//! generation loop is M1.4 wiring.

use anyhow::{anyhow, Result};
use mlx_rs::ops::concatenate;
use mlx_rs::Array;

use super::config::DecoderConfig;
use super::nn::{causal_mask, Weights, COMPUTE_DTYPE};

/// Sinusoidal time-embedding for adaptive-RMSNorm conditioning (port of
/// `compute_time_embedding`). Computed on the host in F32: `[cos(t·invfreq) ‖
/// sin(t·invfreq)]`, length `dim`. `theta` defaults to 10000 (note: distinct
/// from the RoPE θ=1M).
fn time_embedding(t_value: f32, dim: usize, theta: f32) -> Array {
    let half = dim / 2;
    let mut data = vec![0.0_f32; dim];
    let ln_theta = theta.ln();
    for i in 0..half {
        let inv_freq = (-ln_theta * i as f32 / half as f32).exp();
        let emb = t_value * inv_freq;
        data[i] = emb.cos();
        data[half + i] = emb.sin();
    }
    Array::from_slice(&data, &[dim as i32])
}

/// A per-layer growing KV cache.
///
/// `k`/`v` are `[1, n_kv_heads, T, head_dim]`, extended along the time axis (2)
/// on each step. Bounded in practice by the decoded length; the rotating ring
/// buffer (M3) replaces this for long audio.
#[derive(Default)]
pub struct KvCache {
    kv: Option<(Array, Array)>,
}

impl KvCache {
    /// A fresh, empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends `k`/`v` (`[1, n_kv, seq, hd]`) and returns the full cached tensors.
    fn update(&mut self, k: &Array, v: &Array) -> Result<(Array, Array)> {
        let (nk, nv) = match &self.kv {
            None => (k.clone(), v.clone()),
            Some((pk, pv)) => (
                concatenate(&[pk, k], 2).map_err(|e| anyhow!("kv k concat: {e}"))?,
                concatenate(&[pv, v], 2).map_err(|e| anyhow!("kv v concat: {e}"))?,
            ),
        };
        self.kv = Some((nk.clone(), nv.clone()));
        Ok((nk, nv))
    }
}

/// The decoder, borrowing the loaded weights for the duration of a session.
pub struct Decoder<'a> {
    w: Weights<'a>,
    cfg: DecoderConfig,
}

impl<'a> Decoder<'a> {
    /// Wraps `weights` (the full model map) with the decoder config.
    pub fn new(weights: Weights<'a>, cfg: DecoderConfig) -> Self {
        Self { w: weights, cfg }
    }

    /// Allocates a fresh per-layer KV cache (one [`KvCache`] per decoder layer).
    pub fn make_cache(&self) -> Vec<KvCache> {
        (0..self.cfg.n_layers).map(|_| KvCache::new()).collect()
    }

    /// Precomputes the per-layer adaptive-norm gains `1 + ada_up(gelu(ada_down(t)))`
    /// once for a given delay (`t_value` = number of delay tokens). Each gain is
    /// `[dim]` F16, applied multiplicatively to the FFN-norm output. The ada MLPs
    /// are F32, so the bottleneck math runs in F32 before the F16 cast.
    pub fn precompute_ada_gains(&self, t_value: f32) -> Result<Vec<Array>> {
        let t_cond =
            time_embedding(t_value, self.cfg.dim, 10_000.0).reshape(&[1, self.cfg.dim as i32])?;
        let one = Array::from_float(1.0);
        let mut gains = Vec::with_capacity(self.cfg.n_layers);
        for layer in 0..self.cfg.n_layers {
            let lp = format!("decoder.layers.{layer}.ada_rms_norm_t_cond");
            let down = self.w.get(&format!("{lp}.ada_down.weight"))?; // F32 [bottleneck, dim]
            let up = self.w.get(&format!("{lp}.ada_up.weight"))?; // F32 [dim, bottleneck]
            let hidden = mlx_rs::nn::gelu(t_cond.matmul(&down.transpose(&[1, 0])?)?)?;
            let scale = hidden.matmul(&up.transpose(&[1, 0])?)?; // [1, dim] F32
            let gain = scale.add(&one)?.as_dtype(COMPUTE_DTYPE)?; // [1, dim] F16
            gains.push(gain);
        }
        Ok(gains)
    }

    /// Looks up F16 token embeddings for `ids`, returning `[ids.len(), dim]`.
    pub fn embed_tokens(&self, ids: &[i32]) -> Result<Array> {
        let emb = self.w.get("decoder.tok_embeddings.weight")?;
        let idx = Array::from_slice(ids, &[ids.len() as i32]);
        emb.take(&idx, 0).map_err(|e| anyhow!("embed take: {e}"))
    }

    /// One layer's GQA attention (no biases). `start_pos` is the RoPE offset / the
    /// absolute position of the first token in `x`; `cache` is appended in place.
    fn attention(
        &self,
        x: &Array,
        layer: usize,
        start_pos: i32,
        cache: &mut KvCache,
    ) -> Result<Array> {
        let prefix = format!("decoder.layers.{layer}.attention");
        let seq = x.shape()[0];
        let nh = self.cfg.n_heads as i32;
        let nkv = self.cfg.n_kv_heads as i32;
        let hd = self.cfg.head_dim as i32;

        let q = self.w.qlinear(x, &format!("{prefix}.wq"), false)?;
        let k = self.w.qlinear(x, &format!("{prefix}.wk"), false)?;
        let v = self.w.qlinear(x, &format!("{prefix}.wv"), false)?;

        // [seq, h*hd] -> [1, h, seq, hd]
        let q = q.reshape(&[1, seq, nh, hd])?.transpose(&[0, 2, 1, 3])?;
        let k = k.reshape(&[1, seq, nkv, hd])?.transpose(&[0, 2, 1, 3])?;
        let v = v.reshape(&[1, seq, nkv, hd])?.transpose(&[0, 2, 1, 3])?;

        let theta = self.cfg.rope_theta;
        let q = mlx_rs::fast::rope(&q, hd, true, theta, 1.0, start_pos, None)?;
        let k = mlx_rs::fast::rope(&k, hd, true, theta, 1.0, start_pos, None)?;

        let (k, v) = cache.update(&k, &v)?;

        // Prefill (seq > 1) gets a causal mask; a single decode step attends to
        // the whole cache (mask = None). GQA broadcast is native to SDPA.
        let mask = if seq > 1 {
            Some(causal_mask(seq as usize)?)
        } else {
            None
        };
        let scale = 1.0 / (self.cfg.head_dim as f32).sqrt();
        let out =
            mlx_rs::fast::scaled_dot_product_attention(&q, &k, &v, scale, mask.as_ref(), None)
                .map_err(|e| anyhow!("decoder sdpa layer {layer}: {e}"))?;

        let out = out.transpose(&[0, 2, 1, 3])?.reshape(&[seq, nh * hd])?;
        self.w.qlinear(&out, &format!("{prefix}.wo"), false)
    }

    /// One decoder layer: pre-norm GQA attention + ada-conditioned SwiGLU FFN.
    fn layer(
        &self,
        x: &Array,
        layer: usize,
        start_pos: i32,
        ada_gain: &Array,
        cache: &mut KvCache,
    ) -> Result<Array> {
        let lp = format!("decoder.layers.{layer}");
        let h = self
            .w
            .rms_norm(x, &format!("{lp}.attention_norm"), self.cfg.norm_eps)?;
        let h = self.attention(&h, layer, start_pos, cache)?;
        let x = x.add(&h)?;

        // FFN with adaptive norm: ffn_norm then multiply by (1 + ada_scale).
        let h = self
            .w
            .rms_norm(&x, &format!("{lp}.ffn_norm"), self.cfg.norm_eps)?;
        let h = h.multiply(ada_gain)?;
        let gate = mlx_rs::nn::silu(self.w.qlinear(
            &h,
            &format!("{lp}.feed_forward_w1"),
            false,
        )?)?;
        let up = self
            .w
            .qlinear(&h, &format!("{lp}.feed_forward_w3"), false)?;
        let ff = self.w.qlinear(
            &gate.multiply(&up)?,
            &format!("{lp}.feed_forward_w2"),
            false,
        )?;
        x.add(&ff)
            .map_err(|e| anyhow!("decoder ffn residual layer {layer}: {e}"))
    }

    /// Runs the decoder over input `embeds` (`[seq, dim]`), updating `caches` in
    /// place, and returns the final-norm hidden states `[seq, dim]`.
    pub fn forward(
        &self,
        embeds: &Array,
        start_pos: i32,
        ada_gains: &[Array],
        caches: &mut [KvCache],
    ) -> Result<Array> {
        let mut h = embeds.as_dtype(COMPUTE_DTYPE)?;
        for layer in 0..self.cfg.n_layers {
            h = self.layer(&h, layer, start_pos, &ada_gains[layer], &mut caches[layer])?;
        }
        self.w.rms_norm(&h, "decoder.norm", self.cfg.norm_eps)
    }

    /// LM head via tied embeddings: `logits = h @ tok_embeddingsᵀ` → `[seq, vocab]`.
    pub fn logits(&self, h: &Array) -> Result<Array> {
        let emb = self
            .w
            .get("decoder.tok_embeddings.weight")?
            .as_dtype(COMPUTE_DTYPE)?;
        h.matmul(&emb.transpose(&[1, 0])?)
            .map_err(|e| anyhow!("logits matmul: {e}"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::config::VoxtralMlxConfig;
    use super::super::{load_safetensors, Weights};
    use super::Decoder;
    use mlx_rs::Dtype;

    fn model_dir() -> Option<std::path::PathBuf> {
        std::env::var("OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
    }

    fn assert_finite(a: &mlx_rs::Array, what: &str) {
        let a = a.as_dtype(Dtype::Float32).unwrap();
        a.eval().unwrap();
        assert!(
            a.as_slice::<f32>().iter().all(|x| x.is_finite()),
            "{what} must be finite (no NaN/Inf)"
        );
    }

    /// Prefills a short token sequence, decodes one step from the KV cache, and
    /// checks the hidden states + logits have the right shape and are finite —
    /// exercising the 26 INT4 layers, GQA, ada-norm, RoPE, and the growing KV
    /// cache on Metal (the M1.3 deliverable).
    #[test]
    #[ignore = "requires the INT4 Voxtral model; set OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL=<dir> (#933 M1.3)"]
    fn decoder_forward_and_decode_step_run_on_metal() {
        let Some(dir) = model_dir() else {
            panic!("set OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL=<dir with model.safetensors>");
        };
        let map = load_safetensors(&dir.join("model.safetensors")).expect("load weights");
        let cfg = VoxtralMlxConfig::voxtral_realtime_mini_4b();
        let dec = Decoder::new(
            Weights::new(&map, cfg.quant.group_size, cfg.quant.bits),
            cfg.decoder,
        );

        // 480 ms delay at 12.5 Hz = 6 delay tokens.
        let ada = dec.precompute_ada_gains(6.0).expect("ada gains");
        assert_eq!(ada.len(), cfg.decoder.n_layers);
        let mut caches = dec.make_cache();

        // Prefill 4 tokens.
        let prompt = [1_i32, 17, 42, 9];
        let embeds = dec.embed_tokens(&prompt).expect("embed");
        assert_eq!(embeds.shape(), &[4, cfg.decoder.dim as i32]);
        let h = dec.forward(&embeds, 0, &ada, &mut caches).expect("prefill");
        assert_eq!(h.shape(), &[4, cfg.decoder.dim as i32]);
        let logits = dec.logits(&h).expect("logits");
        assert_eq!(logits.shape(), &[4, cfg.decoder.vocab_size as i32]);
        assert_finite(&logits, "prefill logits");

        // Decode one step at position 4 (single token, mask=None, cache append).
        let step = dec.embed_tokens(&[123]).expect("embed step");
        let h2 = dec
            .forward(&step, 4, &ada, &mut caches)
            .expect("decode step");
        assert_eq!(h2.shape(), &[1, cfg.decoder.dim as i32]);
        let logits2 = dec.logits(&h2).expect("step logits");
        assert_eq!(logits2.shape(), &[1, cfg.decoder.vocab_size as i32]);
        assert_finite(&logits2, "decode-step logits");
    }
}
