//! A local, unsynced store for the worktrees UI's row colours.
//!
//! The VS Code companion's `omniDevWorktrees.rowColors` setting lives in VS
//! Code's own configuration — invisible to this TUI. Rather than block on
//! moving that setting into the daemon (its own follow-up issue, per #1585
//! §7), the TUI keeps its own copy here, at `~/.omni-dev/worktrees-ui-row-
//! colors.yaml` — the same `~/.omni-dev/*.yaml` user-config convention as
//! `~/.omni-dev/models.yaml` (`src/claude/model_config.rs`), distinct from
//! the daemon's own `dirs::data_dir()`-rooted runtime-artifact convention
//! (`src/daemon/paths.rs`). Keys mirror the extension's own `repo:<root>` /
//! `wt:<path>` scheme (`tree.ts::nodeId`) so a future migration into the
//! daemon can read this file's keys unchanged — but nothing here talks to
//! the extension or the daemon today.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// Workbench colour ids the row-colour picker accepts, ported from the
/// extension's `icons.ts::ROW_COLORS`. Rejected on write; tolerated on read
/// (forward-compat with a colour a newer VS Code release might add).
pub const KNOWN_ROW_COLORS: &[&str] = &[
    "charts.red",
    "charts.orange",
    "charts.yellow",
    "charts.green",
    "charts.blue",
    "charts.purple",
    "charts.foreground",
    "terminal.ansiRed",
    "terminal.ansiYellow",
    "terminal.ansiGreen",
    "terminal.ansiCyan",
    "terminal.ansiBlue",
    "terminal.ansiMagenta",
    "descriptionForeground",
];

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RowColorKey {
    Repo(PathBuf),
    Worktree(PathBuf),
}

impl RowColorKey {
    fn wire_key(&self) -> String {
        match self {
            Self::Repo(path) => format!("repo:{}", path.display()),
            Self::Worktree(path) => format!("wt:{}", path.display()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct StoreFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    colors: HashMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct RowColorStore {
    path: PathBuf,
    colors: HashMap<String, String>,
}

impl RowColorStore {
    /// Loads the store at `path` (or the default
    /// `~/.omni-dev/worktrees-ui-row-colors.yaml` when `None`). A missing
    /// file is not an error — just an empty store.
    pub fn load(path: Option<PathBuf>) -> Result<Self> {
        let path = match path {
            Some(path) => path,
            None => default_path()?,
        };
        let colors = match fs::read_to_string(&path) {
            Ok(contents) => {
                let file: StoreFile = serde_yaml::from_str(&contents)
                    .with_context(|| format!("failed to parse {}", path.display()))?;
                file.colors
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
        };
        Ok(Self { path, colors })
    }

    pub fn get(&self, key: &RowColorKey) -> Option<&str> {
        self.colors.get(&key.wire_key()).map(String::as_str)
    }

    pub fn set(&mut self, key: RowColorKey, color_id: impl Into<String>) -> Result<()> {
        let color_id = color_id.into();
        if !KNOWN_ROW_COLORS.contains(&color_id.as_str()) {
            bail!("unknown row colour id: {color_id}");
        }
        self.colors.insert(key.wire_key(), color_id);
        self.save()
    }

    pub fn clear(&mut self, key: &RowColorKey) -> Result<()> {
        self.colors.remove(&key.wire_key());
        self.save()
    }

    pub fn clear_all(&mut self) -> Result<()> {
        self.colors.clear();
        self.save()
    }

    /// Atomic write via a temp-file-plus-rename in the same directory, so a
    /// crash mid-write never leaves a truncated/corrupt store.
    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let file = StoreFile {
            version: 1,
            colors: self.colors.clone(),
        };
        let contents = serde_yaml::to_string(&file).context("failed to serialize row colours")?;
        let tmp_path = self.path.with_extension("yaml.tmp");
        fs::write(&tmp_path, contents)
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        fs::rename(&tmp_path, &self.path)
            .with_context(|| format!("failed to replace {}", self.path.display()))?;
        Ok(())
    }
}

fn default_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("could not determine the user home directory")?;
    Ok(home.join(".omni-dev").join("worktrees-ui-row-colors.yaml"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_file_returns_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = RowColorStore::load(Some(dir.path().join("missing.yaml"))).unwrap();
        assert!(store.colors.is_empty());
    }

    #[test]
    fn set_then_get_round_trips_through_a_reload() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("colors.yaml");
        let mut store = RowColorStore::load(Some(file.clone())).unwrap();
        let key = RowColorKey::Worktree(PathBuf::from("/repo/wt"));
        store.set(key.clone(), "charts.blue").unwrap();
        assert_eq!(store.get(&key), Some("charts.blue"));

        let reloaded = RowColorStore::load(Some(file)).unwrap();
        assert_eq!(reloaded.get(&key), Some("charts.blue"));
    }

    #[test]
    fn set_rejects_an_unknown_color_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = RowColorStore::load(Some(dir.path().join("colors.yaml"))).unwrap();
        let key = RowColorKey::Repo(PathBuf::from("/repo"));
        assert!(store.set(key, "not-a-real-color").is_err());
    }

    #[test]
    fn clear_removes_only_that_key() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = RowColorStore::load(Some(dir.path().join("colors.yaml"))).unwrap();
        let a = RowColorKey::Repo(PathBuf::from("/repo/a"));
        let b = RowColorKey::Repo(PathBuf::from("/repo/b"));
        store.set(a.clone(), "charts.blue").unwrap();
        store.set(b.clone(), "charts.green").unwrap();
        store.clear(&a).unwrap();
        assert_eq!(store.get(&a), None);
        assert_eq!(store.get(&b), Some("charts.green"));
    }

    #[test]
    fn clear_all_empties_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = RowColorStore::load(Some(dir.path().join("colors.yaml"))).unwrap();
        store
            .set(RowColorKey::Repo(PathBuf::from("/repo")), "charts.blue")
            .unwrap();
        store.clear_all().unwrap();
        assert!(store
            .get(&RowColorKey::Repo(PathBuf::from("/repo")))
            .is_none());
    }

    #[test]
    fn load_tolerates_an_unrecognized_future_color_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("colors.yaml");
        fs::write(
            &path,
            "version: 1\ncolors:\n  \"repo:/x\": \"charts.mystery-future-color\"\n",
        )
        .unwrap();
        let store = RowColorStore::load(Some(path)).unwrap();
        assert_eq!(
            store.get(&RowColorKey::Repo(PathBuf::from("/x"))),
            Some("charts.mystery-future-color")
        );
    }
}
