//! The merged, rendering-agnostic view of the worktrees tree: the daemon's
//! `tree` snapshot, joined sessions, the local ahead/behind cache, local row
//! colours, and local "a tab of mine is open here" state.
//!
//! Everything in this module is plain data and pure functions — no
//! daemon/tokio/ratatui types appear here, so it is unit-testable without a
//! socket or a terminal.

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::pr_status::PrCheckState;
use crate::sessions::{SessionState, Source};

use super::ahead_behind::AheadBehindCache;
use super::local_state::OpenTabs;
use super::row_colors::{RowColorKey, RowColorStore};
use super::wire::{SessionEntryWire, TreeRepoWire, TreeSnapshotWire, TreeWorktreeWire};

/// The merged view the rendering layer draws from.
#[derive(Debug, Clone, Default)]
pub struct WorktreesViewModel {
    pub repos: Vec<RepoRow>,
    pub show_closed: bool,
    /// Bumped on every rebuild — lets a consumer skip a redraw via a cheap
    /// generation compare instead of a deep diff.
    pub generation: u64,
    pub worktrees_status: FeedStatus,
    pub sessions_status: FeedStatus,
}

/// Human-facing connection status for one of the two live feeds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FeedStatus {
    #[default]
    Connecting,
    Live,
    Reconnecting {
        attempt: u32,
        retry_in: Duration,
    },
    Polling,
}

#[derive(Debug, Clone)]
pub struct RepoRow {
    pub main_repo: String,
    pub github: Option<GithubIdentity>,
    pub root: PathBuf,
    pub polling_enabled: bool,
    pub row_color: Option<String>,
    pub worktrees: Vec<WorktreeRow>,
}

#[derive(Debug, Clone)]
pub struct GithubIdentity {
    pub owner: String,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct WorktreeRow {
    pub path: PathBuf,
    pub branch: Option<String>,
    /// Not rendered by Phase 1 (the hub tracks these itself, off the wire
    /// snapshot directly, to invalidate stale `ahead_behind` cache entries —
    /// see `Hub::on_tree_changed`); carried through here as part of a
    /// complete mirror of the daemon's row, for a later phase's detail view.
    #[allow(dead_code)]
    pub head_sha: Option<String>,
    #[allow(dead_code)]
    pub upstream_sha: Option<String>,
    pub is_main: bool,
    pub open: bool,
    /// Not rendered by Phase 1; the daemon `open`/`focus` op target once the
    /// tree pane's `focus`/`o` parity command lands (Phase 2).
    #[allow(dead_code)]
    pub window_key: Option<String>,
    pub pr: Option<PrBadgeRow>,
    /// The daemon-confirmed "no open PR" negative (vs. simply not-yet-
    /// resolved — both currently render the same blank space in Phase 1's
    /// plain tree pane). Distinguished once the full glyph table lands
    /// (Phase 4).
    #[allow(dead_code)]
    pub pr_none: bool,
    pub operation: Option<String>,
    pub rebasing: bool,
    pub pushing: bool,
    pub ahead_behind: AheadBehindState,
    pub sessions: Vec<SessionBadge>,
    pub row_color: Option<String>,
    /// Local, daemon-free: a tab of *this* TUI process is open here. Always
    /// `false` until a later phase's tab lifecycle wires
    /// [`super::hub::HubCommand::SetOpenTab`].
    pub here: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AheadBehindState {
    /// Never asked for (not currently visible).
    Unknown,
    /// A fetch is in flight.
    Loading,
    Known {
        ahead: usize,
        behind: usize,
        main_behind: Option<usize>,
    },
    /// Asked for, and the daemon has nothing to report (no upstream).
    Unavailable,
}

#[derive(Debug, Clone)]
pub struct PrBadgeRow {
    pub number: u64,
    pub is_draft: bool,
    pub checks: PrCheckState,
    /// The PR's web URL. Not rendered by Phase 1's plain tree pane; carried
    /// through for the `openPullRequestInBrowser` parity command (#1585 §3,
    /// Phase 2).
    #[allow(dead_code)]
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct SessionBadge {
    /// Not rendered by Phase 1; carried through as the action target once a
    /// per-session action (e.g. "move Claude session here") lands (Phase 2).
    #[allow(dead_code)]
    pub session_id: String,
    pub state: SessionState,
    pub source: SessionSourceRow,
    pub model: Option<String>,
    pub last_seen: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub enum SessionSourceRow {
    Terminal,
    VsCode {
        /// Not rendered by Phase 1 (which shows only Terminal-vs-VsCode);
        /// carried through as the daemon's `focus` op target once the tree
        /// pane can jump to a session's owning window (Phase 2+).
        #[allow(dead_code)]
        window_key: String,
    },
}

/// Glyph precedence, first match wins — ported from the VS Code companion's
/// `icons.ts::worktreeRowIcon`: current-window tick beats every git-operation
/// cue, which beats plain open/closed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphCue {
    Here,
    Pushing,
    Rebasing,
    Operation,
    Open,
    Closed,
}

/// Row colour/emphasis precedence — `icons.ts`'s companion rule: an
/// in-flight operation cue beats even a user's own row tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowEmphasis {
    Operation,
    UserTag(String),
    Open,
    Default,
}

/// Badge severity ranking — `tree.ts::rowColorId`: red > yellow > green >
/// muted. `Ord` follows declaration order, so `max()` picks the most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Muted,
    Green,
    Yellow,
    Red,
}

impl WorktreeRow {
    pub fn glyph_cue(&self) -> GlyphCue {
        if self.here {
            GlyphCue::Here
        } else if self.pushing {
            GlyphCue::Pushing
        } else if self.rebasing {
            GlyphCue::Rebasing
        } else if self.operation.is_some() {
            GlyphCue::Operation
        } else if self.open {
            GlyphCue::Open
        } else {
            GlyphCue::Closed
        }
    }

