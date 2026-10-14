//! Local-only integration coverage for routing and native Bedrock transport.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use base64::Engine;
use serde_json::{json, Value};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::config::{ConfigOverrides, GatewayConfig};
use super::server::GatewayServer;
use crate::test_support::env::MapEnv;

struct Gateway {
    server: GatewayServer,
    client: reqwest::Client,
}
impl Gateway {
    fn new(upstreams: Value, routes: Value) -> Self {
        let env = MapEnv::new()
            .with(super::routing::UPSTREAMS_VAR, &upstreams.to_string())
            .with(super::routing::ROUTES_VAR, &routes.to_string())
            .with("OPENAI_API_KEY", "chat-only-key")
            .with("AWS_BEARER_TOKEN_BEDROCK", "bedrock-only-key")
            .with("OMNI_DEV_GATEWAY_PORT", "0");
        let config = GatewayConfig::from_env_with(&env, &ConfigOverrides::default()).unwrap();
        Self {
            server: GatewayServer::start(config, "inbound-only-token".into()).unwrap(),
            client: reqwest::Client::new(),
        }
    }
    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.server.port())
    }
    async fn post(&self, path: &str, body: &Value) -> reqwest::Response {
        self.client
            .post(self.url(path))
            .bearer_auth("inbound-only-token")
            .header("anthropic-beta", "example-beta")
            .json(body)
            .send()
            .await
            .unwrap()
    }
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(self.url(path))
            .bearer_auth("inbound-only-token")
            .send()
            .await
            .unwrap()
    }
}
fn request(model: &str) -> Value {
    json!({"model":model,"max_tokens":128,"thinking":{"type":"enabled","budget_tokens":1024},"messages":[{"role":"user","content":"hello"}]})
}
fn chat_reply() -> Value {
    json!({"choices":[{"message":{"content":"ok","reasoning_content":"reason","tool_calls":[{"id":"call_1","type":"function","function":{"name":"lookup","arguments":"{\"x\":1}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":3,"completion_tokens":4}})
}
fn native_reply() -> Value {
    json!({"id":"msg_native","type":"message","role":"assistant","model":"provider-native","content":[{"type":"thinking","thinking":"native reasoning","signature":"native-signature"},{"type":"tool_use","id":"tool_native","name":"lookup","input":{"x":1}}],"stop_reason":"tool_use","usage":{"input_tokens":3,"output_tokens":4}})
}

#[tokio::test]
async fn startup_rejects_whitespace_equivalent_keys_before_inference_or_discovery() {
    let upstream = MockServer::start().await;
    let temp = tempfile::tempdir().unwrap();
    for credential in [
        "synthetic-inbound ",
        " synthetic-inbound",
        "\t synthetic-inbound \t",
    ] {
        for protocol in ["chat", "responses", "anthropic", "bedrock"] {
            for routes in [json!([]), json!([{"pattern":"*","upstream":"mock"}])] {
                let named = MapEnv::new()
                    .with(
                        super::routing::UPSTREAMS_VAR,
                        &json!([{
                            "name":"mock", "url":upstream.uri(), "protocol":protocol,
                            "api_key_env":"OPENAI_API_KEY", "catalog_paths":["/v1/models"]
                        }])
                        .to_string(),
                    )
                    .with(super::routing::ROUTES_VAR, &routes.to_string())
                    .with("OPENAI_API_KEY", credential);
                let legacy = MapEnv::new()
                    .with(super::config::UPSTREAM_URL_VAR, &upstream.uri())
                    .with(super::config::UPSTREAM_PROTOCOL_VAR, protocol)
                    .with(super::config::API_KEY_VAR, credential);
                for env in [named, legacy] {
                    let env = env
                        .with(super::config::TOKEN_VAR, credential)
                        .with(super::config::PORT_VAR, "0");
                    let token = super::config::resolve_token_with(&env, &temp.path().join("token"))
                        .unwrap();
                    assert_eq!(token, "synthetic-inbound");
                    let config =
                        GatewayConfig::from_env_with(&env, &ConfigOverrides::default()).unwrap();
                    let error = GatewayServer::start(config, token)
                        .err()
                        .unwrap()
                        .to_string();
                    assert_eq!(
                        error,
                        "upstream keys must not equal the inbound gateway token"
                    );
                    assert!(!error.contains("synthetic-inbound"));
                }
            }
        }
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
    assert!(!temp.path().join("token").exists());
}

#[tokio::test]
async fn one_listener_switches_flavors_protocols_and_preserves_tools_reasoning_over_turns() {
    let chat = MockServer::start().await;
    let bedrock = MockServer::start().await;
    // Catalogs intentionally fail: custom routes must never need them.
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .expect(0)
        .mount(&chat)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer chat-only-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_reply()))
        .expect(2)
        .mount(&chat)
        .await;
    let arn = "arn:aws:bedrock:us-east-1:000000000000:inference-profile/example.claude";
    Mock::given(method("POST"))
        .and(path(
            "/model/arn%3Aaws%3Abedrock%3Aus-east-1%3A000000000000%3Ainference-profile%2Fexample.claude/invoke",
        ))
        .and(header("authorization", "Bearer bedrock-only-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(native_reply()))
        .expect(2)
        .mount(&bedrock)
        .await;
    let g = Gateway::new(
        json!([
            {"name":"chat","url":chat.uri(),"protocol":"chat","flavor":"fireworks","api_key_env":"OPENAI_API_KEY"},
            {"name":"native","url":bedrock.uri(),"protocol":"bedrock","api_key_env":"AWS_BEARER_TOKEN_BEDROCK"}
        ]),
        json!([
            {"pattern":"*","upstream":"chat","model":"chat-model"},
            {"pattern":"claude-native","upstream":"native","model":arn}
        ]),
    );
    for (model, native) in [("claude-chat", false), ("claude-native", true)] {
        let mut body = request(model);
        body["tools"] = json!([{"name":"lookup","input_schema":{"type":"object","properties":{"x":{"type":"integer"}}}}]);
        let response = g.post("/v1/messages", &body).await;
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
        let reply: Value = response.json().await.unwrap();
        assert_eq!(reply["model"], model);
        assert_eq!(reply["content"][0]["type"], "thinking");
        let tool_id = reply["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|block| block["type"] == "tool_use")
            .unwrap()["id"]
            .clone();
        body["messages"].as_array_mut().unwrap().extend([
            json!({"role":"assistant","content":reply["content"]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":tool_id,"content":"done"}]})
        ]);
        assert_eq!(g.post("/v1/messages", &body).await.status(), 200);
        if native {
            let received = bedrock.received_requests().await.unwrap();
            let sent: Value = serde_json::from_slice(&received[1].body).unwrap();
            assert_eq!(sent["messages"], body["messages"]);
            assert_eq!(sent["tools"], body["tools"]);
            assert_eq!(sent["thinking"], body["thinking"]);
            assert_eq!(
                sent["messages"][1]["content"][0]["signature"],
                "native-signature"
            );
            assert_eq!(sent["anthropic_version"], "bedrock-2023-05-31");
            assert_eq!(sent["anthropic_beta"], json!(["example-beta"]));
            assert!(sent.get("stream").is_none() && sent.get("model").is_none());
            assert!(received[1].url.as_str().contains("arn%3Aaws%3Abedrock"));
            assert!(received[1].url.as_str().contains("%2Fexample.claude"));
        } else {
            let received = chat.received_requests().await.unwrap();
            let sent: Value = serde_json::from_slice(&received[1].body).unwrap();
            assert_eq!(sent["model"], "chat-model");
            assert_eq!(sent["messages"][1]["reasoning_content"], "reason");
            assert_eq!(sent["messages"][2]["role"], "tool");
            assert_eq!(sent["context_length_exceeded_behavior"], "error");
        }
    }
    for server in [&chat, &bedrock] {
        for req in server.received_requests().await.unwrap() {
            assert!(req.headers.get("x-api-key").is_none());
            assert!(!String::from_utf8_lossy(&req.body).contains("inbound-only-token"));
            assert!(req.headers.get("anthropic-version").is_none());
        }
    }
    g.server.shutdown().await;
}

#[tokio::test]
async fn discovery_paginates_caches_combines_lists_and_never_guesses() {
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .and(header("authorization", "Bearer chat-only-key"))
        .respond_with(|req: &wiremock::Request| {
            if req.url.query_pairs().any(|(k, _)| k == "after") {
                ResponseTemplate::new(200)
                    .set_body_json(json!({"data":[{"id":"second"}],"has_more":false}))
            } else {
                ResponseTemplate::new(200).set_body_json(
                    json!({"data":[{"id":"first"}],"has_more":true,"last_id":"first"}),
                )
            }
        })
        .expect(2)
        .mount(&a)
        .await;
    Mock::given(method("GET")).and(path("/inference-profiles")).and(header("authorization", "Bearer bedrock-only-key"))
        .respond_with(|req: &wiremock::Request| {
            if req.url.query_pairs().any(|(k,v)| k == "nextToken" && v == "page-2") {
                ResponseTemplate::new(200).set_body_json(json!({"inferenceProfileSummaries":[{"inferenceProfileId":"native-2","inferenceProfileArn":"arn:example:profile/2"}]}))
            } else { ResponseTemplate::new(200).set_body_json(json!({"inferenceProfileSummaries":[{"inferenceProfileId":"native-1"}],"nextToken":"page-2"})) }
        }).expect(2).mount(&b).await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(chat_reply()))
        .expect(2)
        .mount(&a)
        .await;
    Mock::given(method("POST"))
        .and(path("/model/native-2/invoke"))
        .respond_with(ResponseTemplate::new(200).set_body_json(native_reply()))
        .expect(1)
        .mount(&b)
        .await;
    let g = Gateway::new(
        json!([
            {"name":"a","url":a.uri(),"api_key_env":"OPENAI_API_KEY"},
            {"name":"b","url":b.uri(),"protocol":"bedrock","api_key_env":"AWS_BEARER_TOKEN_BEDROCK","catalog_paths":["/inference-profiles"]}
        ]),
        json!([{"pattern":"alias","upstream":"a","model":"first"}]),
    );
    assert_eq!(
        g.client
            .get(g.url("/v1/models"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        g.client
            .post(g.url("/v1/messages/count_tokens"))
            .json(&request("first"))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let listing: Value = g.get("/v1/models").await.json().await.unwrap();
    let ids: Vec<_> = listing["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        [
            "alias",
            "arn:example:profile/2",
            "first",
            "native-1",
            "native-2",
            "second"
        ]
    );
    for model in ["first", "second", "native-2"] {
        assert_eq!(g.post("/v1/messages", &request(model)).await.status(), 200);
    }
    assert_eq!(g.get("/v1/models/native-2").await.status(), 200);
    assert_eq!(g.get("/v1/models/missing").await.status(), 404);
    assert_eq!(
        g.post("/v1/messages", &request("missing")).await.status(),
        404
    );
    let count = g
        .post("/v1/messages/count_tokens", &request("native-2"))
        .await;
    assert_eq!(count.status(), 200);
    assert_eq!(count.headers()["x-omni-dev-token-estimate"], "heuristic");
    assert_eq!(g.server.stats().in_flight, 0);
    assert_eq!(g.server.stats().errors, 2);
    g.server.shutdown().await;
}

#[tokio::test]
async fn native_bedrock_pdf_counting_is_local_and_does_not_apply_translation_restrictions() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/model/native/invoke"))
        .respond_with(ResponseTemplate::new(200).set_body_json(native_reply()))
        .expect(1)
        .mount(&upstream)
        .await;
    let g = Gateway::new(
        json!([{"name":"native","url":upstream.uri(),"protocol":"bedrock"}]),
        json!([{"pattern":"native-alias","upstream":"native","model":"native"}]),
    );
    // A self-contained blank one-page PDF with correctly positioned xref entries.
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (index, object) in [
        "<< /Type /Catalog /Pages 2 0 R >>",
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << >> >>",
    ]
    .iter()
    .enumerate()
    {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", index + 1));
    }
    let xref = pdf.len();
    pdf.push_str("xref\n0 4\n0000000000 65535 f \n");
    for offset in offsets {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size 4 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    ));
    let document = json!({"type":"document","source":{
        "type":"base64","media_type":"application/pdf",
        "data":base64::engine::general_purpose::STANDARD.encode(pdf)
    }});
    let mut body = request("native-alias");
    body["messages"][0]["content"] = json!([document]);
    let response = g.post("/v1/messages/count_tokens", &body).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-omni-dev-token-estimate"], "heuristic");
    let count: Value = response.json().await.unwrap();
    assert!(count["input_tokens"].as_u64().unwrap() > 4);
    assert!(upstream.received_requests().await.unwrap().is_empty());
    assert_eq!(g.post("/v1/messages", &body).await.status(), 200);
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let sent: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(sent["messages"], body["messages"]);
    g.server.shutdown().await;
}

#[tokio::test]
async fn discovery_conflicts_failures_and_inference_errors_do_not_fallback() {
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    for server in [&a, &b] {
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"duplicate"}]})),
            )
            .expect(1)
            .mount(server)
            .await;
    }
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&a)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&b)
        .await;
    let g = Gateway::new(
        json!([{"name":"a","url":a.uri()},{"name":"b","url":b.uri()}]),
        json!([{"pattern":"explicit","upstream":"a","model":"duplicate"}]),
    );
    assert_eq!(
        g.post("/v1/messages", &request("duplicate")).await.status(),
        400
    );
    assert_eq!(g.get("/v1/models").await.status(), 400);
    assert_eq!(
        g.post("/v1/messages", &request("explicit")).await.status(),
        529
    );
    g.server.shutdown().await;

    let failing = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .expect(1)
        .mount(&failing)
        .await;
    let g = Gateway::new(json!([{"name":"bad","url":failing.uri()}]), json!([]));
    for _ in 0..2 {
        assert_eq!(
            g.post("/v1/messages", &request("unknown")).await.status(),
            502
        );
    }
    assert_eq!(g.get("/v1/models").await.status(), 502);
    assert_eq!(failing.received_requests().await.unwrap().len(), 1);
    g.server.shutdown().await;
}

