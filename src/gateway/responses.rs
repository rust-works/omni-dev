//! Anthropic Messages ↔ OpenAI Responses API (`/v1/responses`).
//!
//! The Responses API is OpenAI's newer surface; some models only call tools
//! through it. The gateway always sends `store: false`, so it keeps no
//! upstream state: prior reasoning is replayed from the `encrypted_content`
//! the gateway carried inside its own thinking signature (see
//! [`signature`](super::signature)), requested with
//! `include: ["reasoning.encrypted_content"]`.

use serde_json::{json, Map, Value};

use super::anthropic::{MessagesRequest, ThinkingConfig, ToolChoice};
use super::chat::{plan_reasoning, required_max_tokens, BuildContext};
use super::completion::{Completion, Finish, Reasoning, UpstreamToolCall, UpstreamUsage};
use super::error::{upstream_message, GatewayError};
use super::flavor::ReasoningStyle;
use super::lower::{prompt_cache_key, Lowered, ReplayedReasoning, Turn, UserPart};

/// The signature payload of a Responses reasoning item: what the next turn
/// needs to send it back.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ReasoningPayload {
    /// The Responses reasoning item id (empty for the Chat target).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// The encrypted reasoning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encrypted_content: Option<String>,
    /// Reasoning text hidden from the client by `display: omitted`, carried
    /// so the next turn can still replay it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// The payload to sign for a thinking block. With `display: omitted` the
/// client never sees the reasoning, so the text travels inside the signed
/// payload instead and the next turn can still replay it — which Fireworks
/// needs for multi-turn tool use.
pub(crate) fn signing_payload(payload: &str, text: &str, omitted: bool) -> String {
    if !omitted || text.is_empty() {
        return payload.to_string();
    }
    let mut decoded = if payload.is_empty() {
        ReasoningPayload::default()
    } else {
        match ReasoningPayload::decode(payload) {
            Some(decoded) => decoded,
            None => return payload.to_string(),
        }
    };
    decoded.text = Some(text.to_string());
    decoded.encode()
}

/// The reasoning text a verified block replays: what the client saw, else
/// the text an omitted block carried in its payload.
pub(crate) fn replay_text(visible: &str, payload: &str) -> String {
    if !visible.is_empty() {
        return visible.to_string();
    }
    ReasoningPayload::decode(payload)
        .and_then(|p| p.text)
        .unwrap_or_default()
}

