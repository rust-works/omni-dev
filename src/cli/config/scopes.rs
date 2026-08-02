//! `config scopes` CLI commands — scope-taxonomy operations on
//! `.omni-dev/scopes.yaml`.
//!
//! `usage` tallies declared commit scopes against `scopes.yaml`; `lint`
//! validates `scopes.yaml` against the source tree it claims to describe
//! (issue #1475). Lint is zero AI, zero network: both its assertions are
//! pure globset matching against the tracked-file list, reusing
//! `crate::git::commit::scope_matches_files` and `crate::git::resolve_scope`
//! rather than a second matcher.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use globset::GlobMatcher;

use crate::data::check::OutputFormat;
use crate::data::context::ScopeDefinition;
use crate::data::scopes_lint::{DeadPattern, ScopesLintReport};
use crate::git::commit::{tally_scope_usage, ScopeUsageReport};

/// Scopes operations.
#[derive(Parser)]
pub struct ScopesCommand {
    /// Scopes subcommand to execute.
    #[command(subcommand)]
    pub command: ScopesSubcommands,
}

/// Scopes subcommands.
#[derive(Subcommand)]
pub enum ScopesSubcommands {
    /// Tallies declared commit scopes against `scopes.yaml`, reporting
    /// unknown, unused, and scope-less commits.
    Usage(UsageCommand),
    /// Validates scopes.yaml against the source tree: every `file_patterns`
    /// entry must match a tracked file, and every tracked file under
    /// `--root` must be matched by some scope.
    Lint(LintCommand),
}

impl ScopesCommand {
    /// Executes the scopes command.
    pub fn execute(self, repo: Option<&Path>) -> Result<()> {
        match self.command {
            ScopesSubcommands::Usage(usage_cmd) => usage_cmd.execute(repo),
            ScopesSubcommands::Lint(cmd) => cmd.execute(repo),
        }
    }
}

/// Usage command options — tallies declared commit scopes against history.
#[derive(Parser)]
pub struct UsageCommand {
    /// Commit range to analyze (e.g., HEAD~300..HEAD, abc123..def456).
    /// Defaults to the entire history reachable from HEAD.
    #[arg(value_name = "COMMIT_RANGE", conflicts_with = "max_count")]
    pub commit_range: Option<String>,

    /// Limits analysis to the newest N commits reachable from HEAD (ignored
    /// when COMMIT_RANGE is given). Useful for comparing the answer at
    /// different window sizes, e.g. `-n 150` vs `-n 400`.
    #[arg(short = 'n', long = "max-count", value_name = "N")]
    pub max_count: Option<usize>,

    /// Path to custom context directory (defaults to .omni-dev/).
    #[arg(long)]
    pub context_dir: Option<PathBuf>,

    /// Excludes ecosystem default scopes (cargo/core/lib/test, …) from the
    /// known set, so they are reported as unknown rather than accepted.
    #[arg(long)]
    pub project_only: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Text)]
    pub output: OutputFormat,
}

impl UsageCommand {
    /// Executes the usage command: tallies declared scopes across the
    /// resolved commit range and prints a report. Always exits 0 on a
    /// successful tally (including an empty range) — this is a reporting
    /// command, never a gate.
    pub fn execute(self, repo: Option<&Path>) -> Result<()> {
        let repo_root = match repo {
            Some(p) => p.to_path_buf(),
            None => std::env::current_dir().context("Failed to determine current directory")?,
        };
        let repo_root = repo_root.as_path();

        let git_repo = crate::git::GitRepository::open_at(repo_root)
            .context("Failed to open git repository at the given path")?;

        let commits = match &self.commit_range {
            Some(range) => git_repo.get_commits_in_range(range)?,
            None => git_repo.get_commits_from_head(self.max_count)?,
        };

        let subjects: Vec<&str> = commits
            .iter()
            .map(|c| c.original_message.lines().next().unwrap_or("").trim())
            .collect();

        let context_dir =
            crate::claude::context::resolve_context_dir_at(self.context_dir.as_deref(), repo_root);
        let scopes_yaml_only = crate::claude::context::load_project_scopes_only(&context_dir);
        let known_scopes = if self.project_only {
            scopes_yaml_only.clone()
        } else {
            crate::claude::context::load_project_scopes(&context_dir, repo_root)
        };

        let report = tally_scope_usage(&subjects, &known_scopes, &scopes_yaml_only);

        self.output_report(&report)
    }

