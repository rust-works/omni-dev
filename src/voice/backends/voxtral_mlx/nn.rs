//! Functional neural-net building blocks for the Voxtral MLX port.
//!
//! Rather than the `mlx-rs` `Module` parameter framework, the port operates
//! directly on the `name → Array` map returned by [`super::load_safetensors`].
//! [`Weights`] borrows that map and applies the primitives the Voxtral forward
//! pass needs (quantized/full-precision linear, RMS norm), naming each tensor by
//! its safetensors key so a layout mismatch surfaces as a clear error.
//!
//! **Compute dtype is F16.** The `mlx-community/…-4bit` model stores activations'
//! natural dtype as F16 (token embedding + quant scales/biases are F16; norms are
//! F32). Every primitive casts its full-precision weight to F16 at use so a single
//! activation dtype flows through the graph — matching the bandwidth-optimized
//! path INT4 exists for (the norms' F32 → F16 cast is numerically negligible).

use std::collections::HashMap;

use anyhow::{anyhow, Result};
use mlx_rs::ops::quantized_matmul;
use mlx_rs::{Array, Dtype};

/// The F16 compute dtype the whole port runs in (see module docs).
pub const COMPUTE_DTYPE: Dtype = Dtype::Float16;

/// A borrowed view over the loaded weight map plus the INT4 quantization
/// parameters, exposing the linear/norm primitives keyed by safetensors name.
pub struct Weights<'a> {
    map: &'a HashMap<String, Array>,
    group_size: i32,
    bits: i32,
}

impl<'a> Weights<'a> {
    /// Borrows `map` with the group quantization parameters from the config.
    pub fn new(map: &'a HashMap<String, Array>, group_size: i32, bits: i32) -> Self {
        Self {
            map,
            group_size,
            bits,
        }
    }

    /// Fetches a tensor by exact name, erroring if absent.
    pub fn get(&self, name: &str) -> Result<&'a Array> {
        self.map
            .get(name)
            .ok_or_else(|| anyhow!("missing expected tensor: {name:?}"))
    }

    /// True if `name` is present (used to branch on selective biases).
    pub fn has(&self, name: &str) -> bool {
        self.map.contains_key(name)
    }

    /// INT4-quantized linear: `y = x @ dequant(W)ᵀ (+ bias)`.
    ///
    /// `W` is stored `[out, in/pack]` U32 with F16 `scales`/`biases` (the quant
    /// zero-points, distinct from the optional affine `.bias`). `transpose=true`
    /// makes `quantized_matmul` compute `x @ Wᵀ`, i.e. a `[.., in] → [.., out]`
    /// linear. When `bias` is set, the F32 affine `prefix.bias` is added.
    pub fn qlinear(&self, x: &Array, prefix: &str, bias: bool) -> Result<Array> {
        let w = self.get(&format!("{prefix}.weight"))?;
        let scales = self.get(&format!("{prefix}.scales"))?;
        let qbiases = self.get(&format!("{prefix}.biases"))?;
        let mut y = quantized_matmul(x, w, scales, qbiases, true, self.group_size, self.bits)
            .map_err(|e| anyhow!("quantized_matmul {prefix}: {e}"))?;
        if bias {
            let b = self
                .get(&format!("{prefix}.bias"))?
                .as_dtype(COMPUTE_DTYPE)?;
            y = y.add(&b).map_err(|e| anyhow!("add bias {prefix}: {e}"))?;
        }
        Ok(y)
    }

    /// Full-precision linear: `y = x @ Wᵀ (+ bias)`, `W` stored `[out, in]` F16.
    pub fn linear(&self, x: &Array, prefix: &str, bias: bool) -> Result<Array> {
        let w = self
            .get(&format!("{prefix}.weight"))?
            .as_dtype(COMPUTE_DTYPE)?;
        let wt = w.transpose(&[1, 0])?;
        let mut y = x.matmul(&wt).map_err(|e| anyhow!("matmul {prefix}: {e}"))?;
        if bias {
            let b = self
                .get(&format!("{prefix}.bias"))?
                .as_dtype(COMPUTE_DTYPE)?;
            y = y.add(&b).map_err(|e| anyhow!("add bias {prefix}: {e}"))?;
        }
        Ok(y)
    }

    /// RMS norm over the last axis with the `prefix.weight` gain (F32 → F16).
    pub fn rms_norm(&self, x: &Array, prefix: &str, eps: f32) -> Result<Array> {
        let w = self
            .get(&format!("{prefix}.weight"))?
            .as_dtype(COMPUTE_DTYPE)?;
        mlx_rs::fast::rms_norm(x, &w, eps).map_err(|e| anyhow!("rms_norm {prefix}: {e}"))
    }
}

/// Builds an additive causal attention mask `[seq, seq]` in the compute dtype:
/// `0` on/below the diagonal, `-inf` above. Broadcasts against the SDPA scores
/// `[1, n_heads, seq, seq]`. Materialised on the host (cheap for the short, in-
/// window sequences the batch `encode_full` path handles).
pub fn causal_mask(seq: usize) -> Result<Array> {
    let mut data = vec![0.0_f32; seq * seq];
    for (i, row) in data.chunks_mut(seq).enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            if j > i {
                *cell = f32::NEG_INFINITY;
            }
        }
    }
    Ok(Array::from_slice(&data, &[seq as i32, seq as i32]).as_dtype(COMPUTE_DTYPE)?)
}

/// Builds an additive **sliding-window** causal mask `[seq, seq]`: `0` where
/// `j <= i` and `i - j < window`, `-inf` otherwise. A single SDPA over this mask
/// is mathematically identical to the reference's chunked rotating-KV-cache
/// encoding for the whole sequence (both realise sliding-window causal
/// attention); it trades the streaming memory bound for simplicity, so it is used
/// for offline clips that fit in memory (the rotating cache returns for long
/// audio / streaming, M3).
pub fn sliding_window_mask(seq: usize, window: usize) -> Result<Array> {
    let mut data = vec![0.0_f32; seq * seq];
    for (i, row) in data.chunks_mut(seq).enumerate() {
        for (j, cell) in row.iter_mut().enumerate() {
            if j > i || i - j >= window {
                *cell = f32::NEG_INFINITY;
            }
        }
    }
    Ok(Array::from_slice(&data, &[seq as i32, seq as i32]).as_dtype(COMPUTE_DTYPE)?)
}
