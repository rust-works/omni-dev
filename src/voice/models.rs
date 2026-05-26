//! Model storage convention and path resolution.
//!
//! Two distinct kinds of model are tracked by this module:
//!
//! - **Whisper ASR** (`tiny.en`), loaded by the `whisper-candle` backend.
//! - **Wespeaker speaker embedding** (`resnet34_LM`), loaded by the
//!   speaker-embedding subsystem added in #805 / ADR-0034.
//!
//! Both follow the same three-tier resolution priority:
//!
//! 1. Explicit `--model <path>` (Whisper) or `--speaker-model <path>`
//!    (wespeaker) on the relevant CLI command.
//! 2. `OMNI_DEV_VOICE_WHISPER_MODEL` / `OMNI_DEV_VOICE_SPEAKER_MODEL`
//!    env var.
//! 3. Default install location under the user's home directory.
//!
//! Sharing the helper means the install command writes to exactly the
//! place the backend later reads from — bugs can't diverge between
//! download-target and load-target.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::voice::VoiceOpts;

// ── Whisper constants (retained for backwards compatibility) ──────────────

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

// ── ModelSpec shape ──────────────────────────────────────────────────────

/// Where the bytes of a model come from. Each variant carries the
/// transport-specific metadata the install command needs to fetch the
/// model exactly once and verify its integrity.
#[derive(Debug, Clone, Copy)]
pub enum ModelSource {
    /// HuggingFace Hub — Whisper's distribution. The install command
    /// uses `hf_hub::api::sync::Api` to download `required_files` at a
    /// pinned revision.
    HfHub {
        /// HF repository identifier, e.g. `"openai/whisper-tiny.en"`.
        repo_id: &'static str,
        /// Pinned revision (branch, tag, or ref).
        revision: &'static str,
    },
    /// A single signed GitHub release asset — wespeaker's distribution.
    /// The install command downloads the asset, verifies SHA-256, and
    /// atomically installs into `required_files[0]`.
    HttpReleaseAsset {
        /// Direct download URL.
        url: &'static str,
        /// Expected SHA-256 of the downloaded bytes (hex).
        sha256: &'static str,
        /// Expected size in bytes; informational, for progress messages.
        bytes: u64,
    },
}

/// Fully describes a model variant's storage, install transport, and CLI
/// surface. Static lifetime: every field is `&'static str` (or
/// `&'static [&'static str]`) so `ModelSpec` is `Copy` and `'static`.
#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    /// CLI-facing variant identifier: `"whisper-tiny.en"` or
    /// `"speaker-wespeaker-en"`. Matches the `--variant` value the user
    /// passes to `voice install-model`.
    pub variant: &'static str,
    /// Human label used in error messages: `"Whisper"` or `"Speaker"`.
    pub kind_label: &'static str,
    /// Subdirectory beneath `~/.omni-dev/voice/models/` where this
    /// model's files live.
    pub default_subdir: &'static str,
    /// Files that must exist in the install directory for the model to
    /// be considered installed.
    pub required_files: &'static [&'static str],
    /// Environment-variable override for the install directory.
    pub env_var: &'static str,
    /// Recommended `install-model` invocation, used verbatim in the
    /// `ensure_model_present` error hint.
    pub install_command: &'static str,
    /// CLI flag that overrides the model path on consumer commands,
    /// e.g. `"--model"` (Whisper) or `"--speaker-model"` (wespeaker).
    pub model_flag: &'static str,
    /// How to fetch the bytes.
    pub source: ModelSource,
}

impl ModelSpec {
    /// Default install directory: `~/.omni-dev/voice/models/<default_subdir>/`.
    ///
    /// `None` when the user's home directory cannot be located — same
    /// failure mode as `dirs::home_dir()`.
    pub fn default_dir(&self) -> Option<PathBuf> {
        dirs::home_dir().map(|home| {
            home.join(".omni-dev")
                .join("voice")
                .join("models")
                .join(self.default_subdir)
        })
    }

    /// Resolves the install directory for this spec.
    ///
    /// Priority: `override_path` → env var → default. The returned path
    /// is *not* validated for existence; pair with [`Self::ensure_present`]
    /// for fail-fast.
    pub fn resolve_dir(&self, override_path: Option<&Path>) -> Result<PathBuf> {
        if let Some(p) = override_path {
            return Ok(p.to_path_buf());
        }
        if let Ok(env) = crate::utils::settings::get_env_var(self.env_var) {
            if !env.is_empty() {
                return Ok(PathBuf::from(env));
            }
        }
        self.default_dir().ok_or_else(|| {
            anyhow!(
                "could not determine home directory; \
                 pass {} <path> or set {}",
                self.model_flag,
                self.env_var
            )
        })
    }

