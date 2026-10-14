//! The gateway's HTTP surface: an axum server on `127.0.0.1`.
//!
//! Every request passes `guard` first — a loopback `Host` (DNS-rebinding
//! defence), no browser `Origin`/`Sec-Fetch-Site` (a web page the operator
//! visits must not reach it), and the gateway token as `x-api-key` or
//! `Authorization: Bearer` (what the Anthropic SDKs and Claude Code send) —
//! the browser bridge's default-closed posture (ADR-0036), restated for this
//! listener in ADR-0098.

use std::collections::HashSet;
use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::body::{Body, Bytes};
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde::Serialize;
use serde_json::{json, Map, Value};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use super::anthropic::{new_message_id, MessagesRequest, ThinkingConfig};
use super::chat::{self, BuildContext};
use super::chat_stream::ChatStreamParser;
use super::completion::{self, Completion, ResponseContext};
use super::config::GatewayConfig;
use super::error::{ErrorKind, GatewayError};
use super::flavor::UpstreamProtocol;
use super::lower;
use super::policy::{self, PolicyReport};
use super::responses;
use super::responses_stream::ResponsesStreamParser;
use super::signature::Signer;
use super::sse::{encode, SseParser};
use super::stream::{ping, AnthropicStream, StreamSettings, UpstreamEvent};
use super::tokens;
use crate::browser::auth;

/// Request-log `service` tag for upstream calls.
pub(crate) const LOG_SERVICE: &str = "anthropic-gateway";
/// Response header listing the request fields the gateway ignored.
pub(crate) const IGNORED_HEADER: &str = "x-omni-dev-gateway-ignored";
/// Response header naming the upstream model that served the request.
pub(crate) const UPSTREAM_MODEL_HEADER: &str = "x-omni-dev-gateway-upstream-model";
/// How long an upstream may be silent before a keep-alive `ping` is sent.
const PING_INTERVAL: Duration = Duration::from_secs(15);

/// Request counters, for `daemon status` and the tray.
#[derive(Debug, Default)]
pub(crate) struct Stats {
    requests: AtomicU64,
    errors: AtomicU64,
    in_flight: AtomicU64,
    last_error: Mutex<Option<String>>,
}

/// A snapshot of the server's request counters.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct StatsSnapshot {
    /// Requests received.
    pub requests: u64,
    /// Requests answered with an error.
    pub errors: u64,
    /// Upstream calls in progress.
    pub in_flight: u64,
    /// The most recent error message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl Stats {
    fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            requests: self.requests.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            in_flight: self.in_flight.load(Ordering::Relaxed),
            last_error: self
                .last_error
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone(),
        }
    }

    fn record_error(&self, error: &GatewayError) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        *self
            .last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.to_string());
    }
}

