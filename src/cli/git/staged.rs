//! `omni-dev git commit message staged` — generate a Conventional Commits
//! message from staged changes via the configured AI backend and (by default)
//! commit them.
//!
//! Default behaviour mirrors `git commit -m <message>` so user-installed
//! `pre-commit` / `commit-msg` hooks fire normally. Pass `--print-only` to
//! print the generated message to stdout without committing, or `--no-ai`
//! for a deterministic (no AI, no network) `type(scope): ` skeleton instead
//! of a full AI-drafted message — always print-only, regardless of
//! `--print-only`.

use anyhow::{Context, Result};
use clap::Parser;
use std::process::{Command, Stdio};

use crate::data::context::ScopeDefinition;
use crate::git::commit::FileChanges;

/// `omni-dev git commit message staged` CLI command.
///
/// Model/beta-header selection uses the global `--model`/`--beta-header`
/// flags (propagated as `OMNI_DEV_MODEL`/`OMNI_DEV_BETA_HEADER`) and the
/// per-backend env chain; the only subcommand-local flags are `--print-only`,
/// `--context-dir`, and `--no-ai`.
#[derive(Parser)]
pub struct StagedCommand {
    /// Print the generated message to stdout instead of committing.
    #[arg(long)]
    pub print_only: bool,

    /// Override the context directory used to load project scopes.
    #[arg(long, value_name = "DIR")]
    pub context_dir: Option<std::path::PathBuf>,

