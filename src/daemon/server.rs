//! The daemon server core: bind the control socket, accept NDJSON connections,
//! route envelopes to services (or built-in ops), and shut down gracefully.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::UnixStream;
use tokio::task::{JoinError, JoinSet};
use tokio_util::codec::{Framed, LinesCodec};
use tokio_util::sync::CancellationToken;

use super::lifecycle;
use super::paths;
use super::protocol::{DaemonEnvelope, DaemonReply, StatusReport, DAEMON_SERVICE};
use super::registry::ServiceRegistry;
use super::single_instance;

/// How long to wait for accepted-but-unfinished connections to drain on
/// shutdown before aborting the stragglers. Generous enough for a normal
/// in-flight dispatch+reply, bounded so a stuck or idle client cannot hang
/// shutdown indefinitely (a service manager would `SIGKILL` us eventually).
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Configuration for a [`run`] invocation.
#[derive(Debug, Clone)]
pub struct DaemonOptions {
    /// Path the control socket is bound to.
    pub socket_path: PathBuf,
}

/// Runs the daemon until a `SIGTERM`/`SIGINT` or a built-in `shutdown` op,
/// then drains every service and removes the socket.
///
/// Binding the socket doubles as the single-instance lock (see
/// [`single_instance`]).
pub async fn run(registry: ServiceRegistry, opts: DaemonOptions) -> Result<()> {
    run_with_shutdown(Arc::new(registry), opts, CancellationToken::new()).await
}

/// Like [`run`], but with a shared registry and an externally-owned token.
///
/// The menu-bar host uses this to share the [`ServiceRegistry`] with the tray
/// and to stop the daemon from a "Quit" menu action via the
/// [`CancellationToken`].
pub async fn run_with_shutdown(
    registry: Arc<ServiceRegistry>,
    opts: DaemonOptions,
    shutdown: CancellationToken,
) -> Result<()> {
    if let Some(parent) = opts.socket_path.parent() {
        paths::ensure_dir_0700(parent)?;
    }
    paths::check_socket_path_len(&opts.socket_path)?;

    let listener = single_instance::bind_or_reclaim(&opts.socket_path).await?;
    tracing::info!("daemon listening on {}", opts.socket_path.display());

    lifecycle::install_signal_handlers(shutdown.clone());

    // Connection handlers are tracked here rather than detached, so accepted
    // requests can be drained on shutdown instead of being abandoned (#992).
    let mut conns: JoinSet<()> = JoinSet::new();
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        conns.spawn(handle_connection(
                            stream,
                            registry.clone(),
                            shutdown.clone(),
                        ));
                    }
                    Err(e) => tracing::warn!("daemon accept error: {e}"),
                }
            }
            // Reap finished handlers during normal operation so the set does
            // not grow unbounded over a long-lived daemon. The guard disables
            // this arm when empty (an empty `JoinSet` yields `None` at once,
            // which would otherwise busy-loop the select).
            joined = conns.join_next(), if !conns.is_empty() => {
                if let Some(result) = joined {
                    note_reaped(result);
                }
            }
        }
    }

    drain_connections(&mut conns, DRAIN_TIMEOUT).await;

    tracing::info!("daemon shutting down; draining services");
    registry.shutdown_all().await;
    if let Err(e) = std::fs::remove_file(&opts.socket_path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                "failed to remove socket {}: {e}",
                opts.socket_path.display()
            );
        }
    }
    Ok(())
}

/// Logs a reaped connection task that ended by panicking; clean exits and
/// cancellations are ignored. Shared by the accept-loop reaper and the drain so
/// both report a crashed handler the same way.
fn note_reaped(result: Result<(), JoinError>) {
    if let Err(e) = result {
        if e.is_panic() {
            tracing::warn!("daemon connection task panicked: {e}");
        }
    }
}

/// Awaits outstanding connection handlers (bounded by `timeout`) so an accepted
/// request finishes its dispatch+reply before the daemon tears down. Called once
/// the accept loop has stopped and *before* `shutdown_all()`, since in-flight
/// handlers may still be dispatching into live services. Stragglers past the
/// deadline are aborted rather than allowed to hang shutdown. (`timeout` is a
/// parameter, fixed to [`DRAIN_TIMEOUT`] in production, so tests can drive the
/// abort path without a multi-second wait.)
async fn drain_connections(conns: &mut JoinSet<()>, timeout: Duration) {
    let count = conns.len();
    if count == 0 {
        return;
    }
    tracing::info!("draining {count} in-flight connection(s)");
    let drain = async {
        while let Some(result) = conns.join_next().await {
            note_reaped(result);
        }
    };
    if tokio::time::timeout(timeout, drain).await.is_err() {
        tracing::warn!(
            "timed out draining connections after {timeout:?}; aborting {} straggler(s)",
            conns.len()
        );
        conns.abort_all();
        while conns.join_next().await.is_some() {}
    }
}

