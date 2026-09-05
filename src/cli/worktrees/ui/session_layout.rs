//! Pane-layout persistence across runs (issue #1585 Phase 5).
//!
//! What survives a restart is the *shape* of the workspace — how many pane
//! groups, their relative sizes, and which worktree each tab was opened in
//! — never a live process. PTYs are hosted in this process (ADR-0072 §2),
//! so a restored tab is a **new** child in the same place, and the file
//! records only what is needed to spawn it again.
//!
//! Stored beside the row-colour store and written the same way
//! (`~/.omni-dev/worktrees-ui-layout.yaml`, `0600` under a `0700`
//! directory, atomic temp-file rename), so there is one convention for
//! this UI's local state rather than two.
//!
//! **Restoration is best-effort and silent.** A worktree that has since
//! been deleted, a shell that no longer exists, a file written by a newer
//! version: each is dropped rather than surfaced, because a startup error
//! about a convenience feature is worse than starting with fewer tabs.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::terminal::TabKind;
use crate::daemon::paths::{ensure_dir_0700, set_file_0600};

/// The persisted form of one tab: what to run, and where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedTab {
    /// The worktree the tab was opened in.
    pub path: PathBuf,
    /// `shell` or `claude`. A future kind read from an older build's file
    /// falls back to `shell` rather than failing the whole restore.
    #[serde(default)]
    pub kind: SavedTabKind,
}

/// [`TabKind`]'s serialized twin. Kept separate so the on-disk vocabulary
/// is a deliberate choice rather than whatever the internal enum happens to
/// be called, and so an unknown value degrades instead of erroring.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SavedTabKind {
    #[default]
    Shell,
    Claude,
}

impl From<TabKind> for SavedTabKind {
    fn from(kind: TabKind) -> Self {
        match kind {
            TabKind::Shell => Self::Shell,
            TabKind::Claude => Self::Claude,
        }
    }
}

impl From<SavedTabKind> for TabKind {
    fn from(kind: SavedTabKind) -> Self {
        match kind {
            SavedTabKind::Shell => Self::Shell,
            SavedTabKind::Claude => Self::Claude,
        }
    }
}

/// One persisted pane group: its tabs, which was active, and its weight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedGroup {
    #[serde(default)]
    pub tabs: Vec<SavedTab>,
    #[serde(default)]
    pub active: usize,
    #[serde(default = "one")]
    pub weight: u16,
}

fn one() -> u16 {
    1
}

/// The whole saved workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedLayout {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub groups: Vec<SavedGroup>,
    #[serde(default)]
    pub focused: usize,
}

