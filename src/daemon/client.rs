//! [`DaemonClient`]: a thin client for the daemon's Unix control socket, used
//! by `daemon status` / `stop` / `restart` and the single-instance `ping`
//! probe.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use futures::{SinkExt, StreamExt};
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LinesCodec};

use super::protocol::{DaemonEnvelope, DaemonReply, StatusReport, MAX_LINE_BYTES};

/// A one-shot client over the daemon's Unix-domain control socket. Each call
/// opens a fresh connection, sends one [`DaemonEnvelope`], and reads one
/// [`DaemonReply`].
#[derive(Debug, Clone)]
pub struct DaemonClient {
    socket_path: PathBuf,
}

impl DaemonClient {
    /// Creates a client targeting the socket at `socket_path`.
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    /// The socket path this client targets.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Sends one envelope and returns the daemon's reply.
    pub async fn request(&self, envelope: DaemonEnvelope) -> Result<DaemonReply> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| {
                format!(
                    "failed to connect to daemon socket {} (is the daemon running?)",
                    self.socket_path.display()
                )
            })?;
        let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(MAX_LINE_BYTES));
        let line = serde_json::to_string(&envelope).context("failed to encode daemon request")?;
        framed
            .send(line)
            .await
            .context("failed to send daemon request")?;
        let response = framed
            .next()
            .await
            .context("daemon closed the connection without replying")?
            .context("failed to read daemon reply")?;
        serde_json::from_str(&response).context("failed to decode daemon reply")
    }

    /// Opens a streaming subscription: sends `envelope` once and returns a
    /// handle over which the daemon pushes **many** replies on the same
    /// connection (an initial snapshot then a fresh one on each change) until the
    /// daemon or the client closes it. Per the control-socket protocol the client
    /// must only *read* after this — sending any further line is interpreted as an
    /// explicit cancel — so [`DaemonSubscription`] exposes reads only.
    pub async fn subscribe(&self, envelope: DaemonEnvelope) -> Result<DaemonSubscription> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .with_context(|| {
                format!(
                    "failed to connect to daemon socket {} (is the daemon running?)",
                    self.socket_path.display()
                )
            })?;
        let mut framed = Framed::new(stream, LinesCodec::new_with_max_length(MAX_LINE_BYTES));
        let line = serde_json::to_string(&envelope).context("failed to encode daemon request")?;
        framed
            .send(line)
            .await
            .context("failed to send daemon subscription request")?;
        Ok(DaemonSubscription { framed })
    }

    /// Sends an envelope and returns its payload, turning an `ok: false` reply
    /// into an `Err`.
    async fn request_ok(&self, envelope: DaemonEnvelope) -> Result<serde_json::Value> {
        let reply = self.request(envelope).await?;
        if reply.ok {
            Ok(reply.payload)
        } else {
            bail!(
                "daemon returned an error: {}",
                reply.error.as_deref().unwrap_or("unknown error")
            )
        }
    }

    /// Probes whether a live daemon is answering on the socket.
    pub async fn ping(&self) -> Result<()> {
        self.request_ok(DaemonEnvelope::builtin("ping"))
            .await
            .map(|_| ())
    }

    /// Returns the resident daemon's advertised binary version, or `None` when it
    /// advertises none (a pre-#1113 daemon whose `ping` carries no version).
    ///
    /// Lets a client warn that it is driving a stale daemon after a binary
    /// upgrade without a separate `status` round-trip.
    pub async fn version(&self) -> Result<Option<String>> {
        let payload = self.request_ok(DaemonEnvelope::builtin("ping")).await?;
        Ok(payload
            .get("version")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string))
    }

    /// Requests aggregated per-service status.
    pub async fn status(&self) -> Result<StatusReport> {
        let payload = self.request_ok(DaemonEnvelope::builtin("status")).await?;
        serde_json::from_value(payload).context("failed to decode daemon status report")
    }

    /// Asks the daemon to shut down gracefully.
    pub async fn shutdown(&self) -> Result<()> {
        self.request_ok(DaemonEnvelope::builtin("shutdown"))
            .await
            .map(|_| ())
    }
}

