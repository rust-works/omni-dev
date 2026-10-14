//! End-to-end tests: a real [`GatewayServer`] on a loopback port in front of a
//! `wiremock` upstream, driven over HTTP the way Claude Code drives it.
//!
//! Streaming replies are compared against golden snapshots of the Anthropic
//! event sequence produced from the upstream fixtures in
//! `tests/fixtures/gateway/`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde_json::{json, Value};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::config::{ConfigOverrides, GatewayConfig};
use super::server::GatewayServer;
use super::signature::Signer;
use crate::test_support::env::MapEnv;

const TOKEN: &str = "test-gateway-token";

struct Harness {
    upstream: MockServer,
    server: GatewayServer,
    client: reqwest::Client,
    token: String,
}

impl Harness {
    async fn start(vars: &[(&str, &str)]) -> Self {
        Self::start_with_token(vars, TOKEN).await
    }

    async fn start_with_token(vars: &[(&str, &str)], token: &str) -> Self {
        let upstream = MockServer::start().await;
        let mut env = MapEnv::new()
            .with("OMNI_DEV_GATEWAY_UPSTREAM_URL", &upstream.uri())
            .with("OMNI_DEV_GATEWAY_MODEL", "up-model")
            .with("OMNI_DEV_GATEWAY_API_KEY", "upstream-key")
            .with("OMNI_DEV_GATEWAY_PORT", "0");
        for (k, v) in vars {
            env = env.with(k, v);
        }
        let config = GatewayConfig::from_env_with(&env, &ConfigOverrides::default()).unwrap();
        let server = GatewayServer::start(config, token.into()).unwrap();
        Self {
            upstream,
            server,
            client: reqwest::Client::new(),
            token: token.to_string(),
        }
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.server.port())
    }

    async fn post(&self, path: &str, body: &Value) -> reqwest::Response {
        self.client
            .post(self.url(path))
            .bearer_auth(&self.token)
            .header("anthropic-version", "2023-06-01")
            .json(body)
            .send()
            .await
            .unwrap()
    }

    /// The JSON bodies the upstream received.
    async fn upstream_bodies(&self) -> Vec<Value> {
        self.upstream
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect()
    }
}

fn fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/gateway/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(path).unwrap()
}

fn sse(body: String) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

fn message(stream: bool) -> Value {
    json!({
        "model": "claude-sonnet-5",
        "max_tokens": 1024,
        "stream": stream,
        "messages": [{"role": "user", "content": "hi"}]
    })
}

/// Replaces the random message id so snapshots are stable.
fn normalize(text: &str) -> String {
    let re = regex::Regex::new(r"msg_gw_[A-Za-z0-9]{24}").unwrap();
    re.replace_all(text, "msg_gw_<id>").into_owned()
}

async fn stream_snapshot(
    fixture_name: &str,
    request: Value,
    vars: &[(&str, &str)],
    route: &str,
) -> String {
    let h = Harness::start(vars).await;
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(sse(fixture(fixture_name)))
        .mount(&h.upstream)
        .await;
    let response = h.post("/v1/messages", &request).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    normalize(&response.text().await.unwrap())
}

// ── Guards ──────────────────────────────────────────────────────────

#[tokio::test]
async fn requests_without_the_token_are_refused() {
    let h = Harness::start(&[]).await;
    let missing = h
        .client
        .post(h.url("/v1/messages"))
        .json(&message(false))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 401);
    let body: Value = missing.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "authentication_error");

    let wrong = h
        .client
        .post(h.url("/v1/messages"))
        .header("x-api-key", "nope")
        .json(&message(false))
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    assert!(h.upstream_bodies().await.is_empty());
}

