//! The daemon server core: bind the control socket, accept NDJSON connections,
//! route envelopes to services (or built-in ops), and shut down gracefully.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::{UnixListener, UnixStream};
use tokio::task::{JoinError, JoinSet};
use tokio_util::codec::{Framed, LinesCodec, LinesCodecError};
use tokio_util::sync::CancellationToken;

use super::lifecycle;
use super::paths;
use super::protocol::{DaemonEnvelope, DaemonReply, StatusReport, DAEMON_SERVICE, MAX_LINE_BYTES};
use super::registry::ServiceRegistry;
use super::service::ServiceStream;
use super::single_instance;

/// How long to wait for accepted-but-unfinished connections to drain on
/// shutdown before aborting the stragglers. Generous enough for a normal
/// in-flight dispatch+reply, bounded so a stuck or idle client cannot hang
/// shutdown indefinitely (a service manager would `SIGKILL` us eventually).
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a push subscription re-samples and diffs its snapshot even without
/// a change notification, so purely on-disk state changes (a branch switch, new
/// commits) — which fire **no** registry event — are still reflected within the
/// interval. Kept in the issue's 2–5 s band (#1267).
const STREAM_TICK: Duration = Duration::from_secs(3);

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

    let (listener, socket_activated) = acquire_listener(&opts.socket_path).await?;
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

    // Close the control socket *before* draining (see #993). The accept loop has
    // already exited, so any `connect`+`ping` arriving during the drain below
    // would otherwise sit unaccepted in the backlog and block the caller until
    // process exit. Dropping the listener makes those connects fail fast
    // (ECONNREFUSED) on the self-bound path.
    //
    // Unlinking the path is conditional. On the self-bound path we remove it here
    // — rather than after the drain — to avoid a restart race: a replacement
    // daemon could reclaim the stale socket and rebind its *own* listener
    // mid-drain, and a late unlink would then delete that fresh socket out from
    // under it. On the socket-activated path the socket inode belongs to the
    // service manager (launchd on macOS, systemd on Linux), not us: unlinking it
    // would make the next `connect(path)` hit ENOENT and never re-activate the
    // daemon — so we leave it in place for the manager to reuse on the next demand
    // spawn (#1081).
    drop(listener);
    if !socket_activated {
        remove_socket(&opts.socket_path);
    }

    // Drain in-flight connection handlers before stopping services (#992).
    drain_connections(&mut conns, DRAIN_TIMEOUT).await;

    tracing::info!("daemon shutting down; draining services");
    registry.shutdown_all().await;
    Ok(())
}

/// Acquires the control-socket listener, returning it alongside whether the
/// service manager owns the socket inode (i.e. the daemon was socket-activated).
///
/// On macOS (launchd) and Linux (systemd) the daemon is normally
/// **socket-activated**: the service manager creates and owns the listening
/// socket and hands us the inherited fd (`launchd::launchd_listener` /
/// `systemd::systemd_listener` — plain code spans, not intra-doc links, since
/// those modules are OS-gated and absent from the cross-platform docs build), so
/// there is no bind and no single-instance handling — the manager guarantees at
/// most one spawn per socket. When that lookup reports no inherited socket (a
/// manual `daemon run` from a shell, CI, the detached-spawn fallback, or any
/// other platform) the daemon binds the socket itself via
/// [`single_instance::bind_or_reclaim`], which doubles as the single-instance
/// lock. The returned bool gates whether shutdown unlinks the path: a
/// manager-owned inode must be left in place to re-activate (#1081).
async fn acquire_listener(socket_path: &Path) -> Result<(UnixListener, bool)> {
    #[cfg(target_os = "macos")]
    if let Some(listener) = super::launchd::launchd_listener("Listener")? {
        tracing::info!("daemon adopting launchd-activated control socket");
        return Ok((listener, true));
    }
    #[cfg(target_os = "linux")]
    if let Some(listener) = super::systemd::systemd_listener()? {
        tracing::info!("daemon adopting systemd-activated control socket");
        return Ok((listener, true));
    }
    let listener = single_instance::bind_or_reclaim(socket_path).await?;
    Ok((listener, false))
}

