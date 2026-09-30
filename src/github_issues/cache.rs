//! A short-lived, on-disk cache of fetched GitHub issues and pull requests
//! (#1858).
//!
//! `ai jev route` and `verify-decision` are typically re-run against the same
//! issue while tuning flags or prompts, by people and by agents alike. Each
//! run used to pay a fresh `gh api graphql` round trip for identical data;
//! this cache lets a run within [`DEFAULT_TTL`] of the last fetch reuse it.
//!
//! One JSON file per item, keyed by `(project, number)` and namespaced by the
//! `gh` login that fetched it, under the user cache directory:
//! `<cache_dir>/omni-dev/github-issues/<account>/<owner>/<repo>/<number>.json`.
//! `<account>` is a digest of the token `gh` is using (see `account_scope`),
//! so a different host or a switched `gh` account never reads another's
//! entries, and no login can be served issue text it could not fetch itself.
//! The project is lowercased in the path, since GitHub names are
//! case-insensitive. Both [`super::fetch_issues`] and [`super::fetch_items`]
//! build the same [`IssueDoc`] for an issue, so they share entries; a caller
//! that only accepts issues filters on [`IssueDoc::kind`] at lookup.
//!
//! The cache is best-effort throughout: a missing, unreadable, corrupt,
//! old-schema or expired entry is a miss, and a failed write is logged and
//! ignored, so the worst case is exactly the uncached behaviour. Not-found
//! results are never cached, so fixing a typo or granting `gh` access takes
//! effect at once. Entries hold issue text from possibly private
//! repositories, so [`IssueCache::prune_expired`] sweeps expired ones (and
//! the orphaned temp files of a crashed write) each run rather than leaving
//! them on disk indefinitely, along with any empty
//! `<account>/<owner>/<repo>` directory. A lookup never deletes: the fetch
//! that follows a miss overwrites the entry, and leaving removal to the sweep
//! means a reader can never delete a fresh entry a concurrent writer just
//! renamed into place.
//!
//! A flat file rather than a daemon op was a deliberate choice: the usage
//! pattern is *sequential* re-runs, which a shared file serves fully, while a
//! daemon would add a wire op, a daemon-down fallback and version skew for
//! the one extra it offers — deduplicating concurrent in-flight fetches.

use std::collections::HashSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use crate::provider::{IssueDoc, ItemRef};
use crate::utils::env::{non_empty_var, EnvSource};

/// Environment variable / settings key setting the cache lifetime in
/// seconds. `0` disables the cache entirely (no reads, no writes).
pub const GITHUB_CACHE_TTL_ENV: &str = "OMNI_DEV_GITHUB_CACHE_TTL_SECS";

/// How long a fetched item is reused when [`GITHUB_CACHE_TTL_ENV`] is unset.
///
/// Long enough to cover rapid re-runs, short enough that a closed or
/// newly-commented issue is seen soon (and `--refresh` covers "I just
/// changed it").
pub const DEFAULT_TTL: Duration = Duration::from_secs(300);

/// Bumped whenever the entry layout (or [`IssueDoc`]'s) changes; an entry
/// with any other value is a miss. A new `#[serde(default)]` field would
/// otherwise deserialise from an old entry without error and be served
/// empty, so `schema_tracks_the_issue_doc_shape` pins the field set.
const SCHEMA: u32 = 1;

/// The cache's directory under the user cache directory.
const CACHE_SUBDIR: [&str; 2] = ["omni-dev", "github-issues"];

/// Entries deeper than `<account>/<owner>/<repo>/<file>` are never written, so
/// the sweep does not descend further.
const MAX_SWEEP_DEPTH: usize = 4;

/// Extension of an in-flight write's temp file, which is renamed to `.json`.
const TEMP_EXT: &str = "partial";

/// How long a temp file may sit before the sweep treats it as orphaned by a
/// crash. Far longer than any write takes, so a live one is never removed.
const TEMP_GRACE: Duration = Duration::from_secs(3600);

/// One cached item, as stored on disk.
#[derive(Serialize, Deserialize)]
struct Entry {
    schema: u32,
    /// Unix seconds at which `doc` was fetched.
    fetched_at: u64,
    doc: IssueDoc,
}

/// How much of a run's GitHub input came from the cache.
///
/// Reported as `github_cache` in `route` and `verify-decision`'s JSON and YAML
/// output, and omitted when nothing was reused, so an uncached run serialises
/// as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CacheUsage {
    /// How many items were served from the cache.
    pub items_reused: usize,
    /// The age in seconds of the oldest of them.
    pub oldest_age_secs: u64,
}