/// Serves one client connection: decode each NDJSON line, dispatch it, and
/// write back one reply line, until the client hangs up or a read/write error.
///
/// There is deliberately no `shutdown.cancelled()` arm here: an accepted line
/// always finishes its dispatch+reply, and shutdown is handled by the server
/// draining these tasks (see [`drain_connections`]). `shutdown` is still
/// threaded through for the built-in `shutdown` op (see [`handle_builtin`]).
async fn handle_connection(
    stream: UnixStream,
    registry: Arc<ServiceRegistry>,
    shutdown: CancellationToken,
) {
    let mut framed = Framed::new(stream, LinesCodec::new());
    while let Some(line) = framed.next().await {
        let reply = match line {
            Ok(line) => dispatch_line(&line, &registry, &shutdown).await,
            Err(e) => DaemonReply::err(format!("read error: {e}")),
        };
        let encoded = match serde_json::to_string(&reply) {
            Ok(encoded) => encoded,
            Err(e) => {
                tracing::warn!("failed to encode daemon reply: {e}");
                break;
            }
        };
        if let Err(e) = framed.send(encoded).await {
            tracing::debug!("daemon client write failed: {e}");
            break;
        }
    }
}

/// Parses one NDJSON request line and produces its reply.
async fn dispatch_line(
    line: &str,
    registry: &ServiceRegistry,
    shutdown: &CancellationToken,
) -> DaemonReply {
    let envelope: DaemonEnvelope = match serde_json::from_str(line) {
        Ok(envelope) => envelope,
        Err(e) => return DaemonReply::err(format!("invalid envelope: {e}")),
    };
    match envelope.service.as_deref() {
        None | Some(DAEMON_SERVICE) => handle_builtin(&envelope.op, registry, shutdown).await,
        Some(name) => match registry
            .dispatch(name, &envelope.op, envelope.payload)
            .await
        {
            Ok(payload) => DaemonReply::ok(payload),
            // `{:#}` includes the full anyhow source chain (e.g. "Snowflake
            // query failed: snowflake server error (000630): …") so the client
            // can see the underlying cause, not just the top-level wrapper.
            Err(e) => DaemonReply::err(format!("{e:#}")),
        },
    }
}

/// Handles the daemon's own built-in operations.
async fn handle_builtin(
    op: &str,
    registry: &ServiceRegistry,
    shutdown: &CancellationToken,
) -> DaemonReply {
    match op {
        "ping" => DaemonReply::ok(json!({ "pong": true })),
        "status" => {
            let report = StatusReport {
                services: registry.statuses().await,
            };
            match serde_json::to_value(report) {
                Ok(payload) => DaemonReply::ok(payload),
                Err(e) => DaemonReply::err(format!("failed to encode status: {e}")),
            }
        }
        "shutdown" => {
            shutdown.cancel();
            DaemonReply::ok(json!({ "stopping": true }))
        }
        other => DaemonReply::err(format!("unknown daemon op: {other}")),
    }
}

/// Resolves the control-socket path: the explicit override, or the per-user
/// default from [`paths::socket_path`].
pub fn resolve_socket(socket: Option<PathBuf>) -> Result<PathBuf> {
    match socket {
        Some(path) => Ok(path),
        None => paths::socket_path().context("failed to resolve the default daemon socket path"),
    }
}

// The daemon-server tests that bind a socket (and thus mutate the process-global
// umask via `bind_or_reclaim`) live in `tests/daemon_socket.rs`, isolated in
// their own process so the umask write cannot race the library's other parallel
// unit tests. See #1017. The tests below are socket-free: they exercise the
// connection-draining logic directly, with no `bind`, so they stay here.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drain_connections_returns_immediately_when_empty() {
        let mut conns: JoinSet<()> = JoinSet::new();
        drain_connections(&mut conns, Duration::from_secs(5)).await;
        assert!(conns.is_empty());
    }

    #[tokio::test]
    async fn drain_connections_awaits_completed_tasks() {
        let mut conns: JoinSet<()> = JoinSet::new();
        conns.spawn(async {});
        drain_connections(&mut conns, Duration::from_secs(5)).await;
        // Every tracked handler was joined.
        assert!(conns.is_empty());
    }

    #[tokio::test]
    async fn drain_connections_times_out_and_aborts_stragglers() {
        let mut conns: JoinSet<()> = JoinSet::new();
        // A task that never finishes on its own forces the timeout + abort path;
        // the only way `drain_connections` can return is by aborting it.
        conns.spawn(std::future::pending::<()>());
        drain_connections(&mut conns, Duration::from_millis(50)).await;
        assert!(
            conns.is_empty(),
            "straggler should have been aborted and joined"
        );
    }

    #[tokio::test]
    async fn note_reaped_ignores_success_and_logs_panic() {
        // A clean exit is a no-op.
        note_reaped(Ok(()));
        // A panicked handler yields a `JoinError` with `is_panic()`, which
        // `note_reaped` logs (and must not propagate).
        let mut js: JoinSet<()> = JoinSet::new();
        js.spawn(async { panic!("boom") });
        let result = js.join_next().await.unwrap();
        assert!(result.is_err());
        note_reaped(result);
    }
}
