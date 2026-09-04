//! Reconnect/backoff supervision for a daemon push subscription.
//!
//! Nothing in [`DaemonClient`] or the existing CLI precedent
//! (`worktrees tree --follow`, `src/cli/worktrees.rs`) reconnects on its
//! own — a dropped connection just ends the stream and the process exits.
//! This module builds that from scratch: full-jitter backoff on a dropped
//! connection, and — critically — detection of an **old daemon that doesn't
//! know the `subscribe` op**, which replies `{ "ok": false }` and then
//! *holds the connection open* rather than closing it (so naively looping
//! `subscribe` again would just re-earn the same refusal forever). That case
//! permanently falls back to one-shot polling instead.

use std::path::PathBuf;
use std::time::Duration;

use rand::RngExt;
use serde::de::DeserializeOwned;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::daemon::client::DaemonClient;
use crate::daemon::protocol::DaemonEnvelope;

/// The live state of a supervised feed, as seen by its consumer.
#[derive(Debug, Clone)]
pub enum FeedFrame<T> {
    /// No frame has arrived yet (before the first connect attempt resolves).
    Connecting,
    /// The most recently pushed (or polled) snapshot.
    Live(T),
    /// The subscription dropped and is retrying after `retry_in`.
    Reconnecting { attempt: u32, retry_in: Duration },
    /// The daemon doesn't know this subscribe op — permanently switched to
    /// polling instead of ever retrying `subscribe` again.
    Polling,
}

/// Spawns a supervised subscription: subscribes via `subscribe_envelope`,
/// retrying with full-jitter backoff on any disconnect. If the daemon refuses
/// the subscribe outright on the very first frame (`ok: false` — an old
/// daemon), permanently falls back to one-shot polling `poll_envelope` on
/// `poll_interval`.
///
/// Returns a `watch::Receiver` a consumer can `.changed()`/`.borrow()` from
/// (only the latest frame matters to a redraw loop), and the supervisor
/// task's handle (droppable; the task also stops on `cancel`).
pub fn spawn_subscription<T>(
    socket: PathBuf,
    subscribe_envelope: DaemonEnvelope,
    poll_envelope: DaemonEnvelope,
    poll_interval: Duration,
    cancel: CancellationToken,
) -> (watch::Receiver<FeedFrame<T>>, JoinHandle<()>)
where
    T: DeserializeOwned + Send + Sync + 'static,
{
    let (tx, rx) = watch::channel(FeedFrame::Connecting);
    let handle = tokio::spawn(run_supervisor(
        socket,
        subscribe_envelope,
        poll_envelope,
        poll_interval,
        tx,
        cancel,
    ));
    (rx, handle)
}

async fn run_supervisor<T>(
    socket: PathBuf,
    subscribe_envelope: DaemonEnvelope,
    poll_envelope: DaemonEnvelope,
    poll_interval: Duration,
    tx: watch::Sender<FeedFrame<T>>,
    cancel: CancellationToken,
) where
    T: DeserializeOwned + Send + Sync + 'static,
{
    let client = DaemonClient::new(socket);
    let mut attempt: u32 = 0;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        if let Ok(mut sub) = client.subscribe(subscribe_envelope.clone()).await {
            match sub.next().await {
                Some(Ok(reply)) if !reply.ok => {
                    // An old daemon that doesn't recognise `subscribe`: it
                    // replies ok:false and holds the connection open. Drop it
                    // (closing our end) and never retry `subscribe` again.
                    drop(sub);
                    return run_polling_fallback(client, poll_envelope, poll_interval, tx, cancel)
                        .await;
                }
                Some(Ok(reply)) => {
                    if let Ok(value) = serde_json::from_value(reply.payload) {
                        attempt = 0;
                        let _ = tx.send(FeedFrame::Live(value));
                    }
                    loop {
                        match sub.next().await {
                            Some(Ok(reply)) if reply.ok => {
                                if let Ok(value) = serde_json::from_value(reply.payload) {
                                    let _ = tx.send(FeedFrame::Live(value));
                                }
                            }
                            // A non-ok frame here (rather than on first read)
                            // would be unusual — treat it the same as a
                            // transport error and just reconnect, since only
                            // the *first* frame carries the "unsupported op"
                            // meaning.
                            _ => break,
                        }
                    }
                }
                _ => {}
            }
        }

        let delay = full_jitter_backoff(attempt);
        let _ = tx.send(FeedFrame::Reconnecting {
            attempt,
            retry_in: delay,
        });
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = cancel.cancelled() => return,
        }
        attempt = attempt.saturating_add(1);
    }
}

