//! `omni-dev voice install-model` — one-time fetch of model artefacts.
//!
//! Supports two variants: `whisper-tiny.en` for the `whisper-candle` ASR
//! backend, and `speaker-wespeaker-en` for the speaker-embedding runtime
//! added in #805 / ADR-0034. Files land in the conventional install
//! locations beneath `~/.omni-dev/voice/models/`.
//!
//! Bumps the model-download cost to install time rather than transcribe/
//! enrol time, so network failures surface explicitly when the user opts
//! in to installing rather than silently on first use.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, ValueEnum};
use hf_hub::{api::sync::Api, Repo, RepoType};
use sha2::{Digest, Sha256};

use crate::voice::models::{
    ModelSource, ModelSpec, PARAKEET_TDT_0_6B_V2, SPEAKER_WESPEAKER_EN, WHISPER_TINY_EN,
};

/// Which model variant to install.
///
/// `--variant` defaults to `whisper-tiny.en` so bare
/// `voice install-model` continues to install the ASR model — the
/// pre-#805 behaviour.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Variant {
    /// OpenAI Whisper `tiny.en` (ADR-0033).
    #[default]
    #[value(name = "whisper-tiny.en")]
    WhisperTinyEn,
    /// NVIDIA Parakeet-TDT-0.6B-v2 — pure-Rust ASR per issue #898.
    /// Downloads the MLX-format safetensors and runs the
    /// `scripts/convert_parakeet_weights.py` permute pass to produce
    /// the candle-friendly weights the backend loads.
    #[value(name = "parakeet-tdt-0.6b-v2")]
    ParakeetTdt06bV2,
    /// Wespeaker `resnet34_LM` English-only speaker embedding (ADR-0034).
    #[value(name = "speaker-wespeaker-en")]
    SpeakerWespeakerEn,
}

impl Variant {
    /// Returns the [`ModelSpec`] for this variant.
    pub fn spec(self) -> &'static ModelSpec {
        match self {
            Self::WhisperTinyEn => &WHISPER_TINY_EN,
            Self::ParakeetTdt06bV2 => &PARAKEET_TDT_0_6B_V2,
            Self::SpeakerWespeakerEn => &SPEAKER_WESPEAKER_EN,
        }
    }
}

/// Downloads the model files for a chosen variant into the conventional
/// install location at `~/.omni-dev/voice/models/<variant-subdir>/` (or
/// `--dest` to override).
///
/// Idempotent: if every required file is already present and non-empty,
/// the command prints a "model already installed" line and exits 0. Pass
/// `--force` to re-download anyway.
#[derive(Parser)]
pub struct InstallModelCommand {
    /// Override the install directory. Defaults to the variant's
    /// canonical location under `~/.omni-dev/voice/models/`.
    #[arg(long)]
    pub dest: Option<PathBuf>,

    /// Re-download even if all required files are already present.
    #[arg(long)]
    pub force: bool,

    /// Which model variant to install. Defaults to `whisper-tiny.en`.
    #[arg(long, value_enum, default_value_t = Variant::WhisperTinyEn)]
    pub variant: Variant,
}

impl InstallModelCommand {
    /// Entry point. Writes user-facing progress to stderr so stdout stays
    /// reserved for machine-readable output (parity with `voice
    /// transcribe`'s JSONL pipe-detection convention).
    pub fn execute(self) -> Result<()> {
        let mut err = std::io::stderr().lock();
        self.run(&mut err)
    }