    /// Renders `report` per `self.output`.
    fn output_report(&self, report: &ScopeUsageReport) -> Result<()> {
        match self.output {
            OutputFormat::Text => {
                print!("{}", format_text_report(report));
                Ok(())
            }
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
}

/// Formats a [`ScopeUsageReport`] as human-readable text.
fn format_text_report(report: &ScopeUsageReport) -> String {
    use std::fmt::Write;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "📊 Scope usage: {} commit(s) analyzed",
        report.total_commits
    );
    out.push('\n');

    if report.declared.is_empty() {
        out.push_str("Declared scopes: (none)\n");
    } else {
        out.push_str("Declared scopes:\n");
        for sc in &report.declared {
            let _ = writeln!(out, "  {:<20} {}", sc.name, sc.count);
        }
    }
    out.push('\n');

    if !report.unknown.is_empty() {
        out.push_str("⚠️  Unknown (declared, not in scopes.yaml):\n");
        for sc in &report.unknown {
            let _ = writeln!(out, "  {:<20} {}", sc.name, sc.count);
        }
        out.push('\n');
    }

    if !report.unused.is_empty() {
        out.push_str("💤 Unused (defined in scopes.yaml, never declared):\n");
        for name in &report.unused {
            let _ = writeln!(out, "  {name}");
        }
        out.push('\n');
    }

    let _ = writeln!(out, "Scope-less commits: {}", report.scope_less_count);

    out
}

/// Lint command options.
#[derive(Parser)]
pub struct LintCommand {
    /// Root path(s) to check for scope coverage (repeatable).
    #[arg(long, default_value = "src")]
    pub root: Vec<String>,

    /// Disables `--project-only` (on by default): folds in the
    /// ecosystem-detected default scopes (e.g. Rust's `lib` =
    /// `["src/lib.rs", "src/**"]`) before checking coverage. Without
    /// `--project-only`, a catch-all ecosystem scope can make the "every
    /// file is scoped" check vacuously true.
    #[arg(long)]
    pub no_project_only: bool,

    /// Additional glob(s) for paths that need no scope (repeatable).
    /// Unioned with any `allow:` list in scopes.yaml.
    #[arg(long)]
    pub allow: Vec<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Text)]
    pub output: OutputFormat,

    /// Path to custom context directory (defaults to .omni-dev/).
    #[arg(long)]
    pub context_dir: Option<PathBuf>,
}

impl LintCommand {
    /// Executes the lint command.
    pub fn execute(self, repo: Option<&Path>) -> Result<()> {
        let repo_root = match repo {
            Some(p) => p.to_path_buf(),
            None => std::env::current_dir().context("Failed to determine current directory")?,
        };
        let repo_root = repo_root.as_path();

        let report = run_scopes_lint(repo_root, &self)?;
        render_report(&report, self.output)?;
        std::process::exit(report.exit_code());
    }
}

