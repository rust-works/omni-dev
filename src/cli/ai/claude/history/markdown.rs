//! Renders a Claude Code session jsonl as LLM-friendly markdown.
//!
//! Input is a byte slice of jsonl (already truncated to the snapshot-EOF
//! length by the sync's reader); output is a markdown document with YAML
//! frontmatter and per-turn `## User` / `## Assistant` sections. Tool calls
//! render as `### Tool call: <name>` blocks; tool results pair back to their
//! call by `tool_use_id`. Thinking blocks collapse into `<details>`.
//!
//! Unknown event shapes fall through to a defensive blockquote so a future
//! Claude Code version can never silently drop content from the analyst's
//! corpus.
//!
//! See [crate::cli::ai::claude::history] for the design rationale.

use std::collections::HashMap;
use std::fmt::Write as _;

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use serde_json::Value;

use super::common::decode_slug;

/// Options for [`render`].
#[derive(Clone, Copy, Debug)]
pub struct RenderOptions<'a> {
    /// Encoded project slug (e.g. `-Users-jky-foo`). Used in frontmatter and
    /// to derive the decoded `project_cwd`.
    pub project_slug: &'a str,
    /// Session UUID (file stem of the source jsonl).
    pub session_uuid: &'a str,
    /// When true, suppresses system-side events (system reminders inside user
    /// text, attachments, permission-mode events, summary events, generic
    /// `system` events) from the rendered body. Frontmatter is unchanged.
    pub exclude_system: bool,
}

/// Renders the supplied jsonl bytes as markdown.
///
/// Tolerant of a trailing partial line (snapshot-EOF) — the partial prefix
/// is silently skipped rather than failing the render. Tolerant of unknown
/// event types and unfamiliar field shapes — these fall through to a
/// `> **Unknown event:**` blockquote so the output is never silently lossy.
pub fn render(jsonl_bytes: &[u8], options: RenderOptions<'_>) -> Result<String> {
    let events = parse_events(jsonl_bytes);
    let frontmatter_yaml =
        build_frontmatter_yaml(&events, options).context("Failed to build markdown frontmatter")?;

    let mut body = String::new();
    let mut tool_use_names: HashMap<String, String> = HashMap::new();

    for event in &events {
        render_event(event, options, &mut tool_use_names, &mut body);
    }

    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&frontmatter_yaml);
    out.push_str("---\n\n");
    out.push_str(&body);

    Ok(normalise_whitespace(&out))
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Event {
    /// Top-level `.type` value, captured verbatim.
    kind: String,
    /// `.timestamp` parsed as RFC 3339 when available.
    timestamp: Option<DateTime<Utc>>,
    /// The full event JSON, retained so renderers can pull arbitrary fields.
    raw: Value,
}

/// Parses each non-empty line as JSON. Lines that fail to parse are
/// **dropped silently** — this is the snapshot-EOF tolerance: a truncated
/// trailing line shouldn't fail the whole render.
fn parse_events(jsonl_bytes: &[u8]) -> Vec<Event> {
    let text = std::str::from_utf8(jsonl_bytes).unwrap_or("");
    let mut out = Vec::new();
    for line in text.split('\n') {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_rfc3339);
        out.push(Event {
            kind,
            timestamp,
            raw: value,
        });
    }
    out
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

// ---------------------------------------------------------------------------
// Frontmatter
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Frontmatter {
    session_id: String,
    project_slug: String,
    project_cwd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    git_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entrypoint: Option<String>,
    ai_title: Option<String>,
    first_event: Option<String>,
    last_event: Option<String>,
    event_count: usize,
}

fn build_frontmatter_yaml(events: &[Event], options: RenderOptions<'_>) -> Result<String> {
    let session_id = events
        .iter()
        .find_map(|e| {
            e.raw
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| options.session_uuid.to_string());

    // Pull the most recent value for each rich-event-only field. Claude
    // sometimes regenerates these mid-session; "last wins" matches the
    // intuition that the latest snapshot of state is the most useful.
    let git_branch = last_string_field(events, &["assistant", "user"], "gitBranch");
    let version = last_string_field(events, &["assistant", "user"], "version");
    let entrypoint = last_string_field(events, &["assistant", "user"], "entrypoint");

    let ai_title = events
        .iter()
        .rev()
        .find(|e| e.kind == "ai-title" || e.kind == "custom-title")
        .and_then(|e| {
            e.raw
                .get("aiTitle")
                .or_else(|| e.raw.get("customTitle"))
                .and_then(Value::as_str)
                .map(str::to_string)
        });

    let first_event = events.iter().find_map(|e| e.timestamp.map(format_ts));
    let last_event = events.iter().rev().find_map(|e| e.timestamp.map(format_ts));

    let frontmatter = Frontmatter {
        session_id,
        project_slug: options.project_slug.to_string(),
        project_cwd: decode_slug(options.project_slug),
        git_branch,
        version,
        entrypoint,
        ai_title,
        first_event,
        last_event,
        event_count: events.len(),
    };

    serde_yaml::to_string(&frontmatter).context("Failed to serialise frontmatter as YAML")
}

fn last_string_field(events: &[Event], kinds: &[&str], field: &str) -> Option<String> {
    events
        .iter()
        .rev()
        .filter(|e| kinds.contains(&e.kind.as_str()))
        .find_map(|e| e.raw.get(field).and_then(Value::as_str).map(str::to_string))
}

fn format_ts(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Secs, true)
}

// ---------------------------------------------------------------------------
// Body rendering
// ---------------------------------------------------------------------------

fn render_event(
    event: &Event,
    options: RenderOptions<'_>,
    tool_use_names: &mut HashMap<String, String>,
    out: &mut String,
) {
    match event.kind.as_str() {
        "user" => render_user_event(event, options, tool_use_names, out),
        "assistant" => render_assistant_event(event, options, tool_use_names, out),
        // Hoisted into frontmatter, or pure plumbing — never rendered in the body.
        "ai-title"
        | "custom-title"
        | "file-history-snapshot"
        | "queue-operation"
        | "last-prompt"
        | "progress"
        | "pr-link"
        | "agent-name"
        | "worktree-state" => {}
        "attachment" => {
            if !options.exclude_system {
                render_attachment_event(event, out);
            }
        }
        "permission-mode" => {
            if !options.exclude_system {
                render_permission_mode_event(event, out);
            }
        }
        "system" => {
            if !options.exclude_system {
                render_system_event(event, out);
            }
        }
        "summary" => {
            if !options.exclude_system {
                render_summary_event(event, out);
            }
        }
        other => {
            if !options.exclude_system {
                writeln!(out, "> **Unknown event:** `{other}`\n").ok();
            }
        }
    }
}

