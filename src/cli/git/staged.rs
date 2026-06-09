//! `omni-dev git commit message staged` — generate a Conventional Commits
//! message from staged changes via the configured AI backend and (by default)
//! commit them.
//!
//! Default behaviour mirrors `git commit -m <message>` so user-installed
//! `pre-commit` / `commit-msg` hooks fire normally. Pass `--print-only` to
//! print the generated message to stdout without committing.

use anyhow::{Context, Result};
use clap::Parser;
use std::process::{Command, Stdio};

use super::parse_beta_header;
use crate::data::context::ScopeDefinition;

/// `omni-dev git commit message staged` CLI command.
#[derive(Parser)]
pub struct StagedCommand {
    /// Print the generated message to stdout instead of committing.
    #[arg(long)]
    pub print_only: bool,

    /// Claude API model to use (if not specified, uses settings or default).
    #[arg(long)]
    pub model: Option<String>,

    /// Beta header to send with API requests (format: key:value).
    /// Only sent if the model supports it in the registry.
    #[arg(long, value_name = "KEY:VALUE")]
    pub beta_header: Option<String>,

    /// Override the context directory used to load project scopes.
    #[arg(long, value_name = "DIR")]
    pub context_dir: Option<std::path::PathBuf>,
}

/// Outcome of a staged-commit run.
#[derive(Debug, Clone)]
pub struct StagedOutcome {
    /// The generated commit message (trimmed of surrounding whitespace).
    pub message: String,
    /// `true` when the commit was applied to the repository; `false` for
    /// `--print-only` or any path that did not run `git commit`.
    pub applied: bool,
}

impl StagedCommand {
    /// Executes the staged command.
    ///
    /// `repo` is the repository location resolved at the CLI boundary
    /// (`None` = current working directory).
    pub async fn execute(self, repo: Option<&std::path::Path>) -> Result<()> {
        let beta = self
            .beta_header
            .as_deref()
            .map(parse_beta_header)
            .transpose()?;
        let _ = run_staged(
            self.print_only,
            self.model,
            beta,
            self.context_dir.as_deref(),
            repo,
        )
        .await?;
        Ok(())
    }
}

/// Public entry point for the staged-commit command.
///
/// Mirrors [`crate::cli::git::run_twiddle`]'s shape so the MCP server can wrap
/// it the same way: pin the CWD, run AI preflight, build the client, delegate
/// to the test-injectable inner [`run_staged_with_client`].
pub async fn run_staged(
    print_only: bool,
    model: Option<String>,
    beta_header: Option<(String, String)>,
    context_dir: Option<&std::path::Path>,
    repo_path: Option<&std::path::Path>,
) -> Result<StagedOutcome> {
    // Resolve the repo root once (the CWD is the default when no path is
    // injected); every git subprocess and config/scopes read below anchors to
    // it, so nothing deeper reads the ambient CWD.
    let repo_root = match repo_path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("Failed to determine current directory")?,
    };
    let repo_root = repo_root.as_path();

    if !has_staged_changes(repo_root)? {
        anyhow::bail!("no staged changes — stage files with `git add` before running this command");
    }

    crate::utils::check_ai_command_prerequisites(model.as_deref(), repo_root)?;
    let claude_client = crate::claude::create_default_claude_client(model, beta_header).await?;

    let resolved_context_dir =
        crate::claude::context::resolve_context_dir_at(context_dir, repo_root);
    let valid_scopes =
        crate::claude::context::load_project_scopes(&resolved_context_dir, repo_root);

    run_staged_with_client(print_only, &valid_scopes, &claude_client, repo_root).await
}

/// Test-injectable core of [`run_staged`].
///
/// Assumes the caller has already:
/// - Verified the working directory contains staged changes.
/// - Verified AI credentials.
/// - Constructed a fully initialised `ClaudeClient`.
/// - Loaded `valid_scopes` (may be empty).
pub(crate) async fn run_staged_with_client(
    print_only: bool,
    valid_scopes: &[ScopeDefinition],
    claude_client: &crate::claude::client::ClaudeClient,
    repo_root: &std::path::Path,
) -> Result<StagedOutcome> {
    let diff = read_staged_diff(repo_root)?;
    let system = crate::claude::prompts::generate_staged_commit_system_prompt(valid_scopes);
    let user = crate::claude::prompts::generate_staged_commit_user_prompt(&diff);

    let raw = claude_client.send_message(&system, &user).await?;
    let message = raw.trim().to_string();

    if message.is_empty() {
        anyhow::bail!("AI returned an empty commit message");
    }

    if print_only {
        println!("{message}");
        return Ok(StagedOutcome {
            message,
            applied: false,
        });
    }

    commit_with_message(&message, repo_root)?;
    Ok(StagedOutcome {
        message,
        applied: true,
    })
}