/// The GitHub fetch cache shared by `route` and `verify-decision`.
///
/// Also counts how many items a run reused and the age of the oldest, so the
/// CLI can say so ([`reuse_note`](Self::reuse_note)) — stale input is never
/// silent.
#[derive(Debug)]
pub struct IssueCache {
    /// Everything this cache has ever written, across accounts, for the
    /// sweep. `None` only when there is no cache directory at all.
    root: Option<PathBuf>,
    /// Where this run reads and writes (`root` plus the account digest);
    /// `None` when the cache is disabled.
    dir: Option<PathBuf>,
    ttl: Duration,
    /// Skip reads (but still write), for `--refresh`.
    refresh: bool,
    hits: AtomicUsize,
    oldest_hit_secs: AtomicU64,
    /// Items this run fetched and stored itself, so reading one back (say, a
    /// judged issue that another judged issue cites) is not reported as a
    /// reuse of an earlier run's data.
    stored: Mutex<HashSet<(String, u64)>>,
}

impl IssueCache {
    /// A cache rooted at `dir` (the directory holding `<owner>/<repo>/`),
    /// reusing entries younger than `ttl`. `refresh` re-fetches everything
    /// while still writing the fresh copies back.
    #[must_use]
    pub fn new(dir: PathBuf, ttl: Duration, refresh: bool) -> Self {
        let active = (!ttl.is_zero()).then(|| dir.clone());
        Self::build(Some(dir), active, ttl, refresh)
    }

    fn build(root: Option<PathBuf>, dir: Option<PathBuf>, ttl: Duration, refresh: bool) -> Self {
        Self {
            root,
            dir,
            ttl,
            refresh,
            hits: AtomicUsize::new(0),
            oldest_hit_secs: AtomicU64::new(0),
            stored: Mutex::new(HashSet::new()),
        }
    }

    /// A cache that never reads or writes anything.
    #[must_use]
    pub fn disabled() -> Self {
        Self::build(None, None, Duration::ZERO, false)
    }

    /// The cache configured from `env` (in production, the `SettingsEnv` the
    /// command already loaded), rooted under `base` (`dirs::cache_dir()`).
    ///
    /// `account` is the `account_scope` digest of the `gh` login the fetches will
    /// run as. Without one (`gh` could not say who it is) the cache is
    /// disabled, since an entry it wrote could later be served to another
    /// login. With no base directory the cache is disabled too. A zero TTL
    /// disables reads and writes but keeps the root, so the sweep still
    /// clears what an earlier run cached.
    #[must_use]
    pub fn from_env_with(
        env: &impl EnvSource,
        base: Option<PathBuf>,
        account: Option<&str>,
        refresh: bool,
    ) -> Self {
        let Some(base) = base else {
            debug!("No user cache directory; the GitHub fetch cache is disabled");
            return Self::disabled();
        };
        let root = CACHE_SUBDIR.iter().fold(base, |dir, part| dir.join(part));
        let ttl = ttl_from_env(env);
        let dir = account
            .filter(|account| is_safe_segment(account))
            .filter(|_| !ttl.is_zero())
            .map(|account| root.join(account));
        if dir.is_none() && !ttl.is_zero() {
            debug!("No usable gh login; the GitHub fetch cache is disabled");
        }
        Self::build(Some(root), dir, ttl, refresh)
    }

