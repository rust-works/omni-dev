//! Bounded session pools for concurrent, per-query-context Snowflake access.
//!
//! On the v1 query endpoint, statement context (`warehouse`/`role`/`database`/
//! `schema`) is **session-global** — changed via `USE` and shared by every query
//! on the session token. So a query that needs a specific context must hold its
//! session exclusively for the `USE … + query`. To get concurrency *and*
//! per-query context without re-authenticating per query, each `(account, user)`
//! keeps a **bounded pool** of up to `max` authenticated sessions (each a
//! separate browser auth): a query checks one out, applies any `USE` needed
//! (skipping `USE`s already in effect), runs, and returns it.
//!
//! - **Concurrency is capped at `max`** by a [`tokio::sync::Semaphore`]: a permit
//!   is held for the whole checkout, and a new session is created only while
//!   holding a permit with no idle session available — so the live-session count
//!   (and thus the number of browser auths) never exceeds `max`, and grows lazily
//!   with demand.
//! - **Every member is tracked in one slot table** (a [`std::sync::Mutex`] never
//!   held across an `.await`): an idle slot parks its session handle; a checked-out
//!   slot's handle lives in the [`Checkout`]. Both states stay visible to the
//!   (sync) menu/status snapshots, which is how each individual auth is listed.
//! - **Session creation (browser SSO) is serialized across all pools** by a
//!   shared auth gate (held by the engine's `create` closure) so only one auth
//!   window opens at a time.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, MutexGuard, Weak};

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::sync::{Mutex as TokioMutex, Notify, OwnedSemaphorePermit, Semaphore};

use crate::snowflake::client::{AbortHandle, SnowflakeSession};

/// Identifies a pool by its `(account, user)` — one authentication identity.
///
/// Account-agnostic: keys come from request/config values, never a hardcoded
/// account list.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    /// The Snowflake account identifier (normalized by the engine).
    pub account: String,
    /// The Snowflake user the sessions authenticate as.
    pub user: String,
}

impl SessionKey {
    /// Builds a key from an account and user.
    pub fn new(account: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            account: account.into(),
            user: user.into(),
        }
    }
}

/// A concrete statement context (resolved warehouse/role/database/schema names).
///
/// `None` means "not set / leave at the session's current value". A session's
/// *base* context (captured at creation) is fully concrete, so any per-query
/// override can always be reset back to it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct QueryContext {
    /// Warehouse name.
    pub warehouse: Option<String>,
    /// Role name.
    pub role: Option<String>,
    /// Database name.
    pub database: Option<String>,
    /// Schema name.
    pub schema: Option<String>,
}

impl QueryContext {
    /// Returns `self` with any `Some` field of `overrides` taking precedence.
    ///
    /// Used as `base.overlay(&overrides)` to compute the concrete context a query
    /// should run under.
    #[must_use]
    pub fn overlay(&self, overrides: &Self) -> Self {
        Self {
            warehouse: overrides
                .warehouse
                .clone()
                .or_else(|| self.warehouse.clone()),
            role: overrides.role.clone().or_else(|| self.role.clone()),
            database: overrides.database.clone().or_else(|| self.database.clone()),
            schema: overrides.schema.clone().or_else(|| self.schema.clone()),
        }
    }

