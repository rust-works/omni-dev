//! FastConformer encoder — the front half of the Parakeet model.
//!
//! Mirrors `senstella/parakeet-mlx::parakeet_mlx/conformer.py::Conformer`
//! for the `self_attention_model == "rel_pos"` /
//! `subsampling == "dw_striding"` configuration that ships with
//! Parakeet 0.6B v2.
//!
//! Three sub-pieces:
//!
//! - [`RelPositionalEncoding`] — precomputed sinusoidal positional
//!   embedding table sized `(2 * max_len - 1, d_model)`. Forward
//!   returns `(x_scaled, pos_emb)` where `pos_emb` is the slice of the
//!   table centred on the input length.
//!
//! - [`DwStridingSubsampling`] — stack of 2-D convolutions (one full
//!   conv + N-1 depthwise/pointwise pairs) that reduces the time axis
//!   by `subsampling_factor` (8 for Parakeet) and projects the
//!   resulting flattened (channel, freq) into `d_model`.
//!
//! - [`FastConformerEncoder`] — composition: subsample → pos-enc →
//!   24 × [`ConformerBlock`] → output.
//!
//! Layout differences from the MLX reference:
//!
//! - MLX uses NHWC for `nn.Conv2d`; the upstream code transposes
//!   `(B, 1, T, F) -> (B, T, F, 1)` before the conv stack and back
//!   afterwards. candle is NCHW natively, so this port skips the dance
//!   and runs the convolutions on `(B, C, T, F)` directly. The
//!   converter has already permuted Conv2d weights from MLX
//!   `(out, kH, kW, in)` to PyTorch `(out, in, kH, kW)`.

use anyhow::{Context, Result};
use candle_core::{Module, Tensor};
use candle_nn::{Conv2d, Conv2dConfig, Linear, VarBuilder};

use super::conformer_block::ConformerBlock;

/// Hyperparameters for the FastConformer encoder. Concrete values for
/// Parakeet 0.6B v2 live in [`PARAKEET_0_6B_V2`].
#[derive(Debug, Clone)]
pub struct EncoderConfig {
    /// Input feature dimension — number of mel bins. Parakeet: 80.
    pub feat_in: usize,
    /// Number of Conformer layers. Parakeet: 24.
    pub n_layers: usize,
    /// Model dimension (channels for each layer). Parakeet: 1024.
    pub d_model: usize,
    /// Number of attention heads. Parakeet: 8.
    pub n_heads: usize,
    /// Feed-forward expansion factor — hidden size of each FF sub-block
    /// is `ff_expansion_factor * d_model`. Parakeet: 4.
    pub ff_expansion_factor: usize,
    /// Time-axis subsampling factor. Must be a power of two. Parakeet: 8.
    pub subsampling_factor: usize,
    /// Channels used inside the subsampling conv stack. Parakeet: 256.
    pub subsampling_conv_channels: usize,
    /// Depthwise conv kernel size in the conv module. Parakeet: 9.
    pub conv_kernel_size: usize,
    /// Maximum precomputed positional-encoding length. Parakeet: 5000.
    pub pos_emb_max_len: usize,
    /// Whether linear and conv layers carry bias terms. Parakeet: true.
    pub use_bias: bool,
    /// Whether to scale input by sqrt(d_model) inside the positional
    /// encoding. Parakeet: false.
    pub xscaling: bool,
}

/// Concrete encoder config for Parakeet-TDT-0.6B-v2.
///
/// Kept as a documented reference and as a fallback when
/// [`EncoderConfig::from_config_json`] can't read the installed file.
/// The runtime load path in `mod.rs` reads `config.json` from the
/// install dir rather than depending on this const — see
/// [`EncoderConfig::from_config_json`] for the loader.
pub const PARAKEET_0_6B_V2: EncoderConfig = EncoderConfig {
    feat_in: 128,
    n_layers: 24,
    d_model: 1024,
    n_heads: 8,
    ff_expansion_factor: 4,
    subsampling_factor: 8,
    subsampling_conv_channels: 256,
    conv_kernel_size: 9,
    pos_emb_max_len: 5000,
    use_bias: false,
    xscaling: false,
};

