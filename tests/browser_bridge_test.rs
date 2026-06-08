//! End-to-end tests for the browser bridge.
//!
//! Each test boots a real bridge on OS-assigned ports (`--ws-port 0` /
//! `--control-port 0`), connects a fake "browser" over the WebSocket plane
//! presenting the token subprotocol, and drives the HTTP control plane with
//! `reqwest`. The fake browser echoes a canned response so request/reply
//! correlation can be asserted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::ClientRequestBuilder;
use tokio_tungstenite::tungstenite::Message;

use omni_dev::browser::{self, BridgeConfig};

/// Boots a bridge on random ports and returns `(control_port, ws_port, token)`.
/// Serialises the reserve→drop→rebind window across all tests. `tokio::test`
/// runs test fns concurrently, so without this two tests can be handed the same
/// just-freed ephemeral port and the second bridge fails to bind. We hold the
/// lock until *both* of a bridge's ports are accepting, so no two bridges are
/// ever mid-rebind on overlapping ports at once.
static START_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn start_bridge(allow_origin: Option<String>, timeout: Duration) -> (u16, u16, String) {
    start_bridge_with(allow_origin, timeout, 1024 * 1024).await
}

async fn start_bridge_with(
    allow_origin: Option<String>,
    timeout: Duration,
    max_body_bytes: usize,
) -> (u16, u16, String) {
    let token = "test-token-abcdef".to_string();

    // The reserve→drop→rebind dance below has an inherent race: between dropping a
    // reserved ephemeral port and the bridge rebinding it, another concurrently
    // running test's client socket can squat it, and the bridge then fails to bind
    // (fail-closed → `run` returns immediately). Rather than wait out a fixed
    // timeout and panic, detect that case directly — the spawned `run` future only
    // completes when binding failed — and retry with fresh ports.
    for _ in 0..20 {
        let _guard = START_LOCK.lock().await;

        let c = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let w = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let control_port = c.local_addr().unwrap().port();
        let ws_port = w.local_addr().unwrap().port();
        drop(c);
        drop(w);

        let config = BridgeConfig {
            ws_port,
            control_port,
            request_timeout: timeout,
            allow_origin: allow_origin.clone(),
            max_body_bytes,
            max_concurrent: 64,
        };
        let token_clone = token.clone();
        let mut handle = tokio::spawn(async move { browser::run(config, token_clone).await });

        // Race "both planes accept" against "`run` returned" (a bind failure). A
        // successful bind keeps `run` serving forever, so only the listening
        // branch can complete; a failed bind completes the handle → retry.
        let up = tokio::select! {
            ok = both_listening(control_port, ws_port) => ok,
            _ = &mut handle => false,
        };
        if up {
            return (control_port, ws_port, token);
        }
        // Bridge failed to bind a squatted port; drop the lock and try again.
    }
    panic!("could not start a bridge on free ports after 20 attempts");
}

/// Resolves `true` once both planes accept connections. Bounded so a wedged
/// plane can't hang the `select!` in `start_bridge_with` forever.
async fn both_listening(control_port: u16, ws_port: u16) -> bool {
    wait_until_listening(control_port).await && wait_until_listening(ws_port).await
}

async fn wait_until_listening(port: u16) -> bool {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// A fake browser: connects, presents the token subprotocol, and (optionally)
/// echoes a 200 response for each command it receives.
struct FakeBrowser {
    handle: tokio::task::JoinHandle<()>,
}

impl FakeBrowser {
    async fn connect(ws_port: u16, token: &str, reply: bool) -> Self {
        let uri = format!("ws://127.0.0.1:{ws_port}/").parse().unwrap();
        let request = ClientRequestBuilder::new(uri).with_sub_protocol(token);
        let (ws, _resp) = tokio_tungstenite::connect_async(request).await.unwrap();
        let (mut sink, mut stream) = ws.split();
        let handle = tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                if let Message::Text(txt) = msg {
                    let cmd: Value = serde_json::from_str(&txt).unwrap();
                    let id = cmd["id"].as_u64().unwrap();
                    if reply {
                        let body = format!("echo:{}", cmd["url"].as_str().unwrap_or(""));
                        let resp = serde_json::json!({
                            "id": id,
                            "status": 200,
                            "headers": {"content-type": "text/plain"},
                            "body": body,
                        });
                        sink.send(Message::Text(resp.to_string().into()))
                            .await
                            .unwrap();
                    }
                }
            }
        });
        Self { handle }
    }

    /// Connects presenting an `Origin` header and echoes `tab:<tag>:<url>` for
    /// each command, so multi-tab routing tests can assert *which* tab replied.
    async fn connect_tagged(ws_port: u16, token: &str, origin: &str, tag: &str) -> Self {
        let uri = format!("ws://127.0.0.1:{ws_port}/").parse().unwrap();
        let request = ClientRequestBuilder::new(uri)
            .with_sub_protocol(token)
            .with_header("Origin", origin);
        let (ws, _resp) = tokio_tungstenite::connect_async(request).await.unwrap();
        let (mut sink, mut stream) = ws.split();
        let tag = tag.to_string();
        let handle = tokio::spawn(async move {
            while let Some(Ok(msg)) = stream.next().await {
                if let Message::Text(txt) = msg {
                    let cmd: Value = serde_json::from_str(&txt).unwrap();
                    let id = cmd["id"].as_u64().unwrap();
                    let body = format!("tab:{tag}:{}", cmd["url"].as_str().unwrap_or(""));
                    let resp = serde_json::json!({
                        "id": id,
                        "status": 200,
                        "headers": {"content-type": "text/plain"},
                        "body": body,
                    });
                    sink.send(Message::Text(resp.to_string().into()))
                        .await
                        .unwrap();
                }
            }
        });
        Self { handle }
    }
}