    /// Writer-generic core, parameterised over stderr so tests can drive
    /// the success/idempotency paths without touching the global stream.
    fn run<W: Write>(self, w: &mut W) -> Result<()> {
        let spec = self.variant.spec();
        let dest = match self.dest {
            Some(p) => p,
            None => spec
                .default_dir()
                .ok_or_else(|| anyhow!("could not determine home directory; pass --dest <path>"))?,
        };

        if !self.force && all_present(spec, &dest) {
            writeln!(w, "model already installed at {}", dest.display())?;
            return Ok(());
        }

        // Parakeet has a different upstream file set than the install
        // dir's required_files (raw MLX safetensors vs. converted
        // candle safetensors), so it gets its own install path that
        // shells out to the Python converter after download.
        if matches!(self.variant, Variant::ParakeetTdt06bV2) {
            return install_parakeet(spec, &dest, w);
        }

        match spec.source {
            ModelSource::HfHub { repo_id, revision } => {
                download_hf_hub(spec, repo_id, revision, &dest, w)
            }
            ModelSource::HttpReleaseAsset { url, sha256, bytes } => {
                download_release_asset(spec, url, sha256, bytes, &dest, w)
            }
        }
    }
}

/// Files the Parakeet install pipeline pulls from the upstream HF repo
/// (distinct from the spec's `required_files`, which lists what the
/// backend expects in the dir *after* conversion). `tokenizer.json` is
/// NOT in this list because the upstream `mlx-community/parakeet-tdt-0.6b-v2`
/// repo doesn't ship one — the 1024-token BPE vocab is embedded in
/// `config.json` at `joint.vocabulary`. The install pipeline synthesises
/// a decode-only `tokenizer.json` from that vocab after download.
const PARAKEET_UPSTREAM_FILES: &[&str] = &["config.json", "model.safetensors"];

/// CC-BY-4.0 attribution written to the Parakeet model dir per the
/// issue #898 acceptance criterion. Single source of truth; the file
/// content is exact-match-tested in install_model's test module.
const PARAKEET_ATTRIBUTION: &str = "\
NVIDIA Parakeet-TDT-0.6B-v2 (mlx-community/parakeet-tdt-0.6b-v2)
Licensed under CC-BY-4.0
https://creativecommons.org/licenses/by/4.0/

