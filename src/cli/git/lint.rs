//! Lint command — deterministic, no-AI validation of commit messages against
//! guidelines. The AI sibling is [`super::check`]; the two share
//! [`crate::data::check::CheckReport`] so they're interchangeable in
//! scripts. Core rule logic lives in [`crate::git::lint`].

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Parser;

use crate::data::check::{CheckReport, CommitCheckResult, OutputFormat};

/// Lint command options - deterministically validates commit messages
/// against guidelines (no AI, no network).
#[derive(Parser)]
pub struct LintCommand {
    /// Commit range to lint (e.g., HEAD~3..HEAD, abc123..def456).
    /// Defaults to commits ahead of the default base branch
    /// (origin/main, origin/master, main, or master).
    #[arg(value_name = "COMMIT_RANGE")]
    pub commit_range: Option<String>,

    /// Path to custom context directory (defaults to .omni-dev/).
    #[arg(long)]
    pub context_dir: Option<std::path::PathBuf>,

    /// Accepted for CLI parity with `check`; unused by `lint` (there is no
    /// AI prose guidelines file to load — deterministic rules come from
    /// `.omni-dev/commit-rules.yaml` and `.omni-dev/scopes.yaml` instead).
    #[arg(long)]
    pub guidelines: Option<std::path::PathBuf>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Text)]
    pub output: OutputFormat,

    /// Exits with error code if any issues found (including warnings).
    #[arg(long)]
    pub strict: bool,

    /// Only shows errors/warnings, suppresses info-level output.
    #[arg(long)]
    pub quiet: bool,

    /// Shows the resolved rules/scopes configuration sources.
    #[arg(long)]
    pub verbose: bool,

    /// Includes passing commits in output (hidden by default).
    #[arg(long)]
    pub show_passing: bool,

    /// Reads a single commit message from standard input instead of a
    /// commit range (for a `commit-msg` git hook call site).
    #[arg(long)]
    pub stdin: bool,

    /// Populates a deterministic corrected-scope suggestion for each commit
    /// with an `unknown-scope`/`missing-scope` issue, resolved from the
    /// commit's changed files against `.omni-dev/scopes.yaml` + ecosystem
    /// defaults — no AI, no network. Report-only; use `--fix` to apply.
    /// Requires a commit range (incompatible with `--stdin`, which has no
    /// changed-files list to resolve a scope from).
    #[arg(long)]
    pub suggest: bool,

    /// Applies `--suggest`'s deterministic scope corrections directly to
    /// the repository, via the same `AmendmentHandler` path `git commit
    /// message amend` uses. No AI, no confirmation prompt — only ever
    /// touches a commit with a resolvable `unknown-scope`/`missing-scope`
    /// issue. Implies `--suggest`. Requires a commit range (incompatible
    /// with `--stdin`).
    #[arg(long)]
    pub fix: bool,

    /// Permits `--fix` to amend commits already present in a detected
    /// remote main branch (rewrites published history). Mirrors `git
    /// commit message amend --allow-pushed` / `twiddle --allow-pushed`.
    /// Ignored without `--fix`.
    #[arg(long)]
    pub allow_pushed: bool,
}

impl LintCommand {
    /// Executes the lint command, validating commit messages against
    /// deterministic rules.
    pub async fn execute(self, repo: Option<&Path>) -> Result<()> {
        if self.stdin && (self.suggest || self.fix) {
            anyhow::bail!(
                "--suggest/--fix require a commit range (not --stdin) — a bare message has no \
                 changed-files list to resolve a scope from"
            );
        }

        let repo_root = match repo {
            Some(p) => p.to_path_buf(),
            None => std::env::current_dir().context("Failed to determine current directory")?,
        };
        let repo_root = repo_root.as_path();
        let output_format = self.output;

        let context_dir =
            crate::claude::context::resolve_context_dir_at(self.context_dir.as_deref(), repo_root);
        let valid_scopes = crate::claude::context::load_project_scopes(&context_dir, repo_root);
        let rules = crate::claude::context::load_commit_rules(&context_dir);

        if self.verbose && output_format == OutputFormat::Text {
            self.show_config_status(repo_root, &context_dir, &valid_scopes, &rules);
        }

        let report = if self.stdin {
            let mut message = String::new();
            std::io::stdin()
                .read_to_string(&mut message)
                .context("Failed to read commit message from stdin")?;
            lint_report_for_message(&message, &rules, &valid_scopes)
        } else {
            let range = self.resolve_range(repo_root)?;
            lint_report_for_range(
                repo_root,
                &range,
                &rules,
                &valid_scopes,
                self.suggest || self.fix,
            )?
        };

        self.output_report(&report, output_format)?;

        if self.fix {
            self.apply_fixes(repo_root, &report)?;
        }

        // Unlike `check`, an empty range is a clean exit (0), not an error —
        // a deterministic gate with nothing to lint isn't a failure. Note
        // this reads the pre-`--fix` report (mirroring `check --twiddle`'s
        // identical ordering): `--fix`'s own success/failure is reported
        // separately by `apply_fixes` above.
        let exit_code = report.exit_code(self.strict);
        if exit_code != 0 {
            std::process::exit(exit_code);
        }

        Ok(())
    }