/// A concurrency permit that also counts the upstream call as in flight.
pub(crate) struct Permit {
    _permit: OwnedSemaphorePermit,
    stats: Arc<Stats>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.stats.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Shared server state.
#[derive(Clone)]
pub(crate) struct AppState {
    pub config: Arc<GatewayConfig>,
    token: Arc<String>,
    signer: Arc<Signer>,
    http: reqwest::Client,
    discovery: Arc<super::routing::Discovery>,
    permits: Arc<Semaphore>,
    stats: Arc<Stats>,
    port: u16,
    warned: Arc<Mutex<HashSet<String>>>,
}

impl AppState {
    fn new(config: GatewayConfig, token: String, port: u16) -> Result<Self> {
        if config
            .routing
            .upstreams
            .iter()
            .filter_map(|u| u.api_key.as_ref())
            .chain(config.api_key.iter())
            .any(|k| auth::constant_time_eq(k.expose_secret().trim(), &token))
        {
            anyhow::bail!("upstream keys must not equal the inbound gateway token");
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(config.connect_timeout)
            .read_timeout(config.read_timeout)
            .build()
            .context("failed to build the gateway's HTTP client")?;
        Ok(Self {
            signer: Arc::new(Signer::from_token(&token)),
            token: Arc::new(token),
            permits: Arc::new(Semaphore::new(config.max_concurrent)),
            config: Arc::new(config),
            http,
            discovery: Arc::new(super::routing::Discovery::default()),
            stats: Arc::new(Stats::default()),
            port,
            warned: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    pub(crate) fn record_stream_error(&self, error: &GatewayError) {
        self.stats.record_error(error);
    }

    async fn selected(&self, body: &Map<String, Value>) -> Result<Self, GatewayError> {
        if self.config.routing.upstreams.is_empty() {
            return Ok(self.clone());
        }
        let requested = body
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| GatewayError::invalid("model: nonempty string required"))?;
        let _permit = self.permit()?;
        let (index, model) = self
            .discovery
            .resolve(&self.config.routing, &self.http, requested)
            .await?;
        let mut selected = self.clone();
        selected.config = Arc::new(self.config.routing.selected(
            &self.config,
            index,
            requested,
            &model,
        ));
        Ok(selected)
    }

    /// Takes a concurrency permit, or answers `429` when the gateway is at
    /// its cap (clients retry `429` with backoff).
    pub(crate) fn permit(&self) -> Result<Permit, GatewayError> {
        let permit = Arc::clone(&self.permits).try_acquire_owned().map_err(|_| {
            GatewayError::new(
                StatusCode::TOO_MANY_REQUESTS,
                ErrorKind::RateLimit,
                format!(
                    "the gateway is at its concurrency cap ({}); raise OMNI_DEV_GATEWAY_MAX_CONCURRENT",
                    self.config.max_concurrent
                ),
            )
        })?;
        self.stats.in_flight.fetch_add(1, Ordering::Relaxed);
        Ok(Permit {
            _permit: permit,
            stats: Arc::clone(&self.stats),
        })
    }

    /// Logs ignored fields: once per field name at `warn` (so a field every
    /// request carries does not flood the log), then at `debug`.
    pub(crate) fn note_ignored(&self, report: &PolicyReport) {
        if report.ignored.is_empty() {
            return;
        }
        let mut warned = self
            .warned
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fresh: Vec<&str> = report
            .ignored
            .iter()
            .filter(|name| warned.insert((*name).clone()))
            .map(String::as_str)
            .collect();
        if fresh.is_empty() {
            tracing::debug!(fields = ?report.ignored, "gateway ignored request fields");
        } else {
            tracing::warn!(
                fields = %fresh.join(", "),
                "gateway ignored request fields it cannot translate (reported in the {IGNORED_HEADER} header)"
            );
        }
    }

    /// POSTs `body` upstream with the upstream key, logs the call, and maps a
    /// non-2xx reply onto an Anthropic error.
    pub(crate) async fn send(
        &self,
        url: &str,
        body: &Value,
        extra_headers: &[(&'static str, String)],
    ) -> Result<reqwest::Response, GatewayError> {
        let mut request = self.http.post(url).json(body);
        if self
            .config
            .api_key
            .as_ref()
            .is_some_and(|k| auth::constant_time_eq(k.expose_secret().trim(), &self.token))
        {
            return Err(GatewayError::invalid(
                "upstream key must not equal the inbound gateway token",
            ));
        }
        if let Some(key) = &self.config.api_key {
            // One credential header, in the form the upstream expects: an
            // Anthropic-compatible endpoint reads `Authorization: Bearer` as an
            // OAuth token, so it gets `x-api-key`; OpenAI-protocol upstreams and
            // Fireworks' Anthropic surface take a bearer key.
            request = if self.config.protocol == UpstreamProtocol::Anthropic
                && self.config.flavor != super::flavor::Flavor::Fireworks
            {
                request.header("x-api-key", key.expose_secret())
            } else {
                request.bearer_auth(key.expose_secret())
            };
        }
        for (name, value) in extra_headers {
            request = request.header(*name, value);
        }
        let started = Instant::now();
        let result = request.send().await;
        crate::request_log::record_http_result(LOG_SERVICE, "POST", url, started, &result);
        let response = result
            .map_err(|e| GatewayError::bad_gateway(format!("could not reach the upstream: {e}")))?;
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let text = response.text().await.unwrap_or_default();
        Err(GatewayError::from_upstream(status, &text, retry_after))
    }
}

/// Adds the `ignored` and `upstream model` headers.
pub(crate) fn annotate(headers: &mut HeaderMap, report: &PolicyReport, model: &str) {
    if !report.ignored.is_empty() {
        let joined: String = report
            .ignored
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
            .chars()
            .filter(|c| c.is_ascii() && !c.is_ascii_control())
            .collect();
        if let Ok(value) = HeaderValue::from_str(&joined) {
            headers.insert(IGNORED_HEADER, value);
        }
    }
    if let Ok(value) = HeaderValue::from_str(model) {
        headers.insert(UPSTREAM_MODEL_HEADER, value);
    }
}

/// The `count_tokens` estimate response.
pub(crate) fn estimate_response(request: &MessagesRequest) -> Response {
    estimated_tokens_response(tokens::estimate(request))
}

fn estimated_tokens_response(input_tokens: u64) -> Response {
    let mut response = Json(tokens::response_body(input_tokens)).into_response();
    response.headers_mut().insert(
        tokens::ESTIMATE_HEADER,
        HeaderValue::from_static("heuristic"),
    );
    response
}

/// A running gateway.
pub struct GatewayServer {
    port: u16,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<()>,
    stats: Arc<Stats>,
}

impl GatewayServer {
    /// Binds `127.0.0.1:<port>` (fail-closed: a taken port is an error) and
    /// starts serving. `token` is the resolved gateway token (never from argv).
    pub fn start(config: GatewayConfig, token: String) -> Result<Self> {
        // `SO_REUSEADDR`, like the browser bridge (#990): a restart rebinds the
        // same port while just-closed client connections sit in `TIME_WAIT`. It
        // does not allow a second live listener, so a taken port still fails.
        let listener = crate::browser::bridge::bind_loopback_reuse(SocketAddr::from((
            Ipv4Addr::LOCALHOST,
            config.port,
        )))
        .with_context(|| {
            format!(
                "failed to bind the gateway to 127.0.0.1:{} (already in use?)",
                config.port
            )
        })?;
        let port = listener.local_addr()?.port();
        let max_body = config.max_body_bytes;
        let state = AppState::new(config, token, port)?;
        let stats = Arc::clone(&state.stats);
        let app = router(state, max_body);
        let shutdown = CancellationToken::new();
        let signal = shutdown.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app)
                .with_graceful_shutdown(signal.cancelled_owned())
                .await
            {
                tracing::warn!("gateway server error: {e}");
            }
        });
        Ok(Self {
            port,
            shutdown,
            task,
            stats,
        })
    }

    /// The bound port.
    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Request counters.
    #[must_use]
    pub fn stats(&self) -> StatsSnapshot {
        self.stats.snapshot()
    }

    /// Stops accepting, drains in-flight requests (bounded), and returns.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        if tokio::time::timeout(Duration::from_secs(10), self.task)
            .await
            .is_err()
        {
            tracing::warn!("gateway shutdown timed out; in-flight requests were dropped");
        }
    }

    /// Serves until Ctrl-C, then shuts down. Used by the standalone `serve`.
    pub async fn wait(self) -> Result<()> {
        let _ = tokio::signal::ctrl_c().await;
        self.shutdown().await;
        Ok(())
    }
}

fn router(state: AppState, max_body: usize) -> Router {
    Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .route("/v1/models", get(list_models))
        .route("/v1/models/{*id}", get(get_model))
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .layer(DefaultBodyLimit::max(max_body))
        .with_state(state)
}

/// The trust boundary: loopback `Host`, not browser-originated, and the token.
async fn guard(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let headers = request.headers();
    let text = |name: header::HeaderName| headers.get(name).and_then(|v| v.to_str().ok());
    let host_ok = text(header::HOST).is_some_and(|h| auth::host_allowed(h, state.port));
    if !host_ok {
        return GatewayError::new(
            StatusCode::FORBIDDEN,
            ErrorKind::Permission,
            "Host header not allowed",
        )
        .into_response();
    }
    if auth::is_browser_originated(
        text(header::ORIGIN),
        headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()),
    ) {
        return GatewayError::new(
            StatusCode::FORBIDDEN,
            ErrorKind::Permission,
            "browser-originated requests are refused by the gateway",
        )
        .into_response();
    }
    let api_key = headers.get("x-api-key").and_then(|v| v.to_str().ok());
    let authorized = api_key.is_some_and(|k| auth::constant_time_eq(k.trim(), &state.token))
        || auth::bearer_matches(text(header::AUTHORIZATION), &state.token);
    if !authorized {
        return GatewayError::new(
            StatusCode::UNAUTHORIZED,
            ErrorKind::Authentication,
            "invalid or missing gateway token (send it as x-api-key or Authorization: Bearer; see \
             `omni-dev ai gateway env`)",
        )
        .into_response();
    }
    next.run(request).await
}

async fn fallback() -> Response {
    GatewayError::not_found(
        "the gateway serves /v1/messages, /v1/messages/count_tokens and /v1/models",
    )
    .into_response()
}

/// Reads the body as a JSON object.
fn json_object(body: Result<Bytes, BytesRejection>) -> Result<Map<String, Value>, GatewayError> {
    let bytes = body.map_err(|e| {
        GatewayError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            ErrorKind::RequestTooLarge,
            format!("request body could not be read: {e}"),
        )
    })?;
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(GatewayError::invalid("request body must be a JSON object")),
        Err(e) => Err(GatewayError::invalid(format!(
            "request body is not valid JSON: {e}"
        ))),
    }
}