    /// Returns the cached doc for `item_ref` if it is fresh and `accept`s it,
    /// counting the reuse. Never deletes: see the module docs.
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
                let shown = path.display();
                debug!("Ignoring GitHub cache entry {shown}: {e}");
                return None;
            }
        };
        // A corrupt or old-layout entry is a miss; the fetch it triggers
        // rewrites it, so it heals itself (debug, not warn).
        let entry = match serde_json::from_slice::<Entry>(&bytes) {
            Ok(entry) if entry.schema == SCHEMA => entry,
            Ok(_) => return None,
            Err(e) => {
                let shown = path.display();
                debug!("Ignoring unreadable GitHub cache entry {shown}: {e}");
                return None;
            }
        };
        let age = self.fresh_age(entry.fetched_at)?;
        // A mismatched key or a rejected kind is a miss but still a valid
        // entry (a cached pull request serves `fetch_items`), so it is kept.
        // GitHub names are case-insensitive, and so is the entry's path.
        if !entry.doc.project.eq_ignore_ascii_case(&item_ref.project)
            || entry.doc.number != item_ref.number
            || !accept(&entry.doc)
        {
            return None;
        }
        if !self.stored_this_run(&item_ref.project, item_ref.number) {
            self.hits.fetch_add(1, Ordering::Relaxed);
            self.oldest_hit_secs.fetch_max(age, Ordering::Relaxed);
        }
        // The caller's spelling, so the doc is what an uncached fetch of this
        // ref would have returned.
        let mut doc = entry.doc;
        doc.project.clone_from(&item_ref.project);
        Some(doc)
    }

    /// Deletes every expired entry, and every temp file a crashed write
    /// orphaned. Best-effort, like everything else here; **blocking**, so run
    /// it on the same blocking thread as the fetch.
    ///
    /// Age is the file's mtime, which a write sets to when it stored the entry,
    /// so the sweep never reads or parses an entry. It covers every account's
    /// entries, and with a zero TTL it removes them all.
    pub fn prune_expired(&self) {
        let Some(root) = &self.root else {
            return;
        };
        for (path, is_temp) in sweep_files(root) {
            // An unknown age (a vanished file, or an mtime in the future after
            // the clock moved) is not trusted, as in `fresh_age`; but only an
            // entry is removed on it, since a temp file may be live.
            let age = file_age(&path);
            let expired = if is_temp {
                age.is_some_and(|age| age >= TEMP_GRACE)
            } else {
                age.is_none_or(|age| age.as_secs() >= self.ttl.as_secs())
            };
            if expired {
                remove_entry(&path);
            }
        }
        remove_empty_dirs(root);
    }

    /// The age of an entry fetched at `fetched_at`, or `None` once it has
    /// expired. A `fetched_at` in the future means the clock went backwards;
    /// the entry's age is unknown, so it is not trusted either.
    fn fresh_age(&self, fetched_at: u64) -> Option<u64> {
        now_secs()
            .checked_sub(fetched_at)
            .filter(|age| *age < self.ttl.as_secs())
    }

    fn stored_this_run(&self, project: &str, number: u64) -> bool {
        self.stored
            .lock()
            .is_ok_and(|stored| stored.contains(&(project.to_ascii_lowercase(), number)))
    }

    /// Writes `doc` to the cache. Best-effort: a failure is logged at debug
    /// level and otherwise ignored, since the caller already has the doc.
    pub(super) fn store(&self, doc: &IssueDoc) {
        let Some(path) = self.entry_path(&doc.project, doc.number) else {
            return;
        };
        match write_entry(&path, doc) {
            Ok(()) => {
                if let Ok(mut stored) = self.stored.lock() {
                    stored.insert((doc.project.to_ascii_lowercase(), doc.number));
                }
            }
            Err(e) => {
                let (project, number) = (&doc.project, doc.number);
                debug!("Failed to cache {project}#{number}: {e:#}");
            }
        }
    }

    /// What this run reused, for the machine-readable output, or `None` when
    /// it reused nothing. The stderr [`reuse_note`](Self::reuse_note) is the
    /// human's copy of the same facts; a script reading only stdout needs this.
    #[must_use]
    pub fn usage(&self) -> Option<CacheUsage> {
        let items_reused = self.hits.load(Ordering::Relaxed);
        (items_reused > 0).then(|| CacheUsage {
            items_reused,
            oldest_age_secs: self.oldest_hit_secs.load(Ordering::Relaxed),
        })
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

    /// The entry file for `project#number` (lowercased, as GitHub names are
    /// case-insensitive), or `None` when the cache is disabled or `project`
    /// isn't a pair of path-safe segments.
    fn entry_path(&self, project: &str, number: u64) -> Option<PathBuf> {
        let dir = self.dir.as_ref()?;
        let project = project.to_ascii_lowercase();
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
            let default = DEFAULT_TTL.as_secs();
            warn!("Ignoring {GITHUB_CACHE_TTL_ENV}={raw:?} ({e}); using {default}s");
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

/// How many times a write starts over when the sweep removes its directory
/// from under it. A second collision in a row means a sweep is running in a
/// loop, not a race, and the write is given up as any other failed write is.
const WRITE_ATTEMPTS: usize = 3;

/// Writes one entry atomically: a `0600` temp file in the same `0700`
/// directory, then a rename, so a concurrent reader sees either the old entry
/// or the new one, never a partial file.
///
/// The sweep removes the directories it empties, and a directory is empty
/// between its creation and the temp file landing in it, so the whole
/// sequence starts over when a directory vanishes. Once the temp file exists
/// its directory is non-empty and `remove_dir` refuses it, so the rename
/// itself cannot lose that race.
fn write_entry(path: &Path, doc: &IssueDoc) -> Result<()> {
    retry_on_not_found(|| write_entry_once(path, doc))
}

/// Runs `write`, up to [`WRITE_ATTEMPTS`] times while it fails with `NotFound`.
fn retry_on_not_found(mut write: impl FnMut() -> Result<()>) -> Result<()> {
    let mut attempt = 1;
    loop {
        match write() {
            Err(e) if attempt < WRITE_ATTEMPTS && is_not_found(&e) => attempt += 1,
            result => return result,
        }
    }
}

fn is_not_found(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_err| io_err.kind() == std::io::ErrorKind::NotFound)
    })
}

