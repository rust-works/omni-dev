//! Shared test-only helpers.
//!
//! These utilities are consumed by unit tests across the crate and must
//! stay in sync between shim-writing sites — see issue #642.

#![allow(clippy::unwrap_used, clippy::expect_used)]

/// Process-wide mutex serialising every test in the crate that mutates the
/// global `HOME` environment variable, or any credential env var whose
/// resolution depends on it (`dirs::home_dir()` /
/// `Settings::get_settings_path()`).
///
/// Every HOME-mutating test-support module (Atlassian, Datadog, Gmail, the
/// `ai_chat`/`cli::ai::chat` provider tests, …) aliases this **one** static
/// rather than declaring its own `Mutex<()>`. Independent per-module mutexes
/// provide no mutual exclusion against each other — each only serialises its
/// own module's tests — while `HOME` itself is shared process-wide state, so
/// two modules' tests can still interleave and race on it. That exact
/// pattern caused the flaky race fixed for Atlassian in issue #950, and
/// resurfaced as a Gmail-vs-Datadog race (both mutating `HOME` under their
/// own independent mutex) in issue #1465.
pub(crate) static HOME_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Process-wide mutex serialising every test that mutates
/// `OMNI_DEV_LOG_FILE`, `OMNI_DEV_AUDIT_LOG_FILE` or `OMNI_DEV_LOG_DISABLE` —
/// `crate::request_log`'s own path-resolution and fail-closed-audit tests,
/// and `crate::cli::log`'s `--audit` flag-selection test. Same rationale as
/// [`HOME_ENV_MUTEX`]: these are shared, process-wide env vars, so two tests
/// mutating them under independent locks (or no lock) can still interleave
/// and race. Does **not** cover every existing `OMNI_DEV_LOG_FILE` mutation
/// in the crate (e.g. `daemon::services::worktrees`'s poller tests predate
/// this lock) — new tests should take it; retrofitting older ones is a
/// follow-up, not a blocker.
pub(crate) static REQUEST_LOG_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Redirects the audit log into an isolated tempdir for the life of one
/// test — for this thread only, through `request_log::TEST_AUDIT_ROUTE`,
/// not the process-global `OMNI_DEV_AUDIT_LOG_FILE` — so it needs no
/// [`REQUEST_LOG_ENV_MUTEX`] and tests holding one run fully in parallel.
///
/// Every Drive lease-check/acquire test that reaches a live lease or a
/// refusal triggers a best-effort or write-ahead audit write (ADR-0080
/// §11) as a side effect of calling production code, whether or not the
/// test cares about its content. A test that doesn't redirect lands in
/// `request_log`'s shared scratch file — never the real machine's
/// `audit.jsonl`, and never another test's env override either, since an
/// un-opted thread does not consult the env var at all; a test that wants
/// to read its *own* records back takes this guard. The writes this guard
/// observes must therefore happen on the test's own thread (a
/// `#[tokio::test]` body does; a `spawn_blocking` closure does not).
///
/// [`Self::records`] reads back what production code wrote, so a test
/// asserting on the audit trail needs no private line-parsing helper.
pub(crate) struct AuditLogGuard {
    path: std::path::PathBuf,
}

impl AuditLogGuard {
    pub(crate) fn redirect(dir: &std::path::Path) -> Self {
        let path = dir.join("audit.jsonl");
        crate::request_log::TEST_AUDIT_ROUTE.with(|slot| {
            *slot.borrow_mut() = Some(crate::request_log::TestAuditRoute::Path(path.clone()));
        });
        Self { path }
    }

    /// Every record written so far, in order. An audit file that was never
    /// created (nothing wrote) reads as empty rather than panicking, so a
    /// "must write nothing" assertion is `assert!(guard.records().is_empty())`.
    pub(crate) fn records(&self) -> Vec<crate::request_log::LogRecord> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => text
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect(),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(err) => panic!("failed to read {}: {err}", self.path.display()),
        }
    }

    /// The `verdict` of every record written so far, in order — the
    /// assertion almost every audit-trail test makes.
    pub(crate) fn verdicts(&self) -> Vec<String> {
        self.records()
            .iter()
            .map(|record| record.context.get("verdict").cloned().unwrap_or_default())
            .collect()
    }
}

impl Drop for AuditLogGuard {
    fn drop(&mut self) {
        crate::request_log::TEST_AUDIT_ROUTE.with(|slot| *slot.borrow_mut() = None);
    }
}

#[cfg(test)]
mod audit_log_guard_tests {
    use super::AuditLogGuard;

    /// Direct cover for the "nothing wrote" branch of [`AuditLogGuard::records`]
    /// (`ErrorKind::NotFound` reads as empty rather than panicking): every
    /// other caller in the crate redirects and then triggers a write before
    /// reading records back, so this path is otherwise never exercised.
    #[test]
    fn records_reads_as_empty_before_anything_writes() {
        let dir = tempfile::tempdir().unwrap();
        let guard = AuditLogGuard::redirect(dir.path());
        assert!(guard.records().is_empty());
    }

