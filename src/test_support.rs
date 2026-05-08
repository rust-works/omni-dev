//! Shared test-only helpers.
//!
//! These utilities are consumed by unit tests across the crate and must
//! stay in sync between shim-writing sites — see issue #642.

#![allow(clippy::unwrap_used, clippy::expect_used)]

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