Source: https://huggingface.co/mlx-community/parakeet-tdt-0.6b-v2
Original model: NVIDIA Corporation
MLX port: senstella + mlx-community contributors
Candle port: omni-dev project (issue #898)
";

/// Parakeet install: download raw MLX safetensors from HF, run the
/// Python converter to produce `candle_weights.safetensors`, write the
/// CC-BY-4.0 attribution file. The HF download uses the same Api the
/// other variants do; the converter call is a `std::process::Command`
/// because the converter lives in `scripts/` for ease of iteration
/// independent of the Rust release cycle.
fn install_parakeet<W: Write>(spec: &ModelSpec, dest: &Path, w: &mut W) -> Result<()> {
    let ModelSource::HfHub { repo_id, revision } = spec.source else {
        bail!(
            "internal error: Parakeet variant has non-HfHub source ({:?})",
            spec.source
        );
    };

    writeln!(
        w,
        "Installing {repo_id} (revision {revision}) -> {}",
        dest.display()
    )?;
    std::fs::create_dir_all(dest)
        .with_context(|| format!("create install directory at {}", dest.display()))?;

    let api = Api::new().context("initialise HuggingFace Hub client")?;
    let repo = api.repo(Repo::with_revision(
        repo_id.to_string(),
        RepoType::Model,
        revision.to_string(),
    ));

    // Download upstream files (config.json, tokenizer.json,
    // model.safetensors) into the dest dir.
    for file in PARAKEET_UPSTREAM_FILES {
        let start = Instant::now();
        write!(w, "  fetching {file}... ")?;
        w.flush()?;
        let downloaded = repo.get(file).with_context(|| {
            format!(
                "download {file} from {repo_id} (revision {revision}). \
                 Check your network or set HTTPS_PROXY"
            )
        })?;
        let target = dest.join(file);
        atomic_install_copy(&downloaded, &target).with_context(|| {
            format!(
                "install {file} into {} (atomic rename failed)",
                target.display()
            )
        })?;
        let bytes = std::fs::metadata(&target).map_or(0, |m| m.len());
        writeln!(
            w,
            "done ({bytes} bytes in {:.1}s)",
            start.elapsed().as_secs_f64()
        )?;
    }

    // Run the Python converter to produce candle_weights.safetensors.
    let src_safetensors = dest.join("model.safetensors");
    let out_safetensors = dest.join("candle_weights.safetensors");
    let converter =
        locate_parakeet_converter().context("locate scripts/convert_parakeet_weights.py")?;

    write!(w, "  converting weights via {}... ", converter.display())?;
    w.flush()?;
    let start = Instant::now();
    let status = std::process::Command::new(python_binary())
        .arg(&converter)
        .arg("--src")
        .arg(&src_safetensors)
        .arg("--out")
        .arg(&out_safetensors)
        .status()
        .context(
            "spawn python3 for converter. \
             Ensure python3 + numpy + safetensors are installed: \
             `pip install numpy safetensors`",
        )?;
    if !status.success() {
        bail!(
            "converter failed with exit code {:?}; see PARAKEET-CONVERT: log lines above",
            status.code()
        );
    }
    writeln!(w, "done ({:.1}s)", start.elapsed().as_secs_f64())?;

    // Delete the raw MLX safetensors to save ~2.47 GB — the converted
    // file is what the backend loads.
    if let Err(e) = std::fs::remove_file(&src_safetensors) {
        // Non-fatal: log a warning and continue.
        writeln!(
            w,
            "  warning: failed to delete raw {} ({e}); leaving in place",
            src_safetensors.display()
        )?;
    }

    // Synthesise tokenizer.json from config.json's embedded vocab.
    // The upstream HF repo doesn't ship tokenizer.json; the 1024-token
    // BPE vocab lives at config.json::joint.vocabulary.
    let config_path = dest.join("config.json");
    let tokenizer_path = dest.join("tokenizer.json");
    write!(w, "  writing tokenizer.json from config.json vocab... ")?;
    w.flush()?;
    write_tokenizer_json(&config_path, &tokenizer_path)
        .context("synthesise tokenizer.json from config.json vocab")?;
    let tok_bytes = std::fs::metadata(&tokenizer_path).map_or(0, |m| m.len());
    writeln!(w, "done ({tok_bytes} bytes)")?;

    // Write CC-BY-4.0 attribution.
    let attribution_path = dest.join("ATTRIBUTION.txt");
    atomic_install_bytes(PARAKEET_ATTRIBUTION.as_bytes(), &attribution_path)
        .context("write Parakeet ATTRIBUTION.txt")?;

    writeln!(
        w,
        "{} model installed at {}",
        spec.kind_label,
        dest.display()
    )?;
    Ok(())
}

/// Synthesises a decode-only HF `tokenizer.json` from the BPE vocab
/// embedded in Parakeet's `config.json` (`joint.vocabulary` — 1024
/// entries for v2). Decode-only is sufficient because Parakeet's TDT
/// joiner emits token IDs; it never tokenises input text. The shape
/// follows the HuggingFace `tokenizers` library's BPE model schema:
///
/// - `decoder.type = Metaspace` with replacement `▁` (U+2581) — the
///   SentencePiece word-start marker. The decoder strips it on output,
///   turning `["▁the", "▁cat"]` into `"the cat"`.
/// - `model.merges = []` — merge rules are only needed for *encoding*
///   input; the BPE model only needs the vocab to decode IDs.
/// - `byte_fallback = false` — the 1024-token vocab covers the
///   training corpus by construction.
fn write_tokenizer_json(config_path: &Path, out_path: &Path) -> Result<()> {
    let cfg_text = std::fs::read_to_string(config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    let cfg: serde_json::Value = serde_json::from_str(&cfg_text).context("parse config.json")?;
    let vocab_list = cfg["joint"]["vocabulary"]
        .as_array()
        .context("config.json: missing joint.vocabulary array")?;
    let vocab: serde_json::Map<String, serde_json::Value> = vocab_list
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let tok = t
                .as_str()
                .ok_or_else(|| anyhow!("vocab entry {i} is not a string"))?;
            Ok::<_, anyhow::Error>((tok.to_string(), serde_json::Value::from(i)))
        })
        .collect::<Result<_>>()?;
    let tok = serde_json::json!({
        "version": "1.0",
        "truncation": null,
        "padding": null,
        "added_tokens": [],
        "normalizer": null,
        "pre_tokenizer": null,
        "post_processor": null,
        "decoder": {
            "type": "Metaspace",
            "replacement": "\u{2581}",
            "prepend_scheme": "first",
            "split": true,
        },
        "model": {
            "type": "BPE",
            "dropout": null,
            "unk_token": "<unk>",
            "continuing_subword_prefix": null,
            "end_of_word_suffix": null,
            "fuse_unk": false,
            "byte_fallback": false,
            "ignore_merges": true,
            "vocab": vocab,
            "merges": [],
        },
    });
    let json_bytes = serde_json::to_vec(&tok).context("serialise tokenizer.json")?;
    atomic_install_bytes(&json_bytes, out_path)
        .with_context(|| format!("write tokenizer.json to {}", out_path.display()))
}

