//! Per-user runtime paths for the daemon (control socket, token file) and the
//! filesystem-permission helpers that keep them owner-private.
//!
//! The runtime directory is resolved via [`dirs::data_dir`] —
//! `~/Library/Application Support/omni-dev/` on macOS, `~/.local/share/omni-dev/`
//! on Linux. This deliberately diverges from the `~/.omni-dev` config-file
//! convention used by [`crate::claude::context`]: that governs user-editable
//! configuration, whereas these are *runtime* artifacts of a long-lived
//! menu-bar app, for which the platform data directory is the native home.
//! See ADR-0039.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// macOS caps a `sockaddr_un` path at 104 bytes (including the NUL
/// terminator); Linux allows 108. Use the smaller, portable bound.
pub const MAX_SOCKET_PATH_LEN: usize = 104;

/// The per-user runtime directory holding the daemon's socket and token file.
pub fn runtime_dir() -> Result<PathBuf> {
    let base = dirs::data_dir().context("could not determine the user data directory")?;
    Ok(base.join("omni-dev"))
}

/// Default control-socket path: `<runtime_dir>/daemon.sock`.
pub fn socket_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("daemon.sock"))
}

/// Default bridge token-file path: `<runtime_dir>/bridge.token`. Thin clients
/// (`request`/`harvest`) fall back to this when no `--token-file`/env is set.
pub fn token_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("bridge.token"))
}

/// Default per-repo PR-poll preferences path: `<runtime_dir>/worktrees-polling.json`.
///
/// The worktrees service persists the set of GitHub repos whose PR badges it
/// polls here (`0600`), so the enable/disable choice survives a daemon restart
/// (#1376). Non-secret, but co-located with the other `0600` runtime state.
pub fn worktrees_polling_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("worktrees-polling.json"))
}

/// Default resolved-PR-badge cache path: `<runtime_dir>/worktrees-pr-cache.json`.
///
/// The worktrees service persists the last polled PR badges (number / URL /
/// check-state / draft flag) here (`0600`), so a daemon restart serves badges
/// instantly and can skip its immediate re-poll when the verdicts are still fresh
/// (#1389, fix 4). Non-secret — the same badge data already rides the tree wire —
/// but co-located with the other `0600` runtime state for a consistent posture.
pub fn worktrees_pr_cache_path() -> Result<PathBuf> {
    Ok(runtime_dir()?.join("worktrees-pr-cache.json"))
}

/// The bridge token file co-located with a control socket
/// (`<socket dir>/bridge.token`), so a custom `--socket` keeps its token beside
/// it. For the default socket this equals [`token_path`].
pub fn token_path_for_socket(socket: &Path) -> PathBuf {
    socket.parent().map_or_else(
        || PathBuf::from("bridge.token"),
        |dir| dir.join("bridge.token"),
    )
}

/// The daemon log file co-located with a control socket
/// (`<socket dir>/daemon.log`).
///
/// A custom `--socket` keeps its log beside it. The non-macOS `daemon start`
/// launcher appends the detached daemon's stdout/stderr here.
pub fn log_path_for_socket(socket: &Path) -> PathBuf {
    socket
        .parent()
        .map_or_else(|| PathBuf::from("daemon.log"), |dir| dir.join("daemon.log"))
}

/// Creates `dir` (and ancestors) if absent and tightens it to owner-only
/// (`0700`) on Unix.
///
/// On Unix the mode is passed to `mkdir(2)` itself, so no directory is ever
/// looser than `0700` even for an instant (the umask can only clear bits); the
/// follow-up `chmod` re-tightens pre-existing directories and guarantees the
/// exact mode under exotic umasks (#1139).
pub fn ensure_dir_0700(dir: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .with_context(|| format!("failed to create directory {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to set 0700 on {}", dir.display()))?;
    }
    Ok(())
}

