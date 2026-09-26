//! A short-lived, on-disk cache of fetched GitHub issues and pull requests
//! (#1858).
//!
//! `ai jev route` and `verify-decision` are typically re-run against the same
//! issue while tuning flags or prompts, by people and by agents alike. Each
//! run used to pay a fresh `gh api graphql` round trip for identical data;
//! this cache lets a run within [`DEFAULT_TTL`] of the last fetch reuse it.
//!
//! One JSON file per item, keyed by `(project, number)`, under the user cache
//! directory: `<cache_dir>/omni-dev/github-issues/<owner>/<repo>/<number>.json`.
//! Both [`super::fetch_issues`] and [`super::fetch_items`] build the same
//! [`IssueDoc`] for an issue, so they share entries; a caller that only
//! accepts issues filters on [`IssueDoc::kind`] at lookup.
//!
//! The cache is best-effort throughout: a missing, unreadable, corrupt,
//! old-schema or expired entry is a miss, and a failed write is logged and
//! ignored, so the worst case is exactly the uncached behaviour. Not-found
//! results are never cached, so fixing a typo or granting `gh` access takes
//! effect at once.
//!
//! A flat file rather than a daemon op was a deliberate choice: the usage
//! pattern is *sequential* re-runs, which a shared file serves fully, while a
//! daemon would add a wire op, a daemon-down fallback and version skew for
//! the one extra it offers — deduplicating concurrent in-flight fetches.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::provider::{IssueDoc, ItemRef};
use crate::utils::env::{non_empty_var, EnvSource};

/// Environment variable / settings key setting the cache lifetime in
/// seconds. `0` disables the cache entirely (no reads, no writes).
pub const GITHUB_CACHE_TTL_ENV: &str = "OMNI_DEV_GITHUB_CACHE_TTL_SECS";

/// How long a fetched item is reused when [`GITHUB_CACHE_TTL_ENV`] is unset.
///
/// Long enough to cover rapid re-runs, short enough that a newly posted
/// decision comment is seen soon (and `--refresh` covers "I just commented").
pub const DEFAULT_TTL: Duration = Duration::from_secs(300);

/// Bumped whenever the entry layout (or [`IssueDoc`]'s) changes
/// incompatibly; an entry with any other value is a miss.
const SCHEMA: u32 = 1;

/// The cache's directory under the user cache directory.
const CACHE_SUBDIR: [&str; 2] = ["omni-dev", "github-issues"];

/// One cached item, as stored on disk.
#[derive(Serialize, Deserialize)]
struct Entry {
    schema: u32,
    /// Unix seconds at which `doc` was fetched.
    fetched_at: u64,
    doc: IssueDoc,
}

/// The GitHub fetch cache shared by `route` and `verify-decision`.
///
/// Also counts how many items a run reused and the age of the oldest, so the
/// CLI can say so ([`reuse_note`](Self::reuse_note)) — stale input is never
/// silent.
#[derive(Debug)]
pub struct IssueCache {
    /// `None` when the cache is disabled.
    dir: Option<PathBuf>,
    ttl: Duration,
    /// Skip reads (but still write), for `--refresh`.
    refresh: bool,
    hits: AtomicUsize,
    oldest_hit_secs: AtomicU64,
}

impl IssueCache {
    /// A cache rooted at `dir` (the directory holding `<owner>/<repo>/`),
    /// reusing entries younger than `ttl`. `refresh` re-fetches everything
    /// while still writing the fresh copies back.
    #[must_use]
    pub fn new(dir: PathBuf, ttl: Duration, refresh: bool) -> Self {
        Self {
            dir: (!ttl.is_zero()).then_some(dir),
            ttl,
            refresh,
            hits: AtomicUsize::new(0),
            oldest_hit_secs: AtomicU64::new(0),
        }
    }

    /// A cache that never reads or writes anything.
    #[must_use]
    pub fn disabled() -> Self {
        Self::new(PathBuf::new(), Duration::ZERO, false)
    }

    /// The cache configured from the environment (with the `settings.json`
    /// fallback), rooted under the user cache directory.
    #[must_use]
    pub fn from_env(refresh: bool) -> Self {
        Self::from_env_with(
            &crate::utils::settings::SettingsEnv::load(),
            dirs::cache_dir(),
            refresh,
        )
    }