/// Runs the lint against a real repository.
///
/// Opens it, loads `scopes.yaml` strictly, conditionally merges ecosystem
/// defaults, then delegates to the pure [`lint_scopes`]. Separated from
/// [`LintCommand::execute`] so tests can drive it directly (STYLE-0025)
/// without spawning the binary.
pub fn run_scopes_lint(repo_root: &Path, cmd: &LintCommand) -> Result<ScopesLintReport> {
    let repo = crate::git::GitRepository::open_at(repo_root)
        .context("Failed to open git repository at the given path")?;
    let tracked_files = repo.tracked_files()?;

    let context_dir =
        crate::claude::context::resolve_context_dir_at(cmd.context_dir.as_deref(), repo_root);
    let scopes_file = crate::claude::context::load_scopes_file_strict(&context_dir)
        .context("Failed to load .omni-dev/scopes.yaml")?;

    let project_only = !cmd.no_project_only;
    // `merge_ecosystem_scopes` only skips a default whose *name* already
    // exists in the vector it's given, so it must be seeded with the
    // project's own scopes — not called against an empty vector — or a
    // same-named ecosystem default (e.g. Rust's `test`) gets added
    // alongside the project's version instead of yielding to it
    // (docs/plan/config-internals.md's documented "user-defined scopes
    // always win" contract). Diff back out the seed so this stays the
    // *additional* scopes assertion 2 folds in, as `lint_scopes` expects.
    let ecosystem_scopes: Vec<ScopeDefinition> = if project_only {
        Vec::new()
    } else {
        let mut combined = scopes_file.scopes.clone();
        crate::claude::context::discovery::merge_ecosystem_scopes(&mut combined, repo_root);
        combined
            .into_iter()
            .filter(|s| !scopes_file.scopes.iter().any(|p| p.name == s.name))
            .collect()
    };

    let mut allow_globs = cmd.allow.clone();
    allow_globs.extend(scopes_file.allow.iter().cloned());

    Ok(lint_scopes(
        &tracked_files,
        &scopes_file.scopes,
        &ecosystem_scopes,
        &cmd.root,
        &allow_globs,
        project_only,
    ))
}

/// Pure core of `omni-dev config scopes lint` — no filesystem or git I/O, so
/// this is what the unit tests below drive directly.
///
/// - `tracked_files`: the full tracked-file set.
/// - `project_scopes`: scopes parsed directly from `scopes.yaml`. Assertion
///   1 (dead patterns) always checks only these — ecosystem scopes are Rust
///   constants a user cannot fix by editing `scopes.yaml`, so flagging their
///   patterns dead would not be actionable.
/// - `ecosystem_scopes`: the scopes `merge_ecosystem_scopes` would inject
///   for this repo. Folded into the effective scope set assertion 2 checks
///   coverage against only when `project_only` is `false` — this function
///   enforces that itself, so passing a non-empty `ecosystem_scopes`
///   alongside `project_only: true` still excludes it.
/// - `roots`: repo-relative root paths (no trailing slash) assertion 2
///   walks.
/// - `allow_globs`: globs suppressing assertion-2 violations for paths that
///   legitimately need no scope.
pub fn lint_scopes(
    tracked_files: &[String],
    project_scopes: &[ScopeDefinition],
    ecosystem_scopes: &[ScopeDefinition],
    roots: &[String],
    allow_globs: &[String],
    project_only: bool,
) -> ScopesLintReport {
    let file_refs: Vec<&str> = tracked_files.iter().map(String::as_str).collect();

    let mut dead_patterns = dead_patterns(&file_refs, project_scopes);
    dead_patterns.sort_by(|a, b| (&a.scope, &a.pattern).cmp(&(&b.scope, &b.pattern)));

    // The pure core enforces `project_only` itself, rather than trusting
    // the caller to have already passed an empty `ecosystem_scopes` — this
    // is the load-bearing invariant the flag exists for, so it must not be
    // bypassable by a caller mistake.
    let mut effective_scopes: Vec<ScopeDefinition> = project_scopes.to_vec();
    if !project_only {
        effective_scopes.extend(ecosystem_scopes.iter().cloned());
    }

    let allow_matchers = build_allow_matchers(allow_globs);
    let files_in_scope: Vec<&str> = file_refs
        .iter()
        .copied()
        .filter(|f| file_in_roots(f, roots))
        .filter(|f| !is_allowed(f, &allow_matchers))
        .collect();

    let mut unscoped_files = unscoped_files(&files_in_scope, &effective_scopes);
    unscoped_files.sort();

    ScopesLintReport {
        roots: roots.to_vec(),
        project_only,
        scopes_checked: effective_scopes.len(),
        files_checked: files_in_scope.len(),
        dead_patterns,
        unscoped_files,
    }
}

