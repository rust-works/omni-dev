//! The long-lived bridge server.
//!
//! A WebSocket plane the browser connects to and an HTTP control plane the
//! operator drives, joined by an `id`-keyed correlator.
//!
//! A request flows: control plane (authenticated) → assign `id` + register a
//! `oneshot` waiter → serialise a [`Command`] frame → WebSocket → browser
//! `fetch()` → [`BrowserReply`] frame → correlator resolves the waiter by `id`
//! → control plane returns the HTTP response.

use std::collections::{BTreeMap, HashMap};
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex, Semaphore};
use tokio_tungstenite::tungstenite::Message;

use crate::browser::auth;
use crate::browser::protocol::{
    BrowserReply, Command, ControlRequest, ReplyOutcome, ResponseEnvelope, StatusResponse,
};
use crate::browser::snippet;

/// Resolved runtime configuration for a bridge instance.
#[derive(Debug, Clone)]
pub struct BridgeConfig {
    /// WebSocket-plane port (`0` binds an OS-assigned free port).
    pub ws_port: u16,
    /// HTTP control-plane port (`0` binds an OS-assigned free port).
    pub control_port: u16,
    /// Per-request timeout before the control plane returns `504`.
    pub request_timeout: Duration,
    /// Optional cross-origin allowlist for both the WS upgrade and outbound URLs.
    pub allow_origin: Option<String>,
    /// Maximum browser response body size accepted, in bytes.
    pub max_body_bytes: usize,
    /// Maximum number of concurrent in-flight requests.
    pub max_concurrent: usize,
}

/// `id → oneshot` waiter registry plus the monotonic id counter.
#[derive(Clone)]
struct Correlator {
    pending: Arc<StdMutex<HashMap<u64, oneshot::Sender<BrowserReply>>>>,
    next_id: Arc<AtomicU64>,
}

impl Correlator {
    fn new() -> Self {
        Self {
            pending: Arc::new(StdMutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Allocates an id and registers a waiter for its reply.
    fn register(&self) -> (u64, oneshot::Receiver<BrowserReply>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.lock().insert(id, tx);
        (id, rx)
    }

    /// Drops a waiter without resolving it (timeout / send failure cleanup).
    fn remove(&self, id: u64) {
        self.lock().remove(&id);
    }

    /// Resolves the waiter for `reply.id`, if one is still registered.
    fn resolve(&self, reply: BrowserReply) {
        let waiter = self.lock().remove(&reply.id);
        if let Some(tx) = waiter {
            let _ = tx.send(reply);
        }
    }

    fn pending_count(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, oneshot::Sender<BrowserReply>>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The single authenticated browser connection currently held.
struct WsConn {
    /// Unique id so a stale disconnect doesn't clear a newer connection.
    conn_id: u64,
    /// Outbound message channel to this connection's writer task.
    sender: mpsc::UnboundedSender<Message>,
    /// The connecting tab's `Origin`, if it sent one.
    origin: Option<String>,
}

/// Shared server state, cloned into every handler and task.
#[derive(Clone)]
struct AppState {
    token: Arc<String>,
    config: Arc<BridgeConfig>,
    correlator: Correlator,
    ws: Arc<Mutex<Option<WsConn>>>,
    in_flight: Arc<Semaphore>,
    conn_counter: Arc<AtomicU64>,
}

/// Binds both planes (fail-closed) and serves until the process is stopped.
///
/// `token` is the already-resolved session token (never sourced from argv).
pub async fn run(mut config: BridgeConfig, token: String) -> Result<()> {
    let control_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, config.control_port))
        .await
        .with_context(|| {
            format!(
                "Failed to bind control plane to 127.0.0.1:{} (already in use?)",
                config.control_port
            )
        })?;
    let ws_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, config.ws_port))
        .await
        .with_context(|| {
            format!(
                "Failed to bind WebSocket plane to 127.0.0.1:{} (already in use?)",
                config.ws_port
            )
        })?;

    // Read back the OS-assigned ports so port-0 (random) is reflected
    // everywhere: the snippet, the printed instructions, and the Host check.
    config.control_port = control_listener.local_addr()?.port();
    config.ws_port = ws_listener.local_addr()?.port();

    let token = Arc::new(token);
    let state = AppState {
        token: token.clone(),
        config: Arc::new(config.clone()),
        correlator: Correlator::new(),
        ws: Arc::new(Mutex::new(None)),
        in_flight: Arc::new(Semaphore::new(config.max_concurrent)),
        conn_counter: Arc::new(AtomicU64::new(1)),
    };

    print_startup(&config, &token);

    // WebSocket accept loop.
    let ws_state = state.clone();
    tokio::spawn(async move {
        loop {
            match ws_listener.accept().await {
                Ok((stream, _peer)) => {
                    tokio::spawn(handle_ws_conn(stream, ws_state.clone()));
                }
                Err(e) => tracing::warn!("WebSocket accept error: {e}"),
            }
        }
    });

    let app = control_router(state, config.max_body_bytes);
    axum::serve(control_listener, app)
        .await
        .context("Control-plane server error")
}

/// Prints the bound ports, session token, and paste-ready snippet to stdout.
fn print_startup(config: &BridgeConfig, token: &str) {
    let snippet = snippet::render(config.ws_port, token);
    println!("omni-dev browser bridge");
    println!("  control plane : http://127.0.0.1:{}", config.control_port);
    println!("  websocket     : ws://127.0.0.1:{}", config.ws_port);
    println!("  session token : {token}");
    if let Some(origin) = &config.allow_origin {
        println!("  allow-origin  : {origin}");
    }
    println!();
    println!("Paste this into the DevTools console of the authenticated tab:");
    println!();
    println!("{snippet}");
    println!();
    println!("Then drive requests, e.g.:");
    println!(
        "  omni-dev browser request --control-port {} --url /path",
        config.control_port
    );
}

// ── WebSocket plane ──────────────────────────────────────────────────

/// Handles one inbound TCP connection on the WebSocket plane: authenticates the
/// upgrade, registers the connection, and pumps replies into the correlator.
//
// `clippy::result_large_err`: the handshake callback's `Result<Response,
// ErrorResponse>` return type is dictated by `tungstenite::accept_hdr_async`;
// `ErrorResponse` is a large `http::Response`, but the signature is not ours to
// change.
#[allow(clippy::result_large_err)]
async fn handle_ws_conn(stream: TcpStream, state: AppState) {
    use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};

