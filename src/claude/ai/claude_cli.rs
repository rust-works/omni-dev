//! Claude Code CLI subprocess AI client.
//!
//! Shells out to `claude -p` in a locked-down sandbox so users with an
//! authenticated Claude Code session can reuse it without provisioning a
//! separate API key.
//!
//! Sandboxing:
//! - `--tools ""` disables built-in tools (skipped when `ALLOW_TOOLS_ENV_VAR`
//!   is set).
//! - `--strict-mcp-config` (with no accompanying `--mcp-config`) blocks MCP
//!   server pickup from user settings (skipped when `ALLOW_MCP_ENV_VAR` is
//!   set; the two escape hatches are independent).
//! - `--setting-sources ""` skips user / project / local settings discovery.
//! - `--disable-slash-commands` blocks skills.
//! - `--no-session-persistence` avoids writing session state to disk.
//! - Subprocess runs with cwd set to a fresh temp directory.
//! - Environment inherits from the parent, then removes `CLAUDE_PROJECT_DIR`,
//!   any `CLAUDE_CODE_*`, and any `CLAUDE_PROJECT_*` vars that could re-scope
//!   the nested session.
//! - When the tool-access escape hatch is enabled, well-known secret vars
//!   (`*_API_KEY`, `*_TOKEN`, `*_SECRET`, `*_PASSWORD`, `*_CREDENTIALS`,
//!   AWS credentials) are also removed, since the tool-capable session is
//!   driven by untrusted prompt content (issue #1144). Exceptions:
//!   `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` (the child may
//!   authenticate through them) and names listed in [`KEEP_ENV_VAR`].

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tracing::{debug, info, warn};

use super::{AiClient, AiClientCapabilities, AiClientMetadata, RequestOptions};
use crate::claude::error::ClaudeError;
use crate::request_log;

/// Default subprocess timeout.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

/// Default stdout cap (4 MiB).
pub(crate) const DEFAULT_STDOUT_CAP: usize = 4 * 1024 * 1024;

/// Default binary name.
pub(crate) const DEFAULT_BINARY: &str = "claude";

/// Env var overriding [`DEFAULT_TIMEOUT`] (value: seconds).
pub(crate) const TIMEOUT_ENV_VAR: &str = "OMNI_DEV_CLAUDE_CLI_TIMEOUT_SECS";

/// Env var overriding [`DEFAULT_STDOUT_CAP`] (value: bytes).
pub(crate) const STDOUT_CAP_ENV_VAR: &str = "OMNI_DEV_CLAUDE_CLI_STDOUT_MAX_BYTES";

/// Env var overriding [`DEFAULT_BINARY`] (value: path to the `claude` binary).
pub(crate) const BINARY_ENV_VAR: &str = "OMNI_DEV_CLAUDE_CLI_BIN";

/// Env var enabling the tool-access escape hatch.
///
/// When set to `true` / `1` / `yes`, the nested `claude -p` session is
/// allowed to use its default tool set (file-system access, shell, etc.)
/// instead of being run with `--tools ""`. **This weakens the sandbox and
/// should only be used for deliberately tool-capable use cases.**
pub(crate) const ALLOW_TOOLS_ENV_VAR: &str = "OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS";

/// Env var enabling the MCP-access escape hatch.
///
/// When set to `true` / `1` / `yes`, the nested `claude -p` session is
/// allowed to load MCP servers from the user's `~/.claude/settings.json`
/// instead of being run with `--strict-mcp-config` (which blocks MCP pickup
/// entirely). **This exposes any OAuth tokens or network-attached services
/// configured in those MCP servers and should be opted into deliberately,
/// independently of the tool-access escape hatch.**
pub(crate) const ALLOW_MCP_ENV_VAR: &str = "OMNI_DEV_CLAUDE_CLI_ALLOW_MCP";

/// Env var setting a per-invocation spending cap in USD.
///
/// Forwarded to `claude -p --max-budget-usd <amount>`. When the subprocess
/// exceeds this budget it aborts with an error rather than running away
/// with cost. Accepts floating-point dollar amounts (e.g. `0.50`).
pub(crate) const MAX_BUDGET_ENV_VAR: &str = "OMNI_DEV_CLAUDE_CLI_MAX_BUDGET_USD";

/// Env var exempting names from the tool-enabled secret scrub.
///
/// Comma-separated exact variable names (e.g. `GITHUB_TOKEN,NPM_TOKEN`)
/// that stay in the subprocess environment when the tool-access escape
/// hatch triggers the secret scrub (see [`SECRET_ENV_SUFFIXES`]).
/// `ANTHROPIC_API_KEY` / `ANTHROPIC_AUTH_TOKEN` are always kept and need
/// not be listed. Has no effect while the scrub is inactive.
pub(crate) const KEEP_ENV_VAR: &str = "OMNI_DEV_CLAUDE_CLI_KEEP_ENV";

/// Defence-in-depth suffix appended to the caller's system prompt.
///
/// Even with tools disabled at runtime, the model "knows" Claude Code tools
/// exist and will sometimes emit XML `<function_calls>` tags in the output.
/// This instruction cleanly suppresses that behaviour.
pub(crate) const TOOL_SUPPRESSION_SUFFIX: &str =
    "\n\nYou have no tools available in this session. \
Do not emit function_calls XML or attempt any tool invocation. Output only the requested content.";

/// Env var prefixes to remove from the subprocess environment.
const SCRUBBED_ENV_PREFIXES: &[&str] = &["CLAUDE_CODE_", "CLAUDE_PROJECT_"];

/// Exact env var names to remove from the subprocess environment.
const SCRUBBED_ENV_EXACT: &[&str] = &["CLAUDE_PROJECT_DIR"];

/// Suffixes marking an env var as a likely secret.
///
/// Matching vars are removed from the subprocess environment when the
/// tool-access escape hatch is enabled: the tool-capable nested session
/// is driven by untrusted prompt content (diffs, commit messages, JIRA
/// text), so a prompt-injection payload could otherwise read inherited
/// credentials (issue #1144).
const SECRET_ENV_SUFFIXES: &[&str] =
    &["_API_KEY", "_TOKEN", "_SECRET", "_PASSWORD", "_CREDENTIALS"];

/// Exact secret env var names the suffix list misses (AWS credentials;
/// `AWS_SESSION_TOKEN` is already caught by `_TOKEN`).
const SECRET_ENV_EXACT: &[&str] = &["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"];

/// Secret-looking vars that are never scrubbed: the nested `claude`
/// process may authenticate through them (omni-dev itself passes no
/// credentials to the child).
const SECRET_ENV_KEEP: &[&str] = &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];

/// Parses a [`KEEP_ENV_VAR`] value into exact names exempted from the
/// secret scrub: splits on commas, trims whitespace, drops empty segments.
fn parse_keep_env(value: Option<&str>) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Returns whether `key` is a secret env var the tool-enabled scrub must
/// remove: it matches [`SECRET_ENV_SUFFIXES`] or [`SECRET_ENV_EXACT`] and
/// is neither in the built-in [`SECRET_ENV_KEEP`] allowlist nor in the
/// user-supplied `keep` list.
fn is_scrubbed_secret(key: &str, keep: &[String]) -> bool {
    if SECRET_ENV_KEEP.contains(&key) || keep.iter().any(|k| k == key) {
        return false;
    }
    SECRET_ENV_EXACT.contains(&key) || SECRET_ENV_SUFFIXES.iter().any(|s| key.ends_with(s))
}

/// Subset of the `claude -p --output-format json` envelope we care about.
///
/// `claude -p` emits a single JSON object on stdout. Additional fields
/// (`session_id`, `usage`, etc.) are ignored.
#[derive(Deserialize)]
struct JsonOutput {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    api_error_status: Option<i64>,
    #[serde(default)]
    result: String,
    /// Total billed cost for this invocation in USD (inclusive of cache
    /// creation, input and output tokens). Surfaced via tracing for cost
    /// observability regardless of budget cap.
    #[serde(default)]
    total_cost_usd: Option<f64>,
}

/// Subprocess-based AI client that shells out to `claude -p` in a
/// locked-down sandbox.
///
/// Selected by `OMNI_DEV_AI_BACKEND=claude-cli` (or `--ai-backend
/// claude-cli`); see the module-level docs above for the full sandbox
/// posture and the "AI Backend Dispatch" section of `CLAUDE.md` for
/// dispatch ordering.
///
/// Four runtime knobs weaken or bound the sandbox:
/// - `OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS` — re-enable nested tool use (also
///   activates the secret env scrub, see [`SECRET_ENV_SUFFIXES`]).
/// - `OMNI_DEV_CLAUDE_CLI_ALLOW_MCP` — re-enable nested MCP server pickup.
/// - `OMNI_DEV_CLAUDE_CLI_MAX_BUDGET_USD` — per-invocation spending cap in USD.
/// - `OMNI_DEV_CLAUDE_CLI_KEEP_ENV` — exact names exempted from the secret
///   env scrub.
pub struct ClaudeCliAiClient {
    /// Model identifier (alias like `sonnet` or full ID like
    /// `claude-sonnet-4-6`). Forwarded verbatim to `claude -p --model`.
    model: String,
    /// Subprocess timeout.
    timeout: Duration,
    /// Maximum bytes of stdout to accept before erroring.
    stdout_cap: usize,
    /// When true, skip `--tools ""` (escape hatch for future tool-enabled
    /// use cases). Off by default.
    allow_tools: bool,
    /// When true, skip `--strict-mcp-config`, letting the nested session
    /// pick up MCP servers from the user's `~/.claude/settings.json`.
    /// Off by default. Independent of [`Self::allow_tools`].
    allow_mcp: bool,
    /// Path to the `claude` binary (defaults to `claude` on PATH).
    binary_path: PathBuf,
    /// Optional per-invocation spending cap in USD (forwarded to
    /// `claude -p --max-budget-usd`). `None` means no explicit cap.
    max_budget_usd: Option<f64>,
}

impl ClaudeCliAiClient {
    /// Creates a client with defaults from environment variables (or the
    /// compiled-in defaults when unset).
    #[must_use]
    pub fn new(model: String) -> Self {
        Self::new_with_config(
            model,
            Self::timeout_from_env(),
            Self::stdout_cap_from_env(),
            Self::allow_tools_from_env(),
            Self::binary_from_env(),
        )
        .with_allow_mcp(Self::allow_mcp_from_env())
        .with_max_budget_usd(Self::max_budget_from_env())
    }

    /// Creates a client with explicit configuration. Primarily for tests.
    ///
    /// `max_budget_usd` is set separately via [`Self::with_max_budget_usd`] so
    /// that existing callers of this constructor do not need to update
    /// when new optional knobs are added.
    #[must_use]
    pub fn new_with_config(
        model: String,
        timeout: Duration,
        stdout_cap: usize,
        allow_tools: bool,
        binary_path: PathBuf,
    ) -> Self {
        Self {
            model,
            timeout,
            stdout_cap,
            allow_tools,
            allow_mcp: false,
            binary_path,
            max_budget_usd: None,
        }
    }

    /// Sets the MCP-access escape hatch. Builder-style for ergonomics so
    /// existing callers of [`Self::new_with_config`] do not need to update
    /// when new optional knobs are added.
    #[must_use]
    pub fn with_allow_mcp(mut self, allow_mcp: bool) -> Self {
        self.allow_mcp = allow_mcp;
        self
    }

