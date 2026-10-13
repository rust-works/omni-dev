//! The field policy: every Anthropic request field, content-block type and
//! tool type the gateway knows, classed as translated, ignored or rejected.
//!
//! The tables here are the single record of that classing. The operator guide
//! (`docs/anthropic-gateway.md`) prints the same table, kept in sync by a test,
//! and the weekly drift check ([`drift`](super::drift)) compares them with the
//! Anthropic SDK's current types.
//!
//! The rule the issue (#2226) sets is that the gateway **never silently drops a
//! field that changes behaviour**: an ignored field is named in the
//! `x-omni-dev-gateway-ignored` response header and logged, and anything the
//! gateway does not know is rejected with an `invalid_request_error`, unless the
//! operator downgrades unknown fields to ignored (`OMNI_DEV_GATEWAY_UNKNOWN_FIELDS=warn`).

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use super::error::GatewayError;

/// How the gateway handles a field or value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldClass {
    /// Translated to an upstream equivalent.
    Translated,
    /// Accepted, not sent upstream, and reported in the
    /// `x-omni-dev-gateway-ignored` header and the log.
    Ignored,
    /// Answered with an `invalid_request_error`; nothing is sent upstream.
    Rejected,
}

impl FieldClass {
    /// The label used in the docs table and `ai gateway fields`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Translated => "translated",
            Self::Ignored => "ignored",
            Self::Rejected => "rejected",
        }
    }
}

/// One row of a policy table.
#[derive(Debug, Clone, Copy)]
pub struct FieldRule {
    /// The field name, block type or tool type.
    pub name: &'static str,
    /// How the gateway handles it.
    pub class: FieldClass,
    /// What happens, in a few words.
    pub note: &'static str,
}

const fn rule(name: &'static str, class: FieldClass, note: &'static str) -> FieldRule {
    FieldRule { name, class, note }
}

use FieldClass::{Ignored, Rejected, Translated};

/// Top-level `POST /v1/messages` (and `count_tokens`) request fields.
pub const REQUEST_FIELDS: &[FieldRule] = &[
    rule("model", Translated, "mapped to an upstream model"),
    rule(
        "max_tokens",
        Translated,
        "`max_tokens`, `max_completion_tokens` (OpenAI) or `max_output_tokens` (Responses); 0 is rejected",
    ),
    rule("messages", Translated, "chat messages / Responses input items"),
    rule("system", Translated, "a leading system message / Responses `instructions`"),
    rule(
        "stop_sequences",
        Translated,
        "`stop`; more than 4 rejected on OpenAI and Fireworks; rejected on the Responses target",
    ),
    rule("stream", Translated, "upstream streaming, re-framed as Anthropic events"),
    rule("temperature", Translated, "dropped (with a warning) for GPT-5/o-series models"),
    rule("top_p", Translated, "dropped (with a warning) for GPT-5/o-series models"),
    rule("top_k", Translated, "sent to Fireworks and generic upstreams; ignored on OpenAI and Responses"),
    rule("tools", Translated, "custom tools become functions; see the tool tables"),
    rule(
        "tool_choice",
        Translated,
        "`auto`, `any`→`required`, `tool`→named function, `none`; `disable_parallel_tool_use`→`parallel_tool_calls: false`",
    ),
    rule("thinking", Translated, "upstream reasoning controls; see the operator guide"),
    rule(
        "output_config",
        Translated,
        "`effort`→reasoning effort, `format`→JSON-schema response format; other keys rejected",
    ),
    rule(
        "cache_control",
        Translated,
        "sets `prompt_cache_key` where the upstream takes one, else ignored",
    ),
    rule("metadata", Ignored, "no upstream equivalent"),
    rule("service_tier", Ignored, "no upstream equivalent"),
    rule("inference_geo", Ignored, "no upstream equivalent"),
    rule("diagnostics", Ignored, "no upstream equivalent"),
    rule("speed", Ignored, "no upstream equivalent"),
    rule("user_profile_id", Ignored, "Anthropic account routing"),
    rule("workspace_id", Ignored, "Anthropic account routing"),
    rule(
        "context_management",
        Ignored,
        "the client's history is sent whole; an overflow returns `prompt is too long`",
    ),
    rule(
        "compaction",
        Ignored,
        "the client's history is sent whole; an overflow returns `prompt is too long`",
    ),
    rule("fallbacks", Ignored, "only the mapped model is called"),
    rule("fallback_credit_token", Ignored, "only the mapped model is called"),
    rule("container", Rejected, "server-side code execution has no upstream equivalent"),
    rule(
        "safeguards",
        Rejected,
        "Anthropic's server-side tool-use classifiers cannot run upstream, and a safety control is never dropped silently; Claude Code retries without it",
    ),
    rule("mcp_servers", Rejected, "the MCP connector has no upstream equivalent"),
];