#[tokio::test]
async fn bearer_auth_is_accepted() {
    let h = Harness::start(&[]).await;
    let response = h
        .client
        .get(h.url("/v1/models"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn browser_and_rebinding_requests_are_refused() {
    let h = Harness::start(&[]).await;
    let origin = h
        .client
        .get(h.url("/v1/models"))
        .header("x-api-key", TOKEN)
        .header("origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(origin.status(), 403);
    let fetch_site = h
        .client
        .get(h.url("/v1/models"))
        .header("x-api-key", TOKEN)
        .header("sec-fetch-site", "cross-site")
        .send()
        .await
        .unwrap();
    assert_eq!(fetch_site.status(), 403);
    let host = h
        .client
        .get(h.url("/v1/models"))
        .header("x-api-key", TOKEN)
        .header("host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(host.status(), 403);
    let body: Value = host.json().await.unwrap();
    assert_eq!(body["error"]["type"], "permission_error");
}

#[tokio::test]
async fn unknown_paths_are_anthropic_404s() {
    let h = Harness::start(&[]).await;
    let response = h
        .client
        .get(h.url("/v1/files"))
        .header("x-api-key", TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "not_found_error");
}

// ── Non-streaming ───────────────────────────────────────────────────

#[tokio::test]
async fn a_non_streaming_request_round_trips() {
    let h = Harness::start(&[]).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer upstream-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "c", "model": "up-model",
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": "Hello!"}}],
            "usage": {"prompt_tokens": 9, "completion_tokens": 2}
        })))
        .mount(&h.upstream)
        .await;
    let mut request = message(false);
    request["metadata"] = json!({"user_id": "u"});
    request["system"] =
        json!([{"type": "text", "text": "Be kind.", "cache_control": {"type": "ephemeral"}}]);
    let response = h.post("/v1/messages", &request).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-omni-dev-gateway-upstream-model"],
        "up-model"
    );
    assert_eq!(
        response.headers()["x-omni-dev-gateway-ignored"],
        "cache_control, metadata"
    );
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["type"], "message");
    assert_eq!(body["model"], "claude-sonnet-5");
    assert_eq!(body["content"], json!([{"type": "text", "text": "Hello!"}]));
    assert_eq!(body["stop_reason"], "end_turn");
    assert_eq!(body["usage"]["input_tokens"], 9);

    let sent = &h.upstream_bodies().await[0];
    assert_eq!(sent["model"], "up-model");
    assert_eq!(
        sent["messages"][0],
        json!({"role": "system", "content": "Be kind."})
    );
    assert!(sent.get("metadata").is_none());
    assert_eq!(h.server.stats().requests, 1);
}

#[tokio::test]
async fn rejected_fields_never_reach_the_upstream() {
    let h = Harness::start(&[]).await;
    let mut request = message(false);
    request["container"] = json!("ctr_1");
    let response = h.post("/v1/messages", &request).await;
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("container"));

    let mut tools = message(false);
    tools["tools"] = json!([{"type": "web_search_20260318", "name": "web_search"}]);
    assert_eq!(h.post("/v1/messages", &tools).await.status(), 400);
    for bad in [json!("not an object"), json!({"model": "m"})] {
        assert_eq!(h.post("/v1/messages", &bad).await.status(), 400);
    }
    let raw = h
        .client
        .post(h.url("/v1/messages"))
        .header("x-api-key", TOKEN)
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(raw.status(), 400);
    let empty = json!({"model": "m", "max_tokens": 1, "messages": []});
    assert_eq!(h.post("/v1/messages", &empty).await.status(), 400);
    assert!(h.upstream_bodies().await.is_empty());
    assert!(h.server.stats().errors >= 5);
}

#[tokio::test]
async fn unknown_fields_can_be_downgraded_to_ignored() {
    let h = Harness::start(&[("OMNI_DEV_GATEWAY_UNKNOWN_FIELDS", "warn")]).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"finish_reason": "stop", "message": {"content": "ok"}}]
        })))
        .mount(&h.upstream)
        .await;
    let mut request = message(false);
    request["future_field"] = json!(true);
    let response = h.post("/v1/messages", &request).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-omni-dev-gateway-ignored"],
        "future_field"
    );
}

#[tokio::test]
async fn upstream_errors_map_to_anthropic_errors() {
    let h = Harness::start(&[]).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": {"message": "This model's maximum context length is 8192 tokens", "code": "context_length_exceeded"}
        })))
        .up_to_n_times(1)
        .mount(&h.upstream)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
        .up_to_n_times(1)
        .mount(&h.upstream)
        .await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "3")
                .set_body_string("slow down"),
        )
        .mount(&h.upstream)
        .await;

    let too_long = h.post("/v1/messages", &message(false)).await;
    assert_eq!(too_long.status(), 400);
    let body: Value = too_long.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .starts_with("prompt is too long"));

    let auth = h.post("/v1/messages", &message(false)).await;
    assert_eq!(auth.status(), 403);

    let limited = h.post("/v1/messages", &message(false)).await;
    assert_eq!(limited.status(), 429);
    assert_eq!(limited.headers()["retry-after"], "3");
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_502() {
    let mut harness_env = vec![("OMNI_DEV_GATEWAY_UPSTREAM_URL", "http://127.0.0.1:9")];
    harness_env.push(("OMNI_DEV_GATEWAY_MODEL", "m"));
    let h = Harness::start(&harness_env).await;
    let response = h.post("/v1/messages", &message(false)).await;
    assert_eq!(response.status(), 502);
}

