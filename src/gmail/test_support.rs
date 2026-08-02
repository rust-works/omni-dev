//! Shared test helpers for Gmail unit tests.
//!
//! Any test that mutates `HOME` or a `GMAIL_*` environment variable must
//! acquire [`EnvGuard`] so parallel tests don't race on process-wide state.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::gmail::auth::{GMAIL_CLIENT_ID, GMAIL_CLIENT_SECRET, GMAIL_REFRESH_TOKEN, GMAIL_SCOPE};

/// Process-wide mutex serialising tests that mutate `HOME` and the Gmail
/// credential environment variables.
///
/// Aliases the crate-wide [`crate::test_support::HOME_ENV_MUTEX`] so Gmail's
/// `HOME` mutation also serialises against every other domain's (Atlassian,
/// Datadog, …) — an independent `Mutex<()>` here provides no real exclusion
/// against them, which is exactly the race that surfaced between Gmail and
/// Datadog in issue #1465.
static GMAIL_ENV_MUTEX: &Mutex<()> = &crate::test_support::HOME_ENV_MUTEX;

/// RAII guard: snapshots `HOME` + every Gmail credential env var on
/// construction and restores them on drop.
pub(crate) struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    snapshot: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    pub(crate) fn take() -> Self {
        let lock = GMAIL_ENV_MUTEX
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let keys = [
            "HOME",
            GMAIL_CLIENT_ID,
            GMAIL_CLIENT_SECRET,
            GMAIL_REFRESH_TOKEN,
            GMAIL_SCOPE,
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

    /// Sets `HOME` to a fresh tempdir and clears all `GMAIL_*` env vars.
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
        std::env::remove_var(GMAIL_CLIENT_ID);
        std::env::remove_var(GMAIL_CLIENT_SECRET);
        std::env::remove_var(GMAIL_REFRESH_TOKEN);
        std::env::remove_var(GMAIL_SCOPE);
        dir
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