fn render_user_event(
    event: &Event,
    options: RenderOptions<'_>,
    tool_use_names: &HashMap<String, String>,
    out: &mut String,
) {
    let Some(content) = event.raw.get("message").and_then(|m| m.get("content")) else {
        return;
    };

    // Tool results render as a separate labelled section under the assistant
    // (we don't emit a `## User` heading for a turn that's purely tool
    // results — that would clutter the transcript).
    if let Some(items) = content.as_array() {
        if items
            .iter()
            .all(|i| i.get("type").and_then(Value::as_str) == Some("tool_result"))
        {
            for item in items {
                render_tool_result(item, tool_use_names, out);
            }
            return;
        }
    }

    let texts = collect_user_text_blocks(content);
    if texts.is_empty() {
        return;
    }

    let mut visible_segments: Vec<String> = Vec::new();
    let mut system_segments: Vec<String> = Vec::new();

    for text in &texts {
        for (label, body) in split_system_envelopes(text) {
            let cleaned = body.trim();
            if cleaned.is_empty() {
                continue;
            }
            if let Some(name) = label {
                system_segments.push(format!(
                    "> **{}:** {}",
                    humanise_tag(&name),
                    single_line_summary(cleaned)
                ));
            } else {
                visible_segments.push(cleaned.to_string());
            }
        }
    }

    if visible_segments.is_empty() && (options.exclude_system || system_segments.is_empty()) {
        return;
    }

    let timestamp = event
        .timestamp
        .map(|ts| format!(" · {}", format_ts(ts)))
        .unwrap_or_default();

    out.push_str("## User");
    out.push_str(&timestamp);
    out.push_str("\n\n");

    if !visible_segments.is_empty() {
        out.push_str(&visible_segments.join("\n\n"));
        out.push_str("\n\n");
    }

    if !options.exclude_system {
        for seg in &system_segments {
            out.push_str(seg);
            out.push_str("\n\n");
        }
    }
}

fn collect_user_text_blocks(content: &Value) -> Vec<String> {
    if let Some(s) = content.as_str() {
        return vec![s.to_string()];
    }
    if let Some(items) = content.as_array() {
        return items
            .iter()
            .filter_map(|i| match i.get("type").and_then(Value::as_str) {
                Some("text") => i.get("text").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect();
    }
    Vec::new()
}

/// Splits a user-text string at `<tag>...</tag>` envelopes Claude Code injects
/// (e.g. `<system-reminder>`, `<command-name>`, `<ide_opened_file>`). Returns
/// `(tag_name, body)` segments in source order; `tag_name = None` for plain
/// prose between envelopes.
fn split_system_envelopes(text: &str) -> Vec<(Option<String>, String)> {
    let known = [
        "system-reminder",
        "command-name",
        "command-message",
        "command-args",
        "ide_opened_file",
        "ide_selection",
        "user-prompt-submit-hook",
        "local-command-stdout",
        "stdout",
        "stderr",
        "user-memory-input",
        "persisted-output",
    ];
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while cursor < text.len() {
        // Find the next opening tag from `cursor`.
        let mut next_tag: Option<(&str, usize, usize)> = None;
        for tag in known {
            let needle = format!("<{tag}>");
            if let Some(rel) = text[cursor..].find(&needle) {
                let abs = cursor + rel;
                match next_tag {
                    Some((_, prior, _)) if prior <= abs => {}
                    _ => next_tag = Some((tag, abs, needle.len())),
                }
            }
        }
        match next_tag {
            None => {
                let leftover = &text[cursor..];
                if !leftover.is_empty() {
                    out.push((None, leftover.to_string()));
                }
                break;
            }
            Some((tag, open_at, open_len)) => {
                if open_at > cursor {
                    out.push((None, text[cursor..open_at].to_string()));
                }
                let body_start = open_at + open_len;
                let close = format!("</{tag}>");
                if let Some(rel) = text[body_start..].find(&close) {
                    let body_end = body_start + rel;
                    out.push((
                        Some(tag.to_string()),
                        text[body_start..body_end].to_string(),
                    ));
                    cursor = body_end + close.len();
                } else {
                    // Unterminated — keep the rest as plain text including the open tag.
                    out.push((None, text[open_at..].to_string()));
                    break;
                }
            }
        }
    }
    out
}

fn humanise_tag(tag: &str) -> String {
    let mut s: String = tag
        .chars()
        .map(|c| if c == '-' || c == '_' { ' ' } else { c })
        .collect();
    if let Some(c) = s.get_mut(0..1) {
        c.make_ascii_uppercase();
    }
    s
}

fn single_line_summary(text: &str) -> String {
    // Many system-reminder bodies are multi-line; the first non-empty line is
    // a usable headline, with the remainder folded into a `<details>` if it
    // adds anything beyond the headline.
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let head = lines.next().unwrap_or("").trim().to_string();
    let rest: Vec<&str> = lines.collect();
    if rest.is_empty() {
        return head;
    }
    let body = rest.join("\n");
    format!(
        "{head}\n>\n> <details>\n> <summary>more</summary>\n>\n> {}\n>\n> </details>",
        body.replace('\n', "\n> ")
    )
}

fn render_assistant_event(
    event: &Event,
    options: RenderOptions<'_>,
    tool_use_names: &mut HashMap<String, String>,
    out: &mut String,
) {
    let Some(content) = event.raw.get("message").and_then(|m| m.get("content")) else {
        return;
    };
    let Some(items) = content.as_array() else {
        return;
    };
    if items.is_empty() {
        return;
    }

    out.push_str("## Assistant\n\n");

    for item in items {
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "text" => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        out.push_str(trimmed);
                        out.push_str("\n\n");
                    }
                }
            }
            "thinking" => {
                if let Some(text) = item.get("thinking").and_then(Value::as_str) {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        out.push_str("<details>\n<summary>Thinking</summary>\n\n");
                        out.push_str(trimmed);
                        out.push_str("\n\n</details>\n\n");
                    }
                }
            }
            "tool_use" => {
                let id = item
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if !id.is_empty() {
                    tool_use_names.insert(id, name.clone());
                }
                let input = item.get("input").cloned().unwrap_or(Value::Null);
                render_tool_use(&name, &input, options, out);
            }
            _ => {
                if !options.exclude_system {
                    writeln!(out, "> **Unknown assistant block:** `{kind}`\n").ok();
                }
            }
        }
    }
}

fn render_tool_use(name: &str, input: &Value, _options: RenderOptions<'_>, out: &mut String) {
    // AskUserQuestion has its own structured rendering — the JSON shape is
    // dense and the analyst signal is the question text + options, not the
    // wire format.
    if name == "AskUserQuestion" {
        render_ask_user_question(input, out);
        return;
    }

    let _ = writeln!(out, "### Tool call: `{name}`");
    out.push('\n');

    // Special case: Agent — render only the prompt, hide description / subagent_type.
    let payload = if name == "Agent" {
        match input.get("prompt") {
            Some(p) => p.clone(),
            None => Value::Null,
        }
    } else {
        input.clone()
    };

    let body = match (&payload, name) {
        (Value::String(s), _) => s.clone(),
        _ => serde_json::to_string_pretty(&payload).unwrap_or_else(|_| payload.to_string()),
    };

    let lang = language_for_tool(name);
    write_fenced(out, lang, &body);
    out.push('\n');
}

fn language_for_tool(name: &str) -> &'static str {
    match name {
        "Bash" => "bash",
        _ => "",
    }
}