    /// A compact `wh/role/db/schema` label for menus (`(default)` when empty).
    #[must_use]
    pub fn summary(&self) -> String {
        let parts: Vec<&str> = [
            self.warehouse.as_deref(),
            self.role.as_deref(),
            self.database.as_deref(),
            self.schema.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        if parts.is_empty() {
            "(default)".to_string()
        } else {
            parts.join("/")
        }
    }
}

/// The query a member is currently running (set while checked out).
#[derive(Clone, Debug, Serialize)]
pub struct RunningQuery {
    /// A single-line, truncated preview of the SQL.
    pub sql: String,
    /// When the query started running.
    pub started_at: DateTime<Utc>,
}

/// One pool member: an authenticated session and its tracked state. The session
/// handle is `Some` while idle and `None` while checked out (then it lives in the
/// [`Checkout`]); the slot itself stays so every auth remains visible.
struct Slot<S> {
    id: u64,
    base: QueryContext,
    current: QueryContext,
    last_used: DateTime<Utc>,
    query_count: u64,
    running: Option<RunningQuery>,
    /// A handle to cancel the running statement, captured at
    /// [`start_query`](SessionPool::start_query). Held here (not in the checked-out
    /// [`Checkout`]) so a concurrent cancel can reach the busy session; cleared on
    /// checkin.
    running_abort: Option<AbortHandle>,
    session: Option<S>,
}

/// A serializable snapshot of one pool member (one authenticated session).
#[derive(Clone, Debug, Serialize)]
pub struct MemberInfo {
    /// Stable per-pool member id.
    pub id: u64,
    /// Whether the member is currently running a query.
    pub busy: bool,
    /// The context currently applied to the member.
    pub context: QueryContext,
    /// When the member last finished a query.
    pub last_used: DateTime<Utc>,
    /// How many queries this member has run.
    pub query_count: u64,
    /// The query currently running, when busy.
    pub running: Option<RunningQuery>,
}

/// A session checked out of a pool.
///
/// Return it with [`SessionPool::checkin`] or drop it with
/// [`SessionPool::discard`]. If it is dropped without either (a panic or early
/// return between checkout and checkin), its [`Drop`] guard removes the orphaned
/// slot so the member doesn't linger as phantom-busy.
pub struct Checkout<S = SnowflakeSession> {
    _permit: OwnedSemaphorePermit,
    id: u64,
    base: QueryContext,
    current: QueryContext,
    /// `Some` until `checkin`/`discard` takes it; `None` afterwards.
    session: Option<S>,
    /// Back-reference to the pool's slot table for the `Drop` guard.
    slots: Weak<StdMutex<Vec<Slot<S>>>>,
    /// Set by `checkin`/`discard` so the `Drop` guard becomes a no-op.
    done: bool,
}

impl<S> Checkout<S> {
    /// The checked-out session.
    pub fn session(&self) -> &S {
        self.session
            .as_ref()
            .unwrap_or_else(|| unreachable!("session is present until checkin/discard"))
    }
    /// The session's base (creation-time) context.
    pub fn base(&self) -> &QueryContext {
        &self.base
    }
    /// The context currently applied to the session.
    pub fn current(&self) -> &QueryContext {
        &self.current
    }
    /// The session member id.
    pub fn id(&self) -> u64 {
        self.id
    }
}

impl<S> Drop for Checkout<S> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        // Orphaned (not checked in/discarded): remove the slot so it isn't left
        // permanently busy. The session handle drops with `self`.
        if let Some(slots) = self.slots.upgrade() {
            let mut slots = slots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            slots.retain(|slot| slot.id != self.id);
        }
    }
}

/// Mutable per-pool bookkeeping for the (sync) menu/status snapshot.
#[derive(Clone, Debug)]
struct PoolMeta {
    created_at: DateTime<Utc>,
    last_used: DateTime<Utc>,
    query_count: u64,
}

/// A serializable snapshot of one pool, used for `sessions`/`status`/menu.
#[derive(Clone, Debug, Serialize)]
pub struct SessionInfo {
    /// Stable per-process pool id (used in tray `disconnect:<id>` actions).
    pub id: u64,
    /// The Snowflake account identifier.
    pub account: String,
    /// The authenticated user.
    pub user: String,
    /// When the pool was created (first query).
    pub created_at: DateTime<Utc>,
    /// When the pool last served a query.
    pub last_used: DateTime<Utc>,
    /// How many queries the pool has served.
    pub query_count: u64,
    /// Live (authenticated) sessions in the pool.
    pub sessions: usize,
    /// Maximum sessions / concurrency for the pool.
    pub max_sessions: usize,
    /// One entry per live authenticated session.
    pub members: Vec<MemberInfo>,
}

/// A bounded pool of authenticated sessions for one `(account, user)`.
///
/// Generic over the session type for testability; the engine uses
/// `SessionPool<SnowflakeSession>`.
pub struct SessionPool<S = SnowflakeSession> {
    id: u64,
    key: SessionKey,
    max: usize,
    permits: Arc<Semaphore>,
    /// Shared across all pools so only one browser auth runs at a time.
    auth_gate: Arc<TokioMutex<()>>,
    slots: Arc<StdMutex<Vec<Slot<S>>>>,
    /// Notified whenever a session is returned to the idle set, so a waiter can
    /// grab it immediately rather than waiting out an in-flight auth.
    idle_notify: Notify,
    next_member_id: AtomicU64,
    meta: StdMutex<PoolMeta>,
}

