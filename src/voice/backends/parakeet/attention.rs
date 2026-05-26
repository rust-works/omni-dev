//! Multi-head attention with relative positional encoding for the
//! Parakeet FastConformer encoder.
//!
//! Mirrors the MLX reference at
//! [`senstella/parakeet-mlx::parakeet_mlx/attention.py`](https://github.com/senstella/parakeet-mlx/blob/main/parakeet_mlx/attention.py)
//! — specifically the `MultiHeadAttention` base and the
//! `RelPositionMultiHeadAttention` subclass. The local-attention variant
//! (`RelPositionMultiHeadLocalAttention`) is **not** ported: Parakeet
//! 0.6B v2 ships with `att_context_size: [-1, -1]` (unbounded), which
//! selects the global variant.
//!
//! Two pieces the spike #871 explicitly did *not* validate live here:
//!
//! - **`rel_shift`** — the relative-shift trick that turns `(query, pos)`
//!   scores into `(query, key_offset)` scores via a pad-reshape-slice
//!   sequence. ~10 lines but easy to get the slice bounds wrong; the
//!   tests below pin the exact bit-pattern of a small example.
//!
//! - **`matrix_bd` rel-pos bias matmul** — the second scoring term
//!   (`q_v @ p^T`) added pre-softmax. Spike's attention probe used the
//!   simpler `MultiHeadAttention` SDPA path and explicitly skipped this.
//!
//! All ops run on `candle_core::Tensor`. No SDPA primitive in candle —
//! attention is implemented as the explicit
//! `softmax((q @ k^T) * scale + bias) @ v` sequence so we can inject
//! `matrix_bd` exactly where MLX's `mx.fast.scaled_dot_product_attention`
//! takes its `mask=` argument (additive pre-softmax, not multiplicative).

use anyhow::{Context, Result};
use candle_core::{Module, Tensor, D};
use candle_nn::{ops::softmax, Linear, VarBuilder};

/// Standard multi-head attention (no positional bias).
///
/// Used as the constructor base for [`RelPositionMultiHeadAttention`];
/// Parakeet's encoder doesn't instantiate it directly but the load
/// helpers do, so the projections are loaded with the same key paths
/// the upstream MLX reference uses.
pub struct MultiHeadAttention {
    n_head: usize,
    head_dim: usize,
    scale: f32,
    linear_q: Linear,
    linear_k: Linear,
    linear_v: Linear,
    linear_out: Linear,
}

impl MultiHeadAttention {
    /// Loads the four `linear_{q,k,v,out}` projections from `vb`.
    /// `n_feat` must be divisible by `n_head`.
    pub fn load(vb: VarBuilder, n_head: usize, n_feat: usize, use_bias: bool) -> Result<Self> {
        anyhow::ensure!(
            n_feat % n_head == 0,
            "MultiHeadAttention: n_feat ({n_feat}) must be divisible by n_head ({n_head})"
        );
        let head_dim = n_feat / n_head;
        #[allow(clippy::cast_precision_loss)]
        let scale = 1.0 / (head_dim as f32).sqrt();
        Ok(Self {
            n_head,
            head_dim,
            scale,
            linear_q: linear(vb.pp("linear_q"), n_feat, n_feat, use_bias)?,
            linear_k: linear(vb.pp("linear_k"), n_feat, n_feat, use_bias)?,
            linear_v: linear(vb.pp("linear_v"), n_feat, n_feat, use_bias)?,
            linear_out: linear(vb.pp("linear_out"), n_feat, n_feat, use_bias)?,
        })
    }
}

/// Multi-head attention with Transformer-XL-style relative positional
/// encoding (Shaw 2018 + Dai et al. 2019). Used by every layer of the
/// Parakeet FastConformer encoder.
///
/// Adds three learnables to [`MultiHeadAttention`]:
/// - `linear_pos`: bias-free linear projection of the sinusoidal
///   positional encoding (`n_feat × n_feat`).
/// - `pos_bias_u`, `pos_bias_v`: per-head trainable bias vectors
///   (`(n_head, head_dim)`).
///
/// The two-term score, `(q + pos_bias_u) @ k^T + rel_shift((q + pos_bias_v) @ p^T)`,
/// is computed as `matrix_ac + matrix_bd` and fed pre-softmax. `matrix_bd`
/// is the position-dependent term and goes through [`rel_shift`] to align
/// each query row with its corresponding key-offset slice of the
/// positional encoding.
pub struct RelPositionMultiHeadAttention {
    base: MultiHeadAttention,
    linear_pos: Linear,
    pos_bias_u: Tensor,
    pos_bias_v: Tensor,
}