/// Checks the policy and parses the translated fields.
fn parse_request(
    state: &AppState,
    body: Map<String, Value>,
) -> Result<(MessagesRequest, PolicyReport), GatewayError> {
    let report = policy::check_request(&body, state.config.unknown_fields)?;
    let request: MessagesRequest = serde_json::from_value(Value::Object(body))
        .map_err(|e| GatewayError::invalid(e.to_string()))?;
    if request.messages.is_empty() {
        return Err(GatewayError::invalid(
            "messages: at least one message is required",
        ));
    }
    Ok((request, report))
}

async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    state.stats.requests.fetch_add(1, Ordering::Relaxed);
    match handle_messages(&state, &headers, body).await {
        Ok(response) => response,
        Err(error) => {
            state.stats.record_error(&error);
            tracing::debug!(%error, "gateway request failed");
            error.into_response()
        }
    }
}

async fn handle_messages(
    state: &AppState,
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, GatewayError> {
    let body = json_object(body)?;
    let selected = state.selected(&body).await?;
    let state = &selected;
    if state.config.protocol == UpstreamProtocol::Bedrock {
        return super::bedrock::messages(state, headers, body).await;
    }
    if state.config.protocol == UpstreamProtocol::Anthropic {
        return super::passthrough::messages(state, headers, body).await;
    }
    let (request, mut report) = parse_request(state, body)?;
    let model = state.config.models.resolve(&request.model)?;
    let replay = match state.config.protocol {
        UpstreamProtocol::Chat => state.config.flavor.replays_reasoning(),
        UpstreamProtocol::Responses | UpstreamProtocol::Anthropic | UpstreamProtocol::Bedrock => {
            true
        }
    };
    let lowered = lower::lower(&request, &state.signer, replay, &mut report)?;
    let mut ctx = BuildContext {
        config: &state.config,
        model: &model,
        report: &mut report,
    };
    let (url, upstream_body) = match state.config.protocol {
        UpstreamProtocol::Responses => (
            format!("{}/v1/responses", state.config.upstream_url),
            responses::build_request(&request, &lowered, &mut ctx)?,
        ),
        UpstreamProtocol::Chat | UpstreamProtocol::Anthropic | UpstreamProtocol::Bedrock => (
            crate::claude::ai::openai::chat_completions_url(&state.config.upstream_url),
            chat::build_request(&request, &lowered, &mut ctx)?,
        ),
    };
    state.note_ignored(&report);
    let permit = state.permit()?;
    let upstream = state.send(&url, &upstream_body, &[]).await?;
    let wants_thinking = request
        .thinking
        .as_ref()
        .is_some_and(ThinkingConfig::wants_thinking);
    let display = request
        .thinking
        .as_ref()
        .map(ThinkingConfig::display)
        .unwrap_or_default();
    if request.stream {
        let settings = StreamSettings {
            message_id: new_message_id(),
            client_model: request.model.clone(),
            wants_thinking,
            display,
            signer: Arc::clone(&state.signer),
            names: lowered.names,
        };
        return Ok(stream_response(
            state, upstream, settings, permit, &report, &model,
        ));
    }
    let reply: Value = upstream
        .json()
        .await
        .map_err(|e| GatewayError::bad_gateway(format!("unreadable upstream reply: {e}")))?;
    drop(permit);
    let completion: Completion = match state.config.protocol {
        UpstreamProtocol::Responses => responses::parse_response(&reply)?,
        UpstreamProtocol::Chat | UpstreamProtocol::Anthropic | UpstreamProtocol::Bedrock => {
            chat::parse_response(&reply, state.config.think_tags)?
        }
    };
    let ctx = ResponseContext {
        client_model: &request.model,
        wants_thinking,
        display,
        signer: &state.signer,
        names: &lowered.names,
    };
    let mut response = Json(completion::to_response(completion, &ctx)).into_response();
    annotate(response.headers_mut(), &report, &model);
    Ok(response)
}

/// Either upstream stream parser.
enum StreamParser {
    Chat(ChatStreamParser),
    Responses(ResponsesStreamParser),
}

impl StreamParser {
    /// `Ok(None)` marks the end of the stream.
    fn push(&mut self, data: &str) -> Result<Option<Vec<UpstreamEvent>>, GatewayError> {
        match self {
            Self::Chat(parser) => parser.push(data),
            Self::Responses(parser) => parser.push(data).map(Some),
        }
    }

    fn finish(&mut self) -> Vec<UpstreamEvent> {
        match self {
            Self::Chat(parser) => parser.finish(),
            Self::Responses(_) => Vec::new(),
        }
    }
}

fn stream_response(
    state: &AppState,
    upstream: reqwest::Response,
    settings: StreamSettings,
    permit: Permit,
    report: &PolicyReport,
    model: &str,
) -> Response {
    let parser = match state.config.protocol {
        UpstreamProtocol::Responses => StreamParser::Responses(ResponsesStreamParser::default()),
        UpstreamProtocol::Chat | UpstreamProtocol::Anthropic | UpstreamProtocol::Bedrock => {
            StreamParser::Chat(ChatStreamParser::new(state.config.think_tags))
        }
    };
    let (tx, rx) = mpsc::channel::<Bytes>(64);
    let stats = Arc::clone(&state.stats);
    tokio::spawn(async move {
        let _permit = permit;
        if let Err(error) = pump(upstream, parser, AnthropicStream::new(settings), &tx).await {
            stats.record_error(&error);
        }
    });
    let body = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|bytes| (Ok::<_, Infallible>(bytes), rx))
    });
    let mut response = Response::new(Body::from_stream(body));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    annotate(headers, report, model);
    response
}