impl ReasoningPayload {
    /// The payload as signature text.
    pub(crate) fn encode(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Reads a payload; `None` for one the Chat target produced (empty).
    pub(crate) fn decode(raw: &str) -> Option<Self> {
        serde_json::from_str(raw).ok()
    }
}

/// Builds the `/v1/responses` body.
pub(crate) fn build_request(
    request: &MessagesRequest,
    lowered: &Lowered,
    ctx: &mut BuildContext<'_>,
) -> Result<Value, GatewayError> {
    let max_tokens = required_max_tokens(request)?;
    if !request.stop_sequences.is_empty() {
        return Err(GatewayError::invalid(
            "stop_sequences: the Responses API has no stop sequences; use the chat upstream protocol",
        ));
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(ctx.model));
    body.insert("input".into(), Value::Array(input(lowered, ctx)));
    if let Some(system) = lowered.system.as_deref().filter(|s| !s.is_empty()) {
        body.insert("instructions".into(), json!(system));
    }
    body.insert("max_output_tokens".into(), json!(max_tokens));
    body.insert("stream".into(), json!(request.stream));
    body.insert("store".into(), json!(false));
    for (name, value) in [
        ("temperature", request.temperature),
        ("top_p", request.top_p),
    ] {
        if let Some(v) = value {
            if ctx.config.flavor.accepts_sampling(ctx.model) {
                body.insert(name.into(), json!(v));
            } else {
                ctx.report.ignore(name);
            }
        }
    }
    if request.top_k.is_some() {
        ctx.report.ignore("top_k");
    }
    tools(request, lowered, &mut body);
    let plan = plan_reasoning(
        request,
        ReasoningStyle::Effort,
        &ctx.config.effort_levels,
        ctx.report,
    );
    let wants_thinking = request
        .thinking
        .as_ref()
        .is_some_and(ThinkingConfig::wants_thinking);
    if plan.effort.is_some() || wants_thinking {
        let mut reasoning = Map::new();
        if let Some(effort) = plan.effort {
            reasoning.insert("effort".into(), json!(effort.as_str()));
        }
        if wants_thinking {
            reasoning.insert("summary".into(), json!("auto"));
        }
        body.insert("reasoning".into(), Value::Object(reasoning));
        body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    }
    if let Some(format) = request
        .output_config
        .as_ref()
        .and_then(|o| o.format.as_ref())
    {
        body.insert(
            "text".into(),
            json!({"format": {"type": "json_schema", "name": "response",
                              "schema": format.schema, "strict": ctx.config.flavor.strict_json_schema()}}),
        );
    }
    if ctx.report.cache_hint {
        body.insert(
            "prompt_cache_key".into(),
            json!(prompt_cache_key(ctx.model, lowered)),
        );
    }
    Ok(Value::Object(body))
}

fn tools(request: &MessagesRequest, lowered: &Lowered, body: &mut Map<String, Value>) {
    if lowered.tools.is_empty() {
        return;
    }
    let defs: Vec<Value> = lowered
        .tools
        .iter()
        .map(|tool| {
            let mut def = json!({"type": "function", "name": tool.name, "parameters": tool.parameters,
                                 "strict": tool.strict.unwrap_or(false)});
            if let Some(description) = &tool.description {
                def["description"] = json!(description);
            }
            def
        })
        .collect();
    body.insert("tools".into(), Value::Array(defs));
    if let Some(choice) = &request.tool_choice {
        let mut names = lowered.names.clone();
        body.insert(
            "tool_choice".into(),
            match choice {
                ToolChoice::Auto { .. } => json!("auto"),
                ToolChoice::Any { .. } => json!("required"),
                ToolChoice::None {} => json!("none"),
                ToolChoice::Tool { name, .. } => {
                    json!({"type": "function", "name": names.wire(name)})
                }
            },
        );
        if choice.disables_parallel() {
            body.insert("parallel_tool_calls".into(), json!(false));
        }
    }
}

fn input(lowered: &Lowered, ctx: &mut BuildContext<'_>) -> Vec<Value> {
    let mut out = Vec::new();
    let mut pending_images: Vec<String> = Vec::new();
    for turn in &lowered.turns {
        if !matches!(turn, Turn::ToolResult(_)) {
            flush_images(&mut out, &mut pending_images);
        }
        match turn {
            Turn::System(text) => out.push(json!({"role": "system", "content": text})),
            Turn::User(parts) => out.push(json!({"role": "user", "content": user_parts(parts)})),
            Turn::ToolResult(result) => {
                out.push(json!({"type": "function_call_output", "call_id": result.call_id, "output": result.text}));
                pending_images.extend(result.images.iter().cloned());
            }
            Turn::Assistant(assistant) => {
                for reasoning in &assistant.reasoning {
                    match reasoning_item(reasoning) {
                        Some(item) => out.push(item),
                        None => ctx
                            .report
                            .ignore("thinking (no Responses reasoning to replay)"),
                    }
                }
                if !assistant.text.is_empty() {
                    out.push(json!({"role": "assistant", "content": assistant.text}));
                }
                for call in &assistant.tool_calls {
                    out.push(json!({"type": "function_call", "call_id": call.id,
                                    "name": call.name, "arguments": call.arguments}));
                }
            }
        }
    }
    flush_images(&mut out, &mut pending_images);
    out
}

/// A replayable `reasoning` input item, or `None` when the thinking block was
/// signed by the Chat target and carries no Responses item.
fn reasoning_item(reasoning: &ReplayedReasoning) -> Option<Value> {
    let payload = ReasoningPayload::decode(&reasoning.payload).filter(|p| !p.id.is_empty())?;
    let summary: Vec<Value> = if reasoning.text.is_empty() {
        Vec::new()
    } else {
        vec![json!({"type": "summary_text", "text": reasoning.text})]
    };
    let mut item = json!({"type": "reasoning", "id": payload.id, "summary": summary});
    if let Some(encrypted) = payload.encrypted_content {
        item["encrypted_content"] = json!(encrypted);
    }
    Some(item)
}

fn flush_images(out: &mut Vec<Value>, images: &mut Vec<String>) {
    if images.is_empty() {
        return;
    }
    let mut parts = vec![UserPart::Text(
        "Images returned by the tool calls above:".into(),
    )];
    parts.extend(images.drain(..).map(UserPart::Image));
    out.push(json!({"role": "user", "content": user_parts(&parts)}));
}

fn user_parts(parts: &[UserPart]) -> Value {
    Value::Array(
        parts
            .iter()
            .map(|p| match p {
                UserPart::Text(text) => json!({"type": "input_text", "text": text}),
                UserPart::Image(url) => json!({"type": "input_image", "image_url": url}),
            })
            .collect(),
    )
}

/// The text of a reasoning item: its summaries, else its raw reasoning text.
pub(crate) fn reasoning_item_text(item: &Value) -> String {
    let collect = |field: &str| -> Vec<String> {
        item.get(field)
            .and_then(Value::as_array)
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let summary = collect("summary");
    let parts = if summary.is_empty() {
        collect("content")
    } else {
        summary
    };
    parts.join("\n\n")
}

/// The finish for a response's `status` and `incomplete_details`.
pub(crate) fn finish_for(response: &Value, has_tool_calls: bool) -> Finish {
    match response
        .pointer("/incomplete_details/reason")
        .and_then(Value::as_str)
    {
        Some("max_output_tokens") => Finish::Length,
        Some("content_filter") => Finish::ContentFilter,
        _ if has_tool_calls => Finish::ToolCalls,
        _ => Finish::Stop,
    }
}

/// Reads a non-streaming `/v1/responses` reply.
pub(crate) fn parse_response(body: &Value) -> Result<Completion, GatewayError> {
    if body.get("status").and_then(Value::as_str) == Some("failed") {
        let message = body
            .get("error")
            .map_or_else(String::new, |e| upstream_message(&e.to_string()));
        return Err(GatewayError::bad_gateway(format!(
            "upstream response failed: {message}"
        )));
    }
    let items = body
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| GatewayError::bad_gateway("upstream reply has no output"))?;
    let mut completion = Completion::default();
    let mut texts = Vec::new();
    for item in items {
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => completion.reasoning.push(Reasoning {
                text: reasoning_item_text(item),
                payload: ReasoningPayload {
                    id: item
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    encrypted_content: item
                        .get("encrypted_content")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    text: None,
                }
                .encode(),
            }),
            Some("message") => {
                for part in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    match part.get("type").and_then(Value::as_str) {
                        Some("output_text") => {
                            texts.push(
                                part.get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                            );
                        }
                        Some("refusal") => {
                            completion.refusal = part
                                .get("refusal")
                                .and_then(Value::as_str)
                                .map(str::to_string);
                        }
                        _ => {}
                    }
                }
            }
            Some("function_call") => completion.tool_calls.push(UpstreamToolCall {
                id: item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                name: item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                arguments: item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }),
            other => {
                tracing::debug!(item_type = ?other, "unhandled Responses output item; dropped");
            }
        }
    }
    completion.text = texts.concat();
    completion.finish = finish_for(body, !completion.tool_calls.is_empty());
    completion.usage = body
        .get("usage")
        .map(UpstreamUsage::from_responses)
        .unwrap_or_default();
    Ok(completion)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::gateway::config::{ConfigOverrides, GatewayConfig};
    use crate::gateway::lower::lower;
    use crate::gateway::policy::{check_request, PolicyReport, UnknownFields};
    use crate::gateway::signature::Signer;
    use crate::test_support::env::MapEnv;

    fn build(
        flavor: &str,
        model: &str,
        body: Value,
    ) -> (Result<Value, GatewayError>, PolicyReport) {
        let env = MapEnv::new()
            .with("OMNI_DEV_GATEWAY_UPSTREAM_URL", "http://up")
            .with("OMNI_DEV_GATEWAY_FLAVOR", flavor);
        let config = GatewayConfig::from_env_with(&env, &ConfigOverrides::default()).unwrap();
        let mut report = check_request(body.as_object().unwrap(), UnknownFields::Reject).unwrap();
        let request: MessagesRequest = serde_json::from_value(body).unwrap();
        let lowered = lower(&request, &Signer::from_token("t"), true, &mut report).unwrap();
        let mut ctx = BuildContext {
            config: &config,
            model,
            report: &mut report,
        };
        let result = build_request(&request, &lowered, &mut ctx);
        (result, report)
    }

    #[test]
    fn a_full_request_translates() {
        let signer = Signer::from_token("t");
        let payload = ReasoningPayload {
            id: "rs_1".into(),
            encrypted_content: Some("ENC".into()),
            text: None,
        }
        .encode();
        let sig = signer.sign("prior", &payload);
        let chat_sig = signer.sign("chat", "");
        let body = json!({
            "model": "claude-x", "max_tokens": 50, "system": "sys", "stream": true,
            "temperature": 0.2, "top_k": 3,
            "cache_control": {"type": "ephemeral"},
            "thinking": {"type": "enabled", "budget_tokens": 2000},
            "output_config": {"format": {"type": "json_schema", "schema": {"type": "object"}}},
            "tools": [{"name": "ls", "description": "list", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": "ls", "disable_parallel_tool_use": true},
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "q"}, {"type": "image", "source": {"type": "url", "url": "https://i"}}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "prior", "signature": sig},
                    {"type": "thinking", "thinking": "chat", "signature": chat_sig},
                    {"type": "text", "text": "calling"},
                    {"type": "tool_use", "id": "call_1", "name": "ls", "input": {"p": "."}}
                ]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": [
                    {"type": "text", "text": "a"}, {"type": "image", "source": {"type": "url", "url": "https://j"}}
                ]}]},
                {"role": "system", "content": "brief"}
            ]
        });
        let (result, report) = build("openai", "gpt-4o", body);
        let body = result.unwrap();
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["max_output_tokens"], 50);
        assert_eq!(body["store"], false);
        assert_eq!(body["temperature"], 0.2);
        assert!(report.ignored.contains("top_k"));
        assert!(report
            .ignored
            .contains("thinking (no Responses reasoning to replay)"));
        assert_eq!(
            body["reasoning"],
            json!({"effort": "low", "summary": "auto"})
        );
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["text"]["format"]["strict"], true);
        assert!(body["prompt_cache_key"].is_string());
        assert_eq!(
            body["tool_choice"],
            json!({"type": "function", "name": "ls"})
        );
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["tools"][0]["description"], "list");
        let input = body["input"].as_array().unwrap();
        assert_eq!(
            input[0]["content"][1],
            json!({"type": "input_image", "image_url": "https://i"})
        );
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["id"], "rs_1");
        assert_eq!(input[1]["encrypted_content"], "ENC");
        assert_eq!(input[1]["summary"][0]["text"], "prior");
        assert_eq!(input[2], json!({"role": "assistant", "content": "calling"}));
        assert_eq!(input[3]["type"], "function_call");
        assert_eq!(input[3]["arguments"], r#"{"p":"."}"#);
        assert_eq!(
            input[4],
            json!({"type": "function_call_output", "call_id": "call_1", "output": "a"})
        );
        assert_eq!(input[5]["content"][1]["image_url"], "https://j");
        assert_eq!(input[6], json!({"role": "system", "content": "brief"}));
    }

    #[test]
    fn stop_sequences_are_rejected_and_gpt5_drops_sampling() {
        let body = json!({"model": "c", "max_tokens": 5, "stop_sequences": ["x"], "messages": [{"role": "user", "content": "q"}]});
        assert!(build("openai", "gpt-5", body)
            .0
            .unwrap_err()
            .message
            .contains("stop_sequences"));
        let body = json!({"model": "c", "max_tokens": 5, "top_p": 0.5, "messages": [{"role": "user", "content": "q"}],
                          "tools": [{"name": "a", "input_schema": {}}], "tool_choice": {"type": "any"}});
        let (result, report) = build("openai", "gpt-5", body);
        let result = result.unwrap();
        assert!(report.ignored.contains("top_p"));
        assert_eq!(result["tool_choice"], "required");
        assert!(result.get("reasoning").is_none());
        for (choice, expected) in [
            (json!({"type": "auto"}), "auto"),
            (json!({"type": "none"}), "none"),
        ] {
            let body = json!({"model": "c", "max_tokens": 5, "messages": [{"role": "user", "content": "q"}],
                              "tools": [{"name": "a", "input_schema": {}}], "tool_choice": choice});
            assert_eq!(
                build("openai", "gpt-5", body).0.unwrap()["tool_choice"],
                expected
            );
        }
    }

    #[test]
    fn a_reply_parses_reasoning_text_tools_and_usage() {
        let body = json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "id": "rs_9", "encrypted_content": "E", "summary": [{"type": "summary_text", "text": "s1"}, {"type": "summary_text", "text": "s2"}]},
                {"type": "reasoning", "id": "rs_10", "summary": [], "content": [{"type": "reasoning_text", "text": "raw"}]},
                {"type": "message", "content": [{"type": "output_text", "text": "hi "}, {"type": "output_text", "text": "there"}]},
                {"type": "function_call", "call_id": "c1", "name": "ls", "arguments": "{}"},
                {"type": "web_search_call"}
            ],
            "usage": {"input_tokens": 9, "output_tokens": 4, "output_tokens_details": {"reasoning_tokens": 2}}
        });
        let completion = parse_response(&body).unwrap();
        assert_eq!(completion.reasoning[0].text, "s1\n\ns2");
        let payload = ReasoningPayload::decode(&completion.reasoning[0].payload).unwrap();
        assert_eq!(
            payload,
            ReasoningPayload {
                id: "rs_9".into(),
                encrypted_content: Some("E".into()),
                text: None
            }
        );
        assert_eq!(completion.reasoning[1].text, "raw");
        assert_eq!(completion.text, "hi there");
        assert_eq!(completion.tool_calls[0].id.as_deref(), Some("c1"));
        assert_eq!(completion.finish, Finish::ToolCalls);
        assert_eq!(completion.usage.reasoning_tokens, Some(2));
    }

    #[test]
    fn incomplete_refused_and_failed_replies() {
        let incomplete = json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}, "output": []});
        assert_eq!(parse_response(&incomplete).unwrap().finish, Finish::Length);
        let filtered = json!({"status": "incomplete", "incomplete_details": {"reason": "content_filter"}, "output": [
            {"type": "message", "content": [{"type": "refusal", "refusal": "no"}]}
        ]});
        let completion = parse_response(&filtered).unwrap();
        assert_eq!(completion.finish, Finish::ContentFilter);
        assert_eq!(completion.refusal.as_deref(), Some("no"));
        let failed = json!({"status": "failed", "error": {"message": "boom"}});
        assert!(parse_response(&failed)
            .unwrap_err()
            .message
            .contains("boom"));
        assert!(parse_response(&json!({})).is_err());
        assert!(ReasoningPayload::decode("").is_none());
    }

    #[test]
    fn omitted_reasoning_travels_in_the_payload() {
        assert_eq!(signing_payload("", "r", false), "");
        assert_eq!(signing_payload("", "", true), "");
        assert_eq!(signing_payload("opaque", "r", true), "opaque");
        let chat = signing_payload("", "hidden", true);
        assert_eq!(chat, r#"{"text":"hidden"}"#);
        assert_eq!(replay_text("", &chat), "hidden");
        assert_eq!(replay_text("seen", &chat), "seen");
        assert_eq!(replay_text("", ""), "");
        let responses = ReasoningPayload {
            id: "rs".into(),
            encrypted_content: Some("E".into()),
            text: None,
        }
        .encode();
        let both = ReasoningPayload::decode(&signing_payload(&responses, "hidden", true)).unwrap();
        assert_eq!(
            (both.id.as_str(), both.text.as_deref()),
            ("rs", Some("hidden"))
        );
        // A chat-produced payload carries no Responses item to replay.
        let replayed = ReplayedReasoning {
            text: "hidden".into(),
            payload: chat,
        };
        assert!(reasoning_item(&replayed).is_none());
    }
}
