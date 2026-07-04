//! Append-only, local invocation + HTTP request log (`log.jsonl`).
//!
//! Every `omni-dev` invocation appends one `kind: "invocation"` line; every
//! outbound HTTP request made by one of the integration clients appends one
//! `kind: "http"` line correlated to it by a shared `invocation_id`. The log
//! is **local-machine state** written under the platform state/data directory
//! (`0700` dir / `0600` file, the same posture as [`crate::daemon::paths`]).
//!
//! Design invariants:
//!
//! - **Best effort.** [`record`] swallows every error (logging only at
//!   `tracing::debug`); a logging failure can never change the program's exit
//!   code. Honors `OMNI_DEV_LOG_DISABLE=1` for an absolute opt-out.
//! - **No secrets.** Auth headers/tokens are never written; only a non-secret
//!   `auth_principal` identity is kept. Headers are redacted centrally
//!   ([`redact_headers`]), secret-bearing URL query/fragment parameter values
//!   are redacted (`redact_url`) before writing, and request/response bodies
//!   are opt-in via `OMNI_DEV_LOG_BODIES=1`.
//! - **Forward compatible.** A single [`LogRecord`] is used for both writing
//!   and reading: every field is `#[serde(default)]`, and every optional field
//!   is `skip_serializing_if`, so a newer reader never chokes on an older line
//!   and an older reader never chokes on a newer one — the same forward-rolling
//!   contract the daemon wire types use.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use chrono::SecondsFormat;
use serde::{Deserialize, Serialize};

/// Default log file name under the runtime directory.
const LOG_FILE_NAME: &str = "log.jsonl";

/// Which kind of record a line holds. Unknown future kinds deserialize to
/// [`RecordKind::Unknown`] rather than failing the read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordKind {
    /// One per process invocation (or per MCP tool call).
    #[default]
    Invocation,
    /// One per outbound HTTP request.
    Http,
    /// A kind written by a newer version that this reader does not know.
    #[serde(other)]
    Unknown,
}

/// What drove an invocation. Unknown future sources deserialize to
/// [`Source::Unknown`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// A direct `omni-dev` CLI invocation.
    #[default]
    Cli,
    /// An `omni-dev-mcp` tool call.
    Mcp,
    /// Work performed inside the long-lived daemon process.
    Daemon,
    /// A source written by a newer version that this reader does not know.
    #[serde(other)]
    Unknown,
}

/// One line of the log. Used for both writing and reading; every field is
/// `#[serde(default)]` (tolerant reads) and every optional field is
/// `skip_serializing_if` (compact, forward-compatible writes).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogRecord {
    // --- Core fields (present on every record) ---
    /// Per-record, time-sortable id (see [`new_id`]).
    #[serde(default)]
    pub id: String,
    /// Shared by an invocation record and every HTTP record it spawned.
    #[serde(default)]
    pub invocation_id: String,
    /// Discriminates the record type.
    #[serde(default)]
    pub kind: RecordKind,
    /// RFC3339 timestamp with milliseconds.
    #[serde(default)]
    pub timestamp: String,
    /// Host the record was written on.
    #[serde(default)]
    pub hostname: String,
    /// Writing process id.
    #[serde(default)]
    pub pid: u32,
    /// `omni-dev` version that wrote the record.
    #[serde(default)]
    pub omni_dev_version: String,
    /// Working directory at write time.
    #[serde(default)]
    pub cwd: String,
    /// OS user that owns the process.
    #[serde(default)]
    pub system_user: String,

    // --- `kind: "invocation"` fields ---
    /// Resolved clap subcommand path, e.g. `["jira","read"]`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Full argv.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command_line: Vec<String>,
    /// Process exit code (0 success, 1 error — matches `die`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// Wall time of the whole invocation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Whitelisted, non-secret `OMNI_DEV_*` env snapshot.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// What drove the run (`cli`/`mcp`/`daemon`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<Source>,
    /// When `source = mcp`, the tool name that drove the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_tool: Option<String>,

    // --- `kind: "http"` fields ---
    /// Coarse service tag (`jira`/`confluence`/`datadog`/…) for fast filtering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// HTTP method.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Request URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Response status; absent on a network/transport error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    /// Elapsed time of the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// True when the request ran inside the daemon (bridge/Snowflake pool).
    #[serde(default, skip_serializing_if = "is_false")]
    pub via_daemon: bool,
    /// Which pooled daemon session served the request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_session_id: Option<String>,
    /// Non-secret identity actually used (token id / OAuth principal) — never
    /// the secret itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_principal: Option<String>,
    /// Redacted request headers (only when `OMNI_DEV_LOG_HEADERS=1`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub request_headers: BTreeMap<String, String>,
    /// Redacted response headers (only when `OMNI_DEV_LOG_HEADERS=1`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub response_headers: BTreeMap<String, String>,
    /// Request body (only when `OMNI_DEV_LOG_BODIES=1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_body: Option<String>,
    /// Response body (only when `OMNI_DEV_LOG_BODIES=1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
    /// Free-form correlation tags.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context: BTreeMap<String, String>,

    // --- shared optional ---
    /// Top-level error chain (invocation) or per-request error (http).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `skip_serializing_if` predicate for `bool` fields that default to `false`.