/// Returns the python3 binary to invoke. Honours `PYTHON` env var so a
/// caller can pin a specific interpreter (e.g. `PYTHON=python3.11`) or
/// point at a venv.
fn python_binary() -> std::ffi::OsString {
    std::env::var_os("PYTHON").unwrap_or_else(|| std::ffi::OsString::from("python3"))
}

/// Locates `scripts/convert_parakeet_weights.py` relative to the
/// running binary or the CWD. Honours `OMNI_DEV_PARAKEET_CONVERTER`
/// for explicit overrides (test harness / non-standard installs).
fn locate_parakeet_converter() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("OMNI_DEV_PARAKEET_CONVERTER") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Ok(path);
        }
        bail!(
            "OMNI_DEV_PARAKEET_CONVERTER points at {} which is not a file",
            path.display()
        );
    }
    // Search candidates: CWD/scripts/, exe-dir/../scripts/, exe-dir/scripts/.
    let candidates = std::iter::once(PathBuf::from("scripts/convert_parakeet_weights.py"))
        .chain(std::env::current_exe().ok().and_then(|exe| {
            let dir = exe.parent()?;
            Some(dir.join("../scripts/convert_parakeet_weights.py"))
        }))
        .chain(std::env::current_exe().ok().and_then(|exe| {
            let dir = exe.parent()?;
            Some(dir.join("scripts/convert_parakeet_weights.py"))
        }));
    for c in candidates {
        if c.is_file() {
            return Ok(c);
        }
    }
    bail!(
        "could not find scripts/convert_parakeet_weights.py. \
         Set OMNI_DEV_PARAKEET_CONVERTER=/path/to/convert_parakeet_weights.py \
         or run install-model from the omni-dev repo root"
    )
}

fn all_present(spec: &ModelSpec, dir: &Path) -> bool {
    spec.required_files_in(dir)
        .iter()
        .all(|p| p.is_file() && p.metadata().is_ok_and(|m| m.len() > 0))
}

fn download_hf_hub<W: Write>(
    spec: &ModelSpec,
    repo_id: &str,
    revision: &str,
    dest: &Path,
    w: &mut W,
) -> Result<()> {
    writeln!(
        w,
        "Installing {repo_id} (revision {revision}) -> {}",
        dest.display()
    )?;
    std::fs::create_dir_all(dest)
        .with_context(|| format!("create install directory at {}", dest.display()))?;

    let api = Api::new().context("initialise HuggingFace Hub client")?;
    let repo = api.repo(Repo::with_revision(
        repo_id.to_string(),
        RepoType::Model,
        revision.to_string(),
    ));

    for file in spec.required_files {
        let start = Instant::now();
        write!(w, "  fetching {file}... ")?;
        w.flush()?;
        let downloaded = repo.get(file).with_context(|| {
            format!(
                "download {file} from {repo_id} (revision {revision}). \
                 Check your network or set HTTPS_PROXY"
            )
        })?;
        let target = dest.join(file);
        atomic_install_copy(&downloaded, &target).with_context(|| {
            format!(
                "install {file} into {} (atomic rename failed)",
                target.display()
            )
        })?;
        let bytes = std::fs::metadata(&target).map_or(0, |m| m.len());
        writeln!(
            w,
            "done ({bytes} bytes in {:.1}s)",
            start.elapsed().as_secs_f64()
        )?;
    }

    writeln!(
        w,
        "{} model installed at {}",
        spec.kind_label,
        dest.display()
    )?;
    Ok(())
}

