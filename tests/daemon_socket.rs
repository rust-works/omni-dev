//! Socket-binding daemon tests, isolated in their own process.
//!
//! [`bind_private`](omni_dev::daemon::single_instance::bind_private) (reached
//! here directly and via [`bind_or_reclaim`]/[`run`]) tightens the
//! **process-global** umask for the synchronous span of its socket `bind`
//! (#995). That write is safe in production — a one-shot startup bind with no
//! concurrent file creation — but in a parallel test binary it races *any*
//! other test creating a file in the same instant, stripping that file/dir of
//! permission bits and failing it with `EACCES`. The library's unit-test binary
//! runs thousands of such tests concurrently, so these umask-mutating tests live
//! here instead: a separate integration-test binary is a separate process, and
//! umask is per-process, so this binary's umask windows never touch the unit
//! tests. See issue #1017.
//!
//! Within *this* binary the tests below still share one process, and `umask` is
//! process-global, so every socket-binding test holds [`UMASK_GATE`] for its
//! whole body. That serializes the `bind_private` umask save/restore spans,
//! closing both the socket-mode race (#1023) and the intra-binary temp-dir race
//! a per-test `0700` restore used to guard.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::Notify;

use clap::Parser as _;
use omni_dev::cli::worktrees::{ListCommand, WorktreesCommand, WorktreesSubcommands};
use omni_dev::cli::Cli;
use omni_dev::daemon::client::DaemonClient;
use omni_dev::daemon::protocol::{DaemonEnvelope, DaemonReply, MAX_LINE_BYTES};
use omni_dev::daemon::registry::ServiceRegistry;
use omni_dev::daemon::server::{run, DaemonOptions};
use omni_dev::daemon::service::{DaemonService, MenuSnapshot, ServiceStatus};
use omni_dev::daemon::services::echo::EchoService;
use omni_dev::daemon::services::worktrees::WorktreesService;
use omni_dev::daemon::single_instance::{bind_or_reclaim, bind_private};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

