//! Shared test helpers for Drive unit tests.
//!
//! Any test that mutates `HOME` or a `DRIVE_*` environment variable must
//! acquire [`EnvGuard`] so parallel tests don't race on process-wide state.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::drive::account::DRIVE_ACCOUNT_ENV;
use crate::drive::auth::{
    DRIVE_API_URL, DRIVE_CLIENT_ID, DRIVE_CLIENT_SECRET, DRIVE_REFRESH_TOKEN, DRIVE_SCOPE,
};
use crate::drive::docs::client::DOCS_API_URL;
use crate::drive::sheets::client::SHEETS_API_URL;

/// Process-wide mutex serialising tests that mutate `HOME` and the Drive
/// credential environment variables.
///
/// Aliases the crate-wide [`crate::test_support::HOME_ENV_MUTEX`] so Drive's
/// `HOME` mutation also serialises against every other domain's (Gmail,
/// Atlassian, Datadog, …) — an independent `Mutex<()>` here provides no real
/// exclusion against them, which is exactly the race that surfaced between
/// Gmail and Datadog in issue #1465.
static DRIVE_ENV_MUTEX: &Mutex<()> = &crate::test_support::HOME_ENV_MUTEX;

/// RAII guard: snapshots `HOME` + every Drive credential env var on
/// construction and restores them on drop.
pub(crate) struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    snapshot: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    pub(crate) fn take() -> Self {
        let lock = DRIVE_ENV_MUTEX
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // The three `*_API_URL` host overrides are snapshotted for two
        // reasons: a developer with one exported must not silently redirect
        // a test's requests, and a test that points one at a local server
        // must not leak that setting into the next test. Their absence here
        // was already a latent hazard for `DRIVE_API_URL`; `SHEETS_API_URL`
        // and `DOCS_API_URL` make it sharper, since without an override
        // those clients default to the *real* `sheets.googleapis.com` /
        // `docs.googleapis.com`.
        let keys = [
            "HOME",
            DRIVE_CLIENT_ID,
            DRIVE_CLIENT_SECRET,
            DRIVE_REFRESH_TOKEN,
            DRIVE_SCOPE,
            DRIVE_ACCOUNT_ENV,
            DRIVE_API_URL,
            SHEETS_API_URL,
            DOCS_API_URL,
        ];
        let snapshot = keys
            .into_iter()
            .map(|k| (k, std::env::var(k).ok()))
            .collect();
        Self {
            _lock: lock,
            snapshot,
        }
    }

    /// Sets `HOME` to a fresh tempdir and clears all `DRIVE_*` env vars.
    ///
    /// Returns the tempdir so the caller can inspect the
    /// `.omni-dev/settings.json` written inside it.
    pub(crate) fn clear_credentials(&self) -> tempfile::TempDir {
        let dir = {
            std::fs::create_dir_all("tmp").ok();
            tempfile::TempDir::new_in("tmp").unwrap()
        };
        std::env::set_var("HOME", dir.path());
        std::env::remove_var(crate::utils::settings::PROFILE_ENV_VAR);
        std::env::remove_var(DRIVE_CLIENT_ID);
        std::env::remove_var(DRIVE_CLIENT_SECRET);
        std::env::remove_var(DRIVE_REFRESH_TOKEN);
        std::env::remove_var(DRIVE_SCOPE);
        std::env::remove_var(DRIVE_ACCOUNT_ENV);
        std::env::remove_var(DRIVE_API_URL);
        std::env::remove_var(SHEETS_API_URL);
        std::env::remove_var(DOCS_API_URL);
        dir
    }

    /// Points every Google API host override at a dead local address.
    ///
    /// For tests that exercise a code path which *constructs* a client
    /// without a wiremock server in hand. Without this a `SheetsClient` or
    /// `DocsClient` falls back to the real `sheets.googleapis.com` /
    /// `docs.googleapis.com`, so a routing test would make an outbound
    /// request to Google.
    ///
    /// Every new second-host client must be added here as well as to
    /// [`Self::take`]'s snapshot — this is the one of the two whose omission
    /// is silent, because the test still passes while talking to Google.
    pub(crate) fn redirect_api_hosts_to_a_dead_port(&self) {
        std::env::set_var(DRIVE_API_URL, "http://127.0.0.1:1");
        std::env::set_var(SHEETS_API_URL, "http://127.0.0.1:1");
        std::env::set_var(DOCS_API_URL, "http://127.0.0.1:1");
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in &self.snapshot {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }
}