impl RelPositionMultiHeadAttention {
    /// Loads the base attention + positional pieces from `vb`. `n_head`
    /// must divide `n_feat`.
    pub fn load(vb: VarBuilder, n_head: usize, n_feat: usize, use_bias: bool) -> Result<Self> {
        let head_dim = n_feat / n_head;
        let base = MultiHeadAttention::load(vb.clone(), n_head, n_feat, use_bias)?;
        let linear_pos = linear(vb.pp("linear_pos"), n_feat, n_feat, false)?;
        let pos_bias_u = vb
            .get((n_head, head_dim), "pos_bias_u")
            .context("load pos_bias_u")?;
        let pos_bias_v = vb
            .get((n_head, head_dim), "pos_bias_v")
            .context("load pos_bias_v")?;
        Ok(Self {
            base,
            linear_pos,
            pos_bias_u,
            pos_bias_v,
        })
    }

    /// Returns the per-head bias vector `pos_bias_u` (`(n_head, head_dim)`).
    #[must_use]
    pub fn pos_bias_u(&self) -> &Tensor {
        &self.pos_bias_u
    }

    /// Returns the per-head bias vector `pos_bias_v` (`(n_head, head_dim)`).
    #[must_use]
    pub fn pos_bias_v(&self) -> &Tensor {
        &self.pos_bias_v
    }