impl<S> SessionPool<S> {
    /// Builds an empty pool with capacity `max` (clamped to ≥ 1), sharing the
    /// given auth gate.
    #[must_use]
    pub fn new(
        id: u64,
        key: SessionKey,
        max: usize,
        now: DateTime<Utc>,
        auth_gate: Arc<TokioMutex<()>>,
    ) -> Self {
        let max = max.max(1);
        Self {
            id,
            key,
            max,
            permits: Arc::new(Semaphore::new(max)),
            auth_gate,
            slots: Arc::new(StdMutex::new(Vec::new())),
            idle_notify: Notify::new(),
            next_member_id: AtomicU64::new(1),
            meta: StdMutex::new(PoolMeta {
                created_at: now,
                last_used: now,
                query_count: 0,
            }),
        }
    }

    /// Checks out a session: reuses the most-recently-returned idle one (LIFO,
    /// good temporal affinity), or creates a new one via `create` if none is idle
    /// and the pool is under capacity.
    ///
    /// `create` returns the session and its concrete base context. It is only
    /// invoked while holding a permit with no idle session available, so the live
    /// count never exceeds `max`.
    ///
    /// # Errors
    ///
    /// Propagates `create`'s error (e.g. authentication failure).
    pub async fn checkout<F, Fut, E>(&self, create: F) -> std::result::Result<Checkout<S>, E>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = std::result::Result<(S, QueryContext), E>>,
    {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .unwrap_or_else(|_| unreachable!("pool semaphore is never closed"));

        // Wait for whichever happens first: an idle session is returned, or it is
        // our turn at the auth gate. A session freed while another request is
        // mid-auth is grabbed immediately (the notify wins) instead of waiting out
        // that auth — so a waiter only authenticates when none becomes available.
        let gate_guard = loop {
            if let Some((id, base, current, session)) = self.take_idle() {
                return Ok(self.make_checkout(permit, id, base, current, session));
            }
            // Arm the idle notification, then re-check, so a checkin between the
            // check above and the wait below is never missed (lost-wakeup safe).
            let notified = self.idle_notify.notified();
            tokio::pin!(notified);
            let _ = notified.as_mut().enable();
            if let Some((id, base, current, session)) = self.take_idle() {
                return Ok(self.make_checkout(permit, id, base, current, session));
            }
            tokio::select! {
                biased;
                () = &mut notified => {}                          // a session freed → loop & retry
                guard = self.auth_gate.lock() => break guard,    // our turn to authenticate
            }
        };

        // We hold the auth gate. Re-check once more (a session may have freed
        // while we acquired it), then authenticate.
        let _gate = gate_guard;
        if let Some((id, base, current, session)) = self.take_idle() {
            return Ok(self.make_checkout(permit, id, base, current, session));
        }
        let (session, base) = create().await?;
        let id = self.next_member_id.fetch_add(1, Ordering::Relaxed);
        self.lock_slots().push(Slot {
            id,
            base: base.clone(),
            current: base.clone(),
            last_used: Utc::now(),
            query_count: 0,
            running: None,
            running_abort: None,
            session: None, // handle is held by the returned Checkout
        });
        Ok(self.make_checkout(permit, id, base.clone(), base, session))
    }

    /// Removes and returns an idle member (LIFO), or `None` if all are busy.
    fn take_idle(&self) -> Option<(u64, QueryContext, QueryContext, S)> {
        let mut slots = self.lock_slots();
        for slot in slots.iter_mut().rev() {
            if let Some(session) = slot.session.take() {
                return Some((slot.id, slot.base.clone(), slot.current.clone(), session));
            }
        }
        None
    }

    /// Wraps a session into a [`Checkout`] carrying the permit and a drop guard.
    fn make_checkout(
        &self,
        permit: OwnedSemaphorePermit,
        id: u64,
        base: QueryContext,
        current: QueryContext,
        session: S,
    ) -> Checkout<S> {
        Checkout {
            _permit: permit,
            id,
            base,
            current,
            session: Some(session),
            slots: Arc::downgrade(&self.slots),
            done: false,
        }
    }

    /// Checks out every currently-idle session without waiting or creating.
    ///
    /// Used by the keep-alive heartbeat: each borrowed session holds a real
    /// permit, so a concurrent [`checkout`](Self::checkout) briefly waits for
    /// [`restore`](Self::restore) instead of authenticating a duplicate session
    /// (a spurious browser SSO). Busy sessions are skipped — the query path
    /// keeps them alive itself.
    #[must_use]
    pub fn checkout_all_idle(&self) -> Vec<Checkout<S>> {
        let mut checkouts = Vec::new();
        while let Ok(permit) = Arc::clone(&self.permits).try_acquire_owned() {
            match self.take_idle() {
                Some((id, base, current, session)) => {
                    checkouts.push(self.make_checkout(permit, id, base, current, session));
                }
                // No idle session left; the unused permit drops (released).
                None => break,
            }
        }
        checkouts
    }

