//! In-memory, TTL-bounded cache for near-static JIRA catalogue API responses.
//!
//! Wraps [`AtlassianClient::get_link_types`], [`get_fields`], [`get_projects`],
//! and [`get_boards`] so that repeated MCP calls within a single server process
//! do not re-fetch catalogue data that admins change rarely. See ADR-0024.
//!
//! Single-flight is achieved without an extra mutex: a stale-or-empty read
//! upgrades to a write lock and double-checks before fetching, so concurrent
//! waiters serialise on the lock and only the first fetches.
//!
//! Parameterised client methods (`get_projects(limit)`,
//! `get_boards(project, board_type, limit)`) are cached by **fetching the full
//! unfiltered result** and applying limits/filters at the caller; this keeps
//! the cache hit rate independent of caller arguments.
//!
//! Per-issue editmeta entries are cached with a shorter TTL than the
//! catalogue slots because edit screens can change with workflow/project
//! configuration; the cache is keyed by `(instance_url, issue_key)` and
//! holds the write lock during fetch for single-flight semantics.
//!
//! [`get_fields`]: AtlassianClient::get_fields
//! [`get_projects`]: AtlassianClient::get_projects
//! [`get_boards`]: AtlassianClient::get_boards

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::RwLock;

use crate::atlassian::client::AtlassianClient;
use crate::atlassian::jira_types::{
    AgileBoardList, EditMeta, JiraField, JiraLinkType, JiraProjectList,
};

/// Default cache lifetime. Catalogue data changes rarely (admin-only).
pub const DEFAULT_TTL: Duration = Duration::from_secs(3600);

/// Cache lifetime for per-issue editmeta.
///
/// Shorter than [`DEFAULT_TTL`] because edit screens can change with
/// workflow/screen configuration, but long enough to dedupe bursts of
/// writes against the same issue.
pub const EDITMETA_TTL: Duration = Duration::from_secs(60);

/// One cached catalogue result, tagged with the JIRA instance it came from.
struct CacheEntry<T> {
    instance_url: String,
    fetched_at: Instant,
    value: Arc<T>,
}

/// Shared cache for the four near-static JIRA catalogues plus per-issue
/// editmeta.
///
/// Cloning is cheap (wrap in `Arc<CatalogueCache>` at the owner). Each catalogue
/// has its own `RwLock`, so contention on one does not block the others.
pub struct CatalogueCache {
    link_types: RwLock<Option<CacheEntry<Vec<JiraLinkType>>>>,
    fields: RwLock<Option<CacheEntry<Vec<JiraField>>>>,
    projects: RwLock<Option<CacheEntry<JiraProjectList>>>,
    boards: RwLock<Option<CacheEntry<AgileBoardList>>>,
    editmeta: RwLock<HashMap<String, CacheEntry<EditMeta>>>,
    ttl: Duration,
    editmeta_ttl: Duration,
}

impl CatalogueCache {
    /// Constructs a cache with the given entry lifetime for catalogue slots.
    ///
    /// Per-issue editmeta entries use the separate [`EDITMETA_TTL`] constant.
    #[must_use]
    pub fn new(ttl: Duration) -> Self {
        Self::with_ttls(ttl, EDITMETA_TTL)
    }

    /// Constructs a cache with explicit catalogue and editmeta lifetimes.
    /// Exposed primarily so tests can shorten the editmeta TTL.
    #[must_use]
    pub fn with_ttls(ttl: Duration, editmeta_ttl: Duration) -> Self {
        Self {
            link_types: RwLock::new(None),
            fields: RwLock::new(None),
            projects: RwLock::new(None),
            boards: RwLock::new(None),
            editmeta: RwLock::new(HashMap::new()),
            ttl,
            editmeta_ttl,
        }
    }

    /// Returns the cached link-type catalogue, fetching it on miss/expiry.
    pub async fn link_types(&self, client: &AtlassianClient) -> Result<Arc<Vec<JiraLinkType>>> {
        get_or_fetch(
            &self.link_types,
            client.instance_url(),
            self.ttl,
            || async { client.get_link_types().await },
        )
        .await
    }

