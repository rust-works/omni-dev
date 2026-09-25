//! A **poll-only** observer of a Codex app-server's thread status.
//!
//! The source of exact state for Codex sessions `omni-dev codex-wrap` launches
//! (#1910, ADR-0088), the Codex analogue of the `claude-wrap` stream tracker.
//!
//! Codex's app-server speaks JSON-RPC over a WebSocket; with
//! `--listen unix://PATH` the WebSocket rides a Unix socket. Its
//! `thread/loaded/list` names the threads in memory and `thread/read` gives
//! each one's `status`: `idle`, `systemError`, `notLoaded`, or `active` with
//! `activeFlags` `waitingOnApproval` / `waitingOnUserInput`. That is the state
//! the Codex hooks can only infer.
//!
//! Two rules keep an observer from ever acting on a session:
//!
//! - **Poll, never subscribe.** The observer calls only `initialize`,
//!   `thread/loaded/list` and `thread/read`. It never starts, resumes or
//!   subscribes to a thread, so the server never routes a thread's approval
//!   requests to it (verified on 0.155.1: an observer connection received no
//!   server request while the TUI on the same server sat on an approval).
//! - **Never answer.** If a server request does arrive, it is dropped
//!   unanswered, so an observer cannot approve, deny or duplicate one.
//!
//! This module holds the client and the pure mapping; the process wiring lives
//! in [`crate::cli::codex_wrap`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::UnixStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use super::{Agent, ObserveRequest, SessionEvent, SessionState};

/// How long one request may wait for its response.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// A connected, initialized app-server client.
pub struct AppServerClient {
    ws: WebSocketStream<UnixStream>,
    next_id: u64,
}

impl AppServerClient {
    /// Connects to the app-server listening on the Unix socket `path` and runs
    /// the `initialize` handshake.
    pub async fn connect(path: &Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .await
            .with_context(|| format!("failed to connect to {}", path.display()))?;
        // The host is required by the handshake and ignored over a Unix socket.
        let (ws, _) = tokio_tungstenite::client_async("ws://localhost/", stream)
            .await
            .context("app-server WebSocket handshake failed")?;
        let mut client = Self { ws, next_id: 1 };
        client
            .request(
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "omni-dev-codex-wrap",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                }),
            )
            .await?;
        client.notify("initialized").await?;
        Ok(client)
    }

    /// Sends a notification (no response expected).
    async fn notify(&mut self, method: &str) -> Result<()> {
        let message = json!({ "jsonrpc": "2.0", "method": method });
        self.ws
            .send(Message::text(message.to_string()))
            .await
            .context("app-server send failed")
    }

    /// Sends one request and waits for its response, skipping notifications and
    /// dropping any server request unanswered (see the module docs).
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let message = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.ws
            .send(Message::text(message.to_string()))
            .await
            .context("app-server send failed")?;
        tokio::time::timeout(REQUEST_TIMEOUT, async {
            while let Some(frame) = self.ws.next().await {
                let text = match frame.context("app-server receive failed")? {
                    Message::Text(text) => text,
                    Message::Close(_) => break,
                    _ => continue,
                };
                let Ok(reply) = serde_json::from_str::<Value>(text.as_str()) else {
                    continue;
                };
                // A notification or a server request carries a `method`; neither
                // is ours to handle.
                if reply.get("method").is_some() || reply.get("id") != Some(&json!(id)) {
                    continue;
                }
                if let Some(error) = reply.get("error") {
                    bail!("app-server {method} failed: {error}");
                }
                return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
            }
            Err(anyhow!("app-server closed the connection"))
        })
        .await
        .map_err(|_| anyhow!("app-server {method} timed out"))?
    }

    /// The ids of the threads loaded in the server.
    pub async fn loaded_threads(&mut self) -> Result<Vec<String>> {
        let result = self.request("thread/loaded/list", json!({})).await?;
        Ok(result
            .get("data")
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default())
    }

    /// One thread's metadata and status, without its turns.
    pub async fn read_thread(&mut self, id: &str) -> Result<Option<ThreadView>> {
        let result = self
            .request(
                "thread/read",
                json!({ "threadId": id, "includeTurns": false }),
            )
            .await?;
        Ok(result.get("thread").and_then(ThreadView::from_json))
    }

    /// One poll of the server: the loaded thread ids, and the view of every
    /// loaded top-level thread that could be read. A thread whose read fails
    /// (it unloaded between the two calls, or the read timed out) is simply
    /// missing from `views` this poll; only a failed `thread/loaded/list` is an
    /// error.
    pub async fn snapshot(&mut self) -> Result<Snapshot> {
        let loaded = self.loaded_threads().await?;
        let mut views = Vec::new();
        for id in &loaded {
            if let Ok(Some(view)) = self.read_thread(id).await {
                if !view.subagent {
                    views.push(view);
                }
            }
        }
        Ok(Snapshot { loaded, views })
    }
}

