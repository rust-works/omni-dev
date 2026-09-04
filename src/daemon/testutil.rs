//! Test-only helpers for exercising the daemon control socket from unit tests.
//!
//! Compiled only under `cfg(test)` (and Unix, since the control plane is an
//! `AF_UNIX` socket). Shared by the thin-client tests across `cli::daemon`,
//! `daemon::client`, and friends so the one-shot fake-daemon harness is not
//! duplicated per module.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;

use serde_json::Value;
use tempfile::TempDir;
use tokio::task::JoinHandle;

/// Spawns a one-shot fake daemon on a short-path Unix socket that reads exactly
/// one request line and replies with `reply` (a full `DaemonReply`-shaped JSON
/// value). Returns the temp dir (keep it alive for the socket's lifetime), the
/// socket path, and the server task (await it to assert the exchange completed).
///
/// A short `/tmp` base path keeps the socket under the 104-byte `sockaddr_un`
/// limit that a long `TMPDIR` would otherwise blow.
pub(crate) fn fake_daemon_reply(reply: Value) -> (TempDir, PathBuf, JoinHandle<()>) {
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
        framed
            .send(serde_json::to_string(&reply).unwrap())
            .await
            .unwrap();
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