    pub fn emphasis(&self) -> RowEmphasis {
        if self.pushing || self.rebasing || self.operation.is_some() {
            RowEmphasis::Operation
        } else if let Some(tag) = &self.row_color {
            RowEmphasis::UserTag(tag.clone())
        } else if self.open {
            RowEmphasis::Open
        } else {
            RowEmphasis::Default
        }
    }

    pub fn badge_severity(&self) -> Severity {
        badge_severity(
            self.pr.as_ref().map(|pr| pr.checks),
            self.sessions.iter().map(|s| s.state),
        )
    }
}

/// The shared severity ranking, factored out so it can be unit-tested
/// against synthetic inputs without building a whole [`WorktreeRow`].
pub fn badge_severity(
    pr_checks: Option<PrCheckState>,
    session_states: impl IntoIterator<Item = SessionState>,
) -> Severity {
    let mut severity = Severity::Muted;
    if let Some(checks) = pr_checks {
        severity = severity.max(match checks {
            PrCheckState::Failure => Severity::Red,
            PrCheckState::Pending => Severity::Yellow,
            PrCheckState::Success => Severity::Green,
            PrCheckState::None => Severity::Muted,
        });
    }
    for state in session_states {
        severity = severity.max(match state {
            SessionState::WaitingForInput | SessionState::WaitingForPermission => Severity::Yellow,
            SessionState::Working | SessionState::Starting => Severity::Green,
            SessionState::Idle | SessionState::Ended => Severity::Muted,
        });
    }
    severity
}

/// Merges the daemon's tree snapshot with joined sessions and local state
/// into one [`WorktreesViewModel`]. `tree` is `None` before the first
/// snapshot has arrived (the view starts empty, not stale).
#[allow(clippy::too_many_arguments)]
pub fn merge(
    tree: Option<&TreeSnapshotWire>,
    sessions: &[SessionEntryWire],
    ahead_behind: &AheadBehindCache,
    row_colors: &RowColorStore,
    open_tabs: &OpenTabs,
    worktrees_status: FeedStatus,
    sessions_status: FeedStatus,
    generation: u64,
) -> WorktreesViewModel {
    let Some(tree) = tree else {
        return WorktreesViewModel {
            generation,
            worktrees_status,
            sessions_status,
            ..Default::default()
        };
    };
    let repos = tree
        .repos
        .iter()
        .map(|repo| merge_repo(repo, sessions, ahead_behind, row_colors, open_tabs))
        .collect();
    WorktreesViewModel {
        repos,
        show_closed: tree.show_closed,
        generation,
        worktrees_status,
        sessions_status,
    }
}

fn merge_repo(
    repo: &TreeRepoWire,
    sessions: &[SessionEntryWire],
    ahead_behind: &AheadBehindCache,
    row_colors: &RowColorStore,
    open_tabs: &OpenTabs,
) -> RepoRow {
    let root = PathBuf::from(&repo.root);
    RepoRow {
        main_repo: repo.main_repo.clone(),
        github: repo.github.as_ref().map(|g| GithubIdentity {
            owner: g.owner.clone(),
            name: g.name.clone(),
        }),
        row_color: row_colors
            .get(&RowColorKey::Repo(root.clone()))
            .map(str::to_string),
        root,
        polling_enabled: repo.polling_enabled,
        worktrees: repo
            .worktrees
            .iter()
            .map(|wt| merge_worktree(wt, sessions, ahead_behind, row_colors, open_tabs))
            .collect(),
    }
}

fn merge_worktree(
    wt: &TreeWorktreeWire,
    sessions: &[SessionEntryWire],
    ahead_behind: &AheadBehindCache,
    row_colors: &RowColorStore,
    open_tabs: &OpenTabs,
) -> WorktreeRow {
    let path = PathBuf::from(&wt.path);
    // `Path::starts_with("")` is true for every path, so an empty `path` (a
    // malformed/older daemon payload omitting the field) must never be used
    // as a session-join prefix — otherwise every live session in the system
    // would attach to this one row.
    let joined_sessions = if path.as_os_str().is_empty() {
        Vec::new()
    } else {
        sessions
            .iter()
            .filter(|s| s.cwd.as_deref().is_some_and(|cwd| cwd.starts_with(&path)))
            .map(|s| SessionBadge {
                session_id: s.session_id.clone(),
                state: s.state,
                source: match &s.source {
                    Source::Terminal => SessionSourceRow::Terminal,
                    Source::VsCode { window_key } => SessionSourceRow::VsCode {
                        window_key: window_key.clone(),
                    },
                },
                model: s.model.clone(),
                last_seen: s.last_seen,
            })
            .collect()
    };
    WorktreeRow {
        branch: wt.branch.clone(),
        head_sha: wt.head_sha.clone(),
        upstream_sha: wt.upstream_sha.clone(),
        is_main: wt.is_main,
        open: wt.open,
        window_key: wt.window_key.clone(),
        pr: wt.pr.as_ref().map(|pr| PrBadgeRow {
            number: pr.number,
            is_draft: pr.is_draft,
            checks: pr.checks,
            url: pr.url.clone(),
        }),
        pr_none: wt.pr_none,
        operation: wt.operation.clone(),
        rebasing: wt.rebasing,
        pushing: wt.pushing,
        ahead_behind: ahead_behind.get(&path),
        sessions: joined_sessions,
        row_color: row_colors
            .get(&RowColorKey::Worktree(path.clone()))
            .map(str::to_string),
        here: open_tabs.contains(&path),
        path,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::sessions::Source;

    fn session(cwd: &str, repo: Option<&str>) -> SessionEntryWire {
        SessionEntryWire {
            session_id: "s1".to_string(),
            cwd: Some(PathBuf::from(cwd)),
            repo: repo.map(str::to_string),
            model: None,
            state: SessionState::Working,
            source: Source::Terminal,
            last_seen: chrono::Utc::now(),
        }
    }

    fn worktree_wire(path: &str) -> TreeWorktreeWire {
        TreeWorktreeWire {
            path: path.to_string(),
            branch: None,
            head_sha: None,
            upstream_sha: None,
            is_main: false,
            open: false,
            window_key: None,
            pr: None,
            pr_none: false,
            operation: None,
            rebasing: false,
            pushing: false,
        }
    }

    #[test]
    fn merge_joins_session_by_cwd_prefix() {
        let wt = worktree_wire("/repo/wt-1");
        let sessions = vec![session("/repo/wt-1/src", None)];
        let ahead_behind = AheadBehindCache::new(super::super::client::WorktreesClient::new(
            "/tmp/nonexistent.sock",
        ));
        let row_colors = RowColorStore::default();
        let open_tabs = OpenTabs::default();
        let row = merge_worktree(&wt, &sessions, &ahead_behind, &row_colors, &open_tabs);
        assert_eq!(row.sessions.len(), 1);
    }

    #[test]
    fn merge_does_not_join_a_sibling_worktree_with_a_shared_string_prefix() {
        // /repo/wt-1 must NOT match a session under /repo/wt-10 — a naive
        // string-prefix check would get this wrong; `Path::starts_with` is
        // component-wise and gets it right.
        let wt = worktree_wire("/repo/wt-1");
        let sessions = vec![session("/repo/wt-10/src", None)];
        let ahead_behind = AheadBehindCache::new(super::super::client::WorktreesClient::new(
            "/tmp/nonexistent.sock",
        ));
        let row_colors = RowColorStore::default();
        let open_tabs = OpenTabs::default();
        let row = merge_worktree(&wt, &sessions, &ahead_behind, &row_colors, &open_tabs);
        assert!(row.sessions.is_empty());
    }

    #[test]
    fn merge_does_not_join_any_session_when_worktree_path_is_empty() {
        // A malformed/older daemon payload omitting `path` deserializes it to
        // "" (see wire.rs's `sanitized` deserializer default). Path::starts_with("")
        // is true for every path, so without the empty-path guard this would
        // attach every session in the system to this one row.
        let wt = worktree_wire("");
        let sessions = vec![
            session("/repo/wt-1/src", None),
            session("/other-repo/wt/src", None),
        ];
        let ahead_behind = AheadBehindCache::new(super::super::client::WorktreesClient::new(
            "/tmp/nonexistent.sock",
        ));
        let row_colors = RowColorStore::default();
        let open_tabs = OpenTabs::default();
        let row = merge_worktree(&wt, &sessions, &ahead_behind, &row_colors, &open_tabs);
        assert!(row.sessions.is_empty());
    }

    #[test]
    fn merge_here_reflects_local_open_tabs_not_daemon_open_flag() {
        let mut wt = worktree_wire("/repo/wt-1");
        wt.open = true; // a VS Code window has it open...
        let ahead_behind = AheadBehindCache::new(super::super::client::WorktreesClient::new(
            "/tmp/nonexistent.sock",
        ));
        let row_colors = RowColorStore::default();
        let mut open_tabs = OpenTabs::default();
        // ...but no TUI tab does, so `here` must be false.
        let row = merge_worktree(&wt, &[], &ahead_behind, &row_colors, &open_tabs);
        assert!(!row.here);
        assert!(row.open);

        open_tabs.set(PathBuf::from("/repo/wt-1"));
        let row = merge_worktree(&wt, &[], &ahead_behind, &row_colors, &open_tabs);
        assert!(row.here);
    }

    #[test]
    fn merge_row_color_looked_up_by_worktree_path() {
        let wt = worktree_wire("/repo/wt-1");
        let ahead_behind = AheadBehindCache::new(super::super::client::WorktreesClient::new(
            "/tmp/nonexistent.sock",
        ));
        let dir = tempfile::tempdir().unwrap();
        let mut row_colors = RowColorStore::load(Some(dir.path().join("colors.yaml"))).unwrap();
        row_colors
            .set(
                RowColorKey::Worktree(PathBuf::from("/repo/wt-1")),
                "charts.blue",
            )
            .unwrap();
        let open_tabs = OpenTabs::default();
        let row = merge_worktree(&wt, &[], &ahead_behind, &row_colors, &open_tabs);
        assert_eq!(row.row_color.as_deref(), Some("charts.blue"));
    }

    #[test]
    fn merge_ahead_behind_defaults_to_unknown_for_unfetched_row() {
        let wt = worktree_wire("/repo/wt-1");
        let ahead_behind = AheadBehindCache::new(super::super::client::WorktreesClient::new(
            "/tmp/nonexistent.sock",
        ));
        let row_colors = RowColorStore::default();
        let open_tabs = OpenTabs::default();
        let row = merge_worktree(&wt, &[], &ahead_behind, &row_colors, &open_tabs);
        assert_eq!(row.ahead_behind, AheadBehindState::Unknown);
    }

    #[test]
    fn glyph_cue_here_beats_every_other_cue() {
        let mut wt = worktree_row();
        wt.here = true;
        wt.pushing = true;
        assert_eq!(wt.glyph_cue(), GlyphCue::Here);
    }

    #[test]
    fn glyph_cue_pushing_beats_rebasing_beats_operation_beats_open() {
        let mut wt = worktree_row();
        wt.pushing = true;
        wt.rebasing = true;
        wt.operation = Some("rebase".to_string());
        wt.open = true;
        assert_eq!(wt.glyph_cue(), GlyphCue::Pushing);

        wt.pushing = false;
        assert_eq!(wt.glyph_cue(), GlyphCue::Rebasing);

        wt.rebasing = false;
        assert_eq!(wt.glyph_cue(), GlyphCue::Operation);

        wt.operation = None;
        assert_eq!(wt.glyph_cue(), GlyphCue::Open);

        wt.open = false;
        assert_eq!(wt.glyph_cue(), GlyphCue::Closed);
    }

    #[test]
    fn emphasis_operation_beats_user_tag_beats_open_beats_default() {
        let mut wt = worktree_row();
        wt.row_color = Some("charts.blue".to_string());
        wt.rebasing = true;
        wt.open = true;
        assert_eq!(wt.emphasis(), RowEmphasis::Operation);

        wt.rebasing = false;
        assert_eq!(
            wt.emphasis(),
            RowEmphasis::UserTag("charts.blue".to_string())
        );

        wt.row_color = None;
        assert_eq!(wt.emphasis(), RowEmphasis::Open);

        wt.open = false;
        assert_eq!(wt.emphasis(), RowEmphasis::Default);
    }

    #[test]
    fn badge_severity_red_beats_yellow_beats_green_beats_muted() {
        assert_eq!(
            badge_severity(Some(PrCheckState::Failure), [SessionState::Idle]),
            Severity::Red
        );
        assert_eq!(
            badge_severity(Some(PrCheckState::Pending), [SessionState::Working]),
            Severity::Yellow
        );
        assert_eq!(
            badge_severity(None, [SessionState::WaitingForPermission]),
            Severity::Yellow
        );
        assert_eq!(
            badge_severity(Some(PrCheckState::Success), []),
            Severity::Green
        );
        assert_eq!(badge_severity(None, [SessionState::Idle]), Severity::Muted);
        assert_eq!(badge_severity(None, []), Severity::Muted);
    }

    fn worktree_row() -> WorktreeRow {
        WorktreeRow {
            path: PathBuf::from("/repo/wt"),
            branch: None,
            head_sha: None,
            upstream_sha: None,
            is_main: false,
            open: false,
            window_key: None,
            pr: None,
            pr_none: false,
            operation: None,
            rebasing: false,
            pushing: false,
            ahead_behind: AheadBehindState::Unknown,
            sessions: Vec::new(),
            row_color: None,
            here: false,
        }
    }
}
