//! Local, daemon-free "a tab of mine is open on this worktree" tracking.
//!
//! Computed purely from this process's own terminal tabs (wired up when a
//! later phase adds embedded PTYs) — never a daemon round-trip, since the TUI
//! does not register itself as a window with the worktrees registry (issue
//! #1585 §7: registering would make the daemon's `open`/`reposition` actions
//! — which assume a VS Code window — target a TUI pane that has neither a
//! `code` process to launch nor an `NSWindow` to reposition).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct OpenTabs {
    paths: HashSet<PathBuf>,
}

impl OpenTabs {
    pub fn set(&mut self, path: PathBuf) {
        self.paths.insert(path);
    }

    pub fn clear(&mut self, path: &Path) {
        self.paths.remove(path);
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.paths.contains(path)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn set_then_contains() {
        let mut tabs = OpenTabs::default();
        let path = PathBuf::from("/repo/wt");
        assert!(!tabs.contains(&path));
        tabs.set(path.clone());
        assert!(tabs.contains(&path));
    }

    #[test]
    fn clear_removes_only_that_path() {
        let mut tabs = OpenTabs::default();
        let a = PathBuf::from("/repo/a");
        let b = PathBuf::from("/repo/b");
        tabs.set(a.clone());
        tabs.set(b.clone());
        tabs.clear(&a);
        assert!(!tabs.contains(&a));
        assert!(tabs.contains(&b));
    }
}