fn write_entry_once(path: &Path, doc: &IssueDoc) -> Result<()> {
    crate::daemon::paths::ensure_parent_dir_0700(path)?;
    let dir = path.parent().context("cache entry path has no parent")?;
    let entry = Entry {
        schema: SCHEMA,
        fetched_at: now_secs(),
        doc: doc.clone(),
    };
    // `NamedTempFile` is created `0600` on Unix with a random name, so two
    // processes caching the same item never share a temp file. Its name is
    // recognisable (`TEMP_EXT`) so the sweep can remove one a crash orphaned.
    let mut tmp = tempfile::Builder::new()
        .prefix(".entry-")
        .suffix(&format!(".{TEMP_EXT}"))
        .tempfile_in(dir)
        .with_context(|| format!("Failed to create a temp file in {}", dir.display()))?;
    serde_json::to_writer(&mut tmp, &entry).context("Failed to serialise cache entry")?;
    tmp.persist(path)
        .with_context(|| format!("Failed to replace {}", path.display()))?;
    Ok(())
}

/// Deletes one file, best-effort: a failure only means it is tried again on
/// the next sweep.
fn remove_entry(path: &Path) {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            let shown = path.display();
            debug!("Failed to remove GitHub cache file {shown}: {e}");
        }
        _ => {}
    }
}

/// Removes every directory under `root` (never `root` itself) that holds
/// nothing, deepest first, down to [`MAX_SWEEP_DEPTH`] levels. Directories
/// that were already empty — an earlier release's, or a write that failed
/// after making its directory — go too, not only those this sweep emptied.
///
/// `remove_dir` refuses a directory that still holds anything — a fresh
/// entry, a live write's temp file, a file that is not the cache's — so it is
/// the emptiness check as well, and a directory a writer has just filled keeps
/// it. One a writer has just made but not yet filled can lose the race, which
/// `write_entry` absorbs. Symlinks are not followed.
fn remove_empty_dirs(root: &Path) {
    fn walk(dir: &Path, depth: usize) {
        let Ok(children) = std::fs::read_dir(dir) else {
            return;
        };
        for child in children.flatten() {
            if depth < MAX_SWEEP_DEPTH && child.file_type().is_ok_and(|kind| kind.is_dir()) {
                let path = child.path();
                walk(&path, depth + 1);
                log_unexpected_remove_dir_failure(&path, std::fs::remove_dir(&path));
            }
        }
    }
    walk(root, 1);
}

/// Logs a `remove_dir` failure worth knowing about. Refusing a directory that
/// still holds something, or one a concurrent sweep already removed, is the
/// sweep working as designed and stays silent.
fn log_unexpected_remove_dir_failure(path: &Path, result: std::io::Result<()>) {
    match result {
        Err(e) if !is_not_empty(&e) && e.kind() != std::io::ErrorKind::NotFound => {
            let shown = path.display();
            debug!("Failed to remove GitHub cache directory {shown}: {e}");
        }
        _ => {}
    }
}

/// Whether `remove_dir` refused because the directory still holds something:
/// `DirectoryNotEmpty`, or `AlreadyExists` where `rmdir` reports `EEXIST`.
fn is_not_empty(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::DirectoryNotEmpty | std::io::ErrorKind::AlreadyExists
    )
}

