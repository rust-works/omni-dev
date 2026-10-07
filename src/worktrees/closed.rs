//! The recently-closed worktree log (#2211).
//!
//! The registry forgets a window the moment it unregisters, so once a window is
//! gone nothing records that the worktree existed. This is the daemon-owned
//! memory of that: one [`ClosedWorktree`] per closed path, newest first, bounded
//! in both size and age, and persisted so a daemon restart does not lose it.
//!
//! Pure data and bookkeeping only — no git and no clock of its own. The caller
//! passes `now`, and the adapter
//! ([`crate::daemon::services::worktrees`]) resolves a closed folder to its
//! branch, repository and head with `git2` before handing the record here.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};

/// The most entries kept.
///
/// Eviction is oldest-first, so the list is a window onto the most recent
/// closures rather than a history. A VS Code quit unregisters every window at
/// once, so this is sized to hold a whole session's worth.
pub const MAX_CLOSED: usize = 50;

/// How long an entry is kept, in days. A closure older than this is not what
/// anyone is looking for, and an old entry is the likeliest to point at a branch
/// or commit that no longer exists.
pub const MAX_AGE_DAYS: i64 = 30;

/// The on-disk format version, so a later change can tell an older file apart.
const FILE_VERSION: u32 = 1;

/// The GitHub identity of a closed worktree's repository. The same wire shape as
/// the tree snapshot's `github` field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosedGithub {
    /// The repository owner (user or org).
    pub owner: String,
    /// The repository name.
    pub name: String,
}

/// One closed worktree: where it was, what it had checked out, and whether it is
/// still on disk.
///
/// Serialized verbatim into the `recent-closed` reply, the tree snapshot's
/// `recently_closed` field and the persisted file, so a field added here must be
/// `#[serde(default)]` (an older file) and skipped when empty (an older client).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClosedWorktree {
    /// The worktree folder, canonicalized while it still existed. Identifies the
    /// entry: a newer closure of the same path replaces the older one.
    pub path: PathBuf,
    /// The repository's main working tree — where a removed worktree is
    /// recreated *from*.
    pub repo_root: PathBuf,
    /// The main repository's directory name.
    pub main_repo: String,
    /// The GitHub identity of `origin`, when it is a `github.com` remote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github: Option<ClosedGithub>,
    /// The branch checked out when it closed; `None` for a detached HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The commit HEAD pointed at when it closed — what a removed worktree whose
    /// branch was deleted is recreated at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    /// Whether this is the repository's main working tree rather than a linked
    /// worktree. A main working tree is never recreated.
    pub is_main: bool,
    /// Whether the worktree was **deleted from disk** (`true`) or only its window
    /// was closed (`false`). Reopening them is different work: the first creates
    /// a worktree, the second just opens a folder.
    #[serde(default)]
    pub removed: bool,
    /// When the closure was observed.
    pub closed_at: DateTime<Utc>,
}

/// The bounded, newest-first list of closures. Not thread-safe by itself: the
/// registry holds it behind a `Mutex` of its own.
#[derive(Debug, Default)]
pub struct ClosedLog {
    /// Newest first, one entry per path.
    entries: Vec<ClosedWorktree>,
}

impl ClosedLog {
    /// Builds a log from `entries` (a persisted file's contents, in any order),
    /// keeping the newest per path and applying both bounds as of `now`.
    #[must_use]
    pub fn from_entries(entries: Vec<ClosedWorktree>, now: DateTime<Utc>) -> Self {
        let mut log = Self::default();
        for entry in entries {
            log.insert(entry);
        }
        log.prune(now);
        log
    }

    /// The entries, newest first.
    #[must_use]
    pub fn entries(&self) -> &[ClosedWorktree] {
        &self.entries
    }

    /// The entry for `path`, if one is recorded.
    #[must_use]
    pub fn get(&self, path: &Path) -> Option<&ClosedWorktree> {
        self.entries.iter().find(|e| e.path == path)
    }

    /// Records a closure, replacing any earlier one for the same path, then
    /// applies both bounds as of `now`. Returns whether the list changed.
    pub fn record(&mut self, entry: ClosedWorktree, now: DateTime<Utc>) -> bool {
        let before = self.entries.clone();
        self.insert(entry);
        self.prune(now);
        self.entries != before
    }