/// Sends events to the client; `false` once it has gone away.
async fn emit(tx: &mpsc::Sender<Bytes>, events: Vec<Value>) -> bool {
    for event in events {
        if tx.send(encode(&event)).await.is_err() {
            return false;
        }
    }
    true
}

/// Drives one upstream stream into Anthropic events. Returns the error that
/// ended it, if any (already sent to the client as an `error` event).
async fn pump(
    upstream: reqwest::Response,
    mut parser: StreamParser,
    mut builder: AnthropicStream,
    tx: &mpsc::Sender<Bytes>,
) -> Result<(), GatewayError> {
    if !emit(tx, builder.start()).await {
        return Ok(());
    }
    let mut bytes = upstream.bytes_stream();
    let mut sse = SseParser::default();
    let mut ended = false;
    while !ended {
        let Ok(next) = tokio::time::timeout(PING_INTERVAL, bytes.next()).await else {
            if !emit(tx, vec![ping()]).await {
                return Ok(());
            }
            continue;
        };
        let events = match next {
            None => break,
            Some(Err(e)) => {
                let error = GatewayError::bad_gateway(format!("upstream stream broke: {e}"));
                emit(tx, builder.fail(&error)).await;
                return Err(error);
            }
            Some(Ok(chunk)) => sse.push(&chunk),
        };
        for event in events {
            match feed(&mut parser, &mut builder, &event.data) {
                Ok(Some(out)) => {
                    if !emit(tx, out).await {
                        return Ok(());
                    }
                }
                Ok(None) => ended = true,
                Err(error) => {
                    emit(tx, builder.fail(&error)).await;
                    return Err(error);
                }
            }
        }
    }
    if !ended {
        if let Some(event) = sse.finish() {
            match feed(&mut parser, &mut builder, &event.data) {
                Ok(Some(out)) => {
                    emit(tx, out).await;
                }
                Ok(None) => ended = true,
                Err(error) => {
                    emit(tx, builder.fail(&error)).await;
                    return Err(error);
                }
            }
        }
    }
    let mut out = Vec::new();
    for event in parser.finish() {
        out.extend(builder.push(event));
    }
    if !ended && !builder.saw_finish() {
        // The connection closed before `[DONE]`, a `finish_reason` or
        // `response.completed`: the reply was cut off. Ending it with
        // `message_stop` would pass a truncated answer (or a half-written tool
        // call) off as complete, so it ends with an `error` event instead,
        // which clients retry.
        emit(tx, out).await;
        let error =
            GatewayError::bad_gateway("the upstream stream ended before the reply was complete");
        emit(tx, builder.fail(&error)).await;
        return Err(error);
    }
    out.extend(builder.finish());
    emit(tx, out).await;
    Ok(())
}