    /// Direct cover for the panic branch of [`AuditLogGuard::records`]: a
    /// read failure other than `NotFound` must not read as "nothing wrote
    /// yet" — it must panic instead, the same fail-loud contract every
    /// other test-support helper in this file follows. A directory in
    /// place of the audit file forces that non-`NotFound` failure, the
    /// same trick `drive::lease::check`'s own fail-closed test uses.
    #[test]
    fn records_panics_when_the_read_fails_for_a_reason_other_than_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let guard = AuditLogGuard::redirect(dir.path());
        std::fs::create_dir(dir.path().join("audit.jsonl")).unwrap();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| guard.records()));
        assert!(result.is_err());
    }
}

/// Opts the current test thread into release-build audit path resolution
/// — `OMNI_DEV_AUDIT_LOG_FILE` if set, else the scratch default — for the
/// life of one test. The counterpart of [`AuditLogGuard`] for the handful
/// of tests *of* the env override itself; a thread holding neither is
/// pinned to the scratch file and cannot see the env var. Anything that
/// sets the env var must still hold [`REQUEST_LOG_ENV_MUTEX`], since the
/// var itself remains process-global among the threads that opted in.
pub(crate) struct AuditEnvRouteGuard;

impl AuditEnvRouteGuard {
    pub(crate) fn take() -> Self {
        crate::request_log::TEST_AUDIT_ROUTE
            .with(|slot| *slot.borrow_mut() = Some(crate::request_log::TestAuditRoute::Env));
        Self
    }
}

impl Drop for AuditEnvRouteGuard {
    fn drop(&mut self) {
        crate::request_log::TEST_AUDIT_ROUTE.with(|slot| *slot.borrow_mut() = None);
    }
}

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
    //! [`retry_on_etxtbsy_async`] is the same retry for a test that drives a
    //! production `async fn` end-to-end (e.g. a `WorktreesService` op that
    //! `spawn_blocking`s the shim exec internally) rather than calling the
    //! sync `Command`-spawning function directly — a plain `shim_lock` guard
    //! bounds concurrent shim subprocesses but does **not** retry the exec
    //! race, so a test that only takes the lock can still flake. See #1348 for
    //! the sync case and the `merge_queue_with_*` tests in
    //! `daemon::services::worktrees` for the async one.
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

    /// Async counterpart to [`retry_on_etxtbsy`], for a test that awaits a
    /// production `async fn` that shells out to a freshly-written shim
    /// (rather than calling the sync spawn function directly, which
    /// [`retry_on_etxtbsy`] already covers). Same retry budget and backoff,
    /// but sleeps via `tokio::time::sleep` so it can run inside a
    /// `#[tokio::test]` without blocking the runtime thread.
    pub(crate) async fn retry_on_etxtbsy_async<T, F, Fut>(mut f: F) -> anyhow::Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<T>>,
    {
        use std::time::Duration;
        const MAX_ATTEMPTS: u32 = 8;
        let mut backoff = Duration::from_millis(5);
        for _ in 1..MAX_ATTEMPTS {
            match f().await {
                Err(e) if is_etxtbsy(&e) => {
                    tokio::time::sleep(backoff).await;
                    backoff = backoff.saturating_mul(2);
                }
                other => return other,
            }
        }
        // Budget exhausted: return the final attempt's result, ETXTBSY or not,
        // so the caller fails loudly rather than looping forever.
        f().await
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

        #[tokio::test]
        async fn retry_on_etxtbsy_async_succeeds_after_transient_failures() {
            let mut calls = 0;
            let out = retry_on_etxtbsy_async(|| {
                calls += 1;
                let calls = calls;
                async move {
                    if calls <= 3 {
                        Err(etxtbsy_error())
                    } else {
                        Ok(calls)
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(out, 4, "should succeed on the 4th attempt");
            assert_eq!(calls, 4);
        }

        #[tokio::test]
        async fn retry_on_etxtbsy_async_returns_a_non_etxtbsy_error_without_retrying() {
            let mut calls = 0;
            let result: anyhow::Result<()> = retry_on_etxtbsy_async(|| {
                calls += 1;
                async { Err(anyhow::anyhow!("a real failure")) }
            })
            .await;
            assert!(result.is_err());
            assert_eq!(calls, 1, "a non-ETXTBSY error must not be retried");
        }

        #[tokio::test]
        async fn retry_on_etxtbsy_async_gives_up_after_the_budget_and_returns_the_last_error() {
            let mut calls = 0;
            let result: anyhow::Result<()> = retry_on_etxtbsy_async(|| {
                calls += 1;
                async { Err(etxtbsy_error()) }
            })
            .await;
            assert!(is_etxtbsy(&result.unwrap_err()));
            assert_eq!(calls, 8, "should try MAX_ATTEMPTS times, then give up");
        }
    }
}
