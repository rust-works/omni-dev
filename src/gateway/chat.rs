//! Anthropic Messages ↔ OpenAI Chat Completions.
//!
//! [`build_request`] turns a lowered Anthropic request into a
//! `/v1/chat/completions` body; [`parse_response`] reads a non-streaming reply
//! into a [`Completion`]. Streaming replies go through
//! [`chat_stream`](super::chat_stream).

use serde_json::{json, Map, Value};

use super::anthropic::{Effort, MessagesRequest, ThinkingConfig, ToolChoice};
use super::completion::{Completion, Finish, Reasoning, UpstreamToolCall, UpstreamUsage};
use super::config::GatewayConfig;
use super::error::GatewayError;
use super::flavor::{clamp_effort, effort_for_budget, ReasoningStyle};
use super::lower::{prompt_cache_key, Lowered, Turn, UserPart};
use super::policy::PolicyReport;
use super::think_tags::ThinkTagSplitter;
use crate::claude::ai::openai::ResponseFormatField;

/// What the request builders need besides the request itself.
pub(crate) struct BuildContext<'a> {
    /// The gateway configuration.
    pub config: &'a GatewayConfig,
    /// The upstream model.
    pub model: &'a str,
    /// Where ignored fields are recorded.
    pub report: &'a mut PolicyReport,
}

/// The upstream reasoning controls for a request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct ReasoningPlan {
    /// An Anthropic-style `thinking` object (Fireworks only).
    pub thinking: Option<Value>,
    /// A reasoning effort.
    pub effort: Option<Effort>,
}

/// Decides the reasoning controls. Fireworks takes `thinking` *or*
/// `reasoning_effort`, never both; everyone else takes an effort, bucketed from
/// the thinking budget when the client gave no explicit effort.
pub(crate) fn plan_reasoning(
    request: &MessagesRequest,
    style: ReasoningStyle,
    levels: &[Effort],
    report: &mut PolicyReport,
) -> ReasoningPlan {
    let explicit = request.output_config.as_ref().and_then(|o| o.effort);
    let clamp = |e: Effort| clamp_effort(e, levels);
    match (&request.thinking, style) {
        (Some(ThinkingConfig::Enabled { budget_tokens, .. }), ReasoningStyle::ThinkingOrEffort) => {
            if explicit.is_some() {
                report.ignore("output_config.effort (Fireworks takes one reasoning control)");
            }
            ReasoningPlan {
                thinking: Some(json!({"type": "enabled", "budget_tokens": budget_tokens})),
                effort: None,
            }
        }
        (Some(ThinkingConfig::Enabled { budget_tokens, .. }), ReasoningStyle::Effort) => {
            ReasoningPlan {
                thinking: None,
                effort: clamp(explicit.unwrap_or_else(|| effort_for_budget(*budget_tokens))),
            }
        }
        (Some(ThinkingConfig::Disabled {}), ReasoningStyle::ThinkingOrEffort) => ReasoningPlan {
            thinking: Some(json!({"type": "disabled"})),
            effort: None,
        },
        (Some(ThinkingConfig::Disabled {}), ReasoningStyle::Effort) => {
            if explicit.is_some() {
                report.ignore("output_config.effort (thinking is disabled)");
            }
            ReasoningPlan::default()
        }
        _ => ReasoningPlan {
            thinking: None,
            effort: explicit.and_then(clamp),
        },
    }
}

/// Validates `max_tokens` for `/v1/messages`.
pub(crate) fn required_max_tokens(request: &MessagesRequest) -> Result<u64, GatewayError> {
    match request.max_tokens {
        None => Err(GatewayError::invalid("max_tokens: field required")),
        Some(0) => Err(GatewayError::invalid(
            "max_tokens: 0 (prompt-cache pre-warming) is not supported by the gateway",
        )),
        Some(n) => Ok(n),
    }
}

