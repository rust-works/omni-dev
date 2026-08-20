//! Deterministic, no-AI commit-message lint.
//!
//! [`lint_message`] is the pure predicate behind `omni-dev git commit
//! message lint`. It takes no git handle and does no I/O, so it is
//! exhaustively unit-testable and shared with
//! [`crate::git::commit::CommitInfoForAI::run_pre_validation_checks`] /
//! [`crate::git::commit::refine_message_scope`] via [`parse_subject`] and
//! the scope-check helpers below — one implementation, multiple consumers
//! (#1474).

use std::sync::LazyLock;

use regex::Regex;

use crate::data::check::{CommitIssue, CommitSuggestion, IssueSeverity};
use crate::data::context::{CommitRules, ScopeDefinition};

/// Matches `type(scope)!: description`, `type(scope): description`,
/// `type: description`, and the lenient legacy `type!(scope): description`
/// form (accepted for backward compatibility, though not the documented
/// form). This is the corrected #1473 pattern from the start: the canonical
/// breaking-change `!` is placed *after* the closing paren, not before it.
static SUBJECT_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)] // compile-time constant pattern
    Regex::new(
        r"^(?P<type>[a-z]+)(?P<bang1>!)?(?:\((?P<scope>[^)]*)\))?(?P<bang2>!)?: (?P<desc>.*)$",
    )
    .expect("SUBJECT_RE is a valid compile-time constant regex")
});

/// Matches invalid comma spacing within a (possibly multi-part) scope
/// segment: any whitespace before a comma, or two-or-more spaces after one.
/// Mirrors `.omni-dev/commit-guidelines.md`'s "Two or more spaces after a
/// comma, or any whitespace before a comma, is not permitted."
static BAD_SCOPE_COMMA_RE: LazyLock<Regex> = LazyLock::new(|| {
    #[allow(clippy::expect_used)] // compile-time constant pattern
    Regex::new(r"\s,|,\s{2,}").expect("BAD_SCOPE_COMMA_RE is a valid compile-time constant regex")
});

/// A parsed conventional-commit subject line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSubject<'a> {
    /// The `type` token, e.g. `feat`.
    pub commit_type: &'a str,
    /// The raw `(scope)` contents, un-split on commas, when present and
    /// non-empty.
    pub scope: Option<&'a str>,
    /// Everything after the `: ` separator.
    pub description: &'a str,
    /// Whether a breaking-change `!` marker was present, either side of the
    /// scope parens.
    pub breaking: bool,
}

/// Parses a commit subject line (the first line of a commit message) into
/// its conventional-commit parts. Returns `None` when the subject doesn't
/// match `type(scope)?!?: description` in any accepted form.
pub fn parse_subject(first_line: &str) -> Option<ParsedSubject<'_>> {
    let caps = SUBJECT_RE.captures(first_line)?;
    let commit_type = caps.name("type")?.as_str();
    let description = caps.name("desc")?.as_str();
    let breaking = caps.name("bang1").is_some() || caps.name("bang2").is_some();
    let scope = caps
        .name("scope")
        .map(|m| m.as_str())
        .filter(|s| !s.is_empty());
    Some(ParsedSubject {
        commit_type,
        scope,
        description,
        breaking,
    })
}

/// Returns `true` when a multi-scope segment's comma spacing is acceptable
/// (`,` or `, ` only — no space before a comma, no 2+ spaces after one).
pub(crate) fn scope_comma_format_ok(scope: &str) -> bool {
    !BAD_SCOPE_COMMA_RE.is_match(scope)
}

/// Returns `true` when every comma-separated part of `scope` is a member of
/// `valid_scopes` (already ecosystem-merged by the caller — see
/// [`crate::claude::context::load_project_scopes`]). An empty `valid_scopes`
/// list is treated as "anything goes" (no `scopes.yaml` configured),
/// matching the pre-existing `run_pre_validation_checks` behaviour.
pub(crate) fn scope_parts_all_valid(scope: &str, valid_scopes: &[ScopeDefinition]) -> bool {
    valid_scopes.is_empty() || invalid_scope_parts(scope, valid_scopes).is_empty()
}

/// Returns the comma-separated parts of `scope` that are not present in
/// `valid_scopes`, trimmed.
fn invalid_scope_parts<'a>(scope: &'a str, valid_scopes: &[ScopeDefinition]) -> Vec<&'a str> {
    scope
        .split(',')
        .map(str::trim)
        .filter(|part| !valid_scopes.iter().any(|s| s.name == *part))
        .collect()
}