/// Writes `contents` to `path`, owner read/write only (`0600`) from birth on
/// Unix.
///
/// The mode is passed to `open(2)` at creation, so a fresh file is never
/// group/world-readable even for an instant; a pre-existing looser-perm file
/// is re-tightened via [`ensure_handle_0600`] *before* the contents land in it
/// (#1132). On non-Unix platforms this is a plain truncating write.
pub fn write_file_0600(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create file {}", path.display()))?;
    ensure_handle_0600(&file)
        .with_context(|| format!("failed to set 0600 on {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("failed to write file {}", path.display()))?;
    Ok(())
}

/// Tightens an open file to owner read/write only (`0600`) on Unix if its
/// current mode is any looser.
///
/// Operates on the handle (`fchmod(2)`), so there is no path race and the
/// umask does not apply. No-op on non-Unix platforms.
pub fn ensure_handle_0600(file: &std::fs::File) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = file
            .metadata()
            .context("failed to read file metadata")?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .context("failed to set 0600 on open file")?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = file;
    }
    Ok(())
}

/// Creates `path`'s parent directory at `0700` if it doesn't already exist.
///
/// A no-op for a bare relative filename, whose "parent" is empty — the
/// "make room for the file I'm about to create" step shared by every
/// `0600`-file writer in the crate (the request log, the Drive lease ledger
/// and its lock, the Drive lease backup directory).
pub fn ensure_parent_dir_0700(path: &Path) -> Result<()> {
    if let Some(dir) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        if !dir.exists() {
            ensure_dir_0700(dir)?;
        }
    }
    Ok(())
}

/// Creates `path` exclusively, owner read/write only (`0600`) from birth on
/// Unix.
///
/// `create_new` (`O_EXCL`), so two callers racing to create the same path
/// see exactly one winner and the other an error, never a silent overwrite
/// — [`write_file_0600`]'s sibling for callers that need "either I created
/// this file first, or I must fail" rather than a truncating write. Used by
/// the Drive lease ledger's advisory lock file, its backup files, and the
/// pre-existing `gmail insert` ledger lock.
pub fn create_new_file_0600(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .with_context(|| format!("failed to exclusively create file {}", path.display()))?;
    if let Err(err) = ensure_handle_0600(&file) {
        // We just created this file exclusively; leaving it behind on this
        // failure would be a phantom lock/marker indistinguishable from a
        // genuine collision to the next caller (issue #1687 point 6).
        // Best-effort: a failure to remove it doesn't change which error is
        // reported.
        if let Err(remove_err) = std::fs::remove_file(path) {
            tracing::debug!(
                "failed to remove {} after its fchmod failed: {remove_err}",
                path.display()
            );
        }
        return Err(err).with_context(|| format!("failed to set 0600 on {}", path.display()));
    }
    Ok(file)
}

/// Whether `err`'s cause chain includes an `io::Error` of kind
/// `AlreadyExists`.
///
/// A caller of [`create_new_file_0600`] that wraps its `Result` in extra
/// "this looks like a collision, retry" guidance must apply that guidance
/// only when the underlying failure actually was the path already existing
/// — not when `open(2)` itself succeeded and the follow-up `fchmod` safety
/// net failed instead, which is an unrelated permissions/filesystem problem
/// that "remove the stale lock and retry" or "may already exist" would
/// misdiagnose (issue #1664 review finding).
pub fn is_already_exists_error(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io_err| io_err.kind() == std::io::ErrorKind::AlreadyExists)
    })
}

/// Why [`try_lock_file_exclusive`] failed.
#[derive(Debug)]
pub enum FileLockError {
    /// The lock is already held by someone else — distinguishable from
    /// [`Self::Io`] without string-sniffing a message, so a waiting caller
    /// can poll on this variant specifically and treat any other error as
    /// unconditionally fatal.
    Busy,
    /// Opening, locking, or verifying the lock file failed for a reason
    /// other than contention.
    Io(anyhow::Error),
}

/// An advisory exclusive lock on `path`, held for [`FileLock`]'s lifetime.
///
/// On Unix this is a `flock(2)` lock: kernel-released when every handle to
/// it closes — including on process death — so unlike
/// [`create_new_file_0600`]'s `O_EXCL` marker, a crashed or SIGKILLed
/// holder never leaves a stale lock. The lock file itself is never deleted
/// (`Drop` unlocks, it does not unlink): **no caller should ever tell an
/// operator to delete this file by hand**, since that reopens the exact
/// double-spend a naive marker-plus-unlink lock risks (issue #1687).
///
/// On non-Unix (`nix`'s `flock` wrapper is Unix-only) this falls back to
/// an `O_EXCL` marker removed on drop, preserving today's semantics there.
#[derive(Debug)]
pub struct FileLock {
    #[cfg(unix)]
    #[allow(dead_code)] // Held only for its Drop (unlocks on drop); never read.
    inner: nix::fcntl::Flock<std::fs::File>,
    #[cfg(not(unix))]
    path: PathBuf,
}