/// Builds the `/v1/chat/completions` body.
pub(crate) fn build_request(
    request: &MessagesRequest,
    lowered: &Lowered,
    ctx: &mut BuildContext<'_>,
) -> Result<Value, GatewayError> {
    let flavor = ctx.config.flavor;
    let max_tokens = required_max_tokens(request)?;
    let mut body = Map::new();
    body.insert("model".into(), json!(ctx.model));
    body.insert(
        "messages".into(),
        Value::Array(messages(lowered, flavor.replays_reasoning())),
    );
    body.insert(flavor.max_tokens_field(ctx.model).into(), json!(max_tokens));
    body.insert("stream".into(), json!(request.stream));
    if request.stream && flavor.requests_stream_usage() {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }
    if !request.stop_sequences.is_empty() {
        if let Some(limit) = flavor
            .stop_limit()
            .filter(|l| request.stop_sequences.len() > *l)
        {
            return Err(GatewayError::invalid(format!(
                "stop_sequences: the {flavor} upstream accepts at most {limit} stop sequences, got {}",
                request.stop_sequences.len()
            )));
        }
        body.insert("stop".into(), json!(request.stop_sequences));
    }
    sampling(request, ctx, &mut body);
    tools(request, lowered, &mut body);
    let plan = plan_reasoning(
        request,
        flavor.reasoning_style(),
        &ctx.config.effort_levels,
        ctx.report,
    );
    if let Some(thinking) = plan.thinking {
        body.insert("thinking".into(), thinking);
    }
    if let Some(effort) = plan.effort {
        body.insert("reasoning_effort".into(), json!(effort.as_str()));
    }
    if let Some(format) = request
        .output_config
        .as_ref()
        .and_then(|o| o.format.as_ref())
    {
        let field =
            ResponseFormatField::json_schema(format.schema.clone(), flavor.strict_json_schema());
        body.insert(
            "response_format".into(),
            serde_json::to_value(field).map_err(|e| GatewayError::internal(e.to_string()))?,
        );
    }
    if ctx.report.cache_hint {
        if flavor.takes_prompt_cache_key() {
            body.insert(
                "prompt_cache_key".into(),
                json!(prompt_cache_key(ctx.model, lowered)),
            );
        } else {
            ctx.report.ignore("cache_control");
        }
    }
    if flavor.sets_context_overflow_error() {
        body.insert("context_length_exceeded_behavior".into(), json!("error"));
    }
    Ok(Value::Object(body))
}

fn sampling(request: &MessagesRequest, ctx: &mut BuildContext<'_>, body: &mut Map<String, Value>) {
    let flavor = ctx.config.flavor;
    for (name, value) in [
        ("temperature", request.temperature),
        ("top_p", request.top_p),
    ] {
        if let Some(v) = value {
            if flavor.accepts_sampling(ctx.model) {
                body.insert(name.into(), json!(v));
            } else {
                ctx.report.ignore(name);
            }
        }
    }
    if let Some(k) = request.top_k {
        if flavor.accepts_top_k() {
            body.insert("top_k".into(), json!(k));
        } else {
            ctx.report.ignore("top_k");
        }
    }
}

fn tools(request: &MessagesRequest, lowered: &Lowered, body: &mut Map<String, Value>) {
    if lowered.tools.is_empty() {
        return;
    }
    let defs: Vec<Value> = lowered
        .tools
        .iter()
        .map(|tool| {
            let mut function = json!({"name": tool.name, "parameters": tool.parameters});
            if let Some(description) = &tool.description {
                function["description"] = json!(description);
            }
            if let Some(strict) = tool.strict {
                function["strict"] = json!(strict);
            }
            json!({"type": "function", "function": function})
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
                    json!({"type": "function", "function": {"name": names.wire(name)}})
                }
            },
        );
        if choice.disables_parallel() {
            body.insert("parallel_tool_calls".into(), json!(false));
        }
    }
}

