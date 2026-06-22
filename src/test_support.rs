//! Shared test-only helpers.
//!
//! These utilities are consumed by unit tests across the crate and must
//! stay in sync between shim-writing sites — see issue #642.

#![allow(clippy::unwrap_used, clippy::expect_used)]

pub(crate) mod failing_io {
    //! Writer fixture that always returns `ErrorKind::Other` from
    //! `write` and `flush`. Used to drive `?`-propagation Err branches
    //! in destructive-command tests where the prompt/preview write or
    //! the post-API-success writeln is expected to fail.
    pub(crate) struct FailingWriter;

    impl std::io::Write for FailingWriter {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("simulated write failure"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("simulated flush failure"))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::Write;

        /// Direct cover for `FailingWriter::flush`. The destructive-command
        /// tests fail at the prior `write!` so flush never fires; this
        /// asserts its body still returns the expected error.
        #[test]
        fn flush_returns_error() {
            let mut w = FailingWriter;
            let err = w.flush().unwrap_err();
            assert!(err.to_string().contains("simulated flush failure"));
        }
    }
}

pub(crate) mod env {
    //! Pure in-memory [`EnvSource`](crate::utils::env::EnvSource) for tests.
    //!
    //! `MapEnv` lets env-parsing boundaries be tested without mutating the
    //! process-global environment: a test builds its own map and passes
    //! `&map` to the seam's `*_with(&impl EnvSource, …)` entry point. Because
    //! the map is an owned value with no shared state, such tests need no
    //! lock and run fully in parallel (issue #1030 / #821).
    use std::collections::HashMap;

    /// An [`EnvSource`](crate::utils::env::EnvSource) backed by an in-memory
    /// map — the test counterpart to
    /// [`SystemEnv`](crate::utils::env::SystemEnv).
    #[derive(Debug, Default, Clone)]
    pub(crate) struct MapEnv(HashMap<String, String>);

    impl MapEnv {
        /// Creates an empty environment (every lookup returns `None`).
        pub(crate) fn new() -> Self {
            Self::default()
        }

        /// Inserts `key = value` and returns `self`, for builder-style setup.
        pub(crate) fn with(mut self, key: &str, value: &str) -> Self {
            self.0.insert(key.to_string(), value.to_string());
            self
        }
    }

    impl crate::utils::env::EnvSource for MapEnv {
        fn var(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }
}

pub(crate) mod atlassian_env {
    //! In-process Atlassian env-var guard for tests that drive
    //! `cli::atlassian::helpers::create_client()` end-to-end.
    //!
    //! Mirrors `tests/mcp_integration_test.rs::AtlassianEnvGuard` (which lives
    //! in a *separate* integration-test process and so keeps its own lock).
    //! Within the lib-test process every guard that mutates the Atlassian
    //! credential env vars **must serialise on the one canonical mutex**
    //! [`crate::atlassian::auth::test_util::AUTH_ENV_MUTEX`] — independent
    //! mutexes over the same process-global vars provide no mutual exclusion
    //! and caused the flaky env race in issue #950.
    //!
    //! This is transitional scaffolding: as the remaining `*Command` tests
    //! migrate to the [`create_client_from`] dependency-injection seam (and
    //! stop mutating env entirely), their use of this guard — and eventually
    //! the guard itself — can be removed.
    //!
    //! [`create_client_from`]: crate::cli::atlassian::helpers::create_client_from
    use std::sync::MutexGuard;

    pub(crate) struct AtlassianEnvGuard {
        _guard: MutexGuard<'static, ()>,
        prev_home: Option<String>,
        prev_xdg: Option<String>,
        prev_url: Option<String>,
        prev_email: Option<String>,
        prev_token: Option<String>,
        _tmp: tempfile::TempDir,
    }

    impl AtlassianEnvGuard {
        /// Repoints HOME at an empty tempdir and sets the Atlassian env
        /// vars so `create_client()` produces a client targeting the
        /// supplied URL with the supplied credentials.
        pub(crate) fn new(instance_url: &str, email: &str, token: &str) -> Self {
            let guard = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let tmp = tempfile::tempdir().unwrap();
            let prev_home = std::env::var("HOME").ok();
            let prev_xdg = std::env::var("XDG_CONFIG_HOME").ok();
            let prev_url = std::env::var("ATLASSIAN_INSTANCE_URL").ok();
            let prev_email = std::env::var("ATLASSIAN_EMAIL").ok();
            let prev_token = std::env::var("ATLASSIAN_API_TOKEN").ok();
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join("xdg"));
            std::env::set_var("ATLASSIAN_INSTANCE_URL", instance_url);
            std::env::set_var("ATLASSIAN_EMAIL", email);
            std::env::set_var("ATLASSIAN_API_TOKEN", token);
            Self {
                _guard: guard,
                prev_home,
                prev_xdg,
                prev_url,
                prev_email,
                prev_token,
                _tmp: tmp,
            }
        }
    }

    impl Drop for AtlassianEnvGuard {
        fn drop(&mut self) {
            restore("HOME", self.prev_home.as_deref());
            restore("XDG_CONFIG_HOME", self.prev_xdg.as_deref());
            restore("ATLASSIAN_INSTANCE_URL", self.prev_url.as_deref());
            restore("ATLASSIAN_EMAIL", self.prev_email.as_deref());
            restore("ATLASSIAN_API_TOKEN", self.prev_token.as_deref());
        }
    }

    fn restore(key: &str, prev: Option<&str>) {
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}

#[cfg(unix)]
pub(crate) mod shim {
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    /// Serializes every test that writes an executable shim and then
    /// `execve`s it. Belt-and-braces pairing with `write_exec_script`'s
    /// sync+close: even with each test's FD fully released before exec,
    /// high parallelism (cargo llvm-cov) could still land a `fork()` from
    /// one test while another thread's writable FD was live, letting the
    /// child inherit it and hit `ETXTBSY`. See issue #642.
    static SHIM_LOCK: Mutex<()> = Mutex::new(());

    /// Acquires the crate-wide shim lock, recovering from poisoning so
    /// intentional panics in one test don't cascade into the rest of the
    /// suite.
    pub(crate) fn shim_lock() -> MutexGuard<'static, ()> {
        SHIM_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Writes an executable script at `path`, flushes it to disk, and
    /// explicitly drops the writable FD before returning. Setting mode
    /// via `OpenOptions` avoids a second open-for-write that
    /// `chmod`-after-`fs::write` would cause.
    pub(crate) fn write_exec_script(path: &Path, script: &str) {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o755)
            .open(path)
            .unwrap();
        file.write_all(script.as_bytes()).unwrap();
        file.sync_all().unwrap();
        drop(file);
    }
}