fn download_release_asset<W: Write>(
    spec: &ModelSpec,
    url: &str,
    expected_sha256: &str,
    expected_bytes: u64,
    dest: &Path,
    w: &mut W,
) -> Result<()> {
    // Wespeaker (and any future single-asset release-driven model) ships
    // exactly one file. The check is defensive: if a future spec mis-
    // declares N!=1 with HttpReleaseAsset, fail loudly rather than
    // silently install only the first.
    if spec.required_files.len() != 1 {
        bail!(
            "HttpReleaseAsset source expects exactly one required_file, \
             got {} for variant {}",
            spec.required_files.len(),
            spec.variant
        );
    }
    let file_name = spec.required_files[0];
    let target = dest.join(file_name);

    writeln!(
        w,
        "Installing {file_name} ({expected_bytes} B) -> {}",
        dest.display()
    )?;
    std::fs::create_dir_all(dest)
        .with_context(|| format!("create install directory at {}", dest.display()))?;

    let start = Instant::now();
    write!(w, "  fetching {url}... ")?;
    w.flush()?;

    let resp = ureq::get(url)
        .call()
        .with_context(|| format!("HTTP GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!(
            "HTTP {} fetching {url}: {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("Unknown"),
        );
    }
    let bytes = resp
        .into_body()
        .read_to_vec()
        .with_context(|| format!("read response body for {url}"))?;

    let actual_sha = {
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        let digest = hasher.finalize();
        let mut hex = String::with_capacity(digest.len() * 2);
        for byte in digest {
            use std::fmt::Write as _;
            // `write!` into a `String` is infallible.
            let _ = write!(&mut hex, "{byte:02x}");
        }
        hex
    };
    if !actual_sha.eq_ignore_ascii_case(expected_sha256) {
        bail!("SHA-256 mismatch for {file_name}: expected {expected_sha256}, got {actual_sha}");
    }

    atomic_install_bytes(&bytes, &target).with_context(|| {
        format!(
            "install {file_name} into {} (atomic rename failed)",
            target.display()
        )
    })?;
    writeln!(
        w,
        "done ({} bytes in {:.1}s; sha256 verified)",
        bytes.len(),
        start.elapsed().as_secs_f64()
    )?;
    writeln!(
        w,
        "{} model installed at {}",
        spec.kind_label,
        dest.display()
    )?;
    Ok(())
}

/// Writes `bytes` to a `.part` sibling of `to`, then atomically renames
/// so a partial download never leaves a half-written file at `to`.
fn atomic_install_bytes(bytes: &[u8], to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir {}", parent.display()))?;
    }
    let tmp = part_sibling(to)?;
    std::fs::write(&tmp, bytes)
        .with_context(|| format!("write {} bytes -> {}", bytes.len(), tmp.display()))?;
    std::fs::rename(&tmp, to)
        .with_context(|| format!("rename {} -> {}", tmp.display(), to.display()))?;
    Ok(())
}

/// Copies `from` into `to` via a temp file sibling + rename so a partial
/// download never leaves a half-written file at the destination.
fn atomic_install_copy(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir {}", parent.display()))?;
    }
    let tmp = part_sibling(to)?;
    std::fs::copy(from, &tmp)
        .with_context(|| format!("copy {} -> {}", from.display(), tmp.display()))?;
    std::fs::rename(&tmp, to)
        .with_context(|| format!("rename {} -> {}", tmp.display(), to.display()))?;
    Ok(())
}

