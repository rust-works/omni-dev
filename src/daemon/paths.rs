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
}