    /// Returns the cached field catalogue, fetching it on miss/expiry.
    pub async fn fields(&self, client: &AtlassianClient) -> Result<Arc<Vec<JiraField>>> {
        get_or_fetch(&self.fields, client.instance_url(), self.ttl, || async {
            client.get_fields().await
        })
        .await
    }

    /// Returns the cached project list (unbounded), fetching it on miss/expiry.
    ///
    /// Always fetches with `limit=0` (unlimited); callers slice/filter the
    /// returned list.
    pub async fn projects(&self, client: &AtlassianClient) -> Result<Arc<JiraProjectList>> {
        get_or_fetch(&self.projects, client.instance_url(), self.ttl, || async {
            client.get_projects(0).await
        })
        .await
    }

    /// Returns the cached board list (unfiltered, unbounded), fetching on
    /// miss/expiry.
    ///
    /// Always fetches with `project=None`, `board_type=None`, `limit=0`;
    /// callers apply filters/slicing.
    pub async fn boards(&self, client: &AtlassianClient) -> Result<Arc<AgileBoardList>> {
        get_or_fetch(&self.boards, client.instance_url(), self.ttl, || async {
            client.get_boards(None, None, 0).await
        })
        .await
    }

    /// Returns the cached editmeta for an issue, fetching it on miss/expiry.
    ///
    /// Keyed by `(instance_url, issue_key)`. The write lock on the whole map
    /// is held during fetch — misses are infrequent and the lock contention
    /// is bounded to a single in-flight fetch per cache.
    pub async fn editmeta(&self, client: &AtlassianClient, key: &str) -> Result<Arc<EditMeta>> {
        let instance_url = client.instance_url();

        {
            let guard = self.editmeta.read().await;
            if let Some(entry) = guard.get(key) {
                if entry.instance_url == instance_url
                    && entry.fetched_at.elapsed() < self.editmeta_ttl
                {
                    return Ok(Arc::clone(&entry.value));
                }
            }
        }

        let mut guard = self.editmeta.write().await;
        if let Some(entry) = guard.get(key) {
            if entry.instance_url == instance_url && entry.fetched_at.elapsed() < self.editmeta_ttl
            {
                return Ok(Arc::clone(&entry.value));
            }
        }

        let value = Arc::new(client.get_editmeta(key).await?);
        guard.insert(
            key.to_string(),
            CacheEntry {
                instance_url: instance_url.to_string(),
                fetched_at: Instant::now(),
                value: Arc::clone(&value),
            },
        );
        Ok(value)
    }
}

impl Default for CatalogueCache {
    fn default() -> Self {
        Self::new(DEFAULT_TTL)
    }
}

/// Read-fast-path / write-on-miss lookup for a single cache slot.
///
/// On miss or expiry, the write lock is held for the duration of the fetch so
/// that concurrent waiters see the populated entry on re-check rather than
/// each issuing their own request. Errors do not poison the cache: the slot
/// is left untouched and the error propagates.
async fn get_or_fetch<T, F, Fut>(
    slot: &RwLock<Option<CacheEntry<T>>>,
    instance_url: &str,
    ttl: Duration,
    fetch: F,
) -> Result<Arc<T>>
where
    T: Send + Sync,
    F: FnOnce() -> Fut + Send,
    Fut: Future<Output = Result<T>> + Send,
{
    if let Some(value) = read_fresh(slot, instance_url, ttl).await {
        return Ok(value);
    }

    let mut guard = slot.write().await;
    if let Some(entry) = guard.as_ref() {
        if entry.instance_url == instance_url && entry.fetched_at.elapsed() < ttl {
            return Ok(Arc::clone(&entry.value));
        }
    }

    let value = Arc::new(fetch().await?);
    *guard = Some(CacheEntry {
        instance_url: instance_url.to_string(),
        fetched_at: Instant::now(),
        value: Arc::clone(&value),
    });
    Ok(value)
}