fn issue(severity: IssueSeverity, section: &str, rule: &str, explanation: String) -> CommitIssue {
    CommitIssue {
        severity,
        section: section.to_string(),
        rule: rule.to_string(),
        explanation,
    }
}

/// Returns `true` when `line` starts with `footer` (case-insensitive)
/// immediately followed by `:`, e.g. `line = "Co-Authored-By: x"`,
/// `footer = "Co-Authored-By"`. Byte-slices via `str::get` rather than
/// direct indexing so a `footer` longer than `line`, or a split that lands
/// mid-codepoint, returns `false` instead of panicking.
fn line_has_forbidden_footer(line: &str, footer: &str) -> bool {
    let Some(prefix) = line.get(..footer.len()) else {
        return false;
    };
    prefix.eq_ignore_ascii_case(footer) && line[footer.len()..].starts_with(':')
}

/// Runs every deterministic rule against `message`.
///
/// Returns the issues found (empty = passes cleanly). Pure — no git, no
/// I/O; `rules` and `valid_scopes` are supplied by the caller (already
/// resolved/merged, e.g. via [`crate::claude::context::load_project_scopes`]
/// / [`crate::claude::context::load_commit_rules`]).
pub fn lint_message(
    message: &str,
    rules: &CommitRules,
    valid_scopes: &[ScopeDefinition],
) -> Vec<CommitIssue> {
    let mut issues = Vec::new();

    let first_line = message.lines().next().unwrap_or("");

    // Subject length — independent of whether the subject otherwise parses.
    let len = first_line.chars().count();
    if len > rules.subject_max_len {
        issues.push(issue(
            IssueSeverity::Error,
            "Subject Line",
            "subject-length",
            format!(
                "Subject is {len} characters, which exceeds the {}-character limit",
                rules.subject_max_len
            ),
        ));
    }

    // Blank line after subject when a body is present — independent of
    // whether the subject otherwise parses.
    if let Some(second_line) = message.lines().nth(1) {
        if !second_line.is_empty() {
            issues.push(issue(
                IssueSeverity::Error,
                "Commit Format",
                "blank-line-after-subject",
                "Line 2 must be blank when the commit message has a body".to_string(),
            ));
        }
    }

    // Forbidden footers — independent of whether the subject otherwise
    // parses; scanned across every line of the message.
    for line in message.lines() {
        for footer in &rules.forbidden_footers {
            if line_has_forbidden_footer(line, footer) {
                issues.push(issue(
                    IssueSeverity::Warning,
                    "Body Guidelines",
                    "forbidden-footer",
                    format!("'{footer}' footer is not permitted"),
                ));
            }
        }
    }

    let Some(parsed) = parse_subject(first_line) else {
        issues.push(issue(
            IssueSeverity::Error,
            "Commit Format",
            "format",
            "Subject must match '<type>(<scope>): <description>'".to_string(),
        ));
        return issues;
    };

    if !rules.types.iter().any(|t| t == parsed.commit_type) {
        issues.push(issue(
            IssueSeverity::Error,
            "Types",
            "unknown-type",
            format!(
                "'{}' is not one of the accepted types: {}",
                parsed.commit_type,
                rules.types.join(", ")
            ),
        ));
    }

    match parsed.scope {
        Some(scope) => {
            if !scope_comma_format_ok(scope) {
                issues.push(issue(
                    IssueSeverity::Error,
                    "Scopes",
                    "scope-comma-format",
                    format!("Scope '{scope}' must separate multiple scopes with ',' or ', ' only"),
                ));
            }

            let invalid = invalid_scope_parts(scope, valid_scopes);
            if !valid_scopes.is_empty() && !invalid.is_empty() {
                issues.push(issue(
                    IssueSeverity::Error,
                    "Scopes",
                    "unknown-scope",
                    format!(
                        "Scope(s) not in the valid scopes list: {}",
                        invalid.join(", ")
                    ),
                ));
            }
        }
        None if rules.require_scope => {
            issues.push(issue(
                IssueSeverity::Error,
                "Scopes",
                "missing-scope",
                "A scope is required, e.g. 'type(scope): description'".to_string(),
            ));
        }
        None => {}
    }

    if parsed
        .description
        .chars()
        .next()
        .is_some_and(char::is_uppercase)
    {
        issues.push(issue(
            IssueSeverity::Info,
            "Subject Line Style",
            "lowercase-description",
            "Description should start with a lowercase letter".to_string(),
        ));
    }

    if first_line.trim_end().ends_with('.') {
        issues.push(issue(
            IssueSeverity::Info,
            "Subject Line Style",
            "no-trailing-period",
            "Subject should not end with a period".to_string(),
        ));
    }

    issues
}