    /// [`from_env`](Self::from_env) over an injected [`EnvSource`] and base
    /// cache directory. With no base directory the cache is disabled.
    #[must_use]
    pub fn from_env_with(env: &impl EnvSource, base: Option<PathBuf>, refresh: bool) -> Self {
        let Some(base) = base else {
            debug!("No user cache directory; the GitHub fetch cache is disabled");
            return Self::disabled();
        };
        let dir = CACHE_SUBDIR.iter().fold(base, |dir, part| dir.join(part));
        Self::new(dir, ttl_from_env(env), refresh)
    }

    /// Returns the cached doc for `item_ref` if it is fresh and `accept`s it,
    /// counting the reuse.
    pub(super) fn lookup(
        &self,
        item_ref: &ItemRef,
        accept: impl Fn(&IssueDoc) -> bool,
    ) -> Option<IssueDoc> {
        if self.refresh {
            return None;
        }
        let path = self.entry_path(&item_ref.project, item_ref.number)?;
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                debug!("Ignoring GitHub cache entry {}: {e}", path.display());
                return None;
            }
        };
        // A corrupt or old-layout entry is rewritten by the fetch this miss
        // triggers, so it heals itself; debug, not warn.
        let entry: Entry = match serde_json::from_slice(&bytes) {
            Ok(entry) => entry,
            Err(e) => {
                debug!(
                    "Ignoring unreadable GitHub cache entry {}: {e}",
                    path.display()
                );
                return None;
            }
        };
        if entry.schema != SCHEMA
            || entry.doc.project != item_ref.project
            || entry.doc.number != item_ref.number
            || !accept(&entry.doc)
        {
            return None;
        }
        // A `fetched_at` in the future means the clock went backwards; the
        // entry's age is unknown, so it is not trusted.
        let age = now_secs().checked_sub(entry.fetched_at)?;
        if age >= self.ttl.as_secs() {
            return None;
        }
        self.hits.fetch_add(1, Ordering::Relaxed);
        self.oldest_hit_secs.fetch_max(age, Ordering::Relaxed);
        Some(entry.doc)
    }

    /// Writes `doc` to the cache. Best-effort: a failure is logged at debug
    /// level and otherwise ignored, since the caller already has the doc.
    pub(super) fn store(&self, doc: &IssueDoc) {
        let Some(path) = self.entry_path(&doc.project, doc.number) else {
            return;
        };
        if let Err(e) = write_entry(&path, doc) {
            debug!("Failed to cache {}#{}: {e:#}", doc.project, doc.number);
        }
    }

    /// A one-line note for stderr when this run reused cached items, naming
    /// how many, the oldest's age, and how to bypass the cache.
    #[must_use]
    pub fn reuse_note(&self) -> Option<String> {
        let hits = self.hits.load(Ordering::Relaxed);
        if hits == 0 {
            return None;
        }
        let age = format_age(self.oldest_hit_secs.load(Ordering::Relaxed));
        let noun = if hits == 1 { "item" } else { "items" };
        Some(format!(
            "note: reused {hits} cached GitHub {noun}, up to {age} old; pass --refresh to re-fetch"
        ))
    }

    /// The entry file for `project#number`, or `None` when the cache is
    /// disabled or `project` isn't a pair of path-safe segments.
    fn entry_path(&self, project: &str, number: u64) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        let (owner, repo) = project.split_once('/')?;
        if !is_safe_segment(owner) || !is_safe_segment(repo) {
            return None;
        }
        Some(dir.join(owner).join(repo).join(format!("{number}.json")))
    }
}

/// Parses [`GITHUB_CACHE_TTL_ENV`], warning on (and ignoring) a malformed
/// value rather than silently disabling or keeping the cache.
fn ttl_from_env(env: &impl EnvSource) -> Duration {
    let Some(raw) = non_empty_var(env, GITHUB_CACHE_TTL_ENV) else {
        return DEFAULT_TTL;
    };
    match raw.trim().parse::<u64>() {
        Ok(secs) => Duration::from_secs(secs),
        Err(e) => {
            warn!(
                "Ignoring {GITHUB_CACHE_TTL_ENV}={raw:?} ({e}); using {}s",
                DEFAULT_TTL.as_secs()
            );
            DEFAULT_TTL
        }
    }
}