    let token = state.token.clone();
    let allow_origin = state.config.allow_origin.clone();
    let captured_origin: Arc<StdMutex<Option<String>>> = Arc::new(StdMutex::new(None));
    let co = captured_origin.clone();

    let callback =
        move |req: &Request, mut response: Response| -> Result<Response, ErrorResponse> {
            let origin = req
                .headers()
                .get("origin")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);

            if !auth::ws_origin_allowed(origin.as_deref(), allow_origin.as_deref()) {
                tracing::warn!("Rejected WebSocket upgrade: origin not allowed");
                return Err(ws_error(StatusCode::FORBIDDEN, "origin not allowed"));
            }

            let protocols: Vec<String> = req
                .headers()
                .get_all("sec-websocket-protocol")
                .iter()
                .filter_map(|v| v.to_str().ok())
                .flat_map(|s| s.split(',').map(|p| p.trim().to_string()))
                .collect();

            let Some(matched) =
                auth::ws_subprotocol_token(protocols.iter().map(String::as_str), &token)
            else {
                tracing::warn!("Rejected WebSocket upgrade: missing or invalid token");
                return Err(ws_error(
                    StatusCode::UNAUTHORIZED,
                    "missing or invalid token",
                ));
            };
            if let Ok(value) = matched.parse() {
                response
                    .headers_mut()
                    .insert("sec-websocket-protocol", value);
            }
            *co.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = origin;
            Ok(response)
        };

    let ws_stream = match tokio_tungstenite::accept_hdr_async(stream, callback).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("WebSocket handshake failed: {e}");
            return;
        }
    };

    let origin = captured_origin
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let conn_id = state.conn_counter.fetch_add(1, Ordering::Relaxed);
    tracing::info!(
        "Browser connected (conn {conn_id}{})",
        origin
            .as_deref()
            .map(|o| format!(", origin {o}"))
            .unwrap_or_default()
    );

    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    {
        let mut guard = state.ws.lock().await;
        *guard = Some(WsConn {
            conn_id,
            sender: tx,
            origin,
        });
    }

    let (mut sink, mut read) = ws_stream.split();
    let writer = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(msg)) = read.next().await {
        match msg {
            Message::Text(txt) => match serde_json::from_str::<BrowserReply>(&txt) {
                Ok(reply) => state.correlator.resolve(reply),
                Err(e) => tracing::debug!("Unparseable browser reply: {e}"),
            },
            Message::Close(_) => break,
            _ => {}
        }
    }

    writer.abort();
    // Only clear the hub if it still points at *this* connection.
    let mut guard = state.ws.lock().await;
    if guard.as_ref().is_some_and(|c| c.conn_id == conn_id) {
        *guard = None;
        tracing::info!("Browser disconnected (conn {conn_id})");
    }
}