    /// Returns the absolute path of each required file inside `dir`.
    pub fn required_files_in(&self, dir: &Path) -> Vec<PathBuf> {
        self.required_files.iter().map(|f| dir.join(f)).collect()
    }

    /// Verifies that `dir` contains every file in `self.required_files`.
    ///
    /// On failure, returns the install hint shaped for this spec (the
    /// `install_command` / `model_flag` baked into the spec).
    pub fn ensure_present(&self, dir: &Path) -> Result<()> {
        for file in self.required_files {
            let path = dir.join(file);
            if !path.is_file() {
                return Err(anyhow!(
                    "no {} model found at {}; \
                     run `{}` or pass {} <path>",
                    self.kind_label,
                    dir.display(),
                    self.install_command,
                    self.model_flag,
                ))
                .with_context(|| format!("missing required file: {}", path.display()));
            }
        }
        Ok(())
    }
}

// ── Registered specs ──────────────────────────────────────────────────────

/// Whisper `tiny.en` — production ASR runtime per ADR-0033.
pub const WHISPER_TINY_EN: ModelSpec = ModelSpec {
    variant: "whisper-tiny.en",
    kind_label: "Whisper",
    default_subdir: DEFAULT_VARIANT_DIR,
    required_files: REQUIRED_FILES,
    env_var: "OMNI_DEV_VOICE_WHISPER_MODEL",
    install_command: "omni-dev voice install-model",
    model_flag: "--model",
    source: ModelSource::HfHub {
        repo_id: MODEL_ID,
        revision: REVISION,
    },
};

/// Parakeet-TDT-0.6B-v2 — pure-Rust ASR backend per issue #898.
///
/// `required_files` lists the files the *backend* expects in the install
/// dir after `voice install-model` has run, including the converted
/// safetensors (`candle_weights.safetensors`), the synthesised
/// `tokenizer.json` (emitted from `config.json::joint.vocabulary` —
/// the upstream MLX repo doesn't ship a tokenizer file), and the
/// `ATTRIBUTION.txt` the install pipeline writes for the model's
/// CC-BY-4.0 licence (the source `model.safetensors` is downloaded
/// and converted but is NOT a required-files entry — the converter's
/// output replaces it).
pub const PARAKEET_TDT_0_6B_V2: ModelSpec = ModelSpec {
    variant: "parakeet-tdt-0.6b-v2",
    kind_label: "Parakeet",
    default_subdir: "parakeet-tdt-0.6b-v2",
    required_files: &[
        "config.json",
        "tokenizer.json",
        "candle_weights.safetensors",
        "ATTRIBUTION.txt",
    ],
    env_var: "OMNI_DEV_VOICE_PARAKEET_MODEL",
    install_command: "omni-dev voice install-model --variant parakeet-tdt-0.6b-v2",
    model_flag: "--model",
    source: ModelSource::HfHub {
        repo_id: "mlx-community/parakeet-tdt-0.6b-v2",
        revision: "main",
    },
};

/// Resolves the Parakeet install directory for the current invocation.
///
/// Priority: `opts.model` → `OMNI_DEV_VOICE_PARAKEET_MODEL` → default.
pub fn resolve_parakeet_model_dir(opts: &VoiceOpts) -> Result<PathBuf> {
    PARAKEET_TDT_0_6B_V2.resolve_dir(opts.model.as_deref())
}

/// Wespeaker `voxceleb_resnet34_LM` — production speaker-embedding
/// runtime per ADR-0034. Not yet wired to consumers; the speaker
/// install variant lands in a follow-up commit.
pub const SPEAKER_WESPEAKER_EN: ModelSpec = ModelSpec {
    variant: "speaker-wespeaker-en",
    kind_label: "Speaker",
    default_subdir: "wespeaker-en-voxceleb-resnet34-LM",
    required_files: &["wespeaker_en_voxceleb_resnet34_LM.onnx"],
    env_var: "OMNI_DEV_VOICE_SPEAKER_MODEL",
    install_command: "omni-dev voice install-model --variant speaker-wespeaker-en",
    model_flag: "--speaker-model",
    source: ModelSource::HttpReleaseAsset {
        url: "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/wespeaker_en_voxceleb_resnet34_LM.onnx",
        sha256: "e9848563da86f263117134dfd7ad63c92355b37de492b55e325400c9d9c39012",
        bytes: 26_530_550,
    },
};

// ── Backwards-compatible Whisper helpers (thin shims) ────────────────────

/// Returns the absolute path of each required model file inside `dir`.
pub fn required_files_in(dir: &Path) -> Vec<PathBuf> {
    WHISPER_TINY_EN.required_files_in(dir)
}