/// One poll's result; see [`AppServerClient::snapshot`].
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Every thread id the server has loaded.
    pub loaded: Vec<String>,
    /// The readable, top-level ones among them.
    pub views: Vec<ThreadView>,
}

/// The fields of a `Thread` the observer reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadView {
    /// The thread id — the same `session_id` Codex's hooks report.
    pub id: String,
    /// The thread's working directory.
    pub cwd: Option<PathBuf>,
    /// The model id, when the server reports one.
    pub model: Option<String>,
    /// The mapped state, or `None` for a status that says nothing
    /// (`notLoaded`, or one this build does not know).
    pub state: Option<SessionState>,
    /// Not a session of its own: a subagent (a parent id, or a `subAgent*`
    /// source), or a system-spawned side thread such as the one Codex uses to
    /// title a chat (`ephemeral`, `threadSource: "system"`). Not reported —
    /// the user's thread already covers it.
    pub subagent: bool,
}

impl ThreadView {
    /// Reads a `Thread` object, tolerating missing optional fields. `None` when
    /// it has no id.
    fn from_json(thread: &Value) -> Option<Self> {
        let id = thread.get("id")?.as_str()?.to_string();
        let source = thread.get("source");
        let subagent = thread.get("parentThreadId").is_some_and(|p| !p.is_null())
            || thread.get("ephemeral").and_then(Value::as_bool) == Some(true)
            || thread
                .get("threadSource")
                .and_then(Value::as_str)
                .is_some_and(|s| s != "user")
            || source.is_some_and(Value::is_object)
            || source
                .and_then(Value::as_str)
                .is_some_and(|s| s.starts_with("subAgent"));
        Some(Self {
            id,
            cwd: thread.get("cwd").and_then(Value::as_str).map(PathBuf::from),
            model: thread
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
            state: thread.get("status").and_then(state_for_status),
            subagent,
        })
    }
}

