//! Single-instance supervision: the exclusive socket bind *is* the lock.
//!
//! Binding the control socket fails with `EADDRINUSE` when the path is already
//! occupied. We then [`ping`](super::client::DaemonClient::ping) it: a live
//! daemon answers (so we refuse to start a second), while a stale socket left
//! by a crashed daemon does not (so we reclaim it and rebind).
//!
//! The socket is bound owner-private (`0600`) *from birth*: [`bind_private`]
//! tightens the process umask across the `bind` so the inode is never created
//! group/world-accessible. This closes the window a post-bind `chmod` would
//! leave open, so the file mode no longer depends on the parent directory
//! already being `0700`. See issue #995.

use std::io::ErrorKind;
use std::path::Path;

use anyhow::{bail, Context, Result};
use tokio::net::UnixListener;

use super::client::DaemonClient;
use super::paths;

/// Binds the control socket, reclaiming a stale socket from a crashed daemon
/// but refusing to displace a live one.
pub async fn bind_or_reclaim(path: &Path) -> Result<UnixListener> {
    match bind_private(path) {
        Ok(listener) => {
            paths::set_file_0600(path)?;
            Ok(listener)
        }
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            if DaemonClient::new(path).ping().await.is_ok() {
                bail!(
                    "a daemon is already running (socket {}); use `omni-dev daemon status`",
                    path.display()
                );
            }
            tracing::warn!(
                "reclaiming stale daemon socket at {} (no live daemon answered)",
                path.display()
            );
            std::fs::remove_file(path)
                .with_context(|| format!("failed to remove stale socket {}", path.display()))?;
            let listener = bind_private(path)
                .with_context(|| format!("failed to bind daemon socket {}", path.display()))?;
            paths::set_file_0600(path)?;
            Ok(listener)
        }
        Err(e) => {
            Err(e).with_context(|| format!("failed to bind daemon socket {}", path.display()))
        }
    }
}

/// Binds a Unix socket owner-private (`0600`) by masking off the group/world
/// permission bits while the inode is created, so there is never a window in
/// which the control socket — over which privileged service ops run — is
/// group/world-accessible. Returns the raw `io::Error` so callers can still
/// match `AddrInUse`.
///
/// The umask is process-global, so [`UmaskGuard`] restores the prior value the
/// instant `bind` returns. The guarded span is fully synchronous (no `.await`),
/// so no other task on this worker thread can observe the tightened umask;
/// genuinely-parallel OS threads creating files in the same instant are the only
/// residual exposure, which is acceptable for a one-shot startup bind.
fn bind_private(path: &Path) -> std::io::Result<UnixListener> {
    #[cfg(unix)]
    let _umask = UmaskGuard::set(nix::sys::stat::Mode::from_bits_truncate(0o177));
    UnixListener::bind(path)
}

/// Sets the process umask for the lifetime of the guard, restoring the previous
/// value on drop.
#[cfg(unix)]
struct UmaskGuard(nix::sys::stat::Mode);

#[cfg(unix)]
impl UmaskGuard {
    fn set(mask: nix::sys::stat::Mode) -> Self {
        Self(nix::sys::stat::umask(mask))
    }
}

#[cfg(unix)]
impl Drop for UmaskGuard {
    fn drop(&mut self) {
        nix::sys::stat::umask(self.0);
    }
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// `bind_private` alone — with no follow-up `chmod` — must yield a `0600`
    /// socket, proving the umask closes the window rather than a post-bind
    /// `set_file_0600`. Needs a Tokio runtime to register the listener fd.
    #[tokio::test]
    async fn bind_private_creates_an_owner_only_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("d.sock");
        let listener = bind_private(&socket).unwrap();
        let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "socket mode was {mode:o}, expected 600");
        drop(listener);
    }
}