impl SavedLayout {
    /// Drops anything that cannot be restored *now*: tabs whose worktree no
    /// longer exists, then groups left with no tabs. Clamps `active` and
    /// `focused` back into range so a restored layout is structurally valid
    /// however the file was edited.
    ///
    /// `exists` is injected so the filter is testable without touching the
    /// filesystem.
    pub fn prune(mut self, exists: impl Fn(&Path) -> bool) -> Self {
        for group in &mut self.groups {
            group.tabs.retain(|tab| exists(&tab.path));
            if group.active >= group.tabs.len() {
                group.active = group.tabs.len().saturating_sub(1);
            }
            group.weight = group.weight.max(1);
        }
        self.groups.retain(|g| !g.tabs.is_empty());
        if self.focused >= self.groups.len() {
            self.focused = self.groups.len().saturating_sub(1);
        }
        self
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

/// Reads the saved layout, pruned against the real filesystem. A missing or
/// unreadable file is an empty layout, never an error — see the module doc.
pub fn load(path: Option<PathBuf>) -> SavedLayout {
    let Ok(path) = resolve(path) else {
        return SavedLayout::default();
    };
    let Ok(contents) = fs::read_to_string(&path) else {
        return SavedLayout::default();
    };
    match serde_yaml::from_str::<SavedLayout>(&contents) {
        Ok(layout) => layout.prune(Path::is_dir),
        Err(e) => {
            // Worth a log line — a corrupt file is a real (if harmless)
            // condition — but never worth failing startup over.
            tracing::debug!("worktrees ui: ignoring unreadable layout file: {e}");
            SavedLayout::default()
        }
    }
}

/// Writes `layout`, replacing any previous one. An empty layout removes the
/// file rather than leaving a stale one behind, so quitting with no tabs
/// open starts the next run clean.
pub fn save(layout: &SavedLayout, path: Option<PathBuf>) -> Result<()> {
    let path = resolve(path)?;
    if layout.is_empty() {
        match fs::remove_file(&path) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(e).with_context(|| format!("failed to remove {}", path.display()))
            }
        }
    }
    if let Some(parent) = path.parent() {
        ensure_dir_0700(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let stored = SavedLayout {
        version: 1,
        ..layout.clone()
    };
    let contents = serde_yaml::to_string(&stored).context("failed to serialize the layout")?;
    let tmp_path = path.with_extension("yaml.tmp");
    fs::write(&tmp_path, contents)
        .with_context(|| format!("failed to write {}", tmp_path.display()))?;
    set_file_0600(&tmp_path)
        .with_context(|| format!("failed to set permissions on {}", tmp_path.display()))?;
    fs::rename(&tmp_path, &path)
        .with_context(|| format!("failed to replace {}", path.display()))?;
    Ok(())
}

fn resolve(path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = path {
        return Ok(path);
    }
    let home = dirs::home_dir().context("could not determine the user home directory")?;
    Ok(home.join(".omni-dev").join("worktrees-ui-layout.yaml"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tab(path: &str) -> SavedTab {
        SavedTab {
            path: PathBuf::from(path),
            kind: SavedTabKind::Shell,
        }
    }

    fn layout() -> SavedLayout {
        SavedLayout {
            version: 1,
            groups: vec![
                SavedGroup {
                    tabs: vec![tab("/a"), tab("/b")],
                    active: 1,
                    weight: 2,
                },
                SavedGroup {
                    tabs: vec![tab("/c")],
                    active: 0,
                    weight: 1,
                },
            ],
            focused: 1,
        }
    }

    #[test]
    fn a_layout_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout.yaml");
        let before = layout();
        save(&before, Some(path.clone())).unwrap();

        // Read back without pruning (the paths are fictional).
        let contents = fs::read_to_string(&path).unwrap();
        let after: SavedLayout = serde_yaml::from_str(&contents).unwrap();
        assert_eq!(after, before);
        assert_eq!(after.version, 1);
    }

    #[test]
    fn saving_an_empty_layout_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout.yaml");
        save(&layout(), Some(path.clone())).unwrap();
        assert!(path.exists());
        save(&SavedLayout::default(), Some(path.clone())).unwrap();
        assert!(!path.exists(), "quitting with no tabs starts clean");
        // Removing an absent file is not an error.
        save(&SavedLayout::default(), Some(path)).unwrap();
    }

    #[test]
    fn a_missing_or_corrupt_file_loads_as_empty_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.yaml");
        assert!(load(Some(missing)).is_empty());

        let corrupt = dir.path().join("corrupt.yaml");
        fs::write(&corrupt, "this: is: not: a: layout\n[[[").unwrap();
        assert!(load(Some(corrupt)).is_empty());

        // A file from a newer version with unknown fields still loads what
        // it can, rather than being discarded wholesale.
        let newer = dir.path().join("newer.yaml");
        fs::write(
            &newer,
            "version: 99\ngroups: []\nfocused: 0\nsomething_new: true\n",
        )
        .unwrap();
        assert!(load(Some(newer)).is_empty());
    }

    #[test]
    fn prune_drops_vanished_worktrees_then_emptied_groups() {
        // Only /a and /c still exist.
        let pruned = layout().prune(|p| p == Path::new("/a") || p == Path::new("/c"));
        assert_eq!(pruned.groups.len(), 2);
        assert_eq!(pruned.groups[0].tabs.len(), 1);
        assert_eq!(pruned.groups[0].tabs[0].path, PathBuf::from("/a"));
        // `active` pointed at the dropped tab; it is clamped back in range.
        assert_eq!(pruned.groups[0].active, 0);
        assert_eq!(pruned.focused, 1);

        // Nothing exists any more: every group goes, and focus with it.
        let empty = layout().prune(|_| false);
        assert!(empty.is_empty());
        assert_eq!(empty.focused, 0);

        // A group that empties is removed and later focus re-clamped.
        let one_left = layout().prune(|p| p == Path::new("/c"));
        assert_eq!(one_left.groups.len(), 1);
        assert_eq!(one_left.focused, 0);
    }

    #[test]
    fn prune_repairs_a_zero_weight_and_an_out_of_range_active() {
        let layout = SavedLayout {
            version: 1,
            groups: vec![SavedGroup {
                tabs: vec![tab("/a")],
                active: 99,
                weight: 0,
            }],
            focused: 42,
        };
        let pruned = layout.prune(|_| true);
        assert_eq!(pruned.groups[0].active, 0);
        assert_eq!(pruned.groups[0].weight, 1, "a zero weight would vanish");
        assert_eq!(pruned.focused, 0);
    }

    #[test]
    fn tab_kinds_map_both_ways_and_an_absent_kind_defaults_to_shell() {
        for kind in [TabKind::Shell, TabKind::Claude] {
            assert_eq!(TabKind::from(SavedTabKind::from(kind)), kind);
        }
        let parsed: SavedTab = serde_yaml::from_str("path: /a\n").unwrap();
        assert_eq!(parsed.kind, SavedTabKind::Shell);
        let claude: SavedTab = serde_yaml::from_str("path: /a\nkind: claude\n").unwrap();
        assert_eq!(claude.kind, SavedTabKind::Claude);
    }

    #[test]
    fn load_prunes_against_the_real_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real-worktree");
        fs::create_dir(&real).unwrap();
        let path = dir.path().join("layout.yaml");
        let saved = SavedLayout {
            version: 1,
            groups: vec![SavedGroup {
                tabs: vec![
                    SavedTab {
                        path: real.clone(),
                        kind: SavedTabKind::Claude,
                    },
                    tab("/definitely/not/here"),
                ],
                active: 1,
                weight: 1,
            }],
            focused: 0,
        };
        save(&saved, Some(path.clone())).unwrap();

        let loaded = load(Some(path));
        assert_eq!(loaded.groups.len(), 1);
        assert_eq!(loaded.groups[0].tabs.len(), 1, "the vanished tab is gone");
        assert_eq!(loaded.groups[0].tabs[0].path, real);
        assert_eq!(loaded.groups[0].tabs[0].kind, SavedTabKind::Claude);
        assert_eq!(loaded.groups[0].active, 0);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_written_0600() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("layout.yaml");
        save(&layout(), Some(path.clone())).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "layout is per-user state");
        // No temp file is left behind.
        assert!(!path.with_extension("yaml.tmp").exists());
    }
}
