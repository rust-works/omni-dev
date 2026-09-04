//! Deserialize mirrors of the daemon's `worktrees`/`sessions` wire payloads.
//!
//! These are hand-written against the documented JSON contract rather than
//! `use`-imports of the daemon-side types (`src/daemon/services/worktrees.rs`,
//! `src/sessions.rs`), which are mostly private and `Serialize`-only — so this
//! client only breaks if the wire contract itself changes, not if the
//! daemon's internal struct visibility does. Every field is
//! `#[serde(default)]` for forward-compat with a newer daemon, matching the
//! project's existing wire-type convention (`src/daemon/protocol.rs`).
//!
//! Every free-text field (branch names, repo names, GitHub identities,
//! operation labels, window keys, session repo/model ids, PR URLs) is
//! sanitized via [`sanitize_for_terminal`] at this JSON boundary — the
//! earliest point a malicious branch name or a `register` payload could
//! otherwise inject raw ANSI escapes into the terminal.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};

use crate::cli::format::sanitize_for_terminal;
use crate::pr_status::PrCheckState;
use crate::sessions::{SessionState, Source};

fn sanitized<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(sanitize_for_terminal(&String::deserialize(deserializer)?))
}

fn sanitized_opt<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.map(|s| sanitize_for_terminal(&s)))
}

/// The `worktrees` service's `tree`/`subscribe` payload:
/// `{ "repos": [...], "show_closed": bool }`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TreeSnapshotWire {
    #[serde(default)]
    pub repos: Vec<TreeRepoWire>,
    #[serde(default)]
    pub show_closed: bool,
}

/// A GitHub `owner/name` identity, present only for a `github.com` remote.
#[derive(Debug, Clone, Deserialize)]
pub struct GithubIdentityWire {
    #[serde(default, deserialize_with = "sanitized")]
    pub owner: String,
    #[serde(default, deserialize_with = "sanitized")]
    pub name: String,
}

/// One repository (with all its worktrees) in the `tree` payload.
#[derive(Debug, Clone, Deserialize)]
pub struct TreeRepoWire {
    #[serde(default, deserialize_with = "sanitized")]
    pub main_repo: String,
    #[serde(default)]
    pub github: Option<GithubIdentityWire>,
    #[serde(default, deserialize_with = "sanitized")]
    pub root: String,
    #[serde(default)]
    pub polling_enabled: bool,
    #[serde(default)]
    pub worktrees: Vec<TreeWorktreeWire>,
}

/// One worktree of a repository in the `tree` payload. Ahead/behind
/// divergence is deliberately absent — fetched lazily via the `ahead-behind`
/// op (see [`super::ahead_behind`]), matching the daemon's own snapshot.
#[derive(Debug, Clone, Deserialize)]
pub struct TreeWorktreeWire {
    #[serde(default, deserialize_with = "sanitized")]
    pub path: String,
    #[serde(default, deserialize_with = "sanitized_opt")]
    pub branch: Option<String>,
    #[serde(default)]
    pub head_sha: Option<String>,
    #[serde(default)]
    pub upstream_sha: Option<String>,
    #[serde(default)]
    pub is_main: bool,
    #[serde(default)]
    pub open: bool,
    #[serde(default, deserialize_with = "sanitized_opt")]
    pub window_key: Option<String>,
    #[serde(default)]
    pub pr: Option<PrBadgeWire>,
    #[serde(default)]
    pub pr_none: bool,
    #[serde(default, deserialize_with = "sanitized_opt")]
    pub operation: Option<String>,
    #[serde(default)]
    pub rebasing: bool,
    #[serde(default)]
    pub pushing: bool,
}

/// The PR badge on a worktree row. `isDraft` is camelCase on the wire —
/// inherited from `gh`'s JSON output, per the daemon-side type's own doc
/// comment (`src/pr_status.rs`).
#[derive(Debug, Clone, Deserialize)]
pub struct PrBadgeWire {
    pub number: u64,
    #[serde(rename = "isDraft", default)]
    pub is_draft: bool,
    /// The daemon always includes this field whenever `pr` itself is
    /// present, but a single missing/malformed `checks` must not fail
    /// deserialization of the *entire* snapshot the way a required field
    /// would — it degrades to "no checks reported" instead.
    #[serde(default = "default_pr_check_state")]
    pub checks: PrCheckState,
    #[serde(default, deserialize_with = "sanitized")]
    pub url: String,
}

fn default_pr_check_state() -> PrCheckState {
    PrCheckState::None
}

/// The `sessions` service's `list`/`subscribe` payload: `{ "sessions": [...] }`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SessionsListWire {
    #[serde(default)]
    pub sessions: Vec<SessionEntryWire>,
}

/// One live Claude Code session. `state`/`last_seen` are required — a session
/// without them is not meaningful and should fail to parse rather than
/// silently render as something plausible.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionEntryWire {
    #[serde(default, deserialize_with = "sanitized")]
    pub session_id: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// Not consumed by [`super::view_model::merge`] (which joins a session
    /// onto a worktree row by `cwd` prefix, not by this field); kept as a
    /// faithful mirror of the wire contract for a future per-session repo
    /// label.
    #[allow(dead_code)]
    #[serde(default, deserialize_with = "sanitized_opt")]
    pub repo: Option<String>,
    #[serde(default, deserialize_with = "sanitized_opt")]
    pub model: Option<String>,
    pub state: SessionState,
    #[serde(default = "default_source")]
    pub source: Source,
    pub last_seen: DateTime<Utc>,
}