/// Returns `true` when `issues` contains no [`IssueSeverity::Error`] entry —
/// the same pass/fail split [`crate::data::check::CheckReport::exit_code`]
/// uses.
#[must_use]
pub fn passes(issues: &[CommitIssue]) -> bool {
    !issues.iter().any(|i| i.severity == IssueSeverity::Error)
}

/// Deterministically suggests a corrected message for an `unknown-scope` or
/// `missing-scope` issue.
///
/// Resolves a scope from `files` against `valid_scopes` via
/// [`crate::git::commit::resolve_scope`] — no AI, no network. Returns `None`
/// when there's nothing to suggest: neither issue is present, the subject
/// doesn't parse, `resolve_scope` can't resolve anything from `files`, or
/// the resolved scope doesn't actually change the message.
///
/// An existing-but-wrong scope (`unknown-scope`) is *replaced* via
/// [`crate::git::commit::refine_message_scope`]. A missing scope
/// (`missing-scope`, only reachable when `rules.require_scope` is set) is
/// *inserted* between the type (and any breaking-change `!`) and the colon —
/// `refine_message_scope` is deliberately not reused for this case, since its
/// contract is "replace an existing scope," never "add one."
pub fn suggest_scope_fix(
    message: &str,
    files: &[&str],
    valid_scopes: &[ScopeDefinition],
    issues: &[CommitIssue],
) -> Option<CommitSuggestion> {
    if !issues
        .iter()
        .any(|i| i.rule == "unknown-scope" || i.rule == "missing-scope")
    {
        return None;
    }

    let first_line = message.lines().next().unwrap_or("");
    let parsed = parse_subject(first_line)?;

    let corrected = if parsed.scope.is_some() {
        let refined = crate::git::commit::refine_message_scope(message, files, valid_scopes);
        if refined == message {
            return None;
        }
        refined
    } else {
        let resolved = crate::git::commit::resolve_scope(files, valid_scopes)?;
        // Canonical placement: the breaking-change `!` goes after the scope
        // parens, matching `SUBJECT_RE`'s documented form (see the module
        // doc comment on `refine_message_scope`'s sibling regex in this
        // file) — not before them, which is only accepted leniently on
        // input.
        let bang = if parsed.breaking { "!" } else { "" };
        let new_first_line = format!(
            "{}({resolved}){bang}: {}",
            parsed.commit_type, parsed.description
        );
        match message.split_once('\n') {
            Some((_, rest)) => format!("{new_first_line}\n{rest}"),
            None => new_first_line,
        }
    };

    Some(CommitSuggestion {
        message: corrected,
        explanation: "Deterministically resolved from the commit's changed files against the \
                       project's scope definitions (no AI)."
            .to_string(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn scope(name: &str) -> ScopeDefinition {
        ScopeDefinition {
            name: name.to_string(),
            description: String::new(),
            examples: vec![],
            file_patterns: vec![],
        }
    }

    fn scopes(names: &[&str]) -> Vec<ScopeDefinition> {
        names.iter().map(|n| scope(n)).collect()
    }

    fn issue_rules<'a>(rule: &str, issues: &'a [CommitIssue]) -> Vec<&'a CommitIssue> {
        issues.iter().filter(|i| i.rule == rule).collect()
    }

    // ── parse_subject ────────────────────────────────────────────────

    #[test]
    fn parse_subject_simple() {
        let p = parse_subject("feat(cli): add twiddle contextual options").unwrap();
        assert_eq!(p.commit_type, "feat");
        assert_eq!(p.scope, Some("cli"));
        assert_eq!(p.description, "add twiddle contextual options");
        assert!(!p.breaking);
    }

    #[test]
    fn parse_subject_scope_less() {
        let p = parse_subject("docs: clarify dry_run helper scope").unwrap();
        assert_eq!(p.commit_type, "docs");
        assert_eq!(p.scope, None);
    }

    #[test]
    fn parse_subject_canonical_breaking_change() {
        // #1473: `!` after the closing paren is the documented form and
        // must match.
        let p = parse_subject("feat(cli)!: change commit check output format").unwrap();
        assert_eq!(p.commit_type, "feat");
        assert_eq!(p.scope, Some("cli"));
        assert!(p.breaking);
    }

    #[test]
    fn parse_subject_lenient_legacy_breaking_change() {
        let p = parse_subject("feat!(cli): add thing").unwrap();
        assert_eq!(p.scope, Some("cli"));
        assert!(p.breaking);
    }

    #[test]
    fn parse_subject_both_bangs_accepted_leniently() {
        assert!(parse_subject("feat!(cli)!: add thing").is_some());
    }

    #[test]
    fn parse_subject_multi_scope() {
        let p = parse_subject("feat(git,data): integrate branch analysis").unwrap();
        assert_eq!(p.scope, Some("git,data"));
    }

    #[test]
    fn parse_subject_rejects_missing_colon_space() {
        assert!(parse_subject("feat(cli):no space").is_none());
    }

    #[test]
    fn parse_subject_rejects_garbage() {
        assert!(parse_subject("this is not a conventional commit").is_none());
    }

    // ── lint_message: Rule 1 — format ───────────────────────────────

    #[test]
    fn format_valid_passes() {
        let rules = CommitRules::default();
        let issues = lint_message("feat(cli): add twiddle contextual options", &rules, &[]);
        assert!(issue_rules("format", &issues).is_empty());
        assert!(passes(&issues));
    }

    #[test]
    fn format_invalid_flags_error() {
        let rules = CommitRules::default();
        let issues = lint_message("not a conventional commit at all", &rules, &[]);
        let found = issue_rules("format", &issues);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].severity, IssueSeverity::Error);
        assert_eq!(found[0].section, "Commit Format");
    }

    // ── lint_message: Rule 2 — types ────────────────────────────────

    #[test]
    fn known_type_passes() {
        let rules = CommitRules::default();
        let issues = lint_message("feat(cli): add thing", &rules, &[]);
        assert!(issue_rules("unknown-type", &issues).is_empty());
    }

    #[test]
    fn unknown_type_flags_error() {
        let rules = CommitRules::default();
        let issues = lint_message("feature(cli): add thing", &rules, &[]);
        let found = issue_rules("unknown-type", &issues);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].severity, IssueSeverity::Error);
        assert_eq!(found[0].section, "Types");
    }

    // ── lint_message: Rule 3 — scope comma format ───────────────────

    #[test]
    fn scope_comma_format_valid_passes() {
        let rules = CommitRules::default();
        let valid = scopes(&["cli", "claude"]);
        let issues = lint_message("feat(cli,claude): add thing", &rules, &valid);
        assert!(issue_rules("scope-comma-format", &issues).is_empty());

        let issues_spaced = lint_message("feat(cli, claude): add thing", &rules, &valid);
        assert!(issue_rules("scope-comma-format", &issues_spaced).is_empty());
    }

    #[test]
    fn scope_comma_two_spaces_after_flags_error() {
        let rules = CommitRules::default();
        let valid = scopes(&["a", "b"]);
        let issues = lint_message("feat(a,  b): add thing", &rules, &valid);
        let found = issue_rules("scope-comma-format", &issues);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].severity, IssueSeverity::Error);
    }

    #[test]
    fn scope_comma_space_before_flags_error() {
        let rules = CommitRules::default();
        let valid = scopes(&["a", "b"]);
        let issues = lint_message("feat(a ,b): add thing", &rules, &valid);
        assert_eq!(issue_rules("scope-comma-format", &issues).len(), 1);
    }

    // ── lint_message: Rule 4 — scope validity ───────────────────────

    #[test]
    fn valid_scope_passes() {
        let rules = CommitRules::default();
        let valid = scopes(&["cli"]);
        let issues = lint_message("feat(cli): add thing", &rules, &valid);
        assert!(issue_rules("unknown-scope", &issues).is_empty());
    }

    #[test]
    fn undefined_scope_flags_error() {
        let rules = CommitRules::default();
        let valid = scopes(&["cli"]);
        let issues = lint_message("feat(bogus): add thing", &rules, &valid);
        let found = issue_rules("unknown-scope", &issues);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].severity, IssueSeverity::Error);
        assert_eq!(found[0].section, "Scopes");
    }

    #[test]
    fn empty_valid_scopes_list_accepts_anything() {
        // No scopes.yaml configured — mirrors run_pre_validation_checks.
        let rules = CommitRules::default();
        let issues = lint_message("feat(anything): add thing", &rules, &[]);
        assert!(issue_rules("unknown-scope", &issues).is_empty());
    }

    // ── lint_message: Rule 5 — require_scope toggle ─────────────────

    #[test]
    fn missing_scope_allowed_by_default() {
        let rules = CommitRules::default();
        assert!(!rules.require_scope);
        let issues = lint_message("docs: update issue references", &rules, &[]);
        assert!(issue_rules("missing-scope", &issues).is_empty());
    }

    #[test]
    fn missing_scope_flagged_when_required() {
        let rules = CommitRules {
            require_scope: true,
            ..CommitRules::default()
        };
        let issues = lint_message("docs: update issue references", &rules, &[]);
        let found = issue_rules("missing-scope", &issues);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].severity, IssueSeverity::Error);
    }

    #[test]
    fn present_scope_not_flagged_when_required() {
        let rules = CommitRules {
            require_scope: true,
            ..CommitRules::default()
        };
        let issues = lint_message("docs(docs): update issue references", &rules, &[]);
        assert!(issue_rules("missing-scope", &issues).is_empty());
    }

    // ── lint_message: Rules 6/7 — subject line style ────────────────

    #[test]
    fn lowercase_description_passes() {
        let rules = CommitRules::default();
        let issues = lint_message("feat(cli): add thing", &rules, &[]);
        assert!(issue_rules("lowercase-description", &issues).is_empty());
    }

    #[test]
    fn uppercase_description_flags_info() {
        let rules = CommitRules::default();
        let issues = lint_message("feat(cli): Add thing", &rules, &[]);
        let found = issue_rules("lowercase-description", &issues);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].severity, IssueSeverity::Info);
        // Info-only issues never fail the commit.
        assert!(passes(&found_as_issues(&issues, "lowercase-description")));
    }

    fn found_as_issues(issues: &[CommitIssue], rule: &str) -> Vec<CommitIssue> {
        issues.iter().filter(|i| i.rule == rule).cloned().collect()
    }

    #[test]
    fn no_trailing_period_passes() {
        let rules = CommitRules::default();
        let issues = lint_message("feat(cli): add thing", &rules, &[]);
        assert!(issue_rules("no-trailing-period", &issues).is_empty());
    }

    #[test]
    fn trailing_period_flags_info() {
        let rules = CommitRules::default();
        let issues = lint_message("feat(cli): add thing.", &rules, &[]);
        let found = issue_rules("no-trailing-period", &issues);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].severity, IssueSeverity::Info);
    }

    // ── lint_message: Rule 8 — subject length ───────────────────────

    #[test]
    fn subject_within_limit_passes() {
        let rules = CommitRules::default();
        let issues = lint_message("feat(cli): add thing", &rules, &[]);
        assert!(issue_rules("subject-length", &issues).is_empty());
    }

    #[test]
    fn subject_over_limit_flags_error() {
        let rules = CommitRules::default();
        let long_desc = "x".repeat(rules.subject_max_len);
        let msg = format!("feat(cli): {long_desc}");
        assert!(msg.chars().count() > rules.subject_max_len);
        let issues = lint_message(&msg, &rules, &[]);
        let found = issue_rules("subject-length", &issues);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].severity, IssueSeverity::Error);
    }

    #[test]
    fn configured_limit_is_honoured() {
        // Same message: passes at 80, fails at 72. Built programmatically
        // (rather than hand-counted) to land reliably between the two.
        let prefix = "feat(cli): ";
        let desc = "x".repeat(73 - prefix.len());
        let msg = format!("{prefix}{desc}");
        assert_eq!(msg.chars().count(), 73);

        let rules_80 = CommitRules {
            subject_max_len: 80,
            ..CommitRules::default()
        };
        assert!(issue_rules("subject-length", &lint_message(&msg, &rules_80, &[])).is_empty());

        let rules_72 = CommitRules {
            subject_max_len: 72,
            ..CommitRules::default()
        };
        assert_eq!(
            issue_rules("subject-length", &lint_message(&msg, &rules_72, &[])).len(),
            1
        );
    }

    // ── lint_message: Rule 9 — blank line after subject ─────────────

    #[test]
    fn blank_line_2_with_body_passes() {
        let rules = CommitRules::default();
        let issues = lint_message("feat(cli): add thing\n\nMore detail here.", &rules, &[]);
        assert!(issue_rules("blank-line-after-subject", &issues).is_empty());
    }

    #[test]
    fn subject_only_no_body_passes() {
        let rules = CommitRules::default();
        let issues = lint_message("feat(cli): add thing", &rules, &[]);
        assert!(issue_rules("blank-line-after-subject", &issues).is_empty());
    }

    #[test]
    fn non_blank_line_2_flags_error() {
        // The 6413fd73 case: a clean subject, non-blank line 2 folds the
        // whole first paragraph into git's %s.
        let rules = CommitRules::default();
        let issues = lint_message(
            "fix(git): handle detached HEAD in branch analysis\nThis line should be blank.",
            &rules,
            &[],
        );
        let found = issue_rules("blank-line-after-subject", &issues);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].severity, IssueSeverity::Error);
        assert_eq!(found[0].section, "Commit Format");
    }

    // ── lint_message: Rule 10 — forbidden footers ───────────────────

    #[test]
    fn no_footer_passes() {
        let rules = CommitRules::default();
        let issues = lint_message(
            "feat(cli): add thing\n\nBody text.\n\nCloses #123",
            &rules,
            &[],
        );
        assert!(issue_rules("forbidden-footer", &issues).is_empty());
    }

    #[test]
    fn co_authored_by_footer_flags_warning() {
        let rules = CommitRules::default();
        let issues = lint_message(
            "feat(cli): add thing\n\nBody text.\n\nCo-Authored-By: Claude <noreply@anthropic.com>",
            &rules,
            &[],
        );
        let found = issue_rules("forbidden-footer", &issues);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].severity, IssueSeverity::Warning);
        assert_eq!(found[0].section, "Body Guidelines");
        // A warning alone must not fail the commit.
        assert!(passes(&found_as_issues(&issues, "forbidden-footer")));
    }

    #[test]
    fn co_authored_by_footer_case_insensitive() {
        let rules = CommitRules::default();
        let issues = lint_message(
            "feat(cli): add thing\n\nco-authored-by: someone",
            &rules,
            &[],
        );
        assert_eq!(issue_rules("forbidden-footer", &issues).len(), 1);
    }

    // ── ecosystem-default scopes (the case most likely to be got wrong) ──

    #[test]
    fn ecosystem_default_scopes_pass_even_when_absent_from_scopes_yaml() {
        // Mirrors merge_ecosystem_scopes' Rust defaults: lib, cargo, core,
        // test are accepted even though a project's scopes.yaml lists
        // neither — the caller is responsible for merging them in before
        // calling lint_message (this test simulates that merged list).
        let rules = CommitRules::default();
        let merged = scopes(&["cli", "git", "lib", "cargo", "core", "test"]);
        for s in ["lib", "cargo", "core", "test"] {
            let msg = format!("chore({s}): bump dependency");
            let issues = lint_message(&msg, &rules, &merged);
            assert!(
                issue_rules("unknown-scope", &issues).is_empty(),
                "ecosystem default scope {s} should be accepted"
            );
        }
    }

    // ── real positive/negative fixtures from the issue ──────────────

    #[test]
    fn positive_fixtures_from_history_pass_cleanly() {
        let rules = CommitRules::default();
        let valid = scopes(&["cli", "claude", "docs"]);
        for msg in [
            "feat(cli,claude): add twiddle contextual options",
            "feat(cli)!: change commit check output format",
            "docs(docs): add architecture overview document",
        ] {
            let issues = lint_message(msg, &rules, &valid);
            assert!(passes(&issues), "{msg:?} should pass cleanly: {issues:?}");
        }
    }

    #[test]
    fn multi_scope_breaking_change_records_both_checks() {
        let rules = CommitRules::default();
        let valid = scopes(&["cli", "claude"]);
        let issues = lint_message("feat(cli,claude)!: add thing", &rules, &valid);
        assert!(passes(&issues));
    }

    // ── suggest_scope_fix ────────────────────────────────────────────

    fn scope_with_patterns(name: &str, patterns: &[&str]) -> ScopeDefinition {
        ScopeDefinition {
            name: name.to_string(),
            description: String::new(),
            examples: vec![],
            file_patterns: patterns.iter().map(|p| (*p).to_string()).collect(),
        }
    }

    #[test]
    fn suggest_scope_fix_replaces_unknown_scope() {
        let valid = vec![scope_with_patterns("cargo", &["Cargo.toml", "Cargo.lock"])];
        let message = "chore(deps): bump the rust-minor-patch group";
        let issues = lint_message(message, &CommitRules::default(), &valid);
        let suggestion =
            suggest_scope_fix(message, &["Cargo.toml"], &valid, &issues).expect("should suggest");
        assert_eq!(
            suggestion.message,
            "chore(cargo): bump the rust-minor-patch group"
        );
    }

    #[test]
    fn suggest_scope_fix_inserts_missing_scope() {
        let valid = vec![scope_with_patterns("cargo", &["Cargo.toml"])];
        let rules = CommitRules {
            require_scope: true,
            ..CommitRules::default()
        };
        let message = "chore: bump deps";
        let issues = lint_message(message, &rules, &valid);
        let suggestion =
            suggest_scope_fix(message, &["Cargo.toml"], &valid, &issues).expect("should suggest");
        assert_eq!(suggestion.message, "chore(cargo): bump deps");
    }

    #[test]
    fn suggest_scope_fix_inserts_missing_scope_preserves_breaking_bang() {
        let valid = vec![scope_with_patterns("cargo", &["Cargo.toml"])];
        let rules = CommitRules {
            require_scope: true,
            ..CommitRules::default()
        };
        let message = "chore!: bump deps";
        let issues = lint_message(message, &rules, &valid);
        let suggestion =
            suggest_scope_fix(message, &["Cargo.toml"], &valid, &issues).expect("should suggest");
        assert_eq!(suggestion.message, "chore(cargo)!: bump deps");
    }

    #[test]
    fn suggest_scope_fix_preserves_body() {
        let valid = vec![scope_with_patterns("cargo", &["Cargo.toml"])];
        let message = "chore(deps): bump deps\n\nSome body text.";
        let issues = lint_message(message, &CommitRules::default(), &valid);
        let suggestion =
            suggest_scope_fix(message, &["Cargo.toml"], &valid, &issues).expect("should suggest");
        assert_eq!(
            suggestion.message,
            "chore(cargo): bump deps\n\nSome body text."
        );
    }

    #[test]
    fn suggest_scope_fix_no_issue_no_suggestion() {
        let valid = vec![scope_with_patterns("cargo", &["Cargo.toml"])];
        let message = "chore(cargo): bump deps";
        let issues = lint_message(message, &CommitRules::default(), &valid);
        assert!(issues.is_empty());
        assert!(suggest_scope_fix(message, &["Cargo.toml"], &valid, &issues).is_none());
    }

    #[test]
    fn suggest_scope_fix_no_matching_scope_defs_no_suggestion() {
        let valid = vec![scope_with_patterns("docs", &["docs/**"])];
        let message = "chore(deps): bump deps";
        let issues = lint_message(message, &CommitRules::default(), &valid);
        assert!(suggest_scope_fix(message, &["Cargo.toml"], &valid, &issues).is_none());
    }

    #[test]
    fn suggest_scope_fix_format_broken_subject_no_suggestion() {
        let valid = vec![scope_with_patterns("cargo", &["Cargo.toml"])];
        let message = "not a conventional commit subject";
        let issues = lint_message(message, &CommitRules::default(), &valid);
        assert_eq!(issue_rules("format", &issues).len(), 1);
        assert!(suggest_scope_fix(message, &["Cargo.toml"], &valid, &issues).is_none());
    }

    #[test]
    fn suggest_scope_fix_tied_scopes_join_with_valid_comma_spacing() {
        let valid = vec![
            scope_with_patterns("cargo", &["Cargo.toml"]),
            scope_with_patterns("lib", &["Cargo.toml"]),
        ];
        let message = "chore(deps): bump deps";
        let issues = lint_message(message, &CommitRules::default(), &valid);
        let suggestion =
            suggest_scope_fix(message, &["Cargo.toml"], &valid, &issues).expect("should suggest");
        assert_eq!(suggestion.message, "chore(cargo, lib): bump deps");
        // The joined scope must itself pass the comma-format rule.
        let rechecked = lint_message(&suggestion.message, &CommitRules::default(), &valid);
        assert!(issue_rules("scope-comma-format", &rechecked).is_empty());
    }
}