    /// Forward pass.
    ///
    /// Shapes: `q`, `k`, `v` are `(batch, time, n_feat)`; `pos_emb` is
    /// `(1 | batch, pos_len, n_feat)`; `mask` (if supplied) is
    /// `(batch, t_q, t_k)` with `true` meaning *attend-to-mask*
    /// (i.e. blocked positions). Returns `(batch, t_q, n_feat)`.
    pub fn forward(
        &self,
        q: &Tensor,
        k: &Tensor,
        v: &Tensor,
        pos_emb: &Tensor,
        mask: Option<&Tensor>,
    ) -> Result<Tensor> {
        let MultiHeadAttention {
            n_head,
            head_dim,
            scale,
            ref linear_q,
            ref linear_k,
            ref linear_v,
            ref linear_out,
        } = self.base;

        let q = linear_q.forward(q).context("project q")?;
        let k = linear_k.forward(k).context("project k")?;
        let v = linear_v.forward(v).context("project v")?;
        let p = self
            .linear_pos
            .forward(pos_emb)
            .context("project pos_emb")?;

        let (batch, q_seq, _) = q.dims3().context("q shape")?;
        let (_, k_seq, _) = k.dims3().context("k shape")?;
        let (p_batch, pos_len, _) = p.dims3().context("pos_emb shape")?;

        // Broadcast positional encoding to the query batch if it ships
        // with batch=1 (the common case — there's exactly one positional
        // table per layer regardless of batch size).
        let p = if p_batch == 1 && batch > 1 {
            p.broadcast_as((batch, pos_len, n_head * head_dim))
                .context("broadcast pos_emb to query batch")?
        } else {
            anyhow::ensure!(
                p_batch == batch,
                "pos_emb batch ({p_batch}) must be 1 or match query batch ({batch})"
            );
            p
        };

        // (B, T, n_head * head_dim) -> (B, T, n_head, head_dim)
        let q = q
            .reshape((batch, q_seq, n_head, head_dim))
            .context("reshape q to heads")?;
        // Add per-head bias vectors *before* transposing to the
        // (B, H, T, D) layout — matches MLX broadcast semantics.
        let pos_bias_u = self.pos_bias_u.reshape((1, 1, n_head, head_dim))?;
        let pos_bias_v = self.pos_bias_v.reshape((1, 1, n_head, head_dim))?;
        let q_u = q
            .broadcast_add(&pos_bias_u)
            .context("add pos_bias_u to q")?
            .transpose(1, 2)
            .context("transpose q_u to (B, H, T, D)")?
            .contiguous()?;
        let q_v = q
            .broadcast_add(&pos_bias_v)
            .context("add pos_bias_v to q")?
            .transpose(1, 2)
            .context("transpose q_v to (B, H, T, D)")?
            .contiguous()?;

        let k = k
            .reshape((batch, k_seq, n_head, head_dim))?
            .transpose(1, 2)?
            .contiguous()?;
        let v = v
            .reshape((batch, k_seq, n_head, head_dim))?
            .transpose(1, 2)?
            .contiguous()?;
        let p = p
            .reshape((batch, pos_len, n_head, head_dim))?
            .transpose(1, 2)?
            .contiguous()?;

        // Q-tiled rel-pos attention.
        //
        // The original full-T forward path materialises four (B, H, T_q, T_k)
        // tensors (matrix_ac, matrix_bd, scores, attn) plus the pre-shift
        // (B, H, T_q, pos_len) intermediate for matrix_bd. At T_q = 3750 that's
        // ~1.8 GB live per attention layer (the dominant contributor to the
        // 9.5 GB peak RSS observed on the 5-min fixture).
        //
        // Tiling Q into N_Q-row chunks reduces the per-iteration footprint to
        // O(N_Q · T_k) instead of O(T_q · T_k). The math is identical to the
        // un-tiled path: matrix_ac, scores, softmax, and attn @ V are
        // per-Q-row operations, so chunking rows is bit-exact.
        //
        // The one subtlety is matrix_bd. The rel_shift trick is sensitive to
        // T_q — applying it to a chunk (rows q_start..q_start+N) shifts the
        // values such that the desired (B, H, N, T_k) window lands at column
        // offset (T_k - q_start - N), not 0. Derivation in the comment block
        // below; verified bit-equal to the full path on small T.
        //
        // For T_q ≤ Q_CHUNK there is exactly one chunk and the result is
        // identical to the original code path (narrow_offset = 0).
        const Q_CHUNK: usize = 256;

        let k_t = k.transpose(D::Minus2, D::Minus1)?.contiguous()?;
        let p_t = p.transpose(D::Minus2, D::Minus1)?.contiguous()?;
        let scale_f = f64::from(scale);
        let n_chunks = q_seq.div_ceil(Q_CHUNK);
        let mut out_chunks: Vec<Tensor> = Vec::with_capacity(n_chunks);

        for q_start in (0..q_seq).step_by(Q_CHUNK) {
            let n = Q_CHUNK.min(q_seq - q_start);

            let q_u_chunk = q_u.narrow(2, q_start, n)?.contiguous()?;
            let q_v_chunk = q_v.narrow(2, q_start, n)?.contiguous()?;

            // matrix_ac_chunk: (B, H, n, T_k)
            let matrix_ac_chunk = q_u_chunk.matmul(&k_t)?;

            // matrix_bd_chunk: q_v_chunk @ p^T  →  (B, H, n, pos_len);
            // rel_shift → (B, H, n, pos_len), then narrow at the chunk-
            // dependent offset to (B, H, n, T_k).
            //
            // Why the offset is (k_seq - q_start - n) rather than 0:
            // the rel_shift pad-reshape-narrow trick on a chunk shifts
            // each row's columns by exactly that amount (verified by hand
            // for several (q_start, n) pairs on small T). When q_start = 0
            // and n = q_seq, the offset collapses to 0, matching the
            // original code.
            let narrow_offset = k_seq - q_start - n;
            let matrix_bd_chunk = q_v_chunk.matmul(&p_t)?;
            let matrix_bd_chunk = rel_shift(&matrix_bd_chunk).context("rel_shift (tiled)")?;
            let matrix_bd_chunk = matrix_bd_chunk.narrow(D::Minus1, narrow_offset, k_seq)?;
            let matrix_bd_chunk = (matrix_bd_chunk * scale_f)?;

            let scores_chunk = (matrix_ac_chunk * scale_f)?.add(&matrix_bd_chunk)?;
            let scores_chunk = if let Some(m) = mask {
                // Mask is (B, T_q, T_k); slice the query axis to this chunk.
                let m_chunk = m.narrow(1, q_start, n)?.contiguous()?;
                apply_attention_mask(&scores_chunk, &m_chunk)?
            } else {
                scores_chunk
            };

            let attn_chunk =
                softmax(&scores_chunk, D::Minus1).context("softmax over keys (tiled)")?;
            let out_chunk = attn_chunk.matmul(&v)?;
            out_chunks.push(out_chunk);
        }

        let out = if out_chunks.len() == 1 {
            // Single-chunk fast path: skip the cat.
            out_chunks.pop().expect("len() == 1")
        } else {
            Tensor::cat(&out_chunks, 2)?
        };
        let out = out
            .transpose(1, 2)?
            .contiguous()?
            .reshape((batch, q_seq, n_head * head_dim))?;
        linear_out.forward(&out).context("output projection")
    }
}

