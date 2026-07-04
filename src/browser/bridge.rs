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
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

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
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use futures::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use crate::browser::auth;
use crate::browser::protocol::{
    BrowserFrame, BrowserReply, CancelCommand, Command, ControlRequest, ReplyOutcome,
    ResponseEnvelope, StatusResponse, StreamItem, StreamLine, TabInfo,
};
use crate::browser::snippet;
use crate::request_log;

/// Default WebSocket-plane port.
pub const DEFAULT_WS_PORT: u16 = 9999;
/// Default HTTP control-plane port.
pub const DEFAULT_CONTROL_PORT: u16 = 9998;
/// Default per-request timeout, in seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Default maximum browser response body size (8 MiB).
pub const DEFAULT_MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
/// Default maximum concurrent in-flight requests.
pub const DEFAULT_MAX_CONCURRENT: usize = 64;

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

impl Default for BridgeConfig {
    /// The documented default ports and limits, shared by the `serve` CLI and
    /// the daemon-hosted bridge so the two never drift.
    fn default() -> Self {
        Self {
            ws_port: DEFAULT_WS_PORT,
            control_port: DEFAULT_CONTROL_PORT,
            request_timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            allow_origin: None,
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            max_concurrent: DEFAULT_MAX_CONCURRENT,
        }
    }
}

/// A registered waiter for a given id: either a buffered one-shot (resolved by a
/// single reply) or a stream (fed many [`StreamItem`]s until `End`/`Error`).
enum Waiter {
    /// Buffered request: one reply resolves it.
    Buffered(oneshot::Sender<BrowserReply>),
    /// Streamed request: head + chunk + terminator items are forwarded here.
    Stream(mpsc::UnboundedSender<StreamItem>),
}

/// `id → waiter` registry plus the monotonic id counter.
#[derive(Clone)]
struct Correlator {
    pending: Arc<StdMutex<HashMap<u64, Waiter>>>,
    next_id: Arc<AtomicU64>,
}

