//! Rendering of [`LogRecord`] lines as `oneline`, `json`, or `full`.

use super::Format;
use crate::request_log::{LogRecord, RecordKind, Source};

/// Renders one record for display.
///
/// `raw` is the verbatim on-disk JSON line; the `json` format returns it
/// unchanged so output is byte-identical to the file (composes with `jq`).
pub fn render(rec: &LogRecord, raw: &str, format: Format) -> String {
    match format {
        Format::Json => raw.to_string(),
        Format::Oneline => oneline(rec),
        Format::Full => full(rec),
    }
}

/// The `HH:MM:SS.mmm` portion of an RFC3339 timestamp, best effort.
fn short_time(ts: &str) -> &str {
    let after_t = ts.split_once('T').map_or(ts, |(_, t)| t);
    // Trim the timezone suffix (`Z` or `+hh:mm`).
    let end = after_t
        .find(['Z', '+'])
        .or_else(|| after_t.rfind('-'))
        .unwrap_or(after_t.len());
    &after_t[..end]
}

/// Lowercase string form of a [`Source`].
fn source_str(source: Option<Source>) -> &'static str {
    match source {
        Some(Source::Cli) => "cli",
        Some(Source::Mcp) => "mcp",
        Some(Source::Daemon) => "daemon",
        Some(Source::Unknown) | None => "-",
    }
}

/// One compact line per record.
fn oneline(rec: &LogRecord) -> String {
    let time = short_time(&rec.timestamp);
    match rec.kind {
        RecordKind::Http => {
            let service = rec.service.as_deref().unwrap_or("-");
            let method = rec.method.as_deref().unwrap_or("-");
            let status = rec
                .status_code
                .map_or_else(|| "ERR".to_string(), |c| c.to_string());
            let elapsed = rec
                .elapsed_ms
                .map_or_else(String::new, |ms| format!("{ms}ms"));
            let url = rec.url.as_deref().unwrap_or("");
            let daemon = if rec.via_daemon { " [daemon]" } else { "" };
            let err = rec
                .error
                .as_deref()
                .map_or_else(String::new, |e| format!("  error={e}"));
            format!(
                "{time}  http  {service:<14} {method:<6} {status:<4} {elapsed:>7}  {url}{daemon}{err}"
            )
        }
        RecordKind::Invocation | RecordKind::Unknown => {
            let source = source_str(rec.source);
            let command = if rec.command.is_empty() {
                rec.mcp_tool.clone().unwrap_or_else(|| "-".to_string())
            } else {
                rec.command.join(" ")
            };
            let exit = rec
                .exit_code
                .map_or_else(String::new, |c| format!(" exit={c}"));
            let dur = rec
                .duration_ms
                .map_or_else(String::new, |ms| format!(" {ms}ms"));
            format!("{time}  inv   {source:<14} {command}{exit}{dur}")
        }
    }
}

/// A labelled, multi-line block per record (pretty-printed JSON body).
fn full(rec: &LogRecord) -> String {
    // serde_json pretty is the most faithful "full" view; fall back to oneline
    // if (somehow) the record cannot be re-serialized.
    match serde_json::to_string_pretty(rec) {
        Ok(pretty) => format!("{pretty}\n"),
        Err(_) => oneline(rec),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn short_time_extracts_clock() {
        assert_eq!(short_time("2026-06-22T12:34:56.789Z"), "12:34:56.789");
        assert_eq!(short_time("2026-06-22T01:02:03.004+10:00"), "01:02:03.004");
    }

    #[test]
    fn json_format_is_verbatim() {
        let rec = LogRecord::default();
        let raw = r#"{"id":"x","kind":"http"}"#;
        assert_eq!(render(&rec, raw, Format::Json), raw);
    }

    #[test]
    fn oneline_http_includes_key_fields() {
        let mut rec = LogRecord {
            kind: RecordKind::Http,
            timestamp: "2026-06-22T12:34:56.789Z".to_string(),
            service: Some("jira".to_string()),
            method: Some("GET".to_string()),
            status_code: Some(200),
            elapsed_ms: Some(42),
            url: Some("https://x/rest/api/3/issue/A-1".to_string()),
            ..LogRecord::default()
        };
        let line = render(&rec, "", Format::Oneline);
        assert!(line.contains("http"));
        assert!(line.contains("jira"));
        assert!(line.contains("GET"));
        assert!(line.contains("200"));
        assert!(line.contains("42ms"));
        assert!(line.contains("issue/A-1"));

        rec.status_code = None;
        rec.error = Some("timeout".to_string());
        let line = render(&rec, "", Format::Oneline);
        assert!(line.contains("ERR"));
        assert!(line.contains("error=timeout"));
    }

    #[test]
    fn oneline_invocation_shows_command() {
        let rec = LogRecord {
            kind: RecordKind::Invocation,
            timestamp: "2026-06-22T12:34:56.789Z".to_string(),
            command: vec!["jira".to_string(), "read".to_string()],
            source: Some(Source::Cli),
            exit_code: Some(0),
            duration_ms: Some(120),
            ..LogRecord::default()
        };
        let line = render(&rec, "", Format::Oneline);
        assert!(line.contains("inv"));
        assert!(line.contains("cli"));
        assert!(line.contains("jira read"));
        assert!(line.contains("exit=0"));
        assert!(line.contains("120ms"));
    }

    #[test]
    fn oneline_invocation_falls_back_to_mcp_tool() {
        let rec = LogRecord {
            kind: RecordKind::Invocation,
            timestamp: "2026-06-22T12:00:00.000Z".to_string(),
            source: Some(Source::Mcp),
            mcp_tool: Some("jira_read".to_string()),
            ..LogRecord::default()
        };
        let line = render(&rec, "", Format::Oneline);
        assert!(line.contains("mcp"));
        assert!(line.contains("jira_read"));
    }

    #[test]
    fn oneline_http_flags_daemon_and_handles_unknown_kind() {
        let rec = LogRecord {
            kind: RecordKind::Http,
            timestamp: "2026-06-22T12:00:00.000Z".to_string(),
            service: Some("snowflake".to_string()),
            method: Some("POST".to_string()),
            status_code: Some(200),
            via_daemon: true,
            ..LogRecord::default()
        };
        assert!(render(&rec, "", Format::Oneline).contains("[daemon]"));

        // An unknown (future) kind falls through the invocation arm without panic.
        let unknown = LogRecord {
            kind: RecordKind::Unknown,
            ..LogRecord::default()
        };
        let _ = render(&unknown, "", Format::Oneline);
    }

    #[test]
    fn full_format_is_pretty_json() {
        let rec = LogRecord {
            kind: RecordKind::Http,
            service: Some("jira".to_string()),
            ..LogRecord::default()
        };
        let out = render(&rec, "", Format::Full);
        assert!(out.contains("\"service\": \"jira\""));
        assert!(out.contains('\n'), "pretty output spans multiple lines");
    }

    #[test]
    fn short_time_handles_negative_offset_and_no_t() {
        // Negative tz offset exercises the rfind('-') fallback.
        assert_eq!(short_time("2026-06-22T01:02:03-05:00"), "01:02:03");
        // No 'T' separator and no tz marker returns the input unchanged.
        assert_eq!(short_time("12:00:00"), "12:00:00");
    }
}