    /// Sets the per-invocation spending cap. Builder-style for ergonomics.
    #[must_use]
    pub fn with_max_budget_usd(mut self, budget: Option<f64>) -> Self {
        self.max_budget_usd = budget;
        self
    }

    fn timeout_from_env() -> Duration {
        crate::utils::settings::get_env_var(TIMEOUT_ENV_VAR)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map_or(DEFAULT_TIMEOUT, Duration::from_secs)
    }

    fn stdout_cap_from_env() -> usize {
        crate::utils::settings::get_env_var(STDOUT_CAP_ENV_VAR)
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(DEFAULT_STDOUT_CAP)
    }

    fn binary_from_env() -> PathBuf {
        crate::utils::settings::get_env_var(BINARY_ENV_VAR)
            .ok()
            .map_or_else(|| PathBuf::from(DEFAULT_BINARY), PathBuf::from)
    }

    /// Reads [`ALLOW_TOOLS_ENV_VAR`] and returns whether the tool-access
    /// escape hatch is enabled. Accepts `true` / `1` / `yes` (case-insensitive);
    /// everything else (including unset) means disabled.
    fn allow_tools_from_env() -> bool {
        crate::utils::settings::get_env_var(ALLOW_TOOLS_ENV_VAR)
            .ok()
            .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
    }

    /// Reads [`ALLOW_MCP_ENV_VAR`] and returns whether the MCP-access escape
    /// hatch is enabled. Accepts `true` / `1` / `yes` (case-insensitive);
    /// everything else (including unset) means disabled.
    fn allow_mcp_from_env() -> bool {
        crate::utils::settings::get_env_var(ALLOW_MCP_ENV_VAR)
            .ok()
            .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"))
    }

    /// Reads [`MAX_BUDGET_ENV_VAR`] and returns the parsed spending cap.
    /// Returns `None` silently when unset; a value that is set but unusable
    /// also returns `None`, after logging a WARN (see
    /// [`Self::max_budget_from_value`]).
    fn max_budget_from_env() -> Option<f64> {
        let raw = crate::utils::settings::get_env_var(MAX_BUDGET_ENV_VAR).ok()?;
        Self::max_budget_from_value(&raw)
    }

    /// Pure core of [`Self::max_budget_from_env`]: validates an explicitly
    /// provided budget value. An unusable value — unparseable, non-finite,
    /// or non-positive — yields `None` (no cap is applied), but logs a WARN
    /// so a typo like `--claude-cli-max-budget-usd 0` cannot silently
    /// disable the cap (issue #1135).
    fn max_budget_from_value(raw: &str) -> Option<f64> {
        let trimmed = raw.trim();
        let parsed = trimmed
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v > 0.0);
        if parsed.is_none() {
            warn!(
                value = trimmed,
                env_var = MAX_BUDGET_ENV_VAR,
                "invalid spending cap ignored (must be a positive, finite \
                 USD amount); running with NO budget cap"
            );
        }
        parsed
    }

    /// Builds the subprocess [`Command`] without spawning.
    ///
    /// Broken out so tests can inspect the argv / env / cwd via
    /// `Command::get_args`, `get_envs`, `get_current_dir` without running
    /// a subprocess.
    #[cfg(test)]
    pub(crate) fn build_command(&self, system_prompt: &str, cwd: &Path) -> Command {
        self.build_command_with_schema(system_prompt, cwd, None)
    }

    /// Variant of `build_command` (test-only) that adds a `--json-schema <json>`
    /// argument when `schema_json` is supplied. `claude -p` requires the
    /// schema verbatim on argv (file paths silently produce empty output);
    /// the caller is responsible for serialising the schema to a JSON
    /// string before calling.
    pub(crate) fn build_command_with_schema(
        &self,
        system_prompt: &str,
        cwd: &Path,
        schema_json: Option<&str>,
    ) -> Command {
        let mut cmd = Command::new(&self.binary_path);
        cmd.arg("-p")
            .arg("--model")
            .arg(&self.model)
            .arg("--output-format")
            .arg("json")
            .arg("--permission-mode")
            .arg("default")
            .arg("--no-session-persistence")
            .arg("--disable-slash-commands")
            .arg("--setting-sources")
            .arg("")
            .arg("--system-prompt")
            .arg(system_prompt);

        if !self.allow_tools {
            cmd.arg("--tools").arg("");
        }

        if !self.allow_mcp {
            cmd.arg("--strict-mcp-config");
        }

        if let Some(budget) = self.max_budget_usd {
            // claude -p expects a decimal dollar amount; use a stable
            // format that round-trips through its parser (no locale
            // formatting, no scientific notation).
            cmd.arg("--max-budget-usd").arg(format!("{budget}"));
        }

        if let Some(json) = schema_json {
            // `claude -p --json-schema` only accepts an inline JSON string;
            // a file path makes the subprocess exit silently with empty
            // output. `Command::arg` skips shell parsing, so escape
            // pitfalls do not apply, and project schemas sit well under
            // ARG_MAX.
            cmd.arg("--json-schema").arg(json);
        }

        cmd.current_dir(cwd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Make the child its own process-group leader so we can later
        // signal the whole group via `killpg`. Without this, `claude -p`
        // helper Node workers spawned at runtime get reparented to PID 1
        // when only the direct PID is killed on timeout, and accumulate
        // as orphans. See issue #633.
        #[cfg(unix)]
        cmd.process_group(0);

        let keep = if self.allow_tools {
            parse_keep_env(std::env::var(KEEP_ENV_VAR).ok().as_deref())
        } else {
            Vec::new()
        };
        self.scrub_env(&mut cmd, std::env::vars().map(|(k, _)| k), &keep);

        cmd
    }

    /// Removes risky env vars from `cmd` rather than clearing wholesale
    /// (clearing the env breaks the Node runtime inside `claude`, which
    /// needs HOME, PATH, possibly DYLD_* / homebrew PATH entries on macOS).
    ///
    /// Always removes the Claude Code re-scoping vars
    /// ([`SCRUBBED_ENV_EXACT`], [`SCRUBBED_ENV_PREFIXES`]). When the
    /// tool-access escape hatch is enabled, additionally removes
    /// secret-looking vars (see [`SECRET_ENV_SUFFIXES`]); `keep` lists
    /// exact names exempted from that secret scrub (resolved from
    /// [`KEEP_ENV_VAR`] by the caller; ignored while tools are disabled).
    ///
    /// Takes the environment as an iterator of keys so tests can exercise
    /// the scrub without mutating the process environment.
    fn scrub_env(
        &self,
        cmd: &mut Command,
        keys: impl IntoIterator<Item = String>,
        keep: &[String],
    ) {
        let mut scrubbed_secrets = Vec::new();
        for k in keys {
            if SCRUBBED_ENV_EXACT.contains(&k.as_str())
                || SCRUBBED_ENV_PREFIXES.iter().any(|p| k.starts_with(p))
            {
                cmd.env_remove(&k);
            } else if self.allow_tools && is_scrubbed_secret(&k, keep) {
                cmd.env_remove(&k);
                scrubbed_secrets.push(k);
            }
        }
        if !scrubbed_secrets.is_empty() {
            scrubbed_secrets.sort_unstable();
            debug!(
                "claude -p secret env scrub (tool-access escape hatch active): \
                 removed {}",
                scrubbed_secrets.join(", ")
            );
        }
    }

    async fn run(&self, system_prompt: &str, user_prompt: &str) -> Result<String> {
        self.run_with_options(system_prompt, user_prompt, &RequestOptions::default())
            .await
    }

    /// Variant of [`Self::run`] that materialises any options on the request.
    ///
    /// Times the whole subprocess run and appends one `service = claude-cli`
    /// record (a non-HTTP entry: the subprocess makes its own API calls) so the
    /// `claude-cli` backend appears in the request log alongside the HTTP ones.
    async fn run_with_options(
        &self,
        system_prompt: &str,
        user_prompt: &str,
        options: &RequestOptions,
    ) -> Result<String> {
        let started = Instant::now();
        let result = self
            .run_with_options_inner(system_prompt, user_prompt, options)
            .await;
        let error = result.as_ref().err().map(|e| format!("{e:#}"));
        request_log::record_http_with(
            "claude-cli",
            "EXEC",
            &self.model,
            started,
            None,
            error.as_deref(),
            request_log::HttpExtra::default(),
        );
        result
    }

    /// Inner implementation of [`Self::run_with_options`] (the actual subprocess
    /// drive); the public method wraps this to time and record the run.
    ///
    /// When `options.response_schema` is `Some`, the schema is serialised
    /// to a JSON string and passed inline via `--json-schema <json>`.
    /// `claude -p` requires the schema on argv; passing a file path makes
    /// the subprocess exit silently with empty output.
    async fn run_with_options_inner(
        &self,
        system_prompt: &str,
        user_prompt: &str,
        options: &RequestOptions,
    ) -> Result<String> {
        let combined_system = format!("{system_prompt}{TOOL_SUPPRESSION_SUFFIX}");

        let temp_dir = tempfile::TempDir::new()
            .context("Failed to create temp directory for claude subprocess")?;

        // `Value::to_string` is infallible; no `?` needed.
        let schema_json = options.response_schema.as_ref().map(ToString::to_string);

        let mut cmd = self.build_command_with_schema(
            &combined_system,
            temp_dir.path(),
            schema_json.as_deref(),
        );

        if self.allow_tools {
            warn!(
                "claude -p sandbox weakened: tool-access escape hatch is enabled \
                 (--claude-cli-allow-tools / OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS). \
                 The nested session can now read, edit, and execute against the \
                 environment it inherits, and its prompt is built from untrusted \
                 content (diffs, commit messages, JIRA text) that could carry a \
                 prompt-injection payload. Well-known secret env vars (*_API_KEY, \
                 *_TOKEN, *_SECRET, *_PASSWORD, *_CREDENTIALS, AWS credentials) \
                 are scrubbed from the subprocess; set \
                 OMNI_DEV_CLAUDE_CLI_KEEP_ENV to exempt specific names."
            );
        }

        if self.allow_mcp {
            warn!(
                "claude -p sandbox weakened: MCP-access escape hatch is enabled \
                 (--claude-cli-allow-mcp / OMNI_DEV_CLAUDE_CLI_ALLOW_MCP). \
                 The nested session can now load MCP servers configured in \
                 ~/.claude/settings.json, exposing any OAuth tokens or \
                 network-attached services they hold."
            );
        }

        info!(
            binary = %self.binary_path.display(),
            model = %self.model,
            allow_tools = self.allow_tools,
            allow_mcp = self.allow_mcp,
            timeout_secs = self.timeout.as_secs(),
            "Spawning claude -p subprocess"
        );

        let mut child = spawn_with_etxtbsy_retry(&mut cmd).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                anyhow::Error::from(ClaudeError::SubprocessBinaryMissing(
                    self.binary_path.display().to_string(),
                ))
            } else {
                anyhow::Error::from(ClaudeError::SubprocessSpawnFailed(e.to_string()))
            }
        })?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to capture claude subprocess stdin"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to capture claude subprocess stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("Failed to capture claude subprocess stderr"))?;

        let cap = self.stdout_cap;
        let prompt_bytes = user_prompt.to_owned();

        // Concurrently: write stdin, read capped stdout, read stderr.
        // Wrapped in a timeout; on timeout we kill and reap the child.
        let io_result = tokio::time::timeout(self.timeout, async {
            let write_task = async move {
                // A child that exits before reading all our input will close
                // its stdin pipe, so subsequent writes get EPIPE. Treat that
                // as a soft signal — the child has already decided its fate
                // and we want to surface the real diagnosis from its exit
                // status and stdout/stderr, not a misleading "broken pipe".
                match stdin.write_all(prompt_bytes.as_bytes()).await {
                    Ok(()) => {}
                    Err(e) if is_pipe_closed(&e) => {
                        debug!("claude subprocess closed stdin before prompt fully written");
                        return Ok::<(), anyhow::Error>(());
                    }
                    Err(e) => {
                        return Err(anyhow::Error::from(e)
                            .context("Failed to write prompt to claude subprocess stdin"));
                    }
                }
                match stdin.shutdown().await {
                    Ok(()) => {}
                    Err(e) if is_pipe_closed(&e) => {
                        debug!("claude subprocess stdin already closed at shutdown");
                    }
                    Err(e) => {
                        return Err(anyhow::Error::from(e)
                            .context("Failed to close claude subprocess stdin"));
                    }
                }
                Ok::<(), anyhow::Error>(())
            };
            let read_stdout_task = read_capped(&mut stdout, cap);
            let read_stderr_task = async {
                let mut buf = Vec::new();
                // stderr read is best-effort; ignore errors.
                let _ = stderr.read_to_end(&mut buf).await;
                Ok::<Vec<u8>, anyhow::Error>(buf)
            };

            let ((), stdout_bytes, stderr_bytes) =
                tokio::try_join!(write_task, read_stdout_task, read_stderr_task)?;
            Ok::<_, anyhow::Error>((stdout_bytes, stderr_bytes))
        })
        .await;

        let (stdout_bytes, stderr_bytes) = match io_result {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                kill_and_reap(&mut child).await;
                return Err(e);
            }
            Err(_) => {
                kill_and_reap(&mut child).await;
                return Err(ClaudeError::SubprocessTimeout {
                    secs: self.timeout.as_secs(),
                }
                .into());
            }
        };

        let status = child
            .wait()
            .await
            .context("Failed to wait for claude subprocess")?;
        let stderr_text = String::from_utf8_lossy(&stderr_bytes).to_string();

        debug!(
            exit_status = ?status,
            stdout_bytes = stdout_bytes.len(),
            stderr_bytes = stderr_bytes.len(),
            "claude -p subprocess finished"
        );

        // Try to parse the JSON envelope. claude -p emits one JSON object on
        // stdout even on API errors (with is_error=true); argparse-level
        // failures go to stderr and produce no JSON.
        let envelope: JsonOutput = match serde_json::from_slice::<JsonOutput>(&stdout_bytes) {
            Ok(env) => env,
            Err(e) => {
                let stdout_text = String::from_utf8_lossy(&stdout_bytes);
                return Err(ClaudeError::SubprocessJsonParseFailed(format!(
                    "{e}; exit_status={status}; stdout={stdout_text}; stderr={stderr_text}"
                ))
                .into());
            }
        };

        // Cost observability: log the total billed cost whenever the CLI
        // reports one, regardless of success / failure. Users running at
        // scale need this to understand spending.
        if let Some(cost) = envelope.total_cost_usd {
            info!(
                total_cost_usd = cost,
                max_budget_usd = ?self.max_budget_usd,
                model = %self.model,
                "claude -p invocation cost"
            );
            if let Some(budget) = self.max_budget_usd {
                if cost > budget {
                    // claude -p enforces the cap itself, but warn in case
                    // its enforcement differs from ours.
                    warn!(
                        total_cost_usd = cost,
                        max_budget_usd = budget,
                        "claude -p reported cost above the configured budget cap"
                    );
                }
            }
        }

        if envelope.is_error {
            return Err(map_api_error(&envelope, &stderr_text));
        }

        if !status.success() {
            return Err(ClaudeError::ApiRequestFailed(format!(
                "claude -p exited with non-zero status ({status}); stderr={stderr_text}"
            ))
            .into());
        }

        let result = strip_wrapping_code_fence(&envelope.result);
        super::log_response_success("Claude CLI", &Ok(result.clone()));
        Ok(result)
    }
}