#[tokio::test]
async fn unmapped_claude_models_are_404s() {
    let upstream = MockServer::start().await;
    let env = MapEnv::new()
        .with("OMNI_DEV_GATEWAY_UPSTREAM_URL", &upstream.uri())
        .with("OMNI_DEV_GATEWAY_PORT", "0")
        .with("OMNI_DEV_GATEWAY_MODEL_MAP", "claude-haiku-*=small");
    let config = GatewayConfig::from_env_with(&env, &ConfigOverrides::default()).unwrap();
    let server = GatewayServer::start(config, TOKEN.into()).unwrap();
    let client = reqwest::Client::new();
    let response = client
        .post(format!("http://127.0.0.1:{}/v1/messages", server.port()))
        .header("x-api-key", TOKEN)
        .json(&message(false))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    let models: Value = client
        .get(format!("http://127.0.0.1:{}/v1/models", server.port()))
        .header("x-api-key", TOKEN)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["data"], json!([]));
    server.shutdown().await;
}

// ── Reasoning across turns ──────────────────────────────────────────

#[tokio::test]
async fn reasoning_is_signed_then_replayed_on_the_next_turn() {
    let h = Harness::start(&[("OMNI_DEV_GATEWAY_FLAVOR", "fireworks")]).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"finish_reason": "tool_calls", "message": {
                "content": null,
                "reasoning_content": "I should list files.",
                "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "Bash", "arguments": "{\"command\":\"ls\"}"}}]
            }}]
        })))
        .mount(&h.upstream)
        .await;
    let tools = json!([{"name": "Bash", "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}}]);
    let first = json!({
        "model": "claude-opus-5", "max_tokens": 4096,
        "thinking": {"type": "enabled", "budget_tokens": 2048},
        "tools": tools,
        "messages": [{"role": "user", "content": "what's here?"}]
    });
    let reply: Value = h.post("/v1/messages", &first).await.json().await.unwrap();
    assert_eq!(reply["stop_reason"], "tool_use");
    let thinking = reply["content"][0].clone();
    assert_eq!(thinking["type"], "thinking");
    assert!(Signer::from_token(TOKEN)
        .verify(
            "I should list files.",
            thinking["signature"].as_str().unwrap()
        )
        .is_some());
    let tool_use = reply["content"][1].clone();
    assert_eq!(tool_use["input"], json!({"command": "ls"}));

    let second = json!({
        "model": "claude-opus-5", "max_tokens": 4096,
        "thinking": {"type": "enabled", "budget_tokens": 2048},
        "tools": tools,
        "messages": [
            {"role": "user", "content": "what's here?"},
            {"role": "assistant", "content": [thinking, tool_use]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "a.rs"}]}
        ]
    });
    assert_eq!(h.post("/v1/messages", &second).await.status(), 200);
    let bodies = h.upstream_bodies().await;
    let replayed = &bodies[1]["messages"][1];
    assert_eq!(replayed["reasoning_content"], "I should list files.");
    assert_eq!(replayed["tool_calls"][0]["id"], "call_1");
    assert_eq!(
        bodies[1]["thinking"],
        json!({"type": "enabled", "budget_tokens": 2048})
    );
    assert!(bodies[1].get("reasoning_effort").is_none());
    assert_eq!(bodies[1]["messages"][2]["role"], "tool");
}

// ── Streaming goldens ───────────────────────────────────────────────

#[tokio::test]
async fn stream_text_with_trailing_usage() {
    let out = stream_snapshot("chat_text.sse", message(true), &[], "/v1/chat/completions").await;
    insta::assert_snapshot!(out);
}

#[tokio::test]
async fn stream_thinking_then_text() {
    let mut request = message(true);
    request["thinking"] = json!({"type": "enabled", "budget_tokens": 2048});
    let out = stream_snapshot(
        "chat_thinking_text.sse",
        request,
        &[],
        "/v1/chat/completions",
    )
    .await;
    insta::assert_snapshot!(out);
}