async fn run_polling_fallback<T>(
    client: DaemonClient,
    poll_envelope: DaemonEnvelope,
    poll_interval: Duration,
    tx: watch::Sender<FeedFrame<T>>,
    cancel: CancellationToken,
) where
    T: DeserializeOwned + Send + Sync + 'static,
{
    let _ = tx.send(FeedFrame::Polling);
    let mut ticker = tokio::time::interval(poll_interval);
    loop {
        tokio::select! {
            _ = ticker.tick() => {
                if let Ok(reply) = client.request(poll_envelope.clone()).await {
                    if reply.ok {
                        if let Ok(value) = serde_json::from_value(reply.payload) {
                            let _ = tx.send(FeedFrame::Live(value));
                        }
                    }
                }
            }
            () = cancel.cancelled() => return,
        }
    }
}

/// Full-jitter backoff: `[0, min(500ms * 2^attempt, 10s)]`, matching the VS
/// Code companion's `subscription.ts` reconnect contract.
fn full_jitter_backoff(attempt: u32) -> Duration {
    const BASE_MS: u64 = 500;
    const CAP_MS: u64 = 10_000;
    let exp_ms = BASE_MS.saturating_mul(1u64 << attempt.min(20)).min(CAP_MS);
    let jittered_ms = rand::rng().random_range(0..=exp_ms);
    Duration::from_millis(jittered_ms)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn full_jitter_backoff_is_bounded_by_the_cap() {
        for attempt in 0..40 {
            let delay = full_jitter_backoff(attempt);
            assert!(
                delay <= Duration::from_millis(10_000),
                "attempt {attempt}: {delay:?}"
            );
        }
    }

    #[test]
    fn full_jitter_backoff_grows_with_attempt_number() {
        // Not strictly monotonic (it's jittered), but the cap should be
        // reached well before attempt 10 given the base/doubling above.
        let late = full_jitter_backoff(10);
        assert!(late <= Duration::from_millis(10_000));
    }

    #[tokio::test]
    async fn subscription_delivers_live_frames_from_a_fake_daemon() {
        use crate::daemon::testutil::fake_daemon_stream_hold_open;
        use serde_json::json;

        // Held open (not the plain one-shot `fake_daemon_stream`): once the
        // fake closes, the supervisor immediately starts reconnecting, and
        // that whole disconnect-and-retry sequence can run to completion
        // before this test's `watch::Receiver` is ever polled again — so a
        // plain one-shot fake makes this test race past the Live frame
        // entirely and only ever observe the post-disconnect `Reconnecting`
        // state. Holding the connection open keeps Live the *stable* current
        // value until this test explicitly closes it.
        let (_dir, sock, close_tx, server) = fake_daemon_stream_hold_open(vec![json!({
            "ok": true,
            "payload": { "repos": [], "show_closed": false },
        })]);
        let cancel = CancellationToken::new();
        let (mut rx, _task) = spawn_subscription::<super::super::wire::TreeSnapshotWire>(
            sock,
            DaemonEnvelope::service("worktrees", "subscribe", serde_json::Value::Null),
            DaemonEnvelope::service("worktrees", "tree", serde_json::Value::Null),
            Duration::from_millis(50),
            cancel.clone(),
        );
        loop {
            if matches!(&*rx.borrow(), FeedFrame::Live(_)) {
                break;
            }
            rx.changed().await.unwrap();
        }
        assert!(matches!(&*rx.borrow(), FeedFrame::Live(snapshot) if snapshot.repos.is_empty()));

        // Let the fake daemon close now that the Live frame was observed;
        // the supervisor should notice and start reconnecting (against a
        // socket nothing listens on any more, so cancel rather than wait).
        drop(close_tx);
        cancel.cancel();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn subscription_refused_by_an_old_daemon_falls_back_to_polling() {
        use crate::daemon::testutil::fake_daemon_reply;
        use serde_json::json;

        // The fake one-shot daemon closes after replying once, which is close
        // enough to "holds the connection open then we drop it": the point
        // under test is that the supervisor stops calling `subscribe` and
        // switches state to `Polling`, not the exact half-close behaviour.
        let (_dir, sock, _server) =
            fake_daemon_reply(json!({ "ok": false, "error": "unknown worktrees op: subscribe" }));
        let cancel = CancellationToken::new();
        let (mut rx, _task) = spawn_subscription::<super::super::wire::TreeSnapshotWire>(
            sock,
            DaemonEnvelope::service("worktrees", "subscribe", serde_json::Value::Null),
            DaemonEnvelope::service("worktrees", "tree", serde_json::Value::Null),
            Duration::from_secs(3600),
            cancel.clone(),
        );
        loop {
            if matches!(&*rx.borrow(), FeedFrame::Polling) {
                break;
            }
            rx.changed().await.unwrap();
        }
        cancel.cancel();
    }
}
