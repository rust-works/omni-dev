//! Lazy, batched fetch of the daemon's `ahead-behind` op for the worktrees a
//! consumer currently cares about.
//!
//! The streamed `tree` snapshot deliberately omits ahead/behind divergence
//! (#1306 — the dominant per-worktree cost when computed eagerly), so a
//! client fetches it on demand. `worktrees tree --follow`'s existing
//! precedent (`src/cli/worktrees.rs::enrich_ahead_behind`) re-fetches *every*
//! visible worktree on *every* frame; this cache is the explicit improvement
//! the plan calls for — it only fetches a path once, and only re-fetches it
//! when [`invalidate`](AheadBehindCache::invalidate) says its OIDs moved.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

use super::client::WorktreesClient;
use super::view_model::AheadBehindState;
use super::wire::AheadBehindEntryWire;

pub struct AheadBehindCache {
    client: WorktreesClient,
    entries: HashMap<PathBuf, AheadBehindState>,
    pending: HashSet<PathBuf>,
    results_tx: mpsc::UnboundedSender<FetchResult>,
    results_rx: mpsc::UnboundedReceiver<FetchResult>,
}

/// One completed batch: the paths it was fetched *for* (so results are
/// merged in scope — a path absent from a stale, still-in-flight batch's
/// result set is never confused with one from a newer batch) and what the
/// daemon reported.
struct FetchResult {
    requested: Vec<PathBuf>,
    results: HashMap<PathBuf, AheadBehindEntryWire>,
}

impl AheadBehindCache {
    pub fn new(client: WorktreesClient) -> Self {
        let (results_tx, results_rx) = mpsc::unbounded_channel();
        Self {
            client,
            entries: HashMap::new(),
            pending: HashSet::new(),
            results_tx,
            results_rx,
        }
    }

    /// Narrows the cache to `paths`: drops entries for paths no longer of
    /// interest, and queues a batched fetch for any newly-of-interest path
    /// with no cached (or already in-flight) entry.
    pub fn set_visible(&mut self, paths: &[PathBuf]) {
        let visible: HashSet<&PathBuf> = paths.iter().collect();
        self.entries.retain(|path, _| visible.contains(path));
        let to_fetch: Vec<PathBuf> = paths
            .iter()
            .filter(|p| !self.entries.contains_key(*p) && !self.pending.contains(*p))
            .cloned()
            .collect();
        if !to_fetch.is_empty() {
            self.spawn_fetch(to_fetch);
        }
    }

    /// Drops a cached entry so a later `set_visible` call re-fetches it.
    /// Called by the hub actor when a worktree's `head_sha`/`upstream_sha`
    /// moves (a commit or a push) since its ahead/behind was last computed.
    pub fn invalidate(&mut self, path: &Path) {
        self.entries.remove(path);
    }

    fn spawn_fetch(&mut self, paths: Vec<PathBuf>) {
        for path in &paths {
            self.pending.insert(path.clone());
        }
        let client = self.client.clone();
        let tx = self.results_tx.clone();
        let requested = paths.clone();
        tokio::spawn(async move {
            let results = client.fetch_ahead_behind(&paths).await.unwrap_or_default();
            let _ = tx.send(FetchResult { requested, results });
        });
    }

    /// Resolves once a fetch batch's results land, merging them into the
    /// cache. Intended as a `tokio::select!` branch alongside a hub's other
    /// feeds; never resolves if nothing has ever been queued.
    pub async fn changed(&mut self) {
        if let Some(FetchResult { requested, results }) = self.results_rx.recv().await {
            for path in requested {
                let state = match results.get(&path) {
                    Some(entry) => match (entry.ahead, entry.behind) {
                        (Some(ahead), Some(behind)) => AheadBehindState::Known {
                            ahead,
                            behind,
                            main_behind: entry.main_behind,
                        },
                        _ => AheadBehindState::Unavailable,
                    },
                    // The daemon omits a path entirely when it has no
                    // upstream to compare against — that is "unavailable",
                    // not a fetch failure worth retrying.
                    None => AheadBehindState::Unavailable,
                };
                self.entries.insert(path.clone(), state);
                self.pending.remove(&path);
            }
        }
    }