#[tokio::test]
async fn stream_interleaved_parallel_tool_calls() {
    let mut request = message(true);
    request["tools"] = json!([
        {"name": "Read", "input_schema": {"type": "object"}},
        {"name": "Grep", "input_schema": {"type": "object"}}
    ]);
    let out = stream_snapshot(
        "chat_parallel_tools.sse",
        request,
        &[],
        "/v1/chat/completions",
    )
    .await;
    insta::assert_snapshot!(out);
}

#[tokio::test]
async fn stream_inline_think_tags() {
    let mut request = message(true);
    request["thinking"] = json!({"type": "adaptive"});
    let out = stream_snapshot(
        "chat_inline_think.sse",
        request,
        &[],
        "/v1/chat/completions",
    )
    .await;
    insta::assert_snapshot!(out);
}

#[tokio::test]
async fn stream_refusal() {
    let out = stream_snapshot(
        "chat_refusal.sse",
        message(true),
        &[],
        "/v1/chat/completions",
    )
    .await;
    insta::assert_snapshot!(out);
}

#[tokio::test]
async fn stream_midstream_error() {
    let out = stream_snapshot(
        "chat_midstream_error.sse",
        message(true),
        &[],
        "/v1/chat/completions",
    )
    .await;
    insta::assert_snapshot!(out);
}

#[tokio::test]
async fn stream_responses_reasoning_and_tool() {
    let mut request = message(true);
    request["thinking"] = json!({"type": "enabled", "budget_tokens": 4096});
    request["tools"] = json!([{"name": "Bash", "input_schema": {"type": "object"}}]);
    let vars = [
        ("OMNI_DEV_GATEWAY_UPSTREAM_PROTOCOL", "responses"),
        ("OMNI_DEV_GATEWAY_FLAVOR", "openai"),
    ];
    let out = stream_snapshot(
        "responses_reasoning_tool.sse",
        request,
        &vars,
        "/v1/responses",
    )
    .await;
    insta::assert_snapshot!(out);
}

// ── Responses API ───────────────────────────────────────────────────

#[tokio::test]
async fn a_non_streaming_responses_request_round_trips() {
    let h = Harness::start(&[
        ("OMNI_DEV_GATEWAY_UPSTREAM_PROTOCOL", "responses"),
        ("OMNI_DEV_GATEWAY_FLAVOR", "openai"),
    ])
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "completed",
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "Hi."}]}],
            "usage": {"input_tokens": 3, "output_tokens": 1}
        })))
        .mount(&h.upstream)
        .await;
    let reply: Value = h
        .post("/v1/messages", &message(false))
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(reply["content"][0]["text"], "Hi.");
    let sent = &h.upstream_bodies().await[0];
    assert_eq!(sent["store"], false);
    assert_eq!(sent["max_output_tokens"], 1024);
}

// ── count_tokens and models ─────────────────────────────────────────

#[tokio::test]
async fn count_tokens_is_a_flagged_estimate() {
    let h = Harness::start(&[]).await;
    let response = h
        .post(
            "/v1/messages/count_tokens",
            &json!({"model": "claude-x", "messages": [{"role": "user", "content": "x".repeat(40)}]}),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-omni-dev-token-estimate"], "heuristic");
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["input_tokens"], 14);
    let bad = h
        .post("/v1/messages/count_tokens", &json!({"model": "x"}))
        .await;
    assert_eq!(bad.status(), 400);
}

#[tokio::test]
async fn models_are_listed_and_retrievable() {
    let h = Harness::start(&[(
        "OMNI_DEV_GATEWAY_MODEL_MAP",
        "claude-opus-5=big,claude-*=small",
    )])
    .await;
    let get = |p: &str| h.client.get(h.url(p)).header("x-api-key", TOKEN).send();
    let list: Value = get("/v1/models").await.unwrap().json().await.unwrap();
    let ids: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["claude-opus-5", "up-model"]);
    assert_eq!(list["has_more"], false);
    let one = get("/v1/models/claude-haiku-9").await.unwrap();
    assert_eq!(one.status(), 200);
    let nested = get("/v1/models/accounts/fireworks/models/glm")
        .await
        .unwrap();
    let nested: Value = nested.json().await.unwrap();
    assert_eq!(nested["id"], "accounts/fireworks/models/glm");
}