/// Computes the default install location: `~/.omni-dev/voice/models/whisper-tiny.en/`.
///
/// Returns `None` only when the user's home directory cannot be located
/// (i.e. `dirs::home_dir()` returns `None`) — vanishingly rare in practice.
pub fn default_whisper_model_dir() -> Option<PathBuf> {
    WHISPER_TINY_EN.default_dir()
}

/// Resolves the Whisper model directory for the current invocation.
///
/// Priority: `opts.model` → `OMNI_DEV_VOICE_WHISPER_MODEL` → default.
/// The returned path is *not* validated for existence; callers that need
/// to fail-fast on missing files should pair this with [`ensure_model_present`].
pub fn resolve_whisper_model_dir(opts: &VoiceOpts) -> Result<PathBuf> {
    WHISPER_TINY_EN.resolve_dir(opts.model.as_deref())
}

/// Verifies that `dir` contains every file in [`REQUIRED_FILES`].
///
/// On failure, returns the install hint specified by issue #802:
/// `"no Whisper model found at <path>; run `omni-dev voice install-model`
/// or pass --model <path>"`.
pub fn ensure_model_present(dir: &Path) -> Result<()> {
    WHISPER_TINY_EN.ensure_present(dir)
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

    // ── ModelSpec-shaped API tests ──────────────────────────────────────

    #[test]
    fn speaker_spec_default_dir_ends_with_wespeaker_subdir() {
        let dir = SPEAKER_WESPEAKER_EN.default_dir().unwrap();
        assert!(dir.ends_with(".omni-dev/voice/models/wespeaker-en-voxceleb-resnet34-LM"));
    }

    #[test]
    fn speaker_spec_resolve_dir_override_takes_priority() {
        let _g = env_guard();
        std::env::set_var("OMNI_DEV_VOICE_SPEAKER_MODEL", "/should/not/be/read");
        let resolved = SPEAKER_WESPEAKER_EN
            .resolve_dir(Some(Path::new("/explicit/path")))
            .unwrap();
        assert_eq!(resolved, PathBuf::from("/explicit/path"));
        std::env::remove_var("OMNI_DEV_VOICE_SPEAKER_MODEL");
    }

    #[test]
    fn speaker_spec_resolve_dir_env_var_used_when_override_absent() {
        let _g = env_guard();
        std::env::set_var("OMNI_DEV_VOICE_SPEAKER_MODEL", "/from/env");
        let resolved = SPEAKER_WESPEAKER_EN.resolve_dir(None).unwrap();
        assert_eq!(resolved, PathBuf::from("/from/env"));
        std::env::remove_var("OMNI_DEV_VOICE_SPEAKER_MODEL");
    }

    #[test]
    fn speaker_spec_ensure_present_errors_with_install_hint() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = SPEAKER_WESPEAKER_EN.ensure_present(tmp.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no Speaker model found"), "got: {msg}");
        assert!(msg.contains("--variant speaker-wespeaker-en"), "got: {msg}");
        assert!(msg.contains("--speaker-model"), "got: {msg}");
        assert!(
            msg.contains("wespeaker_en_voxceleb_resnet34_LM.onnx"),
            "got: {msg}"
        );
    }

    #[test]
    fn speaker_spec_ensure_present_succeeds_when_file_exists() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("wespeaker_en_voxceleb_resnet34_LM.onnx"),
            b"placeholder",
        )
        .unwrap();
        SPEAKER_WESPEAKER_EN.ensure_present(tmp.path()).unwrap();
    }

    #[test]
    fn whisper_spec_required_files_matches_legacy_helper() {
        let dir = Path::new("/x");
        assert_eq!(
            WHISPER_TINY_EN.required_files_in(dir),
            required_files_in(dir)
        );
    }

    #[test]
    fn whisper_spec_source_carries_pinned_hf_metadata() {
        match WHISPER_TINY_EN.source {
            ModelSource::HfHub { repo_id, revision } => {
                assert_eq!(repo_id, MODEL_ID);
                assert_eq!(revision, REVISION);
            }
            ModelSource::HttpReleaseAsset { .. } => {
                panic!("WHISPER_TINY_EN should be HfHub-sourced");
            }
        }
    }

    #[test]
    fn speaker_spec_source_carries_pinned_release_metadata() {
        match SPEAKER_WESPEAKER_EN.source {
            ModelSource::HttpReleaseAsset { url, sha256, bytes } => {
                assert!(url.contains("wespeaker_en_voxceleb_resnet34_LM.onnx"));
                assert_eq!(
                    sha256,
                    "e9848563da86f263117134dfd7ad63c92355b37de492b55e325400c9d9c39012"
                );
                assert_eq!(bytes, 26_530_550);
            }
            ModelSource::HfHub { .. } => {
                panic!("SPEAKER_WESPEAKER_EN should be HttpReleaseAsset-sourced");
            }
        }
    }
}