/// Parses one SSE `data` and feeds the builder. `Ok(None)` marks the end.
fn feed(
    parser: &mut StreamParser,
    builder: &mut AnthropicStream,
    data: &str,
) -> Result<Option<Vec<Value>>, GatewayError> {
    let Some(events) = parser.push(data)? else {
        return Ok(None);
    };
    Ok(Some(
        events.into_iter().flat_map(|e| builder.push(e)).collect(),
    ))
}

async fn count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    state.stats.requests.fetch_add(1, Ordering::Relaxed);
    let result = async {
        let body = json_object(body)?;
        let selected = state.selected(&body).await?;
        let state = &selected;
        if state.config.protocol == UpstreamProtocol::Anthropic {
            return super::passthrough::count_tokens(state, &headers, body).await;
        }
        let _permit = state.permit()?;
        if state.config.protocol == UpstreamProtocol::Bedrock {
            return Ok(estimated_tokens_response(tokens::estimate_native(&body)?));
        }
        Ok(estimate_response(&parse_request(state, body)?.0))
    }
    .await;
    result.unwrap_or_else(|error| {
        state.stats.record_error(&error);
        error.into_response()
    })
}

/// One Anthropic model object.
fn model_object(id: &str) -> Value {
    json!({"type": "model", "id": id, "display_name": id, "created_at": "1970-01-01T00:00:00Z"})
}