/// Assertion 1: every non-negative `file_patterns` entry must match at
/// least one tracked file. `!`-prefixed negative patterns are never
/// reported dead — they exist to exclude matches, not to independently
/// match anything.
fn dead_patterns(files: &[&str], scopes: &[ScopeDefinition]) -> Vec<DeadPattern> {
    let mut dead = Vec::new();
    for scope in scopes {
        for pattern in &scope.file_patterns {
            if pattern.starts_with('!') {
                continue;
            }
            let single = std::slice::from_ref(pattern);
            if crate::git::commit::scope_matches_files(files, single).is_none() {
                dead.push(DeadPattern {
                    scope: scope.name.clone(),
                    pattern: pattern.clone(),
                });
            }
        }
    }
    dead
}

/// Assertion 2: every file in `files` must resolve to at least one scope.
/// Calls `resolve_scope` once per file (never batched) so a facade gap like
/// `src/foo/**` not covering `src/foo.rs` is caught per-file rather than
/// masked by a sibling file matching the same scope.
fn unscoped_files(files: &[&str], scopes: &[ScopeDefinition]) -> Vec<String> {
    files
        .iter()
        .filter(|f| crate::git::resolve_scope(std::slice::from_ref(*f), scopes).is_none())
        .map(|f| (*f).to_string())
        .collect()
}

/// Whether `file` falls under one of `roots`. A bare `starts_with(root)`
/// would wrongly match a sibling directory (root `src` matching
/// `srcfoo/bar.rs`), so this requires an exact match or a `/`-bounded
/// prefix.
fn file_in_roots(file: &str, roots: &[String]) -> bool {
    roots
        .iter()
        .any(|root| file == root || file.starts_with(&format!("{root}/")))
}

fn build_allow_matchers(allow_globs: &[String]) -> Vec<GlobMatcher> {
    allow_globs
        .iter()
        .filter_map(|p| globset::Glob::new(p).ok().map(|g| g.compile_matcher()))
        .collect()
}

fn is_allowed(file: &str, matchers: &[GlobMatcher]) -> bool {
    matchers.iter().any(|m| m.is_match(file))
}

/// Renders `report` to stdout in the requested `format` and returns.
/// Exiting on the report's exit code is the caller's job.
fn render_report(report: &ScopesLintReport, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Text => render_text_report(report),
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(report)?);
            Ok(())
        }
        OutputFormat::Yaml => {
            println!("{}", crate::data::to_yaml(report)?);
            Ok(())
        }
    }
}