impl Correlator {
    fn new() -> Self {
        Self {
            pending: Arc::new(StdMutex::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Allocates an id and registers a buffered waiter for its single reply.
    fn register(&self) -> (u64, oneshot::Receiver<BrowserReply>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.lock().insert(id, Waiter::Buffered(tx));
        (id, rx)
    }

    /// Allocates an id and registers a stream waiter that receives every
    /// [`StreamItem`] of the response until `End`/`Error`.
    fn register_stream(&self) -> (u64, mpsc::UnboundedReceiver<StreamItem>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        self.lock().insert(id, Waiter::Stream(tx));
        (id, rx)
    }

    /// Drops a waiter without resolving it (timeout / send failure cleanup).
    fn remove(&self, id: u64) {
        self.lock().remove(&id);
    }

    /// Routes an inbound browser frame to its waiter.
    ///
    /// Buffered waiters resolve and are removed; stream waiters receive one
    /// [`StreamItem`] and are removed only on a terminal item (or when their
    /// consumer has gone). Returns `Some(id)` when the browser should be told to
    /// cancel that stream (its control-plane consumer disconnected).
    fn deliver(&self, frame: BrowserFrame) -> Option<u64> {
        let id = frame.id;
        let mut guard = self.lock();
        match guard.get(&id) {
            Some(Waiter::Buffered(_)) => {
                if let Some(Waiter::Buffered(tx)) = guard.remove(&id) {
                    let _ = tx.send(frame.into_reply());
                }
                None
            }
            Some(Waiter::Stream(_)) => {
                let item = frame.stream_item();
                let terminal = matches!(item, StreamItem::End | StreamItem::Error(_));
                let send_failed = match guard.get(&id) {
                    Some(Waiter::Stream(tx)) => tx.send(item).is_err(),
                    _ => false,
                };
                if terminal || send_failed {
                    guard.remove(&id);
                }
                send_failed.then_some(id)
            }
            None => None,
        }
    }

    fn pending_count(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, Waiter>> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One authenticated browser connection in the registry.
struct WsConn {
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
    /// Connected tabs keyed by connection id (the public routing selector). A
    /// new authenticated connection never displaces an existing one — each lives
    /// under its own key — so the non-eviction guarantee holds per-connection.
    ///
    /// A `std::sync::Mutex` (not tokio's): the guard is never held across an
    /// `.await`, so a synchronous lock keeps [`AppState::status_snapshot`] and
    /// [`BridgeServer::disconnect_tab`] non-async — matching [`Correlator`].
    tabs: Arc<StdMutex<HashMap<u64, WsConn>>>,
    in_flight: Arc<Semaphore>,
    conn_counter: Arc<AtomicU64>,
}

impl AppState {
    /// Locks the tab registry, recovering from a poisoned mutex (mirrors
    /// [`Correlator::lock`]).
    fn lock_tabs(&self) -> std::sync::MutexGuard<'_, HashMap<u64, WsConn>> {
        self.tabs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Builds a [`StatusResponse`] snapshot of the connected tabs and pending
    /// request count. Shared by the `GET /__bridge/status` handler and
    /// [`BridgeServer::status`] so neither makes an internal HTTP self-call.
    fn status_snapshot(&self) -> StatusResponse {
        let mut tabs: Vec<TabInfo> = {
            let guard = self.lock_tabs();
            guard
                .iter()
                .map(|(id, conn)| TabInfo {
                    id: *id,
                    origin: conn.origin.clone(),
                })
                .collect()
        };
        tabs.sort_by_key(|t| t.id);
        // `browser_origin` is v1 back-compat: meaningful only when exactly one
        // tab is connected; ambiguous (so `None`) for zero or several.
        let browser_origin = match tabs.as_slice() {
            [only] => only.origin.clone(),
            _ => None,
        };
        StatusResponse {
            connected: !tabs.is_empty(),
            browser_origin,
            tabs,
            pending: self.correlator.pending_count(),
        }
    }
}

/// A running bridge instance: both loopback-TCP planes bound and serving, with
/// a [`CancellationToken`] for graceful shutdown.
///
/// Created by [`BridgeServer::start`], which returns once the planes are bound
/// (fail-closed) and the accept loops are spawned. A standalone `serve` calls
/// [`wait`](Self::wait) (run until Ctrl-C); a supervisor (the daemon) instead
/// drives [`status`](Self::status) / [`disconnect_tab`](Self::disconnect_tab) /
/// [`shutdown`](Self::shutdown) directly.
pub struct BridgeServer {
    state: AppState,
    control_port: u16,
    ws_port: u16,
    shutdown: CancellationToken,
    tasks: JoinSet<()>,
}

/// Binds a loopback TCP listener with `SO_REUSEADDR` so the bridge can rebind
/// its fixed ports immediately after a restart even when a just-closed
/// connection still holds the address in `TIME_WAIT` (#990). `SO_REUSEADDR`
/// does *not* permit a second live listener on the same address, so the
/// fail-closed-on-a-live-squatter guarantee (see
/// `start_fails_closed_on_taken_control_port`) is unchanged.
fn bind_loopback_reuse(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(1024) // matches tokio's default `TcpListener::bind` backlog
}

impl BridgeServer {
    /// Binds both planes (fail-closed), spawns the accept loops, and returns
    /// immediately. `token` is the already-resolved session token (never
    /// sourced from argv).
    pub async fn start(mut config: BridgeConfig, token: String) -> Result<Self> {
        let control_listener =
            bind_loopback_reuse(SocketAddr::from((Ipv4Addr::LOCALHOST, config.control_port)))
                .with_context(|| {
                    format!(
                        "Failed to bind control plane to 127.0.0.1:{} (already in use?)",
                        config.control_port
                    )
                })?;
        let ws_listener =
            bind_loopback_reuse(SocketAddr::from((Ipv4Addr::LOCALHOST, config.ws_port)))
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
        let control_port = config.control_port;
        let ws_port = config.ws_port;
        let max_body_bytes = config.max_body_bytes;
        let max_concurrent = config.max_concurrent;

        // Also accept the WebSocket plane on IPv6 loopback (`::1`) at the same
        // port, best-effort. A browser connecting to `ws://localhost` may resolve
        // it to `::1` rather than `127.0.0.1`; binding both means the pasted
        // snippet connects either way. Still loopback-only — ADR-0036 unchanged.
        let ws_listener_v6 =
            bind_loopback_reuse(SocketAddr::from((Ipv6Addr::LOCALHOST, ws_port))).ok();

        let state = AppState {
            token: Arc::new(token),
            config: Arc::new(config),
            correlator: Correlator::new(),
            tabs: Arc::new(StdMutex::new(HashMap::new())),
            in_flight: Arc::new(Semaphore::new(max_concurrent)),
            conn_counter: Arc::new(AtomicU64::new(1)),
        };

        let shutdown = CancellationToken::new();
        let mut tasks = JoinSet::new();

        // WebSocket accept loops (cancellable), one per bound loopback address.
        spawn_ws_accept(&mut tasks, ws_listener, state.clone(), shutdown.clone());
        if let Some(listener) = ws_listener_v6 {
            spawn_ws_accept(&mut tasks, listener, state.clone(), shutdown.clone());
        } else {
            tracing::debug!("IPv6 loopback (::1) WebSocket bind unavailable; IPv4 only");
        }

        // Control plane (axum) with graceful shutdown: drains in-flight requests
        // before the serve future resolves.
        let control_state = state.clone();
        let control_shutdown = shutdown.clone();
        tasks.spawn(async move {
            let app = control_router(control_state, max_body_bytes);
            if let Err(e) = axum::serve(control_listener, app)
                .with_graceful_shutdown(control_shutdown.cancelled_owned())
                .await
            {
                tracing::warn!("Control-plane server error: {e}");
            }
        });

        Ok(Self {
            state,
            control_port,
            ws_port,
            shutdown,
            tasks,
        })
    }

    /// The bound control-plane port (resolved if `0` was requested).
    pub fn control_port(&self) -> u16 {
        self.control_port
    }

    /// The bound WebSocket-plane port (resolved if `0` was requested).
    pub fn ws_port(&self) -> u16 {
        self.ws_port
    }

    /// A synchronous snapshot of connection status (the same payload the
    /// `GET /__bridge/status` endpoint returns).
    pub fn status(&self) -> StatusResponse {
        self.state.status_snapshot()
    }

    /// Disconnects the tab with the given connection id: sends a WebSocket
    /// Close and drops it from the registry. Errors if no such tab is connected.
    pub fn disconnect_tab(&self, id: u64) -> Result<()> {
        let mut tabs = self.state.lock_tabs();
        match tabs.remove(&id) {
            Some(conn) => {
                let _ = conn.sender.send(Message::Close(None));
                Ok(())
            }
            None => anyhow::bail!("no connected tab with id {id}"),
        }
    }

    /// Runs until Ctrl-C, then shuts down gracefully. Used by standalone
    /// `serve` (a supervisor calls [`shutdown`](Self::shutdown) directly).
    pub async fn wait(self) -> Result<()> {
        let _ = tokio::signal::ctrl_c().await;
        self.shutdown().await;
        Ok(())
    }

    /// Cancels the planes and drains spawned tasks (bounded by a timeout, after
    /// which any stragglers are aborted when the [`JoinSet`] drops).
    pub async fn shutdown(self) {
        let Self {
            mut tasks,
            shutdown,
            ..
        } = self;
        shutdown.cancel();
        let drain = async { while tasks.join_next().await.is_some() {} };
        if tokio::time::timeout(Duration::from_secs(10), drain)
            .await
            .is_err()
        {
            tracing::warn!("bridge shutdown timed out; remaining tasks will be aborted");
        }
    }
}

/// Binds both planes (fail-closed) and serves until the process is stopped.
///
/// Thin wrapper over [`BridgeServer::start`] + [`BridgeServer::wait`] that also
/// prints the startup banner — preserved so `omni-dev browser bridge serve` is
/// behaviourally unchanged.
///
/// `token` is the already-resolved session token (never sourced from argv).
pub async fn run(config: BridgeConfig, token: String) -> Result<()> {
    let server = BridgeServer::start(config, token).await?;
    print_startup(server.state.config.as_ref(), &server.state.token);
    server.wait().await
}

/// Prints the bound ports, session token, and paste-ready snippet to stdout.
fn print_startup(config: &BridgeConfig, token: &str) {
    let snippet = snippet::render(config.ws_port, token);
    println!("omni-dev browser bridge serve");
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
        "  omni-dev browser bridge request --control-port {} --url /path",
        config.control_port
    );
}

// ── WebSocket plane ──────────────────────────────────────────────────

/// Spawns a cancellable accept loop for one WebSocket-plane listener; each
/// accepted connection is handled on its own task. Used once per bound loopback
/// address (IPv4 and, when available, IPv6) feeding the shared [`AppState`].
fn spawn_ws_accept(
    tasks: &mut JoinSet<()>,
    listener: TcpListener,
    state: AppState,
    shutdown: CancellationToken,
) {
    tasks.spawn(async move {
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                accepted = listener.accept() => match accepted {
                    Ok((stream, _peer)) => {
                        tokio::spawn(handle_ws_conn(stream, state.clone()));
                    }
                    Err(e) => tracing::warn!("WebSocket accept error: {e}"),
                },
            }
        }
    });
}

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
    state
        .lock_tabs()
        .insert(conn_id, WsConn { sender: tx, origin });

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
            Message::Text(txt) => match serde_json::from_str::<BrowserFrame>(&txt) {
                Ok(frame) => {
                    // If a streamed response's control-plane consumer has gone,
                    // tell *this* browser (the one fetching it) to cancel its
                    // reader so it stops fetching.
                    if let Some(cancel_id) = state.correlator.deliver(frame) {
                        send_cancel(&state, conn_id, cancel_id).await;
                    }
                }
                Err(e) => tracing::debug!("Unparseable browser frame: {e}"),
            },
            Message::Close(_) => break,
            _ => {}
        }
    }

    writer.abort();
    // Drop only this connection's registry entry (keyed by its unique id, so a
    // reconnect under a new id is never clobbered).
    if state.lock_tabs().remove(&conn_id).is_some() {
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
    Json(state.status_snapshot())
}