// ── Passthrough ─────────────────────────────────────────────────────

#[tokio::test]
async fn passthrough_maps_the_model_and_swaps_auth() {
    let h = Harness::start(&[
        ("OMNI_DEV_GATEWAY_UPSTREAM_PROTOCOL", "anthropic"),
        ("OMNI_DEV_GATEWAY_FLAVOR", "fireworks"),
    ])
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer upstream-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_up", "type": "message", "role": "assistant", "model": "up-model",
            "content": [{"type": "text", "text": "passed"}], "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&h.upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
        .mount(&h.upstream)
        .await;
    let mut request = message(false);
    request["inference_geo"] = json!("us");
    request["context_management"] = json!({"edits": []});
    let response = h.post("/v1/messages", &request).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-omni-dev-gateway-ignored"],
        "inference_geo"
    );
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["content"][0]["text"], "passed");
    let sent = &h.upstream_bodies().await[0];
    assert_eq!(sent["model"], "up-model");
    assert!(sent.get("inference_geo").is_none());
    assert_eq!(sent["context_management"], json!({"edits": []}));

    let count = h
        .post(
            "/v1/messages/count_tokens",
            &json!({"model": "claude-x", "messages": [{"role": "user", "content": "abcd"}]}),
        )
        .await;
    assert_eq!(count.status(), 200);
    assert_eq!(count.headers()["x-omni-dev-token-estimate"], "heuristic");
}

#[tokio::test]
async fn passthrough_forwards_exact_counts_and_errors() {
    let h = Harness::start(&[("OMNI_DEV_GATEWAY_UPSTREAM_PROTOCOL", "anthropic")]).await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .and(header("x-api-key", "upstream-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 77})))
        .mount(&h.upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(529).set_body_json(json!({
            "type": "error", "error": {"type": "overloaded_error", "message": "busy"}
        })))
        .mount(&h.upstream)
        .await;
    let count: Value = h
        .post(
            "/v1/messages/count_tokens",
            &json!({"model": "claude-x", "messages": []}),
        )
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(count["input_tokens"], 77);
    let overloaded = h.post("/v1/messages", &message(true)).await;
    assert_eq!(overloaded.status(), 529);
    let body: Value = overloaded.json().await.unwrap();
    assert_eq!(body["error"]["type"], "overloaded_error");
    assert!(body["error"]["message"].as_str().unwrap().contains("busy"));
}

// ── A recorded Claude Code session ──────────────────────────────────

/// Replays the requests a real `claude -p` session sent through the gateway
/// (captured 2026-10-14, scrubbed; see the fixture's `_note`), checking the
/// gateway's answer to each and what reached the upstream.
#[tokio::test]
async fn a_recorded_claude_code_session_replays() {
    let session: Value = serde_json::from_str(&fixture("claude_code_session.json")).unwrap();
    let h =
        Harness::start_with_token(&[("OMNI_DEV_GATEWAY_FLAVOR", "fireworks")], "e2e-token").await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(fixture("chat_text.sse")))
        .mount(&h.upstream)
        .await;

    // Turn 1 as sent: `safeguards` is a safety control the gateway refuses to
    // drop silently, so it is rejected (Claude Code retries without it).
    let turn1 = session["turn1"].clone();
    let rejected = h.post("/v1/messages?beta=true", &turn1).await;
    assert_eq!(rejected.status(), 400);
    let body: Value = rejected.json().await.unwrap();
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("safeguards"));

    // Turn 1 as Claude Code retries it: accepted, `display: updates` included.
    let mut retried = turn1;
    retried.as_object_mut().unwrap().remove("safeguards");
    assert_eq!(retried["thinking"]["display"], "updates");
    let accepted = h.post("/v1/messages?beta=true", &retried).await;
    assert_eq!(accepted.status(), 200);
    assert_eq!(
        accepted.headers()["x-omni-dev-gateway-ignored"],
        "context_management, metadata"
    );
    assert!(accepted
        .text()
        .await
        .unwrap()
        .contains("event: message_stop"));

    // Turn 2 replays the gateway-signed thinking block from turn 1.
    let turn2 = h.post("/v1/messages?beta=true", &session["turn2"]).await;
    assert_eq!(turn2.status(), 200);
    assert_eq!(
        turn2.headers()["x-omni-dev-gateway-ignored"],
        "context_management, metadata"
    );

    let bodies = h.upstream_bodies().await;
    let sent = bodies.last().unwrap();
    assert_eq!(sent["reasoning_effort"], "medium");
    assert!(sent.get("thinking").is_none());
    assert_eq!(sent["tools"].as_array().unwrap().len(), 21);
    let messages = sent["messages"].as_array().unwrap();
    let assistant = messages.iter().find(|m| m["role"] == "assistant").unwrap();
    assert_eq!(assistant["reasoning_content"], "I will run the command.");
    assert_eq!(assistant["tool_calls"][0]["function"]["name"], "Bash");
    let roles: Vec<&str> = messages
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(
        roles,
        ["system", "user", "system", "assistant", "tool", "system"]
    );
}

