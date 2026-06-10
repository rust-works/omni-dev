//! Voxtral Realtime Mini 4B configuration — a direct translation of
//! `mlx-audio`'s `voxtral_realtime/config.py` (the port reference). Fixed for
//! the shipped model; values cross-checked against the INT4 safetensors shapes
//! (#933 M0d).

/// Audio / mel front-end parameters.
#[derive(Debug, Clone, Copy)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub frame_rate: f32,
    pub num_mel_bins: usize,
    pub hop_length: usize,
    pub window_size: usize,
    pub global_log_mel_max: f32,
}

/// The audio **encoder** (a Whisper-style transformer over mel features).
#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub dim: usize,
    pub n_layers: usize,
    pub n_heads: usize,
    pub head_dim: usize,
    pub hidden_dim: usize,
    pub n_kv_heads: usize,
    pub norm_eps: f32,
    pub rope_theta: f32,
    pub sliding_window: usize,
    pub downsample_factor: usize,
}

/// The text **decoder** (a quantized Llama-style transformer with Voxtral's
/// per-layer `ada_rms_norm` time conditioning).
#[derive(Debug, Clone, Copy)]
pub struct DecoderConfig {
    pub dim: usize,
    pub n_layers: usize,
    pub n_heads: usize,
    pub n_kv_heads: usize,
    pub head_dim: usize,
    pub hidden_dim: usize,
    pub vocab_size: usize,
    pub norm_eps: f32,
    pub rope_theta: f32,
    pub sliding_window: usize,
    pub ada_rms_norm_t_cond_dim: usize,
}

/// INT4 quantization parameters (MLX group quantization). Derived from the
/// safetensors shapes: e.g. decoder `wq.weight` `[4096, 384]` U32 packs
/// `384 * 8 = 3072` 4-bit values per row (= input dim), and `wq.scales`
/// `[4096, 48]` → `3072 / 48 = 64` per group.
#[derive(Debug, Clone, Copy)]
pub struct QuantConfig {
    pub group_size: i32,
    pub bits: i32,
}

/// The full model configuration.
#[derive(Debug, Clone, Copy)]
pub struct VoxtralMlxConfig {
    pub audio: AudioConfig,
    pub encoder: EncoderConfig,
    pub decoder: DecoderConfig,
    pub quant: QuantConfig,
    /// Default decoder delay (lookahead) in ms (the #930 spike sweet spot).
    pub default_delay_ms: i32,
}

impl VoxtralMlxConfig {
    /// The shipped Voxtral Realtime Mini 4B configuration.
    #[must_use]
    pub const fn voxtral_realtime_mini_4b() -> Self {
        Self {
            audio: AudioConfig {
                sample_rate: 16_000,
                frame_rate: 12.5,
                num_mel_bins: 128,
                hop_length: 160,
                window_size: 400,
                global_log_mel_max: 1.5,
            },
            encoder: EncoderConfig {
                dim: 1280,
                n_layers: 32,
                n_heads: 32,
                head_dim: 64,
                hidden_dim: 5120,
                n_kv_heads: 32,
                norm_eps: 1e-5,
                rope_theta: 1_000_000.0,
                sliding_window: 750,
                downsample_factor: 4,
            },
            decoder: DecoderConfig {
                dim: 3072,
                n_layers: 26,
                n_heads: 32,
                n_kv_heads: 8,
                head_dim: 128,
                hidden_dim: 9216,
                vocab_size: 131_072,
                norm_eps: 1e-5,
                rope_theta: 1_000_000.0,
                sliding_window: 8192,
                ada_rms_norm_t_cond_dim: 32,
            },
            quant: QuantConfig {
                group_size: 64,
                bits: 4,
            },
            default_delay_ms: 480,
        }
    }
}

impl Default for VoxtralMlxConfig {
    fn default() -> Self {
        Self::voxtral_realtime_mini_4b()
    }
}