async fn read_fresh<T>(
    slot: &RwLock<Option<CacheEntry<T>>>,
    instance_url: &str,
    ttl: Duration,
) -> Option<Arc<T>>
where
    T: Send + Sync,
{
    let guard = slot.read().await;
    let entry = guard.as_ref()?;
    if entry.instance_url == instance_url && entry.fetched_at.elapsed() < ttl {
        Some(Arc::clone(&entry.value))
    } else {
        None
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "u@t.com", "tok").unwrap()
    }

    fn link_types_body() -> serde_json::Value {
        serde_json::json!({
            "issueLinkTypes": [
                {"id": "1", "name": "Blocks", "inward": "is blocked by", "outward": "blocks"}
            ]
        })
    }

    fn fields_body() -> serde_json::Value {
        serde_json::json!([
            {"id": "summary", "name": "Summary", "custom": false}
        ])
    }

    fn projects_body() -> serde_json::Value {
        serde_json::json!({
            "values": [{"id": "10001", "key": "PROJ", "name": "Project"}],
            "total": 1,
            "isLast": true
        })
    }

    fn boards_body() -> serde_json::Value {
        serde_json::json!({
            "values": [{"id": 1, "name": "B", "type": "scrum"}],
            "isLast": true
        })
    }

    #[tokio::test]
    async fn link_types_cached_after_first_call() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issueLinkType"))
            .respond_with(ResponseTemplate::new(200).set_body_json(link_types_body()))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(Duration::from_secs(60));

        let a = cache.link_types(&client).await.unwrap();
        let b = cache.link_types(&client).await.unwrap();

        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        assert_eq!(a[0].name, "Blocks");
    }

    #[tokio::test]
    async fn fields_cached_after_first_call() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/field"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fields_body()))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(Duration::from_secs(60));

        let _ = cache.fields(&client).await.unwrap();
        let second = cache.fields(&client).await.unwrap();
        assert_eq!(second[0].name, "Summary");
    }

    #[tokio::test]
    async fn projects_cached_after_first_call() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/project/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(projects_body()))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(Duration::from_secs(60));

        let _ = cache.projects(&client).await.unwrap();
        let second = cache.projects(&client).await.unwrap();
        assert_eq!(second.projects[0].key, "PROJ");
    }

    #[tokio::test]
    async fn boards_cached_after_first_call() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/agile/1.0/board"))
            .respond_with(ResponseTemplate::new(200).set_body_json(boards_body()))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(Duration::from_secs(60));

        let _ = cache.boards(&client).await.unwrap();
        let second = cache.boards(&client).await.unwrap();
        assert_eq!(second.boards[0].name, "B");
    }

    #[tokio::test]
    async fn cache_refetches_after_ttl_expiry() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issueLinkType"))
            .respond_with(ResponseTemplate::new(200).set_body_json(link_types_body()))
            .expect(2)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(Duration::from_millis(20));

        let _ = cache.link_types(&client).await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        let _ = cache.link_types(&client).await.unwrap();
    }

    #[tokio::test]
    async fn cache_refetches_when_instance_url_changes() {
        let server_a = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issueLinkType"))
            .respond_with(ResponseTemplate::new(200).set_body_json(link_types_body()))
            .expect(1)
            .mount(&server_a)
            .await;
        let server_b = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issueLinkType"))
            .respond_with(ResponseTemplate::new(200).set_body_json(link_types_body()))
            .expect(1)
            .mount(&server_b)
            .await;

        let cache = CatalogueCache::new(Duration::from_secs(60));
        let client_a = mock_client(&server_a.uri());
        let client_b = mock_client(&server_b.uri());

        let _ = cache.link_types(&client_a).await.unwrap();
        let _ = cache.link_types(&client_b).await.unwrap();
        // Re-call against A: cache now holds B's entry, so a refetch from A is
        // expected. Server A's `.expect(1)` will fail if we re-hit it here,
        // which is the correct semantics — cache holds at most one entry.
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_refresh_shares_a_single_fetch() {
        // Exercise the write-lock double-check (the inner
        // `Arc::clone(&entry.value)` return) under a deterministically
        // engineered race.
        //
        // Strategy:
        //
        //  1. Hold an external write lock on the slot, pre-populated with
        //     a *stale* entry. While held, every task that calls
        //     `read_fresh` blocks on the slot's read lock.
        //  2. Spawn two tasks calling `cache.link_types(&client)`. They
        //     both park inside `read_fresh` waiting for the read lock.
        //  3. Drop the external write lock. Both tasks wake, acquire read
        //     locks concurrently (multiple readers permitted), observe
        //     `Some(stale)` and short-circuit to `None`, drop their read
        //     locks, and queue for the write lock.
        //  4. The first to acquire the write lock fetches (with a
        //     server-side delay so contention is unambiguous), populates
        //     the slot fresh, and releases.
        //  5. The second wakes, finds `Some(fresh)`, and returns via the
        //     double-check — covering line 139.
        //
        // A naive "spawn against a cold slot" race does not exercise the
        // double-check because the first task's write lock blocks all
        // subsequent `read_fresh` reads, so later tasks observe the
        // populated slot via `read_fresh`'s fast path instead of through
        // the write-lock-then-double-check path.
        //
        // Multi-thread runtime is required so the two readers run on
        // separate workers and the read-lock acquisitions truly overlap.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issueLinkType"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(link_types_body())
                    .set_delay(Duration::from_millis(100)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let cache = Arc::new(CatalogueCache::new(Duration::from_secs(60)));
        let url = server.uri();

        let mut gate = cache.link_types.write().await;
        *gate = Some(CacheEntry {
            instance_url: url.clone(),
            fetched_at: Instant::now()
                .checked_sub(Duration::from_secs(3600 * 24))
                .unwrap(),
            value: Arc::new(Vec::new()),
        });

        let mut handles = Vec::new();
        for _ in 0..2 {
            let cache = Arc::clone(&cache);
            let url = url.clone();
            handles.push(tokio::spawn(async move {
                let client = mock_client(&url);
                cache.link_types(&client).await.unwrap()
            }));
        }

        // Both tasks now block on `slot.read().await` inside `read_fresh`
        // because we hold the write gate.
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(gate);

        for h in handles {
            let v = h.await.unwrap();
            assert_eq!(v.len(), 1);
            assert_eq!(v[0].name, "Blocks");
        }
    }

    #[tokio::test]
    async fn cache_does_not_populate_on_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issueLinkType"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .expect(2)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(Duration::from_secs(60));

        assert!(cache.link_types(&client).await.is_err());
        assert!(cache.link_types(&client).await.is_err());
    }

    // ── editmeta cache ────────────────────────────────────────────────

    fn editmeta_body() -> serde_json::Value {
        serde_json::json!({
            "fields": {
                "customfield_19300": {
                    "name": "Acceptance Criteria",
                    "schema": {
                        "type": "string",
                        "custom": "com.atlassian.jira.plugin.system.customfieldtypes:textarea"
                    }
                }
            }
        })
    }

    #[tokio::test]
    async fn editmeta_cached_after_first_call() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1/editmeta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(editmeta_body()))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(Duration::from_secs(60));

        let first = cache.editmeta(&client, "PROJ-1").await.unwrap();
        let second = cache.editmeta(&client, "PROJ-1").await.unwrap();
        assert!(first.fields.contains_key("customfield_19300"));
        assert!(second.fields.contains_key("customfield_19300"));
    }

    #[tokio::test]
    async fn editmeta_separate_keys_fetch_independently() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1/editmeta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(editmeta_body()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-2/editmeta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(editmeta_body()))
            .expect(1)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(Duration::from_secs(60));

        let _ = cache.editmeta(&client, "PROJ-1").await.unwrap();
        let _ = cache.editmeta(&client, "PROJ-2").await.unwrap();
        let _ = cache.editmeta(&client, "PROJ-1").await.unwrap();
        let _ = cache.editmeta(&client, "PROJ-2").await.unwrap();
    }

    #[tokio::test]
    async fn editmeta_refetches_after_ttl_expiry() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1/editmeta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(editmeta_body()))
            .expect(2)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::with_ttls(Duration::from_secs(60), Duration::from_millis(20));

        let _ = cache.editmeta(&client, "PROJ-1").await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        let _ = cache.editmeta(&client, "PROJ-1").await.unwrap();
    }

    #[tokio::test]
    async fn editmeta_refetches_when_instance_url_changes() {
        let server_a = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1/editmeta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(editmeta_body()))
            .expect(1)
            .mount(&server_a)
            .await;
        let server_b = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1/editmeta"))
            .respond_with(ResponseTemplate::new(200).set_body_json(editmeta_body()))
            .expect(1)
            .mount(&server_b)
            .await;

        let cache = CatalogueCache::new(Duration::from_secs(60));
        let client_a = mock_client(&server_a.uri());
        let client_b = mock_client(&server_b.uri());

        let _ = cache.editmeta(&client_a, "PROJ-1").await.unwrap();
        let _ = cache.editmeta(&client_b, "PROJ-1").await.unwrap();
    }

    #[tokio::test]
    async fn editmeta_does_not_populate_on_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1/editmeta"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .expect(2)
            .mount(&server)
            .await;
        let client = mock_client(&server.uri());
        let cache = CatalogueCache::new(Duration::from_secs(60));

        assert!(cache.editmeta(&client, "PROJ-1").await.is_err());
        assert!(cache.editmeta(&client, "PROJ-1").await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn editmeta_concurrent_refresh_shares_a_single_fetch() {
        // Deterministic single-flight: pre-populate with a stale entry so
        // both tasks fall past the read-lock fast path; start task A first
        // and sleep long enough for it to hold the write lock during the
        // server-side fetch delay; then start task B which queues for the
        // write lock and — when A finishes and releases — acquires the
        // lock, hits the double-check inside the write critical section,
        // and returns the freshly populated entry. `.expect(1)` on the
        // mock enforces the contract: if the double-check fails and task
        // B re-fetches, wiremock will fail the test.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1/editmeta"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(editmeta_body())
                    .set_delay(Duration::from_millis(300)),
            )
            .expect(1)
            .mount(&server)
            .await;

        let cache = Arc::new(CatalogueCache::new(Duration::from_secs(60)));
        let url = server.uri();

        {
            let mut guard = cache.editmeta.write().await;
            guard.insert(
                "PROJ-1".to_string(),
                CacheEntry {
                    instance_url: url.clone(),
                    fetched_at: Instant::now()
                        .checked_sub(Duration::from_secs(3600 * 24))
                        .unwrap(),
                    value: Arc::new(EditMeta::default()),
                },
            );
        }

        let cache_a = Arc::clone(&cache);
        let url_a = url.clone();
        let task_a = tokio::spawn(async move {
            let client = mock_client(&url_a);
            cache_a.editmeta(&client, "PROJ-1").await.unwrap()
        });

        // Give task A enough time to acquire the write lock and begin the
        // 300ms fetch before task B starts. 100ms is well inside that
        // window and well above tokio's task-startup overhead.
        tokio::time::sleep(Duration::from_millis(100)).await;

        let cache_b = Arc::clone(&cache);
        let url_b = url.clone();
        let task_b = tokio::spawn(async move {
            let client = mock_client(&url_b);
            cache_b.editmeta(&client, "PROJ-1").await.unwrap()
        });

        let a = task_a.await.unwrap();
        let b = task_b.await.unwrap();
        assert!(a.fields.contains_key("customfield_19300"));
        assert!(b.fields.contains_key("customfield_19300"));
    }
}