/// Input content-block types (in `messages[].content` and tool results).
pub const CONTENT_BLOCK_TYPES: &[FieldRule] = &[
    rule("text", Translated, "text"),
    rule(
        "image",
        Translated,
        "`image_url` (base64 data URI or URL); `file` sources rejected",
    ),
    rule(
        "document",
        Translated,
        "plain-text and inline-content documents become text; PDF, URL and file sources rejected",
    ),
    rule("search_result", Translated, "rendered as text"),
    rule(
        "thinking",
        Translated,
        "replayed as upstream reasoning when the gateway's signature verifies, else dropped",
    ),
    rule(
        "redacted_thinking",
        Ignored,
        "encrypted for Anthropic; no upstream can read it",
    ),
    rule(
        "tool_use",
        Translated,
        "assistant `tool_calls` / Responses `function_call`",
    ),
    rule(
        "tool_result",
        Translated,
        "`role: tool` message / Responses `function_call_output`",
    ),
    rule(
        "tool_reference",
        Translated,
        "rendered as text; every tool is sent eagerly",
    ),
    rule(
        "server_tool_use",
        Rejected,
        "server tools have no upstream equivalent",
    ),
    rule(
        "web_search_tool_result",
        Rejected,
        "server tools have no upstream equivalent",
    ),
    rule(
        "web_fetch_tool_result",
        Rejected,
        "server tools have no upstream equivalent",
    ),
    rule(
        "code_execution_tool_result",
        Rejected,
        "server tools have no upstream equivalent",
    ),
    rule(
        "bash_code_execution_tool_result",
        Rejected,
        "server tools have no upstream equivalent",
    ),
    rule(
        "text_editor_code_execution_tool_result",
        Rejected,
        "server tools have no upstream equivalent",
    ),
    rule(
        "tool_search_tool_result",
        Rejected,
        "server tools have no upstream equivalent",
    ),
    rule(
        "container_upload",
        Rejected,
        "server-side code execution has no upstream equivalent",
    ),
];

/// Fields of a client tool definition.
pub const TOOL_FIELDS: &[FieldRule] = &[
    rule(
        "type",
        Translated,
        "absent or `custom`; any Anthropic-defined tool type is rejected",
    ),
    rule(
        "name",
        Translated,
        "names over 64 characters are shortened and mapped back",
    ),
    rule("description", Translated, "function description"),
    rule("input_schema", Translated, "function parameters"),
    rule("strict", Translated, "function `strict`"),
    rule("input_examples", Translated, "appended to the description"),
    rule(
        "cache_control",
        Translated,
        "contributes to `prompt_cache_key`",
    ),
    rule("defer_loading", Ignored, "every tool is sent eagerly"),
    rule("eager_input_streaming", Ignored, "arguments always stream"),
    rule(
        "allowed_callers",
        Ignored,
        "only direct calls exist upstream",
    ),
];

/// Anthropic-defined tool `type` prefixes, all rejected: they are server tools
/// or Anthropic-schema client tools with no portable definition.
pub const ANTHROPIC_TOOL_TYPE_PREFIXES: &[&str] = &[
    "bash_",
    "text_editor_",
    "computer_",
    "browser_",
    "code_execution_",
    "memory_",
    "web_search_",
    "web_fetch_",
    "tool_search_tool_",
    "mcp_toolset",
    "advisor_",
];

/// `thinking.type` values, all translated.
pub const THINKING_TYPES: &[&str] = &["enabled", "disabled", "adaptive", "between_tools"];

