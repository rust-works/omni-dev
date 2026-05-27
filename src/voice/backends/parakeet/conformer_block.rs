//! Conformer block: feed-forward, attention, conv module, feed-forward,
//! output norm. Mirrors `senstella/parakeet-mlx::parakeet_mlx/conformer.py::ConformerBlock`.
//!
//! Each of the 24 layers in the Parakeet FastConformer encoder is one
//! instance of this block, sharing the same hyperparameters
//! (`d_model=1024`, `n_heads=8`, `ff_expansion_factor=4`,
//! `conv_kernel_size=9`).
//!
//! Sub-block order, with `x` flowing top to bottom:
//!
//! ```text
//! x = x + 0.5 * FF1(LN1(x))           // half-step feed-forward residual
//! x = x + Attn(LN2(x), pos_emb, mask) // multi-head self-attention residual
//! x = x + Conv(LN3(x))                // conv module residual
//! x = x + 0.5 * FF2(LN4(x))           // half-step feed-forward residual
//! x = LN_out(x)                       // post-norm
//! ```

use anyhow::{Context, Result};
use candle_core::{Module, Tensor};
use candle_nn::{ops::silu, LayerNorm, Linear, VarBuilder};

use super::attention::{RelPositionMultiHeadAttention, RelPositionMultiHeadLocalAttention};
use super::cache::RotatingConformerCache;
use super::conv_module::ConvolutionModule;

/// Position-wise feed-forward sub-block. Two linear layers separated by
/// SiLU, with `d_ff = ff_expansion_factor * d_model` hidden size.
pub struct FeedForward {
    linear1: Linear,
    linear2: Linear,
}

impl FeedForward {
    /// Loads the two linear layers from `vb`. `d_ff` is the hidden size
    /// (`= ff_expansion_factor * d_model`).
    pub fn load(vb: VarBuilder, d_model: usize, d_ff: usize, use_bias: bool) -> Result<Self> {
        Ok(Self {
            linear1: linear(vb.pp("linear1"), d_model, d_ff, use_bias)?,
            linear2: linear(vb.pp("linear2"), d_ff, d_model, use_bias)?,
        })
    }

    /// `(B, T, d_model)` -> `(B, T, d_model)`.
    pub fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let x = self.linear1.forward(x).context("ff linear1")?;
        let x = silu(&x).context("ff silu")?;
        self.linear2.forward(&x).context("ff linear2")
    }
}

impl Clone for FeedForward {
    fn clone(&self) -> Self {
        // Linear's inner Tensor is Arc-backed; clone is cheap.
        Self {
            linear1: self.linear1.clone(),
            linear2: self.linear2.clone(),
        }
    }
}

/// Single Conformer block. Inputs and outputs are `(B, T, d_model)`.
pub struct ConformerBlock {
    norm_ff1: LayerNorm,
    ff1: FeedForward,
    norm_self_att: LayerNorm,
    self_attn: RelPositionMultiHeadAttention,
    norm_conv: LayerNorm,
    conv: ConvolutionModule,
    norm_ff2: LayerNorm,
    ff2: FeedForward,
    norm_out: LayerNorm,
}

impl ConformerBlock {
    /// Loads one Conformer block from `vb` (which should be rooted at
    /// `encoder.layers.<i>`).
    pub fn load(
        vb: VarBuilder,
        d_model: usize,
        n_heads: usize,
        ff_expansion_factor: usize,
        conv_kernel_size: usize,
        use_bias: bool,
    ) -> Result<Self> {
        let d_ff = d_model * ff_expansion_factor;
        let norm_eps = 1e-5;
        Ok(Self {
            norm_ff1: layer_norm(vb.pp("norm_feed_forward1"), d_model, norm_eps)?,
            ff1: FeedForward::load(vb.pp("feed_forward1"), d_model, d_ff, use_bias)?,
            norm_self_att: layer_norm(vb.pp("norm_self_att"), d_model, norm_eps)?,
            self_attn: RelPositionMultiHeadAttention::load(
                vb.pp("self_attn"),
                n_heads,
                d_model,
                use_bias,
            )?,
            norm_conv: layer_norm(vb.pp("norm_conv"), d_model, norm_eps)?,
            conv: ConvolutionModule::load(vb.pp("conv"), d_model, conv_kernel_size, use_bias)?,
            norm_ff2: layer_norm(vb.pp("norm_feed_forward2"), d_model, norm_eps)?,
            ff2: FeedForward::load(vb.pp("feed_forward2"), d_model, d_ff, use_bias)?,
            norm_out: layer_norm(vb.pp("norm_out"), d_model, norm_eps)?,
        })
    }