/// Maps a `ThreadStatus` onto a [`SessionState`]. `None` for `notLoaded` and
/// any status this build does not know. `systemError` reads as idle: the turn
/// is over and the session waits for the user.
pub fn state_for_status(status: &Value) -> Option<SessionState> {
    match status.get("type")?.as_str()? {
        "idle" | "systemError" => Some(SessionState::Idle),
        "active" => {
            let flags: Vec<&str> = status
                .get("activeFlags")
                .and_then(Value::as_array)
                .map(|flags| flags.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            Some(if flags.contains(&"waitingOnApproval") {
                SessionState::WaitingForPermission
            } else if flags.contains(&"waitingOnUserInput") {
                SessionState::WaitingForInput
            } else {
                SessionState::Working
            })
        }
        _ => None,
    }
}

/// One thing the tracker wants the daemon to hear.
#[derive(Debug, Clone)]
pub enum Report {
    /// An authoritative state for a session.
    Observe(ObserveRequest),
    /// The session is over.
    End(String),
}

/// What the tracker last reported for a thread.
#[derive(Debug, Clone)]
struct Known {
    state: SessionState,
    cwd: Option<PathBuf>,
    model: Option<String>,
}

/// Turns successive [`snapshot`](AppServerClient::snapshot)s into reports.
///
/// Every poll re-asserts each live session's state, not only a change: the
/// same session also gets inferred reports from Codex's hooks and the rollout
/// watcher, and re-asserting the exact state overrides any of them within one
/// poll (a repeat sighting does not bump the registry, so this costs a socket
/// round-trip, not a push). It also keeps an idle session inside the TTL. A
/// thread that leaves the loaded set, or reports a status that says nothing,
/// is ended; one merely unreadable this poll is left alone. Pure, so it is
/// unit-tested without a server.
#[derive(Debug, Default)]
pub struct StatusTracker {
    known: HashMap<String, Known>,
}

impl StatusTracker {
    /// Folds in one poll.
    pub fn update(&mut self, snapshot: &Snapshot) -> Vec<Report> {
        let mut reports = Vec::new();
        let mut silent = Vec::new();
        for view in &snapshot.views {
            let Some(state) = view.state else {
                silent.push(view.id.clone());
                continue;
            };
            let known = Known {
                state,
                cwd: view.cwd.clone(),
                model: view.model.clone(),
            };
            reports.push(Report::Observe(observe(&view.id, &known)));
            self.known.insert(view.id.clone(), known);
        }
        let gone: Vec<String> = self
            .known
            .keys()
            .filter(|id| !snapshot.loaded.contains(id) || silent.contains(id))
            .cloned()
            .collect();
        for id in gone {
            self.known.remove(&id);
            reports.push(Report::End(id));
        }
        reports
    }

    /// Ends every known session — the wrapper is exiting.
    pub fn finish(&mut self) -> Vec<Report> {
        self.known.drain().map(|(id, _)| Report::End(id)).collect()
    }
}

/// The authoritative `observe` for a known thread.
fn observe(id: &str, known: &Known) -> ObserveRequest {
    ObserveRequest {
        pid: None,
        pid_start: None,
        agent: Agent::Codex,
        session_id: id.to_string(),
        cwd: known.cwd.clone(),
        transcript_path: None,
        event: SessionEvent::StreamState(known.state),
        repo: None,
        model: known.model.clone(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn view(id: &str, state: Option<SessionState>) -> ThreadView {
        ThreadView {
            id: id.to_string(),
            cwd: Some(PathBuf::from("/work/repo")),
            model: Some("gpt-5.5".to_string()),
            state,
            subagent: false,
        }
    }

    fn summary(reports: &[Report]) -> Vec<String> {
        reports
            .iter()
            .map(|r| match r {
                Report::Observe(o) => format!("{}:{:?}", o.session_id, o.event),
                Report::End(id) => format!("{id}:end"),
            })
            .collect()
    }

    #[test]
    fn statuses_map_to_exact_states() {
        let s = |v: Value| state_for_status(&v);
        assert_eq!(s(json!({ "type": "idle" })), Some(SessionState::Idle));
        assert_eq!(
            s(json!({ "type": "systemError" })),
            Some(SessionState::Idle)
        );
        assert_eq!(
            s(json!({ "type": "active", "activeFlags": [] })),
            Some(SessionState::Working)
        );
        assert_eq!(
            s(json!({ "type": "active", "activeFlags": ["waitingOnApproval"] })),
            Some(SessionState::WaitingForPermission)
        );
        assert_eq!(
            s(json!({ "type": "active", "activeFlags": ["waitingOnUserInput"] })),
            Some(SessionState::WaitingForInput)
        );
        // Approval outranks input when both are pending.
        assert_eq!(
            s(
                json!({ "type": "active", "activeFlags": ["waitingOnUserInput", "waitingOnApproval"] })
            ),
            Some(SessionState::WaitingForPermission)
        );
        assert_eq!(s(json!({ "type": "notLoaded" })), None);
        assert_eq!(s(json!({ "type": "somethingNew" })), None);
        assert_eq!(s(json!({})), None);
    }

    #[test]
    fn thread_views_read_the_fields_and_flag_subagents() {
        let top = ThreadView::from_json(&json!({
            "id": "t1",
            "cwd": "/work/repo",
            "model": "gpt-5.5",
            "source": "vscode",
            "threadSource": "user",
            "ephemeral": false,
            "status": { "type": "active", "activeFlags": ["waitingOnApproval"] },
            "preview": "never read",
        }))
        .unwrap();
        assert_eq!(top.id, "t1");
        assert_eq!(top.cwd.as_deref(), Some(Path::new("/work/repo")));
        assert_eq!(top.state, Some(SessionState::WaitingForPermission));
        assert!(!top.subagent);
        for sub in [
            json!({ "id": "c", "parentThreadId": "t1", "status": { "type": "idle" } }),
            json!({ "id": "c", "source": "subAgentThreadSpawn", "status": { "type": "idle" } }),
            json!({ "id": "c", "source": { "subagent": "review" }, "status": { "type": "idle" } }),
            // The chat-titling side thread observed on 0.155.1.
            json!({ "id": "c", "ephemeral": true, "source": "vscode", "threadSource": "system",
                    "status": { "type": "active", "activeFlags": [] } }),
        ] {
            assert!(ThreadView::from_json(&sub).unwrap().subagent, "{sub}");
        }
        assert!(ThreadView::from_json(&json!({ "cwd": "/x" })).is_none());
    }

    /// A snapshot where every view is loaded and readable.
    fn snap(views: &[ThreadView]) -> Snapshot {
        Snapshot {
            loaded: views.iter().map(|v| v.id.clone()).collect(),
            views: views.to_vec(),
        }
    }

    #[test]
    fn the_tracker_reasserts_every_poll_and_ends_unloaded_threads() {
        let mut tracker = StatusTracker::default();
        let idle = [view("t1", Some(SessionState::Idle))];
        assert_eq!(
            summary(&tracker.update(&snap(&idle))),
            vec!["t1:StreamState(Idle)"]
        );
        // Unchanged: re-asserted, so a hook's inferred state is overridden
        // within a poll.
        assert_eq!(
            summary(&tracker.update(&snap(&idle))),
            vec!["t1:StreamState(Idle)"]
        );
        // A second thread (`/new`) joins; the first unloads.
        assert_eq!(
            summary(&tracker.update(&snap(&[view("t2", Some(SessionState::Working))]))),
            vec!["t2:StreamState(Working)", "t1:end"]
        );
        // A status that says nothing counts as gone.
        assert_eq!(
            summary(&tracker.update(&snap(&[view("t2", None)]))),
            vec!["t2:end"]
        );
        assert!(tracker.finish().is_empty());
    }

    #[test]
    fn an_unreadable_but_loaded_thread_is_not_ended() {
        let mut tracker = StatusTracker::default();
        tracker.update(&snap(&[view("t1", Some(SessionState::Working))]));
        // `thread/read` failed for t1 this poll, but it is still loaded.
        let partial = Snapshot {
            loaded: vec!["t1".to_string()],
            views: Vec::new(),
        };
        assert!(tracker.update(&partial).is_empty());
        let request = match &tracker.update(&snap(&[view("t1", Some(SessionState::Idle))]))[0] {
            Report::Observe(request) => request.clone(),
            Report::End(_) => panic!("expected an observe"),
        };
        assert_eq!(request.agent, Agent::Codex);
        assert_eq!(request.model.as_deref(), Some("gpt-5.5"));
        assert_eq!(summary(&tracker.finish()), vec!["t1:end"]);
        assert!(tracker.finish().is_empty());
    }

    /// A scripted app-server on a Unix socket: answers `initialize`,
    /// `thread/loaded/list` and `thread/read`, and interleaves a notification
    /// and a server request (which the client must skip and never answer).
    async fn fake_server(path: PathBuf, answered: std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
        while let Some(Ok(Message::Text(text))) = ws.next().await {
            let msg: Value = serde_json::from_str(text.as_str()).unwrap();
            answered.lock().unwrap().push(msg.clone());
            let Some(id) = msg.get("id").cloned() else {
                continue;
            };
            let result = match msg["method"].as_str().unwrap() {
                "initialize" => json!({ "userAgent": "fake" }),
                "thread/loaded/list" => json!({ "data": ["t1", "child"] }),
                "thread/read" if msg["params"]["threadId"] == "t1" => json!({ "thread": {
                    "id": "t1", "cwd": "/w", "source": "cli",
                    "status": { "type": "active", "activeFlags": ["waitingOnApproval"] },
                } }),
                "thread/read" => json!({ "thread": {
                    "id": "child", "parentThreadId": "t1", "status": { "type": "idle" },
                } }),
                other => panic!("unexpected {other}"),
            };
            let noise = [
                json!({ "method": "thread/status/changed", "params": {} }),
                json!({ "id": 900, "method": "item/commandExecution/requestApproval", "params": {} }),
                json!({ "id": id, "result": result }),
            ];
            for frame in noise {
                ws.send(Message::text(frame.to_string())).await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn the_client_polls_and_never_answers_a_server_request() {
        let tmp = tempfile::tempdir_in("/tmp").unwrap();
        let path = tmp.path().join("as.sock");
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let server = tokio::spawn(fake_server(path.clone(), seen.clone()));
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let mut client = AppServerClient::connect(&path).await.unwrap();
        let snapshot = client.snapshot().await.unwrap();
        assert_eq!(snapshot.loaded, ["t1", "child"]);
        let views = snapshot.views;
        assert_eq!(views.len(), 1, "the subagent thread is dropped");
        assert_eq!(views[0].state, Some(SessionState::WaitingForPermission));
        drop(client);
        server.await.unwrap();
        let methods: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|m| m["method"].as_str().unwrap_or("<response>").to_string())
            .collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "initialized",
                "thread/loaded/list",
                "thread/read",
                "thread/read"
            ],
            "only polls were sent — no response to the server's request"
        );
    }
}