/// Bound on the replacement-check retries in [`try_lock_file_exclusive`] —
/// see its doc comment.
#[cfg(unix)]
const LOCK_REPLACEMENT_RETRIES: u32 = 3;

/// Acquires an advisory exclusive lock on `path` (non-blocking).
///
/// Creates the file at `0600` if absent, but not `path`'s parent directory
/// — callers with an opinion on that call [`ensure_parent_dir_0700`] first.
///
/// `flock` locks the *open file description*, not the path: if the file at
/// `path` is deleted and recreated while a lock is held (nothing in this
/// crate does that, but a stray `rm` could), a second locker could lock the
/// new inode while the first still holds the old one — both would then
/// "hold the lock" without conflicting. After locking, this function
/// re-`stat`s `path` and compares device/inode against the locked handle;
/// on a mismatch it drops that lock and retries against the current path,
/// bounded to [`LOCK_REPLACEMENT_RETRIES`] attempts. This narrows the race,
/// it does not close it — the actual mitigation is that nothing in this
/// crate ever instructs an operator to delete the file.
#[cfg(unix)]
pub fn try_lock_file_exclusive(path: &Path) -> std::result::Result<FileLock, FileLockError> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    for _ in 0..LOCK_REPLACEMENT_RETRIES {
        let mut options = std::fs::OpenOptions::new();
        options.create(true).write(true).truncate(false).mode(0o600);
        let file = options
            .open(path)
            .with_context(|| format!("failed to open lock file {}", path.display()))
            .map_err(FileLockError::Io)?;
        ensure_handle_0600(&file)
            .with_context(|| format!("failed to set 0600 on {}", path.display()))
            .map_err(FileLockError::Io)?;

        let locked =
            match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock) {
                Ok(locked) => locked,
                Err((_file, nix::errno::Errno::EWOULDBLOCK)) => return Err(FileLockError::Busy),
                Err((_file, errno)) => {
                    return Err(FileLockError::Io(anyhow::anyhow!(
                        "failed to lock {}: {errno}",
                        path.display()
                    )))
                }
            };

        let locked_meta = locked
            .metadata()
            .with_context(|| format!("failed to stat locked handle for {}", path.display()))
            .map_err(FileLockError::Io)?;
        match std::fs::metadata(path) {
            Ok(current_meta)
                if current_meta.dev() == locked_meta.dev()
                    && current_meta.ino() == locked_meta.ino() =>
            {
                return Ok(FileLock { inner: locked });
            }
            // The path now names a different inode (or nothing) than the
            // one we locked — someone deleted/recreated it out from under
            // us. Drop this lock and retry against the current path.
            _ => {}
        }
    }
    Err(FileLockError::Io(anyhow::anyhow!(
        "gave up acquiring the lock on {} after {LOCK_REPLACEMENT_RETRIES} attempts — the file \
         kept being replaced out from under us",
        path.display()
    )))
}

#[cfg(not(unix))]
pub fn try_lock_file_exclusive(path: &Path) -> std::result::Result<FileLock, FileLockError> {
    match create_new_file_0600(path) {
        Ok(_) => Ok(FileLock {
            path: path.to_path_buf(),
        }),
        Err(err) if is_already_exists_error(&err) => Err(FileLockError::Busy),
        Err(err) => Err(FileLockError::Io(err)),
    }
}

#[cfg(not(unix))]
impl Drop for FileLock {
    fn drop(&mut self) {
        // Best-effort: this is a marker, not the lock itself — the next
        // caller's `create_new` collision check is what actually enforces
        // exclusion, so a failed unlink here just means a future acquire
        // will (incorrectly) see this as still held (STYLE-0018).
        if let Err(err) = std::fs::remove_file(&self.path) {
            tracing::debug!("failed to remove lock file {}: {err}", self.path.display());
        }
    }
}