fn ws_error(
    code: StatusCode,
    msg: &str,
) -> tokio_tungstenite::tungstenite::handshake::server::ErrorResponse {
    let mut resp = tokio_tungstenite::tungstenite::http::Response::new(Some(msg.to_string()));
    *resp.status_mut() = code;
    resp
}

// ── HTTP control plane ───────────────────────────────────────────────

fn control_router(state: AppState, max_body_bytes: usize) -> Router {
    Router::new()
        .route("/__bridge/status", get(status_handler))
        .route("/__bridge/request", post(request_handler))
        .fallback(proxy_handler)
        .layer(DefaultBodyLimit::max(max_body_bytes))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Enforces the control-plane trust boundary on every request: bearer token,
/// `X-Omni-Bridge: 1`, `Host` allowlist, and rejection of browser-originated
/// requests. Emits no CORS headers and refuses `OPTIONS`.
async fn guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    // Never answer OPTIONS (would be a CORS preflight); legitimate CLI clients
    // do not send it.
    if request.method() == axum::http::Method::OPTIONS {
        return (StatusCode::METHOD_NOT_ALLOWED, "OPTIONS not allowed").into_response();
    }

    let headers = request.headers();
    let get = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());

    let host = get(header::HOST.as_str()).unwrap_or("");
    if !auth::host_allowed(host, state.config.control_port) {
        tracing::warn!("Rejected control-plane request: disallowed Host");
        return (StatusCode::BAD_REQUEST, "host not allowed").into_response();
    }

    if auth::is_browser_originated(get("origin"), get("sec-fetch-site")) {
        tracing::warn!("Rejected control-plane request: browser-originated");
        return (
            StatusCode::FORBIDDEN,
            "browser-originated requests are denied",
        )
            .into_response();
    }

    if !auth::has_bridge_header(get(auth::BRIDGE_HEADER)) {
        return (StatusCode::FORBIDDEN, "missing X-Omni-Bridge: 1").into_response();
    }

    if !auth::bearer_matches(get(header::AUTHORIZATION.as_str()), &state.token) {
        return (StatusCode::UNAUTHORIZED, "invalid or missing bearer token").into_response();
    }

    next.run(request).await
}

async fn status_handler(State(state): State<AppState>) -> Json<StatusResponse> {
    let (connected, browser_origin) = {
        let guard = state.ws.lock().await;
        match guard.as_ref() {
            Some(conn) => (true, conn.origin.clone()),
            None => (false, None),
        }
    };
    Json(StatusResponse {
        connected,
        browser_origin,
        pending: state.correlator.pending_count(),
    })
}

/// `POST /__bridge/request` — full-fidelity control endpoint.
async fn request_handler(State(state): State<AppState>, body: Bytes) -> Response {
    let req: ControlRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")).into_response()
        }
    };
    match dispatch(&state, req).await {
        Ok(env) => Json(env).into_response(),
        Err((code, msg)) => (code, msg).into_response(),
    }
}

/// Transparent proxy for any path not under `/__bridge/`.
async fn proxy_handler(State(state): State<AppState>, request: Request) -> Response {
    let (parts, body) = request.into_parts();

    let path = parts.uri.path();
    if auth::normalize_request_path(path).is_none() {
        return (StatusCode::BAD_REQUEST, "unsafe request path").into_response();
    }
    let url = match parts.uri.query() {
        Some(q) => format!("{path}?{q}"),
        None => path.to_string(),
    };

    let headers = forwardable_headers(&parts.headers);

    let Ok(body_bytes) = axum::body::to_bytes(body, state.config.max_body_bytes).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "request body too large").into_response();
    };
    let body = if body_bytes.is_empty() {
        None
    } else {
        Some(String::from_utf8_lossy(&body_bytes).into_owned())
    };

    let req = ControlRequest {
        url,
        method: parts.method.to_string(),
        headers,
        body,
    };

    match dispatch(&state, req).await {
        Ok(env) => envelope_to_response(env),
        Err((code, msg)) => (code, msg).into_response(),
    }
}

/// Copies request headers safe to forward to the browser, dropping the
/// bridge-control and hop-by-hop headers a CLI client adds.
fn forwardable_headers(headers: &axum::http::HeaderMap) -> BTreeMap<String, String> {
    const DROP: &[&str] = &[
        "host",
        "authorization",
        auth::BRIDGE_HEADER,
        "content-length",
        "connection",
        "accept-encoding",
        "origin",
        "sec-fetch-site",
        "sec-fetch-mode",
        "sec-fetch-dest",
    ];
    headers
        .iter()
        .filter_map(|(k, v)| {
            let name = k.as_str();
            if DROP.contains(&name) {
                return None;
            }
            v.to_str()
                .ok()
                .map(|val| (name.to_string(), val.to_string()))
        })
        .collect()
}

