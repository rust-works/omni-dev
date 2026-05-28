//! Conformer convolution module.
//!
//! Pointwise → GLU → depthwise → BatchNorm → SiLU → pointwise. Mirrors
//! `senstella/parakeet-mlx::parakeet_mlx/conformer.py::Convolution` —
//! same op order, same kernel/stride/padding, same `groups=d_model`
//! depthwise (so the depthwise weight has shape `(d_model, 1, k)`).
//!
//! Two pieces worth flagging:
//!
//! - **Layout.** MLX `nn.Conv1d` takes `(B, T, C)` channels-last, and
//!   the upstream module operates on inputs of that shape. candle's
//!   [`candle_nn::Conv1d`] takes channels-first `(B, C, T)`. The forward
//!   pass below transposes once on entry, runs all convs in channels-
//!   first, and transposes back on exit so the calling
//!   [`ConformerBlock`](super::conformer_block::ConformerBlock) still
//!   sees `(B, T, C)`. This matches the
//!   convention the rest of the encoder expects.
//!
//! - **GLU axis.** MLX `nn.glu(x, axis=2)` splits the last axis (channels)
//!   in half. After our channels-first transpose, channels live on
//!   axis 1, so the GLU split happens on dim 1.
//!
//! - **BatchNorm running stats.** The converter ships `running_mean` and
//!   `running_var` in the safetensors blob; this module loads both and
//!   evaluates inference-only (`(x - mean) / sqrt(var + eps) * γ + β`).
//!   Training mode is not supported — the port is inference-only.

use anyhow::{Context, Result};
use candle_core::{Module, ModuleT, Tensor, D};
use candle_nn::{
    batch_norm, ops::silu, BatchNorm, BatchNormConfig, Conv1d, Conv1dConfig, VarBuilder,
};

/// Conformer convolution module — the `conv` block sandwiched between
/// the two feed-forward sub-blocks in a
/// [`ConformerBlock`](super::conformer_block::ConformerBlock).
///
/// Inputs and outputs are `(batch, time, channels)` in channels-last
/// layout to match the rest of the FastConformer encoder; convolutions
/// run channels-first under the hood.
pub struct ConvolutionModule {
    pointwise_conv1: Conv1d,
    depthwise_conv: Conv1d,
    batch_norm: BatchNorm,
    pointwise_conv2: Conv1d,
    padding: usize,
}

impl Clone for ConvolutionModule {
    fn clone(&self) -> Self {
        // All inner types are Arc-backed; cloning is cheap and shares
        // the underlying weight tensors.
        Self {
            pointwise_conv1: self.pointwise_conv1.clone(),
            depthwise_conv: self.depthwise_conv.clone(),
            batch_norm: self.batch_norm.clone(),
            pointwise_conv2: self.pointwise_conv2.clone(),
            padding: self.padding,
        }
    }
}

impl ConvolutionModule {
    /// Loads the four conv layers + BatchNorm from `vb`. `kernel_size`
    /// must be odd (so left/right padding are equal); Parakeet 0.6B v2
    /// uses `kernel_size = 9`.
    pub fn load(
        vb: VarBuilder,
        d_model: usize,
        kernel_size: usize,
        use_bias: bool,
    ) -> Result<Self> {
        anyhow::ensure!(
            kernel_size % 2 == 1,
            "ConvolutionModule: kernel_size ({kernel_size}) must be odd"
        );
        let padding = (kernel_size - 1) / 2;

        // pointwise_conv1: (d_model -> 2*d_model, kernel=1) — output
        // halves drive the GLU below.
        let pointwise_conv1 = conv1d(
            vb.pp("pointwise_conv1"),
            d_model,
            d_model * 2,
            1,
            1,
            0,
            1,
            use_bias,
        )
        .context("load pointwise_conv1")?;

        // depthwise_conv: (d_model -> d_model, kernel=k, groups=d_model).
        // Padding 0 — we manually symmetric-pad the input before calling.
        let depthwise_conv = conv1d(
            vb.pp("depthwise_conv"),
            d_model,
            d_model,
            kernel_size,
            1,
            0,
            d_model,
            use_bias,
        )
        .context("load depthwise_conv")?;

        // BatchNorm over the channel axis — inference-only, uses the
        // running mean/var the converter persisted to safetensors.
        let batch_norm = batch_norm(d_model, BatchNormConfig::default(), vb.pp("batch_norm"))
            .map_err(|e| anyhow::anyhow!("load batch_norm: {e}"))?;

        // pointwise_conv2: (d_model -> d_model, kernel=1).
        let pointwise_conv2 = conv1d(
            vb.pp("pointwise_conv2"),
            d_model,
            d_model,
            1,
            1,
            0,
            1,
            use_bias,
        )
        .context("load pointwise_conv2")?;

        Ok(Self {
            pointwise_conv1,
            depthwise_conv,
            batch_norm,
            pointwise_conv2,
            padding,
        })
    }