/// The `messages` array. Images a tool returned follow the tool results in a
/// user message, since most servers take only text in `role: tool`.
fn messages(lowered: &Lowered, replay_reasoning: bool) -> Vec<Value> {
    let mut out = Vec::new();
    if let Some(system) = lowered.system.as_deref().filter(|s| !s.is_empty()) {
        out.push(json!({"role": "system", "content": system}));
    }
    let mut pending_images: Vec<String> = Vec::new();
    for turn in &lowered.turns {
        if !matches!(turn, Turn::ToolResult(_)) {
            flush_tool_images(&mut out, &mut pending_images);
        }
        match turn {
            Turn::System(text) => out.push(json!({"role": "system", "content": text})),
            Turn::User(parts) => out.push(json!({"role": "user", "content": user_content(parts)})),
            Turn::ToolResult(result) => {
                out.push(
                    json!({"role": "tool", "tool_call_id": result.call_id, "content": result.text}),
                );
                pending_images.extend(result.images.iter().cloned());
            }
            Turn::Assistant(assistant) => {
                let mut message = json!({"role": "assistant"});
                if assistant.tool_calls.is_empty() {
                    message["content"] = json!(assistant.text);
                } else {
                    message["content"] = if assistant.text.is_empty() {
                        Value::Null
                    } else {
                        json!(assistant.text)
                    };
                    message["tool_calls"] = assistant
                        .tool_calls
                        .iter()
                        .map(|c| {
                            json!({"id": c.id, "type": "function",
                                   "function": {"name": c.name, "arguments": c.arguments}})
                        })
                        .collect();
                }
                if replay_reasoning && !assistant.reasoning.is_empty() {
                    let text: Vec<&str> = assistant
                        .reasoning
                        .iter()
                        .map(|r| r.text.as_str())
                        .collect();
                    message["reasoning_content"] = json!(text.join("\n\n"));
                }
                out.push(message);
            }
        }
    }
    flush_tool_images(&mut out, &mut pending_images);
    out
}

fn flush_tool_images(out: &mut Vec<Value>, images: &mut Vec<String>) {
    if images.is_empty() {
        return;
    }
    let mut parts = vec![UserPart::Text(
        "Images returned by the tool calls above:".into(),
    )];
    parts.extend(images.drain(..).map(UserPart::Image));
    out.push(json!({"role": "user", "content": user_content(&parts)}));
}

/// A user message's `content`: a string when it is text only, else parts.
fn user_content(parts: &[UserPart]) -> Value {
    if parts.iter().all(|p| matches!(p, UserPart::Text(_))) {
        let texts = parts.iter().filter_map(|p| match p {
            UserPart::Text(t) => Some(t.as_str()),
            UserPart::Image(_) => None,
        });
        return json!(super::anthropic::join_text(texts));
    }
    Value::Array(
        parts
            .iter()
            .map(|p| match p {
                UserPart::Text(text) => json!({"type": "text", "text": text}),
                UserPart::Image(url) => json!({"type": "image_url", "image_url": {"url": url}}),
            })
            .collect(),
    )
}

/// The reasoning text of a message or delta: `reasoning_content` (DeepSeek,
/// Fireworks, vLLM), or `reasoning` (OpenRouter, Ollama).
pub(crate) fn reasoning_text(message: &Value) -> Option<&str> {
    message
        .get("reasoning_content")
        .or_else(|| message.get("reasoning"))
        .and_then(Value::as_str)
}

