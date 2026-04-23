//! Shared test helpers for Datadog unit tests.
//!
//! Any test that mutates `HOME` or a `DATADOG_*` environment variable must
//! acquire [`EnvGuard`] so parallel tests don't race on process-wide state.
//! Keeping the mutex here (rather than per-module) ensures that tests in
//! `src/datadog/auth.rs`, `src/cli/datadog/auth.rs`, and
//! `src/cli/datadog/helpers.rs` all serialise against each other.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::datadog::auth::{DATADOG_API_KEY, DATADOG_APP_KEY, DATADOG_SITE};

/// Process-wide mutex serialising tests that mutate `HOME` and the
/// Datadog credential environment variables.
static DATADOG_ENV_MUTEX: Mutex<()> = Mutex::new(());

/// RAII guard: snapshots `HOME` + every Datadog credential env var on
/// construction and restores them on drop.
pub(crate) struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    snapshot: Vec<(&'static str, Option<String>)>,
}

impl EnvGuard {
    pub(crate) fn take() -> Self {
        let lock = DATADOG_ENV_MUTEX
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let keys = ["HOME", DATADOG_API_KEY, DATADOG_APP_KEY, DATADOG_SITE];
        let snapshot = keys
            .into_iter()
            .map(|k| (k, std::env::var(k).ok()))
            .collect();
        Self {
            _lock: lock,
            snapshot,
        }
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

/// Sets `HOME` to a fresh tempdir and clears all `DATADOG_*` env vars.
///
/// Returns the tempdir so the caller can populate `.omni-dev/settings.json`
/// inside it. The guard parameter enforces ordering: callers must hold an
/// [`EnvGuard`] before invoking this helper.
///
/// Uses `tempfile::tempdir()` (absolute path via `std::env::temp_dir`)
/// rather than a CWD-relative tempdir so that concurrent tests which
/// mutate CWD (e.g. `mcp::resources` and `cli::git::view` tests) cannot
/// race this helper into resolving against the wrong working directory.
pub(crate) fn with_empty_home(_guard: &EnvGuard) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    std::env::set_var("HOME", dir.path());
    std::env::remove_var(DATADOG_API_KEY);
    std::env::remove_var(DATADOG_APP_KEY);
    std::env::remove_var(DATADOG_SITE);
    dir
}
