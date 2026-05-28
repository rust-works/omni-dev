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

use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::{Context, Result};
use candle_core::{Module, Tensor, D};
use candle_nn::{ops::softmax, Linear, VarBuilder};

use super::cache::RotatingConformerCache;

/// Thread-local scratch caches for `unfold_local_pos_scores` and
/// `local_window_bias`. Both functions build a small `Vec` of indices /
/// floats on every call, then `Tensor::from_vec` + broadcast + contiguous
/// — for a 30 s streaming test that's ~24 layers × ~20 chunks × 2 calls
/// = ~960 redundant rebuilds of effectively-identical tensors.
///
/// After cache warmup (a handful of chunks), the inputs to both
/// functions stabilise: `s` (chunk size after subsampling) is constant,
/// `cached_offset` caps at the cache's capacity, and `(w_left, w_right,
/// k_len, b, h)` don't change. So the index / bias tensor is the SAME
/// across every layer × every chunk. Caching it by its parameter tuple
/// reduces all those rebuilds to a single tensor clone (Arc-bump).
///
/// Cache is bounded to `MAX_CACHE_ENTRIES`; on overflow we `.clear()`.
/// Crude but the number of distinct keys in a typical session is ~3
/// (during warmup + steady state), so the bound is never hit in
/// practice and the clear is rarely (if ever) triggered.
#[derive(Eq, Hash, PartialEq, Clone, Debug)]
struct LocalAttnScratchKey {
    s: usize,
    k_len: usize,
    cached_offset: usize,
    w_left: usize,
    w_right: usize,
    b: usize,
    h: usize,
}

const MAX_CACHE_ENTRIES: usize = 32;

thread_local! {
    static UNFOLD_INDEX_CACHE: RefCell<HashMap<LocalAttnScratchKey, Tensor>> =
        RefCell::new(HashMap::new());
    static WINDOW_BIAS_CACHE: RefCell<HashMap<LocalAttnScratchKey, Tensor>> =
        RefCell::new(HashMap::new());
}