    /// Forward pass on a channels-last `(batch, time, d_model)` tensor.
    /// Returns the same shape. Thin wrapper over [`Self::forward_with_cache`]
    /// with no cache (symmetric zero-padding for the depthwise conv).
    pub fn forward(&self, x: &Tensor) -> Result<Tensor> {
        self.forward_with_cache(x, None)
    }

    /// Forward pass with optional KV-cache for streaming.
    ///
    /// When `cache` is `Some`, the depthwise conv's symmetric zero-padding
    /// is replaced by
    /// [`super::cache::RotatingConformerCache::update_and_fetch_conv`],
    /// which returns the input with the cached prefix prepended and
    /// `padding` zeros suffix-appended. The cache holds the last
    /// `padding` tokens from the previous chunk so the depthwise conv
    /// sees continuous audio context across calls (rather than
    /// hard-zeros at the chunk boundary).
    ///
    /// When `cache` is `None`, behaviour is identical to the existing
    /// batch path: symmetric zero-padding.
    pub fn forward_with_cache(
        &self,
        x: &Tensor,
        cache: Option<&mut super::cache::RotatingConformerCache>,
    ) -> Result<Tensor> {
        // (B, T, C) -> (B, C, T) for the convolutions.
        let x = x.transpose(1, 2)?.contiguous()?;

        // pointwise_conv1: (B, C, T) -> (B, 2C, T)
        let x = self
            .pointwise_conv1
            .forward(&x)
            .context("pointwise_conv1 forward")?;

        // GLU on the channel axis: split (B, 2C, T) -> two (B, C, T)
        // halves; output = a * sigmoid(b).
        let x = glu(&x, 1).context("glu split")?;

        // Pad the time axis. With cache: use update_and_fetch_conv,
        // which returns channels-last (B, T + 2*pad, C) with the cached
        // prefix prepended. Without cache: symmetric zero-pad.
        let x = if let Some(cache) = cache {
            // The cache operates channels-last; transpose, update, transpose back.
            let cl = x.transpose(1, 2)?.contiguous()?; // (B, T, C)
            let padded = cache
                .update_and_fetch_conv(&cl, self.padding)
                .context("update_and_fetch_conv")?;
            padded.transpose(1, 2)?.contiguous()? // (B, C, T + 2*pad)
        } else {
            pad_time(&x, self.padding).context("pad before depthwise")?
        };

        let x = self
            .depthwise_conv
            .forward(&x)
            .context("depthwise_conv forward")?;

        // BatchNorm — inference mode (no per-batch stats).
        let x = self
            .batch_norm
            .forward_t(&x, false)
            .map_err(|e| anyhow::anyhow!("batch_norm forward: {e}"))?;

        let x = silu(&x).context("silu activation")?;

        let x = self
            .pointwise_conv2
            .forward(&x)
            .context("pointwise_conv2 forward")?;

        // Back to channels-last.
        let x = x.transpose(1, 2)?.contiguous()?;
        Ok(x)
    }
}

/// GLU on the given channel axis: split the tensor in half, return
/// `a * sigmoid(b)`. `axis` is the dim whose size must be even.
fn glu(x: &Tensor, axis: usize) -> Result<Tensor> {
    let dim_size = x.dim(axis)?;
    anyhow::ensure!(
        dim_size % 2 == 0,
        "glu: dim {axis} size ({dim_size}) must be even"
    );
    let half = dim_size / 2;
    let a = x.narrow(axis, 0, half)?;
    let b = x.narrow(axis, half, half)?;
    let gate = candle_nn::ops::sigmoid(&b).context("glu sigmoid")?;
    a.mul(&gate).context("glu multiply")
}

