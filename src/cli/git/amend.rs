//! Amend command — applies commit message amendments from a YAML file.

use std::path::Path;

use anyhow::{Context, Result};
use clap::Parser;

/// Amend command options.
#[derive(Parser)]
pub struct AmendCommand {
    /// YAML file containing commit amendments.
    #[arg(value_name = "YAML_FILE")]
    pub yaml_file: String,

    /// Allows amending commits that already exist in remote main branches (rewrites published history).
    #[arg(long)]
    pub allow_pushed: bool,
}

impl AmendCommand {
    /// Executes the amend command.
    ///
    /// `repo` is the repository location resolved at the CLI boundary
    /// (`None` = current working directory).
    pub fn execute(self, repo: Option<&std::path::Path>) -> Result<()> {
        use crate::git::AmendmentHandler;

        // Resolve the repo root once; the preflight checks and the amendment
        // handler (including all its `git` subprocesses) anchor to it.
        let repo_root = match repo {
            Some(p) => p.to_path_buf(),
            None => std::env::current_dir().context("Failed to determine current directory")?,
        };

        // Preflight checks: validate prerequisites before any processing
        crate::utils::check_git_repository_at(&repo_root)?;
        crate::utils::check_working_directory_clean_at(&repo_root)?;

        println!("🔄 Starting commit amendment process...");
        println!("📄 Loading amendments from: {}", self.yaml_file);

        // Create amendment handler and apply amendments
        let handler = AmendmentHandler::new(&repo_root)
            .context("Failed to initialize amendment handler")?
            .with_allow_pushed(self.allow_pushed);

        handler
            .apply_amendments(&self.yaml_file)
            .context("Failed to apply amendments")?;

        Ok(())
    }
}

/// Structured output from [`run_amend`] for programmatic consumers (MCP).
#[derive(Debug, Clone)]
pub struct AmendOutcome {
    /// `true` when amendments were applied to the repository; `false` when the
    /// file contained no amendments (nothing to do).
    pub applied: bool,
    /// Number of amendments in the supplied file.
    pub amendment_count: usize,
}

/// Non-interactive core for `omni-dev git commit message amend`.
///
/// The deterministic, apply-messages-from-YAML counterpart to
/// [`crate::cli::git::run_twiddle`]: unlike twiddle it makes no AI call, taking
/// the amendments verbatim. Shared with the MCP `git_amend_commits` tool, which
/// passes the amendments as an inline YAML string rather than a file path.
///
/// Runs the same preflight the CLI command does (repo present, working tree
/// clean), then applies the amendments via [`crate::git::AmendmentHandler`],
/// which refuses commits already in a remote main branch unless `allow_pushed`
/// is set. `repo_path` selects the repository (`None` = current working
/// directory).
pub fn run_amend(
    amendments_yaml: &str,
    allow_pushed: bool,
    repo_path: Option<&Path>,
) -> Result<AmendOutcome> {
    use crate::data::amendments::AmendmentFile;
    use crate::git::AmendmentHandler;

    let repo_root = match repo_path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("Failed to determine current directory")?,
    };

    crate::utils::check_git_repository_at(&repo_root)?;
    crate::utils::check_working_directory_clean_at(&repo_root)?;

    let amendment_file =
        AmendmentFile::from_yaml_str(amendments_yaml).context("Failed to parse amendments YAML")?;
    let amendment_count = amendment_file.amendments.len();

    if amendment_count == 0 {
        return Ok(AmendOutcome {
            applied: false,
            amendment_count: 0,
        });
    }

    AmendmentHandler::new(&repo_root)
        .context("Failed to initialize amendment handler")?
        .with_allow_pushed(allow_pushed)
        .apply_amendment_file(&amendment_file)
        .context("Failed to apply amendments")?;

    Ok(AmendOutcome {
        applied: true,
        amendment_count,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod run_amend_tests {
    use super::*;
    use std::process::Command;

    /// Runs `git` in `dir` with a deterministic identity, asserting success.
    fn git_in(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(output.status.success(), "git {args:?} failed: {stderr}");
    }

    /// A repo on `main` with one local (unpushed) commit — no remote, so nothing
    /// is in a remote main branch and amending needs no `--allow-pushed`.
    ///
    /// Identity and no-signing are set as **persistent repo-local config** (not
    /// just inline `-c` on the seed commit) because `AmendmentHandler`'s own
    /// `git commit --amend` runs without those flags, so it must find an identity
    /// on the repo — CI runners have no global `user.email`/`user.name`.
    fn repo_with_local_commit() -> tempfile::TempDir {
        let tmp_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&tmp_root).unwrap();
        let work = tempfile::tempdir_in(&tmp_root).unwrap();
        git_in(work.path(), &["init", "-b", "main"]);
        git_in(work.path(), &["config", "user.email", "test@example.com"]);
        git_in(work.path(), &["config", "user.name", "Test"]);
        git_in(work.path(), &["config", "commit.gpgsign", "false"]);
        std::fs::write(work.path().join("file.txt"), "content").unwrap();
        git_in(work.path(), &["add", "."]);
        git_in(work.path(), &["commit", "-m", "original message"]);
        work
    }

    fn head_sha(dir: &Path) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn head_message(dir: &Path) -> String {
        let out = Command::new("git")
            .current_dir(dir)
            .args(["log", "-1", "--format=%B"])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn run_amend_applies_inline_yaml() {
        let work = repo_with_local_commit();
        let sha = head_sha(work.path());
        let yaml = format!(
            "amendments:\n  - commit: {sha}\n    message: \"feat: rewritten inline\"\n    summary: \"\"\n"
        );

        let outcome = run_amend(&yaml, false, Some(work.path())).unwrap();
        assert!(outcome.applied);
        assert_eq!(outcome.amendment_count, 1);
        assert_eq!(head_message(work.path()), "feat: rewritten inline");
    }

    #[test]
    fn run_amend_empty_list_is_noop() {
        let work = repo_with_local_commit();
        let before = head_sha(work.path());

        let outcome = run_amend("amendments: []\n", false, Some(work.path())).unwrap();
        assert!(!outcome.applied);
        assert_eq!(outcome.amendment_count, 0);
        // The commit is untouched.
        assert_eq!(head_sha(work.path()), before);
    }

    #[test]
    fn run_amend_rejects_invalid_yaml() {
        let work = repo_with_local_commit();
        let err = run_amend("not: [valid", false, Some(work.path())).unwrap_err();
        assert!(format!("{err:#}").contains("amendments"));
    }
}
