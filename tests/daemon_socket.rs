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
//! Within *this* binary the few tests below still share one process, so each
//! restores its own temp dir to `0700` via [`tempdir_0700`] before binding.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use omni_dev::daemon::client::DaemonClient;
use omni_dev::daemon::protocol::DaemonEnvelope;
use omni_dev::daemon::registry::ServiceRegistry;
use omni_dev::daemon::server::{run, DaemonOptions};
use omni_dev::daemon::services::echo::EchoService;
use omni_dev::daemon::single_instance::{bind_or_reclaim, bind_private};
use tempfile::TempDir;

/// Creates a temp dir and force-restores its mode to `0700`.
///
/// A sibling test in this binary may have tightened the process-global umask
/// (via a concurrent `bind_private`) at the instant `tempdir()` created this
/// dir, stripping its owner **search** bit and leaving it `0600` — which would
/// then fail any socket/file creation inside it with `EACCES`. Restoring `0700`
/// up front closes that intra-binary race.
fn tempdir_0700() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// `bind_private` alone — with no follow-up `chmod` — must yield a `0600`
/// socket, proving the umask closes the window rather than a post-bind
/// `set_file_0600`. Needs a Tokio runtime to register the listener fd.
#[tokio::test]
async fn bind_private_creates_an_owner_only_socket() {
    let dir = tempdir_0700();
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
    let dir = tempdir_0700();
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

#[tokio::test]
async fn second_bind_is_refused_while_first_is_live() {
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
    let dir = tempdir_0700();
    let socket = dir.path().join("d.sock");
    // A leftover regular file at the socket path stands in for a stale
    // socket: nothing is listening, so the ping probe fails and we reclaim.
    std::fs::write(&socket, b"stale").unwrap();
    let listener = bind_or_reclaim(&socket).await.unwrap();
    drop(listener);
}