/// Reads a non-streaming `/v1/chat/completions` reply.
pub(crate) fn parse_response(body: &Value, think_tags: bool) -> Result<Completion, GatewayError> {
    let choice = body
        .pointer("/choices/0")
        .ok_or_else(|| GatewayError::bad_gateway("upstream reply has no choices"))?;
    let message = choice.get("message").unwrap_or(&Value::Null);
    let mut reasoning = reasoning_text(message).unwrap_or_default().to_string();
    let mut text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if think_tags {
        let (inline, rest) = ThinkTagSplitter::split(&text);
        if !inline.is_empty() {
            if !reasoning.is_empty() {
                reasoning.push_str("\n\n");
            }
            reasoning.push_str(&inline);
            text = rest;
        }
    }
    let tool_calls = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .map(|c| UpstreamToolCall {
                    id: c.get("id").and_then(Value::as_str).map(str::to_string),
                    name: c
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    arguments: c
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Completion {
        reasoning: if reasoning.is_empty() {
            Vec::new()
        } else {
            vec![Reasoning {
                text: reasoning,
                payload: String::new(),
            }]
        },
        text,
        tool_calls,
        finish: choice
            .get("finish_reason")
            .and_then(Value::as_str)
            .map(Finish::from_chat)
            .unwrap_or_default(),
        refusal: message
            .get("refusal")
            .and_then(Value::as_str)
            .filter(|r| !r.is_empty())
            .map(str::to_string),
        usage: body
            .get("usage")
            .map(UpstreamUsage::from_chat)
            .unwrap_or_default(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::gateway::config::ConfigOverrides;
    use crate::gateway::flavor::Flavor;
    use crate::gateway::lower::lower;
    use crate::gateway::signature::Signer;
    use crate::test_support::env::MapEnv;

    fn config(flavor: &str) -> GatewayConfig {
        let env = MapEnv::new()
            .with("OMNI_DEV_GATEWAY_UPSTREAM_URL", "http://up")
            .with("OMNI_DEV_GATEWAY_FLAVOR", flavor);
        GatewayConfig::from_env_with(&env, &ConfigOverrides::default()).unwrap()
    }

    fn build(
        flavor: &str,
        model: &str,
        body: Value,
    ) -> (Result<Value, GatewayError>, PolicyReport) {
        let config = config(flavor);
        let mut report = crate::gateway::policy::check_request(
            body.as_object().unwrap(),
            crate::gateway::policy::UnknownFields::Reject,
        )
        .unwrap();
        let request: MessagesRequest = serde_json::from_value(body).unwrap();
        let signer = Signer::from_token("t");
        let lowered = lower(
            &request,
            &signer,
            config.flavor.replays_reasoning(),
            &mut report,
        )
        .unwrap();
        let mut ctx = BuildContext {
            config: &config,
            model,
            report: &mut report,
        };
        let result = build_request(&request, &lowered, &mut ctx);
        (result, report)
    }

    fn base() -> Value {
        json!({"model": "claude-x", "max_tokens": 100, "messages": [{"role": "user", "content": "hi"}]})
    }

    #[test]
    fn a_minimal_request_has_the_basic_shape() {
        let (body, report) = build("generic", "llama3", base());
        let body = body.unwrap();
        assert_eq!(body["model"], "llama3");
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"], json!([{"role": "user", "content": "hi"}]));
        assert!(body.get("stream_options").is_none());
        assert!(report.ignored.is_empty());
    }

    #[test]
    fn openai_uses_max_completion_tokens_and_drops_sampling_for_gpt5() {
        let mut req = base();
        req["temperature"] = json!(0.5);
        req["top_p"] = json!(0.9);
        req["top_k"] = json!(5);
        req["stream"] = json!(true);
        let (body, report) = build("openai", "gpt-5", req);
        let body = body.unwrap();
        assert_eq!(body["max_completion_tokens"], 100);
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_k").is_none());
        assert_eq!(body["stream_options"]["include_usage"], true);
        for name in ["temperature", "top_p", "top_k"] {
            assert!(report.ignored.contains(name), "{name}");
        }
    }

    #[test]
    fn fireworks_keeps_sampling_and_errors_on_overflow() {
        let mut req = base();
        req["temperature"] = json!(0.5);
        req["top_k"] = json!(5);
        req["stream"] = json!(true);
        let (body, _) = build("fireworks", "accounts/fireworks/models/x", req);
        let body = body.unwrap();
        assert_eq!(body["max_tokens"], 100);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["top_k"], 5);
        assert_eq!(body["context_length_exceeded_behavior"], "error");
        assert!(body.get("stream_options").is_none());
    }

    #[test]
    fn stop_sequences_are_capped_per_flavor() {
        let mut req = base();
        req["stop_sequences"] = json!(["a", "b", "c", "d", "e"]);
        let (result, _) = build("fireworks", "m", req.clone());
        assert!(result.unwrap_err().message.contains("at most 4"));
        let (body, _) = build("generic", "m", req);
        assert_eq!(body.unwrap()["stop"].as_array().unwrap().len(), 5);
    }

    #[test]
    fn max_tokens_is_required_and_positive() {
        let mut req = base();
        req.as_object_mut().unwrap().remove("max_tokens");
        assert!(build("generic", "m", req)
            .0
            .unwrap_err()
            .message
            .contains("required"));
        let mut req = base();
        req["max_tokens"] = json!(0);
        assert!(build("generic", "m", req)
            .0
            .unwrap_err()
            .message
            .contains("pre-warming"));
    }

    #[test]
    fn tools_and_tool_choice_translate() {
        let mut req = base();
        req["tools"] = json!([{"name": "Read", "description": "read", "input_schema": {"type": "object"}, "strict": true}]);
        for (choice, expected) in [
            (json!({"type": "auto"}), json!("auto")),
            (json!({"type": "any"}), json!("required")),
            (json!({"type": "none"}), json!("none")),
            (
                json!({"type": "tool", "name": "Read", "disable_parallel_tool_use": true}),
                json!({"type": "function", "function": {"name": "Read"}}),
            ),
        ] {
            req["tool_choice"] = choice.clone();
            let body = build("openai", "gpt-4o", req.clone()).0.unwrap();
            assert_eq!(body["tool_choice"], expected);
            assert_eq!(body["tools"][0]["function"]["strict"], true);
            assert_eq!(body["tools"][0]["function"]["description"], "read");
            let parallel = choice.get("disable_parallel_tool_use").is_some();
            assert_eq!(body.get("parallel_tool_calls").is_some(), parallel);
        }
    }

    #[test]
    fn a_tool_round_trip_orders_messages_and_replays_reasoning() {
        let signer = Signer::from_token("t");
        let sig = signer.sign("let me look", "");
        let req = json!({"model": "claude-x", "max_tokens": 10, "system": "sys", "messages": [
            {"role": "user", "content": "list files"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": "let me look", "signature": sig},
                {"type": "tool_use", "id": "call_1", "name": "ls", "input": {}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_1", "content": [
                    {"type": "text", "text": "a.txt"},
                    {"type": "image", "source": {"type": "url", "url": "https://img"}}
                ]},
                {"type": "text", "text": "thanks"}
            ]}
        ]});
        let body = build("fireworks", "m", req).0.unwrap();
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages[0], json!({"role": "system", "content": "sys"}));
        assert_eq!(messages[2]["content"], Value::Null);
        assert_eq!(messages[2]["reasoning_content"], "let me look");
        assert_eq!(messages[2]["tool_calls"][0]["function"]["arguments"], "{}");
        assert_eq!(
            messages[3],
            json!({"role": "tool", "tool_call_id": "call_1", "content": "a.txt"})
        );
        assert_eq!(messages[4]["content"][1]["image_url"]["url"], "https://img");
        assert_eq!(messages[5], json!({"role": "user", "content": "thanks"}));

        // OpenAI never replays reasoning, and a text-only assistant turn keeps its text.
        let req = json!({"model": "c", "max_tokens": 1, "messages": [
            {"role": "assistant", "content": "prefill"},
        ]});
        let body = build("openai", "gpt-4o", req).0.unwrap();
        assert_eq!(
            body["messages"][0],
            json!({"role": "assistant", "content": "prefill"})
        );
    }

    #[test]
    fn trailing_tool_images_are_flushed() {
        let req = json!({"model": "c", "max_tokens": 1, "messages": [
            {"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "s", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA"}}
            ]}]}
        ]});
        let body = build("generic", "m", req).0.unwrap();
        let last = body["messages"].as_array().unwrap().last().unwrap().clone();
        assert_eq!(last["role"], "user");
        assert_eq!(
            last["content"][1]["image_url"]["url"],
            "data:image/png;base64,AA"
        );
    }

    #[test]
    fn reasoning_controls_per_flavor() {
        let mut req = base();
        req["thinking"] = json!({"type": "enabled", "budget_tokens": 8000});
        req["output_config"] = json!({"effort": "max"});
        let (fw, report) = build("fireworks", "m", req.clone());
        let fw = fw.unwrap();
        assert_eq!(
            fw["thinking"],
            json!({"type": "enabled", "budget_tokens": 8000})
        );
        assert!(fw.get("reasoning_effort").is_none());
        assert!(report
            .ignored
            .iter()
            .any(|i| i.starts_with("output_config.effort")));
        let openai = build("openai", "gpt-5", req.clone()).0.unwrap();
        assert_eq!(openai["reasoning_effort"], "xhigh");
        let generic = build("generic", "m", req).0.unwrap();
        assert_eq!(generic["reasoning_effort"], "high");

        let mut budget_only = base();
        budget_only["thinking"] = json!({"type": "enabled", "budget_tokens": 8000});
        assert_eq!(
            build("openai", "gpt-5", budget_only).0.unwrap()["reasoning_effort"],
            "medium"
        );

        let mut disabled = base();
        disabled["thinking"] = json!({"type": "disabled"});
        disabled["output_config"] = json!({"effort": "low"});
        let fw = build("fireworks", "m", disabled.clone()).0.unwrap();
        assert_eq!(fw["thinking"], json!({"type": "disabled"}));
        let (generic, report) = build("generic", "m", disabled);
        assert!(generic.unwrap().get("reasoning_effort").is_none());
        assert!(report
            .ignored
            .iter()
            .any(|i| i.contains("thinking is disabled")));

        let mut adaptive = base();
        adaptive["thinking"] = json!({"type": "adaptive"});
        assert!(build("fireworks", "m", adaptive.clone())
            .0
            .unwrap()
            .get("reasoning_effort")
            .is_none());
        adaptive["output_config"] = json!({"effort": "medium"});
        assert_eq!(
            build("fireworks", "m", adaptive).0.unwrap()["reasoning_effort"],
            "medium"
        );
    }

    #[test]
    fn structured_output_and_cache_keys() {
        let mut req = base();
        req["output_config"] =
            json!({"format": {"type": "json_schema", "schema": {"type": "object"}}});
        req["cache_control"] = json!({"type": "ephemeral"});
        let (openai, _) = build("openai", "gpt-4o", req.clone());
        let openai = openai.unwrap();
        assert_eq!(openai["response_format"]["type"], "json_schema");
        assert_eq!(openai["response_format"]["json_schema"]["strict"], true);
        assert!(openai["prompt_cache_key"]
            .as_str()
            .unwrap()
            .starts_with("ogw-"));
        let (generic, report) = build("generic", "m", req);
        let generic = generic.unwrap();
        assert_eq!(generic["response_format"]["json_schema"]["strict"], false);
        assert!(generic.get("prompt_cache_key").is_none());
        assert!(report.ignored.contains("cache_control"));
        assert_eq!(Flavor::Generic.as_str(), "generic");
    }

    #[test]
    fn a_reply_parses_with_reasoning_tools_and_usage() {
        let body = json!({
            "choices": [{"finish_reason": "tool_calls", "message": {
                "role": "assistant",
                "reasoning_content": "r1",
                "content": "<think>r2</think>done",
                "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "ls", "arguments": "{}"}}]
            }}],
            "usage": {"prompt_tokens": 5, "completion_tokens": 3}
        });
        let completion = parse_response(&body, true).unwrap();
        assert_eq!(completion.reasoning[0].text, "r1\n\nr2");
        assert_eq!(completion.text, "done");
        assert_eq!(completion.tool_calls[0].name, "ls");
        assert_eq!(completion.finish, Finish::ToolCalls);
        assert_eq!(completion.usage.prompt_tokens, 5);

        let untagged = parse_response(&body, false).unwrap();
        assert_eq!(untagged.text, "<think>r2</think>done");

        let refusal = json!({"choices": [{"finish_reason": "stop", "message": {"content": null, "refusal": "no", "reasoning": "x"}}]});
        let completion = parse_response(&refusal, true).unwrap();
        assert_eq!(completion.refusal.as_deref(), Some("no"));
        assert_eq!(completion.reasoning[0].text, "x");
        assert!(completion.text.is_empty());

        let only_inline = json!({"choices": [{"message": {"content": "<think>a</think>b"}}]});
        assert_eq!(
            parse_response(&only_inline, true).unwrap().reasoning[0].text,
            "a"
        );
        assert!(parse_response(&json!({"choices": []}), true).is_err());
    }
}