impl EncoderConfig {
    /// Loads encoder hyperparameters from a Parakeet `config.json` on
    /// disk. Fields are read from `encoder.{feat_in, n_layers, d_model,
    /// n_heads, ff_expansion_factor, subsampling_factor,
    /// subsampling_conv_channels, conv_kernel_size}`.
    ///
    /// Fields not surfaced in the upstream config schema use defaults:
    /// `use_bias = false` (all parakeet-tdt variants are unbiased; the
    /// alternative is to introspect the safetensors for the presence
    /// of `feed_forward1.linear1.bias`, deferred for now);
    /// `pos_emb_max_len` and `xscaling` inherit from
    /// [`PARAKEET_0_6B_V2`].
    ///
    /// Loading from disk rather than hardcoding the v2 const prevents
    /// the bug class where a v1-trained set of constants silently
    /// survives a v2 weight swap (the exact failure mode that produced
    /// the `feat_in: 80 / use_bias: true / N_MELS: 80` mismatch caught
    /// in PR review).
    pub fn from_config_json(path: &std::path::Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let cfg: serde_json::Value =
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        let enc = &cfg["encoder"];
        anyhow::ensure!(
            enc.is_object(),
            "{}: missing top-level `encoder` object",
            path.display()
        );

        let read_usize = |field: &str| -> Result<usize> {
            enc.get(field)
                .and_then(serde_json::Value::as_u64)
                .map(|n| n as usize)
                .ok_or_else(|| {
                    anyhow::anyhow!("{}: missing or non-integer encoder.{field}", path.display())
                })
        };

        Ok(Self {
            feat_in: read_usize("feat_in")?,
            n_layers: read_usize("n_layers")?,
            d_model: read_usize("d_model")?,
            n_heads: read_usize("n_heads")?,
            ff_expansion_factor: read_usize("ff_expansion_factor")?,
            subsampling_factor: read_usize("subsampling_factor")?,
            subsampling_conv_channels: read_usize("subsampling_conv_channels")?,
            conv_kernel_size: read_usize("conv_kernel_size")?,
            pos_emb_max_len: PARAKEET_0_6B_V2.pos_emb_max_len,
            use_bias: false,
            xscaling: PARAKEET_0_6B_V2.xscaling,
        })
    }
}

/// Sinusoidal relative positional encoding (Transformer-XL style).
///
/// Stores a `(1, 2 * max_len - 1, d_model)` table covering offsets
/// `[-max_len+1, ..., max_len-1]`. Each row is the standard
/// `[sin(pos / 10000^(2i/d)), cos(pos / 10000^(2i/d))]` interleave.
///
/// Forward returns `(x * scale, pos_emb)` where `pos_emb` is the table
/// slice centred on the input length so that index `0` of the slice
/// corresponds to offset `-(T - 1)`.
pub struct RelPositionalEncoding {
    pe: Tensor,
    d_model: usize,
    scale: f64,
}

impl RelPositionalEncoding {
    /// Precomputes the positional encoding table on `device`. `d_model`
    /// must be even.
    pub fn new(
        d_model: usize,
        max_len: usize,
        scale_input: bool,
        device: &candle_core::Device,
    ) -> Result<Self> {
        anyhow::ensure!(
            d_model % 2 == 0,
            "RelPositionalEncoding: d_model ({d_model}) must be even"
        );
        anyhow::ensure!(max_len > 0, "RelPositionalEncoding: max_len must be > 0");

        let n_rows = 2 * max_len - 1;
        let half = d_model / 2;
        let mut data = vec![0.0_f32; n_rows * d_model];

        let log10000 = 10_000.0_f32.ln();
        for row in 0..n_rows {
            // position counts down from (max_len - 1) to -(max_len - 1).
            #[allow(clippy::cast_precision_loss)]
            #[allow(clippy::cast_possible_wrap)]
            #[allow(clippy::cast_possible_truncation)]
            let pos = ((max_len as i64 - 1) - row as i64) as f32;
            for i in 0..half {
                #[allow(clippy::cast_precision_loss)]
                let two_i = (2 * i) as f32;
                let div = (-two_i * log10000 / d_model as f32).exp();
                let theta = pos * div;
                data[row * d_model + 2 * i] = theta.sin();
                data[row * d_model + 2 * i + 1] = theta.cos();
            }
        }
        let pe = Tensor::from_vec(data, (1, n_rows, d_model), device)?;
        #[allow(clippy::cast_precision_loss)]
        let scale = if scale_input {
            f64::from((d_model as f32).sqrt())
        } else {
            1.0
        };
        Ok(Self { pe, d_model, scale })
    }