/// `POST /__bridge/request` — full-fidelity control endpoint. A `stream: true`
/// body returns an NDJSON stream (head line, `{seq,chunk}` lines, `{done}`);
/// otherwise a single JSON response envelope.
async fn request_handler(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let mut req: ControlRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")).into_response()
        }
    };
    // The header, when present, overrides a `target` body field.
    if let Some(target) = target_header(&headers) {
        req.target = Some(target);
    }
    if req.stream {
        return match start_stream(&state, req).await {
            Ok((status, headers, driver)) => ndjson_stream_response(status, headers, driver),
            Err((code, msg)) => (code, msg).into_response(),
        };
    }
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
    // `?__stream=1` opts the proxied request into a streamed (chunked) response;
    // the marker is stripped so it never reaches the upstream URL.
    let (stream, forwarded_query) = extract_stream_flag(parts.uri.query());
    let url = match forwarded_query.as_deref() {
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
        stream,
        target: target_header(&parts.headers),
        // The transparent proxy has no per-request override channel; cross-origin
        // proxying is governed solely by the `serve --allow-origin` global.
        allow_origin: None,
        // The transparent proxy has no per-request credentials control; it keeps
        // the snippet default (`include`), matching pre-credentials behavior.
        credentials: None,
        // The proxy already lossily decodes the inbound body as UTF-8 text above,
        // so it never tags a base64 request encoding.
        encoding: None,
    };

    if stream {
        return match start_stream(&state, req).await {
            Ok((status, headers, driver)) => raw_stream_response(status, headers, driver),
            Err((code, msg)) => (code, msg).into_response(),
        };
    }

    match dispatch(&state, req).await {
        Ok(env) => envelope_to_response(env),
        Err((code, msg)) => (code, msg).into_response(),
    }
}

/// Splits a `__stream` marker out of a query string.
///
/// Returns whether streaming was requested and the query with the marker
/// removed (`None` when nothing remains). `__stream=0` / `__stream=false`
/// explicitly disable it; any other presence enables it.
fn extract_stream_flag(query: Option<&str>) -> (bool, Option<String>) {
    let Some(query) = query else {
        return (false, None);
    };
    let mut stream = false;
    let kept: Vec<&str> = query
        .split('&')
        .filter(|kv| {
            let (key, value) = match kv.split_once('=') {
                Some((k, v)) => (k, Some(v)),
                None => (*kv, None),
            };
            if key == "__stream" {
                stream = !matches!(value, Some("0" | "false"));
                false
            } else {
                true
            }
        })
        .collect();
    let rebuilt = (!kept.is_empty()).then(|| kept.join("&"));
    (stream, rebuilt)
}