    /// Forward pass.
    ///
    /// Shapes:
    /// - `x`: `(B, T, d_model)`
    /// - `pos_emb`: `(1, 2T-1, d_model)`
    /// - `mask` (optional): `(B, T, T)` with `1.0` at blocked positions
    pub fn forward(&self, x: &Tensor, pos_emb: &Tensor, mask: Option<&Tensor>) -> Result<Tensor> {
        // x = x + 0.5 * FF1(LN1(x))
        let n1 = self.norm_ff1.forward(x).context("LN ff1")?;
        let f1 = self.ff1.forward(&n1).context("ff1")?;
        let f1_half = (f1 * 0.5_f64)?;
        let x = (x + f1_half)?;

        // x = x + Attn(LN2(x), pos_emb, mask)
        let n2 = self.norm_self_att.forward(&x).context("LN self_att")?;
        let attn_out = self
            .self_attn
            .forward(&n2, &n2, &n2, pos_emb, mask)
            .context("self_attn")?;
        let x = (x + attn_out)?;

        // x = x + Conv(LN3(x))
        let n3 = self.norm_conv.forward(&x).context("LN conv")?;
        let c = self.conv.forward(&n3).context("conv")?;
        let x = (x + c)?;

        // x = x + 0.5 * FF2(LN4(x))
        let n4 = self.norm_ff2.forward(&x).context("LN ff2")?;
        let f2 = self.ff2.forward(&n4).context("ff2")?;
        let f2_half = (f2 * 0.5_f64)?;
        let x = (x + f2_half)?;

        // x = LN_out(x)
        let out = self.norm_out.forward(&x).context("LN_out")?;
        Ok(out)
    }
}

/// Streaming variant of [`ConformerBlock`] using local-window attention
/// and an optional [`RotatingConformerCache`] threaded through both the
/// self-attention and conv-module sub-blocks.
///
/// Shares all weights with a full-attention `ConformerBlock` via
/// `Arc`-backed tensor clones — `from_full` does not re-traverse the
/// `VarBuilder`. The only field that differs is `self_attn`, which is
/// constructed from `RelPositionMultiHeadLocalAttention::from_full`
/// using the same Q/K/V/Pos/Out projections.
pub struct LocalConformerBlock {
    norm_ff1: LayerNorm,
    ff1: FeedForward,
    norm_self_att: LayerNorm,
    self_attn: RelPositionMultiHeadLocalAttention,
    norm_conv: LayerNorm,
    conv: ConvolutionModule,
    norm_ff2: LayerNorm,
    ff2: FeedForward,
    norm_out: LayerNorm,
}

impl LocalConformerBlock {
    /// Constructs a streaming variant from a loaded `ConformerBlock`,
    /// sharing all weight tensors via `Arc`-backed clones. The
    /// `context_size = (left, right)` selects the local-attention window.
    #[must_use]
    pub fn from_full(full: &ConformerBlock, context_size: (usize, usize)) -> Self {
        Self {
            norm_ff1: full.norm_ff1.clone(),
            ff1: full.ff1.clone(),
            norm_self_att: full.norm_self_att.clone(),
            self_attn: RelPositionMultiHeadLocalAttention::from_full(&full.self_attn, context_size),
            norm_conv: full.norm_conv.clone(),
            conv: full.conv.clone(),
            norm_ff2: full.norm_ff2.clone(),
            ff2: full.ff2.clone(),
            norm_out: full.norm_out.clone(),
        }
    }