/// Symmetric zero-pad on the *time* axis of a channels-first
/// `(B, C, T)` tensor.
fn pad_time(x: &Tensor, pad: usize) -> Result<Tensor> {
    if pad == 0 {
        return Ok(x.clone());
    }
    let (b, c, _t) = x.dims3().context("pad_time expects 3-D tensor")?;
    let zeros = Tensor::zeros((b, c, pad), x.dtype(), x.device())?;
    Tensor::cat(&[&zeros, x, &zeros], D::Minus1).context("pad_time concat")
}

/// Loads a `(out, in/groups, kernel)`-shaped weight + optional bias.
#[allow(clippy::too_many_arguments)]
fn conv1d(
    vb: VarBuilder,
    in_channels: usize,
    out_channels: usize,
    kernel_size: usize,
    stride: usize,
    padding: usize,
    groups: usize,
    use_bias: bool,
) -> Result<Conv1d> {
    let cfg = Conv1dConfig {
        padding,
        stride,
        groups,
        ..Default::default()
    };
    let w = vb
        .get((out_channels, in_channels / groups, kernel_size), "weight")
        .with_context(|| {
            format!(
                "load conv1d weight ({out_channels}, {}, {kernel_size})",
                in_channels / groups
            )
        })?;
    let b = if use_bias {
        Some(vb.get(out_channels, "bias").context("load conv1d bias")?)
    } else {
        None
    };
    Ok(Conv1d::new(w, b, cfg))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use candle_core::{DType, Device};

    fn cpu() -> Device {
        Device::Cpu
    }

    #[test]
    fn glu_splits_and_gates() {
        // (B=1, C=4, T=1) — input [1, 2, 3, 4]. GLU on axis 1 splits
        // into [1, 2] (linear half) and [3, 4] (gate half), returns
        // [1*sigmoid(3), 2*sigmoid(4)].
        let x = Tensor::from_slice(&[1.0_f32, 2.0, 3.0, 4.0], (1, 4, 1), &cpu()).unwrap();
        let y = glu(&x, 1).unwrap();
        assert_eq!(y.dims(), &[1, 2, 1]);
        let v: Vec<f32> = y.flatten_all().unwrap().to_vec1::<f32>().unwrap();
        let sig3 = 1.0 / (1.0 + (-3.0_f32).exp());
        let sig4 = 1.0 / (1.0 + (-4.0_f32).exp());
        assert!((v[0] - sig3).abs() < 1e-6, "expected {sig3}, got {}", v[0]);
        let expected = 2.0_f32 * sig4;
        assert!(
            (v[1] - expected).abs() < 1e-6,
            "expected {expected}, got {}",
            v[1]
        );
    }

    #[test]
    fn glu_rejects_odd_channel_dim() {
        let x = Tensor::zeros((1, 3, 1), DType::F32, &cpu()).unwrap();
        let err = glu(&x, 1).unwrap_err();
        assert!(err.to_string().contains("must be even"), "got: {err}");
    }

    #[test]
    fn pad_time_pads_symmetrically_on_last_axis() {
        // (B=1, C=2, T=3) padded by 2 -> (1, 2, 7) with 2 zeros each side.
        let x = Tensor::from_slice(&[1.0_f32, 2.0, 3.0, 4.0, 5.0, 6.0], (1, 2, 3), &cpu()).unwrap();
        let y = pad_time(&x, 2).unwrap();
        assert_eq!(y.dims(), &[1, 2, 7]);
        let v: Vec<f32> = y.flatten_all().unwrap().to_vec1::<f32>().unwrap();
        // Row 0: [0, 0, 1, 2, 3, 0, 0]; row 1: [0, 0, 4, 5, 6, 0, 0]
        assert_eq!(
            v,
            vec![0.0, 0.0, 1.0, 2.0, 3.0, 0.0, 0.0, 0.0, 0.0, 4.0, 5.0, 6.0, 0.0, 0.0]
        );
    }

    #[test]
    fn pad_time_zero_is_passthrough() {
        let x = Tensor::ones((1, 2, 3), DType::F32, &cpu()).unwrap();
        let y = pad_time(&x, 0).unwrap();
        assert_eq!(y.dims(), x.dims());
    }
}
