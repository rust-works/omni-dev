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

/// The bridge token file co-located with a control socket
/// (`<socket dir>/bridge.token`), so a custom `--socket` keeps its token beside
/// it. For the default socket this equals [`token_path`].
pub fn token_path_for_socket(socket: &Path) -> PathBuf {
    socket.parent().map_or_else(
        || PathBuf::from("bridge.token"),
        |dir| dir.join("bridge.token"),
    )
}

/// Creates `dir` (and ancestors) if absent and tightens it to owner-only
/// (`0700`) on Unix.
pub fn ensure_dir_0700(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("failed to create directory {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to set 0700 on {}", dir.display()))?;
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