/// Renders an `AskUserQuestion` invocation as a structured Q&A invitation:
/// each question becomes a `### Agent question:` block listing its options,
/// so the analyst can see what the agent asked without parsing JSON.
fn render_ask_user_question(input: &Value, out: &mut String) {
    let questions = input
        .get("questions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if questions.is_empty() {
        // Defensive: fall back to a generic tool-use rendering if shape is unexpected.
        let _ = writeln!(out, "### Agent question");
        out.push('\n');
        let body = serde_json::to_string_pretty(input).unwrap_or_else(|_| input.to_string());
        write_fenced(out, "", &body);
        out.push('\n');
        return;
    }
    for q in questions {
        let header = q
            .get("header")
            .and_then(Value::as_str)
            .unwrap_or("Question");
        let question = q.get("question").and_then(Value::as_str).unwrap_or("");
        let multi = q
            .get("multiSelect")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let multi_marker = if multi { " (multi-select)" } else { "" };
        let _ = writeln!(out, "### Agent question: {header}{multi_marker}");
        out.push('\n');
        if !question.is_empty() {
            out.push_str(question);
            out.push_str("\n\n");
        }
        if let Some(options) = q.get("options").and_then(Value::as_array) {
            for opt in options {
                let label = opt.get("label").and_then(Value::as_str).unwrap_or("");
                let desc = opt.get("description").and_then(Value::as_str).unwrap_or("");
                if desc.is_empty() {
                    let _ = writeln!(out, "- **{label}**");
                } else {
                    let _ = writeln!(out, "- **{label}** — {desc}");
                }
            }
            out.push('\n');
        }
    }
}

/// Classifies what really happened with a tool result. Distinguishes
/// successful runs from user-driven outcomes (denial, interrupt, answered)
/// so the analyst LLM can learn from the interaction shape, not just the
/// content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolResultLabel {
    Ok,
    Error,
    Denied,
    Interrupted,
    Answered,
}

impl ToolResultLabel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Denied => "denied by user",
            Self::Interrupted => "interrupted by user",
            Self::Answered => "user answered",
        }
    }
}

fn classify_tool_result(content: &str, is_error: bool) -> ToolResultLabel {
    // Order matters — these markers are highly specific and don't overlap.
    if content.contains("The user doesn't want to proceed with this tool use") {
        return ToolResultLabel::Denied;
    }
    if content.contains("[Request interrupted by user") {
        return ToolResultLabel::Interrupted;
    }
    if content.starts_with("User has answered your question") {
        return ToolResultLabel::Answered;
    }
    if is_error {
        ToolResultLabel::Error
    } else {
        ToolResultLabel::Ok
    }
}