    /// Returns a borrowed session to its slot untouched — unlike
    /// [`checkin`](Self::checkin) it preserves `last_used` and the recorded
    /// context, because a keep-alive heartbeat is not a query.
    pub fn restore(&self, mut checkout: Checkout<S>) {
        checkout.done = true;
        let session = checkout.session.take();
        {
            let mut slots = self.lock_slots();
            if let Some(slot) = slots.iter_mut().find(|slot| slot.id == checkout.id) {
                slot.session = session;
            }
        }
        // Wake a waiter so it can reuse this session instead of authenticating.
        self.idle_notify.notify_waiters();
        // `_permit` drops with `checkout`, freeing a concurrency slot.
    }

    /// Returns a session to its slot with the context now applied to it.
    pub fn checkin(&self, mut checkout: Checkout<S>, current: QueryContext) {
        checkout.done = true;
        let session = checkout.session.take();
        {
            let mut slots = self.lock_slots();
            if let Some(slot) = slots.iter_mut().find(|slot| slot.id == checkout.id) {
                slot.current = current;
                slot.last_used = Utc::now();
                slot.running = None;
                slot.running_abort = None;
                slot.session = session;
            }
        }
        // Wake a waiter so it can reuse this session instead of authenticating.
        self.idle_notify.notify_waiters();
        // `_permit` drops with `checkout`, freeing a concurrency slot.
    }

    /// Records that a checked-out member has started running `sql` (a truncated
    /// preview), so menus/status can show what each busy session is doing, and
    /// stores an `abort` handle so a concurrent cancel can stop it. `abort` is
    /// `None` for members with no cancellation support (e.g. the test pool).
    pub fn start_query(&self, member_id: u64, sql: String, abort: Option<AbortHandle>) {
        let mut slots = self.lock_slots();
        if let Some(slot) = slots.iter_mut().find(|slot| slot.id == member_id) {
            slot.query_count += 1;
            slot.running = Some(RunningQuery {
                sql,
                started_at: Utc::now(),
            });
            slot.running_abort = abort;
        }
    }

    /// Cloned abort handles for the pool's currently-running statements — the one
    /// member when `member` is `Some`, else every busy member with a handle.
    ///
    /// Snapshotted under the slot lock and returned by value so the caller can
    /// `await` each abort **without** holding the (non-async) lock.
    #[must_use]
    pub fn abort_handles(&self, member: Option<u64>) -> Vec<AbortHandle> {
        let slots = self.lock_slots();
        slots
            .iter()
            .filter(|slot| match member {
                Some(id) => slot.id == id,
                None => true,
            })
            .filter_map(|slot| slot.running_abort.clone())
            .collect()
    }

    /// Discards a session — e.g. after expiry — removing its slot (releases the
    /// permit and frees its capacity for a fresh auth).
    pub fn discard(&self, mut checkout: Checkout<S>) {
        checkout.done = true;
        self.lock_slots().retain(|slot| slot.id != checkout.id);
        drop(checkout);
    }

    /// Records that the pool served a query.
    pub fn touch(&self) {
        let mut meta = self.lock_meta();
        meta.last_used = Utc::now();
        meta.query_count += 1;
    }

    /// The pool id.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The number of live sessions (idle + checked out).
    #[must_use]
    pub fn live(&self) -> usize {
        self.lock_slots().len()
    }

    /// A serializable snapshot of the pool, including each member.
    #[must_use]
    pub fn info(&self) -> SessionInfo {
        let members: Vec<MemberInfo> = {
            let slots = self.lock_slots();
            let mut members: Vec<MemberInfo> = slots
                .iter()
                .map(|slot| MemberInfo {
                    id: slot.id,
                    busy: slot.session.is_none(),
                    context: slot.current.clone(),
                    last_used: slot.last_used,
                    query_count: slot.query_count,
                    running: slot.running.clone(),
                })
                .collect();
            members.sort_by_key(|m| m.id);
            members
        };
        let meta = self.lock_meta();
        SessionInfo {
            id: self.id,
            account: self.key.account.clone(),
            user: self.key.user.clone(),
            created_at: meta.created_at,
            last_used: meta.last_used,
            query_count: meta.query_count,
            sessions: members.len(),
            max_sessions: self.max,
            members,
        }
    }