    /// Applies every commit's deterministic scope-fix suggestion (populated
    /// by `--suggest`/`--fix` on `report`) directly via the same
    /// `AmendmentHandler` path `git commit message amend` uses. No prompt —
    /// safe for CI/hook use, since only a resolvable
    /// `unknown-scope`/`missing-scope` issue is ever touched.
    fn apply_fixes(&self, repo_root: &Path, report: &CheckReport) -> Result<()> {
        use crate::data::amendments::{Amendment, AmendmentFile};
        use crate::git::AmendmentHandler;

        let amendments: Vec<Amendment> = report
            .commits
            .iter()
            .filter_map(|c| {
                let suggestion = c.suggestion.as_ref()?;
                Some(Amendment::new(c.hash.clone(), suggestion.message.clone()))
            })
            .collect();

        if amendments.is_empty() {
            println!("✨ No deterministic scope fixes to apply");
            return Ok(());
        }

        let count = amendments.len();
        let amendment_file = AmendmentFile { amendments };
        let handler = AmendmentHandler::new(repo_root)
            .context("Failed to initialize amendment handler")?
            .with_allow_pushed(self.allow_pushed);
        handler
            .apply_amendment_file(&amendment_file)
            .context("Failed to apply deterministic scope fixes")?;

        println!("✅ Fixed {count} commit message(s)");
        Ok(())
    }

    fn resolve_range(&self, repo_root: &Path) -> Result<String> {
        if let Some(range) = &self.commit_range {
            return Ok(range.clone());
        }
        let repo = crate::git::GitRepository::open_at(repo_root)
            .context("Failed to open git repository at the given path")?;
        super::default_commit_range(&repo)
    }

    fn show_config_status(
        &self,
        _repo_root: &Path,
        context_dir: &Path,
        valid_scopes: &[crate::data::context::ScopeDefinition],
        rules: &crate::data::context::CommitRules,
    ) {
        use crate::claude::context::{config_source_label, ConfigSourceLabel};

        println!("📋 Lint configuration:");
        println!("   📂 Config dir: {}", context_dir.display());

        let scopes_source = if valid_scopes.is_empty() {
            "⚪ None found (any scope accepted)".to_string()
        } else {
            match config_source_label(context_dir, "scopes.yaml") {
                ConfigSourceLabel::NotFound => {
                    format!(
                        "✅ (ecosystem defaults only) ({} scopes)",
                        valid_scopes.len()
                    )
                }
                label => format!("✅ {label} ({} scopes)", valid_scopes.len()),
            }
        };
        println!("   🎯 Valid scopes: {scopes_source}");

        let rules_source = match config_source_label(context_dir, "commit-rules.yaml") {
            ConfigSourceLabel::NotFound => "⚪ Using built-in defaults".to_string(),
            label => format!("✅ {label}"),
        };
        println!("   📏 Commit rules: {rules_source}");
        println!(
            "      subject_max_len={}, require_scope={}, types={}",
            rules.subject_max_len,
            rules.require_scope,
            rules.types.len()
        );
        println!();
    }

    /// Outputs the lint report in the specified format.
    fn output_report(&self, report: &CheckReport, format: OutputFormat) -> Result<()> {
        match format {
            OutputFormat::Text => self.output_text_report(report),
            OutputFormat::Json => {
                let json = serde_json::to_string_pretty(report)
                    .context("Failed to serialize report to JSON")?;
                println!("{json}");
                Ok(())
            }
            OutputFormat::Yaml => {
                let yaml =
                    crate::data::to_yaml(report).context("Failed to serialize report to YAML")?;
                println!("{yaml}");
                Ok(())
            }
        }
    }

