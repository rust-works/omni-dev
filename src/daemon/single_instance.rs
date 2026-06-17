//! Single-instance supervision: the exclusive socket bind *is* the lock.
//!
//! Binding the control socket fails with `EADDRINUSE` when the path is already
//! occupied. We then [`ping`](super::client::DaemonClient::ping) it: a live
//! daemon answers (so we refuse to start a second), while a stale socket left
//! by a crashed daemon does not (so we reclaim it and rebind).

use std::io::ErrorKind;
use std::path::Path;

use anyhow::{bail, Context, Result};
use tokio::net::UnixListener;

use super::client::DaemonClient;
use super::paths;

/// Binds the control socket, reclaiming a stale socket from a crashed daemon
/// but refusing to displace a live one.
pub async fn bind_or_reclaim(path: &Path) -> Result<UnixListener> {
    match UnixListener::bind(path) {
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
            let listener = UnixListener::bind(path)
                .with_context(|| format!("failed to bind daemon socket {}", path.display()))?;
            paths::set_file_0600(path)?;
            Ok(listener)
        }
        Err(e) => {
            Err(e).with_context(|| format!("failed to bind daemon socket {}", path.display()))
        }
    }
}