    fn lock_slots(&self) -> MutexGuard<'_, Vec<Slot<S>>> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_meta(&self) -> MutexGuard<'_, PoolMeta> {
        self.meta
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The account-agnostic registry of session pools, keyed by `(account, user)`.
///
/// Cheap to clone (everything is behind `Arc`). The map mutex is std (never held
/// across `.await`); the shared auth gate serializes browser auths across pools.
#[derive(Clone)]
pub struct PoolRegistry {
    map: Arc<StdMutex<HashMap<SessionKey, Arc<SessionPool>>>>,
    auth_gate: Arc<TokioMutex<()>>,
    next_pool_id: Arc<AtomicU64>,
}

impl Default for PoolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl PoolRegistry {
    /// Builds an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            map: Arc::new(StdMutex::new(HashMap::new())),
            auth_gate: Arc::new(TokioMutex::new(())),
            next_pool_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Returns the pool for `key`, creating an empty one (no auth) if absent.
    /// New pools share the registry's auth gate, so all browser auths across all
    /// pools are serialized to one at a time.
    pub fn get_or_create(&self, key: &SessionKey, max: usize) -> Arc<SessionPool> {
        let mut map = self.lock();
        if let Some(pool) = map.get(key) {
            return Arc::clone(pool);
        }
        let id = self.next_pool_id.fetch_add(1, Ordering::Relaxed);
        let pool = Arc::new(SessionPool::new(
            id,
            key.clone(),
            max,
            Utc::now(),
            Arc::clone(&self.auth_gate),
        ));
        map.insert(key.clone(), Arc::clone(&pool));
        pool
    }

    /// Returns the pool for `key` **without** creating one (unlike
    /// [`get_or_create`](Self::get_or_create)). Used by read-only paths like
    /// `cancel` that must not spin up an empty pool.
    #[must_use]
    pub fn get(&self, key: &SessionKey) -> Option<Arc<SessionPool>> {
        self.lock().get(key).map(Arc::clone)
    }

    /// Returns the pool with the given id, if present, without creating one.
    #[must_use]
    pub fn get_by_id(&self, id: u64) -> Option<Arc<SessionPool>> {
        self.lock()
            .values()
            .find(|pool| pool.id() == id)
            .map(Arc::clone)
    }

    /// Removes the pool for `key`. Returns it if present.
    pub fn remove(&self, key: &SessionKey) -> Option<Arc<SessionPool>> {
        self.lock().remove(key)
    }

    /// Removes the pool with the given id. Returns it if present.
    pub fn remove_by_id(&self, id: u64) -> Option<Arc<SessionPool>> {
        let key = {
            let map = self.lock();
            map.iter()
                .find(|(_, pool)| pool.id() == id)
                .map(|(key, _)| key.clone())
        };
        key.and_then(|key| self.remove(&key))
    }

    /// Drains and returns every pool.
    pub fn take_all(&self) -> Vec<Arc<SessionPool>> {
        self.lock().drain().map(|(_, pool)| pool).collect()
    }

    /// Live handles to every pool, ordered by id, without draining the
    /// registry. Used by the keep-alive heartbeat to visit each pool.
    #[must_use]
    pub fn pools(&self) -> Vec<Arc<SessionPool>> {
        let mut pools: Vec<Arc<SessionPool>> = self.lock().values().map(Arc::clone).collect();
        pools.sort_by_key(|pool| pool.id());
        pools
    }

    /// A snapshot of every pool, ordered by id. Sync; no awaits.
    #[must_use]
    pub fn snapshot(&self) -> Vec<SessionInfo> {
        let mut infos: Vec<SessionInfo> = self.lock().values().map(|pool| pool.info()).collect();
        infos.sort_by_key(|info| info.id);
        infos
    }

    /// The number of pools.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the registry holds no pools.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SessionKey, Arc<SessionPool>>> {
        self.map
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use super::*;

    fn ctx() -> QueryContext {
        QueryContext::default()
    }

    fn fake_pool(max: usize) -> (SessionPool<u32>, Arc<AtomicU32>) {
        let pool = SessionPool::<u32>::new(
            1,
            SessionKey::new("ACCT", "user"),
            max,
            Utc::now(),
            Arc::new(TokioMutex::new(())),
        );
        (pool, Arc::new(AtomicU32::new(0)))
    }

    async fn fake_create(
        counter: &AtomicU32,
    ) -> std::result::Result<(u32, QueryContext), std::convert::Infallible> {
        Ok((counter.fetch_add(1, Ordering::Relaxed), ctx()))
    }

    #[tokio::test]
    async fn start_query_records_running_then_checkin_clears_it() {
        let (pool, calls) = fake_pool(2);
        let c = pool.checkout(|| fake_create(&calls)).await.unwrap();
        pool.start_query(
            c.id(),
            "SELECT 1".to_string(),
            Some(AbortHandle::noop_for_test()),
        );

        let member = pool.info().members[0].clone();
        assert!(member.busy);
        assert_eq!(member.query_count, 1);
        assert_eq!(
            member.running.as_ref().map(|r| r.sql.as_str()),
            Some("SELECT 1")
        );
        // The abort handle is reachable while running…
        assert_eq!(pool.abort_handles(None).len(), 1);
        assert_eq!(pool.abort_handles(Some(c.id())).len(), 1);
        assert!(pool.abort_handles(Some(c.id() + 99)).is_empty());

        pool.checkin(c, ctx());
        let member = pool.info().members[0].clone();
        assert!(!member.busy);
        assert!(member.running.is_none(), "running cleared on checkin");
        assert_eq!(member.query_count, 1, "count persists after checkin");
        // …and gone once idle again.
        assert!(
            pool.abort_handles(None).is_empty(),
            "abort handle cleared on checkin"
        );
    }

    #[test]
    fn overlay_lets_overrides_win() {
        let base = QueryContext {
            warehouse: Some("WH".into()),
            role: Some("R".into()),
            database: Some("DB".into()),
            schema: Some("S".into()),
        };
        let overrides = QueryContext {
            warehouse: Some("OTHER_WH".into()),
            ..QueryContext::default()
        };
        let eff = base.overlay(&overrides);
        assert_eq!(eff.warehouse.as_deref(), Some("OTHER_WH"));
        assert_eq!(eff.role.as_deref(), Some("R")); // unchanged falls back to base
        assert_eq!(eff.database.as_deref(), Some("DB"));
    }

    #[test]
    fn summary_renders_set_dimensions_or_default() {
        assert_eq!(QueryContext::default().summary(), "(default)");
        let c = QueryContext {
            warehouse: Some("WH".into()),
            role: Some("R".into()),
            ..QueryContext::default()
        };
        assert_eq!(c.summary(), "WH/R");
    }

    #[tokio::test]
    async fn checkin_reuses_session_and_lists_members() {
        let (pool, calls) = fake_pool(4);
        let c1 = pool.checkout(|| fake_create(&calls)).await.unwrap();
        let id1 = c1.id();
        // While checked out the member is visible and marked busy.
        let info = pool.info();
        assert_eq!(info.members.len(), 1);
        assert!(info.members[0].busy);
        pool.checkin(c1, ctx());
        // Now idle.
        assert!(!pool.info().members[0].busy);
        // Reuse the idle session: no new create, same member id.
        let c2 = pool.checkout(|| fake_create(&calls)).await.unwrap();
        assert_eq!(c2.id(), id1);
        assert_eq!(calls.load(Ordering::Relaxed), 1, "should not create twice");
        assert_eq!(pool.live(), 1);
        pool.checkin(c2, ctx());
    }

    #[tokio::test]
    async fn never_exceeds_capacity_and_blocks_until_checkin() {
        let (pool, calls) = fake_pool(2);
        let c1 = pool.checkout(|| fake_create(&calls)).await.unwrap();
        let c2 = pool.checkout(|| fake_create(&calls)).await.unwrap();
        assert_eq!(pool.live(), 2);
        // Both members visible as busy.
        assert_eq!(pool.info().members.iter().filter(|m| m.busy).count(), 2);
        // A third checkout must block while both are held.
        let third = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            pool.checkout(|| fake_create(&calls)),
        )
        .await;
        assert!(third.is_err(), "third checkout should block at capacity");
        pool.checkin(c1, ctx());
        let c3 = pool.checkout(|| fake_create(&calls)).await.unwrap();
        assert_eq!(pool.live(), 2, "reuse, not grow");
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        pool.checkin(c2, ctx());
        pool.checkin(c3, ctx());
    }

    #[tokio::test]
    async fn discard_frees_capacity_for_a_fresh_session() {
        let (pool, calls) = fake_pool(1);
        let c1 = pool.checkout(|| fake_create(&calls)).await.unwrap();
        assert_eq!(pool.live(), 1);
        pool.discard(c1); // expired session dropped
        assert_eq!(pool.live(), 0);
        assert!(pool.info().members.is_empty());
        let c2 = pool.checkout(|| fake_create(&calls)).await.unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2, "fresh session created");
        assert_eq!(pool.live(), 1);
        pool.checkin(c2, ctx());
    }

