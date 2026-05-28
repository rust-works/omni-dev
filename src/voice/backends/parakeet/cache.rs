//! Per-layer KV + conv caches for streaming Parakeet inference.
//!
//! Mirrors `parakeet_mlx::cache::RotatingConformerCache` from
//! `newhoggy/parakeet-mlx@32b8034`. The MLX implementation uses a physical
//! ring buffer with in-place writes via `mx.roll`; candle's `Tensor` is
//! immutable, so this port uses a "logical FIFO" representation that
//! returns the same `(K_out, V_out)` for any input sequence as MLX would
//! while reallocating the cache tensor each call. The reallocation cost
//! is negligible relative to the surrounding encoder matmuls.
//!
//! ## Algorithm
//!
//! `update_and_fetch_kv(K, V)` where K and V are `(B, H, S, D)`:
//!
//! 1. Returns `K_out = cat([cached_keys, K], axis=2)` and similarly for V.
//!    `cached_keys` holds up to `capacity` entries from prior calls.
//! 2. Updates the cache by appending the first `S - cache_drop_size` entries
//!    of K/V (the non-speculative prefix) and trimming the combined buffer
//!    to at most `capacity` from the tail.
//!    `cache_drop_size = context_size.1 * depth` is the number of trailing
//!    entries we deliberately don't cache because they may be revised on
//!    the next call.
//!
//! `update_and_fetch_conv(x, padding)` where x is `(B, S, D)`:
//!
//! 1. Initializes the conv cache to `zeros((B, padding, D))` on first call.
//! 2. If `S > cache_drop_size`, updates the cache with the last
//!    `min(padding, S - cache_drop_size)` entries of x, sliding the existing
//!    cache if a partial update.
//! 3. Returns `cat([conv, x, zeros((B, padding, D))], axis=1)` — i.e. x with
//!    the cached prefix prepended and `padding` zeros suffix-appended. Total
//!    output shape is `(B, S + 2*padding, D)`. The downstream
//!    `ConvolutionModule` consumes this in place of doing its own symmetric
//!    `pad_time`.
//!
//! Channels-last throughout (matches MLX). The `ConvolutionModule` is
//! responsible for any channels-first transpose after the cache returns.

use anyhow::{Context, Result};
use candle_core::Tensor;

/// Per-layer rotating KV + conv cache. Mutable: each `update_and_fetch_*`
/// call mutates the internal cache state in place.
#[derive(Debug)]
pub struct RotatingConformerCache {
    /// Cached keys, shape `(B, H, len, D)` with `len <= capacity`. `None`
    /// until the first call.
    cached_keys: Option<Tensor>,
    /// Cached values, shape `(B, H, len, D)`. `None` until the first call.
    cached_values: Option<Tensor>,
    /// Conv module cache, shape `(B, padding, D)`. `None` until the first
    /// `update_and_fetch_conv` call.
    conv: Option<Tensor>,
    /// Total non-speculative KV entries ever cached (cumulative across calls,
    /// not modulo capacity). Tracked for MLX-parity diagnostics; the
    /// local-attention path does not use it.
    offset: usize,
    /// Maximum number of KV entries the cache retains. Equals
    /// `context_size.0` (the left-context window of local attention).
    capacity: usize,
    /// Number of trailing entries per call that are NOT cached (speculative
    /// tail that may be revised on the next chunk). Equals
    /// `context_size.1 * depth` per the MLX fork's wiring.
    cache_drop_size: usize,
}

impl RotatingConformerCache {
    /// Builds a fresh cache with the given capacity and drop size.
    /// Cache buffers are lazily allocated on first use.
    #[must_use]
    pub fn new(capacity: usize, cache_drop_size: usize) -> Self {
        Self {
            cached_keys: None,
            cached_values: None,
            conv: None,
            offset: 0,
            capacity,
            cache_drop_size,
        }
    }

