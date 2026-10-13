//! Live Fireworks conformance smoke test for the Anthropic gateway (#2226).
//!
//! Skipped (it passes without doing anything) unless
//! `OMNI_DEV_GATEWAY_LIVE_FIREWORKS=1`. It never runs in CI. To run it:
//!
//! ```sh
//! OMNI_DEV_GATEWAY_LIVE_FIREWORKS=1 \
//! OMNI_DEV_GATEWAY_API_KEY_FILE=/path/to/fireworks-key \
//! OMNI_DEV_GATEWAY_LIVE_FIREWORKS_MODEL=accounts/fireworks/models/<non-reasoning> \
//! OMNI_DEV_GATEWAY_LIVE_FIREWORKS_REASONING_MODEL=accounts/fireworks/models/<reasoning> \
//!   cargo test --test gateway_fireworks_live -- --nocapture
//! ```
//!
//! The key is read through the gateway's own configuration, so
//! `OMNI_DEV_GATEWAY_API_KEY` and its `_FILE`/`_COMMAND` companions all work.
//! Each check runs in both modes: Chat Completions translation (`fireworks`
//! flavor) and passthrough to Fireworks' Anthropic surface.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_dev::gateway::{ConfigOverrides, Flavor, GatewayConfig, GatewayServer, UpstreamProtocol};
use serde_json::{json, Value};

const FIREWORKS_URL: &str = "https://api.fireworks.ai/inference";
const TOKEN: &str = "live-test-token";

fn enabled() -> bool {
    std::env::var("OMNI_DEV_GATEWAY_LIVE_FIREWORKS").is_ok_and(|v| v == "1")
}

fn model(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|v| !v.is_empty())
}

async fn start(protocol: UpstreamProtocol, model: &str) -> GatewayServer {
    let overrides = ConfigOverrides {
        port: Some(0),
        upstream_url: Some(FIREWORKS_URL.into()),
        protocol: Some(protocol),
        flavor: Some(Flavor::Fireworks),
        model: Some(model.into()),
        model_map: Vec::new(),
    };
    let config = GatewayConfig::from_env(&overrides).expect("gateway configuration");
    assert!(
        config.api_key.is_some(),
        "set OMNI_DEV_GATEWAY_API_KEY (or _FILE / _COMMAND) to a Fireworks key"
    );
    GatewayServer::start(config, TOKEN.into()).await.unwrap()
}

async fn post(server: &GatewayServer, body: &Value) -> (u16, String) {
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{}/v1/messages", server.port()))
        .bearer_auth(TOKEN)
        .header("anthropic-version", "2023-06-01")
        .json(body)
        .send()
        .await
        .unwrap();
    (response.status().as_u16(), response.text().await.unwrap())
}

fn weather_tool() -> Value {
    json!([{
        "name": "get_weather",
        "description": "Current weather for a city.",
        "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}
    }])
}

#[tokio::test]
async fn a_non_reasoning_model_answers_streaming_and_not() {
    if !enabled() {
        return;
    }
    let Some(model) = model("OMNI_DEV_GATEWAY_LIVE_FIREWORKS_MODEL") else {
        eprintln!("skipped: OMNI_DEV_GATEWAY_LIVE_FIREWORKS_MODEL is not set");
        return;
    };
    for protocol in [UpstreamProtocol::Chat, UpstreamProtocol::Anthropic] {
        let server = start(protocol, &model).await;
        let mut body = json!({
            "model": "claude-sonnet-5", "max_tokens": 64,
            "messages": [{"role": "user", "content": "Reply with the single word: pong"}]
        });
        let (status, text) = post(&server, &body).await;
        assert_eq!(status, 200, "{protocol}: {text}");
        let reply: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reply["type"], "message", "{protocol}: {text}");
        assert!(text.to_lowercase().contains("pong"), "{protocol}: {text}");

        body["stream"] = json!(true);
        let (status, stream) = post(&server, &body).await;
        assert_eq!(status, 200, "{protocol}: {stream}");
        assert!(
            stream.contains("event: message_stop"),
            "{protocol}: {stream}"
        );
        server.shutdown().await;
    }
}

#[tokio::test]
async fn a_reasoning_model_uses_a_tool_across_two_turns() {
    if !enabled() {
        return;
    }
    let Some(model) = model("OMNI_DEV_GATEWAY_LIVE_FIREWORKS_REASONING_MODEL") else {
        eprintln!("skipped: OMNI_DEV_GATEWAY_LIVE_FIREWORKS_REASONING_MODEL is not set");
        return;
    };
    for protocol in [UpstreamProtocol::Chat, UpstreamProtocol::Anthropic] {
        let server = start(protocol, &model).await;
        let question =
            json!({"role": "user", "content": "What's the weather in Paris? Use the tool."});
        let first = json!({
            "model": "claude-opus-5", "max_tokens": 4096,
            "thinking": {"type": "enabled", "budget_tokens": 2048},
            "tools": weather_tool(),
            "messages": [question]
        });
        let (status, text) = post(&server, &first).await;
        assert_eq!(status, 200, "{protocol}: {text}");
        let reply: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reply["stop_reason"], "tool_use", "{protocol}: {text}");
        let content = reply["content"].as_array().unwrap().clone();
        let call = content.iter().find(|b| b["type"] == "tool_use").unwrap();
        eprintln!(
            "{protocol}: thinking blocks returned: {}",
            content.iter().filter(|b| b["type"] == "thinking").count()
        );

        let second = json!({
            "model": "claude-opus-5", "max_tokens": 4096,
            "thinking": {"type": "enabled", "budget_tokens": 2048},
            "tools": weather_tool(),
            "messages": [
                question,
                {"role": "assistant", "content": content},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": call["id"], "content": "Sunny, 21°C"}]}
            ]
        });
        let (status, text) = post(&server, &second).await;
        assert_eq!(status, 200, "{protocol}: {text}");
        let reply: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(reply["stop_reason"], "end_turn", "{protocol}: {text}");
        assert!(
            text.contains("21"),
            "{protocol}: the answer should use the tool result: {text}"
        );
        server.shutdown().await;
    }
}