#[allow(clippy::trivially_copy_pass_by_ref)] // serde requires `fn(&T) -> bool`
fn is_false(b: &bool) -> bool {
    !*b
}

impl LogRecord {
    /// Builds a record carrying only the always-present core fields.
    fn new(kind: RecordKind, invocation_id: String) -> Self {
        Self {
            id: new_id(),
            invocation_id,
            kind,
            timestamp: now_rfc3339_millis(),
            hostname: hostname(),
            pid: std::process::id(),
            omni_dev_version: crate::VERSION.to_string(),
            cwd: cwd(),
            system_user: system_user(),
            ..Self::default()
        }
    }
}

/// The per-invocation context every record is stamped with.
///
/// Held once per process in [`GLOBAL`] (CLI/daemon) and overridden per task in
/// [`CTX`] (the multiplexed MCP server), so HTTP records can find their parent
/// invocation without threading state through every call site.
#[derive(Debug, Clone)]
pub struct RequestLogContext {
    /// Shared id linking an invocation to the HTTP it spawned.
    pub invocation_id: String,
    /// What drove the run.
    pub source: Source,
    /// MCP tool name when `source = mcp`.
    pub mcp_tool: Option<String>,
}

impl Default for RequestLogContext {
    fn default() -> Self {
        Self {
            invocation_id: new_id(),
            source: Source::Cli,
            mcp_tool: None,
        }
    }
}

impl RequestLogContext {
    /// A CLI context with a freshly minted invocation id.
    pub fn cli() -> Self {
        Self {
            invocation_id: new_id(),
            source: Source::Cli,
            mcp_tool: None,
        }
    }

    /// An MCP context for a single tool call.
    pub fn mcp(tool: impl Into<String>) -> Self {
        Self {
            invocation_id: new_id(),
            source: Source::Mcp,
            mcp_tool: Some(tool.into()),
        }
    }
}

static GLOBAL: OnceLock<RequestLogContext> = OnceLock::new();

tokio::task_local! {
    /// Per-task context override, set around each MCP tool dispatch.
    pub static CTX: RequestLogContext;
}

/// Installs the process-global context. The first call wins (the CLI/daemon
/// shell sets it once, very early); later calls are ignored.
pub fn set_global(ctx: RequestLogContext) {
    let _ = GLOBAL.set(ctx);
}

/// Resolves the active context: task-local override first, then the
/// process-global default, then a synthesized fallback.
pub fn current_context() -> RequestLogContext {
    if let Ok(ctx) = CTX.try_with(RequestLogContext::clone) {
        return ctx;
    }
    if let Some(ctx) = GLOBAL.get() {
        return ctx.clone();
    }
    RequestLogContext::default()
}

/// Whether logging is disabled entirely (`OMNI_DEV_LOG_DISABLE=1`).
pub fn disabled() -> bool {
    env_flag("OMNI_DEV_LOG_DISABLE")
}

/// Whether request/response bodies may be recorded (`OMNI_DEV_LOG_BODIES=1`).
pub fn bodies_enabled() -> bool {
    env_flag("OMNI_DEV_LOG_BODIES")
}

/// Whether (redacted) headers may be recorded (`OMNI_DEV_LOG_HEADERS=1`).
pub fn headers_enabled() -> bool {
    env_flag("OMNI_DEV_LOG_HEADERS")
}

/// Reads a boolean-ish env var (`1`/`true`/`yes`, case-insensitive).
fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| {
        let v = v.trim().to_ascii_lowercase();
        v == "1" || v == "true" || v == "yes"
    })
}

/// Resolves the log file path: `OMNI_DEV_LOG_FILE` override, else
/// `state_dir` (falling back to `data_dir`) joined with `omni-dev/log.jsonl`.
pub fn log_file_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("OMNI_DEV_LOG_FILE") {
        if !path.is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    let base = dirs::state_dir().or_else(dirs::data_dir)?;
    Some(base.join("omni-dev").join(LOG_FILE_NAME))
}

/// Appends one record. Best effort: every error is swallowed (logged at
/// `tracing::debug`) so logging can never affect the caller's exit code.
pub fn record(entry: &LogRecord) {
    if disabled() {
        return;
    }
    if let Err(e) = try_record(entry) {
        tracing::debug!("request_log: failed to append record: {e}");
    }
}