    #[tokio::test]
    async fn orphaned_checkout_frees_its_slot_on_drop() {
        let (pool, calls) = fake_pool(2);
        {
            let _c = pool.checkout(|| fake_create(&calls)).await.unwrap();
            assert_eq!(pool.live(), 1);
            // `_c` is dropped here without checkin/discard (e.g. a panic path).
        }
        assert_eq!(pool.live(), 0, "the Drop guard removed the orphaned slot");
        assert!(pool.info().members.is_empty());
        // Capacity is freed, so a fresh checkout still works.
        let c = pool.checkout(|| fake_create(&calls)).await.unwrap();
        pool.checkin(c, ctx());
    }

    #[tokio::test]
    async fn waiter_grabs_freed_session_without_waiting_out_the_in_flight_auth() {
        let (pool, calls) = fake_pool(2);
        let pool = Arc::new(pool);

        // One live session, checked out — so there's nothing idle initially.
        let c1 = pool.checkout(|| fake_create(&calls)).await.unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // Hold the auth gate for the WHOLE test, simulating an in-flight auth that
        // never completes.
        let held = Arc::clone(&pool.auth_gate).lock_owned().await;

        // A second request: acquires a permit, finds no idle, then parks in the
        // notify-vs-gate race.
        let pool2 = Arc::clone(&pool);
        let calls2 = Arc::clone(&calls);
        let waiter = tokio::spawn(async move {
            pool2
                .checkout(move || async move {
                    let id = calls2.fetch_add(1, Ordering::Relaxed);
                    Ok::<(u32, QueryContext), std::convert::Infallible>((
                        id,
                        QueryContext::default(),
                    ))
                })
                .await
                .unwrap()
        });

        // Let the waiter park, then free c1's session WITHOUT releasing the gate.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        pool.checkin(c1, ctx());

        // The waiter must complete by reusing the freed session even though the
        // auth gate is still held — proving it didn't wait out the in-flight auth.
        let c2 = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("waiter must not block on the held auth gate")
            .unwrap();
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "reused the freed session — no new auth"
        );
        assert_eq!(pool.live(), 1, "still one session");