async fn list_models(State(state): State<AppState>) -> Response {
    state.stats.requests.fetch_add(1, Ordering::Relaxed);
    let ids = if state.config.routing.upstreams.is_empty() {
        state.config.models.listed()
    } else {
        let result = async {
            let _permit = state.permit()?;
            state
                .discovery
                .listed(&state.config.routing, &state.http)
                .await
        }
        .await;
        match result {
            Ok(ids) => ids,
            Err(error) => {
                state.stats.record_error(&error);
                return error.into_response();
            }
        }
    };
    Json(json!({
        "data": ids.iter().map(|id| model_object(id)).collect::<Vec<_>>(),
        "has_more": false,
        "first_id": ids.first(),
        "last_id": ids.last(),
    }))
    .into_response()
}

async fn get_model(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    state.stats.requests.fetch_add(1, Ordering::Relaxed);
    let result = if state.config.routing.upstreams.is_empty() {
        state.config.models.resolve(&id)
    } else {
        state
            .selected(&serde_json::from_value(json!({"model": id})).unwrap_or_default())
            .await
            .map(|_| id.clone())
    };
    match result {
        Ok(_) => Json(model_object(&id)).into_response(),
        Err(error) => {
            state.stats.record_error(&error);
            error.into_response()
        }
    }
}