    pub fn get(&self, path: &Path) -> AheadBehindState {
        if let Some(state) = self.entries.get(path) {
            return *state;
        }
        if self.pending.contains(path) {
            AheadBehindState::Loading
        } else {
            AheadBehindState::Unknown
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn cache() -> AheadBehindCache {
        AheadBehindCache::new(WorktreesClient::new("/tmp/nonexistent-omni-dev-test.sock"))
    }

    #[test]
    fn unfetched_path_is_unknown() {
        let cache = cache();
        assert_eq!(cache.get(Path::new("/repo/wt")), AheadBehindState::Unknown);
    }

    // `set_visible` spawns a fetch task via `tokio::spawn` whenever it queues
    // a new path, so these need a runtime context (`#[tokio::test]`) even
    // though nothing here is awaited.

    #[tokio::test]
    async fn set_visible_marks_newly_visible_paths_loading() {
        let mut cache = cache();
        cache.set_visible(&[PathBuf::from("/repo/wt")]);
        assert_eq!(cache.get(Path::new("/repo/wt")), AheadBehindState::Loading);
    }

    #[tokio::test]
    async fn set_visible_drops_entries_for_paths_no_longer_visible() {
        let mut cache = cache();
        cache.entries.insert(
            PathBuf::from("/repo/gone"),
            AheadBehindState::Known {
                ahead: 1,
                behind: 0,
                main_behind: None,
            },
        );
        cache.set_visible(&[PathBuf::from("/repo/still-here")]);
        assert_eq!(
            cache.get(Path::new("/repo/gone")),
            AheadBehindState::Unknown
        );
    }

    #[tokio::test]
    async fn set_visible_does_not_re_fetch_an_already_pending_path() {
        let mut cache = cache();
        cache.set_visible(&[PathBuf::from("/repo/wt")]);
        assert_eq!(cache.pending.len(), 1);
        cache.set_visible(&[PathBuf::from("/repo/wt")]);
        // Still exactly one in-flight fetch queued for this path — a second
        // `set_visible` call with the same path must not spawn a duplicate.
        assert_eq!(cache.pending.len(), 1);
    }

    #[tokio::test]
    async fn changed_merges_a_completed_batch_and_clears_pending() {
        let mut cache = cache();
        let path = PathBuf::from("/repo/wt");
        cache.pending.insert(path.clone());
        let mut results = HashMap::new();
        results.insert(
            path.clone(),
            AheadBehindEntryWire {
                ahead: Some(2),
                behind: Some(1),
                main_behind: Some(5),
            },
        );
        cache
            .results_tx
            .send(FetchResult {
                requested: vec![path.clone()],
                results,
            })
            .unwrap();
        cache.changed().await;
        assert_eq!(
            cache.get(&path),
            AheadBehindState::Known {
                ahead: 2,
                behind: 1,
                main_behind: Some(5)
            }
        );
        assert!(!cache.pending.contains(&path));
    }

    #[tokio::test]
    async fn changed_treats_a_missing_result_entry_as_unavailable_not_zero() {
        let mut cache = cache();
        let path = PathBuf::from("/repo/no-upstream");
        cache.pending.insert(path.clone());
        cache
            .results_tx
            .send(FetchResult {
                requested: vec![path.clone()],
                results: HashMap::new(),
            })
            .unwrap();
        cache.changed().await;
        assert_eq!(cache.get(&path), AheadBehindState::Unavailable);
    }

    #[tokio::test]
    async fn changed_only_resolves_the_paths_its_own_batch_requested() {
        // A still-in-flight batch for a *different* path must not be marked
        // resolved (or clobbered to Unavailable) by an unrelated batch's
        // result arriving first.
        let mut cache = cache();
        let a = PathBuf::from("/repo/a");
        let b = PathBuf::from("/repo/b");
        cache.pending.insert(a.clone());
        cache.pending.insert(b.clone());
        let mut results = HashMap::new();
        results.insert(
            a.clone(),
            AheadBehindEntryWire {
                ahead: Some(1),
                behind: Some(0),
                main_behind: None,
            },
        );
        cache
            .results_tx
            .send(FetchResult {
                requested: vec![a.clone()],
                results,
            })
            .unwrap();
        cache.changed().await;
        assert!(matches!(cache.get(&a), AheadBehindState::Known { .. }));
        assert_eq!(cache.get(&b), AheadBehindState::Loading);
    }

    #[test]
    fn invalidate_drops_a_cached_entry() {
        let mut cache = cache();
        cache.entries.insert(
            PathBuf::from("/repo/wt"),
            AheadBehindState::Known {
                ahead: 1,
                behind: 0,
                main_behind: None,
            },
        );
        cache.invalidate(Path::new("/repo/wt"));
        assert_eq!(cache.get(Path::new("/repo/wt")), AheadBehindState::Unknown);
    }
}