/// The fallible append used by [`record`]; all errors flow back to be swallowed.
fn try_record(entry: &LogRecord) -> anyhow::Result<()> {
    use anyhow::Context;

    let path = log_file_path().context("could not resolve the log file path")?;
    // Only create and tighten the parent when it's missing — re-`chmod`ing an
    // existing dir (e.g. a user-chosen OMNI_DEV_LOG_FILE location, or a shared
    // temp dir) is both wrong and may fail; the file itself is always 0600.
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            crate::daemon::paths::ensure_dir_0700(parent)?;
        }
    }
    let mut line = serde_json::to_string(entry).context("failed to serialize record")?;
    line.push('\n');
    append_line(&path, &line)?;
    Ok(())
}

/// Appends a single line with `O_APPEND | O_CREATE`, creating the file `0600`.
/// A pre-existing looser-perm file (an older version's, or a user-set
/// `OMNI_DEV_LOG_FILE` target) is re-tightened to `0600` on every open, via
/// the handle so there is no path race (#1139).
/// When bodies are enabled (lines may exceed the atomic-write size) an advisory
/// exclusive lock guards the write; the common no-body path relies on
/// `O_APPEND` single-write atomicity and takes no lock.
#[cfg(unix)]
fn append_line(path: &std::path::Path, line: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)?;
    crate::daemon::paths::ensure_handle_0600(&file)?;

    if bodies_enabled() {
        match nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive) {
            Ok(mut guard) => {
                guard.write_all(line.as_bytes())?;
            }
            Err((mut file, _)) => {
                file.write_all(line.as_bytes())?;
            }
        }
    } else {
        let mut file = file;
        file.write_all(line.as_bytes())?;
    }
    Ok(())
}

/// Non-unix fallback: `O_APPEND | O_CREATE` single write, no advisory lock and
/// no mode tightening (those are unix concepts).
#[cfg(not(unix))]
fn append_line(path: &std::path::Path, line: &str) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(path)?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// The outcome of an invocation, recorded once after `cli.execute()` returns.
#[derive(Debug, Clone)]
pub struct InvocationOutcome {
    /// Resolved clap subcommand path.
    pub command: Vec<String>,
    /// Full argv.
    pub command_line: Vec<String>,
    /// Process exit code.
    pub exit_code: i32,
    /// Rendered error chain, when the command failed.
    pub error: Option<String>,
    /// Wall time of the whole invocation.
    pub duration: Duration,
}

/// Appends one `kind: "invocation"` record from the active context.
pub fn record_invocation(outcome: InvocationOutcome) {
    let ctx = current_context();
    let mut rec = LogRecord::new(RecordKind::Invocation, ctx.invocation_id);
    rec.source = Some(ctx.source);
    rec.mcp_tool = ctx.mcp_tool;
    rec.command = outcome.command;
    rec.command_line = scrub_argv(&outcome.command_line);
    rec.exit_code = Some(outcome.exit_code);
    rec.error = outcome.error;
    rec.duration_ms = Some(outcome.duration.as_millis() as u64);
    rec.env = whitelisted_env();
    record(&rec);
}

/// Optional, non-secret extras for an HTTP record. Bodies/headers are gated and
/// redacted centrally in [`record_http_with`], so callers may pass them freely.
#[derive(Debug, Clone, Default)]
pub struct HttpExtra {
    /// True when served inside the daemon.
    pub via_daemon: bool,
    /// Pooled daemon session id that served the request.
    pub daemon_session_id: Option<String>,
    /// Non-secret identity used (never the secret).
    pub auth_principal: Option<String>,
    /// Raw request headers (redacted + gated before writing).
    pub request_headers: BTreeMap<String, String>,
    /// Raw response headers (redacted + gated before writing).
    pub response_headers: BTreeMap<String, String>,
    /// Request body (gated before writing).
    pub request_body: Option<String>,
    /// Response body (gated before writing).
    pub response_body: Option<String>,
    /// Free-form correlation tags.
    pub context: BTreeMap<String, String>,
}

/// Appends one `kind: "http"` record with method/url/status/elapsed/error.
pub fn record_http(
    service: &str,
    method: &str,
    url: &str,
    started: Instant,
    status: Option<u16>,
    error: Option<&str>,
) {
    record_http_with(
        service,
        method,
        url,
        started,
        status,
        error,
        HttpExtra::default(),
    );
}

/// Appends one `kind: "http"` record with extra, non-secret fields.
///
/// Headers and bodies are dropped unless their opt-in env var is set, headers
/// are always redacted, and URL query/fragment values under secret-looking
/// keys are replaced with `REDACTED` (`redact_url`) — so no secret can be
/// written here under any caller.
#[allow(clippy::too_many_arguments)]
pub fn record_http_with(
    service: &str,
    method: &str,
    url: &str,
    started: Instant,
    status: Option<u16>,
    error: Option<&str>,
    extra: HttpExtra,
) {
    if disabled() {
        return;
    }
    let ctx = current_context();
    let mut rec = LogRecord::new(RecordKind::Http, ctx.invocation_id);
    rec.source = Some(ctx.source);
    rec.mcp_tool = ctx.mcp_tool;
    rec.service = Some(service.to_string());
    rec.method = Some(method.to_string());
    rec.url = Some(redact_url(url));
    rec.status_code = status;
    rec.elapsed_ms = Some(started.elapsed().as_millis() as u64);
    rec.error = error.map(str::to_string);
    rec.via_daemon = extra.via_daemon;
    rec.daemon_session_id = extra.daemon_session_id;
    rec.auth_principal = extra.auth_principal;
    rec.context = extra.context;
    if headers_enabled() {
        rec.request_headers = redact_headers(&extra.request_headers);
        rec.response_headers = redact_headers(&extra.response_headers);
    }
    if bodies_enabled() {
        rec.request_body = extra.request_body;
        rec.response_body = extra.response_body;
    }
    record(&rec);
}

