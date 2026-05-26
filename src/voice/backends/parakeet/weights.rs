//! Safetensors loader for the converted Parakeet weights.
//!
//! Opens the `candle_weights.safetensors` file produced by
//! `scripts/convert_parakeet_weights.py` and exposes a [`VarBuilder`]
//! rooted at the model namespace. The converter has already done the
//! MLX→candle conv-axis permutes (Conv1d `(out, k, in)` → `(out, in, k)`,
//! Conv2d `(out, kH, kW, in)` → `(out, in, kH, kW)`); the loader here is
//! identity-load.
//!
//! All weights are loaded as f32. The Parakeet checkpoint is bf16-trained
//! but the converter casts to f32; the spike measured ~1e-3 relative error
//! per sub-block from BLAS drift alone — well inside the ±2 % WER bar in
//! issue #898's acceptance criteria.

use std::path::Path;

use anyhow::{Context, Result};
use candle_core::{DType, Device};
use candle_nn::VarBuilder;

/// Tensor dtype used for all Parakeet weights.
///
/// f32 because the converter emits f32 (MLX bf16 → numpy f32 → safetensors
/// f32). The encoder runs in f32 on CPU; switching to f16/bf16 is a
/// post-port optimisation per the issue's "don't optimise pre-emptively"
/// directive.
pub const DTYPE: DType = DType::F32;

/// Opens `path` as a memory-mapped safetensors file and returns a
/// [`VarBuilder`] rooted at the top-level namespace.
///
/// # Safety
///
/// Uses `VarBuilder::from_mmaped_safetensors` which is `unsafe` because
/// another process could mutate the mmap'd file while the model holds the
/// view. Same trust model as [`crate::voice::backends::candle::CandleTranscriber`]:
/// the file lives inside a user-owned `~/.omni-dev/voice/models/` install
/// directory, so the failure mode is "user concurrently overwrites their
/// model" — accepted.
pub fn open_safetensors(path: &Path, device: &Device) -> Result<VarBuilder<'static>> {
    if !path.is_file() {
        anyhow::bail!(
            "Parakeet weights not found at {} — run `omni-dev voice install-model parakeet-tdt-0.6b-v2`",
            path.display()
        );
    }
    #[allow(unsafe_code)]
    let vb = unsafe {
        VarBuilder::from_mmaped_safetensors(&[path], DTYPE, device)
            .with_context(|| format!("mmap Parakeet weights at {}", path.display()))?
    };
    Ok(vb)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn open_safetensors_missing_file_errors_with_install_hint() {
        let missing = PathBuf::from("/nope/does/not/exist/candle_weights.safetensors");
        let Err(err) = open_safetensors(&missing, &Device::Cpu) else {
            panic!("expected missing-file error");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("Parakeet weights not found"), "got: {msg}");
        assert!(msg.contains("voice install-model"), "got: {msg}");
    }
}