fn default_source() -> Source {
    Source::Terminal
}

/// One entry of the `ahead-behind` op's `results` map. A path the daemon
/// omits entirely (no upstream to compare against) is *not* represented here
/// — see [`super::client::WorktreesClient::fetch_ahead_behind`].
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct AheadBehindEntryWire {
    #[serde(default)]
    pub ahead: Option<usize>,
    #[serde(default)]
    pub behind: Option<usize>,
    #[serde(default)]
    pub main_behind: Option<usize>,
}

/// The `close` op's phase-1 safety report (`{ path, remove: true }`,
/// unconfirmed) — mirrors the daemon's private `SafetyReport`
/// (`src/daemon/services/worktrees.rs`). `removable && risks.is_empty()`
/// means "proceed with no confirm"; any `risks` entry means "show one".
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SafetyReportWire {
    #[serde(default)]
    pub removable: bool,
    #[serde(default)]
    pub is_main: bool,
    #[serde(default)]
    pub open: bool,
    #[serde(default, deserialize_with = "sanitized_opt")]
    pub window_key: Option<String>,
    #[serde(default)]
    pub window_folder_count: usize,
    #[serde(default)]
    pub risks: Vec<CloseNoteWire>,
    #[serde(default)]
    pub info: Vec<CloseNoteWire>,
}

/// One risk/info note in a [`SafetyReportWire`]: a machine `kind` slug and a
/// human-readable `detail` — mirrors the daemon's private `Note`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct CloseNoteWire {
    #[serde(default, deserialize_with = "sanitized")]
    pub kind: String,
    #[serde(default, deserialize_with = "sanitized")]
    pub detail: String,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn tree_worktree_wire_sanitizes_a_malicious_branch_name() {
        let json = serde_json::json!({
            "path": "/repo/wt",
            "branch": "evil\u{1b}[31mrepo",
            "is_main": false,
            "open": false,
        });
        let wt: TreeWorktreeWire = serde_json::from_value(json).unwrap();
        assert_eq!(wt.branch.as_deref(), Some("evil[31mrepo"));
    }

    #[test]
    fn safety_report_wire_parses_fields_and_notes() {
        let json = serde_json::json!({
            "removable": true,
            "is_main": false,
            "open": true,
            "window_key": "w1",
            "window_folder_count": 2,
            "risks": [{ "kind": "dirty", "detail": "uncommitted changes" }],
            "info": [{ "kind": "unpushed", "detail": "2 unpushed commits" }],
        });
        let report: SafetyReportWire = serde_json::from_value(json).unwrap();
        assert!(report.removable);
        assert!(!report.is_main);
        assert!(report.open);
        assert_eq!(report.window_key.as_deref(), Some("w1"));
        assert_eq!(report.window_folder_count, 2);
        assert_eq!(report.risks.len(), 1);
        assert_eq!(report.risks[0].kind, "dirty");
        assert_eq!(report.info[0].detail, "2 unpushed commits");
    }

    #[test]
    fn safety_report_wire_defaults_missing_fields() {
        let report: SafetyReportWire =
            serde_json::from_value(serde_json::json!({ "removable": false, "is_main": true }))
                .unwrap();
        assert!(!report.removable);
        assert!(report.is_main);
        assert!(!report.open);
        assert!(report.window_key.is_none());
        assert!(report.risks.is_empty());
    }

    #[test]
    fn tree_snapshot_wire_defaults_missing_fields() {
        let wt: TreeWorktreeWire =
            serde_json::from_value(serde_json::json!({ "path": "/repo/wt" })).unwrap();
        assert_eq!(wt.path, "/repo/wt");
        assert!(wt.branch.is_none());
        assert!(!wt.open);
        assert!(!wt.pushing);
    }

    #[test]
    fn pr_badge_wire_reads_camel_case_is_draft() {
        let pr: PrBadgeWire = serde_json::from_value(serde_json::json!({
            "number": 42,
            "isDraft": true,
            "checks": "pending",
            "url": "https://github.com/o/r/pull/42",
        }))
        .unwrap();
        assert_eq!(pr.number, 42);
        assert!(pr.is_draft);
        assert_eq!(pr.checks, PrCheckState::Pending);
    }

    #[test]
    fn pr_badge_wire_defaults_missing_checks_instead_of_failing_to_parse() {
        let pr: PrBadgeWire = serde_json::from_value(serde_json::json!({
            "number": 42,
            "isDraft": false,
            "url": "https://github.com/o/r/pull/42",
        }))
        .unwrap();
        assert_eq!(pr.checks, PrCheckState::None);
    }

    #[test]
    fn session_entry_wire_sanitizes_repo_and_model() {
        let json = serde_json::json!({
            "session_id": "abc",
            "repo": "evil\nrepo",
            "model": "claude-sonnet\u{7f}",
            "state": "working",
            "source": { "kind": "terminal" },
            "last_seen": "2026-01-01T00:00:00Z",
        });
        let entry: SessionEntryWire = serde_json::from_value(json).unwrap();
        assert_eq!(entry.repo.as_deref(), Some("evilrepo"));
        assert_eq!(entry.model.as_deref(), Some("claude-sonnet"));
        assert_eq!(entry.state, SessionState::Working);
    }
}