impl Drop for FakeBrowser {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// A control-plane client that injects the required auth headers.
fn client(control_port: u16, token: &str) -> (reqwest::Client, String, String) {
    (
        reqwest::Client::new(),
        format!("http://127.0.0.1:{control_port}"),
        token.to_string(),
    )
}

#[tokio::test]
async fn status_reflects_connection() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let (http, base, tok) = client(control_port, &token);

    // Before any browser connects.
    let resp = http
        .get(format!("{base}/__bridge/status"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["connected"], Value::Bool(false));

    let _browser = FakeBrowser::connect(ws_port, &token, true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let resp = http
        .get(format!("{base}/__bridge/status"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .send()
        .await
        .unwrap();
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["connected"], Value::Bool(true));
}

#[tokio::test]
async fn bridge_request_round_trips() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _browser = FakeBrowser::connect(ws_port, &token, true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .json(&serde_json::json!({"url": "/loki/api/v1/labels", "method": "GET"}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let env: Value = resp.json().await.unwrap();
    assert_eq!(env["status"], 200);
    assert_eq!(env["body"], "echo:/loki/api/v1/labels");
}

#[tokio::test]
async fn transparent_proxy_round_trips() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _browser = FakeBrowser::connect(ws_port, &token, true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let resp = http
        .get(format!("{base}/api/datasources"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "echo:/api/datasources");
}

#[tokio::test]
async fn concurrent_requests_correlate_by_id() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _browser = FakeBrowser::connect(ws_port, &token, true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let mut handles = Vec::new();
    for i in 0..20 {
        let http = http.clone();
        let base = base.clone();
        let tok = tok.clone();
        handles.push(tokio::spawn(async move {
            let url = format!("/path/{i}");
            let resp = http
                .post(format!("{base}/__bridge/request"))
                .bearer_auth(&tok)
                .header("x-omni-bridge", "1")
                .json(&serde_json::json!({"url": url, "method": "GET"}))
                .send()
                .await
                .unwrap();
            let env: Value = resp.json().await.unwrap();
            assert_eq!(env["body"], format!("echo:/path/{i}"));
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
}

#[tokio::test]
async fn no_reply_times_out_with_504() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_millis(300)).await;
    // Browser connects but never replies.
    let _browser = FakeBrowser::connect(ws_port, &token, false).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .json(&serde_json::json!({"url": "/slow", "method": "GET"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 504);
}

#[tokio::test]
async fn no_browser_returns_503() {
    let (control_port, _ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let (http, base, tok) = client(control_port, &token);
    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .json(&serde_json::json!({"url": "/x", "method": "GET"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 503);
}

#[tokio::test]
async fn missing_token_is_rejected() {
    let (control_port, _ws_port, _token) = start_bridge(None, Duration::from_secs(5)).await;
    let http = reqwest::Client::new();
    let resp = http
        .get(format!("http://127.0.0.1:{control_port}/__bridge/status"))
        .header("x-omni-bridge", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 401);
}

#[tokio::test]
async fn missing_bridge_header_is_rejected() {
    let (control_port, _ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let http = reqwest::Client::new();
    let resp = http
        .get(format!("http://127.0.0.1:{control_port}/__bridge/status"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn browser_originated_request_is_rejected() {
    let (control_port, _ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let http = reqwest::Client::new();
    let resp = http
        .get(format!("http://127.0.0.1:{control_port}/__bridge/status"))
        .bearer_auth(&token)
        .header("x-omni-bridge", "1")
        .header("origin", "https://evil.test")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn cross_origin_url_rejected_without_allow_origin() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _browser = FakeBrowser::connect(ws_port, &token, true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .json(&serde_json::json!({"url": "https://evil.test/x", "method": "GET"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn unauthenticated_ws_peer_cannot_connect() {
    let (_control_port, ws_port, _token) = start_bridge(None, Duration::from_secs(5)).await;
    let uri = format!("ws://127.0.0.1:{ws_port}/").parse().unwrap();
    // Present a wrong token as the subprotocol.
    let request = ClientRequestBuilder::new(uri).with_sub_protocol("wrong-token");
    let result = tokio_tungstenite::connect_async(request).await;
    assert!(result.is_err(), "unauthenticated peer must be rejected");
}

#[tokio::test]
async fn authenticated_browser_is_not_evicted_by_unauthenticated_peer() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _browser = FakeBrowser::connect(ws_port, &token, true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // An unauthenticated peer tries (and fails) to connect.
    let uri = format!("ws://127.0.0.1:{ws_port}/").parse().unwrap();
    let bad = ClientRequestBuilder::new(uri).with_sub_protocol("wrong-token");
    let _ = tokio_tungstenite::connect_async(bad).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The authenticated browser is still serving requests.
    let (http, base, tok) = client(control_port, &token);
    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .json(&serde_json::json!({"url": "/still-here", "method": "GET"}))
        .send()
        .await
        .unwrap();
    let env: Value = resp.json().await.unwrap();
    assert_eq!(env["body"], "echo:/still-here");
}

#[tokio::test]
async fn proxy_forwards_method_and_headers() {
    // Verify the proxy carries method/headers through to the command frame.
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let token2 = token.clone();

    // Custom fake browser that echoes back the received method + a header.
    let uri = format!("ws://127.0.0.1:{ws_port}/").parse().unwrap();
    let request = ClientRequestBuilder::new(uri).with_sub_protocol(&token2);
    let (ws, _r) = tokio_tungstenite::connect_async(request).await.unwrap();
    let (mut sink, mut stream) = ws.split();
    let _h = tokio::spawn(async move {
        while let Some(Ok(Message::Text(txt))) = stream.next().await {
            let cmd: Value = serde_json::from_str(&txt).unwrap();
            let headers: BTreeMap<String, String> =
                serde_json::from_value(cmd["headers"].clone()).unwrap();
            let resp = serde_json::json!({
                "id": cmd["id"].as_u64().unwrap(),
                "status": 200,
                "headers": {},
                "body": format!("{} accept={}", cmd["method"].as_str().unwrap(),
                    headers.get("accept").cloned().unwrap_or_default()),
            });
            sink.send(Message::Text(resp.to_string().into()))
                .await
                .unwrap();
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (http, base, tok) = client(control_port, &token);
    let resp = http
        .post(format!("{base}/api/thing"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .header("accept", "application/json")
        .send()
        .await
        .unwrap();
    let text = resp.text().await.unwrap();
    assert_eq!(text, "POST accept=application/json");
}

/// Eight bytes (a PNG magic number) and their standard-base64 encoding. Used to
/// assert binary bodies survive the bridge byte-for-byte.
const BINARY_BYTES: &[u8] = &[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
const BINARY_B64: &str = "iVBORw0KGgo=";

/// Connects a fake browser that replies to every command with the given body
/// and optional `encoding` tag (`content-type: image/png`). Returns its handle.
fn spawn_reply_browser(
    ws_port: u16,
    token: String,
    body: String,
    encoding: Option<&'static str>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let uri = format!("ws://127.0.0.1:{ws_port}/").parse().unwrap();
        let request = ClientRequestBuilder::new(uri).with_sub_protocol(&token);
        let (ws, _r) = tokio_tungstenite::connect_async(request).await.unwrap();
        let (mut sink, mut stream) = ws.split();
        while let Some(Ok(Message::Text(txt))) = stream.next().await {
            let cmd: Value = serde_json::from_str(&txt).unwrap();
            let mut resp = serde_json::json!({
                "id": cmd["id"].as_u64().unwrap(),
                "status": 200,
                "headers": {"content-type": "image/png"},
                "body": body,
            });
            if let Some(enc) = encoding {
                resp["encoding"] = Value::String(enc.to_string());
            }
            sink.send(Message::Text(resp.to_string().into()))
                .await
                .unwrap();
        }
    })
}

/// Connects a fake browser that replies with a base64-tagged binary body.
fn spawn_binary_browser(ws_port: u16, token: String) -> tokio::task::JoinHandle<()> {
    spawn_reply_browser(ws_port, token, BINARY_B64.to_string(), Some("base64"))
}

#[tokio::test]
async fn transparent_proxy_decodes_base64_to_raw_bytes() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _browser = spawn_binary_browser(ws_port, token.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let resp = http
        .get(format!("{base}/render/panel.png"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "image/png",
        "content-type is forwarded for binary bodies"
    );
    let bytes = resp.bytes().await.unwrap();
    assert_eq!(
        bytes.as_ref(),
        BINARY_BYTES,
        "proxy must hand curl the decoded raw bytes"
    );
}

#[tokio::test]
async fn bridge_request_returns_base64_envelope() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _browser = spawn_binary_browser(ws_port, token.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .json(&serde_json::json!({"url": "/render/panel.png", "method": "GET"}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let env: Value = resp.json().await.unwrap();
    // The full-fidelity endpoint returns the envelope as-is; the caller decodes.
    assert_eq!(env["status"], 200);
    assert_eq!(env["encoding"], "base64");
    assert_eq!(env["body"], BINARY_B64);
}

#[tokio::test]
async fn oversized_decoded_binary_body_is_rejected() {
    // max-body-bytes accounting is against the *decoded* size. The cap (200) sits
    // above the small request JSON (so the request-body limit doesn't fire first)
    // but below the decoded response: "iVBO" is the base64 of a 3-byte group, so
    // repeating it 100× decodes to 300 bytes — comfortably over the cap.
    let (control_port, ws_port, token) = start_bridge_with(None, Duration::from_secs(5), 200).await;
    let _browser = spawn_reply_browser(ws_port, token.clone(), "iVBO".repeat(100), Some("base64"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .json(&serde_json::json!({"url": "/x.png", "method": "GET"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 502);
    // The message names the limit, the observed size, and steers toward paging.
    let body = resp.text().await.unwrap();
    assert!(body.contains("--max-body-bytes"));
    assert!(body.contains("200")); // the configured limit
    assert!(body.contains("300")); // the observed decoded size
    assert!(body.contains("page the request"));
}

#[tokio::test]
async fn invalid_base64_body_is_rejected() {
    // A base64-tagged body that isn't valid base64 fails closed with 502.
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _browser = spawn_reply_browser(
        ws_port,
        token.clone(),
        "@@@ not base64 @@@".into(),
        Some("base64"),
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .json(&serde_json::json!({"url": "/x.png", "method": "GET"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 502);
}

#[tokio::test]
async fn unsupported_encoding_is_rejected() {
    // An `encoding` the server doesn't understand (e.g. `gzip`) fails closed.
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _browser = spawn_reply_browser(ws_port, token.clone(), "anything".into(), Some("gzip"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .json(&serde_json::json!({"url": "/x.png", "method": "GET"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 502);
}

/// Connects a fake browser that, on a `stream: true` command, emits a head frame
/// then one base64 chunk frame per `chunks` entry (sleeping `delay` between them)
/// then a `done` frame. A `cancel` frame from the server flips `cancelled` and
/// stops further chunks — modelling `reader.cancel()`.
///
/// The WebSocket connect is `await`ed before the read loop is spawned, so the
/// browser is guaranteed connected once this returns (matching
/// `FakeBrowser::connect`), avoiding a connect/dispatch race.
async fn spawn_stream_browser(
    ws_port: u16,
    token: String,
    chunks: Vec<Vec<u8>>,
    delay: Duration,
    cancelled: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    let uri = format!("ws://127.0.0.1:{ws_port}/").parse().unwrap();
    let request = ClientRequestBuilder::new(uri).with_sub_protocol(&token);
    let (ws, _r) = tokio_tungstenite::connect_async(request).await.unwrap();
    tokio::spawn(async move {
        let (sink, mut stream) = ws.split();
        let sink = Arc::new(tokio::sync::Mutex::new(sink));
        while let Some(Ok(Message::Text(txt))) = stream.next().await {
            let cmd: Value = serde_json::from_str(&txt).unwrap();
            if cmd["cancel"].as_bool() == Some(true) {
                cancelled.store(true, Ordering::SeqCst);
                continue;
            }
            let id = cmd["id"].as_u64().unwrap();
            let sink = sink.clone();
            let chunks = chunks.clone();
            let cancelled = cancelled.clone();
            tokio::spawn(async move {
                let head = serde_json::json!({
                    "id": id, "status": 200,
                    "headers": {"content-type": "text/event-stream"}, "stream": true,
                });
                sink.lock()
                    .await
                    .send(Message::Text(head.to_string().into()))
                    .await
                    .ok();
                for (seq, chunk) in chunks.iter().enumerate() {
                    if cancelled.load(Ordering::SeqCst) {
                        return;
                    }
                    let frame =
                        serde_json::json!({"id": id, "seq": seq, "chunk": BASE64.encode(chunk)});
                    if sink
                        .lock()
                        .await
                        .send(Message::Text(frame.to_string().into()))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    tokio::time::sleep(delay).await;
                }
                let done = serde_json::json!({"id": id, "done": true});
                sink.lock()
                    .await
                    .send(Message::Text(done.to_string().into()))
                    .await
                    .ok();
            });
        }
    })
}

#[tokio::test]
async fn transparent_proxy_streams_chunks_as_raw_body() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let cancelled = Arc::new(AtomicBool::new(false));
    let _browser = spawn_stream_browser(
        ws_port,
        token.clone(),
        vec![b"data: 1\n\n".to_vec(), b"data: 2\n\n".to_vec()],
        Duration::from_millis(10),
        cancelled,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let resp = http
        .get(format!("{base}/events?__stream=1"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    let body = resp.bytes().await.unwrap();
    assert_eq!(
        body.as_ref(),
        b"data: 1\n\ndata: 2\n\n",
        "proxy must reassemble decoded chunks into the raw body"
    );
}

#[tokio::test]
async fn bridge_request_streams_ndjson() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let cancelled = Arc::new(AtomicBool::new(false));
    let _browser = spawn_stream_browser(
        ws_port,
        token.clone(),
        vec![b"hello ".to_vec(), b"world".to_vec()],
        Duration::from_millis(10),
        cancelled,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .json(&serde_json::json!({"url": "/events", "method": "GET", "stream": true}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/x-ndjson"
    );
    let text = resp.text().await.unwrap();
    let lines: Vec<Value> = text
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // Head line, two chunk lines, a done line.
    assert_eq!(lines[0]["status"], 200);
    assert_eq!(lines[1]["seq"], 0);
    let mut reassembled = Vec::new();
    for line in lines.iter().filter(|l| l.get("chunk").is_some()) {
        reassembled.extend(BASE64.decode(line["chunk"].as_str().unwrap()).unwrap());
    }
    assert_eq!(reassembled, b"hello world");
    assert_eq!(lines.last().unwrap()["done"], true);
}

#[tokio::test]
async fn streaming_cumulative_cap_aborts_and_cancels() {
    // Cap at 300 decoded bytes; the browser offers 3×200-byte chunks. Chunk 1 (200)
    // passes, chunk 2 (cumulative 400) trips the cap → the server aborts and sends
    // a cancel frame the browser records.
    let (control_port, ws_port, token) = start_bridge_with(None, Duration::from_secs(5), 300).await;
    let cancelled = Arc::new(AtomicBool::new(false));
    let _browser = spawn_stream_browser(
        ws_port,
        token.clone(),
        vec![vec![b'a'; 200], vec![b'b'; 200], vec![b'c'; 200]],
        Duration::from_millis(50),
        cancelled.clone(),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let resp = http
        .get(format!("{base}/big?__stream=1"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .send()
        .await
        .unwrap();
    let body = resp.bytes().await.unwrap();
    // Only the first chunk made it through before the cap aborted the stream.
    assert_eq!(body.len(), 200, "stream truncated at the cumulative cap");

    // The browser is told to cancel its reader.
    for _ in 0..50 {
        if cancelled.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        cancelled.load(Ordering::SeqCst),
        "browser must receive a cancel frame when the cap aborts the stream"
    );
}

#[tokio::test]
async fn streaming_idle_timeout_ends_stream() {
    // Idle timeout (request_timeout) of 250ms; the browser stalls 1s between
    // chunks, so the stream ends after the first chunk and the browser is cancelled.
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_millis(250)).await;
    let cancelled = Arc::new(AtomicBool::new(false));
    let _browser = spawn_stream_browser(
        ws_port,
        token.clone(),
        vec![b"first".to_vec(), b"second".to_vec()],
        Duration::from_secs(1),
        cancelled.clone(),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let resp = http
        .get(format!("{base}/slow?__stream=1"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .send()
        .await
        .unwrap();
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), b"first", "stream ends at the idle timeout");
    for _ in 0..50 {
        if cancelled.load(Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(cancelled.load(Ordering::SeqCst), "idle timeout must cancel");
}

#[tokio::test]
async fn cli_request_client_streams() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let cancelled = Arc::new(AtomicBool::new(false));
    let _browser = spawn_stream_browser(
        ws_port,
        token.clone(),
        vec![b"chunk-one ".to_vec(), b"chunk-two".to_vec()],
        Duration::from_millis(10),
        cancelled,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let bin = env!("CARGO_BIN_EXE_omni-dev");
    let out = tokio::process::Command::new(bin)
        .args([
            "browser",
            "bridge",
            "request",
            "--stream",
            "--control-port",
            &control_port.to_string(),
            "--url",
            "/events",
        ])
        .env("OMNI_BRIDGE_TOKEN", &token)
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "client exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // With --stream the decoded body bytes go straight to stdout, in order.
    assert_eq!(out.stdout, b"chunk-one chunk-two");
}

/// Connects a fake browser that, for each command, replies with the supplied
/// `frames` verbatim — with each frame's `id` overwritten to match the command —
/// then stops. Lets a test script abnormal stream sequences (error head, chunk
/// before head, invalid base64, stray head). The connect is `await`ed.
async fn spawn_templated_browser(
    ws_port: u16,
    token: String,
    frames: Vec<Value>,
) -> tokio::task::JoinHandle<()> {
    let uri = format!("ws://127.0.0.1:{ws_port}/").parse().unwrap();
    let request = ClientRequestBuilder::new(uri).with_sub_protocol(&token);
    let (ws, _r) = tokio_tungstenite::connect_async(request).await.unwrap();
    tokio::spawn(async move {
        let (mut sink, mut stream) = ws.split();
        while let Some(Ok(Message::Text(txt))) = stream.next().await {
            let cmd: Value = serde_json::from_str(&txt).unwrap();
            if cmd["cancel"].as_bool() == Some(true) {
                continue;
            }
            let id = cmd["id"].as_u64().unwrap();
            for frame in &frames {
                let mut frame = frame.clone();
                frame["id"] = Value::from(id);
                sink.send(Message::Text(frame.to_string().into()))
                    .await
                    .ok();
            }
        }
    })
}

#[tokio::test]
async fn streaming_proxy_no_browser_returns_503() {
    // No browser connected → the streaming proxy path fails closed with 503.
    let (control_port, _ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let (http, base, tok) = client(control_port, &token);
    let resp = http
        .get(format!("{base}/events?__stream=1"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 503);
}

#[tokio::test]
async fn streaming_request_cross_origin_url_rejected() {
    // An absolute (cross-origin) URL on a streaming request is rejected (403)
    // with no --allow-origin, exactly as for a buffered request.
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _browser = FakeBrowser::connect(ws_port, &token, false).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);
    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .json(&serde_json::json!({"url": "https://evil.test/x", "method": "GET", "stream": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 403);
}

#[tokio::test]
async fn streaming_request_invalid_header_rejected() {
    // A header value containing CR/LF is rejected (400) on the streaming path.
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _browser = FakeBrowser::connect(ws_port, &token, false).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);
    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .json(&serde_json::json!({
            "url": "/events", "method": "GET", "stream": true,
            "headers": {"X-Bad": "a\r\nInjected: 1"}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

#[tokio::test]
async fn streaming_browser_error_head_returns_502() {
    // The browser's first frame is an error → 502 Bad Gateway.
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _browser = spawn_templated_browser(
        ws_port,
        token.clone(),
        vec![serde_json::json!({"error": "fetch blew up"})],
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);
    let resp = http
        .get(format!("{base}/events?__stream=1"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 502);
}

#[tokio::test]
async fn streaming_chunk_before_head_returns_502() {
    // A body chunk before the head frame is a protocol violation → 502.
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _browser = spawn_templated_browser(
        ws_port,
        token.clone(),
        vec![serde_json::json!({"seq": 0, "chunk": "aGk="})],
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);
    let resp = http
        .get(format!("{base}/events?__stream=1"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 502);
}

#[tokio::test]
async fn streaming_start_idle_timeout_returns_504() {
    // The browser connects but never sends a head → the start-of-stream idle
    // timeout fires, returning 504 and cancelling the browser.
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_millis(250)).await;
    let _browser = FakeBrowser::connect(ws_port, &token, false).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);
    let resp = http
        .get(format!("{base}/events?__stream=1"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 504);
}

#[tokio::test]
async fn streaming_invalid_base64_chunk_truncates_body() {
    // A head then an undecodable base64 chunk aborts the stream: the body is empty
    // and the browser is told to cancel.
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _browser = spawn_templated_browser(
        ws_port,
        token.clone(),
        vec![
            serde_json::json!({"status": 200, "headers": {}, "stream": true}),
            serde_json::json!({"seq": 0, "chunk": "@@@ not base64 @@@"}),
            serde_json::json!({"done": true}),
        ],
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);
    let resp = http
        .get(format!("{base}/events?__stream=1"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    assert!(
        resp.bytes().await.unwrap().is_empty(),
        "an undecodable chunk aborts the stream before any body bytes"
    );
}

#[tokio::test]
async fn streaming_stray_head_after_first_is_ignored() {
    // A second head frame mid-stream is ignored; the body is just the real chunk.
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _browser = spawn_templated_browser(
        ws_port,
        token.clone(),
        vec![
            serde_json::json!({"status": 200, "headers": {}, "stream": true}),
            serde_json::json!({"status": 200, "headers": {}, "stream": true}),
            serde_json::json!({"seq": 0, "chunk": BASE64.encode("payload")}),
            serde_json::json!({"done": true}),
        ],
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);
    let resp = http
        .get(format!("{base}/events?__stream=1"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"payload");
}

#[tokio::test]
async fn bridge_survives_unparseable_browser_frame() {
    // A non-JSON frame is logged and dropped; a subsequent valid reply still
    // correlates, proving the reader loop survives the garbage.
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let uri = format!("ws://127.0.0.1:{ws_port}/").parse().unwrap();
    let request = ClientRequestBuilder::new(uri).with_sub_protocol(token.as_str());
    let (ws, _r) = tokio_tungstenite::connect_async(request).await.unwrap();
    tokio::spawn(async move {
        let (mut sink, mut stream) = ws.split();
        // Send garbage first, then a well-formed reply to each command.
        sink.send(Message::Text("not json at all".into()))
            .await
            .ok();
        while let Some(Ok(Message::Text(txt))) = stream.next().await {
            let cmd: Value = serde_json::from_str(&txt).unwrap();
            let resp = serde_json::json!({
                "id": cmd["id"].as_u64().unwrap(), "status": 200,
                "headers": {"content-type": "text/plain"}, "body": "ok",
            });
            sink.send(Message::Text(resp.to_string().into())).await.ok();
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);
    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .json(&serde_json::json!({"url": "/x", "method": "GET"}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let env: Value = resp.json().await.unwrap();
    assert_eq!(env["body"], "ok");
}

/// The `--stream` thin client surfaces a non-2xx bridge response as a non-zero
/// exit (here: no browser connected → 503).
#[tokio::test]
async fn cli_request_client_stream_reports_error() {
    let (control_port, _ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let bin = env!("CARGO_BIN_EXE_omni-dev");
    let out = tokio::process::Command::new(bin)
        .args([
            "browser",
            "bridge",
            "request",
            "--stream",
            "--control-port",
            &control_port.to_string(),
            "--url",
            "/events",
        ])
        .env("OMNI_BRIDGE_TOKEN", &token)
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("503"), "stderr was: {stderr}");
}

/// Drives the real `omni-dev browser bridge request` thin client against a running
/// bridge. Exercises the CLI dispatch and `request::execute` end to end
/// (token from env, header injection, `--header`, `--body @file`, and envelope
/// printing) rather than re-implementing the HTTP call.
#[tokio::test]
async fn cli_request_client_round_trips() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _browser = FakeBrowser::connect(ws_port, &token, true).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let bin = env!("CARGO_BIN_EXE_omni-dev");

    // GET with a custom header.
    let out = tokio::process::Command::new(bin)
        .args([
            "browser",
            "bridge",
            "request",
            "--control-port",
            &control_port.to_string(),
            "--url",
            "/api/labels",
            "--header",
            "Accept: application/json",
        ])
        .env("OMNI_BRIDGE_TOKEN", &token)
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "client exited non-zero: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let env: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(env["status"], 200);
    assert_eq!(env["body"], "echo:/api/labels");

    // POST with a body read from a file (`@path`).
    let dir = tempfile::tempdir().unwrap();
    let payload = dir.path().join("payload.json");
    std::fs::write(&payload, r#"{"q":"x"}"#).unwrap();
    let out = tokio::process::Command::new(bin)
        .args([
            "browser",
            "bridge",
            "request",
            "--control-port",
            &control_port.to_string(),
            "--url",
            "/api/ds/query",
            "--method",
            "POST",
            "--body",
            &format!("@{}", payload.display()),
        ])
        .env("OMNI_BRIDGE_TOKEN", &token)
        .output()
        .await
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let env: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(env["body"], "echo:/api/ds/query");
}

/// The client exits non-zero with a helpful message when no token is available.
#[tokio::test]
async fn cli_request_client_without_token_errors() {
    let bin = env!("CARGO_BIN_EXE_omni-dev");
    let out = tokio::process::Command::new(bin)
        .args(["browser", "bridge", "request", "--url", "/x"])
        .env_remove("OMNI_BRIDGE_TOKEN")
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("OMNI_BRIDGE_TOKEN"), "stderr was: {stderr}");
}

/// The client surfaces a non-2xx bridge response as a non-zero exit.
#[tokio::test]
async fn cli_request_client_reports_bridge_error() {
    // Bridge running, but no browser connected → control plane returns 503.
    let (control_port, _ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let bin = env!("CARGO_BIN_EXE_omni-dev");
    let out = tokio::process::Command::new(bin)
        .args([
            "browser",
            "bridge",
            "request",
            "--control-port",
            &control_port.to_string(),
            "--url",
            "/x",
        ])
        .env("OMNI_BRIDGE_TOKEN", &token)
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
}

// ── Multi-tab connection routing (#908) ──────────────────────────────

/// Fetches `/__bridge/status` and returns the parsed JSON body.
async fn fetch_status(http: &reqwest::Client, base: &str, tok: &str) -> Value {
    http.get(format!("{base}/__bridge/status"))
        .bearer_auth(tok)
        .header("x-omni-bridge", "1")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Two authenticated tabs connect and both appear in status; neither evicts the
/// other (the per-connection non-eviction guarantee).
#[tokio::test]
async fn two_tabs_coexist_in_status() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _a = FakeBrowser::connect_tagged(ws_port, &token, "https://a.test", "A").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _b = FakeBrowser::connect_tagged(ws_port, &token, "https://b.test", "B").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let body = fetch_status(&http, &base, &tok).await;
    assert_eq!(body["connected"], Value::Bool(true));
    let tabs = body["tabs"].as_array().unwrap();
    assert_eq!(tabs.len(), 2);
    let origins: Vec<&str> = tabs.iter().map(|t| t["origin"].as_str().unwrap()).collect();
    assert!(origins.contains(&"https://a.test") && origins.contains(&"https://b.test"));
    // `browser_origin` is ambiguous with two tabs, so it is omitted.
    assert!(body.get("browser_origin").is_none() || body["browser_origin"].is_null());
}

/// With two tabs connected and no target, the request is rejected (409) and the
/// message lists the connected tabs.
#[tokio::test]
async fn two_tabs_no_target_is_409() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _a = FakeBrowser::connect_tagged(ws_port, &token, "https://a.test", "A").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _b = FakeBrowser::connect_tagged(ws_port, &token, "https://b.test", "B").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .json(&serde_json::json!({"url": "/x", "method": "GET"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 409);
}

/// A request targeted by `Origin` routes to the matching tab.
#[tokio::test]
async fn route_by_origin_selects_the_right_tab() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _a = FakeBrowser::connect_tagged(ws_port, &token, "https://a.test", "A").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _b = FakeBrowser::connect_tagged(ws_port, &token, "https://b.test", "B").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    // Via the `target` body field.
    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .json(&serde_json::json!({"url": "/p", "method": "GET", "target": "https://b.test"}))
        .send()
        .await
        .unwrap();
    let env: Value = resp.json().await.unwrap();
    assert_eq!(env["body"], "tab:B:/p");
}

/// A request targeted by connection id (read from status) routes to that tab,
/// supplied via the `X-Omni-Bridge-Target` header and the transparent proxy.
#[tokio::test]
async fn route_by_id_header_via_proxy() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _a = FakeBrowser::connect_tagged(ws_port, &token, "https://a.test", "A").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _b = FakeBrowser::connect_tagged(ws_port, &token, "https://b.test", "B").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    // Learn which id maps to origin a.test from status.
    let status = fetch_status(&http, &base, &tok).await;
    let id_a = status["tabs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["origin"] == "https://a.test")
        .unwrap()["id"]
        .as_u64()
        .unwrap();

    let resp = http
        .get(format!("{base}/proxied"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .header("x-omni-bridge-target", id_a.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "tab:A:/proxied");
}

/// An unknown connection id is a 404.
#[tokio::test]
async fn unknown_target_id_is_404() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _a = FakeBrowser::connect_tagged(ws_port, &token, "https://a.test", "A").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .json(&serde_json::json!({"url": "/x", "method": "GET", "target": "999"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

/// A single connected tab still routes without a target (v1 back-compat), and
/// status reports it via the legacy `browser_origin` field.
#[tokio::test]
async fn single_tab_back_compat() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _a = FakeBrowser::connect_tagged(ws_port, &token, "https://only.test", "A").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    let status = fetch_status(&http, &base, &tok).await;
    assert_eq!(status["browser_origin"], "https://only.test");
    assert_eq!(status["tabs"].as_array().unwrap().len(), 1);

    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .json(&serde_json::json!({"url": "/x", "method": "GET"}))
        .send()
        .await
        .unwrap();
    let env: Value = resp.json().await.unwrap();
    assert_eq!(env["body"], "tab:A:/x");
}

/// A malformed JSON body to `POST /__bridge/request` is rejected with 400.
#[tokio::test]
async fn request_invalid_json_body_is_400() {
    let (control_port, _ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let (http, base, tok) = client(control_port, &token);
    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .header("content-type", "application/json")
        .body("not valid json {")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
}

/// On `POST /__bridge/request`, the `X-Omni-Bridge-Target` header selects the
/// tab and overrides a conflicting `target` body field.
#[tokio::test]
async fn request_target_header_overrides_body_field() {
    let (control_port, ws_port, token) = start_bridge(None, Duration::from_secs(5)).await;
    let _a = FakeBrowser::connect_tagged(ws_port, &token, "https://a.test", "A").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let _b = FakeBrowser::connect_tagged(ws_port, &token, "https://b.test", "B").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let (http, base, tok) = client(control_port, &token);

    // Body targets A, header targets B; the header wins.
    let resp = http
        .post(format!("{base}/__bridge/request"))
        .bearer_auth(&tok)
        .header("x-omni-bridge", "1")
        .header("x-omni-bridge-target", "https://b.test")
        .json(&serde_json::json!({"url": "/p", "method": "GET", "target": "https://a.test"}))
        .send()
        .await
        .unwrap();
    let env: Value = resp.json().await.unwrap();
    assert_eq!(env["body"], "tab:B:/p");
}