/// Copies request headers safe to forward to the browser, dropping the
/// bridge-control and hop-by-hop headers a CLI client adds.
fn forwardable_headers(headers: &axum::http::HeaderMap) -> BTreeMap<String, String> {
    const DROP: &[&str] = &[
        "host",
        "authorization",
        auth::BRIDGE_HEADER,
        auth::BRIDGE_TARGET_HEADER,
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

/// Extracts the `X-Omni-Bridge-Target` selector from request headers, if present
/// and non-empty.
fn target_header(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(auth::BRIDGE_TARGET_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Resolves which connected tab a request targets, returning its connection id
/// and a clone of its outbound sender.
///
/// An explicit `target` is a connection id (canonical) or an `Origin` that
/// uniquely matches one tab. With no target, routing succeeds only when exactly
/// one tab is connected — otherwise the request is ambiguous and rejected.
fn resolve_target(
    tabs: &HashMap<u64, WsConn>,
    target: Option<&str>,
) -> Result<(u64, mpsc::UnboundedSender<Message>), (StatusCode, String)> {
    if tabs.is_empty() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "no browser connected".to_string(),
        ));
    }
    let Some(sel) = target else {
        // No target: route only when exactly one tab is connected.
        let mut it = tabs.iter();
        return match (it.next(), it.next()) {
            (Some((id, conn)), None) => Ok((*id, conn.sender.clone())),
            _ => Err((
                StatusCode::CONFLICT,
                format!(
                    "multiple tabs connected; select one with the X-Omni-Bridge-Target \
                     header or a `target` field ({})",
                    tab_list(tabs)
                ),
            )),
        };
    };

    // A bare integer selects by connection id (canonical, unambiguous).
    if let Ok(id) = sel.parse::<u64>() {
        return match tabs.get(&id) {
            Some(conn) => Ok((id, conn.sender.clone())),
            None => Err((
                StatusCode::NOT_FOUND,
                format!(
                    "no connected tab with id {id}; connected: {}",
                    tab_list(tabs)
                ),
            )),
        };
    }

    // Otherwise match the selector against tab origins.
    let mut hits = tabs
        .iter()
        .filter(|(_, c)| c.origin.as_deref() == Some(sel));
    match (hits.next(), hits.next()) {
        (Some((id, conn)), None) => Ok((*id, conn.sender.clone())),
        (None, _) => Err((
            StatusCode::NOT_FOUND,
            format!(
                "no connected tab with origin {sel}; connected: {}",
                tab_list(tabs)
            ),
        )),
        (Some(_), Some(_)) => Err((
            StatusCode::CONFLICT,
            format!(
                "origin {sel} matches multiple tabs; target by connection id ({})",
                tab_list(tabs)
            ),
        )),
    }
}

/// Renders the connected tabs as `id N: origin, …` (id-sorted) for error
/// messages. Carries no authenticated data beyond the origin already in status.
fn tab_list(tabs: &HashMap<u64, WsConn>) -> String {
    let mut items: Vec<(u64, Option<&str>)> = tabs
        .iter()
        .map(|(id, c)| (*id, c.origin.as_deref()))
        .collect();
    items.sort_by_key(|(id, _)| *id);
    items
        .iter()
        .map(|(id, origin)| match origin {
            Some(o) => format!("id {id}: {o}"),
            None => format!("id {id}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolves the outbound-origin allowlist for a single request.
///
/// A per-request `request --allow-origin` override (`req.allow_origin`) takes
/// precedence over the `serve --allow-origin` global; otherwise the global is
/// used. This value reaches **only** [`auth::validate_outbound_url`] — never the
/// connection-time `ws_origin_allowed` gate — so a request may target a
/// cross-origin URL without the page's own tab being rejected at upgrade.
///
/// A WARN is logged whenever the per-request override is exercised, since it
/// widens this request's outbound scope beyond what `serve` was started with.
fn resolve_allow_origin<'a>(req: &'a ControlRequest, state: &'a AppState) -> Option<&'a str> {
    match req.allow_origin.as_deref() {
        Some(origin) => {
            tracing::warn!(
                "Per-request --allow-origin override in effect; outbound scope for this request \
                 widened to {origin}"
            );
            Some(origin)
        }
        None => state.config.allow_origin.as_deref(),
    }
}

/// The shared request path: scope-check, register a waiter, send the command,
/// and await the browser's reply (or time out).
/// Appends a best-effort `service = browser-bridge` HTTP record for one proxied
/// request, flagging `via_daemon` when the bridge is hosted by the daemon.
fn record_bridge_http(
    method: &str,
    url: &str,
    started: Instant,
    status: Option<u16>,
    error: Option<&str>,
) {
    let via_daemon = matches!(
        request_log::current_context().source,
        request_log::Source::Daemon
    );
    request_log::record_http_with(
        "browser-bridge",
        method,
        url,
        started,
        status,
        error,
        request_log::HttpExtra {
            via_daemon,
            ..Default::default()
        },
    );
}

async fn dispatch(
    state: &AppState,
    req: ControlRequest,
) -> Result<ResponseEnvelope, (StatusCode, String)> {
    let started = Instant::now();
    auth::validate_outbound_url(&req.url, resolve_allow_origin(&req, state)).map_err(|_| {
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

    // Clone the method/URL for the request log before they move into `Command`.
    let log_method = req.method.clone();
    let log_url = req.url.clone();
    let (id, rx) = state.correlator.register();
    let command = Command {
        id,
        url: req.url,
        method: req.method,
        headers: req.headers,
        body: req.body,
        stream: false,
        credentials: req.credentials,
        encoding: req.encoding,
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
        let tabs = state.lock_tabs();
        let (_conn_id, sender) = match resolve_target(&tabs, req.target.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                state.correlator.remove(id);
                return Err(e);
            }
        };
        if sender.send(Message::Text(frame.into())).is_err() {
            state.correlator.remove(id);
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "no browser connected".to_string(),
            ));
        }
    }

    match tokio::time::timeout(state.config.request_timeout, rx).await {
        Ok(Ok(reply)) => match reply.outcome() {
            ReplyOutcome::Success {
                status,
                headers,
                body,
                encoding,
            } => {
                record_bridge_http(&log_method, &log_url, started, Some(status), None);
                // Size is accounted against the *decoded* body. For base64 that
                // means decoding here to learn the true byte length; the envelope
                // still carries the base64 string (the caller / proxy decodes).
                let decoded_len = match encoding.as_deref() {
                    None => body.len(),
                    Some("base64") => match BASE64.decode(body.as_bytes()) {
                        Ok(bytes) => bytes.len(),
                        Err(_) => {
                            return Err((
                                StatusCode::BAD_GATEWAY,
                                "browser sent an invalid base64 body".to_string(),
                            ))
                        }
                    },
                    Some(other) => {
                        return Err((
                            StatusCode::BAD_GATEWAY,
                            format!("browser sent an unsupported body encoding: {other}"),
                        ))
                    }
                };
                if decoded_len > state.config.max_body_bytes {
                    return Err((
                        StatusCode::BAD_GATEWAY,
                        format!(
                            "browser response body is {decoded_len} bytes, exceeding the \
                             --max-body-bytes limit of {} bytes; page the request to fetch \
                             less per call (e.g. narrow the time range or lower a `limit`/page \
                             size) or raise --max-body-bytes",
                            state.config.max_body_bytes
                        ),
                    ));
                }
                Ok(ResponseEnvelope {
                    id,
                    status,
                    headers,
                    body,
                    encoding,
                })
            }
            ReplyOutcome::Error(msg) => {
                record_bridge_http(&log_method, &log_url, started, None, Some(&msg));
                Err((
                    StatusCode::BAD_GATEWAY,
                    format!("browser fetch failed: {msg}"),
                ))
            }
        },
        Ok(Err(_)) => {
            record_bridge_http(
                &log_method,
                &log_url,
                started,
                None,
                Some("browser connection closed before replying"),
            );
            Err((
                StatusCode::BAD_GATEWAY,
                "browser connection closed before replying".to_string(),
            ))
        }
        Err(_) => {
            state.correlator.remove(id);
            record_bridge_http(
                &log_method,
                &log_url,
                started,
                None,
                Some("browser did not reply in time"),
            );
            Err((
                StatusCode::GATEWAY_TIMEOUT,
                "browser did not reply in time".to_string(),
            ))
        }
    }
}

/// Sends a best-effort cancellation frame to the tab handling stream `id` and
/// drops the pending stream, so a stream whose consumer is gone (or which
/// tripped a limit) stops the in-page reader rather than fetching to completion.
/// A no-op if that tab has since disconnected.
async fn send_cancel(state: &AppState, conn_id: u64, id: u64) {
    state.correlator.remove(id);
    let Ok(frame) = serde_json::to_string(&CancelCommand::new(id)) else {
        return;
    };
    let tabs = state.lock_tabs();
    if let Some(conn) = tabs.get(&conn_id) {
        let _ = conn.sender.send(Message::Text(frame.into()));
    }
}

/// The shared streaming request path: scope-check, register a stream waiter, send
/// the `stream: true` command, and await the head frame (status + headers) under
/// the inter-chunk idle timeout. Returns the head plus a [`StreamDriver`] that
/// pulls the remaining body chunks; the concurrency permit is held by the driver
/// for the stream's lifetime.
async fn start_stream(
    state: &AppState,
    req: ControlRequest,
) -> Result<(u16, BTreeMap<String, String>, StreamDriver), (StatusCode, String)> {
    let started = Instant::now();
    auth::validate_outbound_url(&req.url, resolve_allow_origin(&req, state)).map_err(|_| {
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

    let permit = state.in_flight.clone().try_acquire_owned().map_err(|_| {
        (
            StatusCode::TOO_MANY_REQUESTS,
            "too many in-flight requests".to_string(),
        )
    })?;

    // Clone the method/URL for the request log before they move into `Command`.
    let log_method = req.method.clone();
    let log_url = req.url.clone();
    let (id, mut rx) = state.correlator.register_stream();
    let command = Command {
        id,
        url: req.url,
        method: req.method,
        headers: req.headers,
        body: req.body,
        stream: true,
        credentials: req.credentials,
        encoding: req.encoding,
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

    let conn_id = {
        let tabs = state.lock_tabs();
        let (conn_id, sender) = match resolve_target(&tabs, req.target.as_deref()) {
            Ok(t) => t,
            Err(e) => {
                state.correlator.remove(id);
                return Err(e);
            }
        };
        if sender.send(Message::Text(frame.into())).is_err() {
            state.correlator.remove(id);
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "no browser connected".to_string(),
            ));
        }
        conn_id
    };

    let idle = state.config.request_timeout;
    let (status, headers) = match tokio::time::timeout(idle, rx.recv()).await {
        Ok(Some(StreamItem::Head { status, headers })) => {
            record_bridge_http(&log_method, &log_url, started, Some(status), None);
            (status, headers)
        }
        Ok(Some(StreamItem::Error(msg))) => {
            state.correlator.remove(id);
            record_bridge_http(&log_method, &log_url, started, None, Some(&msg));
            return Err((
                StatusCode::BAD_GATEWAY,
                format!("browser fetch failed: {msg}"),
            ));
        }
        Ok(Some(_)) => {
            state.correlator.remove(id);
            record_bridge_http(
                &log_method,
                &log_url,
                started,
                None,
                Some("browser streamed a body chunk before the response head"),
            );
            return Err((
                StatusCode::BAD_GATEWAY,
                "browser streamed a body chunk before the response head".to_string(),
            ));
        }
        Ok(None) => {
            record_bridge_http(
                &log_method,
                &log_url,
                started,
                None,
                Some("browser connection closed before replying"),
            );
            return Err((
                StatusCode::BAD_GATEWAY,
                "browser connection closed before replying".to_string(),
            ));
        }
        Err(_) => {
            send_cancel(state, conn_id, id).await;
            record_bridge_http(
                &log_method,
                &log_url,
                started,
                None,
                Some("browser did not start streaming in time"),
            );
            return Err((
                StatusCode::GATEWAY_TIMEOUT,
                "browser did not start streaming in time".to_string(),
            ));
        }
    };

    let driver = StreamDriver {
        state: state.clone(),
        id,
        conn_id,
        rx,
        idle,
        max_body: state.config.max_body_bytes,
        sent: 0,
        _permit: permit,
        done: false,
    };
    Ok((status, headers, driver))
}

/// Drives a registered stream's body chunks: applies the inter-chunk idle
/// timeout, decodes each base64 chunk, enforces the cumulative `--max-body-bytes`
/// ceiling, and cancels the browser stream on early/abnormal termination. Holds
/// the concurrency permit until dropped.
struct StreamDriver {
    state: AppState,
    id: u64,
    /// Connection id of the tab serving this stream; cancels route back to it.
    conn_id: u64,
    rx: mpsc::UnboundedReceiver<StreamItem>,
    idle: Duration,
    max_body: usize,
    sent: usize,
    _permit: OwnedSemaphorePermit,
    done: bool,
}

/// One step of a [`StreamDriver`]: decoded chunk bytes, or end-of-stream.
enum NextChunk {
    /// A decoded body chunk and its sequence number.
    Data {
        /// Chunk sequence number reported by the browser.
        seq: u64,
        /// Decoded chunk bytes.
        bytes: Vec<u8>,
    },
    /// The stream is finished (normal end, error, idle timeout, or cap hit).
    End,
}

impl StreamDriver {
    /// Pulls the next decoded chunk, ending the stream on a terminal item, an
    /// invalid chunk, an idle timeout, or the cumulative byte cap.
    async fn next_chunk(&mut self) -> NextChunk {
        if self.done {
            return NextChunk::End;
        }
        loop {
            match tokio::time::timeout(self.idle, self.rx.recv()).await {
                Ok(Some(StreamItem::Chunk { seq, data })) => {
                    let Ok(bytes) = BASE64.decode(data.as_bytes()) else {
                        return self.abort().await;
                    };
                    self.sent = self.sent.saturating_add(bytes.len());
                    if self.sent > self.max_body {
                        return self.abort().await;
                    }
                    return NextChunk::Data { seq, bytes };
                }
                // A stray head after the first is a protocol slip; ignore it.
                Ok(Some(StreamItem::Head { .. })) => {}
                Ok(Some(StreamItem::End | StreamItem::Error(_)) | None) => {
                    return self.finish();
                }
                // Inter-chunk idle timeout: stop the browser and end the stream.
                Err(_) => return self.abort().await,
            }
        }
    }

    /// Ends the stream and removes the pending entry (terminal item / consumer
    /// gone — the browser is already done, so no cancel is sent).
    fn finish(&mut self) -> NextChunk {
        self.done = true;
        self.state.correlator.remove(self.id);
        NextChunk::End
    }

    /// Ends the stream early and tells the browser to cancel its reader (idle
    /// timeout, cap exceeded, or an undecodable chunk).
    async fn abort(&mut self) -> NextChunk {
        self.done = true;
        send_cancel(&self.state, self.conn_id, self.id).await;
        NextChunk::End
    }
}

/// Serialises a [`StreamLine`] as one NDJSON line (trailing newline).
fn to_ndjson_line(line: &StreamLine) -> String {
    let mut s = serde_json::to_string(line).unwrap_or_else(|_| "{}".to_string());
    s.push('\n');
    s
}

/// Builds the transparent-proxy response for a streamed body: status and
/// `content-type` from the head frame, decoded chunk bytes streamed as a chunked
/// HTTP body.
fn raw_stream_response(
    status: u16,
    headers: BTreeMap<String, String>,
    driver: StreamDriver,
) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(code);
    if let Some(ct) = headers.get("content-type") {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }
    let stream = futures::stream::unfold(driver, |mut driver| async move {
        match driver.next_chunk().await {
            NextChunk::Data { bytes, .. } => Some((
                Ok::<_, std::convert::Infallible>(Bytes::from(bytes)),
                driver,
            )),
            NextChunk::End => None,
        }
    });
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

/// Builds the `POST /__bridge/request` response for a streamed body: an NDJSON
/// body of a head line, `{seq,chunk}` lines, and a terminating `{done}` line.
fn ndjson_stream_response(
    status: u16,
    headers: BTreeMap<String, String>,
    driver: StreamDriver,
) -> Response {
    let head_line = to_ndjson_line(&StreamLine::Head { status, headers });
    // State: (pending head line, driver, done-line-emitted).
    let init = (Some(head_line), driver, false);
    let stream = futures::stream::unfold(init, |(head, mut driver, done_emitted)| async move {
        if let Some(line) = head {
            return Some((
                Ok::<_, std::convert::Infallible>(Bytes::from(line)),
                (None, driver, done_emitted),
            ));
        }
        if done_emitted {
            return None;
        }
        match driver.next_chunk().await {
            NextChunk::Data { seq, bytes } => {
                let line = to_ndjson_line(&StreamLine::Chunk {
                    seq,
                    chunk: BASE64.encode(&bytes),
                });
                Some((Ok(Bytes::from(line)), (None, driver, false)))
            }
            NextChunk::End => {
                let line = to_ndjson_line(&StreamLine::Done { done: true });
                Some((Ok(Bytes::from(line)), (None, driver, true)))
            }
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/x-ndjson")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

/// Renders a browser response envelope as the transparent-proxy HTTP response.
///
/// A base64-tagged body is decoded back to raw bytes so a `curl` client gets the
/// original bytes (image, gzip blob, …); the base64 is validated in `dispatch`,
/// but a decode failure here still fails closed with `502`.
fn envelope_to_response(env: ResponseEnvelope) -> Response {
    let status = StatusCode::from_u16(env.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    if let Some(ct) = env.headers.get("content-type") {
        builder = builder.header(header::CONTENT_TYPE, ct);
    }
    let body = match env.encoding.as_deref() {
        Some("base64") => match BASE64.decode(env.body.as_bytes()) {
            Ok(bytes) => Body::from(bytes),
            Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
        },
        _ => Body::from(env.body),
    };
    builder
        .body(body)
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn buffered_frame(id: u64) -> BrowserFrame {
        BrowserFrame {
            id,
            status: Some(200),
            headers: None,
            body: Some("ok".into()),
            encoding: None,
            error: None,
            stream: None,
            chunk: None,
            seq: None,
            done: None,
        }
    }

    /// Builds a `tabs` map entry with a detached sender (the receiver is dropped;
    /// routing tests only assert *which* connection is chosen, not delivery).
    fn tab(origin: Option<&str>) -> WsConn {
        let (sender, _rx) = mpsc::unbounded_channel();
        WsConn {
            sender,
            origin: origin.map(str::to_string),
        }
    }

    #[test]
    fn resolve_target_no_tabs_is_503() {
        let tabs = HashMap::new();
        let err = resolve_target(&tabs, None).unwrap_err();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn resolve_target_single_tab_routes_without_target() {
        let mut tabs = HashMap::new();
        tabs.insert(1, tab(Some("https://a.test")));
        let (id, _s) = resolve_target(&tabs, None).unwrap();
        assert_eq!(id, 1);
    }

    #[test]
    fn resolve_target_multiple_tabs_no_target_is_409() {
        let mut tabs = HashMap::new();
        tabs.insert(1, tab(Some("https://a.test")));
        tabs.insert(2, tab(Some("https://b.test")));
        let err = resolve_target(&tabs, None).unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);
        // The message lists both connected tabs to disambiguate.
        assert!(err.1.contains("id 1") && err.1.contains("id 2"));
    }

    #[test]
    fn resolve_target_by_connection_id() {
        let mut tabs = HashMap::new();
        tabs.insert(1, tab(Some("https://a.test")));
        tabs.insert(2, tab(Some("https://b.test")));
        let (id, _s) = resolve_target(&tabs, Some("2")).unwrap();
        assert_eq!(id, 2);
        // Unknown id is a 404.
        assert_eq!(
            resolve_target(&tabs, Some("9")).unwrap_err().0,
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn resolve_target_by_unique_origin() {
        let mut tabs = HashMap::new();
        tabs.insert(1, tab(Some("https://a.test")));
        tabs.insert(2, tab(Some("https://b.test")));
        let (id, _s) = resolve_target(&tabs, Some("https://b.test")).unwrap();
        assert_eq!(id, 2);
        // Unknown origin is a 404.
        assert_eq!(
            resolve_target(&tabs, Some("https://nope.test"))
                .unwrap_err()
                .0,
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn resolve_target_ambiguous_origin_is_409() {
        let mut tabs = HashMap::new();
        tabs.insert(1, tab(Some("https://a.test")));
        tabs.insert(2, tab(Some("https://a.test")));
        let err = resolve_target(&tabs, Some("https://a.test")).unwrap_err();
        assert_eq!(err.0, StatusCode::CONFLICT);
        // Two tabs share the origin → caller is told to target by id.
        assert!(err.1.contains("connection id"));
    }

    #[test]
    fn target_header_trims_and_drops_empty() {
        let mut h = axum::http::HeaderMap::new();
        assert_eq!(target_header(&h), None);
        h.insert(auth::BRIDGE_TARGET_HEADER, "  2  ".parse().unwrap());
        assert_eq!(target_header(&h).as_deref(), Some("2"));
        h.insert(auth::BRIDGE_TARGET_HEADER, "   ".parse().unwrap());
        assert_eq!(target_header(&h), None);
    }

    #[test]
    fn tab_list_renders_id_with_and_without_origin() {
        let mut tabs = HashMap::new();
        tabs.insert(1, tab(Some("https://a.test")));
        tabs.insert(2, tab(None));
        // Id-sorted; a tab that sent no `Origin` renders as the bare id.
        assert_eq!(tab_list(&tabs), "id 1: https://a.test, id 2");
    }

    /// A minimal [`AppState`] for exercising `dispatch` / `start_stream` without
    /// a real WebSocket peer.
    fn test_state() -> AppState {
        AppState {
            token: Arc::new("t".to_string()),
            config: Arc::new(BridgeConfig {
                ws_port: 0,
                control_port: 0,
                request_timeout: Duration::from_secs(5),
                allow_origin: None,
                max_body_bytes: 1024,
                max_concurrent: 8,
            }),
            correlator: Correlator::new(),
            tabs: Arc::new(StdMutex::new(HashMap::new())),
            in_flight: Arc::new(Semaphore::new(8)),
            conn_counter: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Inserts a tab whose writer receiver is already dropped, so any send to it
    /// fails — modelling a tab that vanished between routing and dispatch.
    async fn insert_dead_tab(state: &AppState, id: u64) {
        let (sender, rx) = mpsc::unbounded_channel();
        drop(rx);
        state.lock_tabs().insert(
            id,
            WsConn {
                sender,
                origin: None,
            },
        );
    }

    fn plain_request() -> ControlRequest {
        ControlRequest {
            url: "/x".to_string(),
            method: "GET".to_string(),
            headers: BTreeMap::new(),
            body: None,
            stream: false,
            target: None,
            allow_origin: None,
            credentials: None,
            encoding: None,
        }
    }

    /// Builds a state whose `serve` global allow-origin is set, to prove a
    /// per-request override wins over (and a missing one falls back to) it.
    fn state_with_global_origin(global: Option<&str>) -> AppState {
        let mut state = test_state();
        let mut config = (*state.config).clone();
        config.allow_origin = global.map(str::to_string);
        state.config = Arc::new(config);
        state
    }

    #[test]
    fn resolve_allow_origin_prefers_per_request_override() {
        let state = state_with_global_origin(Some("https://global.test"));
        let req = ControlRequest {
            allow_origin: Some("https://per-request.test".to_string()),
            ..plain_request()
        };
        // The per-request value wins over the serve global.
        assert_eq!(
            resolve_allow_origin(&req, &state),
            Some("https://per-request.test")
        );
        // A request carrying the override permits its matched cross-origin
        // target, and still rejects an unmatched one.
        assert_eq!(
            auth::validate_outbound_url(
                "https://per-request.test/x",
                resolve_allow_origin(&req, &state)
            ),
            Ok(())
        );
        assert!(auth::validate_outbound_url(
            "https://other.test/x",
            resolve_allow_origin(&req, &state)
        )
        .is_err());
    }

    #[test]
    fn resolve_allow_origin_falls_back_to_global() {
        let state = state_with_global_origin(Some("https://global.test"));
        let req = plain_request();
        assert!(req.allow_origin.is_none());
        // With no per-request override, the serve global governs the scope.
        assert_eq!(
            resolve_allow_origin(&req, &state),
            Some("https://global.test")
        );
    }

    #[test]
    fn per_request_override_does_not_affect_ws_origin_gate() {
        // The per-request override feeds only the outbound-URL check. The
        // connection-time WS gate reads the `serve` global directly, so a tab on
        // origin A stays connectable even when a request override permits B.
        let state = state_with_global_origin(None);
        let req = ControlRequest {
            allow_origin: Some("https://b.test".to_string()),
            ..plain_request()
        };
        // Outbound to B is permitted by the override...
        assert_eq!(
            auth::validate_outbound_url("https://b.test/x", resolve_allow_origin(&req, &state)),
            Ok(())
        );
        // ...while the WS upgrade gate (serve global = None) still admits any
        // origin, unchanged by the per-request value.
        assert!(auth::ws_origin_allowed(
            Some("https://a.test"),
            state.config.allow_origin.as_deref()
        ));
    }

    #[tokio::test]
    async fn dispatch_returns_503_when_send_fails() {
        let state = test_state();
        insert_dead_tab(&state, 1).await;
        let err = dispatch(&state, plain_request()).await.unwrap_err();
        assert_eq!(err.0, StatusCode::SERVICE_UNAVAILABLE);
        // The failed dispatch leaves no dangling waiter behind.
        assert_eq!(state.correlator.pending_count(), 0);
    }

    #[tokio::test]
    async fn start_stream_returns_503_when_send_fails() {
        let state = test_state();
        insert_dead_tab(&state, 1).await;
        let req = ControlRequest {
            stream: true,
            ..plain_request()
        };
        let err = start_stream(&state, req).await.err().map(|e| e.0);
        assert_eq!(err, Some(StatusCode::SERVICE_UNAVAILABLE));
        assert_eq!(state.correlator.pending_count(), 0);
    }

    #[test]
    fn correlator_register_resolve_round_trip() {
        let c = Correlator::new();
        let (id, rx) = c.register();
        assert_eq!(c.pending_count(), 1);
        assert_eq!(c.deliver(buffered_frame(id)), None);
        assert_eq!(c.pending_count(), 0);
        let reply = rx.now_or_never().unwrap().unwrap();
        assert_eq!(reply.id, id);
    }

    #[test]
    fn correlator_stream_forwards_items_until_terminal() {
        let c = Correlator::new();
        let (id, mut rx) = c.register_stream();
        assert_eq!(c.pending_count(), 1);

        let mut head = buffered_frame(id);
        head.stream = Some(true);
        head.body = None;
        assert_eq!(c.deliver(head), None);
        assert!(matches!(
            rx.try_recv(),
            Ok(StreamItem::Head { status: 200, .. })
        ));
        // Head is non-terminal: the waiter stays registered.
        assert_eq!(c.pending_count(), 1);

        let mut done = BrowserFrame {
            done: Some(true),
            ..buffered_frame(id)
        };
        done.body = None;
        assert_eq!(c.deliver(done), None);
        assert!(matches!(rx.try_recv(), Ok(StreamItem::End)));
        assert_eq!(c.pending_count(), 0);
    }

    #[test]
    fn correlator_deliver_unknown_id_is_noop() {
        let c = Correlator::new();
        // A frame whose id was never registered (or already terminal) is dropped
        // without panicking and never asks the caller to cancel.
        assert_eq!(c.deliver(buffered_frame(999)), None);
        assert_eq!(c.pending_count(), 0);
    }

    #[test]
    fn correlator_stream_signals_cancel_when_consumer_gone() {
        let c = Correlator::new();
        let (id, rx) = c.register_stream();
        drop(rx); // consumer disconnected
        let mut chunk = buffered_frame(id);
        chunk.chunk = Some("aGk=".into());
        chunk.body = None;
        // Delivery fails (receiver dropped) → caller is told to cancel `id`.
        assert_eq!(c.deliver(chunk), Some(id));
        assert_eq!(c.pending_count(), 0);
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
    fn extract_stream_flag_detects_and_strips_marker() {
        assert_eq!(extract_stream_flag(None), (false, None));
        assert_eq!(
            extract_stream_flag(Some("a=1&b=2")),
            (false, Some("a=1&b=2".to_string()))
        );
        assert_eq!(
            extract_stream_flag(Some("a=1&__stream=1&b=2")),
            (true, Some("a=1&b=2".to_string()))
        );
        // Bare marker, nothing else left.
        assert_eq!(extract_stream_flag(Some("__stream")), (true, None));
        // Explicit disable.
        assert_eq!(extract_stream_flag(Some("__stream=0")), (false, None));
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

    #[test]
    fn envelope_to_response_passes_text_body_through() {
        let env = ResponseEnvelope {
            id: 1,
            status: 200,
            headers: BTreeMap::new(),
            body: "hello".into(),
            encoding: None,
        };
        assert_eq!(envelope_to_response(env).status(), StatusCode::OK);
    }

    #[test]
    fn envelope_to_response_rejects_invalid_base64() {
        // `dispatch` validates base64 before this runs, so this path is only
        // reachable defensively — assert it still fails closed with 502.
        let env = ResponseEnvelope {
            id: 1,
            status: 200,
            headers: BTreeMap::new(),
            body: "not valid base64 @@@".into(),
            encoding: Some("base64".into()),
        };
        assert_eq!(envelope_to_response(env).status(), StatusCode::BAD_GATEWAY);
    }

    use futures::FutureExt;

    /// A `BridgeConfig` bound to random free ports, for lifecycle tests.
    fn ephemeral_config() -> BridgeConfig {
        BridgeConfig {
            ws_port: 0,
            control_port: 0,
            request_timeout: Duration::from_secs(5),
            allow_origin: None,
            max_body_bytes: 1024,
            max_concurrent: 8,
        }
    }

    #[tokio::test]
    async fn bridge_server_start_status_shutdown() {
        let server = BridgeServer::start(ephemeral_config(), "tok".to_string())
            .await
            .unwrap();
        // Ports were resolved from the OS (port 0 → a real bound port).
        assert_ne!(server.control_port(), 0);
        assert_ne!(server.ws_port(), 0);
        // No browser is connected yet.
        let status = server.status();
        assert!(!status.connected);
        assert!(status.tabs.is_empty());
        assert_eq!(status.pending, 0);
        // Graceful shutdown drains and returns.
        server.shutdown().await;
    }

    #[tokio::test]
    async fn disconnect_unknown_tab_is_error() {
        let server = BridgeServer::start(ephemeral_config(), "tok".to_string())
            .await
            .unwrap();
        let err = server.disconnect_tab(999).unwrap_err();
        assert!(err.to_string().contains("no connected tab"));
        server.shutdown().await;
    }

    #[tokio::test]
    async fn start_fails_closed_on_taken_control_port() {
        // Occupy a port, then ask the bridge to bind the same one.
        let squatter = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let taken = squatter.local_addr().unwrap().port();
        let config = BridgeConfig {
            control_port: taken,
            ..ephemeral_config()
        };
        assert!(BridgeServer::start(config, "tok".to_string())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn rebind_same_fixed_port_after_close_succeeds() {
        // Regression for #990: a daemon/tray restart tears the planes down and
        // rebinds the *same* fixed ports. A just-closed connection can leave the
        // listener address in TIME_WAIT; without SO_REUSEADDR the rebind fails
        // with EADDRINUSE and the bridge is wedged not-running. With it, the
        // rebind succeeds.
        let server = BridgeServer::start(ephemeral_config(), "tok".to_string())
            .await
            .unwrap();
        let control_port = server.control_port();
        let ws_port = server.ws_port();

        // Hold loopback connections to both planes across the teardown so the
        // server is the active closer and its listener ports pass through
        // TIME_WAIT.
        let control_conn = TcpStream::connect((Ipv4Addr::LOCALHOST, control_port))
            .await
            .unwrap();
        let ws_conn = TcpStream::connect((Ipv4Addr::LOCALHOST, ws_port))
            .await
            .unwrap();

        server.shutdown().await;
        drop(control_conn);
        drop(ws_conn);
        // Let the four-way close settle so the ports are genuinely in TIME_WAIT.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let config = BridgeConfig {
            ws_port,
            control_port,
            ..ephemeral_config()
        };
        let server2 = BridgeServer::start(config, "tok".to_string())
            .await
            .expect("rebinding just-released fixed ports must succeed with SO_REUSEADDR");
        server2.shutdown().await;
    }
}
