//! Claude Code CLI subprocess AI client.
//!
//! Shells out to `claude -p` in a locked-down sandbox so users with an
//! authenticated Claude Code session can reuse it without provisioning a
//! separate API key.
//!
//! Sandboxing:
//! - `--tools ""` disables built-in tools.
//! - `--strict-mcp-config` (with no accompanying `--mcp-config`) blocks MCP
//!   server pickup from user settings.
//! - `--setting-sources ""` skips user / project / local settings discovery.
//! - `--disable-slash-commands` blocks skills.
//! - `--no-session-persistence` avoids writing session state to disk.
//! - Subprocess runs with cwd set to a fresh temp directory.
//! - Environment inherits from the parent, then removes `CLAUDE_PROJECT_DIR`,
//!   any `CLAUDE_CODE_*`, and any `CLAUDE_PROJECT_*` vars that could re-scope
//!   the nested session.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tracing::{debug, info, warn};

use super::{AiClient, AiClientMetadata};
use crate::claude::error::ClaudeError;

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
}

/// Claude Code CLI subprocess AI client.
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
    /// Path to the `claude` binary (defaults to `claude` on PATH).
    binary_path: PathBuf,
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
            false,
            Self::binary_from_env(),
        )
    }

    /// Creates a client with explicit configuration. Primarily for tests.
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
            binary_path,
        }
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

    /// Builds the subprocess [`Command`] without spawning.
    ///
    /// Broken out so tests can inspect the argv / env / cwd via
    /// `Command::get_args`, `get_envs`, `get_current_dir` without running
    /// a subprocess.
    pub(crate) fn build_command(&self, system_prompt: &str, cwd: &Path) -> Command {
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
            .arg("--strict-mcp-config")
            .arg("--setting-sources")
            .arg("")
            .arg("--system-prompt")
            .arg(system_prompt);

        if !self.allow_tools {
            cmd.arg("--tools").arg("");
        }

        cmd.current_dir(cwd);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Scrub risky env vars rather than clearing wholesale. Clearing the
        // env breaks the Node runtime inside `claude` (needs HOME, PATH,
        // possibly DYLD_* / homebrew PATH entries on macOS).
        for (k, _) in std::env::vars() {
            if SCRUBBED_ENV_EXACT.contains(&k.as_str())
                || SCRUBBED_ENV_PREFIXES.iter().any(|p| k.starts_with(p))
            {
                cmd.env_remove(&k);
            }
        }

        cmd
    }

    async fn run(&self, system_prompt: &str, user_prompt: &str) -> Result<String> {
        let combined_system = format!("{system_prompt}{TOOL_SUPPRESSION_SUFFIX}");

        let temp_dir = tempfile::TempDir::new()
            .context("Failed to create temp directory for claude subprocess")?;

        let mut cmd = self.build_command(&combined_system, temp_dir.path());

        info!(
            binary = %self.binary_path.display(),
            model = %self.model,
            timeout_secs = self.timeout.as_secs(),
            "Spawning claude -p subprocess"
        );

        let mut child = cmd.spawn().map_err(|e| {
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
                let _ = child.kill().await;
                let _ = child.wait().await;
                return Err(e);
            }
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
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
        use std::os::unix::fs::PermissionsExt;

        // GNU `yes` on Linux errors on any unknown flag (including our
        // leading `-p`), so we can't use it as a stand-in. Instead write
        // a tiny shell-script shim that ignores argv and floods stdout.
        let tmp = TempDir::new().unwrap();
        let shim = tmp.path().join("runaway-claude");
        std::fs::write(&shim, "#!/bin/sh\nwhile true; do printf 'y\\n'; done\n").unwrap();
        let mut perms = std::fs::metadata(&shim).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&shim, perms).unwrap();

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

    /// Writes a shell-script shim that drains stdin, emits `body` on
    /// stdout, and exits with `exit_code`. Returns the shim path.
    #[cfg(unix)]
    fn make_shim(tmp: &TempDir, body: &str, exit_code: i32) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
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
        std::fs::write(&shim, script).unwrap();
        let mut perms = std::fs::metadata(&shim).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&shim, perms).unwrap();
        shim
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
        let tmp = TempDir::new().unwrap();
        let shim = make_shim(&tmp, r#"{"is_error":false,"result":"hello from shim"}"#, 0);
        let out = client_with_shim(shim).run("sys", "user").await.unwrap();
        assert_eq!(out, "hello from shim");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn success_strips_top_level_yaml_fence() {
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
}