/// The shared request path: scope-check, register a waiter, send the command,
/// and await the browser's reply (or time out).
async fn dispatch(
    state: &AppState,
    req: ControlRequest,
) -> Result<ResponseEnvelope, (StatusCode, String)> {
    auth::validate_outbound_url(&req.url, state.config.allow_origin.as_deref()).map_err(|_| {
        (
            StatusCode::FORBIDDEN,
            "outbound URL is cross-origin; pass --allow-origin to permit it".to_string(),
        )
    })?;

    for (name, value) in &req.headers {
        if !auth::header_is_safe(name, value) {
            return Err((
                StatusCode::BAD_REQUEST,
                "invalid header name or value".to_string(),
            ));
        }
    }

    let _permit = state.in_flight.clone().try_acquire_owned().map_err(|_| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            "too many in-flight requests".to_string(),
        )
    })?;

    let (id, rx) = state.correlator.register();
    let command = Command {
        id,
        url: req.url,
        method: req.method,
        headers: req.headers,
        body: req.body,
    };
    let frame = match serde_json::to_string(&command) {
        Ok(f) => f,
        Err(e) => {
            state.correlator.remove(id);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("serialise error: {e}"),
            ));
        }
    };

    {
        let guard = state.ws.lock().await;
        match guard.as_ref() {
            Some(conn) if conn.sender.send(Message::Text(frame)).is_ok() => {}
            _ => {
                state.correlator.remove(id);
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "no browser connected".to_string(),
                ));
            }
        }
    }

    match tokio::time::timeout(state.config.request_timeout, rx).await {
        Ok(Ok(reply)) => match reply.outcome() {
            ReplyOutcome::Success {
                status,
                headers,
                body,
            } => {
                if body.len() > state.config.max_body_bytes {
                    return Err((
                        StatusCode::BAD_GATEWAY,
                        "browser response body exceeds --max-body-bytes".to_string(),
                    ));
                }
                Ok(ResponseEnvelope {
                    id,
                    status,
                    headers,
                    body,
                })
            }
            ReplyOutcome::Error(msg) => Err((
                StatusCode::BAD_GATEWAY,
                format!("browser fetch failed: {msg}"),
            )),
        },
        Ok(Err(_)) => Err((
            StatusCode::BAD_GATEWAY,
            "browser connection closed before replying".to_string(),
        )),
        Err(_) => {
            state.correlator.remove(id);
            Err((
                StatusCode::GATEWAY_TIMEOUT,
                "browser did not reply in time".to_string(),
            ))
        }
    }
}

/// Renders a browser response envelope as the transparent-proxy HTTP response.
fn envelope_to_response(env: ResponseEnvelope) -> Response {
    let status = StatusCode::from_u16(env.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    if let Some(ct) = env.headers.get("content-type") {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }
    builder
        .body(Body::from(env.body))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn correlator_register_resolve_round_trip() {
        let c = Correlator::new();
        let (id, rx) = c.register();
        assert_eq!(c.pending_count(), 1);
        c.resolve(BrowserReply {
            id,
            status: Some(200),
            headers: None,
            body: Some("ok".into()),
            error: None,
        });
        assert_eq!(c.pending_count(), 0);
        let reply = rx.now_or_never().unwrap().unwrap();
        assert_eq!(reply.id, id);
    }

    #[test]
    fn correlator_ids_are_monotonic() {
        let c = Correlator::new();
        let (a, _ra) = c.register();
        let (b, _rb) = c.register();
        assert!(b > a);
    }

    #[test]
    fn correlator_remove_drops_waiter() {
        let c = Correlator::new();
        let (id, _rx) = c.register();
        c.remove(id);
        assert_eq!(c.pending_count(), 0);
    }

    #[test]
    fn forwardable_headers_drops_control_headers() {
        let mut h = axum::http::HeaderMap::new();
        h.insert("host", "localhost:9998".parse().unwrap());
        h.insert("authorization", "Bearer x".parse().unwrap());
        h.insert("x-omni-bridge", "1".parse().unwrap());
        h.insert("accept", "application/json".parse().unwrap());
        let out = forwardable_headers(&h);
        assert!(!out.contains_key("host"));
        assert!(!out.contains_key("authorization"));
        assert!(!out.contains_key("x-omni-bridge"));
        assert_eq!(
            out.get("accept").map(String::as_str),
            Some("application/json")
        );
    }

    use futures::FutureExt;
}
