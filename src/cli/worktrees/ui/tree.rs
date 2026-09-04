//! Cursor/selection state over the tree pane (issue #1585 Phase 2) — the
//! minimum slice of the plan's eventual `tree.rs` needed to drive actions
//! interactively. Mouse handling, collapsing, scrolling, and the full glyph
//! table stay Phase 4; this is just "which row(s) is an action about".

use std::collections::HashSet;
use std::path::PathBuf;

use super::actions::Target;
use super::view_model::{RepoRow, WorktreesViewModel};

/// One addressable row in the flattened tree — a repo header or one of its
/// worktrees — used for cursor movement and to resolve a row back to an
/// [`Target`].
#[derive(Debug, Clone)]
pub enum RowRef {
    Repo { index: usize },
    Worktree { repo_index: usize, wt_index: usize },
}

impl RowRef {
    fn path<'a>(&self, view: &'a WorktreesViewModel) -> Option<&'a std::path::Path> {
        match self {
            Self::Repo { index } => view.repos.get(*index).map(|r| r.root.as_path()),
            Self::Worktree {
                repo_index,
                wt_index,
            } => view
                .repos
                .get(*repo_index)
                .and_then(|r| r.worktrees.get(*wt_index))
                .map(|w| w.path.as_path()),
        }
    }
}

/// Cursor position and multi-select marks over the tree pane. Marks are
/// keyed by path (repo root or worktree path) rather than by flattened
/// index, so they survive the view model's rows being reordered/added
/// between publishes.
#[derive(Debug, Clone, Default)]
pub struct TreeState {
    pub cursor: usize,
    pub marked: HashSet<PathBuf>,
}

impl TreeState {
    /// Flattens the view model's nested repos/worktrees into display order —
    /// a repo header row followed immediately by each of its worktree rows.
    pub fn visible_rows(view: &WorktreesViewModel) -> Vec<RowRef> {
        let mut rows = Vec::new();
        for (repo_index, repo) in view.repos.iter().enumerate() {
            rows.push(RowRef::Repo { index: repo_index });
            for wt_index in 0..repo.worktrees.len() {
                rows.push(RowRef::Worktree {
                    repo_index,
                    wt_index,
                });
            }
        }
        rows
    }

    /// Moves the cursor by `delta` rows, clamped to `[0, row_count)`. A
    /// `row_count` of `0` leaves the cursor at `0` (nothing to clamp into).
    pub fn move_cursor(&mut self, delta: isize, row_count: usize) {
        if row_count == 0 {
            self.cursor = 0;
            return;
        }
        let max = row_count - 1;
        let current = self.cursor.min(max) as isize;
        let next = (current + delta).clamp(0, max as isize);
        self.cursor = next as usize;
    }

    pub fn toggle_mark(&mut self, path: PathBuf) {
        if !self.marked.remove(&path) {
            self.marked.insert(path);
        }
    }

    pub fn clear_marks(&mut self) {
        self.marked.clear();
    }

    /// The targets an action should act on: every marked row (resolved
    /// against the current snapshot — a mark for a row that has since
    /// disappeared is silently dropped), or just the cursor row when nothing
    /// is marked.
    pub fn targets(&self, view: &WorktreesViewModel) -> Vec<Target> {
        if self.marked.is_empty() {
            return self
                .cursor_row(view)
                .map(|row| row_target(view, &row))
                .into_iter()
                .flatten()
                .collect();
        }
        Self::visible_rows(view)
            .iter()
            .filter(|row| row.path(view).is_some_and(|p| self.marked.contains(p)))
            .filter_map(|row| row_target(view, row))
            .collect()
    }

    /// The cursor row alone, ignoring marks entirely — used by the `space`
    /// (mark toggle) and session-relocation entry points, which always act
    /// on "the row the cursor is on" regardless of any multi-select.
    pub fn targets_for_cursor_only(&self, view: &WorktreesViewModel) -> Vec<Target> {
        self.cursor_row(view)
            .and_then(|row| row_target(view, &row))
            .into_iter()
            .collect()
    }

