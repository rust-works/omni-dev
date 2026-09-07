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
