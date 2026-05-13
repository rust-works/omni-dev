//! `omni-dev voice install-model` — one-time fetch of the Whisper
//! `tiny.en` config, tokenizer, and safetensors weights into the
//! conventional install location.
//!
//! Bumps the model-download cost to install time rather than transcribe
//! time, so network failures surface explicitly when the user opts in to
//! installing rather than silently on first `voice transcribe`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use hf_hub::{api::sync::Api, Repo, RepoType};

use crate::voice::models::{
    default_whisper_model_dir, required_files_in, MODEL_ID, REQUIRED_FILES, REVISION,
};

/// Downloads the Whisper `tiny.en` model files from HuggingFace into the
/// conventional install location at
/// `~/.omni-dev/voice/models/whisper-tiny.en/` (or `--dest` to override).
///
/// Idempotent: if every required file is already present and non-empty,
/// the command prints a "model already installed" line and exits 0. Pass
/// `--force` to re-download anyway.
#[derive(Parser)]
pub struct InstallModelCommand {
    /// Override the install directory. Defaults to
    /// `~/.omni-dev/voice/models/whisper-tiny.en/`.
    #[arg(long)]
    pub dest: Option<PathBuf>,

    /// Re-download even if all required files are already present.
    #[arg(long)]
    pub force: bool,
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
        let dest = match self.dest {
            Some(p) => p,
            None => default_whisper_model_dir()
                .ok_or_else(|| anyhow!("could not determine home directory; pass --dest <path>"))?,
        };

        if !self.force && all_present(&dest) {
            writeln!(w, "model already installed at {}", dest.display())?;
            return Ok(());
        }

        download_into(&dest, w)
    }
}

fn all_present(dir: &Path) -> bool {
    required_files_in(dir)
        .iter()
        .all(|p| p.is_file() && p.metadata().is_ok_and(|m| m.len() > 0))
}

fn download_into<W: Write>(dest: &Path, w: &mut W) -> Result<()> {
    writeln!(
        w,
        "Installing {MODEL_ID} (revision {REVISION}) -> {}",
        dest.display()
    )?;
    std::fs::create_dir_all(dest)
        .with_context(|| format!("create install directory at {}", dest.display()))?;

    let api = Api::new().context("initialise HuggingFace Hub client")?;
    let repo = api.repo(Repo::with_revision(
        MODEL_ID.to_string(),
        RepoType::Model,
        REVISION.to_string(),
    ));

    for file in REQUIRED_FILES {
        let start = Instant::now();
        write!(w, "  fetching {file}... ")?;
        w.flush()?;
        let downloaded = repo.get(file).with_context(|| {
            format!(
                "download {file} from {MODEL_ID} (revision {REVISION}). \
                 Check your network or set HTTPS_PROXY"
            )
        })?;
        let target = dest.join(file);
        atomic_install(&downloaded, &target).with_context(|| {
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

    writeln!(w, "Whisper {MODEL_ID} installed at {}", dest.display())?;
    Ok(())
}

/// Copies `from` into `to` via a temp file sibling + rename so a partial
/// download never leaves a half-written file at the destination.
fn atomic_install(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir {}", parent.display()))?;
    }
    let file_name = to
        .file_name()
        .ok_or_else(|| anyhow!("destination path has no file name: {}", to.display()))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(".part");
    let tmp = to.with_file_name(tmp_name);
    std::fs::copy(from, &tmp)
        .with_context(|| format!("copy {} -> {}", from.display(), tmp.display()))?;
    std::fs::rename(&tmp, to)
        .with_context(|| format!("rename {} -> {}", tmp.display(), to.display()))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
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

    fn stage_complete_model(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for f in REQUIRED_FILES {
            std::fs::write(dir.join(f), b"placeholder").unwrap();
        }
    }

    #[test]
    fn idempotent_when_all_files_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        stage_complete_model(tmp.path());

        let cmd = InstallModelCommand {
            dest: Some(tmp.path().to_path_buf()),
            force: false,
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
        assert!(!all_present(tmp.path()));
    }

    #[test]
    fn atomic_install_replaces_target() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::write(&src, b"hello").unwrap();
        std::fs::write(&dst, b"old").unwrap();
        atomic_install(&src, &dst).unwrap();
        let got = std::fs::read(&dst).unwrap();
        assert_eq!(got, b"hello");
        // No leftover temp file.
        let leftover = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().ends_with(".part"));
        assert!(!leftover, "atomic_install must not leave .part files");
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
    fn run_with_dest_none_resolves_default_install_dir_from_home() {
        // Covers the `match self.dest { None => default_whisper_model_dir()… }`
        // arm — the priority-3 path that the explicit-dest tests skip. We
        // stage the model files at the default location *under a tempdir
        // HOME* so the idempotent branch returns Ok and we never touch the
        // network or the real user's home.
        let _g = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());

        let default_dir = default_whisper_model_dir().unwrap();
        stage_complete_model(&default_dir);

        let cmd = InstallModelCommand {
            dest: None,
            force: false,
        };
        let mut out: Vec<u8> = Vec::new();
        let result = cmd.run(&mut out);

        // Restore HOME before asserting so a failed assertion doesn't
        // poison subsequent tests.
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
}
