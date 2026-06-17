//! Process-lifecycle wiring: translate OS termination signals into a graceful
//! shutdown of the daemon's [`CancellationToken`].

use tokio_util::sync::CancellationToken;

/// Spawns a task that cancels `shutdown` when the process is asked to stop.
///
/// On Unix this listens for both `SIGTERM` (what `launchctl bootout` and
/// service managers send) and `SIGINT` (Ctrl-C in a foreground `daemon run`).
/// Elsewhere it listens for Ctrl-C only.
pub fn install_signal_handlers(shutdown: CancellationToken) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        tokio::spawn(async move {
            let mut term = match signal(SignalKind::terminate()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("failed to install SIGTERM handler: {e}");
                    return;
                }
            };
            let mut interrupt = match signal(SignalKind::interrupt()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("failed to install SIGINT handler: {e}");
                    return;
                }
            };
            tokio::select! {
                _ = term.recv() => tracing::info!("received SIGTERM; shutting down"),
                _ = interrupt.recv() => tracing::info!("received SIGINT; shutting down"),
            }
            shutdown.cancel();
        });
    }
    #[cfg(not(unix))]
    {
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                tracing::info!("received Ctrl-C; shutting down");
                shutdown.cancel();
            }
        });
    }
}