/// `thinking.display` values. `updates` (Claude Code's
/// `thinking-display-updates` beta) is served like `summarized`.
pub const THINKING_DISPLAYS: &[&str] = &["summarized", "omitted", "updates"];

/// Anthropic stop reasons, and whether the gateway can produce each.
pub const STOP_REASONS: &[FieldRule] = &[
    rule("end_turn", Translated, "`finish_reason: stop`"),
    rule("max_tokens", Translated, "`finish_reason: length`"),
    rule(
        "stop_sequence",
        Ignored,
        "upstreams do not say which sequence matched; reported as `end_turn`",
    ),
    rule("tool_use", Translated, "`finish_reason: tool_calls`"),
    rule("pause_turn", Ignored, "only server tools pause"),
    rule(
        "refusal",
        Translated,
        "`content_filter` or an upstream `refusal`",
    ),
    rule(
        "model_context_window_exceeded",
        Ignored,
        "upstreams return an error, answered as `prompt is too long`",
    ),
];

/// Stream `content_block_delta` types, and whether the gateway emits each.
pub const STREAM_DELTA_TYPES: &[FieldRule] = &[
    rule("text_delta", Translated, "text"),
    rule("input_json_delta", Translated, "tool arguments"),
    rule("thinking_delta", Translated, "reasoning text"),
    rule("signature_delta", Translated, "the gateway's signature"),
    rule("citations_delta", Ignored, "upstreams return no citations"),
];

/// How unknown fields are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnknownFields {
    /// Rejected with an `invalid_request_error` (the default).
    #[default]
    Reject,
    /// Ignored with a warning.
    Warn,
}

/// What the policy check found in an accepted request.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PolicyReport {
    /// Ignored fields and values, by generic name (deduplicated, sorted).
    pub ignored: BTreeSet<String>,
    /// Whether the client marked anything with `cache_control`.
    pub cache_hint: bool,
}

impl PolicyReport {
    /// Records an ignored field or value.
    pub(crate) fn ignore(&mut self, name: impl Into<String>) {
        self.ignored.insert(name.into());
    }
}

fn lookup<'a>(table: &'a [FieldRule], name: &str) -> Option<&'a FieldRule> {
    table.iter().find(|r| r.name == name)
}

/// Checks a request body against the tables.
///
/// Returns the ignored fields, or the first rejection as an
/// `invalid_request_error`. Structural checks (types, required fields) are
/// left to deserialization, which runs next.
pub(crate) fn check_request(
    body: &Map<String, Value>,
    unknown: UnknownFields,
) -> Result<PolicyReport, GatewayError> {
    let mut report = PolicyReport::default();
    for (key, value) in body {
        if value.is_null() {
            continue;
        }
        match lookup(REQUEST_FIELDS, key) {
            Some(rule) => match rule.class {
                Translated => {}
                Ignored => report.ignore(key.as_str()),
                Rejected => return Err(rejected(key, rule.note)),
            },
            None => unknown_field(&mut report, unknown, key)?,
        }
    }
    if body.get("cache_control").is_some_and(|v| !v.is_null()) {
        report.cache_hint = true;
    }
    if let Some(Value::Array(system)) = body.get("system") {
        for block in system {
            check_block_extras(block, &mut report);
        }
    }
    if let Some(Value::Array(messages)) = body.get("messages") {
        for (i, message) in messages.iter().enumerate() {
            check_message_keys(message, &mut report, unknown)?;
            if let Some(Value::Array(blocks)) = message.get("content") {
                for (j, block) in blocks.iter().enumerate() {
                    check_block(
                        block,
                        &format!("messages[{i}].content[{j}]"),
                        &mut report,
                        unknown,
                    )?;
                }
            }
        }
    }
    if let Some(Value::Array(tools)) = body.get("tools") {
        for (i, tool) in tools.iter().enumerate() {
            check_tool(tool, i, &mut report, unknown)?;
        }
    }
    if let Some(thinking) = body.get("thinking").filter(|v| !v.is_null()) {
        check_thinking(thinking)?;
        if let Some(display) = thinking.get("display").and_then(Value::as_str) {
            if !THINKING_DISPLAYS.contains(&display) {
                unknown_field(
                    &mut report,
                    unknown,
                    &format!("thinking.display `{display}`"),
                )?;
            }
        }
    }
    if let Some(Value::Object(output)) = body.get("output_config") {
        for (key, value) in output {
            match key.as_str() {
                "effort" | "format" => {}
                _ if value.is_null() => {}
                other => unknown_field(&mut report, unknown, &format!("output_config.{other}"))?,
            }
        }
        if let Some(format) = output.get("format").filter(|v| !v.is_null()) {
            let kind = format.get("type").and_then(Value::as_str);
            if kind != Some("json_schema") {
                return Err(GatewayError::invalid(format!(
                    "output_config.format.type {kind:?} is not supported by the gateway; only \
                     `json_schema` is"
                )));
            }
        }
    }
    Ok(report)
}