fn part_sibling(to: &Path) -> Result<PathBuf> {
    let file_name = to
        .file_name()
        .ok_or_else(|| anyhow!("destination path has no file name: {}", to.display()))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(".part");
    Ok(to.with_file_name(tmp_name))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::voice::models::REQUIRED_FILES;
    use std::sync::{Mutex, MutexGuard};

    // HOME-mutating tests share this guard so they don't race the
    // env-mutating tests in `voice::models`.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    fn env_guard() -> MutexGuard<'static, ()> {
        match ENV_GUARD.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn stage_complete_whisper_model(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for f in REQUIRED_FILES {
            std::fs::write(dir.join(f), b"placeholder").unwrap();
        }
    }

    fn stage_complete_speaker_model(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for f in SPEAKER_WESPEAKER_EN.required_files {
            std::fs::write(dir.join(f), b"placeholder").unwrap();
        }
    }

    #[test]
    fn idempotent_when_all_files_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        stage_complete_whisper_model(tmp.path());

        let cmd = InstallModelCommand {
            dest: Some(tmp.path().to_path_buf()),
            force: false,
            variant: Variant::WhisperTinyEn,
        };
        let mut out: Vec<u8> = Vec::new();
        cmd.run(&mut out).unwrap();
        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("already installed"), "got: {msg}");
    }

    #[test]
    fn idempotent_when_speaker_model_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        stage_complete_speaker_model(tmp.path());

        let cmd = InstallModelCommand {
            dest: Some(tmp.path().to_path_buf()),
            force: false,
            variant: Variant::SpeakerWespeakerEn,
        };
        let mut out: Vec<u8> = Vec::new();
        cmd.run(&mut out).unwrap();
        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("already installed"), "got: {msg}");
    }

    #[test]
    fn idempotent_skip_treats_zero_byte_file_as_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        for f in REQUIRED_FILES {
            // Zero-byte file would normally pass `is_file()` but is_not
            // a valid model artifact; the idempotency check must reject it.
            std::fs::write(tmp.path().join(f), b"").unwrap();
        }
        assert!(!all_present(&WHISPER_TINY_EN, tmp.path()));
    }

    #[test]
    fn atomic_install_copy_replaces_target() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::write(&src, b"hello").unwrap();
        std::fs::write(&dst, b"old").unwrap();
        atomic_install_copy(&src, &dst).unwrap();
        let got = std::fs::read(&dst).unwrap();
        assert_eq!(got, b"hello");
        // No leftover temp file.
        let leftover = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().ends_with(".part"));
        assert!(!leftover, "atomic_install_copy must not leave .part files");
    }

    #[test]
    fn atomic_install_bytes_writes_and_renames() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dst = tmp.path().join("out");
        atomic_install_bytes(b"hello", &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello");
        let leftover = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().ends_with(".part"));
        assert!(!leftover, "atomic_install_bytes must not leave .part files");
    }

    #[test]
    fn parses_no_args() {
        #[derive(Parser)]
        struct T {
            #[command(flatten)]
            c: InstallModelCommand,
        }
        let t = T::try_parse_from(["test"]).unwrap();
        assert!(t.c.dest.is_none());
        assert!(!t.c.force);
        assert_eq!(t.c.variant, Variant::WhisperTinyEn);
    }

    #[test]
    fn parses_dest_and_force() {
        #[derive(Parser)]
        struct T {
            #[command(flatten)]
            c: InstallModelCommand,
        }
        let t = T::try_parse_from(["test", "--dest", "/opt/x", "--force"]).unwrap();
        assert_eq!(t.c.dest.as_deref(), Some(Path::new("/opt/x")));
        assert!(t.c.force);
    }

    #[test]
    fn parses_speaker_variant() {
        #[derive(Parser)]
        struct T {
            #[command(flatten)]
            c: InstallModelCommand,
        }
        let t = T::try_parse_from(["test", "--variant", "speaker-wespeaker-en"]).unwrap();
        assert_eq!(t.c.variant, Variant::SpeakerWespeakerEn);
    }

    #[test]
    fn parses_whisper_variant_explicit() {
        #[derive(Parser)]
        struct T {
            #[command(flatten)]
            c: InstallModelCommand,
        }
        let t = T::try_parse_from(["test", "--variant", "whisper-tiny.en"]).unwrap();
        assert_eq!(t.c.variant, Variant::WhisperTinyEn);
    }

    #[test]
    fn rejects_unknown_variant() {
        #[derive(Parser)]
        struct T {
            #[command(flatten)]
            c: InstallModelCommand,
        }
        let err = T::try_parse_from(["test", "--variant", "klingon"]);
        assert!(err.is_err(), "unknown variant should fail to parse");
    }

    #[test]
    fn run_with_dest_none_resolves_default_install_dir_from_home() {
        // Covers the `match self.dest { None => spec.default_dir()… }`
        // arm — the priority-3 path that the explicit-dest tests skip.
        // We stage the model files at the default location *under a
        // tempdir HOME* so the idempotent branch returns Ok and we
        // never touch the network or the real user's home.
        let _g = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());

        let default_dir = WHISPER_TINY_EN.default_dir().unwrap();
        stage_complete_whisper_model(&default_dir);

        let cmd = InstallModelCommand {
            dest: None,
            force: false,
            variant: Variant::WhisperTinyEn,
        };
        let mut out: Vec<u8> = Vec::new();
        let result = cmd.run(&mut out);

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }

        result.unwrap();
        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("already installed"), "got: {msg}");
        assert!(
            msg.contains("whisper-tiny.en"),
            "expected resolved default dir in message, got: {msg}"
        );
    }

    #[test]
    fn run_speaker_variant_with_dest_none_resolves_default() {
        let _g = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());

        let default_dir = SPEAKER_WESPEAKER_EN.default_dir().unwrap();
        stage_complete_speaker_model(&default_dir);

        let cmd = InstallModelCommand {
            dest: None,
            force: false,
            variant: Variant::SpeakerWespeakerEn,
        };
        let mut out: Vec<u8> = Vec::new();
        let result = cmd.run(&mut out);

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }

        result.unwrap();
        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("already installed"), "got: {msg}");
        assert!(
            msg.contains("wespeaker-en-voxceleb-resnet34-LM"),
            "expected resolved default dir in message, got: {msg}"
        );
    }

    #[test]
    fn variant_spec_returns_correct_spec() {
        assert_eq!(
            Variant::WhisperTinyEn.spec().variant,
            WHISPER_TINY_EN.variant
        );
        assert_eq!(
            Variant::SpeakerWespeakerEn.spec().variant,
            SPEAKER_WESPEAKER_EN.variant
        );
    }

    #[test]
    fn write_tokenizer_json_emits_loadable_decode_only_tokenizer() {
        // Stage a minimal config.json with a 4-token vocab including
        // the SentencePiece word-start marker ▁ (U+2581).
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        let tok = tmp.path().join("tokenizer.json");
        std::fs::write(
            &cfg,
            r#"{"joint": {"vocabulary": ["<unk>", "▁the", "▁cat", "s"]}}"#,
        )
        .unwrap();

        write_tokenizer_json(&cfg, &tok).unwrap();

        // The synthesised file must load through the same `tokenizers`
        // crate path the backend uses.
        let loaded = tokenizers::Tokenizer::from_file(&tok).expect("tokenizer.json must load");
        assert_eq!(loaded.get_vocab_size(false), 4);
        // Decode an ID sequence — Metaspace decoder strips ▁ and joins
        // with spaces, so [1, 2, 3] → "the cats".
        let text = loaded.decode(&[1_u32, 2, 3], false).unwrap();
        assert_eq!(text, "the cats");
    }

    #[test]
    fn write_tokenizer_json_errors_when_vocab_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        let tok = tmp.path().join("tokenizer.json");
        std::fs::write(&cfg, r#"{"joint": {}}"#).unwrap();
        let Err(err) = write_tokenizer_json(&cfg, &tok) else {
            panic!("expected missing-vocab error");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("missing joint.vocabulary"), "got: {msg}");
    }
}