    fn cursor_row(&self, view: &WorktreesViewModel) -> Option<RowRef> {
        Self::visible_rows(view).into_iter().nth(self.cursor)
    }
}

fn row_target(view: &WorktreesViewModel, row: &RowRef) -> Option<Target> {
    match row {
        RowRef::Repo { index } => view.repos.get(*index).map(Target::from_repo),
        RowRef::Worktree {
            repo_index,
            wt_index,
        } => {
            let repo: &RepoRow = view.repos.get(*repo_index)?;
            let wt = repo.worktrees.get(*wt_index)?;
            Some(Target::from_worktree(wt, repo.github.as_ref()))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cli::worktrees::ui::view_model::{AheadBehindState, WorktreeRow};

    fn worktree(path: &str) -> WorktreeRow {
        WorktreeRow {
            path: PathBuf::from(path),
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

    fn view_with_two_repos() -> WorktreesViewModel {
        WorktreesViewModel {
            repos: vec![
                RepoRow {
                    main_repo: "repo-a".to_string(),
                    github: None,
                    root: PathBuf::from("/repo-a"),
                    polling_enabled: false,
                    row_color: None,
                    worktrees: vec![worktree("/repo-a/wt-1"), worktree("/repo-a/wt-2")],
                },
                RepoRow {
                    main_repo: "repo-b".to_string(),
                    github: None,
                    root: PathBuf::from("/repo-b"),
                    polling_enabled: false,
                    row_color: None,
                    worktrees: vec![worktree("/repo-b/wt-1")],
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn visible_rows_flattens_repo_headers_and_their_worktrees_in_order() {
        let view = view_with_two_repos();
        let rows = TreeState::visible_rows(&view);
        assert_eq!(rows.len(), 5); // 2 repo headers + 2 + 1 worktrees
        assert!(matches!(rows[0], RowRef::Repo { index: 0 }));
        assert!(matches!(
            rows[1],
            RowRef::Worktree {
                repo_index: 0,
                wt_index: 0
            }
        ));
        assert!(matches!(rows[3], RowRef::Repo { index: 1 }));
    }

    #[test]
    fn move_cursor_clamps_at_both_ends() {
        let mut state = TreeState::default();
        state.move_cursor(-5, 5);
        assert_eq!(state.cursor, 0);
        state.move_cursor(100, 5);
        assert_eq!(state.cursor, 4);
        state.move_cursor(-1, 5);
        assert_eq!(state.cursor, 3);
    }

    #[test]
    fn move_cursor_with_zero_rows_stays_at_zero() {
        let mut state = TreeState::default();
        state.move_cursor(3, 0);
        assert_eq!(state.cursor, 0);
    }

    #[test]
    fn targets_with_no_marks_returns_only_the_cursor_row() {
        let view = view_with_two_repos();
        let state = TreeState {
            cursor: 1, // repo-a/wt-1
            ..Default::default()
        };
        let targets = state.targets(&view);
        assert_eq!(targets.len(), 1);
        assert!(matches!(
            &targets[0],
            Target::Worktree { path, .. } if path == std::path::Path::new("/repo-a/wt-1")
        ));
    }

    #[test]
    fn targets_with_marks_returns_every_marked_row_regardless_of_cursor() {
        let view = view_with_two_repos();
        let mut state = TreeState::default();
        state.toggle_mark(PathBuf::from("/repo-a/wt-2"));
        state.toggle_mark(PathBuf::from("/repo-b/wt-1"));
        let targets = state.targets(&view);
        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn toggle_mark_is_idempotent_on_and_off() {
        let mut state = TreeState::default();
        let path = PathBuf::from("/repo-a/wt-1");
        state.toggle_mark(path.clone());
        assert!(state.marked.contains(&path));
        state.toggle_mark(path.clone());
        assert!(!state.marked.contains(&path));
    }
}