/// Header names whose values must never be written (compared lowercased).
const SENSITIVE_HEADERS: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "x-api-key",
    "api-key",
    "dd-api-key",
    "dd-application-key",
    "x-datadog-api-key",
    "x-datadog-application-key",
    "x-omni-bridge",
    "x-omni-bridge-target",
];

/// Substrings that mark a header name as secret-bearing (compared lowercased),
/// guarding against off-list auth headers (e.g. `x-auth-token`,
/// `x-goog-api-key`). False positives redact harmlessly.
const SENSITIVE_HEADER_MARKERS: &[&str] = &[
    "auth",
    "token",
    "secret",
    "key",
    "cookie",
    "password",
    "session",
    "signature",
    "credential",
];

/// Replaces sensitive header values with `REDACTED`, passing others through.
///
/// A header is sensitive when its lowercased name is in [`SENSITIVE_HEADERS`]
/// or contains any [`SENSITIVE_HEADER_MARKERS`] substring.
pub fn redact_headers(headers: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    headers
        .iter()
        .map(|(name, value)| {
            let lower = name.to_ascii_lowercase();
            let redacted = SENSITIVE_HEADERS.contains(&lower.as_str())
                || SENSITIVE_HEADER_MARKERS
                    .iter()
                    .any(|marker| lower.contains(marker));
            (
                name.clone(),
                if redacted {
                    "REDACTED".to_string()
                } else {
                    value.clone()
                },
            )
        })
        .collect()
}

/// Flag-name segments marking a long flag's value as secret-bearing — the argv
/// counterpart of [`SECRETISH`]. Matched per `-`/`_`-separated segment of the
/// flag name so `--api-key` is caught but a name like `--keyword` is not.
const SECRETISH_FLAG_WORDS: &[&str] = &["token", "secret", "password", "passwd", "key"];

/// True when the long flag `--<name>` takes a secret-bearing value. Flags whose
/// last segment is `file` or `path` carry paths, not secrets, and are exempt
/// (e.g. `--token-file`).
fn is_secretish_flag(name: &str) -> bool {
    let segments: Vec<String> = name
        .split(['-', '_'])
        .map(str::to_ascii_lowercase)
        .collect();
    let takes_path = matches!(segments.last().map(String::as_str), Some("file" | "path"));
    !takes_path
        && segments
            .iter()
            .any(|segment| SECRETISH_FLAG_WORDS.contains(&segment.as_str()))
}

/// Scrubs one `--header` value (`Name: Value`): values of [`SENSITIVE_HEADERS`]
/// are redacted keeping the name, other headers pass through (`None`), and a
/// value with no colon is redacted wholesale.
fn scrub_header_arg(value: &str) -> Option<String> {
    let Some((name, _)) = value.split_once(':') else {
        return Some("REDACTED".to_string());
    };
    SENSITIVE_HEADERS
        .contains(&name.trim().to_ascii_lowercase().as_str())
        .then(|| format!("{}: REDACTED", name.trim()))
}

/// Returns the scrubbed replacement for the value of flag `--<name>`, or
/// `None` when the value is safe to log verbatim. `--body` keeps `@file`
/// references (a path, not a secret).
fn scrub_flag_value(name: &str, value: &str) -> Option<String> {
    match name {
        "header" => scrub_header_arg(value),
        "body" => (!value.starts_with('@')).then(|| "REDACTED".to_string()),
        _ if is_secretish_flag(name) => Some("REDACTED".to_string()),
        _ => None,
    }
}

/// Scrubs secret-bearing values out of a raw argv before it is logged. Two
/// write-side layers, so the on-disk line is clean and every reader/format is
/// covered with no reader changes:
///
/// 1. [`scrub_flag_secrets`] — flag-aware whole-value redaction (`--header`/
///    `--body` plus any [`is_secretish_flag`] name, in both `--flag value` and
///    `--flag=value` forms).
/// 2. [`redact_url`] over every resulting element — a secret-bearing query or
///    fragment parameter on a URL argument (most naturally
///    `--url /path?access_token=…`, which no flag-name rule catches) has its
///    value redacted, while benign argv passes through byte-identical (#1162).
fn scrub_argv(argv: &[String]) -> Vec<String> {
    scrub_flag_secrets(argv)
        .iter()
        .map(|arg| redact_url(arg))
        .collect()
}