#[tokio::test]
async fn omitted_reasoning_is_still_replayed() {
    // With `display: omitted` the client never sees the reasoning, but the
    // gateway's signature carries it, so the next turn still replays it.
    let h = Harness::start(&[("OMNI_DEV_GATEWAY_FLAVOR", "fireworks")]).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(sse(fixture("chat_thinking_text.sse")))
        .mount(&h.upstream)
        .await;
    let mut first = message(true);
    first["thinking"] = json!({"type": "adaptive", "display": "omitted"});
    let stream = h.post("/v1/messages", &first).await.text().await.unwrap();
    assert!(!stream.contains("thinking_delta"));
    let signature = stream
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|d| serde_json::from_str::<Value>(d).unwrap())
        .find(|e| e["delta"]["type"] == "signature_delta")
        .unwrap()["delta"]["signature"]
        .as_str()
        .unwrap()
        .to_string();
    let mut second = message(false);
    second["thinking"] = json!({"type": "adaptive", "display": "omitted"});
    second["messages"] = json!([
        {"role": "user", "content": "hi"},
        {"role": "assistant", "content": [
            {"type": "thinking", "thinking": "", "signature": signature},
            {"type": "text", "text": "Autumn moon rises"}
        ]},
        {"role": "user", "content": "again"}
    ]);
    h.post("/v1/messages", &second).await;
    let bodies = h.upstream_bodies().await;
    assert_eq!(
        bodies[1]["messages"][1]["reasoning_content"],
        "The user wants a haiku."
    );
}

#[tokio::test]
async fn a_truncated_stream_ends_with_an_error_not_a_clean_stop() {
    let out = stream_snapshot(
        "chat_truncated.sse",
        message(true),
        &[],
        "/v1/chat/completions",
    )
    .await;
    assert!(!out.contains("message_stop"), "{out}");
    assert!(out.contains("event: error"), "{out}");
    assert!(out.contains("ended before the reply was complete"), "{out}");
}

#[tokio::test]
async fn unknown_block_types_are_dropped_under_warn() {
    let h = Harness::start(&[("OMNI_DEV_GATEWAY_UNKNOWN_FIELDS", "warn")]).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"finish_reason": "stop", "message": {"content": "ok"}}]
        })))
        .mount(&h.upstream)
        .await;
    let mut request = message(false);
    request["messages"] = json!([
        {"role": "user", "content": [
            {"type": "text", "text": "hi"},
            {"type": "hologram", "data": 1}
        ]},
        {"role": "assistant", "content": [{"type": "hologram"}, {"type": "text", "text": "a"}]},
        {"role": "user", "content": "b"}
    ]);
    let response = h.post("/v1/messages", &request).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["x-omni-dev-gateway-ignored"],
        "content block type `hologram`"
    );
    let sent = &h.upstream_bodies().await[0];
    assert_eq!(
        sent["messages"][0],
        json!({"role": "user", "content": "hi"})
    );
}

#[tokio::test]
async fn passthrough_to_anthropic_sends_only_x_api_key() {
    let h = Harness::start(&[("OMNI_DEV_GATEWAY_UPSTREAM_PROTOCOL", "anthropic")]).await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"type": "message", "content": []})),
        )
        .mount(&h.upstream)
        .await;
    assert_eq!(h.post("/v1/messages", &message(false)).await.status(), 200);
    let requests = h.upstream.received_requests().await.unwrap();
    let headers = &requests[0].headers;
    assert_eq!(headers.get("x-api-key").unwrap(), "upstream-key");
    assert!(headers.get("authorization").is_none());
}