    /// Forward: returns `(x * scale, pos_emb)`. `pos_emb` is a
    /// `(1, 2 * input_len - 1, d_model)` slice of the precomputed table
    /// centred so that `pos_emb[:, input_len - 1, :]` is the zero-offset
    /// row.
    pub fn forward(&self, x: &Tensor) -> Result<(Tensor, Tensor)> {
        let (_, input_len, _) = x.dims3().context("pos_enc input must be (B, T, D)")?;
        let buffer_len = self.pe.dim(1)?;
        let max_len = buffer_len.div_ceil(2);
        anyhow::ensure!(
            input_len <= max_len,
            "input length ({input_len}) exceeds positional-encoding capacity (max_len = {max_len})",
        );
        let start = buffer_len / 2 + 1 - input_len;
        let pos_emb = self.pe.narrow(1, start, 2 * input_len - 1)?;
        let x_scaled = (x * self.scale)?;
        Ok((x_scaled, pos_emb))
    }

    /// Returns `d_model` (so callers can sanity-check at construction).
    #[must_use]
    pub fn d_model(&self) -> usize {
        self.d_model
    }
}

/// Local-window sinusoidal positional encoding for streaming attention.
///
/// Mirrors `parakeet_mlx::attention::LocalRelPositionalEncoding`. Holds a
/// precomputed table of shape `(1, left + right + 1, d_model)` covering
/// relative positions `[+left, +left-1, ..., 0, ..., -right]` — in
/// descending order so `pe[j]` corresponds to relative offset
/// `(left - j)`. Used by [`RelPositionMultiHeadLocalAttention`] in place
/// of the standard `(1, 2 * max_len - 1, d_model)` table from
/// [`RelPositionalEncoding`].
///
/// The streaming wrapper passes the entire table to attention each call;
/// no offset parameter is consumed (unlike the full-attention encoding,
/// the local window is centred on each query intrinsically).
pub struct LocalRelPositionalEncoding {
    pe: Tensor,
    d_model: usize,
    scale: f64,
    left_context: usize,
    right_context: usize,
}

impl LocalRelPositionalEncoding {
    /// Precomputes the positional encoding table on `device`. `d_model`
    /// must be even. `context_size = (left, right)` defines the local
    /// attention window — table length is `left + right + 1`.
    pub fn new(
        d_model: usize,
        context_size: (usize, usize),
        scale_input: bool,
        device: &candle_core::Device,
    ) -> Result<Self> {
        anyhow::ensure!(
            d_model % 2 == 0,
            "LocalRelPositionalEncoding: d_model ({d_model}) must be even"
        );
        let (left_context, right_context) = context_size;
        let n_rows = left_context + right_context + 1;
        let half = d_model / 2;
        let mut data = vec![0.0_f32; n_rows * d_model];

        let log10000 = 10_000.0_f32.ln();
        for row in 0..n_rows {
            // positions descend: pe[0] @ +left, pe[n-1] @ -right.
            #[allow(clippy::cast_possible_wrap, clippy::cast_precision_loss)]
            let pos = (left_context as i64 - row as i64) as f32;
            for i in 0..half {
                #[allow(clippy::cast_precision_loss)]
                let two_i = (2 * i) as f32;
                let div = (-two_i * log10000 / d_model as f32).exp();
                let theta = pos * div;
                data[row * d_model + 2 * i] = theta.sin();
                data[row * d_model + 2 * i + 1] = theta.cos();
            }
        }
        let pe = Tensor::from_vec(data, (1, n_rows, d_model), device)?;
        #[allow(clippy::cast_precision_loss)]
        let scale = if scale_input {
            f64::from((d_model as f32).sqrt())
        } else {
            1.0
        };
        Ok(Self {
            pe,
            d_model,
            scale,
            left_context,
            right_context,
        })
    }