/// Flag-aware first layer of [`scrub_argv`]: redacts secret-bearing flag values
/// (`--header`/`--body` plus any [`is_secretish_flag`] name, in both
/// `--flag value` and `--flag=value` forms). Everything else passes through to
/// the URL layer.
fn scrub_flag_secrets(argv: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        i += 1;
        let Some(flag_body) = arg.strip_prefix("--") else {
            out.push(arg.clone());
            continue;
        };
        if let Some((name, value)) = flag_body.split_once('=') {
            match scrub_flag_value(name, value) {
                Some(scrubbed) => out.push(format!("--{name}={scrubbed}")),
                None => out.push(arg.clone()),
            }
        } else {
            out.push(arg.clone());
            let takes_secret_value =
                matches!(flag_body, "header" | "body") || is_secretish_flag(flag_body);
            if takes_secret_value {
                if let Some(value) = argv.get(i) {
                    i += 1;
                    out.push(scrub_flag_value(flag_body, value).unwrap_or_else(|| value.clone()));
                }
            }
        }
    }
    out
}

/// Query/fragment keys that are secrets outright (compared decoded + lowercased).
const SENSITIVE_QUERY_KEYS: &[&str] = &["sig", "sas", "jwt", "auth"];

/// Key suffixes marking the open-ended secret families (`access_token`,
/// `client_secret`, `api_key`, …).
const SENSITIVE_QUERY_KEY_SUFFIXES: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "signature",
    "apikey",
    "api_key",
    "api-key",
];

/// Key prefixes for cloud-storage signed-URL parameter families.
const SENSITIVE_QUERY_KEY_PREFIXES: &[&str] = &["x-amz-", "x-goog-"];

/// Returns whether a decoded query/fragment key looks secret-bearing.
fn sensitive_query_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SENSITIVE_QUERY_KEYS.contains(&key.as_str())
        || SENSITIVE_QUERY_KEY_SUFFIXES
            .iter()
            .any(|suffix| key.ends_with(suffix))
        || SENSITIVE_QUERY_KEY_PREFIXES
            .iter()
            .any(|prefix| key.starts_with(prefix))
}

/// Rewrites one `&`-separated pair list, replacing the values of
/// secret-bearing keys with `REDACTED` and passing every other segment
/// through byte-verbatim.
fn redact_pairs(pairs: &str) -> String {
    pairs
        .split('&')
        .map(|segment| match segment.split_once('=') {
            Some((raw_key, _)) => {
                // Decode only the key (handles `access%5Ftoken` and `+`); the
                // raw key text is preserved in the output.
                let sensitive = url::form_urlencoded::parse(raw_key.as_bytes())
                    .next()
                    .is_some_and(|(key, _)| sensitive_query_key(&key));
                if sensitive {
                    format!("{raw_key}=REDACTED")
                } else {
                    segment.to_string()
                }
            }
            // A bare key (no `=`) carries no value to leak.
            None => segment.to_string(),
        })
        .collect::<Vec<_>>()
        .join("&")
}

/// Redacts secret-bearing query and fragment parameter values in a URL,
/// preserving scheme, host, path, and all parameter keys so `--url` substring
/// filtering stays useful. Handles relative URLs (the browser bridge logs
/// page-origin targets like `/api/foo?sig=…`), so this never requires the
/// input to parse as an absolute [`url::Url`].
fn redact_url(url: &str) -> String {
    let (rest, fragment) = url
        .split_once('#')
        .map_or((url, None), |(rest, fragment)| (rest, Some(fragment)));
    let (prefix, query) = rest
        .split_once('?')
        .map_or((rest, None), |(prefix, query)| (prefix, Some(query)));
    let mut out = prefix.to_string();
    if let Some(query) = query {
        out.push('?');
        out.push_str(&redact_pairs(query));
    }
    if let Some(fragment) = fragment {
        out.push('#');
        out.push_str(&redact_pairs(fragment));
    }
    out
}

/// A time-sortable id: 13-digit zero-padded epoch-millis, a dash, then 16 hex.
///
/// Lexical order ≈ chronological order, which is all the reader needs. Mirrors
/// the uuid-shaped minting in [`crate::snowflake::client`] without adding a
/// crate.
pub fn new_id() -> String {
    let millis = chrono::Utc::now().timestamp_millis().max(0);
    let suffix = rand::random::<u64>();
    format!("{millis:013}-{suffix:016x}")
}

/// Current time as RFC3339 with millisecond precision, in UTC.
fn now_rfc3339_millis() -> String {
    chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Best-effort current working directory.
fn cwd() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default()
}

/// Best-effort OS username (`$USER`, then the passwd entry for the euid).
fn system_user() -> String {
    if let Ok(user) = std::env::var("USER") {
        if !user.is_empty() {
            return user;
        }
    }
    #[cfg(unix)]
    {
        if let Ok(Some(user)) = nix::unistd::User::from_uid(nix::unistd::geteuid()) {
            return user.name;
        }
    }
    String::new()
}

