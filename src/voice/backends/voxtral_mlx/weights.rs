//! Loading the INT4 Voxtral weights via `mlx-rs`.
//!
//! The `mlx-community/…-4bit` model stores 1523 tensors in MLX group-quantized
//! format (`{name}.weight` U32-packed, `{name}.scales`/`{name}.biases` F16) plus
//! F32 norms and the F16 token embedding. `mlx-rs`'s `load_safetensors` maps the
//! file straight into a `name → Array` dictionary on the default device (Metal).

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Result};
use mlx_rs::Array;

/// Loads every tensor from `path` (a `model.safetensors`) into a `name → Array`
/// map on the default MLX device.
pub fn load_safetensors(path: &Path) -> Result<HashMap<String, Array>> {
    Array::load_safetensors(path)
        .map_err(|e| anyhow!("load safetensors from {}: {e}", path.display()))
}

/// Fetches a tensor by name, erroring (rather than panicking) if absent — so a
/// weight-layout mismatch surfaces as a clear message during the port.
pub fn get_tensor<'a, S: std::hash::BuildHasher>(
    weights: &'a HashMap<String, Array, S>,
    name: &str,
) -> Result<&'a Array> {
    weights
        .get(name)
        .ok_or_else(|| anyhow!("missing expected tensor: {name:?}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Resolves the INT4 model dir from `OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL`
    /// (a directory containing `model.safetensors`), else `None`.
    fn model_dir() -> Option<std::path::PathBuf> {
        std::env::var("OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
    }

    #[test]
    #[ignore = "requires the INT4 Voxtral model; set OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL=<dir> (#933 M1)"]
    fn loads_int4_weights_via_mlx_on_metal() {
        let Some(dir) = model_dir() else {
            panic!("set OMNI_DEV_VOICE_VOXTRAL_MLX_MODEL=<dir with model.safetensors>");
        };
        let weights = load_safetensors(&dir.join("model.safetensors"))
            .expect("INT4 safetensors should load via mlx-rs");

        // The 4-bit model has 1523 tensors (#933 M0d).
        assert_eq!(weights.len(), 1523, "unexpected tensor count");

        // The token embedding is F16 [vocab=131072, dim=3072]; materialise it on
        // the device to prove the load round-trips through Metal.
        let emb = get_tensor(&weights, "decoder.tok_embeddings.weight").unwrap();
        assert_eq!(emb.shape(), &[131_072, 3072], "tok_embeddings shape");

        // A quantized decoder weight is U32-packed [out, in/8].
        let wq = get_tensor(&weights, "decoder.layers.0.attention.wq.weight").unwrap();
        assert_eq!(wq.shape(), &[4096, 384], "decoder wq packed shape");
    }
}