    /// Forward: returns `(x * scale, pos_emb)`. `pos_emb` is the full
    /// precomputed table `(1, left + right + 1, d_model)` — every call
    /// returns the same slice (no offset-dependence).
    pub fn forward(&self, x: &Tensor) -> Result<(Tensor, Tensor)> {
        let _ = x.dims3().context("local pos_enc input must be (B, T, D)")?;
        let x_scaled = (x * self.scale)?;
        Ok((x_scaled, self.pe.clone()))
    }

    /// Returns `(left_context, right_context)` — for callers that need
    /// to size attention masks.
    #[must_use]
    pub fn context_size(&self) -> (usize, usize) {
        (self.left_context, self.right_context)
    }

    /// Returns `d_model` (so callers can sanity-check at construction).
    #[must_use]
    pub fn d_model(&self) -> usize {
        self.d_model
    }
}

/// Depthwise-striding subsampling pre-encoder.
///
/// Reduces the time axis by `subsampling_factor` (power of two) via
/// a stack of strided 2-D convs interleaved with ReLU, then projects
/// the resulting `(d_model_conv, freq')` slice at each time step to
/// `d_model` via a final linear layer.
///
/// Conv layout (NCHW throughout):
/// - First step: `Conv2d(1 -> C, k=3, s=2, p=1)` + ReLU
/// - Each of the remaining `log2(factor) - 1` steps:
///   `DepthwiseConv2d(C -> C, k=3, s=2, p=1, groups=C)` +
///   `PointwiseConv2d(C -> C, k=1, s=1, p=0)` + ReLU
/// - Output: `Linear(C * freq_final -> d_model)`
///
/// For Parakeet 0.6B v2 (`factor=8`, `feat_in=80`, `C=256`), `freq_final = 10`
/// and the output projection is `Linear(2560 -> 1024)`.
pub struct DwStridingSubsampling {
    convs: Vec<Conv2d>,
    out: Linear,
    n_steps: usize,
    freq_final: usize,
}

impl Clone for DwStridingSubsampling {
    fn clone(&self) -> Self {
        // Conv2d and Linear hold Arc-backed Tensors; clones are cheap.
        Self {
            convs: self.convs.clone(),
            out: self.out.clone(),
            n_steps: self.n_steps,
            freq_final: self.freq_final,
        }
    }
}

