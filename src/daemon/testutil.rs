//! Test-only helpers for exercising the daemon control socket from unit tests.
//!
//! Compiled only under `cfg(test)` (and Unix, since the control plane is an
//! `AF_UNIX` socket). Shared by the thin-client tests across `cli::daemon`,
//! `daemon::client`, and friends so the one-shot fake-daemon harness is not
//! duplicated per module.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use tempfile::TempDir;
use tokio::task::JoinHandle;

use super::service::{DaemonService, MenuSnapshot, ServiceStatus};

/// A [`DaemonService`] that serves nothing of substance and records every accept
/// outage the server credits it (#2111), so tests can see what the accept loop
/// reported. [`StubService::slow`] makes it take its time answering a request.
pub(crate) struct StubService {
    name: &'static str,
    delay: Duration,
    credited: Mutex<Vec<Duration>>,
}

impl StubService {
    /// A service that answers every request at once.
    pub(crate) fn new(name: &'static str) -> Self {
        Self::slow(name, Duration::ZERO)
    }

    /// A service that takes `delay` to answer every request.
    pub(crate) fn slow(name: &'static str, delay: Duration) -> Self {
        Self {
            name,
            delay,
            credited: Mutex::new(Vec::new()),
        }
    }

    /// Every outage credited so far, in order.
    pub(crate) fn credited(&self) -> Vec<Duration> {
        self.credited.lock().unwrap().clone()
    }
}

#[async_trait]
impl DaemonService for StubService {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn handle(&self, _op: &str, _payload: Value) -> Result<Value> {
        tokio::time::sleep(self.delay).await;
        Ok(serde_json::json!({ "done": true }))
    }

    fn menu(&self) -> MenuSnapshot {
        MenuSnapshot {
            title: self.name.to_string(),
            items: vec![],
        }
    }

    async fn menu_action(&self, _action_id: &str) -> Result<()> {
        Ok(())
    }

    async fn status(&self) -> ServiceStatus {
        ServiceStatus {
            name: self.name.to_string(),
            healthy: true,
            summary: String::new(),
            detail: Value::Null,
        }
    }

    async fn shutdown(&self) {}

    fn credit_accept_outage(&self, outage: Duration) {
        self.credited.lock().unwrap().push(outage);
    }
}

/// Spawns a one-shot fake daemon on a short-path Unix socket that reads exactly
/// one request line and replies with `reply` (a full `DaemonReply`-shaped JSON
/// value). Returns the temp dir (keep it alive for the socket's lifetime), the
/// socket path, and the server task (await it to assert the exchange completed).
///
/// A short `/tmp` base path keeps the socket under the 104-byte `sockaddr_un`
/// limit that a long `TMPDIR` would otherwise blow.
pub(crate) fn fake_daemon_reply(reply: Value) -> (TempDir, PathBuf, JoinHandle<()>) {
    fake_daemon_replies(vec![reply])
}

/// Like [`fake_daemon_reply`], but serves one request per entry of `replies`, in
/// order, each on its own connection — what a client that connects afresh for
/// every request (`call_service`) sees. For tests that need a request to fail and
/// a later one to succeed against the same socket path. It serves exactly
/// `replies.len()` connections, so a test that makes more or fewer asks than it
/// scripted fails on a refused connection or an unawaited server task.
pub(crate) fn fake_daemon_replies(replies: Vec<Value>) -> (TempDir, PathBuf, JoinHandle<()>) {
    use futures::{SinkExt, StreamExt};
    use tokio::net::UnixListener;
    use tokio_util::codec::{Framed, LinesCodec};

    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let sock = dir.path().join("d.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let server = tokio::spawn(async move {
        for reply in replies {
            let (stream, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(stream, LinesCodec::new());
            let _req = framed.next().await.unwrap().unwrap();
            framed
                .send(serde_json::to_string(&reply).unwrap())
                .await
                .unwrap();
        }
    });
    (dir, sock, server)
}