/// Message-level keys: `role` and `content`, plus a per-turn
/// `output_config` (Claude Code's `per-turn-control` beta) whose `effort` sets
/// the effort for this turn. Anything else is unknown.
fn check_message_keys(
    message: &Value,
    report: &mut PolicyReport,
    unknown: UnknownFields,
) -> Result<(), GatewayError> {
    let Some(fields) = message.as_object() else {
        return Ok(());
    };
    for (key, value) in fields {
        if value.is_null() {
            continue;
        }
        match key.as_str() {
            "role" | "content" => {}
            "output_config" => {
                for (sub, sub_value) in value.as_object().into_iter().flatten() {
                    if sub != "effort" && !sub_value.is_null() {
                        unknown_field(report, unknown, &format!("messages.output_config.{sub}"))?;
                    }
                }
            }
            other => unknown_field(report, unknown, &format!("messages.{other}"))?,
        }
    }
    Ok(())
}

/// Records `cache_control` and ignored per-block extras (`citations`,
/// image `transformations`).
fn check_block_extras(block: &Value, report: &mut PolicyReport) {
    if block.get("cache_control").is_some_and(|v| !v.is_null()) {
        report.cache_hint = true;
    }
    if block.get("citations").is_some_and(|v| !v.is_null()) {
        report.ignore("citations");
    }
    if block.get("transformations").is_some_and(|v| !v.is_null()) {
        report.ignore("image.transformations");
    }
}