fn render_tool_result(item: &Value, tool_use_names: &HashMap<String, String>, out: &mut String) {
    let id = item
        .get("tool_use_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let tool_name = tool_use_names
        .get(id)
        .cloned()
        .unwrap_or_else(|| "unknown tool".to_string());
    let is_error = item
        .get("is_error")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let body = stringify_tool_result_content(item.get("content"));
    let label = classify_tool_result(&body, is_error);

    // For AskUserQuestion answers, render as a "User response" header rather
    // than burying the answer inside a "Tool result" frame — the analyst
    // signal is "what did the user pick", not "the AskUserQuestion tool ran".
    if label == ToolResultLabel::Answered && tool_name == "AskUserQuestion" {
        let _ = writeln!(out, "## User response");
        out.push('\n');
        write_fenced(out, "", &body);
        out.push('\n');
        return;
    }

    let _ = writeln!(out, "**Tool result ({tool_name}, {}):**", label.as_str());
    out.push('\n');
    write_fenced(out, "", &body);
    out.push('\n');
}

fn stringify_tool_result_content(content: Option<&Value>) -> String {
    let Some(content) = content else {
        return String::new();
    };
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    if let Some(items) = content.as_array() {
        let parts: Vec<String> = items
            .iter()
            .filter_map(|i| match i.get("type").and_then(Value::as_str) {
                Some("text") => i.get("text").and_then(Value::as_str).map(str::to_string),
                _ => None,
            })
            .collect();
        return parts.join("\n");
    }
    serde_json::to_string_pretty(content).unwrap_or_default()
}

/// Writes `body` inside a fenced code block whose fence-length adapts to any
/// embedded backtick fences in the body — we always pick a fence that the
/// body cannot break out of.
fn write_fenced(out: &mut String, lang: &str, body: &str) {
    let mut fence_len = 3usize;
    let mut probe = "```".to_string();
    while body.contains(&probe) {
        fence_len += 1;
        probe.push('`');
    }
    let fence = "`".repeat(fence_len);
    out.push_str(&fence);
    out.push_str(lang);
    out.push('\n');
    out.push_str(body.trim_end_matches('\n'));
    out.push('\n');
    out.push_str(&fence);
    out.push('\n');
}

fn render_attachment_event(event: &Event, out: &mut String) {
    let Some(attachment) = event.raw.get("attachment") else {
        return;
    };
    let kind = attachment
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let summary = match kind {
        "deferred_tools_delta" => {
            let added = attachment
                .get("addedNames")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            let removed = attachment
                .get("removedNames")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            format!("added {added}, removed {removed}")
        }
        "skill_listing" => {
            let count = attachment
                .get("skillCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let initial = attachment
                .get("isInitial")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if initial {
                format!("{count} skills (initial)")
            } else {
                format!("{count} skills")
            }
        }
        "todo_reminder" => {
            let items = attachment
                .get("itemCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            format!("{items} items")
        }
        _ => String::new(),
    };
    if summary.is_empty() {
        let _ = writeln!(out, "> **Attachment ({kind}).**\n");
    } else {
        let _ = writeln!(out, "> **Attachment ({kind}):** {summary}\n");
    }
}

fn render_permission_mode_event(event: &Event, out: &mut String) {
    let mode = event
        .raw
        .get("permissionMode")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let _ = writeln!(out, "> _Permission mode: {mode}_\n");
}

fn render_system_event(event: &Event, out: &mut String) {
    let subtype = event.raw.get("subtype").and_then(Value::as_str);
    match subtype {
        Some("turn_duration") => {
            let duration_ms = event
                .raw
                .get("durationMs")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let messages = event
                .raw
                .get("messageCount")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let _ = writeln!(
                out,
                "> _Turn duration: {duration_ms}ms, messages: {messages}_\n"
            );
        }
        Some(other) => {
            let _ = writeln!(out, "> _System event: {other}_\n");
        }
        None => {
            let _ = writeln!(out, "> _System event_\n");
        }
    }
}

fn render_summary_event(event: &Event, out: &mut String) {
    let summary = event
        .raw
        .get("summary")
        .and_then(Value::as_str)
        .unwrap_or("");
    if summary.is_empty() {
        return;
    }
    let _ = writeln!(out, "> **Summary:** {summary}\n");
}

// ---------------------------------------------------------------------------
// Whitespace
// ---------------------------------------------------------------------------

fn normalise_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_blanks = 0usize;
    let mut produced_any = false;
    for line in s.split('\n') {
        let trimmed = line.trim_end_matches([' ', '\t']);
        if trimmed.is_empty() {
            pending_blanks += 1;
        } else {
            if produced_any {
                // Cap accumulated blank lines at 2 (= 3 newlines including
                // the line terminator before the new content).
                for _ in 0..pending_blanks.min(2) {
                    out.push('\n');
                }
            }
            out.push_str(trimmed);
            out.push('\n');
            produced_any = true;
            pending_blanks = 0;
        }
    }
    if !produced_any {
        return String::new();
    }
    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn opts<'a>() -> RenderOptions<'a> {
        RenderOptions {
            project_slug: "-Users-jky-foo",
            session_uuid: "u-1",
            exclude_system: false,
        }
    }

    fn line(value: serde_json::Value) -> String {
        serde_json::to_string(&value).unwrap() + "\n"
    }

    fn render_lines(lines: &[serde_json::Value]) -> String {
        let buf: String = lines.iter().cloned().map(line).collect();
        render(buf.as_bytes(), opts()).unwrap()
    }

    fn render_lines_with(lines: &[serde_json::Value], options: RenderOptions<'_>) -> String {
        let buf: String = lines.iter().cloned().map(line).collect();
        render(buf.as_bytes(), options).unwrap()
    }

    #[test]
    fn empty_input_renders_just_frontmatter() {
        let out = render(b"", opts()).unwrap();
        assert!(out.starts_with("---\n"));
        assert!(out.contains("project_slug: -Users-jky-foo"));
        assert!(out.contains("event_count: 0"));
    }

    #[test]
    fn frontmatter_decodes_project_cwd_from_slug() {
        let out = render(b"", opts()).unwrap();
        assert!(out.contains("project_cwd: /Users/jky/foo"), "got: {out}");
    }

    #[test]
    fn frontmatter_session_id_falls_back_to_session_uuid() {
        let out = render(b"", opts()).unwrap();
        assert!(out.contains("session_id: u-1"));
    }

    #[test]
    fn frontmatter_picks_up_session_id_from_first_event() {
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "sessionId": "real-session",
            "timestamp": "2026-05-03T03:14:22Z",
            "message": { "content": "hi" },
        })]);
        assert!(out.contains("session_id: real-session"));
    }

    #[test]
    fn frontmatter_includes_timestamps_and_event_count() {
        let out = render_lines(&[
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-05-03T03:14:22Z",
                "message": { "content": "hi" },
            }),
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-05-03T04:00:00Z",
                "message": { "content": "second" },
            }),
        ]);
        assert!(out.contains("first_event: 2026-05-03T03:14:22Z"));
        assert!(out.contains("last_event: 2026-05-03T04:00:00Z"));
        assert!(out.contains("event_count: 2"));
    }

    #[test]
    fn frontmatter_promotes_ai_title() {
        let out = render_lines(&[
            serde_json::json!({"type": "ai-title", "aiTitle": "First title"}),
            serde_json::json!({"type": "ai-title", "aiTitle": "Second title"}),
        ]);
        assert!(out.contains("ai_title: Second title"), "got: {out}");
    }

    #[test]
    fn frontmatter_custom_title_overrides_ai_title() {
        let out = render_lines(&[
            serde_json::json!({"type": "ai-title", "aiTitle": "robot"}),
            serde_json::json!({"type": "custom-title", "customTitle": "human"}),
        ]);
        assert!(out.contains("ai_title: human"));
    }

    #[test]
    fn frontmatter_omits_optional_fields_when_absent() {
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "message": { "content": "hi" },
        })]);
        assert!(!out.contains("git_branch:"));
        assert!(!out.contains("version:"));
        assert!(!out.contains("entrypoint:"));
        assert!(out.contains("ai_title: null"));
    }

    #[test]
    fn frontmatter_picks_up_branch_and_version_from_richer_events() {
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "gitBranch": "issue-665",
            "version": "1.2.3",
            "entrypoint": "claude",
            "message": { "content": [{"type":"text","text":"hi"}] },
        })]);
        assert!(out.contains("git_branch: issue-665"));
        assert!(out.contains("version: 1.2.3"));
        assert!(out.contains("entrypoint: claude"));
    }

    #[test]
    fn frontmatter_quotes_branch_with_special_chars() {
        // Branch with a colon would break unquoted YAML; serde_yaml must quote it.
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "gitBranch": "feat: with-colon",
            "message": { "content": [{"type":"text","text":"hi"}] },
        })]);
        // Round-trip through serde_yaml to assert validity.
        let body = out.split("---\n").nth(1).unwrap();
        let _: serde_yaml::Value = serde_yaml::from_str(body).unwrap();
    }

    #[test]
    fn user_string_content_renders_with_timestamp() {
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "timestamp": "2026-05-03T03:14:22Z",
            "message": { "content": "hello world" },
        })]);
        assert!(
            out.contains("## User · 2026-05-03T03:14:22Z\n\nhello world"),
            "got: {out}"
        );
    }

    #[test]
    fn user_array_text_blocks_concatenate() {
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "message": { "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"},
            ]},
        })]);
        assert!(out.contains("first\n\nsecond"), "got: {out}");
    }

    #[test]
    fn assistant_text_blocks_share_one_heading() {
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"},
            ]},
        })]);
        let count = out.matches("## Assistant").count();
        assert_eq!(count, 1, "exactly one heading per assistant turn: {out}");
    }

    #[test]
    fn thinking_block_uses_details_summary() {
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                {"type": "thinking", "thinking": "ponder this", "signature": "abc"},
            ]},
        })]);
        assert!(out.contains("<details>\n<summary>Thinking</summary>"));
        assert!(out.contains("ponder this"));
        assert!(!out.contains("abc"), "signature must not leak");
    }

    #[test]
    fn tool_use_renders_input_as_json_block() {
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                {"type": "tool_use", "id": "t1", "name": "Read", "input": {"file_path": "/x"}},
            ]},
        })]);
        assert!(out.contains("### Tool call: `Read`"));
        assert!(out.contains("\"file_path\": \"/x\""));
    }

    #[test]
    fn bash_tool_uses_bash_language_hint() {
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}},
            ]},
        })]);
        assert!(out.contains("```bash"), "got: {out}");
    }

    #[test]
    fn agent_tool_renders_only_prompt() {
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                {"type": "tool_use", "id": "t1", "name": "Agent", "input": {
                    "description": "do a thing",
                    "subagent_type": "Plan",
                    "prompt": "plan this for me",
                }},
            ]},
        })]);
        assert!(out.contains("### Tool call: `Agent`"));
        assert!(out.contains("plan this for me"));
        assert!(
            !out.contains("do a thing"),
            "description must be hidden: {out}"
        );
        assert!(!out.contains("subagent_type"));
    }

    #[test]
    fn tool_result_pairs_to_tool_use_by_id() {
        let out = render_lines(&[
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [
                    {"type": "tool_use", "id": "abc", "name": "Bash", "input": {"command": "ls"}},
                ]},
            }),
            serde_json::json!({
                "type": "user",
                "message": { "content": [
                    {"type": "tool_result", "tool_use_id": "abc", "content": "file1\nfile2"},
                ]},
            }),
        ]);
        assert!(out.contains("**Tool result (Bash, ok):**"), "got: {out}");
        assert!(out.contains("file1\nfile2"));
    }

    #[test]
    fn tool_result_without_known_use_renders_unknown_label() {
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "message": { "content": [
                {"type": "tool_result", "tool_use_id": "dangling", "content": "x"},
            ]},
        })]);
        assert!(out.contains("Tool result (unknown tool, ok)"), "got: {out}");
    }

    #[test]
    fn tool_result_error_renders_error_label() {
        let out = render_lines(&[
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [
                    {"type": "tool_use", "id": "x", "name": "Bash", "input": {"command": "no"}},
                ]},
            }),
            serde_json::json!({
                "type": "user",
                "message": { "content": [
                    {"type": "tool_result", "tool_use_id": "x", "content": "boom", "is_error": true},
                ]},
            }),
        ]);
        assert!(out.contains("(Bash, error)"));
    }

    #[test]
    fn tool_result_array_text_blocks_join() {
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "message": { "content": [
                {"type": "tool_result", "tool_use_id": "x", "content": [
                    {"type": "text", "text": "alpha"},
                    {"type": "text", "text": "beta"},
                ]},
            ]},
        })]);
        assert!(out.contains("alpha\nbeta"));
    }

    #[test]
    fn fence_widens_when_body_contains_triple_backtick() {
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "message": { "content": [
                {"type": "tool_result", "tool_use_id": "x", "content": "before\n```\ninner\n```\nafter"},
            ]},
        })]);
        // 4-backtick fence opens.
        assert!(out.contains("````\n"), "expected widened fence: {out}");
    }

    #[test]
    fn persisted_output_envelope_is_rendered_verbatim() {
        let envelope = "<persisted-output>\nOutput too large (12.3KB). Full output saved to: /tmp/x\nPreview (first 2KB):\nfirst bytes...\n</persisted-output>";
        // Persisted-output appearing inside a tool_result content string. It's
        // a system-envelope, so it must be preserved as-is in the result fence,
        // not stripped or treated as a system reminder.
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "message": { "content": [
                {"type": "tool_result", "tool_use_id": "x", "content": envelope},
            ]},
        })]);
        assert!(out.contains("Output too large (12.3KB)"));
        assert!(out.contains("Preview (first 2KB)"));
    }

    #[test]
    fn system_reminder_in_user_text_renders_as_blockquote() {
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "message": { "content": "<system-reminder>be careful</system-reminder>" },
        })]);
        assert!(
            out.contains("> **System reminder:** be careful"),
            "got: {out}"
        );
    }

    #[test]
    fn exclude_system_hides_system_reminder_in_user_text() {
        let mut o = opts();
        o.exclude_system = true;
        let out = render_lines_with(
            &[serde_json::json!({
                "type": "user",
                "message": { "content": "real prompt<system-reminder>noise</system-reminder>" },
            })],
            o,
        );
        assert!(out.contains("real prompt"));
        assert!(!out.contains("System reminder"), "got: {out}");
        assert!(!out.contains("noise"));
    }

    #[test]
    fn attachment_event_renders_with_summary() {
        let out = render_lines(&[serde_json::json!({
            "type": "attachment",
            "attachment": {
                "type": "skill_listing",
                "skillCount": 7,
                "isInitial": true,
            },
        })]);
        assert!(out.contains("**Attachment (skill_listing):** 7 skills (initial)"));
    }

    #[test]
    fn attachment_event_hidden_under_exclude_system() {
        let mut o = opts();
        o.exclude_system = true;
        let out = render_lines_with(
            &[serde_json::json!({
                "type": "attachment",
                "attachment": {"type": "skill_listing", "skillCount": 1},
            })],
            o,
        );
        assert!(!out.contains("Attachment"));
    }

    #[test]
    fn attachment_event_unknown_subtype_uses_generic_label() {
        let out = render_lines(&[serde_json::json!({
            "type": "attachment",
            "attachment": {"type": "novel_kind"},
        })]);
        assert!(out.contains("**Attachment (novel_kind)"), "got: {out}");
    }

    #[test]
    fn attachment_deferred_tools_delta_summary() {
        let out = render_lines(&[serde_json::json!({
            "type": "attachment",
            "attachment": {
                "type": "deferred_tools_delta",
                "addedNames": ["a","b","c"],
                "removedNames": [],
            },
        })]);
        assert!(out.contains("added 3, removed 0"));
    }

    #[test]
    fn attachment_todo_reminder_summary() {
        let out = render_lines(&[serde_json::json!({
            "type": "attachment",
            "attachment": {"type": "todo_reminder", "itemCount": 4},
        })]);
        assert!(out.contains("4 items"));
    }

    #[test]
    fn permission_mode_event_renders_blockquote() {
        let out = render_lines(&[serde_json::json!({
            "type": "permission-mode",
            "permissionMode": "acceptEdits",
        })]);
        assert!(out.contains("> _Permission mode: acceptEdits_"));
    }

    #[test]
    fn system_event_turn_duration_renders_blockquote() {
        let out = render_lines(&[serde_json::json!({
            "type": "system",
            "subtype": "turn_duration",
            "durationMs": 1234,
            "messageCount": 5,
        })]);
        assert!(out.contains("Turn duration: 1234ms, messages: 5"));
    }

    #[test]
    fn system_event_unknown_subtype_renders_blockquote() {
        let out = render_lines(&[serde_json::json!({
            "type": "system",
            "subtype": "novel_kind",
        })]);
        assert!(out.contains("System event: novel_kind"));
    }

    #[test]
    fn system_event_without_subtype_renders_blockquote() {
        let out = render_lines(&[serde_json::json!({
            "type": "system",
        })]);
        assert!(out.contains("System event"));
    }

    #[test]
    fn summary_event_renders_blockquote() {
        let out = render_lines(&[serde_json::json!({
            "type": "summary",
            "summary": "compaction occurred",
        })]);
        assert!(out.contains("**Summary:** compaction occurred"));
    }

    #[test]
    fn summary_event_with_empty_text_is_skipped() {
        let out = render_lines(&[serde_json::json!({
            "type": "summary",
            "summary": "",
        })]);
        assert!(!out.contains("Summary"));
    }

    #[test]
    fn unknown_event_renders_defensive_blockquote() {
        let out = render_lines(&[serde_json::json!({
            "type": "novel-event",
            "data": "x",
        })]);
        assert!(out.contains("**Unknown event:** `novel-event`"));
    }

    #[test]
    fn unknown_event_hidden_under_exclude_system() {
        let mut o = opts();
        o.exclude_system = true;
        let out = render_lines_with(&[serde_json::json!({"type": "novel-event"})], o);
        assert!(!out.contains("Unknown event"));
    }

    #[test]
    fn always_hidden_plumbing_events_never_render() {
        // file-history-snapshot, queue-operation, last-prompt, etc.
        let out = render_lines(&[
            serde_json::json!({"type": "file-history-snapshot"}),
            serde_json::json!({"type": "queue-operation"}),
            serde_json::json!({"type": "last-prompt"}),
            serde_json::json!({"type": "progress"}),
            serde_json::json!({"type": "pr-link"}),
            serde_json::json!({"type": "agent-name"}),
            serde_json::json!({"type": "worktree-state"}),
        ]);
        // None of these should leak into the body — the only content is frontmatter.
        let body = out.split("---\n").nth(2).unwrap_or("").trim();
        assert!(body.is_empty(), "expected empty body, got: {body}");
    }

    #[test]
    fn malformed_jsonl_line_is_skipped() {
        let mut buf = String::new();
        buf.push_str("{not valid json\n");
        buf.push_str(&line(serde_json::json!({
            "type": "user",
            "message": { "content": "ok" },
        })));
        let out = render(buf.as_bytes(), opts()).unwrap();
        assert!(out.contains("ok"));
        assert!(out.contains("event_count: 1"));
    }

    #[test]
    fn trailing_partial_line_is_tolerated() {
        // Snapshot-EOF may produce a partial JSON line at the tail.
        let mut buf = String::new();
        buf.push_str(&line(serde_json::json!({
            "type": "user",
            "message": { "content": "complete" },
        })));
        buf.push_str("{\"type\": \"user\", \"message\": {\"content\": \"par");
        let out = render(buf.as_bytes(), opts()).unwrap();
        assert!(out.contains("complete"));
        assert!(!out.contains("par\""), "partial line should not leak");
    }

    #[test]
    fn round_trip_user_and_assistant_text_preserved_verbatim() {
        // The behavioural-coaching property: every user prompt and every
        // assistant text response must appear verbatim in the rendered markdown.
        let prompts: Vec<String> = (0..5).map(|i| format!("user prompt number {i}")).collect();
        let responses: Vec<String> = (0..5)
            .map(|i| format!("assistant says response {i}"))
            .collect();
        let mut events = Vec::new();
        for i in 0..5 {
            events.push(serde_json::json!({
                "type": "user",
                "message": { "content": prompts[i] },
            }));
            events.push(serde_json::json!({
                "type": "assistant",
                "message": { "content": [{"type": "text", "text": responses[i]}] },
            }));
        }
        let out = render_lines(&events);
        for p in &prompts {
            assert!(out.contains(p), "missing user prompt: {p}");
        }
        for r in &responses {
            assert!(out.contains(r), "missing assistant text: {r}");
        }
    }

    #[test]
    fn whitespace_normaliser_caps_blank_runs_to_two() {
        let s = "a\n\n\n\n\nb\n";
        let out = normalise_whitespace(s);
        assert_eq!(out, "a\n\n\nb\n");
    }

    #[test]
    fn whitespace_normaliser_trims_trailing_spaces() {
        let s = "hello   \nworld\t\n";
        let out = normalise_whitespace(s);
        assert_eq!(out, "hello\nworld\n");
    }

    #[test]
    fn whitespace_normaliser_ensures_trailing_newline() {
        let s = "abc";
        let out = normalise_whitespace(s);
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn humanise_tag_capitalises_and_replaces_separators() {
        assert_eq!(humanise_tag("system-reminder"), "System reminder");
        assert_eq!(humanise_tag("ide_opened_file"), "Ide opened file");
        assert_eq!(humanise_tag(""), "");
    }

    #[test]
    fn split_system_envelopes_preserves_order() {
        let parts = split_system_envelopes(
            "head<system-reminder>r1</system-reminder>mid<command-name>c</command-name>tail",
        );
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[0], (None, "head".to_string()));
        assert_eq!(
            parts[1],
            (Some("system-reminder".to_string()), "r1".to_string())
        );
        assert_eq!(parts[2], (None, "mid".to_string()));
        assert_eq!(
            parts[3],
            (Some("command-name".to_string()), "c".to_string())
        );
        assert_eq!(parts[4], (None, "tail".to_string()));
    }

    #[test]
    fn split_system_envelopes_handles_unterminated_tag() {
        let parts = split_system_envelopes("head<system-reminder>uncl");
        assert_eq!(parts[0], (None, "head".to_string()));
        // Unterminated tag stays as plain text.
        assert!(parts[1].0.is_none());
        assert!(parts[1].1.contains("<system-reminder>"));
    }

    #[test]
    fn assistant_event_with_no_content_is_silent() {
        let out = render_lines(&[serde_json::json!({"type": "assistant"})]);
        let body = out.split("---\n").nth(2).unwrap_or("").trim();
        assert!(body.is_empty());
    }

    #[test]
    fn assistant_event_with_object_content_is_silent() {
        // Defensive: content is present but not an array — let-else returns.
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": {"unexpected": true} },
        })]);
        let body = out.split("---\n").nth(2).unwrap_or("").trim();
        assert!(body.is_empty());
    }

    #[test]
    fn assistant_event_with_empty_content_array_is_silent() {
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": [] },
        })]);
        let body = out.split("---\n").nth(2).unwrap_or("").trim();
        assert!(body.is_empty());
    }

    #[test]
    fn assistant_text_block_without_text_field_is_skipped() {
        // text block with `.text` missing — `if let Some(text)` None branch.
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                {"type": "text"},
                {"type": "text", "text": "kept"},
            ]},
        })]);
        assert!(out.contains("kept"));
    }

    #[test]
    fn assistant_thinking_block_without_thinking_field_is_skipped() {
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                {"type": "thinking", "signature": "x"},
                {"type": "text", "text": "kept"},
            ]},
        })]);
        assert!(out.contains("kept"));
        assert!(!out.contains("<details>"));
    }

    #[test]
    fn assistant_event_with_unknown_inner_block_renders_marker() {
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": [{"type": "novel_block"}] },
        })]);
        assert!(out.contains("Unknown assistant block"));
    }

    #[test]
    fn user_event_with_pure_tool_results_emits_no_user_heading() {
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "message": { "content": [
                {"type": "tool_result", "tool_use_id": "x", "content": "result body"},
            ]},
        })]);
        assert!(!out.contains("## User"), "got: {out}");
        assert!(out.contains("Tool result"));
    }

    #[test]
    fn user_event_without_message_is_silent() {
        let out = render_lines(&[serde_json::json!({"type": "user"})]);
        let body = out.split("---\n").nth(2).unwrap_or("").trim();
        assert!(body.is_empty());
    }

    #[test]
    fn user_event_with_only_empty_text_is_silent() {
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "message": { "content": [{"type":"text","text":"   "}] },
        })]);
        let body = out.split("---\n").nth(2).unwrap_or("").trim();
        assert!(body.is_empty());
    }

    #[test]
    fn tool_use_with_string_input_renders_string_directly() {
        // Some tool inputs are bare strings (rare, but observed).
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                {"type": "tool_use", "id": "x", "name": "Bash", "input": "ls -la"},
            ]},
        })]);
        assert!(out.contains("ls -la"));
    }

    #[test]
    fn user_event_with_empty_array_content_is_silent() {
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "message": { "content": [] },
        })]);
        let body = out.split("---\n").nth(2).unwrap_or("").trim();
        assert!(body.is_empty(), "got: {body}");
    }

    #[test]
    fn user_event_with_object_content_falls_through_to_empty_text() {
        // Defensive: an object-shaped `content` is neither string nor array;
        // collect_user_text_blocks returns Vec::new(), the renderer is silent.
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "message": { "content": {"unexpected": "shape"} },
        })]);
        let body = out.split("---\n").nth(2).unwrap_or("").trim();
        assert!(body.is_empty());
    }

    #[test]
    fn user_array_with_non_text_block_is_filtered() {
        // tool_use blocks should be skipped (returns None from filter_map),
        // and a user event with no usable text blocks is silent.
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "message": { "content": [
                {"type": "image", "source": {}},
            ]},
        })]);
        let body = out.split("---\n").nth(2).unwrap_or("").trim();
        assert!(body.is_empty(), "got: {body}");
    }

    #[test]
    fn single_line_summary_folds_multi_line_into_details() {
        // Two-line system reminder body: head on the first line, rest in <details>.
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "message": { "content": "<system-reminder>headline\nmore detail one\nmore detail two</system-reminder>" },
        })]);
        assert!(
            out.contains("> **System reminder:** headline"),
            "got: {out}"
        );
        assert!(out.contains("<details>"));
        assert!(out.contains("> more detail one"));
        assert!(out.contains("> more detail two"));
    }

    #[test]
    fn agent_tool_without_prompt_renders_null() {
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                {"type": "tool_use", "id": "t1", "name": "Agent", "input": {
                    "description": "no prompt here",
                    "subagent_type": "Plan",
                }},
            ]},
        })]);
        assert!(out.contains("### Tool call: `Agent`"));
        // The fenced block is JSON null when no prompt was supplied.
        assert!(out.contains("null"), "got: {out}");
    }

    #[test]
    fn assistant_text_block_with_only_whitespace_is_silent() {
        // The trimmed branch must be reachable for both text and thinking
        // sub-blocks (covers the inner `if !trimmed.is_empty()` false arm).
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                {"type": "text", "text": "   "},
                {"type": "thinking", "thinking": "  ", "signature": "x"},
            ]},
        })]);
        // The heading is emitted but no inner content follows.
        assert!(out.contains("## Assistant"));
        assert!(!out.contains("<details>"));
    }

    #[test]
    fn tool_result_with_missing_content_renders_empty_block() {
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "message": { "content": [
                {"type": "tool_result", "tool_use_id": "x"},
            ]},
        })]);
        assert!(out.contains("Tool result"));
    }

    #[test]
    fn tool_result_array_skips_non_text_blocks() {
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "message": { "content": [
                {"type": "tool_result", "tool_use_id": "x", "content": [
                    {"type": "image", "source": {}},
                    {"type": "text", "text": "kept"},
                ]},
            ]},
        })]);
        assert!(out.contains("kept"));
    }

    #[test]
    fn tool_result_with_object_content_falls_back_to_pretty_json() {
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "message": { "content": [
                {"type": "tool_result", "tool_use_id": "x", "content": {"ok": true}},
            ]},
        })]);
        assert!(out.contains("\"ok\""), "got: {out}");
    }

    #[test]
    fn attachment_event_without_attachment_field_is_silent() {
        let out = render_lines(&[serde_json::json!({"type": "attachment"})]);
        let body = out.split("---\n").nth(2).unwrap_or("").trim();
        assert!(body.is_empty(), "got: {body}");
    }

    #[test]
    fn normalise_whitespace_returns_empty_for_empty_input() {
        assert_eq!(normalise_whitespace(""), "");
    }

    #[test]
    fn normalise_whitespace_collapses_blank_only_input() {
        // Only blank lines: produced_any stays false; output is empty.
        assert_eq!(normalise_whitespace("\n\n  \n"), "");
    }

    // -----------------------------------------------------------------
    // User-interaction capture: AskUserQuestion, denial, interrupt
    // -----------------------------------------------------------------

    #[test]
    fn ask_user_question_renders_structured_question_block() {
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                {"type": "tool_use", "id": "q1", "name": "AskUserQuestion", "input": {
                    "questions": [{
                        "header": "Timing",
                        "question": "Should ordering use standard comparisons?",
                        "options": [
                            {"label": "Standard (Recommended)", "description": "Use native cmp"},
                            {"label": "Constant-time", "description": "More complex"},
                        ],
                        "multiSelect": false,
                    }],
                }},
            ]},
        })]);
        assert!(out.contains("### Agent question: Timing"), "got: {out}");
        assert!(out.contains("Should ordering use standard comparisons?"));
        assert!(out.contains("- **Standard (Recommended)** — Use native cmp"));
        assert!(out.contains("- **Constant-time** — More complex"));
        // Generic tool-call frame is suppressed.
        assert!(!out.contains("### Tool call: `AskUserQuestion`"));
    }

    #[test]
    fn ask_user_question_marks_multi_select_questions() {
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                {"type": "tool_use", "id": "q1", "name": "AskUserQuestion", "input": {
                    "questions": [{
                        "header": "Tags",
                        "question": "Pick categories",
                        "options": [{"label": "A"}, {"label": "B"}],
                        "multiSelect": true,
                    }],
                }},
            ]},
        })]);
        assert!(out.contains("### Agent question: Tags (multi-select)"));
        // Option without description renders bare.
        assert!(out.contains("- **A**\n"), "got: {out}");
    }

    #[test]
    fn ask_user_question_without_options_renders_question_only() {
        // Cover the `if let Some(options)` None arm — a question with no
        // options field must still emit the header and prompt text.
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                {"type": "tool_use", "id": "q1", "name": "AskUserQuestion", "input": {
                    "questions": [{
                        "header": "Free-form",
                        "question": "What's your reasoning?",
                    }],
                }},
            ]},
        })]);
        assert!(out.contains("### Agent question: Free-form"));
        assert!(out.contains("What's your reasoning?"));
    }

    #[test]
    fn ask_user_question_question_with_missing_fields_uses_defaults() {
        // Question with no header → "Question"; no question text → skipped;
        // option with no label → bare "**" (defensive, never crashes).
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                {"type": "tool_use", "id": "q1", "name": "AskUserQuestion", "input": {
                    "questions": [{
                        "options": [{}],
                    }],
                }},
            ]},
        })]);
        assert!(out.contains("### Agent question: Question"), "got: {out}");
    }

    #[test]
    fn defensive_defaults_apply_when_metadata_fields_are_missing() {
        // Sweeps the `unwrap_or(<default>)` branches across the renderer:
        // attachment with no `.type`, permission-mode with no `.permissionMode`,
        // tool_use with no `.id`/`.name`/`.input`, tool_result with no
        // `.tool_use_id`. None should panic; all should render with their
        // documented fallback strings.
        let out = render_lines(&[
            serde_json::json!({"type": "attachment", "attachment": {}}),
            serde_json::json!({"type": "permission-mode"}),
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [
                    {"type": "tool_use"},
                ]},
            }),
            serde_json::json!({
                "type": "user",
                "message": { "content": [
                    {"type": "tool_result"},
                ]},
            }),
        ]);
        assert!(out.contains("Attachment (unknown)"), "got: {out}");
        assert!(out.contains("Permission mode: unknown"));
        assert!(out.contains("### Tool call:"));
        // No `tool_use_id` → "unknown tool" label, never a panic.
        assert!(out.contains("unknown tool"));
    }

    #[test]
    fn ask_user_question_with_empty_questions_falls_back() {
        let out = render_lines(&[serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                {"type": "tool_use", "id": "q1", "name": "AskUserQuestion", "input": {}},
            ]},
        })]);
        // Defensive: still emits a header so the call is not silently dropped.
        assert!(out.contains("### Agent question"), "got: {out}");
    }

    #[test]
    fn ask_user_question_answer_renders_as_user_response() {
        let out = render_lines(&[
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [
                    {"type": "tool_use", "id": "q1", "name": "AskUserQuestion", "input": {
                        "questions": [{"header": "X", "question": "?", "options": [{"label": "A"}], "multiSelect": false}],
                    }},
                ]},
            }),
            serde_json::json!({
                "type": "user",
                "message": { "content": [
                    {"type": "tool_result", "tool_use_id": "q1",
                     "content": "User has answered your questions: \"?\"=\"A\". You can now continue."},
                ]},
            }),
        ]);
        assert!(out.contains("## User response"), "got: {out}");
        assert!(out.contains("\"?\"=\"A\""));
        // We do NOT emit "Tool result (AskUserQuestion, ...)" for answers.
        assert!(!out.contains("Tool result (AskUserQuestion"), "got: {out}");
    }

    #[test]
    fn tool_denial_renders_with_denied_label() {
        let out = render_lines(&[
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [
                    {"type": "tool_use", "id": "x", "name": "Edit", "input": {"file_path": "/x"}},
                ]},
            }),
            serde_json::json!({
                "type": "user",
                "message": { "content": [
                    {"type": "tool_result", "tool_use_id": "x", "is_error": true,
                     "content": "The user doesn't want to proceed with this tool use. The tool use was rejected (eg. if it was a file edit, the new_string was NOT written to the file). STOP what you are doing."},
                ]},
            }),
        ]);
        assert!(
            out.contains("**Tool result (Edit, denied by user):**"),
            "got: {out}"
        );
        // Body text remains preserved verbatim for context.
        assert!(out.contains("doesn't want to proceed"));
    }

    #[test]
    fn tool_denial_with_user_reason_preserves_reason() {
        let out = render_lines(&[serde_json::json!({
            "type": "user",
            "message": { "content": [
                {"type": "tool_result", "tool_use_id": "x", "is_error": true,
                 "content": "The user doesn't want to proceed with this tool use. The user provided the following reason for the rejection: not now, run after lunch."},
            ]},
        })]);
        assert!(out.contains("denied by user"));
        assert!(out.contains("not now, run after lunch"));
    }

    #[test]
    fn tool_interrupt_renders_with_interrupted_label() {
        let out = render_lines(&[
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [
                    {"type": "tool_use", "id": "x", "name": "Bash", "input": {"command": "sleep 9999"}},
                ]},
            }),
            serde_json::json!({
                "type": "user",
                "message": { "content": [
                    {"type": "tool_result", "tool_use_id": "x", "is_error": true,
                     "content": "Exit code 137\n[Request interrupted by user for tool use]"},
                ]},
            }),
        ]);
        assert!(
            out.contains("**Tool result (Bash, interrupted by user):**"),
            "got: {out}"
        );
        assert!(out.contains("Exit code 137"));
    }

    #[test]
    fn classify_tool_result_priorities() {
        // Each marker is checked independently of is_error so that — e.g. a
        // denial that's also flagged is_error still classifies as Denied.
        assert_eq!(
            classify_tool_result("The user doesn't want to proceed with this tool use", true),
            ToolResultLabel::Denied
        );
        assert_eq!(
            classify_tool_result("[Request interrupted by user for tool use]", true),
            ToolResultLabel::Interrupted
        );
        assert_eq!(
            classify_tool_result("User has answered your questions: \"q\"=\"a\"", false),
            ToolResultLabel::Answered
        );
        assert_eq!(
            classify_tool_result("plain success", false),
            ToolResultLabel::Ok
        );
        assert_eq!(
            classify_tool_result("syntax error in input", true),
            ToolResultLabel::Error
        );
    }

    #[test]
    fn tool_result_label_strings_are_distinct() {
        assert_eq!(ToolResultLabel::Ok.as_str(), "ok");
        assert_eq!(ToolResultLabel::Error.as_str(), "error");
        assert_eq!(ToolResultLabel::Denied.as_str(), "denied by user");
        assert_eq!(ToolResultLabel::Interrupted.as_str(), "interrupted by user");
        assert_eq!(ToolResultLabel::Answered.as_str(), "user answered");
    }

    #[test]
    fn one_of_each_event_type_snapshot() {
        let events = vec![
            serde_json::json!({
                "type": "ai-title",
                "aiTitle": "Test Session",
            }),
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-05-03T03:14:22Z",
                "gitBranch": "issue-665",
                "version": "1.2.3",
                "entrypoint": "claude",
                "message": { "content": "Hello, can you help?<system-reminder>be terse</system-reminder>" },
            }),
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [
                    {"type": "thinking", "thinking": "Let me think...", "signature": "sig"},
                    {"type": "text", "text": "Sure, here's a thought."},
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}},
                ]},
            }),
            serde_json::json!({
                "type": "user",
                "message": { "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "file1\nfile2"},
                ]},
            }),
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [
                    {"type": "tool_use", "id": "t2", "name": "Agent", "input": {
                        "description": "do a thing",
                        "subagent_type": "Plan",
                        "prompt": "plan this",
                    }},
                ]},
            }),
            serde_json::json!({
                "type": "attachment",
                "attachment": {"type": "skill_listing", "skillCount": 3, "isInitial": false},
            }),
            serde_json::json!({
                "type": "permission-mode",
                "permissionMode": "default",
            }),
            serde_json::json!({
                "type": "system",
                "subtype": "turn_duration",
                "durationMs": 1500,
                "messageCount": 2,
            }),
            serde_json::json!({
                "type": "summary",
                "summary": "compaction",
            }),
            serde_json::json!({"type": "file-history-snapshot"}),
            serde_json::json!({"type": "queue-operation"}),
        ];
        let out = render_lines(&events);
        insta::assert_snapshot!("markdown_one_of_each_event_type", out);
    }

    #[test]
    fn one_of_each_event_type_excluding_system_snapshot() {
        let events = vec![
            serde_json::json!({
                "type": "user",
                "timestamp": "2026-05-03T03:14:22Z",
                "message": { "content": "real prompt<system-reminder>noise</system-reminder>" },
            }),
            serde_json::json!({
                "type": "assistant",
                "message": { "content": [{"type": "text", "text": "real reply"}] },
            }),
            serde_json::json!({
                "type": "attachment",
                "attachment": {"type": "skill_listing", "skillCount": 3},
            }),
            serde_json::json!({
                "type": "permission-mode",
                "permissionMode": "acceptEdits",
            }),
            serde_json::json!({
                "type": "system",
                "subtype": "turn_duration",
                "durationMs": 1500,
                "messageCount": 2,
            }),
            serde_json::json!({"type": "summary", "summary": "compacted"}),
            serde_json::json!({"type": "novel-event"}),
        ];
        let opts = RenderOptions {
            project_slug: "-Users-jky-foo",
            session_uuid: "u-2",
            exclude_system: true,
        };
        let out = render_lines_with(&events, opts);
        insta::assert_snapshot!("markdown_one_of_each_event_type_excluding_system", out);
    }
}