impl DwStridingSubsampling {
    /// Loads the conv stack and output projection from `vb`.
    pub fn load(vb: VarBuilder, cfg: &EncoderConfig) -> Result<Self> {
        anyhow::ensure!(
            cfg.subsampling_factor > 0 && cfg.subsampling_factor.is_power_of_two(),
            "subsampling_factor ({}) must be a positive power of two",
            cfg.subsampling_factor
        );
        let n_steps = (cfg.subsampling_factor as f64).log2().round() as usize;
        let stride = 2;
        let kernel = 3;
        let padding = 1;

        // Track freq dim through the conv stack.
        let mut freq = cfg.feat_in;
        for _ in 0..n_steps {
            freq = (freq + 2 * padding - kernel) / stride + 1;
        }
        anyhow::ensure!(
            freq > 0,
            "subsampling reduced freq dim to {freq} (negative or zero) — check feat_in and subsampling_factor"
        );

        // Conv stack. MLX stores conv layers in a flat list under
        // `conv.<i>` for both Conv2d and the embedded ReLUs; ReLUs have
        // no params, so safetensors only carries the convs. The
        // converter preserves the MLX numbering — we load by the same
        // indices, skipping the activation slots.
        let mut convs = Vec::new();
        let conv_vb = vb.pp("conv");

        // Step 0: full Conv2d (in=1 -> C).
        convs.push(
            conv2d(
                conv_vb.pp("0"),
                1,
                cfg.subsampling_conv_channels,
                kernel,
                stride,
                padding,
                1,
            )
            .context("load subsampling conv 0")?,
        );

        // Remaining steps: depthwise + pointwise, MLX indices are
        //   2 + 3*i      depthwise
        //   2 + 3*i + 1  pointwise
        //   2 + 3*i + 2  ReLU (no weights)
        // for i in 0..(n_steps - 1).
        for i in 0..n_steps.saturating_sub(1) {
            let base = 2 + 3 * i;
            convs.push(
                conv2d(
                    conv_vb.pp(format!("{base}")),
                    cfg.subsampling_conv_channels,
                    cfg.subsampling_conv_channels,
                    kernel,
                    stride,
                    padding,
                    cfg.subsampling_conv_channels,
                )
                .with_context(|| format!("load subsampling depthwise conv {base}"))?,
            );
            convs.push(
                conv2d(
                    conv_vb.pp(format!("{}", base + 1)),
                    cfg.subsampling_conv_channels,
                    cfg.subsampling_conv_channels,
                    1,
                    1,
                    0,
                    1,
                )
                .with_context(|| format!("load subsampling pointwise conv {}", base + 1))?,
            );
        }

        let out = linear(
            vb.pp("out"),
            cfg.subsampling_conv_channels * freq,
            cfg.d_model,
            cfg.use_bias,
        )
        .context("load subsampling output projection")?;

        Ok(Self {
            convs,
            out,
            n_steps,
            freq_final: freq,
        })
    }

    /// Forward: `(B, T, feat_in)` -> `(B, T', d_model)` where
    /// `T' = floor((T + 2 - 3) / 2 + 1) ^ n_steps` (≈ T / 2^n_steps).
    pub fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let (batch, time, feat) = x.dims3().context("subsampling input must be (B, T, F)")?;
        // (B, T, F) -> (B, 1, T, F) (NCHW with C=1).
        let mut x = x.reshape((batch, 1, time, feat))?.contiguous()?;

        // Each "step" after the first is depthwise + pointwise; ReLU
        // sits between steps. We apply ReLU after each step boundary so
        // the activation sequence matches MLX's flat-list semantics
        // (Conv2d, ReLU, [Depthwise, Pointwise, ReLU] *).
        let mut idx = 0;
        // Step 0.
        x = self.convs[idx].forward(&x).context("subsample step 0")?;
        idx += 1;
        x = x.relu()?;

        for _ in 0..self.n_steps.saturating_sub(1) {
            // Depthwise.
            x = self.convs[idx]
                .forward(&x)
                .with_context(|| format!("subsample depthwise step (conv idx {idx})"))?;
            idx += 1;
            // Pointwise.
            x = self.convs[idx]
                .forward(&x)
                .with_context(|| format!("subsample pointwise step (conv idx {idx})"))?;
            idx += 1;
            x = x.relu()?;
        }

        // x is now (B, C, T', F'). Permute to (B, T', C, F') and flatten
        // the last two dims so the output projection sees per-time-step
        // (C * F')-vectors.
        let (batch_out, channels, t_prime, f_prime) = x.dims4().context("subsample output dims")?;
        debug_assert_eq!(f_prime, self.freq_final);
        let x = x.permute((0, 2, 1, 3))?.contiguous()?;
        let x = x.reshape((batch_out, t_prime, channels * f_prime))?;
        self.out.forward(&x).context("subsampling out projection")
    }
}

/// Full FastConformer encoder: subsampling + relative positional
/// encoding + 24 Conformer blocks.
pub struct FastConformerEncoder {
    pre_encode: DwStridingSubsampling,
    pos_enc: RelPositionalEncoding,
    layers: Vec<ConformerBlock>,
}

