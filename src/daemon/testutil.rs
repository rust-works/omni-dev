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