/// Whether `segment` can be used as a path component as-is: GitHub owner and
/// repository names only use ASCII alphanumerics, `-`, `_` and `.`, and `.`
/// and `..` would escape the cache directory.
fn is_safe_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Writes one entry atomically: a `0600` temp file in the same `0700`
/// directory, then a rename, so a concurrent reader sees either the old entry
/// or the new one, never a partial file.
fn write_entry(path: &Path, doc: &IssueDoc) -> Result<()> {
    let dir = path.parent().context("cache entry path has no parent")?;
    crate::daemon::paths::ensure_dir_0700(dir)?;
    let entry = Entry {
        schema: SCHEMA,
        fetched_at: now_secs(),
        doc: doc.clone(),
    };
    // `NamedTempFile` is created `0600` on Unix with a random name, so two
    // processes caching the same item never share a temp file.
    let mut tmp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("Failed to create a temp file in {}", dir.display()))?;
    serde_json::to_writer(&mut tmp, &entry).context("Failed to serialise cache entry")?;
    tmp.persist(path)
        .with_context(|| format!("Failed to replace {}", path.display()))?;
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `42s` under two minutes, else whole minutes (`7m`).
fn format_age(secs: u64) -> String {
    if secs < 120 {
        format!("{secs}s")
    } else {
        format!("{}m", secs / 60)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::provider::{GitProvider, ItemKind, ItemState};
    use crate::test_support::env::MapEnv;

    fn doc(project: &str, number: u64, kind: ItemKind) -> IssueDoc {
        IssueDoc {
            provider: GitProvider::GitHub,
            project: project.to_string(),
            number,
            kind,
            title: format!("title {number}"),
            state: ItemState::Open,
            body: "body".to_string(),
            comments: Vec::new(),
            closed_by: Vec::new(),
            url: format!("https://github.com/{project}/issues/{number}"),
        }
    }

    fn item_ref(project: &str, number: u64) -> ItemRef {
        ItemRef {
            provider: GitProvider::GitHub,
            project: project.to_string(),
            kind: ItemKind::Issue,
            number,
        }
    }

    fn cache(dir: &Path) -> IssueCache {
        IssueCache::new(dir.to_path_buf(), DEFAULT_TTL, false)
    }

    /// Overwrites an entry's `fetched_at`, to age it without sleeping.
    fn backdate(dir: &Path, project: &str, number: u64, secs_ago: i64) {
        let path = cache(dir).entry_path(project, number).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let at = i64::try_from(now_secs()).unwrap() - secs_ago;
        value["fetched_at"] = serde_json::json!(at);
        std::fs::write(&path, value.to_string()).unwrap();
    }

    #[test]
    fn a_stored_doc_is_served_back_and_counted() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        let stored = doc("o/r", 7, ItemKind::Issue);
        cache.store(&stored);
        assert_eq!(cache.lookup(&item_ref("o/r", 7), |_| true), Some(stored));
        assert!(cache.lookup(&item_ref("o/r", 8), |_| true).is_none());
        let note = cache.reuse_note().unwrap();
        assert!(
            note.starts_with("note: reused 1 cached GitHub item,"),
            "{note}"
        );
        assert!(note.contains("--refresh"), "{note}");
    }

    #[test]
    fn nothing_reused_means_no_note() {
        let dir = tempfile::tempdir().unwrap();
        assert!(cache(dir.path()).reuse_note().is_none());
    }

    #[test]
    fn the_note_reports_the_count_and_oldest_age() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        cache.store(&doc("o/r", 1, ItemKind::Issue));
        cache.store(&doc("o/r", 2, ItemKind::Issue));
        backdate(dir.path(), "o/r", 2, 150);
        assert!(cache.lookup(&item_ref("o/r", 1), |_| true).is_some());
        assert!(cache.lookup(&item_ref("o/r", 2), |_| true).is_some());
        assert_eq!(
            cache.reuse_note().unwrap(),
            "note: reused 2 cached GitHub items, up to 2m old; pass --refresh to re-fetch"
        );
    }

    #[test]
    fn an_expired_entry_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        cache.store(&doc("o/r", 7, ItemKind::Issue));
        backdate(dir.path(), "o/r", 7, 300);
        assert!(cache.lookup(&item_ref("o/r", 7), |_| true).is_none());
        assert!(cache.reuse_note().is_none());
    }

    #[test]
    fn a_future_fetched_at_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        cache.store(&doc("o/r", 7, ItemKind::Issue));
        backdate(dir.path(), "o/r", 7, -60);
        assert!(cache.lookup(&item_ref("o/r", 7), |_| true).is_none());
    }

    #[test]
    fn a_corrupt_or_old_schema_entry_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        cache.store(&doc("o/r", 7, ItemKind::Issue));
        let path = cache.entry_path("o/r", 7).unwrap();

        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["schema"] = serde_json::json!(SCHEMA + 1);
        std::fs::write(&path, value.to_string()).unwrap();
        assert!(cache.lookup(&item_ref("o/r", 7), |_| true).is_none());

        std::fs::write(&path, "{not json").unwrap();
        assert!(cache.lookup(&item_ref("o/r", 7), |_| true).is_none());
    }

    #[test]
    fn an_entry_filed_under_the_wrong_key_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        cache.store(&doc("o/r", 7, ItemKind::Issue));
        let from = cache.entry_path("o/r", 7).unwrap();
        std::fs::copy(&from, cache.entry_path("o/r", 8).unwrap()).unwrap();
        assert!(cache.lookup(&item_ref("o/r", 8), |_| true).is_none());
    }

    #[test]
    fn a_rejected_kind_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        cache.store(&doc("o/r", 7, ItemKind::ChangeRequest));
        let issues_only = |d: &IssueDoc| d.kind == ItemKind::Issue;
        assert!(cache.lookup(&item_ref("o/r", 7), issues_only).is_none());
        assert!(cache.lookup(&item_ref("o/r", 7), |_| true).is_some());
    }

    #[test]
    fn refresh_skips_reads_but_still_writes() {
        let dir = tempfile::tempdir().unwrap();
        let refreshing = IssueCache::new(dir.path().to_path_buf(), DEFAULT_TTL, true);
        refreshing.store(&doc("o/r", 7, ItemKind::Issue));
        assert!(refreshing.lookup(&item_ref("o/r", 7), |_| true).is_none());
        assert!(cache(dir.path())
            .lookup(&item_ref("o/r", 7), |_| true)
            .is_some());
    }

    #[test]
    fn a_zero_ttl_neither_reads_nor_writes() {
        let dir = tempfile::tempdir().unwrap();
        let off = IssueCache::new(dir.path().to_path_buf(), Duration::ZERO, false);
        off.store(&doc("o/r", 7, ItemKind::Issue));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        cache(dir.path()).store(&doc("o/r", 7, ItemKind::Issue));
        assert!(off.lookup(&item_ref("o/r", 7), |_| true).is_none());
        assert!(IssueCache::disabled()
            .lookup(&item_ref("o/r", 7), |_| true)
            .is_none());
    }

    #[test]
    fn unsafe_project_segments_are_never_cached() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        for project in ["../r", "o/..", "o/.", "no-slash", "o/r/x", "o w/r", "/r"] {
            assert!(cache.entry_path(project, 1).is_none(), "{project}");
        }
        assert!(cache.entry_path("rust-works/.github", 1).is_some());
    }

    #[cfg(unix)]
    #[test]
    fn entries_are_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        cache.store(&doc("o/r", 7, ItemKind::Issue));
        let path = cache.entry_path("o/r", 7).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    #[test]
    fn a_failed_write_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the owner directory should be makes every write fail.
        std::fs::write(dir.path().join("o"), "").unwrap();
        let cache = cache(dir.path());
        cache.store(&doc("o/r", 7, ItemKind::Issue));
        assert!(cache.lookup(&item_ref("o/r", 7), |_| true).is_none());
    }

    #[test]
    fn env_ttl_defaults_parses_and_ignores_garbage() {
        let base = Some(PathBuf::from("/cache"));
        let ttl = |env: &MapEnv| IssueCache::from_env_with(env, base.clone(), false).ttl;
        assert_eq!(ttl(&MapEnv::new()), DEFAULT_TTL);
        assert_eq!(
            ttl(&MapEnv::new().with(GITHUB_CACHE_TTL_ENV, "60")),
            Duration::from_secs(60)
        );
        assert_eq!(
            ttl(&MapEnv::new().with(GITHUB_CACHE_TTL_ENV, "soon")),
            DEFAULT_TTL
        );
    }

    #[test]
    fn env_ttl_zero_or_no_cache_dir_disables_the_cache() {
        let off = IssueCache::from_env_with(
            &MapEnv::new().with(GITHUB_CACHE_TTL_ENV, "0"),
            Some(PathBuf::from("/cache")),
            false,
        );
        assert!(off.dir.is_none());
        assert!(IssueCache::from_env_with(&MapEnv::new(), None, false)
            .dir
            .is_none());
    }

    #[test]
    fn the_cache_lives_under_omni_dev_github_issues() {
        let cache = IssueCache::from_env_with(&MapEnv::new(), Some(PathBuf::from("/c")), false);
        assert_eq!(
            cache.entry_path("o/r", 7).unwrap(),
            Path::new("/c/omni-dev/github-issues/o/r/7.json")
        );
    }

    #[test]
    fn ages_format_in_seconds_then_minutes() {
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(119), "119s");
        assert_eq!(format_age(120), "2m");
        assert_eq!(format_age(299), "4m");
    }
}