    /// Total non-speculative KV entries cached so far. Used for parity with
    /// the MLX fork; the local-attn path does not consume it.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Capacity (left-context window length) the cache was constructed with.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of trailing entries per call excluded from caching.
    #[must_use]
    pub fn cache_drop_size(&self) -> usize {
        self.cache_drop_size
    }

    /// Returns `(K_out, V_out) = (cat([cached, K], 2), cat([cached, V], 2))`
    /// and updates the internal cache with the non-speculative prefix of K/V.
    ///
    /// Input K and V must have shape `(B, H, S, D)`. Output has shape
    /// `(B, H, cached_len + S, D)` where `cached_len <= capacity`.
    pub fn update_and_fetch_kv(
        &mut self,
        keys: &Tensor,
        values: &Tensor,
    ) -> Result<(Tensor, Tensor)> {
        let dims = keys
            .dims4()
            .context("update_and_fetch_kv: keys must be 4-D (B,H,S,D)")?;
        let (_, _, s, _) = dims;

        // Step 1: build outputs by prepending the existing cache (if any).
        let k_out = match &self.cached_keys {
            Some(c) => Tensor::cat(&[c, keys], 2).context("cat cached_keys + keys")?,
            None => keys.clone(),
        };
        let v_out = match &self.cached_values {
            Some(c) => Tensor::cat(&[c, values], 2).context("cat cached_values + values")?,
            None => values.clone(),
        };

        // Step 2: update cache with the non-speculative prefix of K/V.
        let to_cache = s.saturating_sub(self.cache_drop_size);
        if to_cache > 0 {
            let k_new = keys
                .narrow(2, 0, to_cache)
                .context("narrow keys to non-speculative prefix")?;
            let v_new = values
                .narrow(2, 0, to_cache)
                .context("narrow values to non-speculative prefix")?;

            let k_combined = match &self.cached_keys {
                Some(c) => Tensor::cat(&[c, &k_new], 2).context("cat cached_keys + k_new")?,
                None => k_new,
            };
            let v_combined = match &self.cached_values {
                Some(c) => Tensor::cat(&[c, &v_new], 2).context("cat cached_values + v_new")?,
                None => v_new,
            };

            // Trim from the front so the cache holds at most `capacity` entries.
            let combined_len = k_combined.dim(2).context("k_combined dim 2")?;
            let (k_trimmed, v_trimmed) = if combined_len > self.capacity {
                let start = combined_len - self.capacity;
                let kt = k_combined
                    .narrow(2, start, self.capacity)
                    .context("trim cached_keys to capacity")?;
                let vt = v_combined
                    .narrow(2, start, self.capacity)
                    .context("trim cached_values to capacity")?;
                (kt, vt)
            } else {
                (k_combined, v_combined)
            };

            // contiguous() so subsequent matmuls don't fight a non-standard layout
            self.cached_keys = Some(k_trimmed.contiguous().context("contiguous cached_keys")?);
            self.cached_values = Some(v_trimmed.contiguous().context("contiguous cached_values")?);
            self.offset += to_cache;
        }

        Ok((k_out, v_out))
    }