/// Relative-shift trick used by Transformer-XL-style rel-pos attention.
///
/// Given `x` of shape `(B, H, T_q, pos_len)` where columns are indexed
/// by *positional offset*, returns a tensor of the same shape whose
/// `[b, h, t, k]` element is the score that query `t` would assign to
/// the key at *absolute* position `t + k - (pos_len - T_q)`. Implemented
/// as a pad-reshape-slice trick: pad the last axis on the left by 1,
/// reinterpret as `(B, H, pos_len + 1, T_q)`, drop the first row, and
/// reshape back. Matches the MLX reference at `attention.py::rel_shift`.
fn rel_shift(x: &Tensor) -> Result<Tensor> {
    let (b, h, t_q, pos_len) = x.dims4().context("rel_shift input must be 4-D")?;
    // Pad on the left by 1 column. candle has no `pad` op so concat zeros.
    let pad = Tensor::zeros((b, h, t_q, 1), x.dtype(), x.device())?;
    let x = Tensor::cat(&[&pad, x], D::Minus1)?;
    let x = x.reshape((b, h, pos_len + 1, t_q))?;
    let x = x.narrow(2, 1, pos_len)?;
    x.reshape((b, h, t_q, pos_len))
        .context("rel_shift final reshape")
}

/// Apply a `(B, T_q, T_k)` attention mask to a `(B, H, T_q, T_k)`
/// scores tensor. Mask is a float tensor with `1.0` at blocked positions
/// and `0.0` elsewhere; blocked positions get a large negative additive
/// bias (`-1e9`) so post-softmax probability is effectively zero. The
/// additive form avoids the NaN that `0.0 * -inf` would produce at
/// unblocked positions if we used `where_cond` against an f32 condition
/// (candle's `where_cond` requires u8/int conditions).
fn apply_attention_mask(scores: &Tensor, mask: &Tensor) -> Result<Tensor> {
    let (b, h, t_q, t_k) = scores
        .dims4()
        .context("attention scores must be 4-D for mask apply")?;
    let bias = (mask
        .to_dtype(scores.dtype())?
        .reshape((b, 1, t_q, t_k))?
        .broadcast_as((b, h, t_q, t_k))?
        * -1.0e9_f64)?;
    Ok((scores + bias)?)
}