/// Strips a single outer markdown code fence wrapping the entire response.
///
/// `claude -p` tends to wrap structured outputs (YAML, JSON) in a top-level
/// ``` ```yaml ... ``` ``` block even when the prompt didn't ask for it. Some
/// downstream parsers in this crate (e.g. `parse_pr_response`) expect bare
/// YAML and do not tolerate that wrapper.
///
/// Conservative rules:
/// - Only strips if the *entire* trimmed response is wrapped in one fence.
/// - Leaves internal fences (e.g. inside a multi-line `description`) alone.
/// - Returns the trimmed-original on any ambiguity.
fn strip_wrapping_code_fence(raw: &str) -> String {
    let trimmed = raw.trim();
    let Some(after_open) = trimmed.strip_prefix("```") else {
        return trimmed.to_string();
    };
    // Drop optional language tag on the same line as the opening fence.
    let body = match after_open.find('\n') {
        Some(i) => &after_open[i + 1..],
        None => return trimmed.to_string(),
    };
    let Some(without_trailing) = body.trim_end().strip_suffix("```") else {
        return trimmed.to_string();
    };
    // Bail out if there's another fence inside — the content may legitimately
    // contain its own fenced blocks (e.g. a PR description).
    if without_trailing.contains("```") {
        return trimmed.to_string();
    }
    without_trailing.trim_end().to_string()
}

impl AiClient for ClaudeCliAiClient {
    fn send_request<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            debug!(
                system_prompt_len = system_prompt.len(),
                user_prompt_len = user_prompt.len(),
                model = %self.model,
                "Preparing claude -p subprocess request"
            );
            self.run(system_prompt, user_prompt).await
        })
    }

    fn capabilities(&self) -> AiClientCapabilities {
        AiClientCapabilities {
            supports_response_schema: true,
        }
    }

    fn send_request_with_options<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
        options: RequestOptions,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        Box::pin(async move {
            debug!(
                system_prompt_len = system_prompt.len(),
                user_prompt_len = user_prompt.len(),
                has_schema = options.response_schema.is_some(),
                model = %self.model,
                "Preparing claude -p subprocess request (with options)"
            );
            self.run_with_options(system_prompt, user_prompt, &options)
                .await
        })
    }

    fn get_metadata(&self) -> AiClientMetadata {
        // Resolve aliases (sonnet/opus/haiku) to concrete registry IDs for
        // token-budget math; keep the original alias in the reported model
        // string so users see what they asked for.
        let effective_id = resolve_alias(&self.model);
        let (max_context_length, max_response_length) =
            super::registry_model_limits(&effective_id, &None);

        AiClientMetadata {
            provider: "Claude CLI".to_string(),
            model: self.model.clone(),
            max_context_length,
            max_response_length,
            active_beta: None,
        }
    }
}

/// Maps `claude -p` alias names to concrete API identifiers for the model
/// registry lookup. Unknown names pass through unchanged.
fn resolve_alias(model: &str) -> String {
    match model {
        "haiku" => "claude-haiku-4-5-20251001".to_string(),
        "sonnet" => "claude-sonnet-4-6".to_string(),
        "opus" => "claude-opus-4-6".to_string(),
        other => other.to_string(),
    }
}

/// Converts a `claude -p` error envelope into a typed error.
fn map_api_error(env: &JsonOutput, stderr: &str) -> anyhow::Error {
    let status = env.api_error_status;
    let msg = &env.result;
    match status {
        Some(401 | 403) => ClaudeError::ApiRequestFailed(format!(
            "claude -p authentication failed ({status:?}): {msg}; stderr={stderr}"
        ))
        .into(),
        Some(404) => {
            ClaudeError::ApiRequestFailed(format!("claude -p reported unknown model (404): {msg}"))
                .into()
        }
        Some(429) => ClaudeError::RateLimitExceeded.into(),
        Some(code) if (500..=599).contains(&code) => ClaudeError::ApiRequestFailed(format!(
            "claude -p transient API error ({code}): {msg}; stderr={stderr}"
        ))
        .into(),
        _ => ClaudeError::ApiRequestFailed(format!(
            "claude -p reported error (api_error_status={status:?}): {msg}; stderr={stderr}"
        ))
        .into(),
    }
}

/// Returns true if `err` indicates the peer closed the pipe.
fn is_pipe_closed(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
    )
}

