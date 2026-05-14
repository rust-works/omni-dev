//! Whisper model storage convention and path resolution.
//!
//! Centralises the three-tier priority used by both the `whisper-candle`
//! backend (load path) and the `voice install-model` CLI (download path):
//!
//! 1. Explicit `--model <path>` on [`crate::voice::VoiceOpts`].
//! 2. `OMNI_DEV_VOICE_WHISPER_MODEL` env var.
//! 3. Default install location under the user's home directory.
//!
//! Sharing the helper means the install command writes to exactly the
//! place the backend later reads from — bugs can't diverge between
//! download-target and load-target.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::voice::VoiceOpts;

/// HuggingFace repository identifier for the `tiny.en` Whisper variant.
pub const MODEL_ID: &str = "openai/whisper-tiny.en";

/// Pinned HuggingFace revision. `refs/pr/15` adds the safetensors weights
/// to `openai/whisper-tiny.en`; the candle spike in #813 validated this
/// exact revision end-to-end.
pub const REVISION: &str = "refs/pr/15";

/// The three files the Whisper backend needs to load. Order matters for
/// the install command's progress messages; the backend itself loads them
/// via [`required_files_in`] independent of order.
pub const REQUIRED_FILES: &[&str] = &["config.json", "tokenizer.json", "model.safetensors"];

/// Default subdirectory name beneath `~/.omni-dev/voice/models/`.
///
/// Derived from [`MODEL_ID`] by stripping the `openai/` org prefix; keeps
/// room for future variants (`whisper-base.en`, multilingual) as sibling
/// dirs.
pub const DEFAULT_VARIANT_DIR: &str = "whisper-tiny.en";

/// Returns the absolute path of each required model file inside `dir`.
pub fn required_files_in(dir: &Path) -> Vec<PathBuf> {
    REQUIRED_FILES.iter().map(|f| dir.join(f)).collect()
}

/// Computes the default install location: `~/.omni-dev/voice/models/whisper-tiny.en/`.
///
/// Returns `None` only when the user's home directory cannot be located
/// (i.e. `dirs::home_dir()` returns `None`) — vanishingly rare in practice.
pub fn default_whisper_model_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| {
        home.join(".omni-dev")
            .join("voice")
            .join("models")
            .join(DEFAULT_VARIANT_DIR)
    })
}

/// Resolves the Whisper model directory for the current invocation.
///
/// Priority: `opts.model` → `OMNI_DEV_VOICE_WHISPER_MODEL` → default.
/// The returned path is *not* validated for existence; callers that need
/// to fail-fast on missing files should pair this with [`ensure_model_present`].
pub fn resolve_whisper_model_dir(opts: &VoiceOpts) -> Result<PathBuf> {
    if let Some(p) = &opts.model {
        return Ok(p.clone());
    }
    if let Ok(env) = crate::utils::settings::get_env_var("OMNI_DEV_VOICE_WHISPER_MODEL") {
        if !env.is_empty() {
            return Ok(PathBuf::from(env));
        }
    }
    default_whisper_model_dir().ok_or_else(|| {
        anyhow!(
            "could not determine home directory; \
             pass --model <path> or set OMNI_DEV_VOICE_WHISPER_MODEL"
        )
    })
}

/// Verifies that `dir` contains every file in [`REQUIRED_FILES`].
///
/// On failure, returns the install hint specified by issue #802:
/// `"no Whisper model found at <path>; run `omni-dev voice install-model`
/// or pass --model <path>"`.
pub fn ensure_model_present(dir: &Path) -> Result<()> {
    for file in REQUIRED_FILES {
        let path = dir.join(file);
        if !path.is_file() {
            return Err(anyhow!(
                "no Whisper model found at {}; \
                 run `omni-dev voice install-model` or pass --model <path>",
                dir.display()
            ))
            .with_context(|| format!("missing required file: {}", path.display()));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static ENV_GUARD: Mutex<()> = Mutex::new(());

    fn env_guard() -> MutexGuard<'static, ()> {
        match ENV_GUARD.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    #[test]
    fn opts_model_takes_top_priority() {
        let _g = env_guard();
        std::env::set_var("OMNI_DEV_VOICE_WHISPER_MODEL", "/should/not/be/read");
        let opts = VoiceOpts {
            backend: None,
            model: Some(PathBuf::from("/explicit/path")),
        };
        let resolved = resolve_whisper_model_dir(&opts).unwrap();
        assert_eq!(resolved, PathBuf::from("/explicit/path"));
        std::env::remove_var("OMNI_DEV_VOICE_WHISPER_MODEL");
    }

    #[test]
    fn env_var_used_when_opts_absent() {
        let _g = env_guard();
        std::env::set_var("OMNI_DEV_VOICE_WHISPER_MODEL", "/from/env");
        let resolved = resolve_whisper_model_dir(&VoiceOpts::default()).unwrap();
        assert_eq!(resolved, PathBuf::from("/from/env"));
        std::env::remove_var("OMNI_DEV_VOICE_WHISPER_MODEL");
    }

    #[test]
    fn empty_env_var_falls_through_to_default() {
        let _g = env_guard();
        std::env::set_var("OMNI_DEV_VOICE_WHISPER_MODEL", "");
        let resolved = resolve_whisper_model_dir(&VoiceOpts::default()).unwrap();
        let expected = default_whisper_model_dir().unwrap();
        assert_eq!(resolved, expected);
        std::env::remove_var("OMNI_DEV_VOICE_WHISPER_MODEL");
    }

    #[test]
    fn default_path_uses_omni_dev_voice_models_subdir() {
        let dir = default_whisper_model_dir().unwrap();
        assert!(dir.ends_with(".omni-dev/voice/models/whisper-tiny.en"));
    }

    #[test]
    fn ensure_model_present_succeeds_when_all_files_exist() {
        let tmp = tempfile::TempDir::new().unwrap();
        for f in REQUIRED_FILES {
            std::fs::write(tmp.path().join(f), b"placeholder").unwrap();
        }
        ensure_model_present(tmp.path()).unwrap();
    }

    #[test]
    fn ensure_model_present_errors_with_hint_when_files_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = ensure_model_present(tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no Whisper model found"), "got: {msg}");
        assert!(msg.contains("voice install-model"), "got: {msg}");
        assert!(msg.contains("--model"), "got: {msg}");
    }

    #[test]
    fn ensure_model_present_errors_when_any_file_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        // Write two of three required files; tokenizer.json missing.
        std::fs::write(tmp.path().join("config.json"), b"x").unwrap();
        std::fs::write(tmp.path().join("model.safetensors"), b"x").unwrap();
        let err = ensure_model_present(tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("tokenizer.json"), "got: {msg}");
    }

    #[test]
    fn required_files_in_returns_three_paths() {
        let paths = required_files_in(Path::new("/x"));
        assert_eq!(paths.len(), 3);
        assert_eq!(paths[0], PathBuf::from("/x/config.json"));
        assert_eq!(paths[1], PathBuf::from("/x/tokenizer.json"));
        assert_eq!(paths[2], PathBuf::from("/x/model.safetensors"));
    }
}