    /// Outputs the text format report.
    fn output_text_report(&self, report: &CheckReport) -> Result<()> {
        use crate::data::check::IssueSeverity;

        println!();

        for result in &report.commits {
            if result.passes && !self.show_passing {
                continue;
            }

            if self.quiet && !has_errors_or_warnings(&result.issues) {
                continue;
            }

            let icon = super::formatting::determine_commit_icon(result.passes, &result.issues);
            let short_hash = super::formatting::truncate_hash(&result.hash);
            println!("{icon} {short_hash} - \"{}\"", result.message);

            for issue in &result.issues {
                if self.quiet && issue.severity == IssueSeverity::Info {
                    continue;
                }
                let severity_str = super::formatting::format_severity_label(issue.severity);
                println!(
                    "   {} [{}] {}",
                    severity_str, issue.section, issue.explanation
                );
            }

            if !self.quiet {
                if let Some(suggestion) = &result.suggestion {
                    println!();
                    print!(
                        "{}",
                        super::formatting::format_suggestion_text(suggestion, self.verbose)
                    );
                }
            }

            println!();
        }

        println!(
            "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n\
             Summary: {} commits linted\n\
             \x20 {} errors, {} warnings\n\
             \x20 {} passed, {} with issues",
            report.summary.total_commits,
            report.summary.error_count,
            report.summary.warning_count,
            report.summary.passing_commits,
            report.summary.failing_commits,
        );

        Ok(())
    }
}

/// Returns whether any issues have Error or Warning severity.
fn has_errors_or_warnings(issues: &[crate::data::check::CommitIssue]) -> bool {
    use crate::data::check::IssueSeverity;
    issues
        .iter()
        .any(|i| matches!(i.severity, IssueSeverity::Error | IssueSeverity::Warning))
}

/// Builds a single-commit [`CheckReport`] by linting `message` directly —
/// no git, no repository. Used by `--stdin` and the MCP `message` input.
fn lint_report_for_message(
    message: &str,
    rules: &crate::data::context::CommitRules,
    valid_scopes: &[crate::data::context::ScopeDefinition],
) -> CheckReport {
    let issues = crate::git::lint_message(message, rules, valid_scopes);
    let passes = crate::git::lint_passes(&issues);
    let result = CommitCheckResult {
        hash: "-".to_string(),
        message: message.lines().next().unwrap_or("").to_string(),
        issues,
        suggestion: None,
        passes,
        summary: None,
    };
    CheckReport::new(vec![result])
}

/// Builds a [`CheckReport`] by linting every non-merge commit in `range`.
/// Merge commits are already excluded by
/// [`crate::git::GitRepository::get_commits_in_range`] — no additional
/// filtering needed here. An empty range yields an empty (clean) report.
///
/// When `compute_suggestions` is set, each commit with an
/// `unknown-scope`/`missing-scope` issue gets a deterministic
/// [`crate::git::suggest_scope_fix`] suggestion, using its already-computed
/// `file_changes` (populated by `get_commits_in_range`, otherwise unused by
/// lint).
fn lint_report_for_range(
    repo_root: &Path,
    range: &str,
    rules: &crate::data::context::CommitRules,
    valid_scopes: &[crate::data::context::ScopeDefinition],
    compute_suggestions: bool,
) -> Result<CheckReport> {
    let repo = crate::git::GitRepository::open_at(repo_root)
        .context("Failed to open git repository at the given path")?;
    let commits = repo.get_commits_in_range(range)?;

    let results = commits
        .iter()
        .map(|commit| {
            let issues = crate::git::lint_message(&commit.original_message, rules, valid_scopes);
            let passes = crate::git::lint_passes(&issues);
            let suggestion = if compute_suggestions {
                let files: Vec<&str> = commit
                    .analysis
                    .file_changes
                    .file_list
                    .iter()
                    .map(|f| f.file.as_str())
                    .collect();
                crate::git::suggest_scope_fix(
                    &commit.original_message,
                    &files,
                    valid_scopes,
                    &issues,
                )
            } else {
                None
            };
            CommitCheckResult {
                hash: commit.hash.clone(),
                message: commit
                    .original_message
                    .lines()
                    .next()
                    .unwrap_or("")
                    .to_string(),
                issues,
                suggestion,
                passes,
                summary: None,
            }
        })
        .collect();

    Ok(CheckReport::new(results))
}

/// Structured output from [`run_lint`] for programmatic consumers (MCP).
#[derive(Debug, Clone)]
pub struct LintOutcome {
    /// YAML serialisation of the full [`CheckReport`].
    pub report_yaml: String,
    /// `true` when any commit has an error-severity issue.
    pub has_errors: bool,
    /// `true` when any commit has a warning-severity issue.
    pub has_warnings: bool,
    /// Total commits linted.
    pub total_commits: usize,
    /// Strict mode setting that produced `exit_code`.
    pub strict: bool,
    /// Exit code the CLI would use, honouring `strict`.
    pub exit_code: i32,
}