    /// Skip the AI backend entirely and print a deterministic `type(scope): `
    /// skeleton derived from the staged diff's changed files — no AI, no
    /// network, no credentials required. The scope is resolved the same way
    /// `lint --suggest`/`--fix` do (`resolve_scope` against
    /// `.omni-dev/scopes.yaml` + ecosystem defaults); the type is a
    /// best-effort heuristic, not validated against an enumerable list.
    /// Always prints and never commits, regardless of `--print-only` — a
    /// bare skeleton has no description, so it's never a complete message
    /// to commit.
    #[arg(long)]
    pub no_ai: bool,
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
        let _ = run_staged(
            self.print_only,
            self.no_ai,
            None,
            None,
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
/// it the same way: resolve the repo root (the injected path, or the CWD as the
/// default), run AI preflight, build the client, and delegate to the
/// test-injectable inner [`run_staged_with_client`].
///
/// `no_ai` skips AI entirely (no credential preflight, no client, no network
/// call) and returns a deterministic `type(scope): ` skeleton instead — see
/// [`run_staged_no_ai`].
pub async fn run_staged(
    print_only: bool,
    no_ai: bool,
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

    let resolved_context_dir =
        crate::claude::context::resolve_context_dir_at(context_dir, repo_root);
    let valid_scopes =
        crate::claude::context::load_project_scopes(&resolved_context_dir, repo_root);

    if no_ai {
        return run_staged_no_ai(repo_root, &valid_scopes);
    }

    crate::utils::check_ai_command_prerequisites(model.as_deref(), repo_root)?;
    let claude_client = crate::claude::create_default_claude_client(model, beta_header).await?;

    run_staged_with_client(print_only, &valid_scopes, &claude_client, repo_root).await
}

/// Deterministic (no-AI) core of [`run_staged`]'s `no_ai` path.
///
/// Reads the staged file list via `git diff --cached --name-status`, resolves
/// a `type(scope): ` skeleton via [`suggest_staged_skeleton`], prints it, and
/// always returns `applied: false` — a bare skeleton has no description, so
/// it is never committed, regardless of `--print-only`.
fn run_staged_no_ai(
    repo_root: &std::path::Path,
    valid_scopes: &[ScopeDefinition],
) -> Result<StagedOutcome> {
    let files = read_staged_files(repo_root)?;
    let message = suggest_staged_skeleton(&files, valid_scopes);
    println!("{message}");
    Ok(StagedOutcome {
        message,
        applied: false,
    })
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

/// Parses `git diff --cached --name-status` output into [`FileChanges`].
///
/// Each line is `status\tpath` (`A`/`M`/`D`/...), or `status\told\tnew` for a
/// rename/copy (`R100`/`C100`/...) — the *last* tab-separated field is always
/// the file's current path. Pure and unit-testable without a git subprocess;
/// [`read_staged_files`] is the thin subprocess wrapper around it.
fn parse_name_status(text: &str) -> FileChanges {
    let mut file_list = Vec::new();
    let mut files_added = 0;
    let mut files_deleted = 0;

    for line in text.lines().filter(|l| !l.is_empty()) {
        let mut fields = line.split('\t');
        let Some(status) = fields.next() else {
            continue;
        };
        let Some(file) = fields.next_back() else {
            continue;
        };
        let status_char = status.chars().next().unwrap_or('?');
        match status_char {
            'A' => files_added += 1,
            'D' => files_deleted += 1,
            _ => {}
        }
        file_list.push(crate::git::commit::FileChange {
            status: status_char.to_string(),
            file: file.to_string(),
        });
    }

    FileChanges {
        total_files: file_list.len(),
        files_added,
        files_deleted,
        file_list,
    }
}

/// Reads the staged file list via `git diff --cached --name-status`.
fn read_staged_files(repo_root: &std::path::Path) -> Result<FileChanges> {
    let output = Command::new("git")
        .current_dir(repo_root)
        .args(["diff", "--cached", "--name-status"])
        .stdin(Stdio::null())
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .context("Failed to execute git diff --cached --name-status")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff --cached --name-status failed: {stderr}");
    }
    let text = String::from_utf8(output.stdout)
        .context("git diff --cached --name-status produced non-UTF-8 output")?;
    Ok(parse_name_status(&text))
}

/// Builds a deterministic `type(scope): ` (or `type: ` when no scope
/// resolves) skeleton from `files` — a prefix only, never a synthesized
/// description. The type comes from
/// [`crate::git::commit::detect_commit_type_from_message`] with an empty
/// message (there's no message yet to seed from, so it falls straight
/// through to the file-pattern heuristics); the scope from
/// [`crate::git::resolve_scope`], the same deterministic resolution
/// `lint --suggest`/`--fix` use.
fn suggest_staged_skeleton(files: &FileChanges, valid_scopes: &[ScopeDefinition]) -> String {
    let commit_type = crate::git::commit::detect_commit_type_from_message("", files);
    let file_refs: Vec<&str> = files.file_list.iter().map(|f| f.file.as_str()).collect();
    match crate::git::resolve_scope(&file_refs, valid_scopes) {
        Some(scope) => format!("{commit_type}({scope}): "),
        None => format!("{commit_type}: "),
    }
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
        let err = run_staged(true, false, None, None, None, Some(temp_dir.path()))
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
            context_dir: None,
            no_ai: false,
        };
        let err = cmd.execute(Some(temp_dir.path())).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase().contains("no staged changes"),
            "expected 'no staged changes' error from execute(), got: {msg}"
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

    // ── --no-ai (#1564) ──────────────────────────────────────────────

    /// Creates a repo with a baseline commit, then stages a new `Cargo.toml`
    /// so a `cargo`-scoped skeleton can be resolved.
    fn init_repo_with_staged_cargo_toml() -> tempfile::TempDir {
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

        std::fs::write(
            temp_dir.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\n",
        )
        .unwrap();
        let mut idx = repo.index().unwrap();
        idx.add_path(std::path::Path::new("Cargo.toml")).unwrap();
        idx.write().unwrap();

        temp_dir
    }

    /// Writes a `cargo` scope (matching `Cargo.toml`/`Cargo.lock`) to
    /// `context_dir/scopes.yaml`.
    fn write_cargo_scope(context_dir: &std::path::Path) {
        std::fs::create_dir_all(context_dir).unwrap();
        std::fs::write(
            context_dir.join("scopes.yaml"),
            "scopes:\n  - name: cargo\n    description: Cargo files\n    examples: []\n    file_patterns:\n      - Cargo.toml\n      - Cargo.lock\n",
        )
        .unwrap();
    }

    #[test]
    fn parse_name_status_added_file() {
        let files = parse_name_status("A\tCargo.toml\n");
        assert_eq!(files.total_files, 1);
        assert_eq!(files.files_added, 1);
        assert_eq!(files.files_deleted, 0);
        assert_eq!(files.file_list[0].status, "A");
        assert_eq!(files.file_list[0].file, "Cargo.toml");
    }

    #[test]
    fn parse_name_status_modified_file() {
        let files = parse_name_status("M\tsrc/main.rs\n");
        assert_eq!(files.files_added, 0);
        assert_eq!(files.files_deleted, 0);
        assert_eq!(files.file_list[0].status, "M");
    }

    #[test]
    fn parse_name_status_deleted_file() {
        let files = parse_name_status("D\told.rs\n");
        assert_eq!(files.files_deleted, 1);
        assert_eq!(files.file_list[0].status, "D");
    }

    #[test]
    fn parse_name_status_rename_uses_new_path_as_file() {
        let files = parse_name_status("R100\told.rs\tnew.rs\n");
        assert_eq!(files.file_list.len(), 1);
        assert_eq!(files.file_list[0].status, "R");
        assert_eq!(files.file_list[0].file, "new.rs");
    }

    #[test]
    fn parse_name_status_blank_lines_ignored() {
        let files = parse_name_status("A\ta.rs\n\nM\tb.rs\n");
        assert_eq!(files.total_files, 2);
    }

    /// A line with a status but no tab-separated filename (malformed
    /// `git diff --name-status` output) has no file to record — skipped
    /// rather than panicking or fabricating a path.
    #[test]
    fn parse_name_status_line_without_tab_is_skipped() {
        let files = parse_name_status("A\ta.rs\nA\nM\tb.rs\n");
        assert_eq!(files.total_files, 2);
        assert_eq!(files.file_list[0].file, "a.rs");
        assert_eq!(files.file_list[1].file, "b.rs");
    }

    /// `read_staged_files` surfaces a non-zero `git diff --cached
    /// --name-status` exit as an error rather than silently returning an
    /// empty file list. Unlike every other fixture in this module, this one
    /// deliberately does NOT use `tempdir_in(CARGO_MANIFEST_DIR/tmp)` — that
    /// convention nests the fixture inside omni-dev's own working tree, so
    /// `git`'s upward repository discovery would find omni-dev's real
    /// `.git` and the command would succeed trivially. This needs a
    /// directory outside any git repository, so it uses the system temp
    /// dir instead.
    #[test]
    fn read_staged_files_errors_when_git_command_fails() {
        let temp_dir = tempfile::tempdir().unwrap();

        let err = read_staged_files(temp_dir.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase()
                .contains("git diff --cached --name-status failed"),
            "expected a git-failure error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn run_staged_no_ai_prints_deterministic_skeleton_and_does_not_commit() {
        let temp_dir = init_repo_with_staged_cargo_toml();
        let context_dir = temp_dir.path().join(".omni-dev");
        write_cargo_scope(&context_dir);
        let head_before = head_oid(temp_dir.path());

        let outcome = run_staged(
            false,
            true,
            None,
            None,
            Some(&context_dir),
            Some(temp_dir.path()),
        )
        .await
        .unwrap();

        assert!(!outcome.applied, "--no-ai must never commit");
        assert_eq!(outcome.message, "feat(cargo): ");

        let head_after = head_oid(temp_dir.path());
        assert_eq!(head_before, head_after, "HEAD must be unchanged");
    }

    #[test]
    fn run_staged_no_ai_no_matching_scope_omits_parens() {
        // Exercises `suggest_staged_skeleton` directly with an empty
        // `valid_scopes` slice, rather than through `run_staged`'s full
        // `load_project_scopes` config-loading chain — that chain falls back
        // to the *user's real* XDG/`$HOME/.omni-dev/scopes.yaml` when no
        // project-level file exists (by design, for global scope config),
        // which would make this "nothing resolves" case depend on whatever
        // happens to be configured on the machine running the test.
        let files = crate::git::commit::FileChanges {
            total_files: 1,
            files_added: 1,
            files_deleted: 0,
            file_list: vec![crate::git::commit::FileChange {
                status: "A".to_string(),
                file: "new.rs".to_string(),
            }],
        };
        assert_eq!(suggest_staged_skeleton(&files, &[]), "feat: ");
    }

    #[tokio::test]
    async fn run_staged_no_ai_errors_when_nothing_staged() {
        let temp_dir = init_empty_repo();
        let err = run_staged(false, true, None, None, None, Some(temp_dir.path()))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.to_lowercase().contains("no staged changes"));
    }

    #[tokio::test]
    async fn staged_command_execute_no_ai_dispatches_and_never_commits() {
        let temp_dir = init_repo_with_staged_cargo_toml();
        let head_before = head_oid(temp_dir.path());

        let cmd = StagedCommand {
            print_only: false,
            context_dir: Some(temp_dir.path().join(".omni-dev")),
            no_ai: true,
        };
        let result = cmd.execute(Some(temp_dir.path())).await;
        assert!(result.is_ok(), "expected clean exit, got: {result:?}");

        let head_after = head_oid(temp_dir.path());
        assert_eq!(
            head_before, head_after,
            "HEAD must be unchanged (no_ai never commits, even with print_only: false)"
        );
    }
}