    /// Drops the entries for `paths` — a worktree that is open again is not
    /// "closed". Returns whether anything was dropped.
    pub fn forget(&mut self, paths: &[PathBuf]) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| !paths.contains(&e.path));
        self.entries.len() != before
    }

    /// Marks the entry for `path` as removed from disk. Returns whether it
    /// changed (it is absent, or already removed, otherwise).
    pub fn mark_removed(&mut self, path: &Path) -> bool {
        match self.entries.iter_mut().find(|e| e.path == path) {
            Some(entry) if !entry.removed => {
                entry.removed = true;
                true
            }
            _ => false,
        }
    }

    /// Applies both bounds as of `now`: entries older than [`MAX_AGE_DAYS`] go,
    /// then the oldest beyond [`MAX_CLOSED`]. Returns whether anything went.
    pub fn prune(&mut self, now: DateTime<Utc>) -> bool {
        let before = self.entries.len();
        let oldest_kept = now - ChronoDuration::days(MAX_AGE_DAYS);
        self.entries.retain(|e| e.closed_at >= oldest_kept);
        // Newest first, so the tail is the oldest.
        self.entries.truncate(MAX_CLOSED);
        self.entries.len() != before
    }

    /// Inserts `entry` at its place in newest-first order, replacing any entry
    /// for the same path unless that entry is *newer* — the later closure wins
    /// whatever order the two arrive in (a reaped window carries the time it was
    /// last heard from, which can predate a closure recorded since).
    fn insert(&mut self, entry: ClosedWorktree) {
        if self
            .entries
            .iter()
            .any(|e| e.path == entry.path && e.closed_at > entry.closed_at)
        {
            return;
        }
        self.entries.retain(|e| e.path != entry.path);
        // Stable for equal timestamps: a later insert lands before an equal one,
        // so the most recently *recorded* of two simultaneous closures is first.
        let at = self
            .entries
            .partition_point(|e| e.closed_at > entry.closed_at);
        self.entries.insert(at, entry);
    }
}

/// The persisted file's shape.
#[derive(Serialize, Deserialize)]
struct ClosedFile {
    /// The format version; absent in a file from before it existed.
    #[serde(default)]
    version: u32,
    /// The closures, newest first.
    #[serde(default)]
    entries: Vec<ClosedWorktree>,
}

/// Serializes `entries` for the persisted file.
pub fn to_file_bytes(entries: &[ClosedWorktree]) -> Result<Vec<u8>> {
    let file = ClosedFile {
        version: FILE_VERSION,
        entries: entries.to_vec(),
    };
    serde_json::to_vec_pretty(&file).context("failed to serialize the closed-worktree log")
}