/// What to lint.
///
/// Either a commit range (resolved the same way as the CLI's positional
/// argument, defaulting to commits ahead of the base branch when `None`) or
/// a single literal message (the `--stdin` equivalent).
pub enum LintInput {
    /// Lint every non-merge commit in this range.
    Range(Option<String>),
    /// Lint this message directly, bypassing git entirely.
    Message(String),
}

/// Non-interactive core for `omni-dev git commit message lint`.
///
/// Shared by the CLI and the MCP `git_lint_commits` tool. No AI client
/// involved — deterministic and synchronous under the hood, `async` only
/// for call-site parity with [`super::run_check`].
///
/// `repo_path` selects the repository (`None` defaults to the current
/// working directory); `context_dir` overrides the `.omni-dev/` resolution
/// chain for both `scopes.yaml` and `commit-rules.yaml`. `suggest` populates
/// a deterministic scope-fix suggestion per commit (see
/// [`crate::git::suggest_scope_fix`]) — report-only, mirroring the CLI's
/// `--suggest`; it is an error to combine with [`LintInput::Message`], which
/// has no changed-files list to resolve a scope from. There is no `fix`
/// equivalent here: applying amendments stays exclusive to the CLI's
/// `--fix` and the `git_amend_commits` MCP tool.
pub async fn run_lint(
    input: LintInput,
    repo_path: Option<&Path>,
    context_dir: Option<&Path>,
    strict: bool,
    suggest: bool,
) -> Result<LintOutcome> {
    if suggest && matches!(input, LintInput::Message(_)) {
        anyhow::bail!(
            "suggest requires a commit range — a literal message has no changed-files list to \
             resolve a scope from"
        );
    }

    let repo_root = match repo_path {
        Some(p) => p.to_path_buf(),
        None => std::env::current_dir().context("Failed to determine current directory")?,
    };
    let repo_root = repo_root.as_path();

    let ctx_dir = crate::claude::context::resolve_context_dir_at(context_dir, repo_root);
    let valid_scopes = crate::claude::context::load_project_scopes(&ctx_dir, repo_root);
    let rules = crate::claude::context::load_commit_rules(&ctx_dir);

    let report = match input {
        LintInput::Message(message) => lint_report_for_message(&message, &rules, &valid_scopes),
        LintInput::Range(range) => {
            let range = if let Some(r) = range {
                r
            } else {
                let repo = crate::git::GitRepository::open_at(repo_root)
                    .context("Failed to open git repository at the given path")?;
                super::default_commit_range(&repo)?
            };
            lint_report_for_range(repo_root, &range, &rules, &valid_scopes, suggest)?
        }
    };

    let report_yaml = crate::data::to_yaml(&report).context("Failed to serialise CheckReport")?;
    let has_errors = report.has_errors();
    let has_warnings = report.has_warnings();
    let exit_code = report.exit_code(strict);
    let total_commits = report.commits.len();

    Ok(LintOutcome {
        report_yaml,
        has_errors,
        has_warnings,
        total_commits,
        strict,
        exit_code,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn init_test_repo() -> tempfile::TempDir {
        let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&tmp_root).unwrap();
        let temp_dir = tempfile::tempdir_in(&tmp_root).unwrap();
        for args in [
            vec!["init"],
            vec!["checkout", "-b", "main"],
            vec!["commit", "--allow-empty", "-m", "feat(cli): first commit"],
        ] {
            let output = std::process::Command::new("git")
                .current_dir(temp_dir.path())
                .args([
                    "-c",
                    "user.email=test@example.com",
                    "-c",
                    "user.name=Test",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(&args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?} failed");
        }
        temp_dir
    }

    fn commit(dir: &Path, message: &str) {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                message,
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "commit failed: {message}");
    }

    fn merge_dummy_branch(dir: &Path) {
        let sh = |args: &[&str]| {
            let output = std::process::Command::new("git")
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
            assert!(output.status.success(), "git {args:?} failed");
        };
        sh(&["checkout", "-b", "side"]);
        sh(&["commit", "--allow-empty", "-m", "feat(cli): side change"]);
        sh(&["checkout", "main"]);
        sh(&["commit", "--allow-empty", "-m", "feat(cli): main change"]);
        sh(&["merge", "side", "--no-ff", "-m", "Merge branch 'side'"]);
    }

    /// Writes `path` and commits it for real (not `--allow-empty`), so the
    /// commit has a genuine `file_changes` list for `--suggest`/`--fix` to
    /// resolve a scope from.
    fn commit_file(dir: &Path, path: &str, contents: &str, message: &str) {
        std::fs::write(dir.join(path), contents).unwrap();
        let add = std::process::Command::new("git")
            .current_dir(dir)
            .args(["add", path])
            .output()
            .unwrap();
        assert!(add.status.success(), "git add {path} failed");
        commit(dir, message);
    }

    /// Returns HEAD's commit id, in its own function (rather than an inline
    /// block) so `git2::Reference`/`Commit`'s borrow of `repo` doesn't get
    /// tangled up with the enclosing test's other locals.
    fn head_oid(repo_path: &Path) -> git2::Oid {
        let repo = git2::Repository::open(repo_path).unwrap();
        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        commit.id()
    }

    /// Writes a `cargo` scope (matching `Cargo.toml`/`Cargo.lock`) to
    /// `context_dir/scopes.yaml` — the Dependabot-style fixture from #1564.
    fn write_cargo_scope(context_dir: &Path) {
        std::fs::create_dir_all(context_dir).unwrap();
        std::fs::write(
            context_dir.join("scopes.yaml"),
            "scopes:\n  - name: cargo\n    description: Cargo files\n    examples: []\n    file_patterns:\n      - Cargo.toml\n      - Cargo.lock\n",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn run_lint_message_flags_known_issues() {
        let outcome = run_lint(
            LintInput::Message("feature(bogus): Bad Message.".to_string()),
            None,
            None,
            false,
            false,
        )
        .await
        .unwrap();
        assert!(outcome.has_errors);
        assert_eq!(outcome.exit_code, 1);
        assert_eq!(outcome.total_commits, 1);
        assert!(outcome.report_yaml.contains("commits:"));
    }

    #[tokio::test]
    async fn run_lint_message_clean_passes() {
        let outcome = run_lint(
            LintInput::Message("feat(cli): add thing".to_string()),
            None,
            None,
            false,
            false,
        )
        .await
        .unwrap();
        assert!(!outcome.has_errors);
        assert_eq!(outcome.exit_code, 0);
    }

    #[tokio::test]
    async fn run_lint_range_merge_commit_excluded() {
        let temp_dir = init_test_repo();
        merge_dummy_branch(temp_dir.path());

        let outcome = run_lint(
            LintInput::Range(Some("HEAD~2..HEAD".to_string())),
            Some(temp_dir.path()),
            None,
            false,
            false,
        )
        .await
        .unwrap();

        // HEAD~2..HEAD spans the merge commit plus "main change"; the merge
        // commit itself must not appear.
        assert!(!outcome.report_yaml.contains("Merge branch"));
    }

    #[tokio::test]
    async fn run_lint_range_empty_is_clean_not_an_error() {
        let temp_dir = init_test_repo();
        let outcome = run_lint(
            LintInput::Range(Some("HEAD..HEAD".to_string())),
            Some(temp_dir.path()),
            None,
            false,
            false,
        )
        .await
        .unwrap();
        assert_eq!(outcome.total_commits, 0);
        assert!(!outcome.has_errors);
        assert_eq!(outcome.exit_code, 0);
    }

    #[tokio::test]
    async fn run_lint_range_strict_promotes_warnings() {
        let temp_dir = init_test_repo();
        commit(
            temp_dir.path(),
            "feat(cli): add thing\n\nCo-Authored-By: Bot <bot@example.com>",
        );
        let outcome = run_lint(
            LintInput::Range(Some("HEAD~1..HEAD".to_string())),
            Some(temp_dir.path()),
            None,
            true,
            false,
        )
        .await
        .unwrap();
        assert!(!outcome.has_errors);
        assert!(outcome.has_warnings);
        assert_eq!(outcome.exit_code, 2);
    }

    #[tokio::test]
    async fn run_lint_range_and_message_agree_on_same_content() {
        let temp_dir = init_test_repo();
        commit(temp_dir.path(), "feature(bogus): Bad Message.");

        let range_outcome = run_lint(
            LintInput::Range(Some("HEAD~1..HEAD".to_string())),
            Some(temp_dir.path()),
            None,
            false,
            false,
        )
        .await
        .unwrap();
        let message_outcome = run_lint(
            LintInput::Message("feature(bogus): Bad Message.".to_string()),
            None,
            None,
            false,
            false,
        )
        .await
        .unwrap();

        assert_eq!(range_outcome.has_errors, message_outcome.has_errors);
        assert_eq!(range_outcome.exit_code, message_outcome.exit_code);
    }

    #[test]
    fn cli_execute_json_output_matches_check_report_shape() {
        let temp_dir = init_test_repo();
        commit(temp_dir.path(), "feat(cli): second commit");
        let cmd = LintCommand {
            commit_range: Some("HEAD~1..HEAD".to_string()),
            context_dir: None,
            guidelines: None,
            output: OutputFormat::Json,
            strict: false,
            quiet: true,
            verbose: false,
            show_passing: true,
            stdin: false,
            suggest: false,
            fix: false,
            allow_pushed: false,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute(Some(temp_dir.path())));
        assert!(result.is_ok());
    }

    #[test]
    fn cli_execute_yaml_output_matches_check_report_shape() {
        let temp_dir = init_test_repo();
        commit(temp_dir.path(), "feat(cli): second commit");
        let cmd = LintCommand {
            commit_range: Some("HEAD~1..HEAD".to_string()),
            context_dir: None,
            guidelines: None,
            output: OutputFormat::Yaml,
            strict: false,
            quiet: true,
            verbose: false,
            show_passing: true,
            stdin: false,
            suggest: false,
            fix: false,
            allow_pushed: false,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute(Some(temp_dir.path())));
        assert!(result.is_ok());
    }

    /// `commit_range: None` drives `resolve_range` through its
    /// `default_commit_range` fallback rather than the literal-range
    /// shortcut every other test in this module uses.
    #[test]
    fn cli_execute_range_none_uses_default_commit_range() {
        let temp_dir = init_test_repo();
        let cmd = LintCommand {
            commit_range: None,
            context_dir: None,
            guidelines: None,
            output: OutputFormat::Json,
            strict: false,
            quiet: true,
            verbose: false,
            show_passing: true,
            stdin: false,
            suggest: false,
            fix: false,
            allow_pushed: false,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute(Some(temp_dir.path())));
        assert!(result.is_ok(), "expected clean exit, got: {result:?}");
    }

    /// `LintInput::Range(None)` is `run_lint`'s own default-range fallback —
    /// a separate code path from `LintCommand::resolve_range` above, since
    /// `run_lint` is also reachable from the MCP tool with no CLI in front
    /// of it.
    #[tokio::test]
    async fn run_lint_range_none_uses_default_commit_range() {
        let temp_dir = init_test_repo();
        let outcome = run_lint(
            LintInput::Range(None),
            Some(temp_dir.path()),
            None,
            false,
            false,
        )
        .await
        .unwrap();
        assert_eq!(outcome.total_commits, 0);
    }

    #[test]
    fn cli_execute_verbose_config_status_empty_scopes_rules_not_found() {
        let temp_dir = init_test_repo();
        // An explicit (nonexistent) context_dir bypasses walk-up discovery —
        // without it, resolution would walk up from `temp_dir` (created
        // under this crate's own `tmp/`) and find *this repo's* real
        // `.omni-dev/scopes.yaml`, defeating the "nothing configured" case
        // this test means to exercise.
        let context_dir = temp_dir.path().join(".omni-dev");
        let cmd = LintCommand {
            commit_range: Some("HEAD..HEAD".to_string()),
            context_dir: Some(context_dir),
            guidelines: None,
            output: OutputFormat::Text,
            strict: false,
            quiet: false,
            verbose: true,
            show_passing: false,
            stdin: false,
            suggest: false,
            fix: false,
            allow_pushed: false,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute(Some(temp_dir.path())));
        assert!(result.is_ok());
    }

    #[test]
    fn cli_execute_verbose_config_status_scopes_and_rules_found() {
        let temp_dir = init_test_repo();
        let context_dir = temp_dir.path().join(".omni-dev");
        std::fs::create_dir_all(&context_dir).unwrap();
        std::fs::write(
            context_dir.join("scopes.yaml"),
            "scopes:\n  - name: custom\n    description: Custom scope\n    examples: []\n    file_patterns: []\n",
        )
        .unwrap();
        std::fs::write(
            context_dir.join("commit-rules.yaml"),
            "subject_max_len: 72\ntypes:\n  - feat\nrequire_scope: false\nforbidden_footers: []\n",
        )
        .unwrap();

        let cmd = LintCommand {
            commit_range: Some("HEAD..HEAD".to_string()),
            context_dir: Some(context_dir),
            guidelines: None,
            output: OutputFormat::Text,
            strict: false,
            quiet: false,
            verbose: true,
            show_passing: false,
            stdin: false,
            suggest: false,
            fix: false,
            allow_pushed: false,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute(Some(temp_dir.path())));
        assert!(result.is_ok());
    }

    #[test]
    fn cli_execute_verbose_config_status_ecosystem_scopes_no_file() {
        let temp_dir = init_test_repo();
        std::fs::write(temp_dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        // See the comment in the "empty scopes" test above: an explicit
        // (nonexistent) context_dir bypasses walk-up discovery, so the only
        // non-empty scopes are the `Cargo.toml`-derived ecosystem defaults
        // this test targets.
        let context_dir = temp_dir.path().join(".omni-dev");

        let cmd = LintCommand {
            commit_range: Some("HEAD..HEAD".to_string()),
            context_dir: Some(context_dir),
            guidelines: None,
            output: OutputFormat::Text,
            strict: false,
            quiet: false,
            verbose: true,
            show_passing: false,
            stdin: false,
            suggest: false,
            fix: false,
            allow_pushed: false,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute(Some(temp_dir.path())));
        assert!(result.is_ok());
    }

    /// `passes` (and so the `show_passing: false` default's skip-continue at
    /// the top of the per-commit loop) reflects only `Error`-severity
    /// issues, matching `exit_code`'s own error-only gate — a
    /// `Warning`-only commit still counts as passing and is hidden here
    /// just like a clean one. `output_text_report`'s per-issue print lines
    /// are covered separately below (`quiet_mode_filters_...`), since a
    /// commit that fails the `show_passing` check can't reach them without
    /// an `Error`-severity issue, which would drive `execute()`'s
    /// `std::process::exit` and abort this whole test binary.
    #[test]
    fn output_text_report_show_passing_false_hides_warning_only_commits() {
        let temp_dir = init_test_repo();
        commit(temp_dir.path(), "feat(cli): clean second commit");
        commit(
            temp_dir.path(),
            "feat(cli): add thing\n\nCo-Authored-By: Bot <bot@example.com>",
        );
        let cmd = LintCommand {
            commit_range: Some("HEAD~2..HEAD".to_string()),
            context_dir: None,
            guidelines: None,
            output: OutputFormat::Text,
            strict: false,
            quiet: false,
            verbose: false,
            show_passing: false,
            stdin: false,
            suggest: false,
            fix: false,
            allow_pushed: false,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute(Some(temp_dir.path())));
        assert!(result.is_ok(), "expected clean exit, got: {result:?}");
    }

    /// `quiet` filters both a clean commit (`show_passing: true` keeps it
    /// past the first check, but it has no errors/warnings to show) and,
    /// within a surviving commit's own issue list, its `Info`-severity
    /// entries — two distinct skip branches in `output_text_report`.
    #[test]
    fn output_text_report_quiet_mode_filters_clean_and_info_issues() {
        let temp_dir = init_test_repo();
        commit(temp_dir.path(), "feat(cli): clean thing");
        commit(
            temp_dir.path(),
            "feat(cli): Add thing.\n\nCo-Authored-By: Bot <bot@example.com>",
        );
        let cmd = LintCommand {
            commit_range: Some("HEAD~2..HEAD".to_string()),
            context_dir: None,
            guidelines: None,
            output: OutputFormat::Text,
            strict: false,
            quiet: true,
            verbose: false,
            show_passing: true,
            stdin: false,
            suggest: false,
            fix: false,
            allow_pushed: false,
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(cmd.execute(Some(temp_dir.path())));
        assert!(result.is_ok(), "expected clean exit, got: {result:?}");
    }

    // ── --suggest / --fix (#1564) ────────────────────────────────────

    #[tokio::test]
    async fn run_lint_range_with_suggest_resolves_dependabot_style_scope() {
        let temp_dir = init_test_repo();
        let context_dir = temp_dir.path().join(".omni-dev");
        write_cargo_scope(&context_dir);
        commit_file(
            temp_dir.path(),
            "Cargo.toml",
            "[package]\n",
            "chore(deps): bump foo",
        );

        let outcome = run_lint(
            LintInput::Range(Some("HEAD~1..HEAD".to_string())),
            Some(temp_dir.path()),
            Some(&context_dir),
            false,
            true,
        )
        .await
        .unwrap();

        assert!(outcome.has_errors, "unknown-scope should still be flagged");
        assert!(
            outcome.report_yaml.contains("chore(cargo): bump foo"),
            "expected a deterministic suggestion in report_yaml: {}",
            outcome.report_yaml
        );
    }

    #[tokio::test]
    async fn run_lint_range_without_suggest_leaves_suggestion_none() {
        let temp_dir = init_test_repo();
        let context_dir = temp_dir.path().join(".omni-dev");
        write_cargo_scope(&context_dir);
        commit_file(
            temp_dir.path(),
            "Cargo.toml",
            "[package]\n",
            "chore(deps): bump foo",
        );

        let outcome = run_lint(
            LintInput::Range(Some("HEAD~1..HEAD".to_string())),
            Some(temp_dir.path()),
            Some(&context_dir),
            false,
            false,
        )
        .await
        .unwrap();

        assert!(
            !outcome.report_yaml.contains("suggestion:"),
            "no suggestion should be present without --suggest: {}",
            outcome.report_yaml
        );
    }

    #[tokio::test]
    async fn run_lint_message_with_suggest_errors() {
        let err = run_lint(
            LintInput::Message("feat(cli): add thing".to_string()),
            None,
            None,
            false,
            true,
        )
        .await
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.to_lowercase()
                .contains("suggest requires a commit range"),
            "expected a clear validation error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn cli_execute_stdin_with_suggest_errors() {
        let cmd = LintCommand {
            commit_range: None,
            context_dir: None,
            guidelines: None,
            output: OutputFormat::Text,
            strict: false,
            quiet: false,
            verbose: false,
            show_passing: false,
            stdin: true,
            suggest: true,
            fix: false,
            allow_pushed: false,
        };
        let err = cmd.execute(None).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("--suggest/--fix require a commit range"),
            "expected a clear validation error, got: {msg}"
        );
    }

    #[tokio::test]
    async fn cli_execute_stdin_with_fix_errors() {
        let cmd = LintCommand {
            commit_range: None,
            context_dir: None,
            guidelines: None,
            output: OutputFormat::Text,
            strict: false,
            quiet: false,
            verbose: false,
            show_passing: false,
            stdin: true,
            suggest: false,
            fix: true,
            allow_pushed: false,
        };
        let err = cmd.execute(None).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--suggest/--fix require a commit range"));
    }

    /// Exercises `apply_fixes` directly rather than the full `execute()`
    /// path — the fixture commit has an `unknown-scope` error, and
    /// `execute()`'s exit-code handling would call `std::process::exit` and
    /// abort this whole test binary (see the `show_passing` test above for
    /// the same caveat).
    #[test]
    fn apply_fixes_amends_commit_via_amendment_handler() {
        let temp_dir = init_test_repo();
        // The context dir must live OUTSIDE the repo working tree — inside
        // it, an untracked `.omni-dev/scopes.yaml` would make
        // `AmendmentHandler`'s working-directory-clean safety check refuse
        // to amend anything.
        let context_tmp =
            tempfile::tempdir_in(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp"))
                .unwrap();
        let context_dir = context_tmp.path().join(".omni-dev");
        write_cargo_scope(&context_dir);
        commit_file(
            temp_dir.path(),
            "Cargo.toml",
            "[package]\n",
            "chore(deps): bump foo",
        );

        let ctx_dir =
            crate::claude::context::resolve_context_dir_at(Some(&context_dir), temp_dir.path());
        let valid_scopes = crate::claude::context::load_project_scopes(&ctx_dir, temp_dir.path());
        let rules = crate::claude::context::load_commit_rules(&ctx_dir);
        let report =
            lint_report_for_range(temp_dir.path(), "HEAD~1..HEAD", &rules, &valid_scopes, true)
                .unwrap();

        let cmd = LintCommand {
            commit_range: None,
            context_dir: None,
            guidelines: None,
            output: OutputFormat::Json,
            strict: false,
            quiet: true,
            verbose: false,
            show_passing: true,
            stdin: false,
            suggest: false,
            fix: true,
            allow_pushed: false,
        };
        cmd.apply_fixes(temp_dir.path(), &report).unwrap();

        let repo = git2::Repository::open(temp_dir.path()).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        let msg = head.message().unwrap().to_string();
        assert!(
            msg.starts_with("chore(cargo): bump foo"),
            "expected the amended message at HEAD, got: {msg:?}"
        );
    }

    #[test]
    fn apply_fixes_with_no_suggestions_is_a_clean_noop() {
        let temp_dir = init_test_repo();
        let head_before = head_oid(temp_dir.path());

        let report = CheckReport::new(vec![]);
        let cmd = LintCommand {
            commit_range: None,
            context_dir: None,
            guidelines: None,
            output: OutputFormat::Json,
            strict: false,
            quiet: true,
            verbose: false,
            show_passing: true,
            stdin: false,
            suggest: false,
            fix: true,
            allow_pushed: false,
        };
        cmd.apply_fixes(temp_dir.path(), &report).unwrap();

        let head_after = head_oid(temp_dir.path());
        assert_eq!(head_before, head_after, "HEAD must be unchanged");
    }
}