/// How long ago `path` was last written, or `None` if that is unknown (the
/// file is gone, or its mtime is in the future).
fn file_age(path: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

/// Every regular file the cache could have written under `root` — an entry
/// (`*.json`, `false`) or an in-flight write's temp file (`true`) — down to
/// [`MAX_SWEEP_DEPTH`] levels. Anything else is left alone, and symlinks are
/// never followed.
fn sweep_files(root: &Path) -> Vec<(PathBuf, bool)> {
    fn walk(dir: &Path, depth: usize, out: &mut Vec<(PathBuf, bool)>) {
        let Ok(children) = std::fs::read_dir(dir) else {
            return;
        };
        for child in children.flatten() {
            let path = child.path();
            let Ok(kind) = child.file_type() else {
                continue;
            };
            if kind.is_dir() {
                if depth < MAX_SWEEP_DEPTH {
                    walk(&path, depth + 1, out);
                }
            } else if kind.is_file() {
                match path.extension().and_then(|ext| ext.to_str()) {
                    Some("json") => out.push((path, false)),
                    Some(TEMP_EXT) => out.push((path, true)),
                    _ => {}
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, 1, &mut out);
    out
}

/// The cache namespace for one `gh` login: a digest of `token`, which `gh
/// auth token` prints for the account and host `gh` is currently using, so a
/// switched account or another host lands in a different directory. Only the
/// digest is kept; the token itself is never stored or logged.
#[must_use]
pub fn account_scope(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest[..8].iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    })
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
        let stored = doc("o/r", 7, ItemKind::Issue);
        cache(dir.path()).store(&stored);
        let cache = cache(dir.path());
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
    fn an_item_stored_this_run_is_served_but_not_reported_as_reused() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        cache.store(&doc("o/r", 7, ItemKind::Issue));
        assert!(cache.lookup(&item_ref("o/r", 7), |_| true).is_some());
        assert!(cache.reuse_note().is_none());
    }

    #[test]
    fn the_note_reports_the_count_and_oldest_age() {
        let dir = tempfile::tempdir().unwrap();
        cache(dir.path()).store(&doc("o/r", 1, ItemKind::Issue));
        cache(dir.path()).store(&doc("o/r", 2, ItemKind::Issue));
        backdate(dir.path(), "o/r", 2, 150);
        let cache = cache(dir.path());
        assert!(cache.lookup(&item_ref("o/r", 1), |_| true).is_some());
        assert!(cache.lookup(&item_ref("o/r", 2), |_| true).is_some());
        assert_eq!(
            cache.reuse_note().unwrap(),
            "note: reused 2 cached GitHub items, up to 2m old; pass --refresh to re-fetch"
        );
    }

    #[test]
    fn an_expired_entry_is_a_miss_and_is_left_for_the_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        cache.store(&doc("o/r", 7, ItemKind::Issue));
        backdate(dir.path(), "o/r", 7, 300);
        assert!(cache.lookup(&item_ref("o/r", 7), |_| true).is_none());
        assert!(cache.reuse_note().is_none());
        // A lookup must not delete: it could be racing a writer's rename.
        assert!(cache.entry_path("o/r", 7).unwrap().exists());
    }

    /// Sets `path`'s mtime to `secs_ago` seconds in the past (negative: the
    /// future), which is what the sweep reads.
    fn set_age(path: &Path, secs_ago: i64) {
        let now = SystemTime::now();
        let by = Duration::from_secs(secs_ago.unsigned_abs());
        let at = if secs_ago >= 0 { now - by } else { now + by };
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    #[test]
    fn prune_removes_entries_older_than_the_ttl_only() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        for number in 1..=3 {
            cache.store(&doc("o/r", number, ItemKind::Issue));
        }
        let path = |n| cache.entry_path("o/r", n).unwrap();
        set_age(&path(2), 3600);
        set_age(&path(3), -3600);
        let bystander = dir.path().join("o/r/notes.txt");
        std::fs::write(&bystander, "not ours").unwrap();
        set_age(&bystander, 86_400);

        cache.prune_expired();
        assert!(path(1).exists());
        assert!(!path(2).exists(), "expired");
        assert!(!path(3).exists(), "an mtime in the future is not trusted");
        assert!(bystander.exists(), "only the cache's own files are swept");
    }

    #[test]
    fn prune_removes_a_crashed_writes_temp_file_but_not_a_live_one() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        cache.store(&doc("o/r", 1, ItemKind::Issue));
        let repo_dir = dir.path().join("o/r");
        let orphan = repo_dir.join(format!(".entry-abc.{TEMP_EXT}"));
        let live = repo_dir.join(format!(".entry-def.{TEMP_EXT}"));
        std::fs::write(&orphan, "issue text").unwrap();
        std::fs::write(&live, "issue text").unwrap();
        set_age(&orphan, i64::try_from(TEMP_GRACE.as_secs()).unwrap() + 60);

        cache.prune_expired();
        assert!(!orphan.exists(), "an orphaned temp file holds issue text");
        assert!(live.exists(), "an in-flight write is never pruned");
    }

    #[test]
    fn a_write_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        cache(dir.path()).store(&doc("o/r", 1, ItemKind::Issue));
        let names: Vec<_> = std::fs::read_dir(dir.path().join("o/r"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["1.json"]);
    }

    #[test]
    fn prune_sweeps_every_accounts_entries_and_a_zero_ttl_sweeps_them_all() {
        let base = tempfile::tempdir().unwrap();
        let env = MapEnv::new();
        let open = |env: &MapEnv, account: &str| {
            IssueCache::from_env_with(env, Some(base.path().to_path_buf()), Some(account), false)
        };
        let (a, b) = (open(&env, "aaaa"), open(&env, "bbbb"));
        a.store(&doc("o/r", 1, ItemKind::Issue));
        b.store(&doc("o/r", 1, ItemKind::Issue));
        let (path_a, path_b) = (
            a.entry_path("o/r", 1).unwrap(),
            b.entry_path("o/r", 1).unwrap(),
        );
        set_age(&path_a, 3600);

        b.prune_expired();
        assert!(!path_a.exists(), "another account's expired entry");
        assert!(path_b.exists());

        // TTL 0 turns reads and writes off but must still clear the root:
        // what an earlier run cached is private text nothing else will delete.
        let off = open(&MapEnv::new().with(GITHUB_CACHE_TTL_ENV, "0"), "bbbb");
        assert!(off.dir.is_none());
        off.prune_expired();
        assert!(!path_b.exists());
    }

    /// The `<account>/<owner>/<repo>` directories under `root`, at any depth.
    fn dirs_under(root: &Path) -> Vec<PathBuf> {
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for child in std::fs::read_dir(dir).unwrap().flatten() {
                if child.file_type().unwrap().is_dir() {
                    out.push(child.path());
                    walk(&child.path(), out);
                }
            }
        }
        let mut out = Vec::new();
        walk(root, &mut out);
        out
    }

    #[test]
    fn prune_removes_the_directories_an_expired_tree_leaves_empty() {
        let base = tempfile::tempdir().unwrap();
        let env = MapEnv::new();
        let open = |account: &str| {
            IssueCache::from_env_with(&env, Some(base.path().to_path_buf()), Some(account), false)
        };
        let (a, b) = (open("aaaa"), open("bbbb"));
        for cache in [&a, &b] {
            cache.store(&doc("o/r", 1, ItemKind::Issue));
            cache.store(&doc("o/other", 1, ItemKind::Issue));
        }
        let entries = [("o/r", &a), ("o/other", &a), ("o/r", &b), ("o/other", &b)];
        for (project, cache) in entries {
            set_age(&cache.entry_path(project, 1).unwrap(), 3600);
        }

        a.prune_expired();
        let root = a.root.clone().unwrap();
        assert_eq!(dirs_under(&root), Vec::<PathBuf>::new());
        assert!(root.is_dir(), "the cache root itself is never removed");
    }

    #[test]
    fn prune_keeps_the_directories_a_fresh_sibling_or_bystander_still_needs() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        for (project, number) in [("o/r", 1), ("o/r", 2), ("o/gone", 1), ("o/kept", 1)] {
            cache.store(&doc(project, number, ItemKind::Issue));
        }
        let path = |project, n| cache.entry_path(project, n).unwrap();
        set_age(&path("o/r", 1), 3600);
        set_age(&path("o/gone", 1), 3600);
        set_age(&path("o/kept", 1), 3600);
        let bystander = path("o/kept", 1).with_file_name("notes.txt");
        std::fs::write(&bystander, "not ours").unwrap();

        cache.prune_expired();
        assert!(path("o/r", 2).exists(), "the fresh sibling is untouched");
        assert!(
            bystander.exists(),
            "a file that is not ours pins its directory"
        );
        assert!(!dir.path().join("o/gone").exists(), "emptied");
        assert!(dir.path().join("o/r").is_dir());
        assert!(dir.path().join("o/kept").is_dir());
        assert!(dir.path().join("o").is_dir(), "still has children");

        let mut left = dirs_under(dir.path());
        left.sort();
        let expected = ["o", "o/kept", "o/r"].map(|d| dir.path().join(d));
        assert_eq!(left, expected);
    }

    #[test]
    fn only_a_refusal_for_a_reason_other_than_content_or_absence_is_worth_logging() {
        use std::io::{Error, ErrorKind};
        // The kinds `remove_dir` gives for a directory that is not empty
        // (`ENOTEMPTY`, or `EEXIST` on platforms that report it so).
        assert!(is_not_empty(&Error::from(ErrorKind::DirectoryNotEmpty)));
        assert!(is_not_empty(&Error::from(ErrorKind::AlreadyExists)));
        assert!(!is_not_empty(&Error::from(ErrorKind::PermissionDenied)));

        // None of these may panic; the logged arm has no observable effect.
        let path = Path::new("/cache/o/r");
        log_unexpected_remove_dir_failure(path, Ok(()));
        log_unexpected_remove_dir_failure(path, Err(Error::from(ErrorKind::NotFound)));
        log_unexpected_remove_dir_failure(path, Err(Error::from(ErrorKind::DirectoryNotEmpty)));
        log_unexpected_remove_dir_failure(path, Err(Error::from(ErrorKind::PermissionDenied)));
    }

    #[test]
    fn prune_removes_directories_that_were_already_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        cache.store(&doc("o/kept", 1, ItemKind::Issue));
        std::fs::create_dir_all(dir.path().join("o/empty")).unwrap();
        std::fs::create_dir_all(dir.path().join("x/y")).unwrap();

        cache.prune_expired();
        assert!(cache.entry_path("o/kept", 1).unwrap().exists());
        assert!(!dir.path().join("o/empty").exists());
        assert!(!dir.path().join("x").exists());
    }

    #[test]
    fn prune_keeps_a_directory_holding_a_live_writes_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        cache.store(&doc("o/r", 1, ItemKind::Issue));
        let live = dir
            .path()
            .join("o/r")
            .join(format!(".entry-abc.{TEMP_EXT}"));
        std::fs::write(&live, "issue text").unwrap();
        set_age(&cache.entry_path("o/r", 1).unwrap(), 3600);

        cache.prune_expired();
        assert!(live.exists(), "the writer's directory survives its sweep");
    }

    #[test]
    fn a_write_after_the_sweep_removed_its_directories_recreates_them() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        let item = doc("o/r", 1, ItemKind::Issue);
        cache.store(&item);
        set_age(&cache.entry_path("o/r", 1).unwrap(), 3600);
        cache.prune_expired();
        assert!(!dir.path().join("o").exists());

        cache.store(&item);
        assert!(cache.entry_path("o/r", 1).unwrap().exists());
    }

    #[test]
    fn the_error_a_vanished_directory_gives_a_write_is_recognised() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        let err = tempfile::Builder::new()
            .tempfile_in(&gone)
            .with_context(|| format!("Failed to create a temp file in {}", gone.display()))
            .unwrap_err();
        assert!(is_not_found(&err), "{err:?}");
        assert!(!is_not_found(&anyhow::anyhow!("no io error at all")));
    }

    #[test]
    fn a_write_that_loses_its_directory_starts_over_a_bounded_number_of_times() {
        let vanished = || -> Result<()> {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
                .context("Failed to create a temp file")
        };

        let mut calls = 0;
        retry_on_not_found(|| {
            calls += 1;
            if calls < WRITE_ATTEMPTS {
                vanished()
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(calls, WRITE_ATTEMPTS, "the last attempt succeeds");

        let mut calls = 0;
        retry_on_not_found(|| {
            calls += 1;
            vanished()
        })
        .unwrap_err();
        assert_eq!(calls, WRITE_ATTEMPTS, "then it gives up");

        let mut calls = 0;
        retry_on_not_found(|| {
            calls += 1;
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied).into())
        })
        .unwrap_err();
        assert_eq!(calls, 1, "any other failure is not retried");
    }

    #[test]
    fn prune_is_a_no_op_when_disabled_or_empty() {
        IssueCache::disabled().prune_expired();
        let dir = tempfile::tempdir().unwrap();
        cache(&dir.path().join("missing")).prune_expired();
    }

    /// Adding, removing or renaming a field of [`IssueDoc`] or [`Comment`]
    /// changes what a cached entry means, so it must bump [`SCHEMA`] — and
    /// then update this list.
    #[test]
    fn schema_tracks_the_issue_doc_shape() {
        let mut issue = doc("o/r", 7, ItemKind::Issue);
        issue.comments.push(crate::provider::Comment {
            author: "a".to_string(),
            body: "b".to_string(),
            id: Some(1),
        });
        let value = serde_json::to_value(&issue).unwrap();
        let keys = |v: &serde_json::Value| -> Vec<String> {
            let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        };
        assert_eq!(
            (SCHEMA, keys(&value), keys(&value["comments"][0])),
            (
                1,
                [
                    "body", "closed_by", "comments", "kind", "number", "project", "provider",
                    "state", "title", "url"
                ]
                .map(String::from)
                .to_vec(),
                ["author", "body", "id"].map(String::from).to_vec(),
            ),
            "IssueDoc's shape changed: bump SCHEMA in github_issues/cache.rs, then update this test"
        );
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
    fn remove_entry_logs_and_ignores_a_non_not_found_failure() {
        let dir = tempfile::tempdir().unwrap();
        // remove_file on a directory fails with something other than
        // NotFound, exercising the branch a missing entry never takes.
        let path = dir.path().join("looks_like_an_entry");
        std::fs::create_dir(&path).unwrap();
        remove_entry(&path);
        assert!(path.exists(), "a directory can't be removed as a file");
    }

    #[test]
    fn env_ttl_defaults_parses_and_ignores_garbage() {
        let base = Some(PathBuf::from("/cache"));
        let ttl =
            |env: &MapEnv| IssueCache::from_env_with(env, base.clone(), Some("acct"), false).ttl;
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
            Some("acct"),
            false,
        );
        assert!(off.dir.is_none());
        assert!(
            IssueCache::from_env_with(&MapEnv::new(), None, Some("acct"), false)
                .dir
                .is_none()
        );
    }

    #[test]
    fn no_usable_gh_login_disables_the_cache_but_keeps_the_root_to_sweep() {
        let base = Some(PathBuf::from("/cache"));
        for account in [None, Some(""), Some(".."), Some("a/b")] {
            let cache = IssueCache::from_env_with(&MapEnv::new(), base.clone(), account, false);
            assert!(cache.dir.is_none(), "{account:?}");
            assert!(cache.root.is_some(), "{account:?}");
        }
    }

    #[test]
    fn the_cache_lives_under_omni_dev_github_issues() {
        let cache = IssueCache::from_env_with(
            &MapEnv::new(),
            Some(PathBuf::from("/c")),
            Some("0123abcd"),
            false,
        );
        assert_eq!(
            cache.entry_path("o/r", 7).unwrap(),
            Path::new("/c/omni-dev/github-issues/0123abcd/o/r/7.json")
        );
    }

    #[test]
    fn a_different_gh_login_never_reads_an_entry_it_could_not_have_fetched() {
        let base = tempfile::tempdir().unwrap();
        let open = |account: &str| {
            IssueCache::from_env_with(
                &MapEnv::new(),
                Some(base.path().to_path_buf()),
                Some(account),
                false,
            )
        };
        open(&account_scope("token-a")).store(&doc("o/r", 7, ItemKind::Issue));
        let item = item_ref("o/r", 7);
        assert!(open(&account_scope("token-a"))
            .lookup(&item, |_| true)
            .is_some());
        assert!(open(&account_scope("token-b"))
            .lookup(&item, |_| true)
            .is_none());
    }

    #[test]
    fn the_account_scope_is_a_stable_path_safe_digest_that_hides_the_token() {
        let scope = account_scope("gho_secret");
        assert_eq!(scope, account_scope("gho_secret"));
        assert_ne!(scope, account_scope("gho_other"));
        assert_eq!(scope.len(), 16);
        assert!(
            is_safe_segment(&scope) && !scope.contains("secret"),
            "{scope}"
        );
    }

    #[test]
    fn owner_and_repo_case_share_one_entry_and_the_callers_spelling_wins() {
        let dir = tempfile::tempdir().unwrap();
        let cache = cache(dir.path());
        cache.store(&doc("Rust-Works/Omni-Dev", 1, ItemKind::Issue));
        for spelling in [
            "Rust-Works/Omni-Dev",
            "rust-works/omni-dev",
            "RUST-WORKS/omni-dev",
        ] {
            let served = cache.lookup(&item_ref(spelling, 1), |_| true).unwrap();
            assert_eq!(served.project, spelling);
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn usage_reports_the_count_and_oldest_age_for_machine_readable_output() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(cache(dir.path()).usage(), None);
        cache(dir.path()).store(&doc("o/r", 1, ItemKind::Issue));
        cache(dir.path()).store(&doc("o/r", 2, ItemKind::Issue));
        backdate(dir.path(), "o/r", 2, 150);
        let cache = cache(dir.path());
        assert!(cache.lookup(&item_ref("o/r", 1), |_| true).is_some());
        assert!(cache.lookup(&item_ref("o/r", 2), |_| true).is_some());
        let usage = cache.usage().unwrap();
        assert_eq!(usage.items_reused, 2);
        assert!((150..=152).contains(&usage.oldest_age_secs), "{usage:?}");
    }

    #[test]
    fn ages_format_in_seconds_then_minutes() {
        assert_eq!(format_age(0), "0s");
        assert_eq!(format_age(119), "119s");
        assert_eq!(format_age(120), "2m");
        assert_eq!(format_age(299), "4m");
    }
}