/// Reads from `reader` until EOF, erroring if output exceeds `cap` bytes.
async fn read_capped<R>(reader: &mut R, cap: usize) -> Result<Vec<u8>>
where
    R: AsyncReadExt + Unpin,
{
    let mut buf = Vec::with_capacity(4096.min(cap));
    let mut chunk = [0u8; 4096];
    loop {
        let n = reader
            .read(&mut chunk)
            .await
            .context("Failed to read claude subprocess stdout")?;
        if n == 0 {
            break;
        }
        if buf.len().saturating_add(n) > cap {
            warn!(cap, "claude subprocess stdout exceeded cap");
            return Err(ClaudeError::SubprocessOutputTooLarge { limit: cap }.into());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Ok(buf)
}

/// Kills the subprocess and reaps it.
///
/// On Unix, sends SIGKILL to the entire process group so any helpers
/// the child forked (e.g. Node workers spawned by `claude -p`) die
/// alongside the direct child. Pairs with the `process_group(0)` call
/// in `ClaudeCliAiClient::build_command_with_schema`. See issue #633.
///
/// On non-Unix, falls back to `tokio::process::Child::kill`, which
/// already terminates the process tree on Windows via
/// `TerminateProcess`.
async fn kill_and_reap(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            // Cast through i32: tokio's PID is u32; nix's Pid wraps i32
            // (matching the kernel's pid_t). PIDs always fit in i32 in
            // practice — Linux caps at PID_MAX_LIMIT (~2^22) and macOS
            // at 99999.
            let group = nix::unistd::Pid::from_raw(pid as i32);
            if let Err(e) = nix::sys::signal::killpg(group, nix::sys::signal::Signal::SIGKILL) {
                // ESRCH is expected when the group has already drained
                // (everyone exited between timeout and kill). Anything
                // else is worth logging but not fatal — we still wait
                // below to reap whatever remains of the direct child.
                if e != nix::errno::Errno::ESRCH {
                    debug!(error = %e, pid, "killpg failed; falling back to direct kill");
                    let _ = child.start_kill();
                }
            }
        } else {
            // Already reaped; nothing to signal.
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill().await;
    }
    let _ = child.wait().await;
}

/// `execve` returns `ETXTBSY` (errno 26 on Linux) when *any* process
/// holds a writable FD on the target binary. Under high test parallelism
/// a sibling thread can `fork()` between our open-for-write and drop of
/// the shim FD — `O_CLOEXEC` only closes on `execve`, not on bare
/// `fork`, so the child inherits our writable FD and the kernel blocks
/// our own `execve` of that same file until the child execs (or dies).
/// Retry with bounded exponential backoff. See issue #642.
async fn spawn_with_etxtbsy_retry(cmd: &mut Command) -> std::io::Result<tokio::process::Child> {
    const ETXTBSY: i32 = 26;
    const MAX_ATTEMPTS: u32 = 6;

    let mut backoff = Duration::from_millis(5);
    for attempt in 1..=MAX_ATTEMPTS {
        match cmd.spawn() {
            Ok(child) => return Ok(child),
            Err(e) if e.raw_os_error() == Some(ETXTBSY) && attempt < MAX_ATTEMPTS => {
                debug!(
                    attempt,
                    backoff_ms = backoff.as_millis() as u64,
                    "spawn hit ETXTBSY; retrying"
                );
                tokio::time::sleep(backoff).await;
                backoff = backoff.saturating_mul(2);
            }
            Err(e) => return Err(e),
        }
    }
    unreachable!("loop exits via return")
}

/// Test-only mutex serialising every test that mutates global state read
/// by `claude-cli` (env vars and process-global config).
///
/// Shared across this module's tests **and** `crate::cli`'s tests because
/// `Cli::propagate_global_flags` forwards CLI flags to the same env vars
/// (`OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS`, `OMNI_DEV_CLAUDE_CLI_ALLOW_MCP`,
/// `OMNI_DEV_CLAUDE_CLI_MAX_BUDGET_USD`, `OMNI_DEV_AI_BACKEND`) that the
/// guards below snapshot. A single shared mutex eliminates cross-module
/// races and avoids multi-lock deadlock entirely.
#[cfg(test)]
pub(crate) static CLI_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::await_holding_lock)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use tempfile::TempDir;

    fn client_with_defaults(model: &str) -> ClaudeCliAiClient {
        ClaudeCliAiClient::new_with_config(
            model.to_string(),
            DEFAULT_TIMEOUT,
            DEFAULT_STDOUT_CAP,
            false,
            PathBuf::from("claude"),
        )
    }

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.as_std()
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn build_command_includes_sandbox_flags() {
        let cli = client_with_defaults("sonnet");
        let tmp = TempDir::new().unwrap();
        let cmd = cli.build_command("sys-prompt", tmp.path());
        let args = args_of(&cmd);

        assert!(args.contains(&"-p".to_string()), "missing -p: {args:?}");
        assert!(
            args.contains(&"--model".to_string()) && args.contains(&"sonnet".to_string()),
            "missing --model sonnet: {args:?}"
        );
        assert!(
            args.contains(&"--output-format".to_string()) && args.contains(&"json".to_string()),
            "missing --output-format json: {args:?}"
        );
        assert!(
            args.contains(&"--tools".to_string()),
            "missing --tools: {args:?}"
        );
        assert!(
            args.contains(&"--strict-mcp-config".to_string()),
            "missing --strict-mcp-config: {args:?}"
        );
        assert!(
            args.contains(&"--setting-sources".to_string()),
            "missing --setting-sources: {args:?}"
        );
        assert!(
            args.contains(&"--disable-slash-commands".to_string()),
            "missing --disable-slash-commands: {args:?}"
        );
        assert!(
            args.contains(&"--no-session-persistence".to_string()),
            "missing --no-session-persistence: {args:?}"
        );
        assert!(
            args.contains(&"--permission-mode".to_string())
                && args.contains(&"default".to_string()),
            "missing --permission-mode default: {args:?}"
        );
        assert!(
            args.contains(&"--system-prompt".to_string()),
            "missing --system-prompt: {args:?}"
        );
    }

    #[test]
    fn build_command_does_not_include_add_dir() {
        let cli = client_with_defaults("sonnet");
        let tmp = TempDir::new().unwrap();
        let cmd = cli.build_command("sys", tmp.path());
        let args = args_of(&cmd);
        assert!(
            !args.contains(&"--add-dir".to_string()),
            "must not pass --add-dir: {args:?}"
        );
        assert!(
            !args.contains(&"--mcp-config".to_string()),
            "must not pass --mcp-config (strict-mcp-config with no config = lockdown)"
        );
    }

    #[test]
    fn build_command_uses_temp_cwd_not_parent() {
        let cli = client_with_defaults("sonnet");
        let tmp = TempDir::new().unwrap();
        let cmd = cli.build_command("sys", tmp.path());
        assert_eq!(
            cmd.as_std().get_current_dir().map(Path::to_path_buf),
            Some(tmp.path().to_path_buf())
        );
    }

    #[test]
    fn build_command_appends_tool_suppression_in_system_prompt() {
        // The run() method appends the suffix before calling build_command.
        // build_command itself takes the system prompt verbatim, so we
        // verify the argv just echoes what we pass in.
        let cli = client_with_defaults("sonnet");
        let tmp = TempDir::new().unwrap();
        let with_suffix = format!("my system prompt{TOOL_SUPPRESSION_SUFFIX}");
        let cmd = cli.build_command(&with_suffix, tmp.path());
        let args = args_of(&cmd);
        let sys_idx = args
            .iter()
            .position(|a| a == "--system-prompt")
            .expect("--system-prompt present");
        let sys_val = &args[sys_idx + 1];
        assert!(
            sys_val.contains("Do not emit function_calls XML"),
            "system prompt should contain tool-suppression instruction: {sys_val}"
        );
    }

    #[test]
    fn build_command_scrubs_claude_project_env() {
        std::env::set_var("CLAUDE_PROJECT_DIR", "/should/not/leak");
        std::env::set_var("CLAUDE_CODE_ENTRYPOINT", "cli");
        std::env::set_var("CLAUDE_PROJECT_SOMETHING", "x");
        let cli = client_with_defaults("sonnet");
        let tmp = TempDir::new().unwrap();
        let cmd = cli.build_command("sys", tmp.path());

        // env_remove entries show up as (key, None) in get_envs.
        let env: Vec<_> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(OsStr::to_os_string)))
            .collect();

        let was_removed = |key: &str| -> bool { env.iter().any(|(k, v)| k == key && v.is_none()) };

        assert!(
            was_removed("CLAUDE_PROJECT_DIR"),
            "CLAUDE_PROJECT_DIR should be scrubbed: {env:?}"
        );
        assert!(
            was_removed("CLAUDE_CODE_ENTRYPOINT"),
            "CLAUDE_CODE_ENTRYPOINT should be scrubbed: {env:?}"
        );
        assert!(
            was_removed("CLAUDE_PROJECT_SOMETHING"),
            "CLAUDE_PROJECT_SOMETHING should be scrubbed: {env:?}"
        );

        std::env::remove_var("CLAUDE_PROJECT_DIR");
        std::env::remove_var("CLAUDE_CODE_ENTRYPOINT");
        std::env::remove_var("CLAUDE_PROJECT_SOMETHING");
    }

    fn client_with_allow_tools(model: &str) -> ClaudeCliAiClient {
        ClaudeCliAiClient::new_with_config(
            model.to_string(),
            DEFAULT_TIMEOUT,
            DEFAULT_STDOUT_CAP,
            true,
            PathBuf::from("claude"),
        )
    }

    /// Collects the keys `scrub_env` recorded as removed on `cmd`
    /// (env_remove entries show up as `(key, None)` in `get_envs`).
    fn removed_env_keys(cmd: &Command) -> Vec<String> {
        cmd.as_std()
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn scrub_env_removes_secrets_when_tools_allowed() {
        let cli = client_with_allow_tools("sonnet");
        let mut cmd = Command::new("claude");
        let keys = [
            "MY_API_KEY",
            "GITHUB_TOKEN",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "DB_PASSWORD",
            "CLIENT_SECRET",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "PATH",
            "HOME",
            "CLAUDE_PROJECT_DIR",
        ]
        .map(String::from);
        cli.scrub_env(&mut cmd, keys, &[]);
        let removed = removed_env_keys(&cmd);

        for key in [
            "MY_API_KEY",
            "GITHUB_TOKEN",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "DB_PASSWORD",
            "CLIENT_SECRET",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "CLAUDE_PROJECT_DIR",
        ] {
            assert!(
                removed.contains(&key.to_string()),
                "{key} should be scrubbed: {removed:?}"
            );
        }
        for key in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "PATH", "HOME"] {
            assert!(
                !removed.contains(&key.to_string()),
                "{key} should be kept: {removed:?}"
            );
        }
    }

    #[test]
    fn scrub_env_leaves_secrets_when_tools_disabled() {
        let cli = client_with_defaults("sonnet");
        let mut cmd = Command::new("claude");
        let keys = ["MY_API_KEY", "GITHUB_TOKEN", "CLAUDE_PROJECT_DIR"].map(String::from);
        cli.scrub_env(&mut cmd, keys, &[]);
        let removed = removed_env_keys(&cmd);

        assert_eq!(
            removed,
            vec!["CLAUDE_PROJECT_DIR".to_string()],
            "only the re-scoping var should be scrubbed on the default path"
        );
    }

    #[test]
    fn scrub_env_honours_keep_list() {
        let cli = client_with_allow_tools("sonnet");
        let mut cmd = Command::new("claude");
        let keys = ["GITHUB_TOKEN", "NPM_TOKEN"].map(String::from);
        cli.scrub_env(&mut cmd, keys, &["GITHUB_TOKEN".to_string()]);
        let removed = removed_env_keys(&cmd);

        assert!(
            removed.contains(&"NPM_TOKEN".to_string()),
            "NPM_TOKEN should be scrubbed: {removed:?}"
        );
        assert!(
            !removed.contains(&"GITHUB_TOKEN".to_string()),
            "kept GITHUB_TOKEN should survive: {removed:?}"
        );
    }

    #[test]
    fn parse_keep_env_handles_none_and_empty() {
        assert!(parse_keep_env(None).is_empty());
        assert!(parse_keep_env(Some("")).is_empty());
        assert!(parse_keep_env(Some(" , ,")).is_empty());
    }

    #[test]
    fn parse_keep_env_splits_and_trims() {
        assert_eq!(
            parse_keep_env(Some(" GITHUB_TOKEN , NPM_TOKEN ")),
            vec!["GITHUB_TOKEN".to_string(), "NPM_TOKEN".to_string()]
        );
    }

    #[test]
    fn is_scrubbed_secret_matches_suffixes_and_exact_names() {
        for key in [
            "FOO_API_KEY",
            "GH_TOKEN",
            "AWS_SESSION_TOKEN",
            "CLIENT_SECRET",
            "DB_PASSWORD",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(is_scrubbed_secret(key, &[]), "{key} should match");
        }
    }

    #[test]
    fn is_scrubbed_secret_ignores_non_secret_names() {
        for key in ["PATH", "HOME", "EDITOR", "TOKENIZER", "API_KEYRING"] {
            assert!(!is_scrubbed_secret(key, &[]), "{key} should not match");
        }
    }

    #[test]
    fn is_scrubbed_secret_keep_lists_take_precedence() {
        assert!(!is_scrubbed_secret("ANTHROPIC_API_KEY", &[]));
        assert!(!is_scrubbed_secret("ANTHROPIC_AUTH_TOKEN", &[]));
        assert!(!is_scrubbed_secret(
            "GITHUB_TOKEN",
            &["GITHUB_TOKEN".to_string()]
        ));
    }

    #[test]
    fn build_command_with_allow_tools_omits_tools_flag() {
        let cli = ClaudeCliAiClient::new_with_config(
            "sonnet".to_string(),
            DEFAULT_TIMEOUT,
            DEFAULT_STDOUT_CAP,
            true,
            PathBuf::from("claude"),
        );
        let tmp = TempDir::new().unwrap();
        let cmd = cli.build_command("sys", tmp.path());
        let args = args_of(&cmd);
        // --tools flag should be absent when allow_tools=true (lets the
        // CLI use its default tool set).
        assert!(
            !args.contains(&"--tools".to_string()),
            "allow_tools=true should not pass --tools: {args:?}"
        );
    }

    /// Four-way matrix between the two escape hatches. Asserts that
    /// `allow_tools` and `allow_mcp` toggle independent argv flags.
    fn build_args(allow_tools: bool, allow_mcp: bool) -> Vec<String> {
        let cli = ClaudeCliAiClient::new_with_config(
            "sonnet".to_string(),
            DEFAULT_TIMEOUT,
            DEFAULT_STDOUT_CAP,
            allow_tools,
            PathBuf::from("claude"),
        )
        .with_allow_mcp(allow_mcp);
        let tmp = TempDir::new().unwrap();
        args_of(&cli.build_command("sys", tmp.path()))
    }

    #[test]
    fn matrix_default_includes_both_guards() {
        let args = build_args(false, false);
        assert!(
            args.contains(&"--tools".to_string()),
            "default must keep --tools: {args:?}"
        );
        assert!(
            args.contains(&"--strict-mcp-config".to_string()),
            "default must keep --strict-mcp-config: {args:?}"
        );
    }

    #[test]
    fn matrix_allow_mcp_alone_drops_only_strict_mcp() {
        let args = build_args(false, true);
        assert!(
            args.contains(&"--tools".to_string()),
            "allow_mcp alone must keep --tools: {args:?}"
        );
        assert!(
            !args.contains(&"--strict-mcp-config".to_string()),
            "allow_mcp alone must drop --strict-mcp-config: {args:?}"
        );
    }

    #[test]
    fn matrix_allow_tools_alone_drops_only_tools() {
        let args = build_args(true, false);
        assert!(
            !args.contains(&"--tools".to_string()),
            "allow_tools alone must drop --tools: {args:?}"
        );
        assert!(
            args.contains(&"--strict-mcp-config".to_string()),
            "allow_tools alone must keep --strict-mcp-config: {args:?}"
        );
    }

    #[test]
    fn matrix_both_drops_both_guards() {
        let args = build_args(true, true);
        assert!(
            !args.contains(&"--tools".to_string()),
            "both flags must drop --tools: {args:?}"
        );
        assert!(
            !args.contains(&"--strict-mcp-config".to_string()),
            "both flags must drop --strict-mcp-config: {args:?}"
        );
    }

    /// Test-scoped guard for `OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS`. Serialises
    /// against every other env-mutating test via the shared
    /// [`CLI_ENV_LOCK`].
    struct AllowToolsEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Option<String>,
    }

    impl AllowToolsEnvGuard {
        fn new() -> Self {
            let lock = CLI_ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let saved = std::env::var(ALLOW_TOOLS_ENV_VAR).ok();
            std::env::remove_var(ALLOW_TOOLS_ENV_VAR);
            Self { _lock: lock, saved }
        }

        fn set(&self, value: &str) {
            std::env::set_var(ALLOW_TOOLS_ENV_VAR, value);
        }
    }

    impl Drop for AllowToolsEnvGuard {
        fn drop(&mut self) {
            match self.saved.take() {
                Some(v) => std::env::set_var(ALLOW_TOOLS_ENV_VAR, v),
                None => std::env::remove_var(ALLOW_TOOLS_ENV_VAR),
            }
        }
    }

    #[test]
    fn allow_tools_from_env_defaults_to_false_when_unset() {
        let _g = AllowToolsEnvGuard::new();
        assert!(!ClaudeCliAiClient::allow_tools_from_env());
    }

    #[test]
    fn allow_tools_from_env_true() {
        let g = AllowToolsEnvGuard::new();
        g.set("true");
        assert!(ClaudeCliAiClient::allow_tools_from_env());
    }

    #[test]
    fn allow_tools_from_env_true_case_insensitive_and_trimmed() {
        let g = AllowToolsEnvGuard::new();
        g.set("  TRUE  ");
        assert!(ClaudeCliAiClient::allow_tools_from_env());
    }

    #[test]
    fn allow_tools_from_env_one_and_yes_accepted() {
        let g = AllowToolsEnvGuard::new();
        g.set("1");
        assert!(ClaudeCliAiClient::allow_tools_from_env());
        g.set("yes");
        assert!(ClaudeCliAiClient::allow_tools_from_env());
    }

    #[test]
    fn allow_tools_from_env_other_values_are_false() {
        let g = AllowToolsEnvGuard::new();
        for v in ["false", "0", "no", "off", "TRUE1", "YES!", ""] {
            g.set(v);
            assert!(
                !ClaudeCliAiClient::allow_tools_from_env(),
                "value {v:?} should not enable the escape hatch"
            );
        }
    }

    #[test]
    fn new_picks_up_allow_tools_env_var() {
        let g = AllowToolsEnvGuard::new();
        g.set("true");
        let cli = ClaudeCliAiClient::new("sonnet".to_string());
        // Verify via build_command — allow_tools=true omits --tools.
        let tmp = TempDir::new().unwrap();
        let cmd = cli.build_command("sys", tmp.path());
        let args = args_of(&cmd);
        assert!(
            !args.contains(&"--tools".to_string()),
            "ALLOW_TOOLS=true should omit --tools in argv: {args:?}"
        );
    }

    #[test]
    fn new_defaults_to_tools_disabled_when_env_unset() {
        let _g = AllowToolsEnvGuard::new();
        let cli = ClaudeCliAiClient::new("sonnet".to_string());
        let tmp = TempDir::new().unwrap();
        let cmd = cli.build_command("sys", tmp.path());
        let args = args_of(&cmd);
        assert!(
            args.contains(&"--tools".to_string()),
            "default (no env) must include --tools \"\": {args:?}"
        );
    }

    /// Test-scoped guard for `OMNI_DEV_CLAUDE_CLI_ALLOW_MCP`. Serialises
    /// against every other env-mutating test via the shared
    /// [`CLI_ENV_LOCK`].
    struct AllowMcpEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Option<String>,
    }

    impl AllowMcpEnvGuard {
        fn new() -> Self {
            let lock = CLI_ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let saved = std::env::var(ALLOW_MCP_ENV_VAR).ok();
            std::env::remove_var(ALLOW_MCP_ENV_VAR);
            Self { _lock: lock, saved }
        }

        fn set(&self, value: &str) {
            std::env::set_var(ALLOW_MCP_ENV_VAR, value);
        }
    }

    impl Drop for AllowMcpEnvGuard {
        fn drop(&mut self) {
            match self.saved.take() {
                Some(v) => std::env::set_var(ALLOW_MCP_ENV_VAR, v),
                None => std::env::remove_var(ALLOW_MCP_ENV_VAR),
            }
        }
    }

    #[test]
    fn allow_mcp_from_env_defaults_to_false_when_unset() {
        let _g = AllowMcpEnvGuard::new();
        assert!(!ClaudeCliAiClient::allow_mcp_from_env());
    }

    #[test]
    fn allow_mcp_from_env_true() {
        let g = AllowMcpEnvGuard::new();
        g.set("true");
        assert!(ClaudeCliAiClient::allow_mcp_from_env());
    }

    #[test]
    fn allow_mcp_from_env_true_case_insensitive_and_trimmed() {
        let g = AllowMcpEnvGuard::new();
        g.set("  TRUE  ");
        assert!(ClaudeCliAiClient::allow_mcp_from_env());
    }

    #[test]
    fn allow_mcp_from_env_one_and_yes_accepted() {
        let g = AllowMcpEnvGuard::new();
        g.set("1");
        assert!(ClaudeCliAiClient::allow_mcp_from_env());
        g.set("yes");
        assert!(ClaudeCliAiClient::allow_mcp_from_env());
    }

    #[test]
    fn allow_mcp_from_env_other_values_are_false() {
        let g = AllowMcpEnvGuard::new();
        for v in ["false", "0", "no", "off", "TRUE1", "YES!", ""] {
            g.set(v);
            assert!(
                !ClaudeCliAiClient::allow_mcp_from_env(),
                "value {v:?} should not enable the escape hatch"
            );
        }
    }

    #[test]
    fn new_picks_up_allow_mcp_env_var() {
        // Single guard locks `CLI_ENV_LOCK`; we manually snapshot
        // `ALLOW_TOOLS_ENV_VAR` here because acquiring a second guard would
        // deadlock on the shared mutex.
        let mcp_guard = AllowMcpEnvGuard::new();
        let saved_tools = std::env::var(ALLOW_TOOLS_ENV_VAR).ok();
        std::env::remove_var(ALLOW_TOOLS_ENV_VAR);
        mcp_guard.set("true");
        let cli = ClaudeCliAiClient::new("sonnet".to_string());
        let tmp = TempDir::new().unwrap();
        let cmd = cli.build_command("sys", tmp.path());
        let args = args_of(&cmd);
        match saved_tools {
            Some(v) => std::env::set_var(ALLOW_TOOLS_ENV_VAR, v),
            None => std::env::remove_var(ALLOW_TOOLS_ENV_VAR),
        }
        assert!(
            !args.contains(&"--strict-mcp-config".to_string()),
            "ALLOW_MCP=true should omit --strict-mcp-config: {args:?}"
        );
        // --tools must still be present when only MCP is enabled.
        assert!(
            args.contains(&"--tools".to_string()),
            "ALLOW_MCP=true alone should keep --tools: {args:?}"
        );
    }

    #[test]
    fn new_defaults_to_strict_mcp_when_env_unset() {
        let _g = AllowMcpEnvGuard::new();
        let cli = ClaudeCliAiClient::new("sonnet".to_string());
        let tmp = TempDir::new().unwrap();
        let cmd = cli.build_command("sys", tmp.path());
        let args = args_of(&cmd);
        assert!(
            args.contains(&"--strict-mcp-config".to_string()),
            "default (no env) must include --strict-mcp-config: {args:?}"
        );
    }

    // ── Budget cap tests (MAX_BUDGET_ENV_VAR / with_max_budget_usd) ──

    /// Test-scoped guard for `OMNI_DEV_CLAUDE_CLI_MAX_BUDGET_USD`. Serialises
    /// against every other env-mutating test via the shared
    /// [`CLI_ENV_LOCK`].
    struct MaxBudgetEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Option<String>,
    }

    impl MaxBudgetEnvGuard {
        fn new() -> Self {
            let lock = CLI_ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let saved = std::env::var(MAX_BUDGET_ENV_VAR).ok();
            std::env::remove_var(MAX_BUDGET_ENV_VAR);
            Self { _lock: lock, saved }
        }

        fn set(&self, value: &str) {
            std::env::set_var(MAX_BUDGET_ENV_VAR, value);
        }
    }

    impl Drop for MaxBudgetEnvGuard {
        fn drop(&mut self) {
            match self.saved.take() {
                Some(v) => std::env::set_var(MAX_BUDGET_ENV_VAR, v),
                None => std::env::remove_var(MAX_BUDGET_ENV_VAR),
            }
        }
    }

    #[test]
    fn max_budget_from_env_unset_is_none() {
        let _g = MaxBudgetEnvGuard::new();
        assert!(ClaudeCliAiClient::max_budget_from_env().is_none());
    }

    #[test]
    fn max_budget_from_env_parses_decimal() {
        let g = MaxBudgetEnvGuard::new();
        g.set("0.50");
        assert_eq!(ClaudeCliAiClient::max_budget_from_env(), Some(0.50));
        g.set("2.5");
        assert_eq!(ClaudeCliAiClient::max_budget_from_env(), Some(2.5));
    }

    #[test]
    fn max_budget_from_env_trims_whitespace() {
        let g = MaxBudgetEnvGuard::new();
        g.set("  1.25  ");
        assert_eq!(ClaudeCliAiClient::max_budget_from_env(), Some(1.25));
    }

    #[test]
    fn max_budget_from_env_rejects_non_positive() {
        let g = MaxBudgetEnvGuard::new();
        g.set("0");
        assert!(ClaudeCliAiClient::max_budget_from_env().is_none());
        g.set("-1.0");
        assert!(ClaudeCliAiClient::max_budget_from_env().is_none());
    }

    #[test]
    fn max_budget_from_env_rejects_non_finite() {
        let g = MaxBudgetEnvGuard::new();
        g.set("nan");
        assert!(ClaudeCliAiClient::max_budget_from_env().is_none());
        g.set("inf");
        assert!(ClaudeCliAiClient::max_budget_from_env().is_none());
    }

    #[test]
    fn max_budget_from_env_rejects_garbage() {
        let g = MaxBudgetEnvGuard::new();
        g.set("five dollars");
        assert!(ClaudeCliAiClient::max_budget_from_env().is_none());
        g.set("");
        assert!(ClaudeCliAiClient::max_budget_from_env().is_none());
    }

    /// Thread-scoped log capture for asserting on emitted `warn!` lines.
    /// Installed via `tracing::subscriber::with_default`, so it never
    /// touches the global subscriber other tests may have initialised.
    #[derive(Clone, Default)]
    struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Runs `f` under a thread-local WARN-level subscriber and returns
    /// everything it logged.
    fn capture_warnings(f: impl FnOnce()) -> String {
        let writer = CaptureWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let mut sink = writer.clone();
        std::io::Write::flush(&mut sink).expect("flushing the capture buffer cannot fail");
        let logs = String::from_utf8_lossy(&writer.0.lock().unwrap()).into_owned();
        logs
    }

    #[test]
    fn max_budget_from_value_warns_on_non_positive() {
        let logs = capture_warnings(|| {
            assert!(ClaudeCliAiClient::max_budget_from_value("0").is_none());
        });
        assert!(
            logs.contains("invalid spending cap"),
            "expected a WARN for a non-positive cap, got: {logs}"
        );
        assert!(
            logs.contains(MAX_BUDGET_ENV_VAR),
            "WARN should name the env var, got: {logs}"
        );
    }

    #[test]
    fn max_budget_from_value_warns_on_unusable_values() {
        for raw in ["-1.0", "nan", "inf", "five dollars", ""] {
            let logs = capture_warnings(|| {
                assert!(
                    ClaudeCliAiClient::max_budget_from_value(raw).is_none(),
                    "{raw:?} should yield no cap"
                );
            });
            assert!(
                logs.contains("invalid spending cap"),
                "expected a WARN for {raw:?}, got: {logs}"
            );
        }
    }

    #[test]
    fn max_budget_from_value_valid_is_silent() {
        let logs = capture_warnings(|| {
            assert_eq!(ClaudeCliAiClient::max_budget_from_value("0.50"), Some(0.50));
        });
        assert!(logs.is_empty(), "no WARN expected for a valid cap: {logs}");
    }

    #[test]
    fn build_command_omits_max_budget_when_unset() {
        let cli = client_with_defaults("sonnet");
        let tmp = TempDir::new().unwrap();
        let cmd = cli.build_command("sys", tmp.path());
        let args = args_of(&cmd);
        assert!(
            !args.contains(&"--max-budget-usd".to_string()),
            "no budget → argv must omit --max-budget-usd: {args:?}"
        );
    }

    #[test]
    fn build_command_includes_max_budget_when_set() {
        let cli = client_with_defaults("sonnet").with_max_budget_usd(Some(0.50));
        let tmp = TempDir::new().unwrap();
        let cmd = cli.build_command("sys", tmp.path());
        let args = args_of(&cmd);
        let idx = args
            .iter()
            .position(|a| a == "--max-budget-usd")
            .expect("argv should contain --max-budget-usd");
        assert_eq!(args[idx + 1], "0.5");
    }

    #[test]
    fn new_picks_up_max_budget_env_var() {
        let g = MaxBudgetEnvGuard::new();
        g.set("1.25");
        let cli = ClaudeCliAiClient::new("sonnet".to_string());
        let tmp = TempDir::new().unwrap();
        let cmd = cli.build_command("sys", tmp.path());
        let args = args_of(&cmd);
        let idx = args
            .iter()
            .position(|a| a == "--max-budget-usd")
            .expect("argv should contain --max-budget-usd when env set");
        assert_eq!(args[idx + 1], "1.25");
    }

    #[test]
    fn with_max_budget_usd_none_clears_budget() {
        let cli = client_with_defaults("sonnet")
            .with_max_budget_usd(Some(1.0))
            .with_max_budget_usd(None);
        let tmp = TempDir::new().unwrap();
        let cmd = cli.build_command("sys", tmp.path());
        let args = args_of(&cmd);
        assert!(!args.contains(&"--max-budget-usd".to_string()));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn cost_is_extracted_from_json_envelope_and_run_succeeds() {
        let _guard = shim_lock();
        // Verifies the JSON envelope's total_cost_usd is parsed without
        // error and the happy path still returns the result.
        let tmp = TempDir::new().unwrap();
        let shim = make_shim(
            &tmp,
            r#"{"is_error":false,"result":"ok","total_cost_usd":0.0123}"#,
            0,
        );
        let cli = client_with_shim(shim).with_max_budget_usd(Some(1.0));
        let out = cli.run("sys", "user").await.unwrap();
        assert_eq!(out, "ok");
    }

    #[test]
    fn resolve_alias_known() {
        assert_eq!(resolve_alias("haiku"), "claude-haiku-4-5-20251001");
        assert_eq!(resolve_alias("sonnet"), "claude-sonnet-4-6");
        assert_eq!(resolve_alias("opus"), "claude-opus-4-6");
    }

    #[test]
    fn resolve_alias_passthrough() {
        assert_eq!(resolve_alias("claude-sonnet-4-6"), "claude-sonnet-4-6");
        assert_eq!(
            resolve_alias("claude-haiku-4-5-20251001"),
            "claude-haiku-4-5-20251001"
        );
    }

    #[test]
    fn metadata_has_claude_cli_provider() {
        let cli = client_with_defaults("sonnet");
        let meta = cli.get_metadata();
        assert_eq!(meta.provider, "Claude CLI");
        assert_eq!(meta.model, "sonnet");
        assert!(meta.max_context_length > 0);
        assert!(meta.max_response_length > 0);
        assert!(meta.active_beta.is_none());
    }

    #[test]
    fn metadata_prompt_style_is_claude() {
        use crate::claude::ai::PromptStyle;
        let cli = client_with_defaults("sonnet");
        assert_eq!(cli.get_metadata().prompt_style(), PromptStyle::Claude);
    }

    #[test]
    fn capabilities_advertise_response_schema_support() {
        let cli = client_with_defaults("sonnet");
        let caps = cli.capabilities();
        assert!(
            caps.supports_response_schema,
            "claude-cli backend should advertise response-schema support"
        );
    }

    #[test]
    fn build_command_omits_json_schema_when_no_path_given() {
        let cli = client_with_defaults("sonnet");
        let tmp = TempDir::new().unwrap();
        let cmd = cli.build_command_with_schema("sys", tmp.path(), None);
        let args = args_of(&cmd);
        assert!(
            !args.contains(&"--json-schema".to_string()),
            "no schema → argv must omit --json-schema: {args:?}"
        );
    }

    #[test]
    fn build_command_includes_inline_json_schema_when_given() {
        let cli = client_with_defaults("sonnet");
        let tmp = TempDir::new().unwrap();
        let schema_json = r#"{"type":"object","required":["title"]}"#;
        let cmd = cli.build_command_with_schema("sys", tmp.path(), Some(schema_json));
        let args = args_of(&cmd);
        let idx = args
            .iter()
            .position(|a| a == "--json-schema")
            .expect("argv should contain --json-schema");
        assert_eq!(args[idx + 1], schema_json);
    }

    #[test]
    fn build_command_inline_schema_is_passed_verbatim_not_as_path() {
        // Regression for #681: a previous implementation wrote the schema
        // to disk and passed the path. `claude -p --json-schema` silently
        // returns empty stdout for a file path, so the value on argv must
        // be JSON, not a path.
        let cli = client_with_defaults("sonnet");
        let tmp = TempDir::new().unwrap();
        let schema_json = r#"{"type":"object"}"#;
        let cmd = cli.build_command_with_schema("sys", tmp.path(), Some(schema_json));
        let args = args_of(&cmd);
        let idx = args
            .iter()
            .position(|a| a == "--json-schema")
            .expect("argv should contain --json-schema");
        let value = &args[idx + 1];
        assert!(
            value.trim_start().starts_with('{'),
            "--json-schema value must be inline JSON, got: {value:?}"
        );
        assert!(
            !std::path::Path::new(value).is_absolute(),
            "--json-schema value must not be a filesystem path, got: {value:?}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn run_with_options_passes_schema_inline_and_succeeds() {
        let _guard = shim_lock();
        // The shim ignores --json-schema and just emits a JSON envelope.
        // What matters is that run_with_options serialises the schema and
        // adds the flag without erroring out.
        let tmp = TempDir::new().unwrap();
        let shim = make_shim(&tmp, r#"{"is_error":false,"result":"ok"}"#, 0);
        let cli = client_with_shim(shim);
        let schema = serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["title"],
            "properties": {"title": {"type": "string"}}
        });
        let opts = RequestOptions::default().with_response_schema(schema);
        let out = cli
            .run_with_options("sys", "user", &opts)
            .await
            .expect("run_with_options should succeed");
        assert_eq!(out, "ok");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn run_with_options_forwards_inline_schema_to_subprocess_argv() {
        let _guard = shim_lock();
        // The shim writes its argv (NUL-separated) to a side-channel
        // file, then emits a normal JSON envelope on stdout. We then
        // assert the inline schema appears verbatim in the recorded
        // argv — proving the path-based regression (#681) is gone.
        let tmp = TempDir::new().unwrap();
        let (shim, argv_file) = make_argv_capture_shim(&tmp);
        let cli = client_with_shim(shim);
        let schema = serde_json::json!({
            "type": "object",
            "required": ["answer"],
            "properties": {"answer": {"type": "string"}}
        });
        let opts = RequestOptions::default().with_response_schema(schema.clone());
        let out = cli
            .run_with_options("sys", "user", &opts)
            .await
            .expect("run_with_options should succeed");
        assert_eq!(out, "ok");

        let captured = std::fs::read(&argv_file).expect("argv file should exist");
        let argv: Vec<String> = captured
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        let idx = argv
            .iter()
            .position(|a| a == "--json-schema")
            .expect("captured argv should contain --json-schema");
        let expected_inline = serde_json::to_string(&schema).unwrap();
        assert_eq!(argv[idx + 1], expected_inline);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn send_request_with_options_default_acts_like_send_request() {
        let _guard = shim_lock();
        // No schema in options → behaves exactly like send_request.
        let tmp = TempDir::new().unwrap();
        let shim = make_shim(&tmp, r#"{"is_error":false,"result":"hi"}"#, 0);
        let cli = client_with_shim(shim);
        let out = cli
            .send_request_with_options("sys", "user", RequestOptions::default())
            .await
            .expect("send_request_with_options should succeed");
        assert_eq!(out, "hi");
    }

    /// Initialises a tracing subscriber at debug level so the
    /// instrumented `debug!` macro inside [`send_request_with_options`]
    /// evaluates its arguments — without a subscriber, the macro elides
    /// its body and the field-formatting expressions register as
    /// uncovered. The test set is otherwise the same shape as
    /// [`send_request_with_options_default_acts_like_send_request`].
    #[tokio::test]
    #[cfg(unix)]
    async fn send_request_with_options_records_debug_trace() {
        let _guard = shim_lock();
        // Ignore the result: another test may have already installed a
        // subscriber. Either way is fine; we only need *some* subscriber
        // to make the debug! macro's expressions execute.
        let _ = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .try_init();

        let tmp = TempDir::new().unwrap();
        let shim = make_shim(&tmp, r#"{"is_error":false,"result":"hi"}"#, 0);
        let cli = client_with_shim(shim);
        let schema = serde_json::json!({"type": "object", "additionalProperties": false});
        let out = cli
            .send_request_with_options(
                "sys",
                "user",
                RequestOptions::default().with_response_schema(schema),
            )
            .await
            .expect("send_request_with_options should succeed");
        assert_eq!(out, "hi");
    }

    #[test]
    fn strip_fence_removes_yaml_wrapping() {
        let raw = "```yaml\namendments: []\n```";
        assert_eq!(strip_wrapping_code_fence(raw), "amendments: []");
    }

    #[test]
    fn strip_fence_removes_bare_wrapping() {
        let raw = "```\nsome text\n```";
        assert_eq!(strip_wrapping_code_fence(raw), "some text");
    }

    #[test]
    fn strip_fence_preserves_bare_content() {
        let raw = "amendments:\n  - hash: abc";
        assert_eq!(strip_wrapping_code_fence(raw), raw);
    }

    #[test]
    fn strip_fence_preserves_content_with_internal_fences() {
        // A PR description may legitimately embed a ```rust block; we must
        // not mangle it.
        let raw = "title: PR title\ndescription: |\n  Here is code:\n  ```rust\n  fn x() {}\n  ```\n  Done.";
        assert_eq!(strip_wrapping_code_fence(raw), raw);
    }

    #[test]
    fn strip_fence_with_wrapper_around_internal_fences_is_left_alone() {
        // If the response is wrapped AND contains internal fences, we bail
        // out — ambiguity between a legitimate wrapper and a response whose
        // first character just happens to be a fence.
        let raw = "```markdown\nouter\n```rust\nfn x() {}\n```\nmore\n```";
        assert_eq!(strip_wrapping_code_fence(raw), raw);
    }

    #[test]
    fn strip_fence_trims_outer_whitespace() {
        let raw = "\n\n```yaml\namendments: []\n```\n\n";
        assert_eq!(strip_wrapping_code_fence(raw), "amendments: []");
    }

    #[tokio::test]
    async fn spawn_missing_binary_yields_typed_error() {
        let cli = ClaudeCliAiClient::new_with_config(
            "sonnet".to_string(),
            DEFAULT_TIMEOUT,
            DEFAULT_STDOUT_CAP,
            false,
            PathBuf::from("/nonexistent/path/to/claude-binary-xyz"),
        );
        let err = cli
            .run("sys", "user")
            .await
            .expect_err("expected missing-binary error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("Subprocess binary not found"),
            "unexpected error: {chain}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn runaway_output_yields_timeout_or_cap_error() {
        let _guard = shim_lock();

        // GNU `yes` on Linux errors on any unknown flag (including our
        // leading `-p`), so we can't use it as a stand-in. Instead write
        // a tiny shell-script shim that ignores argv and floods stdout.
        let tmp = TempDir::new().unwrap();
        let shim = tmp.path().join("runaway-claude");
        write_exec_script(&shim, "#!/bin/sh\nwhile true; do printf 'y\\n'; done\n");

        let cli = ClaudeCliAiClient::new_with_config(
            "sonnet".to_string(),
            Duration::from_secs(1),
            64 * 1024,
            false,
            shim,
        );
        let err = cli
            .run("sys", "user")
            .await
            .expect_err("expected timeout or size-cap error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("timed out") || chain.contains("output exceeded"),
            "unexpected error: {chain}"
        );
    }

    #[tokio::test]
    async fn non_json_output_yields_typed_error() {
        // /bin/echo prints its args and exits 0 — produces no valid JSON.
        let cli = ClaudeCliAiClient::new_with_config(
            "sonnet".to_string(),
            DEFAULT_TIMEOUT,
            DEFAULT_STDOUT_CAP,
            false,
            PathBuf::from("/bin/echo"),
        );
        let err = cli
            .run("sys", "user")
            .await
            .expect_err("expected JSON parse error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("invalid JSON output"),
            "unexpected error: {chain}"
        );
    }

    // ── End-to-end run() tests via shell-script shims ───────────────

    #[cfg(unix)]
    use crate::test_support::shim::{shim_lock, write_exec_script};

    /// Exercises the poison-recovery branch of `shim_lock()`: panics in
    /// a helper thread while holding the guard so the mutex becomes
    /// poisoned, then verifies a subsequent acquisition still yields a
    /// usable guard. The mutex remains poisoned for the rest of the
    /// binary, which is fine because `shim_lock()` recovers via
    /// `PoisonError::into_inner()`.
    #[cfg(unix)]
    #[test]
    fn shim_lock_recovers_from_poison() {
        let _ = std::thread::spawn(|| {
            let _g = shim_lock();
            panic!("intentional: poisoning SHIM_LOCK for coverage");
        })
        .join();
        let _g = shim_lock();
    }

    /// Writes a shell-script shim that drains stdin, emits `body` on
    /// stdout, and exits with `exit_code`. Returns the shim path.
    #[cfg(unix)]
    fn make_shim(tmp: &TempDir, body: &str, exit_code: i32) -> PathBuf {
        let shim = tmp.path().join("claude-shim");
        // `cat > /dev/null` drains stdin so the parent's write completes
        // cleanly rather than hitting EPIPE. `printf %s` avoids backslash
        // interpretation. The heredoc uses a quoted terminator so JSON
        // braces and quotes pass through unchanged.
        let script = format!(
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{}'\nexit {}\n",
            body.replace('\'', "'\\''"),
            exit_code
        );
        write_exec_script(&shim, &script);
        shim
    }

    /// Writes a shell-script shim that drains stdin, captures its own
    /// argv (NUL-separated) to a side-channel file, and emits a
    /// `{"is_error":false,"result":"ok"}` envelope on stdout. Returns
    /// `(shim_path, argv_capture_path)`.
    ///
    /// The side-channel avoids JSON-escaping argv characters in shell —
    /// the parent reads the file directly and splits on NUL.
    #[cfg(unix)]
    fn make_argv_capture_shim(tmp: &TempDir) -> (PathBuf, PathBuf) {
        let shim = tmp.path().join("claude-argv-capture");
        let argv_file = tmp.path().join("captured-argv.bin");
        let script = format!(
            "#!/bin/sh\n\
             cat >/dev/null\n\
             : > '{path}'\n\
             for a in \"$@\"; do printf '%s\\0' \"$a\" >> '{path}'; done\n\
             printf '%s' '{{\"is_error\":false,\"result\":\"ok\"}}'\n\
             exit 0\n",
            path = argv_file.display()
        );
        write_exec_script(&shim, &script);
        (shim, argv_file)
    }

    #[cfg(unix)]
    fn client_with_shim(shim: PathBuf) -> ClaudeCliAiClient {
        ClaudeCliAiClient::new_with_config(
            "sonnet".to_string(),
            Duration::from_secs(10),
            DEFAULT_STDOUT_CAP,
            false,
            shim,
        )
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn success_returns_result_field() {
        let _guard = shim_lock();
        let tmp = TempDir::new().unwrap();
        let shim = make_shim(&tmp, r#"{"is_error":false,"result":"hello from shim"}"#, 0);
        let out = client_with_shim(shim).run("sys", "user").await.unwrap();
        assert_eq!(out, "hello from shim");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn success_strips_top_level_yaml_fence() {
        let _guard = shim_lock();
        let tmp = TempDir::new().unwrap();
        // `result` field is itself JSON-escaped; the wrapped content is
        // ```yaml\namendments: []\n```
        let shim = make_shim(
            &tmp,
            r#"{"is_error":false,"result":"```yaml\namendments: []\n```"}"#,
            0,
        );
        let out = client_with_shim(shim).run("sys", "user").await.unwrap();
        assert_eq!(out, "amendments: []");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn is_error_401_maps_to_auth_failure() {
        let _guard = shim_lock();
        let tmp = TempDir::new().unwrap();
        let shim = make_shim(
            &tmp,
            r#"{"is_error":true,"api_error_status":401,"result":"unauthorized"}"#,
            0,
        );
        let err = client_with_shim(shim)
            .run("sys", "user")
            .await
            .expect_err("expected auth error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("authentication failed"),
            "unexpected error: {chain}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn is_error_403_maps_to_auth_failure() {
        let _guard = shim_lock();
        let tmp = TempDir::new().unwrap();
        let shim = make_shim(
            &tmp,
            r#"{"is_error":true,"api_error_status":403,"result":"forbidden"}"#,
            0,
        );
        let err = client_with_shim(shim)
            .run("sys", "user")
            .await
            .expect_err("expected auth error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("authentication failed"),
            "unexpected error: {chain}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn is_error_404_maps_to_unknown_model() {
        let _guard = shim_lock();
        let tmp = TempDir::new().unwrap();
        let shim = make_shim(
            &tmp,
            r#"{"is_error":true,"api_error_status":404,"result":"model not found"}"#,
            0,
        );
        let err = client_with_shim(shim)
            .run("sys", "user")
            .await
            .expect_err("expected unknown-model error");
        let chain = format!("{err:#}");
        assert!(chain.contains("unknown model"), "unexpected error: {chain}");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn is_error_429_maps_to_rate_limit() {
        let _guard = shim_lock();
        let tmp = TempDir::new().unwrap();
        let shim = make_shim(
            &tmp,
            r#"{"is_error":true,"api_error_status":429,"result":"too many"}"#,
            0,
        );
        let err = client_with_shim(shim)
            .run("sys", "user")
            .await
            .expect_err("expected rate-limit error");
        let downcast = err
            .downcast_ref::<ClaudeError>()
            .expect("error should be ClaudeError");
        assert!(matches!(downcast, ClaudeError::RateLimitExceeded));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn is_error_500_maps_to_transient() {
        let _guard = shim_lock();
        let tmp = TempDir::new().unwrap();
        let shim = make_shim(
            &tmp,
            r#"{"is_error":true,"api_error_status":503,"result":"upstream unavailable"}"#,
            0,
        );
        let err = client_with_shim(shim)
            .run("sys", "user")
            .await
            .expect_err("expected transient error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("transient API error"),
            "unexpected error: {chain}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn is_error_unknown_status_maps_to_generic() {
        let _guard = shim_lock();
        let tmp = TempDir::new().unwrap();
        // No api_error_status → falls through to the generic arm.
        let shim = make_shim(
            &tmp,
            r#"{"is_error":true,"result":"something went wrong"}"#,
            0,
        );
        let err = client_with_shim(shim)
            .run("sys", "user")
            .await
            .expect_err("expected generic error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("reported error"),
            "unexpected error: {chain}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn non_zero_exit_with_clean_json_still_errors() {
        let _guard = shim_lock();
        // is_error=false but process exits 1 — surfaced as a distinct
        // error so the user sees the unexpected exit status.
        let tmp = TempDir::new().unwrap();
        let shim = make_shim(&tmp, r#"{"is_error":false,"result":"ok"}"#, 1);
        let err = client_with_shim(shim)
            .run("sys", "user")
            .await
            .expect_err("expected exit-status error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("non-zero status"),
            "unexpected error: {chain}"
        );
    }

    /// Holds a writable FD on a shim long enough for `spawn_with_etxtbsy_retry`
    /// to hit ETXTBSY, then drops it on a timer so a subsequent retry
    /// succeeds. Exercises the retry branch on platforms where the kernel
    /// enforces ETXTBSY (Linux). On platforms that don't, the first spawn
    /// succeeds and the retry loop simply isn't exercised.
    #[tokio::test]
    #[cfg(target_os = "linux")]
    async fn spawn_retries_through_etxtbsy() {
        let _guard = shim_lock();
        use std::os::unix::fs::OpenOptionsExt;

        let tmp = TempDir::new().unwrap();
        let shim = tmp.path().join("busy-shim");
        write_exec_script(
            &shim,
            "#!/bin/sh\ncat >/dev/null\nprintf '%s' '{\"is_error\":false,\"result\":\"late\"}'\nexit 0\n",
        );

        // Pin a writable FD to the inode. While this lives, `execve` on
        // the shim returns ETXTBSY.
        let blocker = std::fs::OpenOptions::new()
            .write(true)
            .mode(0o755)
            .open(&shim)
            .unwrap();

        // Drop the blocker after a short delay — enough for at least one
        // ETXTBSY retry to fire.
        let drop_after = Duration::from_millis(20);
        let release = tokio::spawn(async move {
            tokio::time::sleep(drop_after).await;
            drop(blocker);
        });

        let out = client_with_shim(shim).run("sys", "user").await.unwrap();
        release.await.unwrap();
        assert_eq!(out, "late");
    }

    // ── Process-group reaping on timeout (issue #633) ──────────────

    /// Polls `kill(pid, 0)` until the process is gone or the deadline
    /// elapses. `Ok(())` indicates the process is gone (`ESRCH`); `Err`
    /// reports the elapsed time so the assertion site can show how long
    /// the process lingered.
    #[cfg(unix)]
    async fn wait_for_pid_gone(pid: i32, deadline: Duration) -> Result<(), Duration> {
        let nix_pid = nix::unistd::Pid::from_raw(pid);
        let start = std::time::Instant::now();
        loop {
            if nix::sys::signal::kill(nix_pid, None) == Err(nix::errno::Errno::ESRCH) {
                return Ok(());
            }
            if start.elapsed() >= deadline {
                return Err(start.elapsed());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Acceptance test for issue #633: when a `claude -p` invocation
    /// times out, the entire subprocess group is reaped — including
    /// helper processes the child forked into the background. Without
    /// `process_group(0)` + `killpg`, the background sleeper would
    /// be reparented to PID 1 and survive its parent.
    #[tokio::test]
    #[cfg(unix)]
    async fn timeout_reaps_full_process_group() {
        let _guard = shim_lock();
        let tmp = TempDir::new().unwrap();
        let pid_file = tmp.path().join("sleeper.pid");
        let shim = tmp.path().join("group-shim");

        // Shim starts a long-lived background sleeper, records its PID
        // for the parent test, then blocks long enough that the
        // configured timeout fires. With process_group(0) + killpg the
        // background sleeper dies alongside the shim; without them it
        // survives until its own `sleep` elapses.
        let script = format!(
            "#!/bin/sh\nsleep 30 &\nprintf '%s' \"$!\" > '{}'\nsleep 30\n",
            pid_file.display()
        );
        write_exec_script(&shim, &script);

        let cli = ClaudeCliAiClient::new_with_config(
            "sonnet".to_string(),
            Duration::from_millis(500),
            DEFAULT_STDOUT_CAP,
            false,
            shim,
        );

        let err = cli
            .run("sys", "user")
            .await
            .expect_err("expected timeout error");
        let chain = format!("{err:#}");
        assert!(chain.contains("timed out"), "expected timeout: {chain}");

        let pid_str = std::fs::read_to_string(&pid_file)
            .expect("shim should have recorded sleeper PID before sleeping");
        let sleeper_pid: i32 = pid_str.trim().parse().expect("valid pid");

        wait_for_pid_gone(sleeper_pid, Duration::from_secs(3))
            .await
            .unwrap_or_else(|elapsed| {
                panic!(
                    "background sleeper {sleeper_pid} still alive {elapsed:?} after \
                     parent timeout — process-group reap regressed (issue #633)"
                )
            });
    }

    /// Companion to `timeout_reaps_full_process_group`: same shim
    /// shape, but the inner I/O fails (write task errors via a closed
    /// pipe by exiting fast) is not what we want here — instead we
    /// assert the helper still gets reaped on the *write-error* branch
    /// of `run()` by invoking `kill_and_reap` directly against a
    /// fresh subprocess. This exercises the helper without depending
    /// on the timeout path.
    #[tokio::test]
    #[cfg(unix)]
    async fn kill_and_reap_kills_full_process_group() {
        let _guard = shim_lock();
        let tmp = TempDir::new().unwrap();
        let pid_file = tmp.path().join("sleeper-direct.pid");
        let shim = tmp.path().join("direct-shim");
        let script = format!(
            "#!/bin/sh\nsleep 30 &\nprintf '%s' \"$!\" > '{}'\nsleep 30\n",
            pid_file.display()
        );
        write_exec_script(&shim, &script);

        let mut cmd = tokio::process::Command::new(&shim);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        // Bare cmd.spawn() races with sibling test threads on Linux —
        // the kernel returns ETXTBSY while another thread holds a
        // writable FD on the freshly-written shim. Use the same retry
        // helper production code uses.
        let mut child = spawn_with_etxtbsy_retry(&mut cmd)
            .await
            .expect("spawn shim");

        // Wait until the shim has written its sleeper PID — the file
        // appears after a few ms in practice, but poll defensively.
        let pid_path = pid_file.clone();
        let pid_str = tokio::time::timeout(Duration::from_secs(2), async move {
            loop {
                if let Ok(s) = std::fs::read_to_string(&pid_path) {
                    if !s.trim().is_empty() {
                        return s;
                    }
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("shim wrote PID");
        let sleeper_pid: i32 = pid_str.trim().parse().expect("valid pid");

        kill_and_reap(&mut child).await;

        wait_for_pid_gone(sleeper_pid, Duration::from_secs(3))
            .await
            .unwrap_or_else(|elapsed| {
                panic!(
                    "background sleeper {sleeper_pid} still alive {elapsed:?} after \
                     kill_and_reap"
                )
            });
    }

    /// `kill_and_reap` must tolerate a child that has already exited.
    /// In that case `child.id()` may still return Some until reaped —
    /// the killpg call returns ESRCH, which the helper swallows.
    #[tokio::test]
    #[cfg(unix)]
    async fn kill_and_reap_tolerates_already_exited_child() {
        let _guard = shim_lock();
        let tmp = TempDir::new().unwrap();
        let shim = tmp.path().join("fast-exit");
        write_exec_script(&shim, "#!/bin/sh\nexit 0\n");

        let mut cmd = tokio::process::Command::new(&shim);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = spawn_with_etxtbsy_retry(&mut cmd)
            .await
            .expect("spawn shim");

        // Give the child a moment to exit before we try to reap.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Should not panic / hang, even though the group is already
        // empty.
        kill_and_reap(&mut child).await;
    }

    /// `kill_and_reap` must be safe to call even after the child has
    /// been explicitly waited on, which makes `child.id()` return None.
    /// Covers the `else` branch of the helper.
    #[tokio::test]
    #[cfg(unix)]
    async fn kill_and_reap_handles_already_waited_child() {
        let _guard = shim_lock();
        let tmp = TempDir::new().unwrap();
        let shim = tmp.path().join("waited-shim");
        write_exec_script(&shim, "#!/bin/sh\nexit 0\n");

        let mut cmd = tokio::process::Command::new(&shim);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = spawn_with_etxtbsy_retry(&mut cmd)
            .await
            .expect("spawn shim");

        // Reap explicitly so child.id() reports None on the next call.
        let _ = child.wait().await.expect("wait succeeds");
        assert!(child.id().is_none(), "post-wait id should be None");

        // Should be a no-op — exercises the `else` branch where the
        // helper has no PID to signal.
        kill_and_reap(&mut child).await;
    }

    /// `wait_for_pid_gone` must report `Err` (with elapsed time) when
    /// its deadline expires before the process dies. Covers the
    /// deadline branch of the polling loop.
    #[tokio::test]
    #[cfg(unix)]
    async fn wait_for_pid_gone_returns_err_on_deadline() {
        let _guard = shim_lock();
        let tmp = TempDir::new().unwrap();
        let shim = tmp.path().join("alive-long");
        write_exec_script(&shim, "#!/bin/sh\nsleep 30\n");

        let mut cmd = tokio::process::Command::new(&shim);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = spawn_with_etxtbsy_retry(&mut cmd)
            .await
            .expect("spawn shim");
        let pid = child.id().expect("has pid") as i32;

        let deadline = Duration::from_millis(80);
        let res = wait_for_pid_gone(pid, deadline).await;
        let elapsed = res.expect_err("should hit deadline before sleeper exits");
        assert!(
            elapsed >= deadline,
            "elapsed {elapsed:?} should be at least the deadline {deadline:?}"
        );

        // Cleanup so we don't leak a 30s sleeper.
        kill_and_reap(&mut child).await;
    }

    /// `kill_and_reap` must reap a child that's actively running, not
    /// just one that already exited. Confirms the `wait()` half of the
    /// helper (otherwise the child would persist as a zombie).
    #[tokio::test]
    #[cfg(unix)]
    async fn kill_and_reap_reaps_running_child() {
        let _guard = shim_lock();
        let tmp = TempDir::new().unwrap();
        let shim = tmp.path().join("running-shim");
        write_exec_script(&shim, "#!/bin/sh\nsleep 30\n");

        let mut cmd = tokio::process::Command::new(&shim);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        // Use the same ETXTBSY-retry helper that production code uses;
        // a bare cmd.spawn() races with concurrent test threads on Linux
        // because the kernel returns ETXTBSY while another thread holds
        // a writable FD on the shim binary.
        let mut child = spawn_with_etxtbsy_retry(&mut cmd)
            .await
            .expect("spawn shim");
        let pid = child.id().expect("child has pid before reap");

        kill_and_reap(&mut child).await;

        wait_for_pid_gone(pid as i32, Duration::from_secs(3))
            .await
            .unwrap_or_else(|elapsed| {
                panic!("direct child {pid} still alive {elapsed:?} after kill_and_reap")
            });
    }
}