/// Parses the persisted file's contents.
pub fn from_file_bytes(bytes: &[u8]) -> Result<Vec<ClosedWorktree>> {
    let file: ClosedFile =
        serde_json::from_slice(bytes).context("failed to parse the closed-worktree log")?;
    Ok(file.entries)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn entry(path: &str, closed_at: DateTime<Utc>) -> ClosedWorktree {
        ClosedWorktree {
            path: PathBuf::from(path),
            repo_root: PathBuf::from("/repo"),
            main_repo: "repo".to_string(),
            github: None,
            branch: Some("feature".to_string()),
            head_sha: Some("abc123".to_string()),
            is_main: false,
            removed: false,
            closed_at,
        }
    }

    fn now() -> DateTime<Utc> {
        "2026-10-07T12:00:00Z".parse().unwrap()
    }

    fn minutes_ago(n: i64) -> DateTime<Utc> {
        now() - ChronoDuration::minutes(n)
    }

    fn paths(log: &ClosedLog) -> Vec<&str> {
        log.entries()
            .iter()
            .map(|e| e.path.to_str().unwrap())
            .collect()
    }

    #[test]
    fn records_newest_first_whatever_the_insertion_order() {
        let mut log = ClosedLog::default();
        log.record(entry("/b", minutes_ago(5)), now());
        log.record(entry("/c", minutes_ago(1)), now());
        log.record(entry("/a", minutes_ago(9)), now());
        assert_eq!(paths(&log), ["/c", "/b", "/a"]);
    }

    #[test]
    fn a_newer_closure_of_the_same_path_replaces_the_older_one() {
        let mut log = ClosedLog::default();
        log.record(entry("/a", minutes_ago(9)), now());
        log.record(entry("/b", minutes_ago(5)), now());
        let mut again = entry("/a", minutes_ago(1));
        again.removed = true;
        assert!(log.record(again, now()));
        assert_eq!(paths(&log), ["/a", "/b"]);
        assert!(log.get(Path::new("/a")).unwrap().removed);
    }

    #[test]
    fn an_older_closure_never_replaces_a_newer_one() {
        let mut log = ClosedLog::default();
        let mut removed = entry("/a", minutes_ago(1));
        removed.removed = true;
        log.record(removed, now());
        assert!(!log.record(entry("/a", minutes_ago(9)), now()));
        assert!(log.get(Path::new("/a")).unwrap().removed);
    }

    #[test]
    fn recording_an_identical_entry_changes_nothing() {
        let mut log = ClosedLog::default();
        assert!(log.record(entry("/a", minutes_ago(1)), now()));
        assert!(!log.record(entry("/a", minutes_ago(1)), now()));
    }

    #[test]
    fn the_size_bound_evicts_the_oldest_first() {
        let mut log = ClosedLog::default();
        for i in 0..=(MAX_CLOSED as i64) {
            // `/0` is the newest, `/50` the oldest.
            log.record(entry(&format!("/{i}"), minutes_ago(i)), now());
        }
        assert_eq!(log.entries().len(), MAX_CLOSED);
        assert!(log.get(Path::new("/0")).is_some());
        assert!(log.get(Path::new(&format!("/{MAX_CLOSED}"))).is_none());
    }

    #[test]
    fn the_age_bound_drops_entries_older_than_the_limit() {
        let mut log = ClosedLog::default();
        let old = now() - ChronoDuration::days(MAX_AGE_DAYS) - ChronoDuration::seconds(1);
        let edge = now() - ChronoDuration::days(MAX_AGE_DAYS);
        log.record(entry("/old", old), now());
        log.record(entry("/edge", edge), now());
        assert_eq!(paths(&log), ["/edge"]);
        // Entries age out without a new record, too.
        let later = now() + ChronoDuration::seconds(1);
        assert!(log.prune(later));
        assert!(log.entries().is_empty());
    }

    #[test]
    fn forget_drops_only_the_named_paths() {
        let mut log = ClosedLog::default();
        log.record(entry("/a", minutes_ago(3)), now());
        log.record(entry("/b", minutes_ago(2)), now());
        log.record(entry("/c", minutes_ago(1)), now());
        assert!(log.forget(&[PathBuf::from("/a"), PathBuf::from("/c")]));
        assert_eq!(paths(&log), ["/b"]);
        assert!(!log.forget(&[PathBuf::from("/nope")]));
    }

    #[test]
    fn mark_removed_is_idempotent_and_ignores_an_unknown_path() {
        let mut log = ClosedLog::default();
        log.record(entry("/a", minutes_ago(1)), now());
        assert!(log.mark_removed(Path::new("/a")));
        assert!(!log.mark_removed(Path::new("/a")));
        assert!(!log.mark_removed(Path::new("/nope")));
        assert!(log.get(Path::new("/a")).unwrap().removed);
    }

    #[test]
    fn the_file_round_trips_and_tolerates_an_older_shape() {
        let mut full = entry("/a", minutes_ago(1));
        full.github = Some(ClosedGithub {
            owner: "rust-works".to_string(),
            name: "omni-dev".to_string(),
        });
        full.removed = true;
        let bytes = to_file_bytes(&[full.clone()]).unwrap();
        assert_eq!(from_file_bytes(&bytes).unwrap(), vec![full]);

        // No `version`, no optional fields, no `removed`: an older or minimal file.
        let minimal = br#"{"entries":[{"path":"/a","repo_root":"/r","main_repo":"r",
            "is_main":false,"closed_at":"2026-10-07T11:00:00Z"}]}"#;
        let parsed = from_file_bytes(minimal).unwrap();
        assert_eq!(parsed.len(), 1);
        assert!(!parsed[0].removed);
        assert_eq!(parsed[0].branch, None);
    }

    #[test]
    fn a_corrupt_file_is_an_error_not_a_panic() {
        assert!(from_file_bytes(b"not json").is_err());
    }

    #[test]
    fn from_entries_dedupes_orders_and_prunes() {
        let stale = now() - ChronoDuration::days(MAX_AGE_DAYS + 1);
        let log = ClosedLog::from_entries(
            vec![
                entry("/a", minutes_ago(4)),
                entry("/b", minutes_ago(2)),
                entry("/a", minutes_ago(9)),
                entry("/stale", stale),
            ],
            now(),
        );
        assert_eq!(paths(&log), ["/b", "/a"]);
        assert_eq!(
            log.get(Path::new("/a")).unwrap().closed_at,
            minutes_ago(4),
            "the later of two records of one path wins",
        );
    }

    #[test]
    fn optional_fields_are_omitted_from_the_wire_when_empty() {
        let mut bare = entry("/a", minutes_ago(1));
        bare.branch = None;
        bare.head_sha = None;
        let json = serde_json::to_value(&bare).unwrap();
        assert!(json.get("branch").is_none());
        assert!(json.get("head_sha").is_none());
        assert!(json.get("github").is_none());
        assert_eq!(json["removed"], false);
    }
}