/// Best-effort hostname (`gethostname`, then `$HOSTNAME`, then empty).
fn hostname() -> String {
    #[cfg(unix)]
    {
        if let Ok(name) = nix::unistd::gethostname() {
            if let Some(name) = name.to_str() {
                if !name.is_empty() {
                    return name.to_string();
                }
            }
        }
    }
    std::env::var("HOSTNAME").unwrap_or_default()
}

/// Names matching these substrings have their env values redacted, guarding
/// against any future secret-bearing `OMNI_DEV_*` var.
const SECRETISH: &[&str] = &["TOKEN", "SECRET", "KEY", "PASSWORD", "PASSWD"];

/// Snapshot of `OMNI_DEV_*` env vars, with secret-looking values redacted.
fn whitelisted_env() -> BTreeMap<String, String> {
    std::env::vars()
        .filter(|(k, _)| k.starts_with("OMNI_DEV_"))
        .map(|(k, v)| {
            let secretish = SECRETISH.iter().any(|needle| k.contains(needle));
            let value = if secretish { "REDACTED".to_string() } else { v };
            (k, value)
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn record_round_trips_through_json() {
        let mut rec = LogRecord::new(RecordKind::Http, "inv-1".to_string());
        rec.service = Some("jira".to_string());
        rec.method = Some("GET".to_string());
        rec.url = Some("https://example.atlassian.net/rest/api/3/issue/X-1".to_string());
        rec.status_code = Some(200);
        rec.elapsed_ms = Some(42);

        let line = serde_json::to_string(&rec).unwrap();
        let back: LogRecord = serde_json::from_str(&line).unwrap();
        assert_eq!(back.invocation_id, "inv-1");
        assert_eq!(back.kind, RecordKind::Http);
        assert_eq!(back.service.as_deref(), Some("jira"));
        assert_eq!(back.status_code, Some(200));
    }

    #[test]
    fn reader_tolerates_unknown_fields() {
        let line = r#"{"id":"x","invocation_id":"i","kind":"http","method":"GET",
            "future_field":{"nested":true},"another":42}"#;
        let rec: LogRecord = serde_json::from_str(line).unwrap();
        assert_eq!(rec.kind, RecordKind::Http);
        assert_eq!(rec.method.as_deref(), Some("GET"));
    }

    #[test]
    fn reader_tolerates_missing_newer_fields() {
        // An "old" line with only a couple of fields present.
        let line = r#"{"kind":"invocation","command":["git","view"]}"#;
        let rec: LogRecord = serde_json::from_str(line).unwrap();
        assert_eq!(rec.kind, RecordKind::Invocation);
        assert_eq!(rec.command, vec!["git", "view"]);
        assert!(rec.status_code.is_none());
        assert!(rec.id.is_empty());
    }

    #[test]
    fn unknown_kind_and_source_do_not_fail() {
        let line = r#"{"kind":"telemetry","source":"webhook"}"#;
        let rec: LogRecord = serde_json::from_str(line).unwrap();
        assert_eq!(rec.kind, RecordKind::Unknown);
        assert_eq!(rec.source, Some(Source::Unknown));
    }

    #[test]
    fn optional_fields_are_skipped_when_empty() {
        let rec = LogRecord::new(RecordKind::Invocation, "i".to_string());
        let line = serde_json::to_string(&rec).unwrap();
        // Empty collections / None options must not appear on the wire.
        assert!(!line.contains("status_code"));
        assert!(!line.contains("request_headers"));
        assert!(!line.contains("via_daemon"));
        assert!(!line.contains("\"env\""));
    }

    #[test]
    fn ids_are_time_sortable() {
        let a = new_id();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = new_id();
        assert!(a < b, "{a} should sort before {b}");
    }

    #[test]
    fn sensitive_headers_are_redacted() {
        let mut headers = BTreeMap::new();
        headers.insert("Authorization".to_string(), "Bearer secret".to_string());
        headers.insert("X-Api-Key".to_string(), "abc123".to_string());
        headers.insert("Content-Type".to_string(), "application/json".to_string());
        let out = redact_headers(&headers);
        assert_eq!(out["Authorization"], "REDACTED");
        assert_eq!(out["X-Api-Key"], "REDACTED");
        assert_eq!(out["Content-Type"], "application/json");
    }

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().copied().map(String::from).collect()
    }

    #[test]
    fn scrub_argv_redacts_sensitive_header_in_both_forms() {
        let out = scrub_argv(&argv(&[
            "omni-dev",
            "--header",
            "Authorization: Bearer sekret",
            "--header=Cookie: session=abc",
        ]));
        assert_eq!(
            out,
            argv(&[
                "omni-dev",
                "--header",
                "Authorization: REDACTED",
                "--header=Cookie: REDACTED",
            ])
        );
    }

    #[test]
    fn scrub_argv_keeps_non_sensitive_headers() {
        let input = argv(&["omni-dev", "--header", "Content-Type: application/json"]);
        assert_eq!(scrub_argv(&input), input);
    }

    #[test]
    fn scrub_argv_redacts_colonless_header_wholesale() {
        let out = scrub_argv(&argv(&["omni-dev", "--header", "sekret"]));
        assert_eq!(out, argv(&["omni-dev", "--header", "REDACTED"]));
    }

    #[test]
    fn scrub_argv_redacts_inline_body_but_keeps_at_file() {
        let out = scrub_argv(&argv(&["omni-dev", "--body", r#"{"secret":1}"#]));
        assert_eq!(out, argv(&["omni-dev", "--body", "REDACTED"]));

        let file_form = argv(&["omni-dev", "--body", "@payload.json"]);
        assert_eq!(scrub_argv(&file_form), file_form);

        let out = scrub_argv(&argv(&["omni-dev", "--body=sekret"]));
        assert_eq!(out, argv(&["omni-dev", "--body=REDACTED"]));
    }

    #[test]
    fn scrub_argv_redacts_secretish_flag_values() {
        let out = scrub_argv(&argv(&["omni-dev", "--api-key", "abc", "--auth-token=xyz"]));
        assert_eq!(
            out,
            argv(&["omni-dev", "--api-key", "REDACTED", "--auth-token=REDACTED"])
        );
    }

    #[test]
    fn scrub_argv_exempts_path_flags_and_positionals() {
        let input = argv(&["omni-dev", "--token-file", "/tmp/t", "PROJ-123"]);
        assert_eq!(scrub_argv(&input), input);
    }

    #[test]
    fn scrub_argv_redacts_secret_bearing_url_query_in_both_forms() {
        // `--url` is not a secret-ish flag name, so its value is caught by the
        // redact_url layer, not the flag layer (#1162). Both argv shapes plus a
        // bare positional URL are covered; the benign `page` param survives.
        let space = scrub_argv(&argv(&[
            "omni-dev",
            "browser",
            "bridge",
            "request",
            "--url",
            "/api/export?access_token=hunter2&sig=deadbeef&page=3",
        ]));
        assert_eq!(
            *space.last().unwrap(),
            "/api/export?access_token=REDACTED&sig=REDACTED&page=3"
        );

        let eq_form = scrub_argv(&argv(&[
            "omni-dev",
            "--url=/api/export?access_token=hunter2&page=3",
        ]));
        assert_eq!(
            *eq_form.last().unwrap(),
            "--url=/api/export?access_token=REDACTED&page=3"
        );

        let positional = scrub_argv(&argv(&["omni-dev", "https://h/cb#id_token=xyz"]));
        assert_eq!(
            *positional.last().unwrap(),
            "https://h/cb#id_token=REDACTED"
        );
    }

    #[test]
    fn scrub_argv_leaves_benign_argv_byte_identical() {
        let input = argv(&[
            "omni-dev",
            "browser",
            "bridge",
            "request",
            "--control-port",
            "19998",
            "--url",
            "/api/export?page=3&sort=asc",
        ]);
        assert_eq!(scrub_argv(&input), input);
    }

    #[test]
    fn scrub_argv_handles_trailing_flag_without_value() {
        let input = argv(&["omni-dev", "--body"]);
        assert_eq!(scrub_argv(&input), input);
    }

    #[cfg(unix)]
    #[test]
    fn append_line_creates_file_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        append_line(&path, "{\"kind\":\"http\"}\n").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"kind\":\"http\"}\n"
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn append_line_retightens_preexisting_loose_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("log.jsonl");
        std::fs::write(&path, "old\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        append_line(&path, "new\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old\nnew\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn off_list_secretish_headers_are_redacted() {
        let mut headers = BTreeMap::new();
        for name in [
            "X-Auth-Token",
            "x-amz-security-token",
            "X-Goog-Api-Key",
            "x-csrf-token",
            "X-Vendor-Token",
            "X-Omni-Bridge",
        ] {
            headers.insert(name.to_string(), "secret-value".to_string());
        }
        for name in [
            "Content-Type",
            "Accept",
            "User-Agent",
            "x-request-id",
            "traceparent",
        ] {
            headers.insert(name.to_string(), "plain-value".to_string());
        }
        let out = redact_headers(&headers);
        assert_eq!(out["X-Auth-Token"], "REDACTED");
        assert_eq!(out["x-amz-security-token"], "REDACTED");
        assert_eq!(out["X-Goog-Api-Key"], "REDACTED");
        assert_eq!(out["x-csrf-token"], "REDACTED");
        assert_eq!(out["X-Vendor-Token"], "REDACTED");
        assert_eq!(out["X-Omni-Bridge"], "REDACTED");
        assert_eq!(out["Content-Type"], "plain-value");
        assert_eq!(out["Accept"], "plain-value");
        assert_eq!(out["User-Agent"], "plain-value");
        assert_eq!(out["x-request-id"], "plain-value");
        assert_eq!(out["traceparent"], "plain-value");
    }

    #[test]
    fn url_without_query_is_unchanged() {
        assert_eq!(redact_url("https://h/p"), "https://h/p");
        assert_eq!(redact_url("/relative/p"), "/relative/p");
    }

    #[test]
    fn benign_query_is_byte_identical() {
        let url = "https://h/p?q=a%20b&page=2&&x=y+z&keyword=k&sort_key=s&token_type=bearer";
        assert_eq!(redact_url(url), url);
    }

    #[test]
    fn sensitive_query_values_are_redacted() {
        let url = "https://h/p?token=a&access_token=b&client_secret=c&api_key=d&x=1";
        assert_eq!(
            redact_url(url),
            "https://h/p?token=REDACTED&access_token=REDACTED&client_secret=REDACTED\
             &api_key=REDACTED&x=1"
        );
    }

    #[test]
    fn presigned_s3_query_is_redacted() {
        let url = "https://bucket.s3.amazonaws.com/key?X-Amz-Algorithm=AWS4-HMAC-SHA256\
                   &X-Amz-Credential=AKIA%2F20260703%2Fus-east-1%2Fs3%2Faws4_request\
                   &X-Amz-Date=20260703T000000Z&X-Amz-Expires=3600\
                   &X-Amz-SignedHeaders=host&X-Amz-Signature=deadbeef";
        assert_eq!(
            redact_url(url),
            "https://bucket.s3.amazonaws.com/key?X-Amz-Algorithm=REDACTED\
             &X-Amz-Credential=REDACTED&X-Amz-Date=REDACTED&X-Amz-Expires=REDACTED\
             &X-Amz-SignedHeaders=REDACTED&X-Amz-Signature=REDACTED"
        );
    }

    #[test]
    fn key_matching_is_case_insensitive() {
        assert_eq!(
            redact_url("/p?TOKEN=x&Api_Key=y&X-Amz-Signature=z"),
            "/p?TOKEN=REDACTED&Api_Key=REDACTED&X-Amz-Signature=REDACTED"
        );
    }

    #[test]
    fn repeated_sensitive_keys_are_each_redacted() {
        assert_eq!(redact_url("/p?sig=a&sig=b"), "/p?sig=REDACTED&sig=REDACTED");
    }

    #[test]
    fn valueless_key_is_left_alone() {
        assert_eq!(redact_url("/p?token"), "/p?token");
        assert_eq!(redact_url("/p?token="), "/p?token=REDACTED");
    }

    #[test]
    fn relative_url_query_is_redacted() {
        assert_eq!(
            redact_url("/api/foo?sig=abc&x=y"),
            "/api/foo?sig=REDACTED&x=y"
        );
    }

    #[test]
    fn fragment_credentials_are_redacted() {
        assert_eq!(
            redact_url("https://h/cb#access_token=xyz&token_type=bearer"),
            "https://h/cb#access_token=REDACTED&token_type=bearer"
        );
    }

    #[test]
    fn query_and_fragment_are_scrubbed_independently() {
        assert_eq!(
            redact_url("/p?sig=a#id_token=b"),
            "/p?sig=REDACTED#id_token=REDACTED"
        );
    }

    #[test]
    fn question_mark_in_fragment_is_not_parsed_as_query() {
        // The fragment is split off before the query, so `?` inside it never
        // starts a query; the pseudo-key `frag?token` still redacts via the
        // suffix rule (over-redaction in the safe direction).
        assert_eq!(
            redact_url("https://h/p#frag?token=x"),
            "https://h/p#frag?token=REDACTED"
        );
    }

    #[test]
    fn encoded_sensitive_key_is_decoded_before_matching() {
        assert_eq!(
            redact_url("/p?access%5Ftoken=v"),
            "/p?access%5Ftoken=REDACTED"
        );
    }

    #[test]
    fn empty_query_is_unchanged() {
        assert_eq!(redact_url("https://h/p?"), "https://h/p?");
        assert_eq!(redact_url("https://h/p?#f"), "https://h/p?#f");
    }

    #[test]
    fn env_flag_parses_truthy_values() {
        std::env::set_var("OMNI_DEV_TEST_FLAG_ABC", "1");
        assert!(env_flag("OMNI_DEV_TEST_FLAG_ABC"));
        std::env::set_var("OMNI_DEV_TEST_FLAG_ABC", "TRUE");
        assert!(env_flag("OMNI_DEV_TEST_FLAG_ABC"));
        std::env::set_var("OMNI_DEV_TEST_FLAG_ABC", "0");
        assert!(!env_flag("OMNI_DEV_TEST_FLAG_ABC"));
        std::env::remove_var("OMNI_DEV_TEST_FLAG_ABC");
        assert!(!env_flag("OMNI_DEV_TEST_FLAG_ABC"));
    }
}