/// Serializes every umask-mutating test in this binary.
///
/// `bind_private` (reached directly, or via `bind_or_reclaim`/`run`) saves the
/// process-global umask, tightens it to `0o177`, binds, then restores the prior
/// value on guard drop. Two such save/restore spans that interleave corrupt the
/// shared umask — one span restores the *other's* already-tightened value — so a
/// later `bind` yields a `0755` socket instead of `0600` (#1023), or a `tempdir`
/// loses its owner **search** bit (the intra-binary directory race #1017 first
/// surfaced). Each socket-binding test holds this gate for its whole body, so no
/// two umask spans ever overlap. The tests are few and fast, so full
/// serialization is effectively free; an async `Mutex` lets the `#[tokio::test]`
/// bodies hold it across their `.await`s.
static UMASK_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// `bind_private` alone — with no follow-up `chmod` — must yield a `0600`
/// socket, proving the umask closes the window rather than a post-bind
/// `set_file_0600`. Needs a Tokio runtime to register the listener fd.
#[tokio::test]
async fn bind_private_creates_an_owner_only_socket() {
    let _gate = UMASK_GATE.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");
    let listener = bind_private(&socket).unwrap();
    let mode = std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "socket mode was {mode:o}, expected 600");
    drop(listener);
}

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
    let client = DaemonClient::new(&socket);
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
    let _gate = UMASK_GATE.lock().await;
    with_daemon(|socket| async move {
        let client = DaemonClient::new(&socket);

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

/// The worktrees service round-trips its register/heartbeat/list/unregister ops
/// over the real control socket: a window registers, appears in `list`,
/// heartbeats as `known`, then unregisters and is gone. This proves the wire
/// routing end to end; TTL reaping is covered by the service's own unit tests.
#[tokio::test]
async fn worktrees_register_list_unregister_round_trip() {
    let _gate = UMASK_GATE.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");
    let mut registry = ServiceRegistry::new();
    registry.register(Arc::new(WorktreesService::new()));
    let opts = DaemonOptions {
        socket_path: socket.clone(),
    };
    let handle = tokio::spawn(run(registry, opts));

    let client = DaemonClient::new(&socket);
    for _ in 0..50 {
        if client.ping().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // A window registers itself.
    let reply = client
        .request(DaemonEnvelope::service(
            "worktrees",
            "register",
            json!({
                "key": "win-1",
                "folders": ["/tmp/project"],
                "repo": "project",
                "title": "project — main",
                "pid": 4321,
            }),
        ))
        .await
        .unwrap();
    assert!(reply.ok, "register failed: {:?}", reply.error);

    // It shows up in the live cross-window list.
    let reply = client
        .request(DaemonEnvelope::service("worktrees", "list", Value::Null))
        .await
        .unwrap();
    assert!(reply.ok);
    let windows = reply.payload["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 1);
    assert_eq!(windows[0]["key"], json!("win-1"));
    assert_eq!(windows[0]["repo"], json!("project"));

    // A heartbeat for a registered window is `known`.
    let reply = client
        .request(DaemonEnvelope::service(
            "worktrees",
            "heartbeat",
            json!({ "key": "win-1" }),
        ))
        .await
        .unwrap();
    assert_eq!(reply.payload, json!({ "known": true }));

    // The built-in status aggregates the service summary.
    let report = client.status().await.unwrap();
    assert_eq!(report.services.len(), 1);
    assert_eq!(report.services[0].name, "worktrees");
    assert_eq!(report.services[0].summary, "1 window(s) across 1 repo(s)");

    // The window closes and is removed.
    let reply = client
        .request(DaemonEnvelope::service(
            "worktrees",
            "unregister",
            json!({ "key": "win-1" }),
        ))
        .await
        .unwrap();
    assert_eq!(reply.payload, json!({ "removed": true }));

    let reply = client
        .request(DaemonEnvelope::service("worktrees", "list", Value::Null))
        .await
        .unwrap();
    assert_eq!(reply.payload, json!({ "windows": [] }));

    client.shutdown().await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}

/// Drives the real `omni-dev worktrees` CLI command structs against a live
/// daemon: the table path, the `--json` path, the top-level `Cli` dispatch, and
/// the daemon-down error path. This is the only way to cover the CLI's
/// socket-connecting code (it cannot run in the lib unit-test binary, which must
/// not bind sockets — see the module header).
#[tokio::test]
async fn worktrees_cli_list_against_live_daemon() {
    let _gate = UMASK_GATE.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");
    let mut registry = ServiceRegistry::new();
    registry.register(Arc::new(WorktreesService::new()));
    let handle = tokio::spawn(run(
        registry,
        DaemonOptions {
            socket_path: socket.clone(),
        },
    ));

    let client = DaemonClient::new(&socket);
    for _ in 0..50 {
        if client.ping().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Seed one window so the table path has a row to render.
    client
        .request(DaemonEnvelope::service(
            "worktrees",
            "register",
            json!({ "key": "k1", "folders": ["/tmp/p"], "repo": "p", "title": "p" }),
        ))
        .await
        .unwrap();

    // Table path (json = false) and JSON path (json = true).
    ListCommand {
        socket: Some(socket.clone()),
        json: false,
    }
    .execute()
    .await
    .unwrap();
    ListCommand {
        socket: Some(socket.clone()),
        json: true,
    }
    .execute()
    .await
    .unwrap();

    // Through the subcommand wrapper.
    WorktreesCommand {
        command: WorktreesSubcommands::List(ListCommand {
            socket: Some(socket.clone()),
            json: false,
        }),
    }
    .execute()
    .await
    .unwrap();

    // Through the top-level CLI parse + dispatch (covers the dispatch arm).
    let sock_str = socket.to_str().unwrap();
    Cli::try_parse_from([
        "omni-dev",
        "worktrees",
        "list",
        "--socket",
        sock_str,
        "--json",
    ])
    .unwrap()
    .execute()
    .await
    .unwrap();

    client.shutdown().await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;

    // With the daemon down, the CLI surfaces the connection failure.
    let missing = dir.path().join("absent.sock");
    assert!(ListCommand {
        socket: Some(missing),
        json: false,
    }
    .execute()
    .await
    .is_err());
}

#[tokio::test]
async fn second_bind_is_refused_while_first_is_live() {
    let _gate = UMASK_GATE.lock().await;
    with_daemon(|socket| async move {
        let mut registry = ServiceRegistry::new();
        registry.register(Arc::new(EchoService));
        let err = bind_or_reclaim(&socket).await.unwrap_err();
        assert!(err.to_string().contains("already running"));
    })
    .await;
}

#[tokio::test]
async fn stale_socket_is_reclaimed() {
    let _gate = UMASK_GATE.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");
    // A leftover regular file at the socket path stands in for a stale
    // socket: nothing is listening, so the ping probe fails and we reclaim.
    std::fs::write(&socket, b"stale").unwrap();
    let listener = bind_or_reclaim(&socket).await.unwrap();
    drop(listener);
}

/// A service whose `slow` op blocks until released, then echoes. It signals when
/// its handler has *started* (so the test can guarantee the request is genuinely
/// in-flight before shutting down) and records when it has *completed*, standing
/// in for in-flight work that must be drained rather than abandoned on shutdown.
struct SlowService {
    started: Arc<Notify>,
    release: Arc<Notify>,
    completed: Arc<AtomicBool>,
}

#[async_trait]
impl DaemonService for SlowService {
    fn name(&self) -> &'static str {
        "slow"
    }
    async fn handle(&self, op: &str, payload: Value) -> Result<Value> {
        match op {
            "slow" => {
                self.started.notify_one();
                self.release.notified().await;
                self.completed.store(true, Ordering::SeqCst);
                Ok(payload)
            }
            other => anyhow::bail!("unknown slow op: {other}"),
        }
    }
    fn menu(&self) -> MenuSnapshot {
        MenuSnapshot::default()
    }
    async fn menu_action(&self, _action_id: &str) -> Result<()> {
        Ok(())
    }
    async fn status(&self) -> ServiceStatus {
        ServiceStatus {
            name: self.name().to_string(),
            healthy: true,
            summary: "ready".to_string(),
            detail: Value::Null,
        }
    }
    async fn shutdown(&self) {}
}

/// An accepted request still mid-`handle()` when shutdown fires must be drained,
/// not abandoned: `run()` waits for it to finish before returning (#992).
#[tokio::test]
async fn in_flight_request_is_drained_on_shutdown() {
    let _gate = UMASK_GATE.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");

    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let completed = Arc::new(AtomicBool::new(false));

    let mut registry = ServiceRegistry::new();
    registry.register(Arc::new(SlowService {
        started: started.clone(),
        release: release.clone(),
        completed: completed.clone(),
    }));
    let opts = DaemonOptions {
        socket_path: socket.clone(),
    };
    let mut server = tokio::spawn(run(registry, opts));

    // Wait for the socket to accept.
    let client = DaemonClient::new(&socket);
    for _ in 0..50 {
        if client.ping().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Fire a slow request and wait until it is genuinely in `handle()`.
    let slow = tokio::spawn({
        let socket = socket.clone();
        async move {
            DaemonClient::new(&socket)
                .request(DaemonEnvelope::service("slow", "slow", json!({ "v": 1 })))
                .await
        }
    });
    started.notified().await;

    // Ask the daemon to shut down while that request is mid-`handle()`.
    DaemonClient::new(&socket).shutdown().await.ok();

    // `run()` must not return while the request is still in flight: it is
    // draining, not abandoning it. Pre-fix, the handler was a detached task and
    // `run()` returned at once, so this `timeout` would observe it already done.
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut server)
            .await
            .is_err(),
        "run() returned before the in-flight request was drained"
    );
    assert!(!completed.load(Ordering::SeqCst));

    // Release the handler: the drain completes, the reply is delivered, and only
    // then does the server stop.
    release.notify_one();
    let reply = slow.await.unwrap().unwrap();
    assert!(reply.ok);
    assert_eq!(reply.payload, json!({ "v": 1 }));
    assert!(completed.load(Ordering::SeqCst));
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .expect("server should stop after draining")
        .expect("server task should not panic")
        .expect("run() should return Ok");
}

/// A line that is not valid UTF-8 fails the `LinesCodec` decode; the connection
/// handler must answer with a `read error` reply rather than drop the client
/// silently.
#[tokio::test]
async fn invalid_utf8_line_yields_read_error_reply() {
    let _gate = UMASK_GATE.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");
    let mut registry = ServiceRegistry::new();
    registry.register(Arc::new(EchoService));
    let opts = DaemonOptions {
        socket_path: socket.clone(),
    };
    let server = tokio::spawn(run(registry, opts));

    let client = DaemonClient::new(&socket);
    for _ in 0..50 {
        if client.ping().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Raw connection so we can send bytes the JSON client never would.
    let mut stream = UnixStream::connect(&socket).await.unwrap();
    let mut line = String::new();
    {
        stream.write_all(b"\xff\xff\n").await.unwrap();
        let mut reader = BufReader::new(&mut stream);
        reader.read_line(&mut line).await.unwrap();
    }
    let reply: Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(reply["ok"], json!(false));
    assert!(reply["error"].as_str().unwrap().contains("read error"));

    // Drop the raw connection before shutdown so the drain does not wait on it.
    drop(stream);
    client.shutdown().await.ok();
    let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
}

/// A request line past `MAX_LINE_BYTES` must get one error reply and then have
/// the connection closed, rather than growing the read buffer without bound
/// (#989) or looping in the codec's post-overflow discard mode.
#[tokio::test]
async fn over_limit_line_is_rejected_and_closes() {
    let _gate = UMASK_GATE.lock().await;
    with_daemon(|socket| async move {
        let mut stream = UnixStream::connect(&socket).await.unwrap();

        // One byte past the cap with no newline: the unbounded-growth attack
        // the cap exists to stop.
        let payload = vec![b'x'; MAX_LINE_BYTES + 1];
        stream.write_all(&payload).await.unwrap();
        stream.flush().await.unwrap();

        let mut reader = BufReader::new(stream);

        // The daemon replies once with an error naming the limit.
        let mut line = String::new();
        let n = reader.read_line(&mut line).await.unwrap();
        assert!(n > 0, "expected an error reply line");
        let reply: DaemonReply = serde_json::from_str(line.trim_end()).unwrap();
        assert!(!reply.ok);
        assert!(reply.error.unwrap().contains("limit"));

        // Then it closes the connection rather than entering an error storm.
        line.clear();
        let n = reader.read_line(&mut line).await.unwrap();
        assert_eq!(n, 0, "daemon should close after an over-limit line");
    })
    .await;
}

/// If the client hangs up before reading, the daemon's reply write fails
/// (BrokenPipe) and that connection is closed cleanly — the daemon keeps
/// serving other clients rather than wedging (#989).
#[tokio::test]
async fn client_hangup_before_reply_keeps_daemon_serving() {
    let _gate = UMASK_GATE.lock().await;
    with_daemon(|socket| async move {
        {
            let mut stream = UnixStream::connect(&socket).await.unwrap();
            let env = serde_json::to_string(&DaemonEnvelope::builtin("ping")).unwrap();
            stream.write_all(env.as_bytes()).await.unwrap();
            stream.write_all(b"\n").await.unwrap();
            stream.flush().await.unwrap();
            // Dropped here without reading the reply: the daemon's write hits a
            // closed peer, exercising the write-failure close path.
        }

        // The daemon must still answer a fresh client.
        let client = DaemonClient::new(&socket);
        client.ping().await.unwrap();
    })
    .await;
}

/// A service whose `shutdown` blocks until released, so a test can hold the
/// daemon in its drain window and probe behaviour there.
struct GatedService {
    /// Notified the instant `shutdown` starts draining.
    entered: Arc<Notify>,
    /// `shutdown` parks on this until the test lets the drain finish.
    release: Arc<Notify>,
}

#[async_trait]
impl DaemonService for GatedService {
    fn name(&self) -> &'static str {
        "gated"
    }
    async fn handle(&self, _op: &str, payload: Value) -> Result<Value> {
        Ok(payload)
    }
    fn menu(&self) -> MenuSnapshot {
        MenuSnapshot::default()
    }
    async fn menu_action(&self, _action_id: &str) -> Result<()> {
        Ok(())
    }
    async fn status(&self) -> ServiceStatus {
        ServiceStatus {
            name: self.name().to_string(),
            healthy: true,
            summary: "gated".to_string(),
            detail: Value::Null,
        }
    }
    async fn shutdown(&self) {
        self.entered.notify_one();
        self.release.notified().await;
    }
}

/// Regression test for #993: once the accept loop breaks, the listener must be
/// closed *before* draining, so a stray ping during a slow drain fails fast
/// instead of sitting unaccepted until process exit.
#[tokio::test]
async fn stray_ping_fails_fast_while_draining() {
    let _gate = UMASK_GATE.lock().await;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("d.sock");

    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());

    let mut registry = ServiceRegistry::new();
    registry.register(Arc::new(GatedService {
        entered: entered.clone(),
        release: release.clone(),
    }));
    let opts = DaemonOptions {
        socket_path: socket.clone(),
    };
    let handle = tokio::spawn(run(registry, opts));

    // Wait until the daemon is accepting.
    let client = DaemonClient::new(&socket);
    for _ in 0..50 {
        if client.ping().await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    client.ping().await.unwrap();

    // Ask it to stop; the gated service blocks the drain so we stay inside the
    // shutdown window.
    client.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .expect("drain should begin");

    // The listener is now closed, so a stray ping must fail fast rather than sit
    // unaccepted until process exit. Before the fix the connect succeeded but was
    // never accepted, so this `ping` hung and the timeout elapsed (`Err(Elapsed)`)
    // instead of resolving to a connection error.
    let probe = tokio::time::timeout(Duration::from_millis(500), client.ping()).await;
    assert!(
        matches!(probe, Ok(Err(_))),
        "stray ping during drain should fail fast, got {probe:?}"
    );

    // Let the drain complete and the server task return cleanly.
    release.notify_one();
    let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
}