/// Helpers shared with sibling test modules.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(crate) mod tests {
    use super::*;
    use crate::gateway::config::ConfigOverrides;
    use crate::test_support::env::MapEnv;

    /// A state with a default model, for unit tests that need no server.
    pub(crate) fn state(flavor: &str, protocol: &str) -> AppState {
        let env = MapEnv::new()
            .with("OMNI_DEV_GATEWAY_UPSTREAM_URL", "http://127.0.0.1:1")
            .with("OMNI_DEV_GATEWAY_FLAVOR", flavor)
            .with("OMNI_DEV_GATEWAY_UPSTREAM_PROTOCOL", protocol)
            .with("OMNI_DEV_GATEWAY_MODEL", "up-default");
        let config = GatewayConfig::from_env_with(&env, &ConfigOverrides::default()).unwrap();
        AppState::new(config, "tok".into(), 1).unwrap()
    }

    #[tokio::test]
    async fn send_rejects_whitespace_equivalent_inbound_credentials_before_network_io() {
        let upstream = wiremock::MockServer::start().await;
        for protocol in ["chat", "responses", "anthropic", "bedrock"] {
            for credential in [
                "synthetic-inbound ",
                " synthetic-inbound",
                "\t synthetic-inbound \t",
            ] {
                let mut state = state("generic", protocol);
                state.token = Arc::new("synthetic-inbound".into());
                // Exercise the send-time defense independently of startup.
                Arc::make_mut(&mut state.config).api_key =
                    Some(crate::utils::secret::Secret::new(credential));
                let error = state
                    .send(&upstream.uri(), &json!({}), &[])
                    .await
                    .unwrap_err();
                assert_eq!(error.status, StatusCode::BAD_REQUEST);
                assert_eq!(
                    error.message,
                    "upstream key must not equal the inbound gateway token"
                );
                assert!(!error.message.contains("synthetic-inbound"));
            }
        }
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn annotate_sets_both_headers_and_skips_empty_ignores() {
        let mut headers = HeaderMap::new();
        let mut report = PolicyReport::default();
        annotate(&mut headers, &report, "m");
        assert!(headers.get(IGNORED_HEADER).is_none());
        report.ignore("metadata");
        report.ignore("speed");
        annotate(&mut headers, &report, "m");
        assert_eq!(headers[IGNORED_HEADER], "metadata, speed");
        assert_eq!(headers[UPSTREAM_MODEL_HEADER], "m");
    }

    #[test]
    fn the_permit_counts_in_flight_and_caps() {
        let state = state("generic", "chat");
        let permits: Vec<_> = (0..crate::gateway::config::DEFAULT_MAX_CONCURRENT)
            .map(|_| state.permit().unwrap())
            .collect();
        assert_eq!(state.stats.snapshot().in_flight, permits.len() as u64);
        let err = state.permit().err().unwrap();
        assert_eq!(err.status, StatusCode::TOO_MANY_REQUESTS);
        drop(permits);
        assert_eq!(state.stats.snapshot().in_flight, 0);
    }

    #[test]
    fn note_ignored_is_quiet_after_the_first_time() {
        let state = state("generic", "chat");
        let mut report = PolicyReport::default();
        state.note_ignored(&report);
        report.ignore("metadata");
        state.note_ignored(&report);
        state.note_ignored(&report);
        assert!(state.warned.lock().unwrap().contains("metadata"));
    }

    #[test]
    fn stats_record_errors() {
        let stats = Stats::default();
        stats.record_error(&GatewayError::invalid("x"));
        let snapshot = stats.snapshot();
        assert_eq!(snapshot.errors, 1);
        assert_eq!(
            snapshot.last_error.as_deref(),
            Some("invalid_request_error: x")
        );
    }
}