        drop(held);
        pool.checkin(c2, ctx());
    }

    #[tokio::test]
    async fn checkout_all_idle_borrows_only_idle_sessions() {
        let (pool, calls) = fake_pool(4);
        let c1 = pool.checkout(|| fake_create(&calls)).await.unwrap();
        let c2 = pool.checkout(|| fake_create(&calls)).await.unwrap();
        let busy_id = c2.id();
        pool.checkin(c1, ctx());

        // One idle, one busy: only the idle session is borrowed.
        let borrowed = pool.checkout_all_idle();
        assert_eq!(borrowed.len(), 1);
        assert_ne!(borrowed[0].id(), busy_id);
        // Both slots stay visible; the borrowed one shows as busy.
        assert_eq!(pool.live(), 2);
        assert_eq!(pool.info().members.iter().filter(|m| m.busy).count(), 2);

        for c in borrowed {
            pool.restore(c);
        }
        pool.checkin(c2, ctx());
        // Nothing was created or lost by the borrow cycle.
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(pool.live(), 2);
    }

    #[tokio::test]
    async fn checkout_all_idle_is_empty_when_nothing_is_idle() {
        let (pool, calls) = fake_pool(2);
        assert!(pool.checkout_all_idle().is_empty(), "empty pool");
        let c = pool.checkout(|| fake_create(&calls)).await.unwrap();
        assert!(pool.checkout_all_idle().is_empty(), "all busy");
        pool.checkin(c, ctx());
    }

    #[tokio::test]
    async fn restore_preserves_last_used_and_context() {
        let (pool, calls) = fake_pool(2);
        let c = pool.checkout(|| fake_create(&calls)).await.unwrap();
        let recorded = QueryContext {
            warehouse: Some("WH".into()),
            ..QueryContext::default()
        };
        pool.checkin(c, recorded.clone());
        let before = pool.info().members[0].clone();

        // Ensure a bookkeeping bump would produce a strictly later timestamp.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let borrowed = pool.checkout_all_idle();
        assert_eq!(borrowed.len(), 1);
        for c in borrowed {
            pool.restore(c);
        }

        let after = pool.info().members[0].clone();
        assert_eq!(after.last_used, before.last_used, "not a query");
        assert_eq!(after.context, recorded, "recorded context untouched");
        assert_eq!(after.query_count, before.query_count);
    }

    #[tokio::test]
    async fn discarding_a_borrowed_idle_session_frees_capacity() {
        let (pool, calls) = fake_pool(1);
        let c = pool.checkout(|| fake_create(&calls)).await.unwrap();
        pool.checkin(c, ctx());

        let mut borrowed = pool.checkout_all_idle();
        pool.discard(borrowed.pop().unwrap()); // dead beyond renewal
        assert_eq!(pool.live(), 0);

        // Capacity is freed, so the next checkout authenticates afresh.
        let c = pool.checkout(|| fake_create(&calls)).await.unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        pool.checkin(c, ctx());
    }

    #[tokio::test]
    async fn checkout_during_heartbeat_borrow_waits_and_reuses() {
        let (pool, calls) = fake_pool(1);
        let pool = Arc::new(pool);
        let c = pool.checkout(|| fake_create(&calls)).await.unwrap();
        let id = c.id();
        pool.checkin(c, ctx());

        // The heartbeat borrows the only session (and its only permit).
        let mut borrowed = pool.checkout_all_idle();
        assert_eq!(borrowed.len(), 1);

        // A query arriving mid-heartbeat parks on the permit instead of
        // authenticating a duplicate session (no spurious SSO).
        let pool2 = Arc::clone(&pool);
        let calls2 = Arc::clone(&calls);
        let waiter = tokio::spawn(async move {
            pool2
                .checkout(move || async move {
                    let id = calls2.fetch_add(1, Ordering::Relaxed);
                    Ok::<(u32, QueryContext), std::convert::Infallible>((
                        id,
                        QueryContext::default(),
                    ))
                })
                .await
                .unwrap()
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "waiter blocks while borrowed");

        pool.restore(borrowed.pop().unwrap());
        let c = tokio::time::timeout(std::time::Duration::from_secs(2), waiter)
            .await
            .expect("waiter must proceed once the session is restored")
            .unwrap();
        assert_eq!(c.id(), id, "reused the restored session");
        assert_eq!(calls.load(Ordering::Relaxed), 1, "no new auth");
        pool.checkin(c, ctx());
    }

    #[test]
    fn registry_pools_returns_handles_without_draining() {
        let registry = PoolRegistry::new();
        assert!(registry.pools().is_empty());
        let p1 = registry.get_or_create(&SessionKey::new("A", "u"), 4);
        let p2 = registry.get_or_create(&SessionKey::new("B", "u"), 4);
        let pools = registry.pools();
        assert_eq!(
            pools.iter().map(|p| p.id()).collect::<Vec<_>>(),
            vec![p1.id(), p2.id()],
            "ordered by id"
        );
        assert_eq!(registry.len(), 2, "not drained");
    }

    #[test]
    fn registry_get_and_get_by_id_never_create() {
        let registry = PoolRegistry::new();
        let key = SessionKey::new("ACCT", "user");
        // Read-only lookups on an absent key/id create nothing.
        assert!(registry.get(&key).is_none());
        assert!(registry.get_by_id(1).is_none());
        assert!(registry.is_empty());

        let pool = registry.get_or_create(&key, 4);
        assert_eq!(registry.get(&key).map(|p| p.id()), Some(pool.id()));
        assert_eq!(
            registry.get_by_id(pool.id()).map(|p| p.id()),
            Some(pool.id())
        );
        assert!(registry.get_by_id(pool.id() + 99).is_none());
    }

    #[test]
    fn registry_get_or_create_is_idempotent_per_key() {
        let registry = PoolRegistry::new();
        assert!(registry.is_empty());
        let key = SessionKey::new("ACCT", "user");
        let p1 = registry.get_or_create(&key, 4);
        let p2 = registry.get_or_create(&key, 4);
        assert_eq!(p1.id(), p2.id());
        assert_eq!(registry.len(), 1);
        assert!(registry.remove(&key).is_some());
        assert!(registry.remove(&key).is_none());
    }
}