/// Removes the control-socket file, tolerating its absence (a replacement
/// daemon may have already reclaimed it). Any other error is logged, not fatal.
fn remove_socket(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("failed to remove socket {}: {e}", path.display());
        }
    }
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
/// The normal request→one-reply path has deliberately no `shutdown.cancelled()`
/// arm: an accepted line always finishes its dispatch+reply, and shutdown is
/// handled by the server draining these tasks (see [`drain_connections`]). A
/// **subscription** op is the exception — it takes over the connection via
/// [`run_stream`], which *does* select on `shutdown` so a long-lived stream is
/// torn down promptly on drain rather than waiting out [`DRAIN_TIMEOUT`].
/// `shutdown` is threaded through for both (also the built-in `shutdown` op, see
/// [`handle_builtin`]).
async fn handle_connection(
    stream: UnixStream,
    registry: Arc<ServiceRegistry>,
    shutdown: CancellationToken,
) {
    let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(MAX_LINE_BYTES));
    while let Some(line) = framed.next().await {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                // A decode error ends the `Framed` stream (the next poll yields
                // `None`), so there is nothing more to serve on this connection:
                // reply once (best effort) and close. `MaxLineLengthExceeded`
                // additionally puts the codec in discard mode — the
                // unbounded-growth case the cap exists to stop (#989) — so it
                // gets a clearer message.
                let msg = match e {
                    LinesCodecError::MaxLineLengthExceeded => {
                        format!("request line exceeds the {MAX_LINE_BYTES}-byte limit")
                    }
                    LinesCodecError::Io(io) => format!("read error: {io}"),
                };
                let _ = send_reply(&mut framed, DaemonReply::err(msg)).await;
                break;
            }
        };

        // Parse once, so a subscription op can be detected before it is
        // dispatched as a normal one-reply op. A malformed envelope replies with
        // an error but keeps the connection open, matching the pre-#1267 path.
        let envelope: DaemonEnvelope = match serde_json::from_str(&line) {
            Ok(envelope) => envelope,
            Err(e) => {
                if !send_reply(
                    &mut framed,
                    DaemonReply::err(format!("invalid envelope: {e}")),
                )
                .await
                {
                    break;
                }
                continue;
            }
        };

        // A streaming op takes over the connection for its whole lifetime: it
        // never returns a single reply, so once `run_stream` finishes (client
        // gone or daemon shutting down) the connection is done.
        if let Some(name) = envelope.service.as_deref() {
            if name != DAEMON_SERVICE {
                if let Some(stream) = registry.subscribe(name, &envelope.op, &envelope.payload) {
                    run_stream(&mut framed, stream, &shutdown).await;
                    return;
                }
            }
        }

        let reply = dispatch_envelope(envelope, &registry, &shutdown).await;
        if !send_reply(&mut framed, reply).await {
            break;
        }
    }
}