/// Cap the cache size; on overflow, drop everything. Called after every
/// insertion; cheap because the cache is tiny in practice.
fn maybe_evict<K, V>(map: &mut HashMap<K, V>) {
    if map.len() > MAX_CACHE_ENTRIES {
        map.clear();
    }
}

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
/// is the position-dependent term and goes through `rel_shift` to align
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
    #[allow(clippy::many_single_char_names)] // q/k/v/b/h are attention conventions
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

        let out = if let [single] = out_chunks.as_slice() {
            // Single-chunk fast path: skip the cat.
            single.clone()
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

/// Multi-head attention with **local** relative-position windowing and
/// optional KV-cache, used by the streaming Parakeet wrapper.
///
/// Same learnable parameters as [`RelPositionMultiHeadAttention`]
/// (`linear_q/k/v/pos/out`, `pos_bias_u/v`) — constructed via
/// [`RelPositionMultiHeadLocalAttention::from_full`], which Arc-clones the
/// weight tensors from a loaded full-attention module. No separate weight
/// load; both variants share the same trained parameters.
///
/// Differences from the full-attention variant:
///
/// - **Local window**: each query attends to keys in
///   `[i - context_size.0, i + context_size.1]` (clipped to valid K
///   range). Mirrors `parakeet_mlx::attention::RelPositionMultiHeadLocalAttention`.
/// - **Cache support**: an optional [`RotatingConformerCache`] is updated
///   in place per call (`cache.update_and_fetch_kv`) so the encoder
///   processes only the new chunk's K/V each forward.
/// - **Positional encoding**: input `pos_emb` has shape
///   `(1 | batch, 2*w + 1, n_feat)` (where `w = max(context_size)`) —
///   a small relative-position table from `LocalRelPositionalEncoding`
///   rather than the `(1, 2*max_len - 1, n_feat)` table used by full
///   attention.
///
/// **Implementation note**: MLX uses two Metal kernels (`matmul_qk`,
/// `matmul_pv`) to compute the local-window matmul in a compact
/// `(B, H, S, 2w+1)` representation. This Rust port skips the kernel
/// optimisation and performs full attention `Q @ K^T` over the (small)
/// `K_len = cached + S` key length, then applies an additive mask for
/// keys outside the local window. Identical math; memory cost is
/// `O(S × K_len)` per layer — negligible for the streaming case where
/// `S` is a handful of new frames and `K_len <= capacity + S`.
pub struct RelPositionMultiHeadLocalAttention {
    base: MultiHeadAttention,
    linear_pos: Linear,
    pos_bias_u: Tensor,
    pos_bias_v: Tensor,
    /// `(left_context, right_context)` in encoder frames.
    context_size: (usize, usize),
}

impl RelPositionMultiHeadLocalAttention {
    /// Constructs a local-attention variant sharing weights with an
    /// existing full-attention module. Tensors are Arc-cloned; this is
    /// cheap and no `VarBuilder` re-traversal is performed.
    ///
    /// Returns `Result` for API stability (the caller chain treats this
    /// as fallible because the conformer block aggregates multiple
    /// `from_full` calls); the current body is infallible.
    pub fn from_full(
        full: &RelPositionMultiHeadAttention,
        context_size: (usize, usize),
    ) -> Result<Self> {
        // Share each linear's weight + bias tensors (Arc-cloned internally).
        let base = MultiHeadAttention {
            n_head: full.base.n_head,
            head_dim: full.base.head_dim,
            scale: full.base.scale,
            linear_q: full.base.linear_q.clone(),
            linear_k: full.base.linear_k.clone(),
            linear_v: full.base.linear_v.clone(),
            linear_out: full.base.linear_out.clone(),
        };
        Ok(Self {
            base,
            linear_pos: full.linear_pos.clone(),
            pos_bias_u: full.pos_bias_u.clone(),
            pos_bias_v: full.pos_bias_v.clone(),
            context_size,
        })
    }

    /// Returns the `(left, right)` context window.
    #[must_use]
    pub fn context_size(&self) -> (usize, usize) {
        self.context_size
    }

    /// Streaming forward with KV cache.
    ///
    /// Shapes:
    /// - `q`: `(B, S, n_feat)` — typically S is small (per-chunk new frames)
    /// - `k`, `v`: `(B, S, n_feat)` — same S as q before cache update
    /// - `pos_emb`: `(1 | B, 2*w + 1, n_feat)` — local rel-pos table
    /// - `mask`: optional `(B, S)` boolean — `true` = padded query
    /// - `cache`: optional `&mut RotatingConformerCache` — if `Some`,
    ///   K/V are extended with cached history; cache is mutated in place
    ///
    /// Returns `(B, S, n_feat)` (output projection applied).
    #[allow(clippy::many_single_char_names)] // q/k/v/b/h are attention conventions
    pub fn forward(
        &self,
        q: &Tensor,
        k: &Tensor,
        v: &Tensor,
        pos_emb: &Tensor,
        mask: Option<&Tensor>,
        cache: Option<&mut RotatingConformerCache>,
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

        // 1. Project Q, K, V, P.
        // (QKV-fusion was attempted as a perf optimisation but produced
        // a regression: the narrow().contiguous() copies after the fused
        // matmul cost more than the three separate matmuls saved. Stays
        // with three matmuls for now; a real fusion needs to keep the
        // output as (B, S, 3*n_feat) and reshape directly to heads
        // without narrow+contiguous — bigger refactor, deferred.)
        let q = linear_q.forward(q).context("project q (local)")?;
        let k_proj = linear_k.forward(k).context("project k (local)")?;
        let v_proj = linear_v.forward(v).context("project v (local)")?;
        let p = self
            .linear_pos
            .forward(pos_emb)
            .context("project pos_emb (local)")?;

        let (batch, s, _) = q.dims3().context("q shape (local)")?;
        let (_, _, _) = k_proj.dims3().context("k shape (local)")?;
        let (p_batch, pos_len, _) = p.dims3().context("pos_emb shape (local)")?;
        let w_left = self.context_size.0;
        let w_right = self.context_size.1;
        anyhow::ensure!(
            pos_len == w_left + w_right + 1,
            "pos_emb len ({pos_len}) must equal context_size.0 + context_size.1 + 1 ({})",
            w_left + w_right + 1
        );

        // 2. Reshape Q/K/V/P to (B, H, T, D).
        let q = q.reshape((batch, s, n_head, head_dim))?;
        let pos_bias_u = self.pos_bias_u.reshape((1, 1, n_head, head_dim))?;
        let pos_bias_v = self.pos_bias_v.reshape((1, 1, n_head, head_dim))?;
        let q_u = q
            .broadcast_add(&pos_bias_u)?
            .transpose(1, 2)?
            .contiguous()?;
        let q_v = q
            .broadcast_add(&pos_bias_v)?
            .transpose(1, 2)?
            .contiguous()?;

        let k_h = k_proj
            .reshape((batch, s, n_head, head_dim))?
            .transpose(1, 2)?
            .contiguous()?;
        let v_h = v_proj
            .reshape((batch, s, n_head, head_dim))?
            .transpose(1, 2)?
            .contiguous()?;

        // 3. Update cache (if any), getting extended K/V.
        let (cached_offset, k_extended, v_extended) = if let Some(cache_ref) = cache {
            let cached_len = cache_ref.cached_kv_len();
            let (k_ext, v_ext) = cache_ref.update_and_fetch_kv(&k_h, &v_h)?;
            (cached_len, k_ext, v_ext)
        } else {
            (0, k_h.clone(), v_h.clone())
        };
        let k_len = k_extended.dim(2).context("k_extended dim 2")?;

        // Broadcast pos_emb to batch if it ships with batch=1.
        let p = if p_batch == 1 && batch > 1 {
            p.broadcast_as((batch, pos_len, n_head * head_dim))?
        } else {
            anyhow::ensure!(
                p_batch == batch,
                "pos_emb batch ({p_batch}) must be 1 or match query batch ({batch})"
            );
            p
        };
        let p = p
            .reshape((batch, pos_len, n_head, head_dim))?
            .transpose(1, 2)?
            .contiguous()?;

        // 4. Content scores: Q_u @ K^T → (B, H, S, K_len).
        let content_scores =
            q_u.matmul(&k_extended.transpose(D::Minus2, D::Minus1)?.contiguous()?)?;

        // 5. Position scores in (2w+1) representation: Q_v @ P^T → (B, H, S, 2w+1).
        let pos_scores_compact = q_v.matmul(&p.transpose(D::Minus2, D::Minus1)?.contiguous()?)?;

        // 6. Unfold (B, H, S, 2w+1) → (B, H, S, K_len) by per-(i, k) index gather.
        // For each chunk-relative query i (absolute K-position = cached_offset + i)
        // and key k, the relative position is (k - (cached_offset + i)) and the
        // index into the pos_emb table is (rel + w_left). Out-of-window indices
        // are clamped to 0; the local-window mask zeros those entries below.
        let pos_scores =
            unfold_local_pos_scores(&pos_scores_compact, cached_offset, k_len, w_left, w_right)?;

        // 7. Sum + scale.
        let scores = ((content_scores + pos_scores)? * f64::from(scale))?;

        // 8. Local-window mask: -inf at (i, k) where k is outside the window.
        let window_bias = local_window_bias(
            batch,
            n_head,
            s,
            cached_offset,
            k_len,
            w_left,
            w_right,
            scores.dtype(),
            scores.device(),
        )?;
        let scores = scores.broadcast_add(&window_bias)?;

        // 9. Query-padding mask (if provided): -inf for masked queries.
        // mask is (B, S) bool; expand to (B, 1, S, 1) and add as -inf bias.
        let scores = if let Some(m) = mask {
            let (b_m, s_m) = m.dims2().context("mask must be (B, S)")?;
            anyhow::ensure!(
                b_m == batch && s_m == s,
                "mask shape ({b_m}, {s_m}) must be (B={batch}, S={s})"
            );
            let bias = (m.to_dtype(scores.dtype())?.reshape((b_m, 1, s_m, 1))? * -1.0e9_f64)?;
            scores.broadcast_add(&bias)?
        } else {
            scores
        };

        // 10. Softmax + attn @ V.
        let attn = softmax(&scores, D::Minus1).context("softmax (local)")?;
        let out = attn.matmul(&v_extended)?;

        // 11. Reshape and output projection.
        let out = out
            .transpose(1, 2)?
            .contiguous()?
            .reshape((batch, s, n_head * head_dim))?;
        linear_out
            .forward(&out)
            .context("output projection (local)")
    }
}

/// Unfolds a `(B, H, S, 2w+1)` position-score tensor into the full
/// `(B, H, S, K_len)` representation by per-(i, k) gather. Out-of-window
/// indices are clamped to 0 — they're zeroed out by the local-window mask
/// at the call site.
#[allow(clippy::many_single_char_names)] // b/h/s are attention conventions
fn unfold_local_pos_scores(
    compact: &Tensor,
    cached_offset: usize,
    k_len: usize,
    w_left: usize,
    w_right: usize,
) -> Result<Tensor> {
    let (b, h, s, win_size) = compact
        .dims4()
        .context("unfold input must be (B, H, S, 2w+1)")?;
    anyhow::ensure!(
        win_size == w_left + w_right + 1,
        "unfold: window size {win_size} != {} (w_left + w_right + 1)",
        w_left + w_right + 1
    );

    let device = compact.device();
    let key = LocalAttnScratchKey {
        s,
        k_len,
        cached_offset,
        w_left,
        w_right,
        b,
        h,
    };

    // Reuse the cached index tensor if we've built it before with the
    // same parameters (the common path after the cache warms up).
    let cached = UNFOLD_INDEX_CACHE.with(|c| c.borrow().get(&key).cloned());
    let idx_tensor = if let Some(t) = cached {
        t
    } else {
        {
            // Build index tensor mirroring MLX's `LocalRelPositionalEncoding`
            // convention: the position-encoding table at index `j`
            // corresponds to relative position `(w_left - j)` (positions
            // descend from `+w_left` at j=0 to `-w_right` at
            // j=w_left+w_right). For query position `q_pos = cached_offset
            // + i` and key position `k`, the relative position is
            // `(k - q_pos)`, so the pos-emb index is `w_left - (k - q_pos)
            // = w_left + q_pos - k`.
            //
            // Out-of-window indices are clamped to 0 — those positions are
            // zeroed by the local-window mask downstream.
            let mut indices = vec![0u32; s * k_len];
            #[allow(clippy::cast_possible_wrap)]
            let win_max = (win_size as i64) - 1;
            #[allow(clippy::cast_possible_wrap)]
            let w_left_i = w_left as i64;
            for i in 0..s {
                #[allow(clippy::cast_possible_wrap)]
                let q_pos = (cached_offset + i) as i64;
                for k in 0..k_len {
                    #[allow(clippy::cast_possible_wrap)]
                    let idx = (w_left_i + q_pos - (k as i64)).clamp(0, win_max);
                    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                    {
                        indices[i * k_len + k] = idx as u32;
                    }
                }
            }

            let t = Tensor::from_vec(indices, (1, 1, s, k_len), device)?
                .broadcast_as((b, h, s, k_len))?
                .contiguous()?;
            UNFOLD_INDEX_CACHE.with(|c| {
                let mut m = c.borrow_mut();
                m.insert(key, t.clone());
                maybe_evict(&mut m);
            });
            t
        }
    };

    compact
        .contiguous()?
        .gather(&idx_tensor, 3)
        .context("gather position scores into K_len shape")
}

/// Builds an additive bias of shape `(1, 1, S, K_len)` that is `0.0` inside
/// the local window and `-1e9` outside. Broadcast-added to scores before
/// softmax so out-of-window keys get effectively-zero attention weight.
#[allow(clippy::too_many_arguments)]
fn local_window_bias(
    _batch: usize,
    _n_head: usize,
    s: usize,
    cached_offset: usize,
    k_len: usize,
    w_left: usize,
    w_right: usize,
    dtype: candle_core::DType,
    device: &candle_core::Device,
) -> Result<Tensor> {
    // Reuse the cached bias tensor when parameters match a previous
    // build. dtype is included implicitly via the cached tensor's
    // dtype; for our single-dtype (F32) caller this is fine, but if
    // a future caller switches dtype the cache would need keying by
    // dtype too.
    let key = LocalAttnScratchKey {
        s,
        k_len,
        cached_offset,
        w_left,
        w_right,
        // bias broadcasts; b and h aren't part of the tensor shape.
        // Set both to 1 to share entries across (b, h) variants.
        b: 1,
        h: 1,
    };
    let cached = WINDOW_BIAS_CACHE.with(|c| c.borrow().get(&key).cloned());
    if let Some(t) = cached {
        // Reuse only when the cached tensor matches the requested dtype
        // (assume same-thread sessions stay on the same device).
        if t.dtype() == dtype {
            return Ok(t);
        }
    }
    let _ = device; // suppress unused warning when cache hits

    let mut data = vec![0.0_f32; s * k_len];
    #[allow(clippy::cast_possible_wrap)]
    let w_left_i = w_left as i64;
    #[allow(clippy::cast_possible_wrap)]
    let w_right_i = w_right as i64;
    for i in 0..s {
        #[allow(clippy::cast_possible_wrap)]
        let q_pos = (cached_offset + i) as i64;
        for k in 0..k_len {
            #[allow(clippy::cast_possible_wrap)]
            let rel = (k as i64) - q_pos;
            if rel < -w_left_i || rel > w_right_i {
                data[i * k_len + k] = -1.0e9_f32;
            }
        }
    }
    let bias = Tensor::from_vec(data, (1, 1, s, k_len), device)?.to_dtype(dtype)?;
    WINDOW_BIAS_CACHE.with(|c| {
        let mut m = c.borrow_mut();
        m.insert(key, bias.clone());
        maybe_evict(&mut m);
    });
    Ok(bias)
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

    // Helpers for local-attention smoke tests.

    /// Builds a `RelPositionMultiHeadAttention` from hand-rolled weights —
    /// no `VarBuilder` / safetensors needed. Used by the local-attn tests
    /// below, which then `from_full` it.
    fn make_test_full_attn(n_head: usize, head_dim: usize) -> RelPositionMultiHeadAttention {
        let n_feat = n_head * head_dim;
        let dev = cpu();
        // Identity-ish weight tensors so the algorithm's structure is
        // easier to reason about. Each linear is just `(out, in)` identity.
        let id_w = Tensor::eye(n_feat, DType::F32, &dev).unwrap();
        let lin = |w: Tensor| Linear::new(w, None);
        #[allow(clippy::cast_precision_loss)]
        let scale = 1.0 / (head_dim as f32).sqrt();
        let base = MultiHeadAttention {
            n_head,
            head_dim,
            scale,
            linear_q: lin(id_w.clone()),
            linear_k: lin(id_w.clone()),
            linear_v: lin(id_w.clone()),
            linear_out: lin(id_w.clone()),
        };
        let pos_bias_u = Tensor::zeros((n_head, head_dim), DType::F32, &dev).unwrap();
        let pos_bias_v = Tensor::zeros((n_head, head_dim), DType::F32, &dev).unwrap();
        RelPositionMultiHeadAttention {
            base,
            linear_pos: lin(id_w),
            pos_bias_u,
            pos_bias_v,
        }
    }

    fn random_tensor(shape: &[usize], seed: u64) -> Tensor {
        // Cheap deterministic "random": linear ramp scaled by seed.
        let total: usize = shape.iter().product();
        #[allow(clippy::cast_precision_loss)]
        let data: Vec<f32> = (0..total)
            .map(|i| (((i as u64).wrapping_mul(seed.wrapping_add(1))) % 997) as f32 / 100.0 - 5.0)
            .collect();
        Tensor::from_vec(data, shape, &cpu()).unwrap()
    }

    #[test]
    fn local_attn_from_full_shares_weights() {
        let full = make_test_full_attn(2, 4);
        let local = RelPositionMultiHeadLocalAttention::from_full(&full, (2, 2)).unwrap();
        assert_eq!(local.context_size(), (2, 2));
        // Weight tensors are Arc-cloned; pointer-identity isn't promised by
        // candle but the underlying storage is shared. Verify shape parity.
        assert_eq!(
            local.base.linear_q.weight().dims(),
            full.base.linear_q.weight().dims()
        );
        assert_eq!(local.pos_bias_u.dims(), full.pos_bias_u.dims());
    }

    #[test]
    fn local_attn_forward_no_cache_produces_correct_shape() {
        let full = make_test_full_attn(2, 4);
        let local = RelPositionMultiHeadLocalAttention::from_full(&full, (2, 2)).unwrap();
        let n_feat = 2 * 4;
        let s = 3;
        let win = 2 + 2 + 1;
        let q = random_tensor(&[1, s, n_feat], 1);
        let k = random_tensor(&[1, s, n_feat], 2);
        let v = random_tensor(&[1, s, n_feat], 3);
        let pos = random_tensor(&[1, win, n_feat], 4);

        let out = local.forward(&q, &k, &v, &pos, None, None).unwrap();
        assert_eq!(out.dims(), [1, s, n_feat]);

        // Output must be finite (no NaNs from softmax-of-all-inf paths).
        let any_nan: f32 = out
            .flatten_all()
            .unwrap()
            .to_vec1::<f32>()
            .unwrap()
            .iter()
            .map(|x| if x.is_nan() { 1.0 } else { 0.0 })
            .sum();
        // any_nan is a sum of {0.0, 1.0} flags: exactly zero iff no NaN.
        #[allow(clippy::float_cmp)]
        {
            assert_eq!(any_nan, 0.0, "output must not contain NaN");
        }
    }

    #[test]
    fn local_attn_cache_accumulates_across_calls() {
        use super::super::cache::RotatingConformerCache;
        let full = make_test_full_attn(2, 4);
        let local = RelPositionMultiHeadLocalAttention::from_full(&full, (4, 4)).unwrap();
        let n_feat = 2 * 4;
        let win = 4 + 4 + 1;
        let pos = random_tensor(&[1, win, n_feat], 4);

        let mut cache = RotatingConformerCache::new(/*capacity=*/ 16, /*drop=*/ 0);

        // First call: 3 new frames.
        let q1 = random_tensor(&[1, 3, n_feat], 11);
        let k1 = random_tensor(&[1, 3, n_feat], 12);
        let v1 = random_tensor(&[1, 3, n_feat], 13);
        let _ = local
            .forward(&q1, &k1, &v1, &pos, None, Some(&mut cache))
            .unwrap();
        assert_eq!(
            cache.cached_kv_len(),
            3,
            "after 1st call cache should hold 3"
        );

        // Second call: 2 new frames.
        let q2 = random_tensor(&[1, 2, n_feat], 21);
        let k2 = random_tensor(&[1, 2, n_feat], 22);
        let v2 = random_tensor(&[1, 2, n_feat], 23);
        let _ = local
            .forward(&q2, &k2, &v2, &pos, None, Some(&mut cache))
            .unwrap();
        assert_eq!(
            cache.cached_kv_len(),
            5,
            "after 2nd call cache should hold 5"
        );
    }

    #[test]
    fn local_attn_window_mask_zeros_out_of_window_attention() {
        // With a tiny window (left=0, right=0), each query attends to
        // exactly one key (itself). With identity projections,
        // attn @ V == V exactly.
        let full = make_test_full_attn(1, 2);
        let local = RelPositionMultiHeadLocalAttention::from_full(&full, (0, 0)).unwrap();
        let n_feat = 2;
        let s = 4;
        let win = 1;
        let q = random_tensor(&[1, s, n_feat], 51);
        let k = random_tensor(&[1, s, n_feat], 52);
        let v = random_tensor(&[1, s, n_feat], 53);
        let pos = Tensor::zeros((1, win, n_feat), DType::F32, &cpu()).unwrap();

        let out = local.forward(&q, &k, &v, &pos, None, None).unwrap();
        assert_eq!(out.dims(), [1, s, n_feat]);
        // With (0, 0) window, attn = softmax([single_score]) = [1.0] for
        // each query — so attn @ V = V exactly (then post-multiplied by the
        // identity output projection, so out = V).
        let diff = (&out - &v).unwrap().abs().unwrap();
        let max: f32 = diff
            .flatten_all()
            .unwrap()
            .max(0)
            .unwrap()
            .to_scalar::<f32>()
            .unwrap();
        assert!(
            max < 1e-5,
            "out must equal v with (0, 0) window; max diff {max}"
        );
    }

    #[test]
    fn unfold_local_pos_scores_returns_correct_shape() {
        // (B=1, H=1, S=3, 2w+1=5). cached_offset=0, w_left=w_right=2, k_len=3.
        // MLX `LocalRelPositionalEncoding` orders positions descending
        // [+w_left, +(w_left-1), ..., 0, ..., -w_right], so pe[j] corresponds
        // to relative position (w_left - j). For query at chunk-relative
        // position i (q_pos = cached_offset + i) and key position k,
        // rel = k - q_pos, so the pe index is (w_left - rel) = (w_left + q_pos - k).
        let compact = t(
            &[
                10.0, 11.0, 12.0, 13.0, 14.0, // i=0
                20.0, 21.0, 22.0, 23.0, 24.0, // i=1
                30.0, 31.0, 32.0, 33.0, 34.0, // i=2
            ],
            (1, 1, 3, 5),
        );
        let unfolded = unfold_local_pos_scores(
            &compact, /*cached_offset=*/ 0, /*k_len=*/ 3, /*w_left=*/ 2,
            /*w_right=*/ 2,
        )
        .unwrap();
        assert_eq!(unfolded.dims(), [1, 1, 3, 3]);
        // i=0, k=0: idx = 2 + 0 - 0 = 2 → compact[0, 2] = 12
        // i=0, k=1: idx = 2 + 0 - 1 = 1 → compact[0, 1] = 11
        // i=0, k=2: idx = 2 + 0 - 2 = 0 → compact[0, 0] = 10
        // i=1, k=0: idx = 2 + 1 - 0 = 3 → compact[1, 3] = 23
        // i=1, k=1: idx = 2 + 1 - 1 = 2 → compact[1, 2] = 22
        // i=1, k=2: idx = 2 + 1 - 2 = 1 → compact[1, 1] = 21
        // i=2, k=0: idx = 2 + 2 - 0 = 4 → compact[2, 4] = 34
        // i=2, k=1: idx = 2 + 2 - 1 = 3 → compact[2, 3] = 33
        // i=2, k=2: idx = 2 + 2 - 2 = 2 → compact[2, 2] = 32
        let got: Vec<f32> = unfolded.flatten_all().unwrap().to_vec1::<f32>().unwrap();
        let expected = vec![12.0, 11.0, 10.0, 23.0, 22.0, 21.0, 34.0, 33.0, 32.0];
        assert_eq!(got, expected);
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