fn render_text_report(report: &ScopesLintReport) -> Result<()> {
    let project_only_label = if report.project_only {
        "project-only"
    } else {
        "project + ecosystem"
    };
    println!("🔍 Linting .omni-dev/scopes.yaml against the source tree...");
    println!(
        "   📂 Scopes checked: {} ({})",
        report.scopes_checked, project_only_label
    );
    println!("   📁 Roots: {}", report.roots.join(", "));
    println!("   📄 Files checked: {}", report.files_checked);
    println!();

    if report.dead_patterns.is_empty() {
        println!("✅ No dead patterns.");
    } else {
        println!("❌ Dead patterns ({}):", report.dead_patterns.len());
        for dead in &report.dead_patterns {
            println!("   {} ({})", dead.pattern, dead.scope);
        }
    }

    if report.unscoped_files.is_empty() {
        println!("✅ No unscoped files.");
    } else {
        println!("❌ Unscoped files ({}):", report.unscoped_files.len());
        for file in &report.unscoped_files {
            println!("   {file}");
        }
    }

    let total = report.dead_patterns.len() + report.unscoped_files.len();
    println!();
    println!(
        "Summary: {} scopes, {} files checked — {} violation{}",
        report.scopes_checked,
        report.files_checked,
        total,
        if total == 1 { "" } else { "s" }
    );

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::git::commit::ScopeCount;

    // ── usage report ─────────────────────────────────────────────────

    fn make_report() -> ScopeUsageReport {
        ScopeUsageReport {
            total_commits: 5,
            declared: vec![
                ScopeCount {
                    name: "cli".to_string(),
                    count: 3,
                },
                ScopeCount {
                    name: "lib".to_string(),
                    count: 1,
                },
            ],
            unknown: vec![ScopeCount {
                name: "lib".to_string(),
                count: 1,
            }],
            unused: vec!["workflows".to_string()],
            scope_less_count: 1,
        }
    }

    #[test]
    fn text_report_includes_all_sections() {
        let report = make_report();
        let text = format_text_report(&report);
        assert!(text.contains("5 commit(s) analyzed"));
        assert!(text.contains("cli"));
        assert!(text.contains("Unknown"));
        assert!(text.contains("lib"));
        assert!(text.contains("Unused"));
        assert!(text.contains("workflows"));
        assert!(text.contains("Scope-less commits: 1"));
    }

    #[test]
    fn text_report_empty_omits_unknown_and_unused_sections() {
        let report = ScopeUsageReport {
            total_commits: 0,
            declared: vec![],
            unknown: vec![],
            unused: vec![],
            scope_less_count: 0,
        };
        let text = format_text_report(&report);
        assert!(text.contains("(none)"));
        assert!(!text.contains("Unknown"));
        assert!(!text.contains("Unused"));
        assert!(text.contains("Scope-less commits: 0"));
    }

    #[test]
    fn json_output_round_trips() {
        let report = make_report();
        let json = serde_json::to_string_pretty(&report).unwrap();
        let parsed: ScopeUsageReport = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, report);
    }

    // ── lint ────────────────────────────────────────────────────────

    fn scope(name: &str, patterns: &[&str]) -> ScopeDefinition {
        ScopeDefinition {
            name: name.to_string(),
            description: String::new(),
            examples: Vec::new(),
            file_patterns: patterns.iter().map(|p| (*p).to_string()).collect(),
        }
    }

    fn files(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| (*p).to_string()).collect()
    }

    // ── dead patterns ────────────────────────────────────────────────

    #[test]
    fn dead_pattern_reported_when_no_file_matches() {
        let scopes = vec![scope("cli", &["src/cli/**"])];
        let report = lint_scopes(
            &files(&["src/git/commit.rs"]),
            &scopes,
            &[],
            &["src".to_string()],
            &[],
            true,
        );
        assert_eq!(report.dead_patterns.len(), 1);
        assert_eq!(report.dead_patterns[0].pattern, "src/cli/**");
        assert_eq!(report.dead_patterns[0].scope, "cli");
    }

    #[test]
    fn dead_pattern_not_reported_when_one_file_matches() {
        let scopes = vec![scope("cli", &["src/cli/**"])];
        let report = lint_scopes(
            &files(&["src/cli/mod.rs"]),
            &scopes,
            &[],
            &["src".to_string()],
            &[],
            true,
        );
        assert!(report.dead_patterns.is_empty());
    }

    #[test]
    fn negative_pattern_never_reported_dead() {
        // Neither the positive nor the negative pattern matches anything —
        // only the positive one is a candidate for "dead"; the negative one
        // must never be reported regardless of match outcome.
        let scopes = vec![scope("cli", &["src/cli/**", "!src/cli/generated/**"])];
        let report = lint_scopes(
            &files(&["src/other.rs"]),
            &scopes,
            &[],
            &["src".to_string()],
            &[],
            true,
        );
        assert_eq!(report.dead_patterns.len(), 1);
        assert_eq!(report.dead_patterns[0].pattern, "src/cli/**");
    }

    // ── unscoped files ───────────────────────────────────────────────

    #[test]
    fn unscoped_file_reported_under_root() {
        let scopes = vec![scope("cli", &["src/cli/**"])];
        let report = lint_scopes(
            &files(&["src/newmod/foo.rs"]),
            &scopes,
            &[],
            &["src".to_string()],
            &[],
            true,
        );
        assert_eq!(report.unscoped_files, vec!["src/newmod/foo.rs".to_string()]);
    }

    #[test]
    fn unscoped_file_cleared_by_covering_scope() {
        let scopes = vec![
            scope("cli", &["src/cli/**"]),
            scope("newmod", &["src/newmod/**"]),
        ];
        let report = lint_scopes(
            &files(&["src/newmod/foo.rs"]),
            &scopes,
            &[],
            &["src".to_string()],
            &[],
            true,
        );
        assert!(report.unscoped_files.is_empty());
    }

    #[test]
    fn facade_pattern_does_not_cover_sibling_file() {
        // src/foo/** does not match src/foo.rs — pins the facade-gap
        // semantics of the underlying resolve_scope/scope_matches_files.
        let scopes = vec![scope("foo", &["src/foo/**"])];
        let report = lint_scopes(
            &files(&["src/foo.rs"]),
            &scopes,
            &[],
            &["src".to_string()],
            &[],
            true,
        );
        assert_eq!(report.unscoped_files, vec!["src/foo.rs".to_string()]);
    }

    #[test]
    fn allow_glob_suppresses_only_matching_paths() {
        let scopes = vec![scope("cli", &["src/cli/**"])];
        let report = lint_scopes(
            &files(&["src/lib.rs", "src/newmod/foo.rs"]),
            &scopes,
            &[],
            &["src".to_string()],
            &["src/lib.rs".to_string()],
            true,
        );
        assert_eq!(report.unscoped_files, vec!["src/newmod/foo.rs".to_string()]);
    }

    // ── --project-only regression (the one that matters most) ─────────

    #[test]
    fn project_only_true_reports_ecosystem_gap() {
        let ecosystem = vec![scope("lib", &["src/lib.rs", "src/**"])];
        let report = lint_scopes(
            &files(&["src/newmod/foo.rs"]),
            &[], // no project scopes at all
            &ecosystem,
            &["src".to_string()],
            &[],
            true, // project_only: ecosystem scopes excluded from coverage
        );
        assert_eq!(report.unscoped_files, vec!["src/newmod/foo.rs".to_string()]);
    }

    #[test]
    fn project_only_false_uses_ecosystem_catchall() {
        let ecosystem = vec![scope("lib", &["src/lib.rs", "src/**"])];
        let report = lint_scopes(
            &files(&["src/newmod/foo.rs"]),
            &[],
            &ecosystem,
            &["src".to_string()],
            &[],
            false, // ecosystem scopes folded into coverage
        );
        assert!(report.unscoped_files.is_empty());
    }

    // ── file_in_roots ────────────────────────────────────────────────

    #[test]
    fn file_in_roots_rejects_sibling_prefix() {
        assert!(!file_in_roots("srcfoo/bar.rs", &["src".to_string()]));
    }

    #[test]
    fn file_in_roots_matches_root_itself_and_subpath() {
        assert!(file_in_roots("src", &["src".to_string()]));
        assert!(file_in_roots("src/main.rs", &["src".to_string()]));
    }

    #[test]
    fn roots_filter_excludes_files_outside_root() {
        let scopes = vec![scope("cli", &["src/cli/**"])];
        let report = lint_scopes(
            &files(&["docs/newmod.md"]),
            &scopes,
            &[],
            &["src".to_string()],
            &[],
            true,
        );
        assert!(report.unscoped_files.is_empty());
        assert_eq!(report.files_checked, 0);
    }

    #[test]
    fn multiple_roots_both_checked() {
        let scopes = vec![scope("cli", &["src/cli/**"])];
        let report = lint_scopes(
            &files(&["src/newmod/foo.rs", "editors/newmod/bar.ts"]),
            &scopes,
            &[],
            &["src".to_string(), "editors".to_string()],
            &[],
            true,
        );
        assert_eq!(report.unscoped_files.len(), 2);
    }

    // ── report exit code ────────────────────────────────────────────

    #[test]
    fn report_exit_code_zero_when_clean() {
        let scopes = vec![scope("cli", &["src/cli/**"])];
        let report = lint_scopes(
            &files(&["src/cli/mod.rs"]),
            &scopes,
            &[],
            &["src".to_string()],
            &[],
            true,
        );
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn report_exit_code_one_when_dead_pattern_only() {
        let scopes = vec![scope("cli", &["src/cli/**"])];
        let report = lint_scopes(&files(&[]), &scopes, &[], &["src".to_string()], &[], true);
        assert_eq!(report.exit_code(), 1);
    }

    // ── CLI-level ────────────────────────────────────────────────────

    #[test]
    fn no_project_only_flag_parses() {
        let cmd = LintCommand::try_parse_from(["lint", "--no-project-only"]).unwrap();
        assert!(cmd.no_project_only);

        let cmd = LintCommand::try_parse_from(["lint"]).unwrap();
        assert!(!cmd.no_project_only);
    }

    #[test]
    fn root_defaults_to_src() {
        let cmd = LintCommand::try_parse_from(["lint"]).unwrap();
        assert_eq!(cmd.root, vec!["src".to_string()]);
    }

    // ── render_report / render_text_report ──────────────────────────
    //
    // Exercised via direct calls rather than only end-to-end, because the
    // "fully clean" render_text_report branches (both "No dead patterns"
    // and "No unscoped files") are unreachable against this repo's own
    // scopes.yaml: dead_patterns ignores `--root`/`--allow` entirely, and
    // this repo currently carries one pre-existing dead pattern (scope
    // "resources") that fixing is out of scope for #1475 — so no CLI
    // invocation against the real tree can produce a clean report.

    fn report(
        project_only: bool,
        dead_patterns: &[(&str, &str)],
        unscoped_files: &[&str],
    ) -> ScopesLintReport {
        ScopesLintReport {
            roots: vec!["src".to_string()],
            project_only,
            scopes_checked: 1,
            files_checked: 1,
            dead_patterns: dead_patterns
                .iter()
                .map(|(scope, pattern)| DeadPattern {
                    scope: (*scope).to_string(),
                    pattern: (*pattern).to_string(),
                })
                .collect(),
            unscoped_files: unscoped_files.iter().map(|f| (*f).to_string()).collect(),
        }
    }

    #[test]
    fn render_report_dispatches_on_format() {
        let clean = report(true, &[], &[]);
        assert!(render_report(&clean, OutputFormat::Text).is_ok());
        assert!(render_report(&clean, OutputFormat::Json).is_ok());
        assert!(render_report(&clean, OutputFormat::Yaml).is_ok());
    }

    #[test]
    fn render_text_report_project_only_with_violations_of_both_kinds() {
        // project_only: true label, non-empty dead patterns (1 entry) and
        // non-empty unscoped files (2 entries) loops, plural "violations".
        let r = report(
            true,
            &[("cli", "src/cli/nonexistent/**")],
            &["src/newmod/a.rs", "src/newmod/b.rs"],
        );
        assert!(render_text_report(&r).is_ok());
    }

    #[test]
    fn render_text_report_ecosystem_label_with_single_violation() {
        // project_only: false label, empty dead patterns checkmark, a
        // single unscoped file (singular "violation").
        let r = report(false, &[], &["src/newmod/a.rs"]);
        assert!(render_text_report(&r).is_ok());
    }

    #[test]
    fn render_text_report_fully_clean() {
        // Both checkmark branches: "No dead patterns" and "No unscoped
        // files" — unreachable end-to-end against this repo (see comment
        // above), so only reachable via a synthetic report.
        let r = report(true, &[], &[]);
        assert!(render_text_report(&r).is_ok());
    }

    /// The `scopes` command dispatches to `lint`; a nonexistent repo path
    /// makes the leaf command error before it ever reaches
    /// `std::process::exit`, which exercises the dispatch path end-to-end
    /// without needing a real repository (mirrors `coverage.rs`'s
    /// `dispatches_to_diff`).
    #[test]
    fn dispatches_to_lint() {
        let cmd = ScopesCommand {
            command: ScopesSubcommands::Lint(LintCommand {
                root: vec!["src".to_string()],
                no_project_only: false,
                allow: Vec::new(),
                output: OutputFormat::Text,
                context_dir: None,
            }),
        };
        let result = cmd.execute(Some(Path::new("/nonexistent/repo/path")));
        assert!(result.is_err());
    }

    // ── run_scopes_lint ecosystem-merge regression (issue #1475) ──────

    /// Creates an empty git-inited tempdir under `$CARGO_MANIFEST_DIR/tmp`,
    /// mirroring `git::repository`'s `init_tmp_repo` test helper.
    fn init_tmp_git_repo() -> tempfile::TempDir {
        let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&tmp_root).unwrap();
        let temp_dir = tempfile::tempdir_in(&tmp_root).unwrap();
        git2::Repository::init(temp_dir.path()).unwrap();
        temp_dir
    }

    /// Stages every file in `dir` so `GitRepository::tracked_files` sees them.
    fn git_add_all(dir: &Path) {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .args(["add", "."])
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// Regression for the bug where `run_scopes_lint` seeded
    /// `merge_ecosystem_scopes` with an empty vector instead of the
    /// project's own scopes: a same-named ecosystem default (npm's `test`
    /// = `["test/**", "tests/**", "**/*.test.js"]`) was added *alongside* a
    /// narrower project-defined `test` scope instead of yielding to it,
    /// silently widening coverage under `--no-project-only` beyond what
    /// real `load_project_scopes` resolution grants —
    /// `docs/plan/config-internals.md`'s documented "user-defined scopes
    /// always win, matched by name" contract.
    #[test]
    fn no_project_only_does_not_duplicate_a_same_named_ecosystem_scope() {
        let repo = init_tmp_git_repo();
        let p = repo.path();
        // package.json (not Cargo.toml) selects the npm ecosystem defaults,
        // which — unlike Rust's `lib` = `src/**` — has no catch-all pattern
        // that would mask the regression this test is pinning.
        std::fs::write(p.join("package.json"), "{}").unwrap();
        std::fs::create_dir_all(p.join("src/foo")).unwrap();
        std::fs::write(p.join("src/foo/helper.test.js"), "// test helper").unwrap();
        let context_dir = p.join(".omni-dev");
        std::fs::create_dir_all(&context_dir).unwrap();
        std::fs::write(
            context_dir.join("scopes.yaml"),
            r#"
scopes:
  - name: test
    description: Narrow project test scope
    examples: []
    file_patterns:
      - "tests/only/**"
"#,
        )
        .unwrap();
        git_add_all(p);

        let cmd = LintCommand {
            root: vec!["src".to_string()],
            no_project_only: true, // --no-project-only: folds in ecosystem defaults
            allow: Vec::new(),
            output: OutputFormat::Text,
            context_dir: Some(context_dir),
        };
        let report = run_scopes_lint(p, &cmd).expect("lint should run against the temp repo");

        // npm's `test` = ["test/**", "tests/**", "**/*.test.js"] must be
        // skipped by name — the project already defines `test` narrowly —
        // so src/foo/helper.test.js (matched only by the ecosystem pattern)
        // stays unscoped, exactly as real `load_project_scopes` resolution
        // would leave it.
        assert_eq!(
            report.unscoped_files,
            vec!["src/foo/helper.test.js".to_string()]
        );
        // Only the genuinely-new ecosystem scopes (deps, config, build,
        // docs — 4 of npm's 5 defaults) are folded in alongside the
        // project's own `test`: 1 + 4 = 5, never a duplicate `test` entry.
        assert_eq!(report.scopes_checked, 5);
    }
}