fn check_block(
    block: &Value,
    path: &str,
    report: &mut PolicyReport,
    unknown: UnknownFields,
) -> Result<(), GatewayError> {
    let Some(kind) = block.get("type").and_then(Value::as_str) else {
        return Err(GatewayError::invalid(format!(
            "{path}: content block has no `type`"
        )));
    };
    check_block_extras(block, report);
    let Some(rule) = lookup(CONTENT_BLOCK_TYPES, kind) else {
        unknown_field(report, unknown, &format!("content block type `{kind}`"))?;
        return Ok(());
    };
    match rule.class {
        Translated => {}
        Ignored => report.ignore(kind),
        Rejected => return Err(rejected(&format!("{path} ({kind})"), rule.note)),
    }
    match kind {
        "image" => check_source(block, path, &["base64", "url"], "image")?,
        "document" => check_source(block, path, &["text", "content"], "document")?,
        "tool_result" => {
            if let Some(Value::Array(inner)) = block.get("content") {
                for (k, nested) in inner.iter().enumerate() {
                    check_block(nested, &format!("{path}.content[{k}]"), report, unknown)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_source(
    block: &Value,
    path: &str,
    allowed: &[&str],
    what: &str,
) -> Result<(), GatewayError> {
    let source = block
        .get("source")
        .and_then(|s| s.get("type"))
        .and_then(Value::as_str);
    match source {
        Some(kind) if allowed.contains(&kind) => {
            if what == "document" {
                if let Some(Value::Array(items)) = block.pointer("/source/content") {
                    if items
                        .iter()
                        .any(|i| i.get("type").and_then(Value::as_str) != Some("text"))
                    {
                        return Err(GatewayError::invalid(format!(
                            "{path}: only text blocks are supported inside a content document"
                        )));
                    }
                }
            }
            Ok(())
        }
        other => Err(GatewayError::invalid(format!(
            "{path}: {what} source type {other:?} is not supported by the gateway (supported: {})",
            allowed.join(", ")
        ))),
    }
}

fn check_tool(
    tool: &Value,
    index: usize,
    report: &mut PolicyReport,
    unknown: UnknownFields,
) -> Result<(), GatewayError> {
    let kind = tool.get("type").and_then(Value::as_str);
    if let Some(kind) = kind.filter(|k| *k != "custom") {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("?");
        return Err(GatewayError::invalid(format!(
            "tools[{index}] ({name}): tool type `{kind}` is an Anthropic-defined or server tool \
             with no OpenAI-protocol equivalent; define it as a custom tool with an input_schema"
        )));
    }
    let Some(fields) = tool.as_object() else {
        return Err(GatewayError::invalid(format!(
            "tools[{index}] is not an object"
        )));
    };
    for (key, value) in fields {
        if value.is_null() {
            continue;
        }
        match lookup(TOOL_FIELDS, key) {
            Some(rule) => match rule.class {
                Translated => {
                    if key == "cache_control" {
                        report.cache_hint = true;
                    }
                }
                Ignored => report.ignore(format!("tools.{key}")),
                Rejected => return Err(rejected(&format!("tools[{index}].{key}"), rule.note)),
            },
            None => unknown_field(report, unknown, &format!("tools.{key}"))?,
        }
    }
    Ok(())
}

fn check_thinking(thinking: &Value) -> Result<(), GatewayError> {
    let kind = thinking.get("type").and_then(Value::as_str).unwrap_or("");
    if THINKING_TYPES.contains(&kind) {
        Ok(())
    } else {
        Err(GatewayError::invalid(format!(
            "thinking.type `{kind}` is not known to the gateway (known: {})",
            THINKING_TYPES.join(", ")
        )))
    }
}

fn rejected(what: &str, note: &str) -> GatewayError {
    GatewayError::invalid(format!(
        "`{what}` is not supported by the omni-dev Anthropic gateway: {note}"
    ))
}

fn unknown_field(
    report: &mut PolicyReport,
    unknown: UnknownFields,
    name: &str,
) -> Result<(), GatewayError> {
    match unknown {
        UnknownFields::Warn => {
            report.ignore(name);
            Ok(())
        }
        UnknownFields::Reject => Err(GatewayError::invalid(format!(
            "`{name}` is not known to the omni-dev Anthropic gateway, so it cannot be translated \
             safely; set OMNI_DEV_GATEWAY_UNKNOWN_FIELDS=warn to ignore unknown fields instead"
        ))),
    }
}

/// Renders every table as Markdown, as printed by `ai gateway fields` and
/// mirrored in the operator guide.
#[must_use]
pub fn render_markdown() -> String {
    let mut out = String::new();
    for (title, table) in [
        ("Request fields", REQUEST_FIELDS),
        ("Content blocks", CONTENT_BLOCK_TYPES),
        ("Tool fields", TOOL_FIELDS),
        ("Stop reasons", STOP_REASONS),
        ("Stream deltas", STREAM_DELTA_TYPES),
    ] {
        out.push_str(&format!(
            "#### {title}\n\n| Name | Class | Handling |\n|---|---|---|\n"
        ));
        for rule in table {
            out.push_str(&format!(
                "| `{}` | {} | {} |\n",
                rule.name,
                rule.class.as_str(),
                rule.note
            ));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn check(body: Value) -> Result<PolicyReport, GatewayError> {
        check_request(body.as_object().unwrap(), UnknownFields::Reject)
    }

    fn base() -> Value {
        json!({"model": "m", "max_tokens": 10, "messages": [{"role": "user", "content": "hi"}]})
    }

    fn with(mut body: Value, key: &str, value: Value) -> Value {
        body[key] = value;
        body
    }

    #[test]
    fn every_table_name_is_unique() {
        for table in [
            REQUEST_FIELDS,
            CONTENT_BLOCK_TYPES,
            TOOL_FIELDS,
            STOP_REASONS,
            STREAM_DELTA_TYPES,
        ] {
            let names: BTreeSet<_> = table.iter().map(|r| r.name).collect();
            assert_eq!(names.len(), table.len());
        }
    }

    #[test]
    fn a_plain_request_passes_with_nothing_ignored() {
        let report = check(base()).unwrap();
        assert!(report.ignored.is_empty());
        assert!(!report.cache_hint);
    }

    #[test]
    fn each_request_field_gets_its_class() {
        for rule in REQUEST_FIELDS {
            let body = with(base(), rule.name, json!("x"));
            let result = check(body);
            match rule.class {
                Translated => {}
                Ignored => assert!(result.unwrap().ignored.contains(rule.name), "{}", rule.name),
                Rejected => {
                    let err = result.unwrap_err();
                    assert!(err.message.contains(rule.name), "{}", err.message);
                }
            }
        }
    }

    #[test]
    fn null_fields_are_absent() {
        let report = check(with(base(), "container", Value::Null)).unwrap();
        assert!(report.ignored.is_empty());
    }

    #[test]
    fn unknown_fields_are_rejected_unless_downgraded() {
        let body = with(base(), "brand_new_field", json!(1));
        let err = check(body.clone()).unwrap_err();
        assert!(err.message.contains("brand_new_field"));
        assert!(err.message.contains("OMNI_DEV_GATEWAY_UNKNOWN_FIELDS"));
        let report = check_request(body.as_object().unwrap(), UnknownFields::Warn).unwrap();
        assert!(report.ignored.contains("brand_new_field"));
    }

    fn message_with(block: Value) -> Value {
        with(
            base(),
            "messages",
            json!([{"role": "user", "content": [block]}]),
        )
    }

    #[test]
    fn each_content_block_type_gets_its_class() {
        for rule in CONTENT_BLOCK_TYPES {
            let block = match rule.name {
                "image" => {
                    json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "A"}})
                }
                "document" => {
                    json!({"type": "document", "source": {"type": "text", "media_type": "text/plain", "data": "d"}})
                }
                other => json!({"type": other}),
            };
            let result = check(message_with(block));
            match rule.class {
                Translated => assert!(result.is_ok(), "{}", rule.name),
                Ignored => assert!(result.unwrap().ignored.contains(rule.name)),
                Rejected => assert!(result.unwrap_err().message.contains(rule.name)),
            }
        }
    }

    #[test]
    fn unsupported_sources_are_rejected() {
        let pdf = json!({"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "A"}});
        assert!(check(message_with(pdf))
            .unwrap_err()
            .message
            .contains("document source"));
        let file = json!({"type": "image", "source": {"type": "file", "file_id": "f"}});
        assert!(check(message_with(file))
            .unwrap_err()
            .message
            .contains("image source"));
        let image_doc = json!({"type": "document", "source": {"type": "content", "content": [{"type": "image"}]}});
        assert!(check(message_with(image_doc))
            .unwrap_err()
            .message
            .contains("only text"));
        let untyped = json!({"text": "x"});
        assert!(check(message_with(untyped))
            .unwrap_err()
            .message
            .contains("no `type`"));
    }

    #[test]
    fn rejections_inside_tool_results_are_found() {
        let block = json!({"type": "tool_result", "tool_use_id": "t", "content": [
            {"type": "text", "text": "ok"},
            {"type": "web_search_tool_result"}
        ]});
        let err = check(message_with(block)).unwrap_err();
        assert!(
            err.message.contains("content[0].content[1]"),
            "{}",
            err.message
        );
    }

    #[test]
    fn message_level_keys_are_checked() {
        let per_turn =
            json!([{"role": "system", "content": "s", "output_config": {"effort": "high"}}]);
        assert!(check(with(base(), "messages", per_turn)).is_ok());
        let format = json!([{"role": "user", "content": "s", "output_config": {"format": {}}}]);
        assert!(check(with(base(), "messages", format)).is_err());
        let extra = json!([{"role": "user", "content": "s", "name": "bob"}]);
        assert!(check(with(base(), "messages", extra.clone()))
            .unwrap_err()
            .message
            .contains("messages.name"));
        let report = check_request(
            with(base(), "messages", extra).as_object().unwrap(),
            UnknownFields::Warn,
        )
        .unwrap();
        assert!(report.ignored.contains("messages.name"));
        assert!(check(with(base(), "messages", json!(["not an object"]))).is_ok());
    }

    #[test]
    fn cache_control_anywhere_sets_the_hint() {
        let top = check(with(base(), "cache_control", json!({"type": "ephemeral"}))).unwrap();
        assert!(top.cache_hint);
        let block = check(message_with(
            json!({"type": "text", "text": "x", "cache_control": {"type": "ephemeral"}}),
        ))
        .unwrap();
        assert!(block.cache_hint);
        let system = check(with(
            base(),
            "system",
            json!([{"type": "text", "text": "s", "cache_control": {"type": "ephemeral"}}]),
        ))
        .unwrap();
        assert!(system.cache_hint);
        let tool = check(with(
            base(),
            "tools",
            json!([{"name": "t", "input_schema": {}, "cache_control": {"type": "ephemeral"}}]),
        ))
        .unwrap();
        assert!(tool.cache_hint);
    }

    #[test]
    fn block_extras_are_reported() {
        let report = check(message_with(
            json!({"type": "text", "text": "x", "citations": []}),
        ))
        .unwrap();
        assert!(report.ignored.contains("citations"));
        let report = check(message_with(
            json!({"type": "image", "source": {"type": "url", "url": "u"}, "transformations": {}}),
        ))
        .unwrap();
        assert!(report.ignored.contains("image.transformations"));
    }

    #[test]
    fn tool_fields_get_their_class() {
        for rule in TOOL_FIELDS {
            if rule.name == "type" {
                continue;
            }
            let tool = json!({"name": "t", "input_schema": {}, rule.name: true});
            let report = check(with(base(), "tools", json!([tool]))).unwrap();
            if rule.class == Ignored {
                assert!(report.ignored.contains(&format!("tools.{}", rule.name)));
            }
        }
        let unknown = json!([{"name": "t", "input_schema": {}, "surprise": 1}]);
        assert!(check(with(base(), "tools", unknown)).is_err());
        assert!(check(with(base(), "tools", json!(["not an object"]))).is_err());
    }

    #[test]
    fn anthropic_defined_tools_are_rejected_and_custom_accepted() {
        for kind in ["bash_20250124", "web_search_20260318", "computer_20260801"] {
            let tools = json!([{"type": kind, "name": "x"}]);
            let err = check(with(base(), "tools", tools)).unwrap_err();
            assert!(err.message.contains(kind));
        }
        let custom = json!([{"type": "custom", "name": "x", "input_schema": {}}]);
        assert!(check(with(base(), "tools", custom)).is_ok());
    }

    #[test]
    fn thinking_types_are_checked() {
        for kind in THINKING_TYPES {
            assert!(check(with(base(), "thinking", json!({"type": kind}))).is_ok());
        }
        assert!(check(with(base(), "thinking", json!({"type": "deep"}))).is_err());
        for display in THINKING_DISPLAYS {
            let thinking = json!({"type": "adaptive", "display": display});
            assert!(check(with(base(), "thinking", thinking)).is_ok());
        }
        let future = with(
            base(),
            "thinking",
            json!({"type": "adaptive", "display": "vivid"}),
        );
        assert!(check(future.clone()).unwrap_err().message.contains("vivid"));
        let report = check_request(future.as_object().unwrap(), UnknownFields::Warn).unwrap();
        assert!(report.ignored.contains("thinking.display `vivid`"));
    }

    #[test]
    fn output_config_keys_and_format_type_are_checked() {
        let ok = json!({"effort": "high", "format": {"type": "json_schema", "schema": {}}});
        assert!(check(with(base(), "output_config", ok)).is_ok());
        let unknown = json!({"speed": "fast"});
        assert!(check(with(base(), "output_config", unknown)).is_err());
        let grammar = json!({"format": {"type": "grammar"}});
        assert!(check(with(base(), "output_config", grammar)).is_err());
    }

    #[test]
    fn markdown_lists_every_row() {
        let md = render_markdown();
        for rule in REQUEST_FIELDS.iter().chain(CONTENT_BLOCK_TYPES) {
            assert!(
                md.contains(&format!("| `{}` |", rule.name)),
                "{}",
                rule.name
            );
        }
        assert_eq!(FieldClass::Rejected.as_str(), "rejected");
    }
}
