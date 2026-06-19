//! The daemon server core: bind the control socket, accept NDJSON connections,
//! route envelopes to services (or built-in ops), and shut down gracefully.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LinesCodec};
use tokio_util::sync::CancellationToken;

use super::lifecycle;
use super::paths;
use super::protocol::{DaemonEnvelope, DaemonReply, StatusReport, DAEMON_SERVICE};
use super::registry::ServiceRegistry;
use super::single_instance;

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

    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        tokio::spawn(handle_connection(
                            stream,
                            registry.clone(),
                            shutdown.clone(),
                        ));
                    }
                    Err(e) => tracing::warn!("daemon accept error: {e}"),
                }
            }
        }
    }

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

/// Serves one client connection: decode each NDJSON line, dispatch it, and
/// write back one reply line, until the client hangs up or shutdown fires.
async fn handle_connection(
    stream: UnixStream,
    registry: Arc<ServiceRegistry>,
    shutdown: CancellationToken,
) {
    let mut framed = Framed::new(stream, LinesCodec::new());
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            line = framed.next() => {
                let Some(line) = line else { break };
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::daemon::services::echo::EchoService;
    use serde_json::Value;
    use std::time::Duration;

    /// Spins up a daemon on a temp socket, runs `body`, then shuts it down and
    /// joins the server task.
    async fn with_daemon<F, Fut>(body: F)
    where
        F: FnOnce(PathBuf) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("d.sock");
        let mut registry = ServiceRegistry::new();
        registry.register(Arc::new(EchoService));
        let opts = DaemonOptions {
            socket_path: socket.clone(),
        };
        let handle = tokio::spawn(run(registry, opts));

        // Wait for the socket to accept.
        let client = crate::daemon::client::DaemonClient::new(&socket);
        for _ in 0..50 {
            if client.ping().await.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        body(socket.clone()).await;

        client.shutdown().await.ok();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn ping_and_status_and_routing() {
        with_daemon(|socket| async move {
            let client = crate::daemon::client::DaemonClient::new(&socket);

            // Built-in ping.
            client.ping().await.unwrap();

            // Built-in status reports the echo service.
            let report = client.status().await.unwrap();
            assert_eq!(report.services.len(), 1);
            assert_eq!(report.services[0].name, "echo");
            assert!(report.services[0].healthy);

            // Routed op reaches the echo service and round-trips the payload.
            let reply = client
                .request(DaemonEnvelope::service(
                    "echo",
                    "echo",
                    json!({ "hello": "world" }),
                ))
                .await
                .unwrap();
            assert!(reply.ok);
            assert_eq!(reply.payload, json!({ "hello": "world" }));

            // Unknown service is an error, not a panic.
            let reply = client
                .request(DaemonEnvelope::service("nope", "x", Value::Null))
                .await
                .unwrap();
            assert!(!reply.ok);
            assert!(reply.error.unwrap().contains("unknown service"));

            // Unknown built-in op is an error.
            let reply = client
                .request(DaemonEnvelope::builtin("frobnicate"))
                .await
                .unwrap();
            assert!(!reply.ok);
        })
        .await;
    }

    #[tokio::test]
    async fn second_bind_is_refused_while_first_is_live() {
        with_daemon(|socket| async move {
            let mut registry = ServiceRegistry::new();
            registry.register(Arc::new(EchoService));
            let err = single_instance::bind_or_reclaim(&socket).await.unwrap_err();
            assert!(err.to_string().contains("already running"));
        })
        .await;
    }

    #[tokio::test]
    async fn stale_socket_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("d.sock");
        // A leftover regular file at the socket path stands in for a stale
        // socket: nothing is listening, so the ping probe fails and we reclaim.
        std::fs::write(&socket, b"stale").unwrap();
        let listener = single_instance::bind_or_reclaim(&socket).await.unwrap();
        drop(listener);
    }
}