impl FastConformerEncoder {
    /// Loads the full encoder from `vb` (rooted at the model's
    /// `encoder.` namespace).
    pub fn load(vb: VarBuilder, cfg: &EncoderConfig, device: &candle_core::Device) -> Result<Self> {
        let pre_encode =
            DwStridingSubsampling::load(vb.pp("pre_encode"), cfg).context("load pre_encode")?;
        let pos_enc =
            RelPositionalEncoding::new(cfg.d_model, cfg.pos_emb_max_len, cfg.xscaling, device)
                .context("init positional encoding")?;
        let mut layers = Vec::with_capacity(cfg.n_layers);
        for i in 0..cfg.n_layers {
            layers.push(
                ConformerBlock::load(
                    vb.pp(format!("layers.{i}")),
                    cfg.d_model,
                    cfg.n_heads,
                    cfg.ff_expansion_factor,
                    cfg.conv_kernel_size,
                    cfg.use_bias,
                )
                .with_context(|| format!("load conformer layer {i}"))?,
            );
        }
        Ok(Self {
            pre_encode,
            pos_enc,
            layers,
        })
    }

    /// Forward: `(B, T, feat_in)` mel features -> `(B, T', d_model)`
    /// encoder output, with `T' = T / subsampling_factor`.
    pub fn forward(&self, mel: &Tensor) -> Result<Tensor> {
        let mut x = self.pre_encode.forward(mel).context("pre_encode")?;
        let (x_scaled, pos_emb) = self.pos_enc.forward(&x).context("pos_enc")?;
        x = x_scaled;
        for (i, layer) in self.layers.iter().enumerate() {
            x = layer
                .forward(&x, &pos_emb, None)
                .with_context(|| format!("conformer layer {i}"))?;
        }
        Ok(x)
    }

    /// Returns the number of Conformer layers. Used by the streaming
    /// wrapper to size the per-layer cache vector.
    #[must_use]
    pub fn n_layers(&self) -> usize {
        self.layers.len()
    }
}

/// Streaming variant of [`FastConformerEncoder`] using local-window
/// attention + per-layer `RotatingConformerCache`.
///
/// Constructed from a loaded [`FastConformerEncoder`] via
/// [`FastConformerEncoderLocal::from_full`] which Arc-clones the
/// pre-encode subsampling, swaps the `RelPositionalEncoding` for a
/// `LocalRelPositionalEncoding`, and constructs each layer as a
/// `LocalConformerBlock` sharing weights with the full-attention block.
///
/// The streaming session holds an instance of this encoder + a
/// `Vec<Option<RotatingConformerCache>>` (one cache per layer). The
/// batch path keeps using the original `FastConformerEncoder`.
pub struct FastConformerEncoderLocal {
    pre_encode: DwStridingSubsampling,
    pos_enc: LocalRelPositionalEncoding,
    layers: Vec<super::conformer_block::LocalConformerBlock>,
}

impl FastConformerEncoderLocal {
    /// Builds the streaming encoder from a loaded full-attention encoder,
    /// sharing all weight tensors via `Arc`-cloned `Tensor` handles.
    /// `context_size = (left, right)` configures the local-attention window.
    pub fn from_full(
        full: &FastConformerEncoder,
        context_size: (usize, usize),
        d_model: usize,
        scale_input: bool,
        device: &candle_core::Device,
    ) -> Result<Self> {
        let pos_enc = LocalRelPositionalEncoding::new(d_model, context_size, scale_input, device)
            .context("init local positional encoding")?;
        let layers: Vec<super::conformer_block::LocalConformerBlock> = full
            .layers
            .iter()
            .enumerate()
            .map(|(i, b)| {
                super::conformer_block::LocalConformerBlock::from_full(b, context_size)
                    .with_context(|| format!("build LocalConformerBlock for layer {i}"))
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            pre_encode: full.pre_encode.clone(),
            pos_enc,
            layers,
        })
    }