/// Spawns a fake daemon that reads one request line, then pushes each value in
/// `replies` as its own NDJSON line (each a full `DaemonReply`-shaped JSON), and
/// finally closes the connection — modelling a streaming subscription that ends.
/// The client sees the frames in order followed by EOF (its `next()` returns
/// `None`). Uses the same short-path `/tmp` socket as [`fake_daemon_reply`].
pub(crate) fn fake_daemon_stream(replies: Vec<Value>) -> (TempDir, PathBuf, JoinHandle<()>) {
    use futures::{SinkExt, StreamExt};
    use tokio::net::UnixListener;
    use tokio_util::codec::{Framed, LinesCodec};

    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let sock = dir.path().join("d.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut framed = Framed::new(stream, LinesCodec::new());
        let _req = framed.next().await.unwrap().unwrap();
        for reply in replies {
            framed
                .send(serde_json::to_string(&reply).unwrap())
                .await
                .unwrap();
        }
        // Dropping `framed` (and `listener`) closes the connection: the client's
        // stream reader then sees EOF and ends the subscription.
    });
    (dir, sock, server)
}

/// Like [`fake_daemon_stream`], but holds the connection open after sending
/// `replies` until `close` is dropped or fired, instead of closing
/// immediately.
///
/// A caller driving a reconnect-supervisor (one that treats "connection
/// closed" as "go reconnect") over a plain [`fake_daemon_stream`] cannot
/// reliably observe a pushed frame as a *stable* value — the moment the
/// one-shot fake closes, the supervisor immediately moves on to its next
/// state (a reconnect attempt), and on a single-threaded runtime that whole
/// disconnect-and-retry sequence can run to completion before a consumer
/// polling e.g. a `tokio::sync::watch::Receiver` is ever scheduled, so it
/// observes only the post-disconnect state, never the frame itself. Holding
/// the connection open gives the test explicit control over when the
/// disconnect happens, so it can assert on the stable, pre-disconnect state
/// first.
pub(crate) fn fake_daemon_stream_hold_open(
    replies: Vec<Value>,
) -> (
    TempDir,
    PathBuf,
    tokio::sync::oneshot::Sender<()>,
    JoinHandle<()>,
) {
    use futures::{SinkExt, StreamExt};
    use tokio::net::UnixListener;
    use tokio::sync::oneshot;
    use tokio_util::codec::{Framed, LinesCodec};

    let dir = tempfile::tempdir_in("/tmp").unwrap();
    let sock = dir.path().join("d.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let (close_tx, close_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut framed = Framed::new(stream, LinesCodec::new());
        let _req = framed.next().await.unwrap().unwrap();
        for reply in replies {
            framed
                .send(serde_json::to_string(&reply).unwrap())
                .await
                .unwrap();
        }
        // Held open until the caller signals (or drops `close_tx`); a
        // `RecvError` on drop is exactly the "close now" signal, same as an
        // explicit send.
        let _ = close_rx.await;
    });
    (dir, sock, close_tx, server)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_stub_service_answers_at_once_and_serves_nothing_else() {
        let stub = StubService::new("stub");

        assert_eq!(stub.name(), "stub");
        let reply = stub.handle("anything", Value::Null).await.unwrap();
        assert_eq!(reply, serde_json::json!({ "done": true }));
        assert_eq!(stub.menu().title, "stub");
        assert!(stub.menu().items.is_empty());
        stub.menu_action("anything").await.unwrap();
        let status = stub.status().await;
        assert_eq!(status.name, "stub");
        assert!(status.healthy);
        stub.shutdown().await;
    }

    #[tokio::test]
    async fn a_slow_stub_service_takes_its_delay_to_answer() {
        let stub = StubService::slow("slow", Duration::from_millis(40));

        let started = std::time::Instant::now();
        stub.handle("work", Value::Null).await.unwrap();

        assert!(started.elapsed() >= Duration::from_millis(40));
    }

    #[test]
    fn a_stub_service_records_each_credited_outage_in_order() {
        let stub = StubService::new("stub");
        assert!(stub.credited().is_empty());

        stub.credit_accept_outage(Duration::from_secs(1));
        stub.credit_accept_outage(Duration::from_secs(2));

        assert_eq!(
            stub.credited(),
            vec![Duration::from_secs(1), Duration::from_secs(2)]
        );
    }
}