// ── Lease ledger fixtures ────────────────────────────────────────────────

use crate::drive::lease::ledger::{LeaseBackup, LeaseLedger, LeaseRecord};

/// The dummy backup every `seed_lease*` variant uses when the caller
/// doesn't need to control it.
fn dummy_lease_backup() -> LeaseBackup {
    LeaseBackup::Bytes {
        path: std::path::PathBuf::from("/tmp/test-backup"),
        sha256: "deadbeef".to_string(),
        size: 0,
    }
}

/// Seeds `ledger_path` with a fresh, live lease for `file_id` at `version`
/// using the fixed token `"test-lease-token"`, a dummy `Bytes` backup, and
/// a 30-minute expiry, returning the token — the shape every lease-gated
/// write engine's tests need.
pub(crate) fn seed_lease(ledger_path: &std::path::Path, file_id: &str, version: &str) -> String {
    seed_lease_with_backup(ledger_path, file_id, version, dummy_lease_backup())
}

/// [`seed_lease`], with an explicit `backup` instead of the dummy one — for
/// tests that assert on the backup's own content/shape.
pub(crate) fn seed_lease_with_backup(
    ledger_path: &std::path::Path,
    file_id: &str,
    version: &str,
    backup: LeaseBackup,
) -> String {
    seed_lease_full(
        ledger_path,
        "test-lease-token",
        file_id,
        version,
        chrono::Duration::minutes(30),
        backup,
    )
}

/// [`seed_lease_full`], but with explicit `acquired_at`/`expires_at`
/// timestamps instead of a relative expiry — for tests that need to
/// control staleness independent of "now" (e.g. a lease already expired,
/// or held for a specific duration).
pub(crate) fn seed_lease_at(
    ledger_path: &std::path::Path,
    token: &str,
    file_id: &str,
    version: &str,
    acquired_at: chrono::DateTime<chrono::Utc>,
    expires_at: chrono::DateTime<chrono::Utc>,
    backup: LeaseBackup,
) -> String {
    let mut ledger = LeaseLedger::default();
    ledger.insert(LeaseRecord {
        token: token.to_string(),
        file_id: file_id.to_string(),
        version: version.to_string(),
        modified_time: None,
        backup,
        acquired_at,
        expires_at,
        released_at: None,
        superseded_by: None,
        restored_at: None,
        restored_sheet_id: None,
    });
    ledger.save(ledger_path).unwrap();
    token.to_string()
}

/// The fully general form: explicit token and expiry as well as backup.
pub(crate) fn seed_lease_full(
    ledger_path: &std::path::Path,
    token: &str,
    file_id: &str,
    version: &str,
    expiry: chrono::Duration,
    backup: LeaseBackup,
) -> String {
    let now = chrono::Utc::now();
    seed_lease_at(
        ledger_path,
        token,
        file_id,
        version,
        now,
        now + expiry,
        backup,
    )
}

// ── Authenticator test doubles ──────────────────────────────────────────

use crate::drive::lease::authenticate::{AuthOutcome, AuthPolicy, Authenticator};

/// An [`Authenticator`] that always returns a fixed, caller-supplied
/// [`AuthOutcome`] — for tests exercising both the authorized and
/// denied/unavailable paths without a real prompt.
pub(crate) struct FakeAuthenticator(pub(crate) AuthOutcome);

impl Authenticator for FakeAuthenticator {
    fn authenticate(&self, _reason: &str, _policy: AuthPolicy) -> AuthOutcome {
        self.0.clone()
    }
}