    /// Returns x with the cached prefix prepended and `padding` zeros
    /// suffix-appended. The downstream `ConvolutionModule` uses this in
    /// place of doing its own symmetric padding.
    ///
    /// Input x must have shape `(B, S, D)` (channels-last). Output shape is
    /// `(B, S + 2 * padding, D)`.
    ///
    /// If `padding == 0`, returns x unchanged.
    pub fn update_and_fetch_conv(&mut self, x: &Tensor, padding: usize) -> Result<Tensor> {
        if padding == 0 {
            return Ok(x.clone());
        }

        let (b, s, d) = x
            .dims3()
            .context("update_and_fetch_conv: x must be 3-D (B,S,D)")?;
        let dtype = x.dtype();
        let device = x.device();

        // Lazily allocate the conv cache as zeros on first call.
        if self.conv.is_none() {
            self.conv = Some(
                Tensor::zeros((b, padding, d), dtype, device)
                    .context("init conv cache to zeros")?,
            );
        }

        // Update the conv cache with the last `tokens_to_cache` entries of x.
        if s > self.cache_drop_size {
            let tokens_to_cache = padding.min(s - self.cache_drop_size);
            // MLX uses `x[:, S - tokens_to_cache : S, :]` — i.e. the last
            // `tokens_to_cache` entries of x. (Note: the cache_drop_size term
            // only gates whether to update at all; the slice itself is the
            // trailing window. This matches commit 32b8034 verbatim.)
            let cache_update = x
                .narrow(1, s - tokens_to_cache, tokens_to_cache)
                .context("narrow conv cache_update")?;

            if tokens_to_cache < padding {
                // Slide existing cache: drop the first `tokens_to_cache`
                // entries and append cache_update.
                let current = self.conv.as_ref().ok_or_else(|| {
                    anyhow::anyhow!("conv cache must be initialised by this point")
                })?;
                let kept = current
                    .narrow(1, tokens_to_cache, padding - tokens_to_cache)
                    .context("narrow existing conv cache for slide")?;
                self.conv = Some(
                    Tensor::cat(&[&kept, &cache_update], 1)
                        .context("cat slid conv cache + update")?
                        .contiguous()
                        .context("contiguous conv cache")?,
                );
            } else {
                self.conv = Some(
                    cache_update
                        .contiguous()
                        .context("contiguous conv cache (replace)")?,
                );
            }
        }

        // Build the output: cat([conv, x], 1) then suffix-pad with `padding` zeros.
        let conv_ref = self
            .conv
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("conv cache must be initialised by this point"))?;
        let prefixed = Tensor::cat(&[conv_ref, x], 1).context("cat conv cache + x")?;
        let zero_suffix =
            Tensor::zeros((b, padding, d), dtype, device).context("alloc zero suffix")?;
        let result =
            Tensor::cat(&[&prefixed, &zero_suffix], 1).context("cat prefixed + zero suffix")?;