/// Drives a push subscription over `framed` until the client goes away or the
/// daemon shuts down. Sends an initial snapshot, then re-samples the stream on
/// each change notification and on a periodic [`STREAM_TICK`], pushing **only**
/// snapshots that differ from the last one sent — so identical frames are never
/// duplicated (the acceptance criterion). Mirrors the browser bridge's
/// `start_stream` coalescing shape, but on the control socket.
///
/// The subscription owns the connection for its lifetime: any further inbound
/// line is treated as an explicit cancel and ends the stream, matching the
/// one-op-per-connection the companion uses (a dedicated subscribe socket).
async fn run_stream(
    framed: &mut Framed<UnixStream, LinesCodec>,
    mut stream: Box<dyn ServiceStream>,
    shutdown: &CancellationToken,
) {
    // Initial snapshot up front. The stream's change source was captured when it
    // was built (before this snapshot), so the loop below only pushes deltas —
    // and any change racing this initial sample is caught by the first wakeup.
    let mut last = stream.snapshot().await;
    if !send_reply(framed, DaemonReply::ok(last.clone())).await {
        return;
    }

    // `interval` fires immediately on the first `tick()`; consume that so the
    // periodic re-sample starts one full interval out.
    let mut tick = tokio::time::interval(STREAM_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tick.tick().await;

    loop {
        tokio::select! {
            () = stream.changed() => {}
            _ = tick.tick() => {}
            // Reading `framed` serves double duty and every outcome ends the
            // stream: an inbound line is an explicit cancel, `None` is the client
            // hanging up, and an `Err` is a read/decode error. `Framed`'s decode
            // buffer lives in the codec, not this future, so cancelling this arm
            // mid-poll loses no buffered bytes.
            _ = framed.next() => break,
            () = shutdown.cancelled() => break,
        }
        // Any wakeup means "maybe changed": re-sample and push only a real delta.
        let snap = stream.snapshot().await;
        if snap != last {
            if !send_reply(framed, DaemonReply::ok(snap.clone())).await {
                break;
            }
            last = snap;
        }
    }
}

/// Encodes and writes one reply line. Returns `false` when the connection
/// should be closed (encode failed, or the write failed).
async fn send_reply(framed: &mut Framed<UnixStream, LinesCodec>, reply: DaemonReply) -> bool {
    let encoded = match serde_json::to_string(&reply) {
        Ok(encoded) => encoded,
        Err(e) => {
            tracing::warn!("failed to encode daemon reply: {e}");
            return false;
        }
    };
    if let Err(e) = framed.send(encoded).await {
        tracing::debug!("daemon client write failed: {e}");
        return false;
    }
    true
}

/// Produces the one-reply response for a (already-parsed, non-streaming)
/// request envelope. Streaming ops are peeled off earlier in
/// [`handle_connection`]; everything else routes here.
async fn dispatch_envelope(
    envelope: DaemonEnvelope,
    registry: &ServiceRegistry,
    shutdown: &CancellationToken,
) -> DaemonReply {
    match envelope.service.as_deref() {
        None | Some(DAEMON_SERVICE) => handle_builtin(&envelope.op, registry, shutdown).await,
        Some(name) => {
            // Correlate any HTTP the service issues to the originating client's
            // invocation, when it threaded its id across the socket (#1198).
            // Built-in ops issue no HTTP, so only the service path is scoped.
            let dispatch = registry.dispatch(name, &envelope.op, envelope.payload);
            let result = match envelope.origin_invocation_id {
                Some(origin) => crate::request_log::scope_origin_id(origin, dispatch).await,
                None => dispatch.await,
            };
            match result {
                Ok(payload) => DaemonReply::ok(payload),
                // `{:#}` includes the full anyhow source chain (e.g. "Snowflake
                // query failed: snowflake server error (000630): …") so the
                // client can see the underlying cause, not just the top-level
                // wrapper.
                Err(e) => DaemonReply::err(format!("{e:#}")),
            }
        }
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

    // --- Push-subscription streaming (#1267) --------------------------------
    //
    // `UnixStream::pair()` is an unbound, connected socket pair — no `bind`, so
    // no umask mutation — so these `run_stream` tests stay here (in-process)
    // rather than in the socket-binding `tests/daemon_socket.rs` binary.

    use std::sync::Mutex as StdMutex;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::sync::watch;

    /// A controllable [`ServiceStream`] for driving `run_stream` directly: the
    /// test bumps `tx` to wake it and swaps `snap` to change what it reports.
    struct FakeStream {
        rx: watch::Receiver<u64>,
        snap: Arc<StdMutex<serde_json::Value>>,
    }

    #[async_trait::async_trait]
    impl ServiceStream for FakeStream {
        async fn changed(&mut self) {
            // Mirror the real impl: park (rather than spin) once the sender drops.
            if self.rx.changed().await.is_err() {
                std::future::pending::<()>().await;
            }
        }
        async fn snapshot(&self) -> serde_json::Value {
            self.snap.lock().unwrap().clone()
        }
    }

    /// Reads one NDJSON reply line from the client end, asserting it is not EOF.
    /// Generic over the reader so it works on both an owned `BufReader<UnixStream>`
    /// and one wrapping a `&mut UnixStream` (test 2 keeps the stream to write to).
    async fn read_reply<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> DaemonReply {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.unwrap();
        assert!(n > 0, "expected a reply line, got EOF");
        serde_json::from_str(line.trim_end()).unwrap()
    }

    #[tokio::test]
    async fn run_stream_pushes_initial_then_deltas_and_dedupes() {
        let (client, server) = UnixStream::pair().unwrap();
        let (tx, rx) = watch::channel(0u64);
        let snap = Arc::new(StdMutex::new(json!({ "n": 0 })));
        let fake = FakeStream {
            rx,
            snap: snap.clone(),
        };
        let shutdown = CancellationToken::new();
        let server_shutdown = shutdown.clone();

        let server_task = tokio::spawn(async move {
            let mut framed = Framed::new(server, LinesCodec::new_with_max_length(MAX_LINE_BYTES));
            run_stream(&mut framed, Box::new(fake), &server_shutdown).await;
        });

        let mut reader = BufReader::new(client);

        // 1) The initial snapshot is pushed up front.
        let initial = read_reply(&mut reader).await;
        assert!(initial.ok);
        assert_eq!(initial.payload, json!({ "n": 0 }));

        // 2) A wake whose snapshot is unchanged is NOT re-sent (the diff dedupes).
        //    Then a real change is. Because the next frame we read is the changed
        //    one, a spurious duplicate of `{n:0}` would fail this assertion.
        tx.send(1).unwrap(); // wake; snapshot still {n:0} → suppressed
        *snap.lock().unwrap() = json!({ "n": 1 });
        tx.send(2).unwrap(); // wake; snapshot now {n:1} → pushed
        let delta = read_reply(&mut reader).await;
        assert_eq!(delta.payload, json!({ "n": 1 }));

        // 3) Shutdown tears the stream down cleanly: the client hits EOF.
        shutdown.cancel();
        let mut tail = String::new();
        let n = reader.read_line(&mut tail).await.unwrap();
        assert_eq!(n, 0, "stream should close cleanly on shutdown");
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn run_stream_ends_when_client_sends_a_line() {
        use tokio::io::AsyncWriteExt;

        let (mut client, server) = UnixStream::pair().unwrap();
        let (_tx, rx) = watch::channel(0u64);
        let snap = Arc::new(StdMutex::new(json!({ "n": 0 })));
        let fake = FakeStream { rx, snap };
        let shutdown = CancellationToken::new();
        let server_shutdown = shutdown.clone();

        let server_task = tokio::spawn(async move {
            let mut framed = Framed::new(server, LinesCodec::new_with_max_length(MAX_LINE_BYTES));
            run_stream(&mut framed, Box::new(fake), &server_shutdown).await;
        });

        let mut reader = BufReader::new(&mut client);
        let _initial = read_reply(&mut reader).await;
        // Release the borrow of `client` so it can be written to below.
        drop(reader);

        // Any inbound line is a cancel: the stream ends and the task completes
        // even though shutdown was never signalled.
        client.write_all(b"cancel\n").await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("run_stream should end after a client line")
            .unwrap();
    }

    /// `handle_connection`'s parse/route path: a malformed envelope replies with
    /// an error but keeps the connection open, and a well-formed non-subscribe op
    /// then falls through the streaming check to the normal one-reply dispatch.
    #[tokio::test]
    async fn handle_connection_rejects_bad_envelope_then_serves_normal_op() {
        use tokio::io::AsyncWriteExt;

        let (client, server) = UnixStream::pair().unwrap();
        let mut registry = ServiceRegistry::new();
        registry.register(Arc::new(
            crate::daemon::services::worktrees::WorktreesService::new(),
        ));
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(handle_connection(server, Arc::new(registry), shutdown));

        let (read_half, mut write_half) = client.into_split();
        let mut reader = BufReader::new(read_half);

        // 1) A syntactically invalid line → error reply; the connection stays up.
        write_half.write_all(b"not json\n").await.unwrap();
        let bad = read_reply(&mut reader).await;
        assert!(!bad.ok);
        assert!(bad.error.unwrap().contains("invalid envelope"));

        // 2) A well-formed non-subscribe op is served on the same connection
        //    (the streaming check declines `list`, so it dispatches normally).
        let env = serde_json::to_string(&DaemonEnvelope::service(
            "worktrees",
            "list",
            serde_json::Value::Null,
        ))
        .unwrap();
        write_half.write_all(env.as_bytes()).await.unwrap();
        write_half.write_all(b"\n").await.unwrap();
        let listed = read_reply(&mut reader).await;
        assert!(listed.ok);
        assert!(listed.payload.get("windows").is_some());

        // Client hangs up → the handler task ends cleanly.
        drop(write_half);
        drop(reader);
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("handler should end after the client hangs up")
            .unwrap();
    }

    /// `handle_connection` routes a `subscribe` op into streaming mode: the
    /// client gets the pushed initial snapshot, and daemon shutdown ends both the
    /// stream and the handler task.
    #[tokio::test]
    async fn handle_connection_enters_streaming_for_subscribe() {
        use tokio::io::AsyncWriteExt;

        let (client, server) = UnixStream::pair().unwrap();
        let mut registry = ServiceRegistry::new();
        registry.register(Arc::new(
            crate::daemon::services::worktrees::WorktreesService::new(),
        ));
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(handle_connection(
            server,
            Arc::new(registry),
            shutdown.clone(),
        ));

        let (read_half, mut write_half) = client.into_split();
        let mut reader = BufReader::new(read_half);
        let env = serde_json::to_string(&DaemonEnvelope::service(
            "worktrees",
            "subscribe",
            serde_json::Value::Null,
        ))
        .unwrap();
        write_half.write_all(env.as_bytes()).await.unwrap();
        write_half.write_all(b"\n").await.unwrap();

        // The subscription pushes an initial snapshot (no windows → empty repos),
        // with the show/hide-closed toggle at its default (show all).
        let initial = read_reply(&mut reader).await;
        assert!(initial.ok);
        assert_eq!(initial.payload, json!({ "repos": [], "show_closed": true }));

        // Shutdown ends the stream and the handler task.
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(2), task)
            .await
            .expect("shutdown should end the streaming handler")
            .unwrap();
    }

    /// `run_stream` returns immediately when even the initial snapshot cannot be
    /// sent (the client is already gone) rather than entering the select loop.
    #[tokio::test]
    async fn run_stream_returns_when_initial_send_fails() {
        let (client, server) = UnixStream::pair().unwrap();
        // Close the peer before `run_stream` writes, so the first send fails.
        drop(client);
        let (_tx, rx) = watch::channel(0u64);
        let fake = FakeStream {
            rx,
            snap: Arc::new(StdMutex::new(json!({ "n": 0 }))),
        };
        let shutdown = CancellationToken::new();
        let mut framed = Framed::new(server, LinesCodec::new_with_max_length(MAX_LINE_BYTES));
        tokio::time::timeout(
            Duration::from_secs(2),
            run_stream(&mut framed, Box::new(fake), &shutdown),
        )
        .await
        .expect("run_stream should return promptly when the initial send fails");
    }
}