#[tokio::test]
async fn explicit_routes_resolve_conflicts_and_catalog_failures_fail_closed() {
    let a = MockServer::start().await;
    let b = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":[{"id":"owned"}]})))
        .mount(&a)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&b)
        .await;
    let g = Gateway::new(
        json!([{"name":"a","url":a.uri()},{"name":"b","url":b.uri()}]),
        json!([]),
    );
    assert_eq!(
        g.post("/v1/messages", &request("owned")).await.status(),
        502
    );
    assert!(a
        .received_requests()
        .await
        .unwrap()
        .iter()
        .all(|r| r.method == "GET"));
    g.server.shutdown().await;
}

#[tokio::test]
async fn redirects_never_forward_any_credential() {
    let source = MockServer::start().await;
    let target = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(302).insert_header("location", target.uri()))
        .expect(1)
        .mount(&source)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(307).insert_header("location", target.uri()))
        .expect(1)
        .mount(&source)
        .await;
    let g = Gateway::new(
        json!([{"name":"source","url":source.uri(),"api_key_env":"OPENAI_API_KEY"}]),
        json!([{"pattern":"explicit","upstream":"source"}]),
    );
    assert_eq!(g.get("/v1/models").await.status(), 502);
    assert!(!g
        .post("/v1/messages", &request("explicit"))
        .await
        .status()
        .is_success());
    assert!(target.received_requests().await.unwrap().is_empty());
    g.server.shutdown().await;
}