    /// Returns the number of Conformer layers (matches the source full
    /// encoder). Used by the streaming wrapper to size the per-layer
    /// cache vector.
    #[must_use]
    pub fn n_layers(&self) -> usize {
        self.layers.len()
    }

    /// Streaming forward with per-layer KV caches.
    ///
    /// Shapes:
    /// - `mel`: `(B, T, feat_in)` channels-last mel input
    /// - `cache`: a slice of `Option<RotatingConformerCache>`, one entry
    ///   per layer. Each entry is mutated in place: `None` becomes
    ///   uninitialised cache (treated as fresh) the first time a layer is
    ///   called with a present cache; the streaming wrapper pre-populates
    ///   them with `Some(RotatingConformerCache::new(...))` before the
    ///   first call.
    ///
    /// Returns `(B, T', d_model)` where `T' = T / subsampling_factor`.
    pub fn forward_with_cache(
        &self,
        mel: &Tensor,
        cache: &mut [Option<super::cache::RotatingConformerCache>],
    ) -> Result<Tensor> {
        anyhow::ensure!(
            cache.len() == self.layers.len(),
            "cache length ({}) must match number of layers ({})",
            cache.len(),
            self.layers.len()
        );

        let x = self.pre_encode.forward(mel).context("pre_encode")?;
        let (x_scaled, pos_emb) = self.pos_enc.forward(&x).context("local pos_enc")?;
        let mut x = x_scaled;
        for (i, (layer, slot)) in self.layers.iter().zip(cache.iter_mut()).enumerate() {
            x = layer
                .forward_with_cache(&x, &pos_emb, None, slot)
                .with_context(|| format!("local conformer layer {i}"))?;
        }
        Ok(x)
    }
}