    /// Streaming forward with optional per-layer cache.
    ///
    /// Shapes:
    /// - `x`: `(B, S, d_model)` — typically S is small (per-chunk new frames)
    /// - `pos_emb`: `(1, 2w+1, d_model)` from `LocalRelPositionalEncoding`
    /// - `mask` (optional): `(B, S)` — `true` at padded query positions
    /// - `cache` (optional): `&mut Option<RotatingConformerCache>` —
    ///   threaded sequentially through `self_attn` then `conv`, mutated in place
    pub fn forward_with_cache(
        &self,
        x: &Tensor,
        pos_emb: &Tensor,
        mask: Option<&Tensor>,
        cache: &mut Option<RotatingConformerCache>,
    ) -> Result<Tensor> {
        // x = x + 0.5 * FF1(LN1(x))
        let n1 = self.norm_ff1.forward(x).context("LN ff1 (local)")?;
        let f1 = self.ff1.forward(&n1).context("ff1 (local)")?;
        let f1_half = (f1 * 0.5_f64)?;
        let x = (x + f1_half)?;

        // x = x + Attn(LN2(x), pos_emb, mask, cache)
        let n2 = self
            .norm_self_att
            .forward(&x)
            .context("LN self_att (local)")?;
        let attn_out = self
            .self_attn
            .forward(&n2, &n2, &n2, pos_emb, mask, cache.as_mut())
            .context("self_attn (local)")?;
        let x = (x + attn_out)?;

        // x = x + Conv(LN3(x), cache)
        let n3 = self.norm_conv.forward(&x).context("LN conv (local)")?;
        let c = self
            .conv
            .forward_with_cache(&n3, cache.as_mut())
            .context("conv (local)")?;
        let x = (x + c)?;

        // x = x + 0.5 * FF2(LN4(x))
        let n4 = self.norm_ff2.forward(&x).context("LN ff2 (local)")?;
        let f2 = self.ff2.forward(&n4).context("ff2 (local)")?;
        let f2_half = (f2 * 0.5_f64)?;
        let x = (x + f2_half)?;

        // x = LN_out(x)
        self.norm_out.forward(&x).context("LN_out (local)")
    }
}

/// Loads a `(in, out)` linear layer with optional bias.
fn linear(vb: VarBuilder, in_dim: usize, out_dim: usize, use_bias: bool) -> Result<Linear> {
    let w = vb
        .get((out_dim, in_dim), "weight")
        .with_context(|| format!("load linear weight {in_dim}x{out_dim}"))?;
    let b = if use_bias {
        Some(vb.get(out_dim, "bias").context("load linear bias")?)
    } else {
        None
    };
    Ok(Linear::new(w, b))
}

/// Loads a LayerNorm with `weight` and `bias` tensors.
fn layer_norm(vb: VarBuilder, dim: usize, eps: f64) -> Result<LayerNorm> {
    let w = vb.get(dim, "weight").context("load layernorm weight")?;
    let b = vb.get(dim, "bias").context("load layernorm bias")?;
    Ok(LayerNorm::new(w, b, eps))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use candle_core::{DType, Device};
    use candle_nn::VarMap;

    /// Builds a VarBuilder backed by an in-memory VarMap so tests can
    /// construct blocks without loading from disk.
    fn vb(device: &Device) -> (VarMap, VarBuilder<'_>) {
        let varmap = VarMap::new();
        let vb = VarBuilder::from_varmap(&varmap, DType::F32, device);
        (varmap, vb)
    }

    #[test]
    fn feed_forward_loads_and_preserves_shape() {
        let device = Device::Cpu;
        let (_vm, vb) = vb(&device);
        // Initialise weights so VarMap has the required tensors.
        let ff = {
            let _w1 = vb
                .get_with_hints((128, 64), "linear1.weight", candle_nn::Init::Const(0.01))
                .unwrap();
            let _b1 = vb
                .get_with_hints(128, "linear1.bias", candle_nn::Init::Const(0.0))
                .unwrap();
            let _w2 = vb
                .get_with_hints((64, 128), "linear2.weight", candle_nn::Init::Const(0.01))
                .unwrap();
            let _b2 = vb
                .get_with_hints(64, "linear2.bias", candle_nn::Init::Const(0.0))
                .unwrap();
            FeedForward::load(vb, 64, 128, true).unwrap()
        };
        let x = Tensor::zeros((1, 8, 64), DType::F32, &device).unwrap();
        let y = ff.forward(&x).unwrap();
        assert_eq!(y.dims(), &[1, 8, 64]);
    }
}
