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

use crate::voice::models::{ModelSource, ModelSpec, SPEAKER_WESPEAKER_EN, WHISPER_TINY_EN};

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
    /// Wespeaker `resnet34_LM` English-only speaker embedding (ADR-0034).
    #[value(name = "speaker-wespeaker-en")]
    SpeakerWespeakerEn,
}

impl Variant {
    /// Returns the [`ModelSpec`] for this variant.
    pub fn spec(self) -> &'static ModelSpec {
        match self {
            Self::WhisperTinyEn => &WHISPER_TINY_EN,
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
        let home = dirs::home_dir();
        self.run_in(w, home.as_deref())
    }

    /// `run` with the home directory injected rather than read from
    /// `dirs::home_dir()`/`HOME`. The `dest: None` default-dir branch
    /// resolves against `home`, so tests can exercise it against a
    /// tempdir base without mutating the process-global `HOME` (the race
    /// behind #978).
    fn run_in<W: Write>(self, w: &mut W, home: Option<&Path>) -> Result<()> {
        let spec = self.variant.spec();
        let dest = if let Some(p) = self.dest {
            p
        } else {
            let home = home
                .ok_or_else(|| anyhow!("could not determine home directory; pass --dest <path>"))?;
            spec.default_dir_from(home)
        };

        if !self.force && all_present(spec, &dest) {
            writeln!(w, "model already installed at {}", dest.display())?;
            return Ok(());
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
        // Covers the `match self.dest { None => spec.default_dir_from()… }`
        // arm — the priority-3 path that the explicit-dest tests skip.
        // The home base is injected via `run_in`, so we stage the model
        // files at the default location beneath a tempdir without
        // mutating the process-global `HOME` (the #978 race).
        let tmp = tempfile::TempDir::new().unwrap();
        let default_dir = WHISPER_TINY_EN.default_dir_from(tmp.path());
        stage_complete_whisper_model(&default_dir);

        let cmd = InstallModelCommand {
            dest: None,
            force: false,
            variant: Variant::WhisperTinyEn,
        };
        let mut out: Vec<u8> = Vec::new();
        cmd.run_in(&mut out, Some(tmp.path())).unwrap();

        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("already installed"), "got: {msg}");
        assert!(
            msg.contains("whisper-tiny.en"),
            "expected resolved default dir in message, got: {msg}"
        );
    }

    #[test]
    fn run_speaker_variant_with_dest_none_resolves_default() {
        let tmp = tempfile::TempDir::new().unwrap();
        let default_dir = SPEAKER_WESPEAKER_EN.default_dir_from(tmp.path());
        stage_complete_speaker_model(&default_dir);

        let cmd = InstallModelCommand {
            dest: None,
            force: false,
            variant: Variant::SpeakerWespeakerEn,
        };
        let mut out: Vec<u8> = Vec::new();
        cmd.run_in(&mut out, Some(tmp.path())).unwrap();

        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("already installed"), "got: {msg}");
        assert!(
            msg.contains("wespeaker-en-voxceleb-resnet34-LM"),
            "expected resolved default dir in message, got: {msg}"
        );
    }

    #[test]
    fn run_in_errors_when_home_unavailable_and_dest_none() {
        // The `home: None` arm of the default-dir branch — deterministically
        // reachable via `run_in` without simulating a missing `HOME`.
        let cmd = InstallModelCommand {
            dest: None,
            force: false,
            variant: Variant::WhisperTinyEn,
        };
        let mut out: Vec<u8> = Vec::new();
        let err = cmd.run_in(&mut out, None).unwrap_err();
        assert!(
            format!("{err:#}").contains("could not determine home directory"),
            "got: {err:#}"
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
}
