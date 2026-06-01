//! End-to-end tests for the browser bridge.
//!
//! Each test boots a real bridge on OS-assigned ports (`--ws-port 0` /
//! `--control-port 0`), connects a fake "browser" over the WebSocket plane
//! presenting the token subprotocol, and drives the HTTP control plane with
//! `reqwest`. The fake browser echoes a canned response so request/reply
//! correlation can be asserted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::time::Duration;

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
    let _guard = START_LOCK.lock().await;

    // Reserve two OS-assigned ports, then hand them to the bridge. The lock
    // above closes the reserve→rebind race that this drop-then-bind creates.
    let c = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let w = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let control_port = c.local_addr().unwrap().port();
    let ws_port = w.local_addr().unwrap().port();
    drop(c);
    drop(w);

    let token = "test-token-abcdef".to_string();
    let config = BridgeConfig {
        ws_port,
        control_port,
        request_timeout: timeout,
        allow_origin,
        max_body_bytes,
        max_concurrent: 64,
    };
    let token_clone = token.clone();
    tokio::spawn(async move {
        let _ = browser::run(config, token_clone).await;
    });

    // Wait until BOTH planes accept connections before releasing the lock.
    wait_until_listening(control_port).await;
    wait_until_listening(ws_port).await;
    (control_port, ws_port, token)
}

async fn wait_until_listening(port: u16) {
    for _ in 0..100 {
        if tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("bridge plane never came up on port {port}");
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
                        sink.send(Message::Text(resp.to_string())).await.unwrap();
                    }
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
            sink.send(Message::Text(resp.to_string())).await.unwrap();
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
            sink.send(Message::Text(resp.to_string())).await.unwrap();
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

/// Drives the real `omni-dev browser request` thin client against a running
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
        .args(["browser", "request", "--url", "/x"])
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
