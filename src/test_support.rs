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
    //! Helpers for tests that write an executable shim and then `execve` it.
    //!
    //! Writing a file and immediately running it races every other thread in
    //! the test binary that `fork()`s — every `Command::spawn` does. The child
    //! inherits a *duplicate* of our still-open writable FD, and because
    //! `O_CLOEXEC` closes only on `execve` (not on a bare `fork`), the kernel
    //! refuses our own `execve` of that file with `ETXTBSY` ("Text file busy")
    //! until the child execs and the duplicate closes. The window is
    //! microscopic but real under high parallelism (`cargo llvm-cov`); it fired
    //! on the v0.36.0 release CI. See issues #642 and #1348.
    //!
    //! [`write_exec_script`] holds the writable FD open for as short as
    //! possible (one open, `sync_all`, explicit drop) but cannot make the
    //! window zero. [`retry_on_etxtbsy`] closes it for good: re-run the exec a
    //! few times, since the child releases the inherited FD the instant it
    //! execs. The retry lives only in the test harness — `ETXTBSY` here is an
    //! artifact of writing the very binary we then run, which never happens to
    //! a real `gh`/`claude`, so production keeps failing loudly on it. (The
    //! `claude-cli` backend does the equivalent at its own spawn boundary with
    //! [`spawn_with_etxtbsy_retry`](crate::claude::ai::claude_cli).)
    //!
    //! Separately, [`shim_lock`] serialises tests that spawn a subprocess from a
    //! freshly-written shim. This is **not** the `ETXTBSY` fix (the retry above
    //! is) — the offending `fork()` comes from unrelated tests that never take
    //! this lock. What it does buy is bounding how many such subprocesses run at
    //! once: without it, high parallelism (e.g. `cargo test` on a many-core
    //! host) starves timing-sensitive subprocess tests — a freshly-spawned shim
    //! scheduled too late to write its state before a short run timeout reaps
    //! it. See `claude::ai::claude_cli::tests::timeout_reaps_full_process_group`.
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    /// The errno the kernel returns when a process execs a file that some
    /// (possibly other) process still holds open for writing. `26` on both
    /// Linux and macOS.
    const ETXTBSY: i32 = 26;

    /// Serialises tests that spawn a subprocess from a freshly-written shim, so
    /// concurrent subprocess load stays bounded (see the module docs — this is
    /// the starvation guard, **not** the `ETXTBSY` fix).
    static SHIM_LOCK: Mutex<()> = Mutex::new(());

    /// Acquires the shim serialisation lock, recovering from poisoning so an
    /// intentional panic in one shim test doesn't cascade into the rest of the
    /// suite.
    pub(crate) fn shim_lock() -> MutexGuard<'static, ()> {
        SHIM_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Retries `f` while it fails with [`ETXTBSY`], backing off with bounded
    /// exponential delay. Success and every *non*-`ETXTBSY` error return
    /// immediately — so a test that expects a different failure (a non-zero
    /// exit, unparseable output) still sees exactly that, never a spuriously
    /// retried success.
    ///
    /// Wrap any call that ultimately `execve`s a freshly-written shim. See the
    /// module docs for why the race exists and why the retry is test-only.
    pub(crate) fn retry_on_etxtbsy<T>(
        mut f: impl FnMut() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        use std::time::Duration;
        const MAX_ATTEMPTS: u32 = 8;
        let mut backoff = Duration::from_millis(5);
        for _ in 1..MAX_ATTEMPTS {
            match f() {
                Err(e) if is_etxtbsy(&e) => {
                    std::thread::sleep(backoff);
                    backoff = backoff.saturating_mul(2);
                }
                // Success, or a non-ETXTBSY error: hand it straight back.
                other => return other,
            }
        }
        // Budget exhausted: return the final attempt's result, ETXTBSY or not,
        // so the caller fails loudly rather than looping forever.
        f()
    }

    /// Whether any error in `err`'s chain is an [`std::io::Error`] carrying
    /// [`ETXTBSY`]. Walks the whole chain because the exec failure is usually
    /// wrapped in caller `.context(..)` by the time it reaches a test.
    fn is_etxtbsy(err: &anyhow::Error) -> bool {
        err.chain().any(|cause| {
            cause
                .downcast_ref::<std::io::Error>()
                .and_then(std::io::Error::raw_os_error)
                == Some(ETXTBSY)
        })
    }

    /// Writes an executable script at `path`, flushes it to disk, and
    /// explicitly drops the writable FD before returning. Setting mode
    /// via `OpenOptions` avoids a second open-for-write that
    /// `chmod`-after-`fs::write` would cause. Pair the exec of the written
    /// shim with [`retry_on_etxtbsy`] to absorb the residual `fork`/`exec`
    /// race.
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

    #[cfg(test)]
    mod tests {
        use super::*;

        /// An `ETXTBSY` `io::Error` wrapped in `.context(..)`, matching how the
        /// exec failure reaches a test through a caller's error chain.
        fn etxtbsy_error() -> anyhow::Error {
            anyhow::Error::new(std::io::Error::from_raw_os_error(ETXTBSY))
                .context("failed to run the shim")
        }

        #[test]
        fn is_etxtbsy_sees_a_wrapped_text_file_busy() {
            assert!(is_etxtbsy(&etxtbsy_error()));
        }

        #[test]
        fn is_etxtbsy_rejects_other_errors() {
            // ENOENT is a spawn error too, but not the race we retry.
            let enoent = anyhow::Error::new(std::io::Error::from_raw_os_error(2))
                .context("failed to run the shim");
            assert!(!is_etxtbsy(&enoent));
            assert!(!is_etxtbsy(&anyhow::anyhow!("plain error, no io source")));
        }

        #[test]
        fn retry_on_etxtbsy_succeeds_after_transient_failures() {
            let mut calls = 0;
            let out = retry_on_etxtbsy(|| {
                calls += 1;
                if calls <= 3 {
                    Err(etxtbsy_error())
                } else {
                    Ok(calls)
                }
            })
            .unwrap();
            assert_eq!(out, 4, "should succeed on the 4th attempt");
            assert_eq!(calls, 4);
        }

        #[test]
        fn retry_on_etxtbsy_returns_a_non_etxtbsy_error_without_retrying() {
            let mut calls = 0;
            let result: anyhow::Result<()> = retry_on_etxtbsy(|| {
                calls += 1;
                Err(anyhow::anyhow!("a real failure"))
            });
            assert!(result.is_err());
            assert_eq!(calls, 1, "a non-ETXTBSY error must not be retried");
        }

        #[test]
        fn retry_on_etxtbsy_gives_up_after_the_budget_and_returns_the_last_error() {
            // Persistent ETXTBSY: exhaust the retry budget and surface the final
            // error rather than looping forever. Covers the give-up path.
            let mut calls = 0;
            let result: anyhow::Result<()> = retry_on_etxtbsy(|| {
                calls += 1;
                Err(etxtbsy_error())
            });
            assert!(is_etxtbsy(&result.unwrap_err()));
            assert_eq!(calls, 8, "should try MAX_ATTEMPTS times, then give up");
        }
    }
}