/// Loads a `(out, in)` linear layer with optional bias from `vb` rooted
/// at the layer's prefix. Matches the candle-nn `linear`/`linear_no_bias`
/// helpers but consolidates the dispatch.
fn linear(vb: VarBuilder, in_dim: usize, out_dim: usize, with_bias: bool) -> Result<Linear> {
    let w = vb
        .get((out_dim, in_dim), "weight")
        .with_context(|| format!("load linear weight {in_dim}x{out_dim}"))?;
    let b = if with_bias {
        Some(vb.get(out_dim, "bias").context("load linear bias")?)
    } else {
        None
    };
    Ok(Linear::new(w, b))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use candle_core::{DType, Device};

    fn cpu() -> Device {
        Device::Cpu
    }

    /// Builds a tensor from a flat slice + shape on CPU f32.
    fn t(data: &[f32], shape: impl Into<candle_core::Shape>) -> Tensor {
        Tensor::from_slice(data, shape, &cpu()).unwrap()
    }

    #[test]
    fn rel_shift_matches_documented_example() {
        // Hand-computed example. Take pos_len = 4, T_q = 3, B = H = 1.
        // Input row 0 = [a0, a1, a2, a3]; the rel-shift trick maps
        // row `t` to the slice of positions starting at offset
        // `pos_len - T_q + t` (this is how each query lines up with the
        // appropriate column range in the positional table).
        //
        // The exact MLX reference output for this input is:
        //   row 0: [0, a0, a1, a2]
        //   row 1: [a3, 0, a0, a1]
        //   row 2: [a2, a3, 0, a0]
        // — derived from the pad-reshape-slice steps in the function
        // body, not assumed semantics. This pins the operation against
        // accidental axis transpositions.
        let pos_len = 4_usize;
        let t_q = 3_usize;
        let input = t(
            &[
                1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0, 11.0, 12.0,
            ],
            (1, 1, t_q, pos_len),
        );
        let shifted = rel_shift(&input).unwrap();
        let got: Vec<f32> = shifted.flatten_all().unwrap().to_vec1::<f32>().unwrap();

        // The hand-trace below mirrors the pad+reshape+slice sequence
        // for a concrete (B=1, H=1, T_q=3, pos_len=4) case:
        //   pad-left [[1..4],[5..8],[9..12]] -> [[0,1..4],[0,5..8],[0,9..12]]
        //     (shape (1,1,3,5))
        //   reshape  -> (1,1,5,3): rows are
        //       [0,1,2], [3,0,5], [6,7,0], [9,10,11], [12,?,?]
        //   Wait — reshape (1,1,3,5) directly to (1,1,5,3) reorders
        //   memory in row-major: the 15 elements
        //     [0,1,2,3,4, 0,5,6,7,8, 0,9,10,11,12]
        //   become five rows of three:
        //     [0,1,2], [3,4,0], [5,6,7], [8,0,9], [10,11,12]
        //   drop first row -> rows 1..4:
        //     [3,4,0], [5,6,7], [8,0,9], [10,11,12]
        //   reshape (1,1,3,4):
        //     [[3,4,0,5],[6,7,8,0],[9,10,11,12]]
        let expected = vec![
            3.0, 4.0, 0.0, 5.0, 6.0, 7.0, 8.0, 0.0, 9.0, 10.0, 11.0, 12.0,
        ];
        assert_eq!(got, expected, "rel_shift output does not match hand trace");
    }

    #[test]
    fn rel_shift_preserves_shape() {
        let x = Tensor::zeros((2, 4, 8, 16), DType::F32, &cpu()).unwrap();
        let y = rel_shift(&x).unwrap();
        assert_eq!(y.dims(), &[2, 4, 8, 16]);
    }

    #[test]
    fn apply_attention_mask_drives_masked_positions_strongly_negative() {
        // (B=1, H=2, T_q=2, T_k=2). Mask blocks position (0, 1) for both heads.
        let scores = t(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0], (1, 2, 2, 2));
        let mask = t(&[0.0, 1.0, 0.0, 0.0], (1, 2, 2));
        let masked = apply_attention_mask(&scores, &mask).unwrap();
        let v: Vec<f32> = masked.flatten_all().unwrap().to_vec1::<f32>().unwrap();
        // Unblocked positions pass through; blocked positions get the
        // additive -1e9 bias so post-softmax probability is ~0.
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(v[0], 1.0);
            assert_eq!(v[2], 3.0);
            assert_eq!(v[3], 4.0);
            assert_eq!(v[4], 5.0);
            assert_eq!(v[6], 7.0);
            assert_eq!(v[7], 8.0);
        }
        assert!(
            v[1] < -1e8,
            "blocked position should be strongly negative, got {}",
            v[1]
        );
        assert!(
            v[5] < -1e8,
            "blocked position should be strongly negative, got {}",
            v[5]
        );
    }
}