        Ok(result)
    }

    /// Returns the current cached KV length (0 before the first call).
    /// Used by the local-attention path to size attention masks.
    #[must_use]
    pub fn cached_kv_len(&self) -> usize {
        self.cached_keys
            .as_ref()
            .and_then(|t| t.dims4().ok())
            .map_or(0, |(_, _, len, _)| len)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use candle_core::{DType, Device};

    // Helpers for building test tensors.
    fn ones_kv(b: usize, h: usize, s: usize, d: usize, scale: f32) -> Tensor {
        let dev = Device::Cpu;
        let total = b * h * s * d;
        let data: Vec<f32> = (0..total)
            .map(|i| (i as f32).mul_add(0.001, scale))
            .collect();
        Tensor::from_vec(data, (b, h, s, d), &dev).unwrap()
    }

    fn linear_chw(b: usize, s: usize, d: usize) -> Tensor {
        let dev = Device::Cpu;
        let total = b * s * d;
        let data: Vec<f32> = (0..total).map(|i| i as f32).collect();
        Tensor::from_vec(data, (b, s, d), &dev).unwrap()
    }

    #[test]
    fn kv_first_call_returns_just_new_and_caches_prefix() {
        let mut c = RotatingConformerCache::new(4, 1);
        let k = ones_kv(1, 2, 3, 2, 0.0); // S=3, drop=1 → cache 2
        let v = ones_kv(1, 2, 3, 2, 10.0);

        let (k_out, v_out) = c.update_and_fetch_kv(&k, &v).unwrap();
        assert_eq!(k_out.dims(), [1, 2, 3, 2], "first call returns just K");
        assert_eq!(v_out.dims(), [1, 2, 3, 2]);
        assert_eq!(c.cached_kv_len(), 2, "cached non-speculative prefix");
        assert_eq!(c.offset(), 2);
    }

    #[test]
    fn kv_second_call_prepends_history() {
        let mut c = RotatingConformerCache::new(8, 1);
        let k1 = ones_kv(1, 2, 3, 2, 0.0);
        let v1 = ones_kv(1, 2, 3, 2, 10.0);
        c.update_and_fetch_kv(&k1, &v1).unwrap(); // caches 2

        let k2 = ones_kv(1, 2, 3, 2, 100.0);
        let v2 = ones_kv(1, 2, 3, 2, 110.0);
        let (k_out, v_out) = c.update_and_fetch_kv(&k2, &v2).unwrap();

        assert_eq!(k_out.dims(), [1, 2, 5, 2], "history (2) + new (3) = 5");
        assert_eq!(v_out.dims(), [1, 2, 5, 2]);
        assert_eq!(c.cached_kv_len(), 4, "cached 2 + 2 more = 4");
        assert_eq!(c.offset(), 4);
    }

    #[test]
    fn kv_capacity_caps_cached_length() {
        let mut c = RotatingConformerCache::new(3, 0); // small capacity
        let k1 = ones_kv(1, 1, 4, 2, 0.0); // S=4, drop=0 → cache 4 (but capacity=3)
        let v1 = ones_kv(1, 1, 4, 2, 10.0);
        c.update_and_fetch_kv(&k1, &v1).unwrap();
        assert_eq!(c.cached_kv_len(), 3, "trimmed to capacity");

        // Second call: history (3) + new (2) = 5 returned, cache stays at 3
        let k2 = ones_kv(1, 1, 2, 2, 100.0);
        let v2 = ones_kv(1, 1, 2, 2, 110.0);
        let (k_out, _) = c.update_and_fetch_kv(&k2, &v2).unwrap();
        assert_eq!(k_out.dims(), [1, 1, 5, 2]);
        assert_eq!(c.cached_kv_len(), 3);
    }

    #[test]
    fn kv_drop_size_excludes_speculative_tail() {
        let mut c = RotatingConformerCache::new(10, 2); // drop last 2 per call
        let k = ones_kv(1, 1, 5, 2, 0.0); // S=5, drop=2 → cache 3
        let v = ones_kv(1, 1, 5, 2, 10.0);
        c.update_and_fetch_kv(&k, &v).unwrap();
        assert_eq!(c.cached_kv_len(), 3, "S - drop = 5 - 2 = 3");
        assert_eq!(c.offset(), 3);
    }

    #[test]
    fn kv_cache_contents_are_the_first_to_cache_entries() {
        // Verify the cached portion is the LEADING `S - drop` entries of the
        // new K/V, not the tail.
        let mut c = RotatingConformerCache::new(10, 1);
        let k = ones_kv(1, 1, 4, 2, 0.0); // S=4, drop=1 → cache first 3
        let v = ones_kv(1, 1, 4, 2, 10.0);
        c.update_and_fetch_kv(&k, &v).unwrap();

        let cached = c.cached_keys.as_ref().unwrap();
        let expected = k.narrow(2, 0, 3).unwrap().contiguous().unwrap();
        let diff = (cached - &expected).unwrap().abs().unwrap();
        let max = diff.max_keepdim(2).unwrap().max_keepdim(3).unwrap();
        let max_val: f32 = max.flatten_all().unwrap().to_vec1::<f32>().unwrap()[0];
        assert!(max_val < 1e-6, "cached_keys must equal keys[:, :, :3, :]");
    }

    #[test]
    fn kv_zero_drop_caches_everything() {
        let mut c = RotatingConformerCache::new(10, 0);
        let k = ones_kv(1, 1, 3, 2, 0.0);
        let v = ones_kv(1, 1, 3, 2, 10.0);
        c.update_and_fetch_kv(&k, &v).unwrap();
        assert_eq!(c.cached_kv_len(), 3);
        assert_eq!(c.offset(), 3);
    }

    #[test]
    fn conv_padding_zero_is_passthrough() {
        let mut c = RotatingConformerCache::new(10, 0);
        let x = linear_chw(1, 5, 3);
        let result = c.update_and_fetch_conv(&x, 0).unwrap();
        assert_eq!(result.dims(), [1, 5, 3]);
        assert!(c.conv.is_none(), "conv cache untouched when padding=0");
    }

    #[test]
    fn conv_first_call_prefix_is_x_tail_not_zeros() {
        // Verified against MLX behaviour: update_and_fetch_conv overwrites
        // self.conv *before* building the result, so on the first call the
        // prefix is x[S-padding:S] (x's own tail), NOT zeros from the
        // freshly-initialised cache. Confirmed by running the MLX fork
        // with the same inputs and comparing outputs.
        let mut c = RotatingConformerCache::new(10, 0);
        let x = linear_chw(1, 5, 3);
        let padding = 2;
        let result = c.update_and_fetch_conv(&x, padding).unwrap();
        assert_eq!(
            result.dims(),
            [1, 5 + 2 * padding, 3],
            "B, S + 2*padding, D"
        );

        // Prefix (first `padding` entries) is x[3:5] — x's tail — because
        // self.conv was overwritten to that tail before the result was built.
        let prefix = result.narrow(1, 0, padding).unwrap().contiguous().unwrap();
        let expected_prefix = x.narrow(1, 3, 2).unwrap().contiguous().unwrap();
        let diff = (&prefix - &expected_prefix).unwrap().abs().unwrap();
        let max_val: f32 = diff
            .flatten_all()
            .unwrap()
            .max(0)
            .unwrap()
            .to_scalar::<f32>()
            .unwrap();
        assert!(max_val < 1e-6, "prefix on 1st call must equal x[3:5]");

        // Middle is x verbatim.
        let middle = result.narrow(1, padding, 5).unwrap().contiguous().unwrap();
        let expected_middle = x.contiguous().unwrap();
        let diff = (&middle - &expected_middle).unwrap().abs().unwrap();
        let max_val: f32 = diff
            .flatten_all()
            .unwrap()
            .max(0)
            .unwrap()
            .to_scalar::<f32>()
            .unwrap();
        assert!(max_val < 1e-6, "middle must be x");

        // Suffix is zeros.
        let suffix = result
            .narrow(1, padding + 5, padding)
            .unwrap()
            .contiguous()
            .unwrap();
        let abs_sum: f32 = suffix
            .abs()
            .unwrap()
            .sum_all()
            .unwrap()
            .to_scalar::<f32>()
            .unwrap();
        assert!(abs_sum < 1e-6, "suffix must be zeros");
    }

    #[test]
    fn conv_second_call_prefix_is_x2_tail_overwriting_x1_tail() {
        // The cache is overwritten on every call (when tokens_to_cache == padding).
        // After two calls, the cache holds x2's tail, and the second call's
        // result prefix is x2's tail.
        let mut c = RotatingConformerCache::new(10, 0);
        let padding = 2;

        let x1 = linear_chw(1, 5, 3);
        let _ = c.update_and_fetch_conv(&x1, padding).unwrap();

        // After x1: cache = x1[:, 3:5, :].
        let cached_after_x1 = c.conv.as_ref().unwrap().clone();
        let expected_x1_tail = x1.narrow(1, 3, 2).unwrap().contiguous().unwrap();
        let diff = (&cached_after_x1 - &expected_x1_tail)
            .unwrap()
            .abs()
            .unwrap();
        let max_val: f32 = diff
            .flatten_all()
            .unwrap()
            .max(0)
            .unwrap()
            .to_scalar::<f32>()
            .unwrap();
        assert!(max_val < 1e-6, "cache after x1 must equal x1[:, 3:5, :]");

        // Second call with x2 of length 4: tokens_to_cache = 2 == padding,
        // so cache is OVERWRITTEN to x2[:, 2:4, :]. Result prefix is x2's tail.
        let x2 = linear_chw(1, 4, 3);
        let result = c.update_and_fetch_conv(&x2, padding).unwrap();
        assert_eq!(result.dims(), [1, 4 + 2 * padding, 3]);
        let prefix = result.narrow(1, 0, padding).unwrap().contiguous().unwrap();
        let expected_x2_tail = x2.narrow(1, 2, 2).unwrap().contiguous().unwrap();
        let diff = (&prefix - &expected_x2_tail).unwrap().abs().unwrap();
        let max_val: f32 = diff
            .flatten_all()
            .unwrap()
            .max(0)
            .unwrap()
            .to_scalar::<f32>()
            .unwrap();
        assert!(
            max_val < 1e-6,
            "prefix on 2nd call must equal x2[:, 2:4, :]"
        );
    }

    #[test]
    fn conv_small_x_slides_cache_partially() {
        // When `tokens_to_cache < padding`, the existing cache slides by
        // `tokens_to_cache` and the new tail is appended.
        let mut c = RotatingConformerCache::new(10, 0);
        let padding = 3;

        // First call x1 length 5 → tokens_to_cache = min(3, 5) = 3 = padding;
        // cache is OVERWRITTEN to x1[2:5].
        let x1 = linear_chw(1, 5, 2);
        let _ = c.update_and_fetch_conv(&x1, padding).unwrap();
        let cached_after_x1 = c.conv.as_ref().unwrap().clone();

        // Second call x2 length 1 → tokens_to_cache = min(3, 1) = 1 < padding.
        // Cache slides: kept = cached_after_x1[1:3] (length 2), then append x2.
        let x2 = linear_chw(1, 1, 2);
        let _ = c.update_and_fetch_conv(&x2, padding).unwrap();
        let cached_after_x2 = c.conv.as_ref().unwrap();
        assert_eq!(cached_after_x2.dims(), [1, padding, 2]);

        let kept = cached_after_x1
            .narrow(1, 1, padding - 1)
            .unwrap()
            .contiguous()
            .unwrap();
        let expected_after_x2 = Tensor::cat(&[&kept, &x2], 1).unwrap().contiguous().unwrap();
        let diff = (cached_after_x2 - &expected_after_x2)
            .unwrap()
            .abs()
            .unwrap();
        let max_val: f32 = diff
            .flatten_all()
            .unwrap()
            .max(0)
            .unwrap()
            .to_scalar::<f32>()
            .unwrap();
        assert!(max_val < 1e-6, "cache after x2 must be slid x1 tail + x2");
    }

    #[test]
    fn conv_does_not_update_when_s_le_cache_drop_size() {
        let mut c = RotatingConformerCache::new(10, 5); // drop=5
        let padding = 2;

        let x = linear_chw(1, 3, 2); // S=3 ≤ drop=5
        let _ = c.update_and_fetch_conv(&x, padding).unwrap();

        // Cache should still be initial zeros (no update because S <= drop).
        let cached = c.conv.as_ref().unwrap();
        let abs_sum: f32 = cached
            .abs()
            .unwrap()
            .sum_all()
            .unwrap()
            .to_scalar::<f32>()
            .unwrap();
        assert!(
            abs_sum < 1e-6,
            "conv cache must remain zeros when S <= cache_drop_size"
        );
    }

    #[test]
    fn new_initialises_to_empty() {
        let c = RotatingConformerCache::new(16, 4);
        assert_eq!(c.capacity(), 16);
        assert_eq!(c.cache_drop_size(), 4);
        assert_eq!(c.offset(), 0);
        assert_eq!(c.cached_kv_len(), 0);
    }

    // Suppress unused-imports lint when dtype isn't pulled into a specific
    // test signature.
    #[allow(dead_code)]
    fn _dtype_marker() -> DType {
        DType::F32
    }
}