fn conv2d(
    vb: VarBuilder,
    in_channels: usize,
    out_channels: usize,
    kernel: usize,
    stride: usize,
    padding: usize,
    groups: usize,
) -> Result<Conv2d> {
    let cfg = Conv2dConfig {
        padding,
        stride,
        groups,
        ..Default::default()
    };
    let w = vb
        .get(
            (out_channels, in_channels / groups, kernel, kernel),
            "weight",
        )
        .with_context(|| {
            format!(
                "load conv2d weight ({out_channels}, {}, {kernel}, {kernel})",
                in_channels / groups
            )
        })?;
    let b = vb.get(out_channels, "bias").context("load conv2d bias")?;
    Ok(Conv2d::new(w, Some(b), cfg))
}

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use candle_core::{DType, Device};

    #[test]
    fn positional_encoding_table_has_expected_shape() {
        let pe = RelPositionalEncoding::new(64, 100, false, &Device::Cpu).unwrap();
        // Forward on a (B=1, T=10, D=64) input; pos_emb should be
        // (1, 2*10 - 1, 64) = (1, 19, 64).
        let x = Tensor::zeros((1, 10, 64), DType::F32, &Device::Cpu).unwrap();
        let (_x_out, pe_slice) = pe.forward(&x).unwrap();
        assert_eq!(pe_slice.dims(), &[1, 19, 64]);
    }

    #[test]
    fn positional_encoding_zero_offset_row_is_sin0_cos0() {
        let d = 4_usize;
        let pe = RelPositionalEncoding::new(d, 8, false, &Device::Cpu).unwrap();
        let x = Tensor::zeros((1, 1, d), DType::F32, &Device::Cpu).unwrap();
        let (_x, slice) = pe.forward(&x).unwrap();
        // T=1 -> pos_emb shape (1, 1, d); the single row is pos = 0.
        assert_eq!(slice.dims(), &[1, 1, d]);
        let v: Vec<f32> = slice.flatten_all().unwrap().to_vec1::<f32>().unwrap();
        // pos = 0 gives [sin(0), cos(0), sin(0), cos(0), ...] = [0, 1, 0, 1].
        assert!(v[0].abs() < 1e-6, "expected sin(0)=0, got {}", v[0]);
        assert!((v[1] - 1.0).abs() < 1e-6, "expected cos(0)=1, got {}", v[1]);
        assert!(v[2].abs() < 1e-6, "expected sin(0)=0, got {}", v[2]);
        assert!((v[3] - 1.0).abs() < 1e-6, "expected cos(0)=1, got {}", v[3]);
    }

    #[test]
    fn positional_encoding_rejects_odd_d_model() {
        let Err(err) = RelPositionalEncoding::new(31, 10, false, &Device::Cpu) else {
            panic!("expected odd-d_model rejection");
        };
        assert!(err.to_string().contains("must be even"), "got: {err}");
    }

    #[test]
    fn positional_encoding_rejects_oversized_input() {
        let pe = RelPositionalEncoding::new(4, 8, false, &Device::Cpu).unwrap();
        let x = Tensor::zeros((1, 100, 4), DType::F32, &Device::Cpu).unwrap();
        let err = pe.forward(&x).unwrap_err();
        assert!(
            err.to_string()
                .contains("exceeds positional-encoding capacity"),
            "got: {err}"
        );
    }

    #[test]
    fn parakeet_config_constants_match_documented_v2_values() {
        let c = PARAKEET_0_6B_V2;
        assert_eq!(c.feat_in, 128);
        assert_eq!(c.n_layers, 24);
        assert_eq!(c.d_model, 1024);
        assert_eq!(c.n_heads, 8);
        assert_eq!(c.ff_expansion_factor, 4);
        assert_eq!(c.subsampling_factor, 8);
        assert_eq!(c.subsampling_conv_channels, 256);
        assert_eq!(c.conv_kernel_size, 9);
        assert!(!c.use_bias);
        // d_model must divide cleanly into n_heads.
        assert_eq!(c.d_model % c.n_heads, 0);
        // subsampling_factor must be a power of two.
        assert_eq!(c.subsampling_factor & (c.subsampling_factor - 1), 0);
    }

    #[test]
    fn encoder_config_loads_from_minimal_config_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("config.json");
        std::fs::write(
            &p,
            r#"{
                "encoder": {
                    "feat_in": 128,
                    "n_layers": 24,
                    "d_model": 1024,
                    "n_heads": 8,
                    "ff_expansion_factor": 4,
                    "subsampling_factor": 8,
                    "subsampling_conv_channels": 256,
                    "conv_kernel_size": 9
                }
            }"#,
        )
        .unwrap();
        let c = EncoderConfig::from_config_json(&p).unwrap();
        assert_eq!(c.feat_in, 128);
        assert_eq!(c.n_layers, 24);
        assert_eq!(c.d_model, 1024);
        assert_eq!(c.n_heads, 8);
        assert_eq!(c.ff_expansion_factor, 4);
        assert_eq!(c.subsampling_factor, 8);
        assert_eq!(c.subsampling_conv_channels, 256);
        assert_eq!(c.conv_kernel_size, 9);
        // Inherited defaults.
        assert_eq!(c.pos_emb_max_len, PARAKEET_0_6B_V2.pos_emb_max_len);
        assert!(!c.use_bias);
        assert!(!c.xscaling);
    }

    #[test]
    fn encoder_config_errors_on_missing_encoder_section() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("config.json");
        std::fs::write(&p, r#"{"joint": {}}"#).unwrap();
        let Err(err) = EncoderConfig::from_config_json(&p) else {
            panic!("expected missing-encoder error");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("missing top-level `encoder`"), "got: {msg}");
    }

    #[test]
    fn encoder_config_errors_on_missing_field() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("config.json");
        // Missing `d_model`.
        std::fs::write(&p, r#"{"encoder": {"feat_in": 128, "n_layers": 24}}"#).unwrap();
        let Err(err) = EncoderConfig::from_config_json(&p) else {
            panic!("expected missing-field error");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("encoder.d_model"), "got: {msg}");
    }

    #[test]
    fn encoder_config_errors_on_missing_file() {
        let Err(err) =
            EncoderConfig::from_config_json(std::path::Path::new("/nope/does/not/exist.json"))
        else {
            panic!("expected missing-file error");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("read "), "got: {msg}");
    }
}
