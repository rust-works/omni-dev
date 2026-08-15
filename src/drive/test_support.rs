//! Shared test helpers for Drive unit tests.
//!
//! Any test that mutates `HOME` or a `DRIVE_*` environment variable must
//! acquire [`EnvGuard`] so parallel tests don't race on process-wide state.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::drive::account::DRIVE_ACCOUNT_ENV;
use crate::drive::auth::{DRIVE_CLIENT_ID, DRIVE_CLIENT_SECRET, DRIVE_REFRESH_TOKEN, DRIVE_SCOPE};

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
        let keys = [
            "HOME",
            DRIVE_CLIENT_ID,
            DRIVE_CLIENT_SECRET,
            DRIVE_REFRESH_TOKEN,
            DRIVE_SCOPE,
            DRIVE_ACCOUNT_ENV,
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