pub(super) fn frame(event: &Value, event_type: &str) -> Vec<u8> {
    let mut headers = Vec::new();
    for (key, value) in [
        (":message-type", "event"),
        (":event-type", event_type),
        (":content-type", "application/json"),
    ] {
        headers.push(key.len() as u8);
        headers.extend_from_slice(key.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let payload = serde_json::to_vec(&json!({"bytes":base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(event).unwrap())})).unwrap();
    let mut out = Vec::new();
    out.extend_from_slice(&((16 + headers.len() + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    out.extend_from_slice(&super::bedrock::crc32(&out).to_be_bytes());
    out.extend(headers);
    out.extend(payload);
    out.extend_from_slice(&super::bedrock::crc32(&out).to_be_bytes());
    out
}

#[tokio::test]
async fn bedrock_binary_stream_becomes_anthropic_sse_with_native_deltas() {
    let upstream = MockServer::start().await;
    let events = vec![
        json!({"type":"message_start","message":{"id":"msg_native","model":"native","content":[]}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reason"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"native-signature"}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tool_1","name":"lookup","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"x\":1}"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":4}}),
        json!({"type":"message_stop"}),
    ];
    let binary: Vec<u8> = events.iter().flat_map(|e| frame(e, "chunk")).collect();
    Mock::given(method("POST"))
        .and(path("/model/native/invoke-with-response-stream"))
        .and(header("accept", "application/vnd.amazon.eventstream"))
        .and(header("x-amzn-bedrock-accept", "application/json"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(binary.clone()),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let g = Gateway::new(
        json!([{"name":"b","url":upstream.uri(),"protocol":"bedrock","catalog_paths":[]}]),
        json!([{"pattern":"alias","upstream":"b","model":"native"}]),
    );
    let mut body = request("alias");
    body["stream"] = json!(true);
    let response = g.post("/v1/messages", &body).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let text = response.text().await.unwrap();
    assert!(text.contains("event: message_start\n"));
    assert!(text.contains("event: message_stop\n"));
    assert!(text.contains("native-signature"));
    assert!(text.contains("input_json_delta"));
    assert!(text.contains("\"model\":\"alias\""));
    assert!(!text.contains("ogw1."));
    // Every byte boundary, not just whole HTTP chunks, is supported.
    let mut decoder = super::bedrock::EventStream::default();
    let mut decoded = Vec::new();
    for byte in &binary {
        decoded.extend(decoder.push(&[*byte]).unwrap());
    }
    decoder.finish().unwrap();
    assert_eq!(decoded, events);
    g.server.shutdown().await;
}

#[tokio::test]
async fn binary_stream_errors_crc_and_truncation_are_not_successful_completion() {
    for (name, mut binary) in [
        (
            "truncated",
            frame(&json!({"type":"message_start"}), "chunk"),
        ),
        (
            "exception",
            frame(&json!({"message":"overloaded"}), "throttlingException"),
        ),
        ("crc", frame(&json!({"type":"message_stop"}), "chunk")),
        ("missing-stop", frame(&json!({"type":"ping"}), "chunk")),
        (
            "post-stop-event",
            [
                frame(&json!({"type":"message_stop"}), "chunk"),
                frame(&json!({"type":"ping"}), "chunk"),
            ]
            .concat(),
        ),
        (
            "post-stop-truncated",
            [
                frame(&json!({"type":"message_stop"}), "chunk"),
                vec![0, 0, 0],
            ]
            .concat(),
        ),
    ] {
        if name == "truncated" {
            binary.pop();
        }
        if name == "crc" {
            binary[15] ^= 1;
        }
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/vnd.amazon.eventstream")
                    .set_body_bytes(binary),
            )
            .mount(&upstream)
            .await;
        let g = Gateway::new(
            json!([{"name":"b","url":upstream.uri(),"protocol":"bedrock"}]),
            json!([{"pattern":"*","upstream":"b"}]),
        );
        let mut body = request("native");
        body["stream"] = json!(true);
        let text = g.post("/v1/messages", &body).await.text().await.unwrap();
        assert!(text.contains("event: error"), "{name}: {text}");
        assert!(!text.contains("event: message_stop"));
        assert_eq!(g.server.stats().errors, 1);
        g.server.shutdown().await;
    }
}

#[tokio::test]
async fn catalog_pagination_loop_is_bounded() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(query_param("nextToken", "repeat"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"modelSummaries":[],"nextToken":"repeat"})),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/foundation-models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"modelSummaries":[{"modelId":"model"}],"nextToken":"repeat"}),
            ),
        )
        .with_priority(10)
        .expect(1)
        .mount(&upstream)
        .await;
    let g = Gateway::new(
        json!([{"name":"b","url":upstream.uri(),"protocol":"bedrock","catalog_paths":["/foundation-models"]}]),
        json!([]),
    );
    assert_eq!(g.get("/v1/models").await.status(), 502);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    g.server.shutdown().await;
}