/// Returns `true` if `git diff --cached --quiet` reports staged changes.
///
/// Exit codes per `git diff --quiet`:
/// - `0` ⇒ no diff (nothing staged)
/// - `1` ⇒ diff present (staged changes exist)
/// - other ⇒ a real error (not in a repo, permission denied, etc.)
fn has_staged_changes(repo_root: &std::path::Path) -> Result<bool> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["diff", "--cached", "--quiet"])
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("Failed to execute git diff --cached --quiet")?;
    match output.status.code() {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        Some(code) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git diff --cached --quiet exited with code {code}: {stderr}")
        }
        None => anyhow::bail!("git diff --cached --quiet was terminated by a signal"),
    }
}

/// Reads the staged diff via `git diff --cached`.
fn read_staged_diff(repo_root: &std::path::Path) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["diff", "--cached"])
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("Failed to execute git diff --cached")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff --cached failed: {stderr}");
    }
    String::from_utf8(output.stdout).context("git diff --cached produced non-UTF-8 output")
}

/// Commits staged changes via `git commit -m <msg>` as a subprocess.
///
/// Uses `.status()` so stdout/stderr are inherited from the parent — this is
/// deliberate: it lets the user see hook output live and confirms hooks
/// (`pre-commit`, `commit-msg`) fire normally, which `libgit2`'s
/// `repo.commit()` would bypass.
///
/// Stdin is explicitly `Stdio::null()` so neither `git commit` nor any hook
/// can block reading from an inherited stdin fd. On CI runners (Linux), an
/// inherited stdin from `cargo test` can produce indefinite waits that don't
/// reproduce on developer terminals.
fn commit_with_message(message: &str, repo_root: &std::path::Path) -> Result<()> {
    let status = Command::new("git")
        .current_dir(repo_root)
        .args(["commit", "-m", message])
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .status()
        .context("Failed to execute git commit -m")?;
    if !status.success() {
        anyhow::bail!("git commit failed (exit status: {status})");
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::claude::client::ClaudeClient;
    use crate::claude::test_utils::ConfigurableMockAiClient;
    use git2::{Repository, Signature};

    /// Creates an empty repo with no commits and no staged content.
    fn init_empty_repo() -> tempfile::TempDir {
        let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&tmp_root).unwrap();
        let temp_dir = tempfile::tempdir_in(&tmp_root).unwrap();
        let repo = Repository::init(temp_dir.path()).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Test").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();
        cfg.set_str("commit.gpgsign", "false").unwrap();
        temp_dir
    }

    /// Creates a repo with a baseline commit, then stages a new file so
    /// `git diff --cached` is non-empty.
    fn init_repo_with_staged_change() -> tempfile::TempDir {
        let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&tmp_root).unwrap();
        let temp_dir = tempfile::tempdir_in(&tmp_root).unwrap();
        let repo = Repository::init(temp_dir.path()).unwrap();
        {
            let mut cfg = repo.config().unwrap();
            cfg.set_str("user.name", "Test").unwrap();
            cfg.set_str("user.email", "test@example.com").unwrap();
            cfg.set_str("commit.gpgsign", "false").unwrap();
        }
        // Baseline commit so HEAD exists.
        let signature = Signature::now("Test", "test@example.com").unwrap();
        std::fs::write(temp_dir.path().join("README"), "baseline\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(std::path::Path::new("README")).unwrap();
        idx.write().unwrap();
        let tree_id = idx.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(
            Some("HEAD"),
            &signature,
            &signature,
            "chore: baseline",
            &tree,
            &[],
        )
        .unwrap();

        // Stage a new file so the diff is non-empty.
        std::fs::write(temp_dir.path().join("new.rs"), "fn marker_xyz() {}\n").unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(std::path::Path::new("new.rs")).unwrap();
        idx.write().unwrap();

        temp_dir
    }

    fn head_message(repo_path: &std::path::Path) -> String {
        let repo = Repository::open(repo_path).unwrap();
        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        commit.message().unwrap().to_string()
    }

    fn head_oid(repo_path: &std::path::Path) -> String {
        let repo = Repository::open(repo_path).unwrap();
        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        commit.id().to_string()
    }

    #[tokio::test]
    async fn run_staged_errors_when_nothing_staged() {
        let temp_dir = init_empty_repo();
        // `has_staged_changes` is anchored to the injected repo (`.current_dir`),
        // so this empty repo bails regardless of whether the process CWD has
        // staged changes.
        let err = run_staged(true, None, None, None, Some(temp_dir.path()))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("no staged changes"),
            "expected 'no staged changes' error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn run_staged_with_client_print_only_does_not_commit() {
        let temp_dir = init_repo_with_staged_change();
        let head_before = head_oid(temp_dir.path());

        let mock = ConfigurableMockAiClient::new(vec![Ok("feat(foo): add bar".to_string())]);
        let client = ClaudeClient::new(Box::new(mock));

        let outcome = run_staged_with_client(true, &[], &client, temp_dir.path())
            .await
            .unwrap();
        assert!(!outcome.applied, "print_only must not apply");
        assert_eq!(outcome.message, "feat(foo): add bar");

        let head_after = head_oid(temp_dir.path());
        assert_eq!(head_before, head_after, "HEAD must be unchanged");
    }

    #[tokio::test]
    async fn run_staged_with_client_commits_on_default() {
        let temp_dir = init_repo_with_staged_change();
        let head_before = head_oid(temp_dir.path());

        let mock = ConfigurableMockAiClient::new(vec![Ok("feat(foo): add marker".to_string())]);
        let client = ClaudeClient::new(Box::new(mock));

        let outcome = run_staged_with_client(false, &[], &client, temp_dir.path())
            .await
            .unwrap();
        assert!(outcome.applied, "default mode must commit");

        let head_after = head_oid(temp_dir.path());
        assert_ne!(head_before, head_after, "HEAD must advance");

        let msg = head_message(temp_dir.path());
        assert!(
            msg.starts_with("feat(foo): add marker"),
            "expected AI message at HEAD, got: {msg:?}"
        );
    }

    #[tokio::test]
    async fn run_staged_propagates_ai_failure() {
        let temp_dir = init_repo_with_staged_change();
        let head_before = head_oid(temp_dir.path());

        // Empty response queue → mock returns Err on first call.
        let mock = ConfigurableMockAiClient::new(vec![]);
        let client = ClaudeClient::new(Box::new(mock));

        let err = run_staged_with_client(false, &[], &client, temp_dir.path())
            .await
            .unwrap_err();
        let _ = err;

        let head_after = head_oid(temp_dir.path());
        assert_eq!(head_before, head_after, "HEAD must not advance on failure");
    }

    #[tokio::test]
    async fn run_staged_with_client_trims_ai_response_whitespace() {
        let temp_dir = init_repo_with_staged_change();

        let mock = ConfigurableMockAiClient::new(vec![Ok("  feat(x): y  \n\n".to_string())]);
        let client = ClaudeClient::new(Box::new(mock));

        let outcome = run_staged_with_client(true, &[], &client, temp_dir.path())
            .await
            .unwrap();
        assert_eq!(outcome.message, "feat(x): y");
    }

    #[tokio::test]
    async fn run_staged_with_client_empty_ai_response_errors() {
        let temp_dir = init_repo_with_staged_change();

        let mock = ConfigurableMockAiClient::new(vec![Ok("   \n\n".to_string())]);
        let client = ClaudeClient::new(Box::new(mock));

        let err = run_staged_with_client(false, &[], &client, temp_dir.path())
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("empty"),
            "expected 'empty' error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn run_staged_invokes_git_commit_subprocess_so_hooks_fire() {
        let temp_dir = init_repo_with_staged_change();
        let head_before = head_oid(temp_dir.path());

        // Install a commit-msg hook that always fails. If we go through real
        // `git commit`, the hook fires and the commit is rejected. If we
        // were using libgit2's repo.commit(), hooks would be bypassed.
        let hook_path = temp_dir.path().join(".git/hooks/commit-msg");
        std::fs::write(&hook_path, "#!/bin/sh\necho REJECTED-BY-HOOK >&2\nexit 1\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&hook_path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&hook_path, perms).unwrap();
        }

        let mock = ConfigurableMockAiClient::new(vec![Ok("feat(x): y".to_string())]);
        let client = ClaudeClient::new(Box::new(mock));

        let err = run_staged_with_client(false, &[], &client, temp_dir.path())
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("git commit failed"),
            "expected commit-failure error message, got: {msg}"
        );

        let head_after = head_oid(temp_dir.path());
        assert_eq!(
            head_before, head_after,
            "HEAD must not advance when commit-msg hook rejects"
        );
    }

    #[tokio::test]
    async fn run_staged_passes_valid_scopes_into_prompt() {
        let temp_dir = init_repo_with_staged_change();

        let mock = ConfigurableMockAiClient::new(vec![Ok("feat(cli): add".to_string())]);
        let prompts = mock.prompt_handle();
        let client = ClaudeClient::new(Box::new(mock));

        let scopes = vec![ScopeDefinition {
            name: "cli".to_string(),
            description: "CLI module".to_string(),
            examples: Vec::new(),
            file_patterns: Vec::new(),
        }];

        let _ = run_staged_with_client(true, &scopes, &client, temp_dir.path())
            .await
            .unwrap();
        let recorded = prompts.prompts();
        assert_eq!(recorded.len(), 1, "exactly one AI call");
        let (system, _user) = &recorded[0];
        assert!(
            system.contains("VALID SCOPES FOR THIS PROJECT"),
            "scopes section missing from system prompt"
        );
        assert!(system.contains("`cli`: CLI module"));
    }

    #[test]
    fn staged_outcome_clone_and_debug() {
        let outcome = StagedOutcome {
            message: "feat: x".to_string(),
            applied: true,
        };
        let cloned = outcome.clone();
        assert_eq!(format!("{outcome:?}"), format!("{cloned:?}"));
    }

    // Drives `StagedCommand::execute()` through its no-staged-changes bail.
    // The command's `execute` delegates to `run_staged`, which short-circuits
    // before any AI credential check, so this exercises the dispatch wiring
    // without needing real AI credentials.
    #[tokio::test]
    async fn staged_command_execute_bails_when_nothing_staged() {
        let temp_dir = init_empty_repo();
        let cmd = StagedCommand {
            print_only: true,
            model: None,
            beta_header: None,
            context_dir: None,
        };
        let err = cmd.execute(Some(temp_dir.path())).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("no staged changes"),
            "expected 'no staged changes' error from execute(), got: {msg}"
        );
    }

    // `execute()` parses `--beta-header` before any other work; an invalid
    // value should error out with a clear "Invalid --beta-header" message.
    #[tokio::test]
    async fn staged_command_execute_rejects_malformed_beta_header() {
        let temp_dir = init_empty_repo();
        let cmd = StagedCommand {
            print_only: true,
            model: None,
            beta_header: Some("no-colon-here".to_string()),
            context_dir: None,
        };
        let err = cmd.execute(Some(temp_dir.path())).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Invalid --beta-header"),
            "expected beta-header parse error, got: {msg}"
        );
    }

    /// "No silent mix" guard: `read_staged_diff` reads the staged diff from the
    /// INJECTED repo, not the process CWD. We stage a uniquely-marked file in
    /// the temp repo, run with that repo injected (the process CWD is the
    /// omni-dev checkout), and assert the marker reached the AI prompt.
    #[tokio::test]
    async fn run_staged_with_client_reads_diff_from_injected_repo() {
        let temp_dir = init_repo_with_staged_change();

        let mock = ConfigurableMockAiClient::new(vec![Ok("feat: x".to_string())]);
        let prompts = mock.prompt_handle();
        let client = ClaudeClient::new(Box::new(mock));

        let _ = run_staged_with_client(true, &[], &client, temp_dir.path())
            .await
            .unwrap();

        let recorded = prompts.prompts();
        assert_eq!(recorded.len(), 1, "exactly one AI call");
        let (_system, user) = &recorded[0];
        assert!(
            user.contains("marker_xyz"),
            "staged diff from the injected repo must reach the prompt: {user}"
        );
    }
}