/// A live push subscription opened by [`DaemonClient::subscribe`].
///
/// The daemon keeps sending [`DaemonReply`] frames on the connection; call
/// [`next`](Self::next) in a loop to consume them. It exposes reads only — the
/// protocol treats any further write as a cancel — and dropping it (e.g. on
/// Ctrl-C) ends the stream.
#[derive(Debug)]
pub struct DaemonSubscription {
    framed: Framed<UnixStream, LinesCodec>,
}

impl DaemonSubscription {
    /// Reads the next pushed reply, `None` once the daemon closes the stream
    /// (EOF), or `Some(Err(..))` on a read/decode failure.
    pub async fn next(&mut self) -> Option<Result<DaemonReply>> {
        match self.framed.next().await {
            Some(Ok(line)) => {
                Some(serde_json::from_str(&line).context("failed to decode daemon stream frame"))
            }
            Some(Err(e)) => Some(Err(
                anyhow::Error::new(e).context("failed to read daemon stream")
            )),
            None => None,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::daemon::testutil::{fake_daemon_reply, fake_daemon_stream};

    #[tokio::test]
    async fn version_reads_the_version_from_a_ping_reply() {
        let (_dir, sock, server) = fake_daemon_reply(
            serde_json::json!({ "ok": true, "payload": { "pong": true, "version": "1.2.3" } }),
        );
        let version = DaemonClient::new(&sock).version().await.unwrap();
        assert_eq!(version.as_deref(), Some("1.2.3"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn version_is_none_for_a_pre_1113_daemon_without_a_version_field() {
        // An older daemon's ping carries no `version`; `version()` must decode it
        // as `None` rather than erroring.
        let (_dir, sock, server) =
            fake_daemon_reply(serde_json::json!({ "ok": true, "payload": { "pong": true } }));
        let version = DaemonClient::new(&sock).version().await.unwrap();
        assert!(version.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn request_ok_maps_an_error_reply_to_an_err() {
        let (_dir, sock, server) =
            fake_daemon_reply(serde_json::json!({ "ok": false, "error": "boom" }));
        let err = DaemonClient::new(&sock).version().await.unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn subscribe_yields_each_pushed_frame_then_none_on_close() {
        // The daemon pushes two snapshot frames then closes; `next()` returns each
        // decoded reply in order and finally `None` at EOF.
        let (_dir, sock, server) = fake_daemon_stream(vec![
            serde_json::json!({ "ok": true, "payload": { "repos": [], "show_closed": true } }),
            serde_json::json!({ "ok": true, "payload": { "repos": [], "show_closed": false } }),
        ]);
        let mut sub = DaemonClient::new(&sock)
            .subscribe(DaemonEnvelope::service(
                "worktrees",
                "subscribe",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();

        let first = sub.next().await.unwrap().unwrap();
        assert!(first.ok);
        assert_eq!(first.payload["show_closed"], serde_json::json!(true));
        let second = sub.next().await.unwrap().unwrap();
        assert_eq!(second.payload["show_closed"], serde_json::json!(false));
        // The daemon has closed the connection: the stream ends.
        assert!(sub.next().await.is_none());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn subscribe_next_surfaces_a_read_error_on_an_oversized_frame() {
        // A frame longer than MAX_LINE_BYTES trips the client codec's max-length
        // guard, exercising the read-error arm of `DaemonSubscription::next`.
        let big = "x".repeat(crate::daemon::protocol::MAX_LINE_BYTES + 1);
        let (_dir, sock, server) = fake_daemon_stream(vec![serde_json::Value::String(big)]);
        let mut sub = DaemonClient::new(&sock)
            .subscribe(DaemonEnvelope::service(
                "worktrees",
                "subscribe",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        let frame = sub.next().await;
        assert!(
            matches!(frame, Some(Err(_))),
            "an oversized frame should surface as a read error"
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn subscribe_errors_when_the_daemon_is_unreachable() {
        // No daemon at the socket → the connect fails with the usual context.
        let err = DaemonClient::new("/nonexistent/omni-dev-sub.sock")
            .subscribe(DaemonEnvelope::service(
                "worktrees",
                "subscribe",
                serde_json::Value::Null,
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("failed to connect"), "{err}");
    }
}