/// Tightens an existing file to owner read/write only (`0600`) on Unix.
pub fn set_file_0600(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to set 0600 on {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

/// Fails closed if the socket path is too long for the platform's
/// `sockaddr_un`, with an actionable message instead of an opaque bind error.
pub fn check_socket_path_len(path: &Path) -> Result<()> {
    let len = path.as_os_str().len();
    if len >= MAX_SOCKET_PATH_LEN {
        bail!(
            "socket path is {len} bytes, exceeding the {MAX_SOCKET_PATH_LEN}-byte limit: {} — \
             pass a shorter --socket path",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn default_paths_sit_under_the_runtime_dir() {
        let rt = runtime_dir().unwrap();
        assert!(socket_path().unwrap().starts_with(&rt));
        assert!(token_path().unwrap().starts_with(&rt));
        let polling = worktrees_polling_path().unwrap();
        assert!(polling.starts_with(&rt));
        assert_eq!(polling.file_name().unwrap(), "worktrees-polling.json");
    }

    #[test]
    fn token_sits_beside_its_socket() {
        assert_eq!(
            token_path_for_socket(Path::new("/tmp/x/daemon.sock")),
            Path::new("/tmp/x/bridge.token")
        );
        // A bare filename (parent is the empty path) still yields a token name.
        assert_eq!(
            token_path_for_socket(Path::new("daemon.sock")),
            Path::new("bridge.token")
        );
    }

    #[test]
    fn log_sits_beside_its_socket() {
        assert_eq!(
            log_path_for_socket(Path::new("/tmp/x/daemon.sock")),
            Path::new("/tmp/x/daemon.log")
        );
        // A bare filename (parent is the empty path) still yields a log name.
        assert_eq!(
            log_path_for_socket(Path::new("daemon.sock")),
            Path::new("daemon.log")
        );
    }

    #[test]
    fn socket_path_length_guard() {
        check_socket_path_len(Path::new("/tmp/short.sock")).unwrap();
        let too_long = PathBuf::from(format!("/{}", "a".repeat(MAX_SOCKET_PATH_LEN)));
        assert!(check_socket_path_len(&too_long).is_err());
    }

    #[test]
    fn write_file_0600_creates_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("fresh.token");
        write_file_0600(&file, b"secret").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn write_file_0600_retightens_preexisting_loose_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("stale.token");
        std::fs::write(&file, "old-secret-with-longer-content").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_file_0600(&file, b"new").unwrap();
        assert_eq!(std::fs::read(&file).unwrap(), b"new");
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn ensure_dir_and_file_perms() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("run");
        ensure_dir_0700(&sub).unwrap();
        assert!(sub.is_dir());
        let file = sub.join("k");
        std::fs::write(&file, "x").unwrap();
        set_file_0600(&file).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&sub).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn ensure_parent_dir_0700_creates_a_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("run").join("k");
        ensure_parent_dir_0700(&file).unwrap();
        assert!(dir.path().join("run").is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(dir.path().join("run"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn ensure_parent_dir_0700_is_a_noop_when_the_parent_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("k");
        // The parent (`dir`) already exists — this must not error or touch it.
        ensure_parent_dir_0700(&file).unwrap();
        assert!(dir.path().is_dir());
    }

    #[test]
    fn ensure_parent_dir_0700_is_a_noop_for_a_bare_filename() {
        // A bare relative filename has an empty parent — nothing to create.
        ensure_parent_dir_0700(Path::new("bare.txt")).unwrap();
    }

    #[test]
    fn is_already_exists_error_detects_the_wrapped_io_kind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("collide");
        create_new_file_0600(&path).unwrap();
        let err = create_new_file_0600(&path).unwrap_err();
        assert!(is_already_exists_error(&err), "{err:?}");
    }

    #[test]
    fn is_already_exists_error_is_false_for_an_unrelated_failure() {
        let dir = tempfile::tempdir().unwrap();
        // A missing parent directory makes `open()` fail with `NotFound`,
        // not `AlreadyExists`.
        let path = dir.path().join("no-such-parent").join("child");
        let err = create_new_file_0600(&path).unwrap_err();
        assert!(!is_already_exists_error(&err), "{err:?}");
    }
}
