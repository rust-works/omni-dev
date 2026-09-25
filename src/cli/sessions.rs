//! `omni-dev sessions` — track the Claude Code, Codex and pi.dev sessions running
//! across every terminal and VS Code window, via the daemon's `sessions` service.
//!
//! The subcommands split by role:
//! - `list` is a **read** client (like `omni-dev worktrees list`): it asks the
//!   daemon's `sessions` service for the live set and renders it.
//! - `hook` is the **feed sink**: Claude Code (or Codex, with `--agent codex`)
//!   runs it per hook event; it reads
//!   the hook JSON on stdin, maps it to an `observe`/`end` op, and fire-and-forgets
//!   it to the daemon socket. It must **never** block or fail a Claude turn — a
//!   missing daemon, a bad payload, or any other error is swallowed and it always
//!   exits 0.
//! - `install-hooks` / `uninstall-hooks` idempotently merge (or remove) the hook
//!   block in `~/.claude/settings.json`, preserving any hooks already there. When
//!   pi.dev is installed they also write (or remove) the generated pi extension
//!   in `~/.pi/agent/extensions/`, which reports straight to the socket (#1901),
//!   and when Codex is installed the same hook block, tagged `--agent codex`, in
//!   `$CODEX_HOME/hooks.json` — position-stably, since Codex trusts a hook by
//!   its index (#1907).
//! - `install-wrapper` / `uninstall-wrapper` are the same idea for Feed 4: they
//!   write the shim that VS Code's Claude extension launches
//!   [`omni-dev claude-wrap`](crate::cli::claude_wrap) through, and point the
//!   extension's `claudeCode.claudeProcessWrapper` setting at it.
//!
//! The register/heartbeat feed from the companion VS Code extension talks to the
//! socket directly (like the worktrees companion), not through this CLI.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand, ValueEnum};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::cli::format::{sanitize_for_terminal, TableOrJson};
use crate::daemon::client::DaemonClient;
use crate::daemon::paths;
use crate::daemon::protocol::{DaemonEnvelope, DaemonReply};
use crate::daemon::server;
use crate::sessions::{Agent, NotificationKind, ObserveRequest, SessionEvent};

/// The `sessions` service routing key on the daemon control socket.
const SERVICE: &str = "sessions";

/// How long the fire-and-forget `hook` sink waits for the daemon before giving
/// up — short, so a slow or wedged daemon never stalls a Claude turn.
const HOOK_TIMEOUT: Duration = Duration::from_secs(2);

/// Sessions: see the Claude Code sessions running across every terminal and
/// VS Code window, kept live by the daemon.
#[derive(Parser)]
pub struct SessionsCommand {
    /// The sessions subcommand to execute.
    #[command(subcommand)]
    pub command: SessionsSubcommands,
}

/// Sessions subcommands.
#[derive(Subcommand)]
pub enum SessionsSubcommands {
    /// List the Claude Code, Codex and pi.dev sessions currently running across
    /// all windows.
    List(ListCommand),
    /// Claude Code / Codex hook sink: read a hook event on stdin and report it to
    /// the daemon (run by the agent, not by hand).
    Hook(HookCommand),
    /// Install the Claude Code hooks that feed the sessions tracker into
    /// `~/.claude/settings.json`, plus the pi.dev extension when pi is installed
    /// and the Codex hooks when Codex is (idempotent).
    InstallHooks(InstallHooksCommand),
    /// Remove the sessions-tracker hooks from `~/.claude/settings.json`, the
    /// pi.dev extension, and the Codex hooks.
    UninstallHooks(UninstallHooksCommand),
    /// Install the `claude-wrap` shim and point VS Code's Claude extension at it
    /// (idempotent).
    InstallWrapper(InstallWrapperCommand),
    /// Remove the `claude-wrap` shim and VS Code's wrapper setting.
    UninstallWrapper(UninstallWrapperCommand),
    /// Report a window's Claude tab/terminal counts (companion feed op).
    Window(WindowCommand),
    /// Remove a window's embedding report (companion feed op).
    WindowUnregister(WindowUnregisterCommand),
}

impl SessionsCommand {
    /// Executes the sessions command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            SessionsSubcommands::List(cmd) => cmd.execute().await,
            SessionsSubcommands::Hook(cmd) => cmd.execute().await,
            SessionsSubcommands::InstallHooks(cmd) => cmd.execute(),
            SessionsSubcommands::UninstallHooks(cmd) => cmd.execute(),
            SessionsSubcommands::InstallWrapper(cmd) => cmd.execute(),
            SessionsSubcommands::UninstallWrapper(cmd) => cmd.execute(),
            SessionsSubcommands::Window(cmd) => cmd.execute().await,
            SessionsSubcommands::WindowUnregister(cmd) => cmd.execute().await,
        }
    }
}

// --- list --------------------------------------------------------------------

/// Lists the live cross-window set of running Claude sessions.
#[derive(Parser)]
pub struct ListCommand {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = TableOrJson::Table)]
    pub output: TableOrJson,
}

impl ListCommand {
    /// Executes the list command.
    pub async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        let result = call(&socket, "list", Value::Null).await?;
        match self.output {
            TableOrJson::Json => println!("{}", serde_json::to_string_pretty(&result)?),
            TableOrJson::Table => println!("{}", render_sessions(&result)),
        }
        Ok(())
    }
}

// --- window feed -------------------------------------------------------------

/// Reports a window's Claude embedding counts (the companion `window` feed op).
///
/// Exposed as a typed command so scripted/headless reporters and integration
/// tests can drive the sessions registry the way the VS Code companion does.
/// Mirrors `WindowReport`.
#[derive(Parser)]
pub struct WindowCommand {
    /// Stable per-window identity (the companion generates a per-activate UUID).
    #[arg(long, value_name = "KEY")]
    pub key: String,
    /// A workspace-folder path (repeatable) — used to join sessions by `cwd`.
    #[arg(long = "folder", value_name = "PATH")]
    pub folders: Vec<PathBuf>,
    /// How many Claude editor tabs the window has.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub tabs: usize,
    /// How many Claude Code integrated terminals the window has.
    #[arg(long, value_name = "N", default_value_t = 0)]
    pub terminals: usize,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl WindowCommand {
    /// Executes the window command.
    pub async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        let payload = json!({
            "key": self.key,
            "folders": self.folders,
            "tabs": self.tabs,
            "terminals": self.terminals,
        });
        call(&socket, "window", payload).await?;
        println!("Reported window {}", self.key);
        Ok(())
    }
}

/// Removes a window's embedding report — the companion `window-unregister` feed
/// op made typed. Prints whether an entry was actually removed.
#[derive(Parser)]
pub struct WindowUnregisterCommand {
    /// The window key to unregister.
    #[arg(long, value_name = "KEY")]
    pub key: String,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl WindowUnregisterCommand {
    /// Executes the window-unregister command.
    pub async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        let reply = call(&socket, "window-unregister", json!({ "key": self.key })).await?;
        let removed = reply
            .get("removed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        println!("removed: {removed}");
        Ok(())
    }
}

// --- hook --------------------------------------------------------------------

/// The Claude Code / Codex hook sink: reads one hook event's JSON on stdin and
/// reports it to the daemon. Fire-and-forget and infallible-by-design.
#[derive(Parser)]
pub struct HookCommand {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Which agent runs this hook. The payloads share field names, so the agent
    /// cannot be inferred from them; it selects the event mapping and tags the
    /// session.
    #[arg(long, value_enum, default_value_t = HookAgent::Claude)]
    pub agent: HookAgent,
}

/// The agents whose hooks run [`HookCommand`]. pi.dev reports through its own
/// extension, so it has no hook variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HookAgent {
    /// Claude Code (`~/.claude/settings.json`).
    Claude,
    /// OpenAI Codex (`$CODEX_HOME/hooks.json`).
    Codex,
}

impl HookAgent {
    /// The registry tag sessions reported by this agent's hooks carry.
    fn agent(self) -> Agent {
        match self {
            Self::Claude => Agent::Claude,
            Self::Codex => Agent::Codex,
        }
    }
}

impl HookCommand {
    /// Executes the hook sink. Always returns `Ok(())` (exit 0): a hook must
    /// never block or fail a Claude turn, so every error — no daemon, bad JSON,
    /// an unknown event — is swallowed after a best-effort report.
    pub async fn execute(self) -> Result<()> {
        // Read before stdin, while the agent that spawned this hook is surely
        // still alive to be our parent.
        let pid = agent_pid(std::os::unix::process::parent_id());
        let mut input = String::new();
        if std::io::stdin().read_to_string(&mut input).is_err() {
            return Ok(());
        }
        self.report(&input, pid).await;
        Ok(())
    }

    /// Parses the hook JSON, maps it to an op, and best-effort sends it. Split
    /// out so tests can exercise the send path against a fake socket.
    async fn report(&self, input: &str, pid: Option<u32>) {
        let Ok(hook) = serde_json::from_str::<HookPayload>(input) else {
            return;
        };
        let Some((op, payload)) = hook.to_op(self.agent, pid) else {
            return;
        };
        let Ok(socket) = server::resolve_socket(self.socket.clone()) else {
            return;
        };
        // Bounded, and every failure ignored: the daemon may be down, and that
        // must be a silent no-op.
        let env = DaemonEnvelope::service(SERVICE, op, payload);
        let _ = tokio::time::timeout(HOOK_TIMEOUT, DaemonClient::new(&socket).request(env)).await;
    }
}

/// The pid of the agent process that ran this hook, from the sink's parent pid.
///
/// The agent runs the installed hook command directly (a simple command, so no
/// per-hook shell sits in between), making the parent the `claude` / `codex`
/// process itself. The pid tells a resumed session's new process from the old
/// one it replaced (#1948). Pid 1 means the hook was orphaned — its agent already
/// exited and init/launchd adopted it — so it identifies nothing.
fn agent_pid(parent: u32) -> Option<u32> {
    (parent > 1).then_some(parent)
}

/// The subset of a Claude Code hook payload the sink reads. Every field is
/// optional and defaulted, so an unexpected or future payload shape never fails
/// to parse (the sink then simply produces no op). See the hooks docs.
#[derive(Debug, Clone, Default, Deserialize)]
struct HookPayload {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    transcript_path: Option<PathBuf>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    hook_event_name: Option<String>,
    /// Present on `Notification` events — the message classified into a
    /// [`NotificationKind`] when `notification_type` is absent or unrecognised.
    #[serde(default)]
    message: Option<String>,
    /// Present on `Notification` events — Claude Code's own classification
    /// (`permission_prompt`, `idle_prompt`, …), preferred over `message`.
    #[serde(default)]
    notification_type: Option<String>,
    /// Present on `SessionEnd` — why the session ended.
    #[serde(default)]
    reason: Option<String>,
    /// Present on `SessionStart` — why it fired (`startup`, `resume`, `clear`,
    /// `compact`). A `compact` start is not a new session.
    #[serde(default)]
    source: Option<String>,
    /// Present on tool events. Codex's clarifying-question UI is the
    /// `request_user_input` tool, so its `PreToolUse` is a wait for input.
    #[serde(default)]
    tool_name: Option<String>,
    /// Best-effort model id, when a payload carries one.
    #[serde(default)]
    model: Option<String>,
}

impl HookPayload {
    /// Maps this hook payload to a `(op, payload)` for the daemon, or `None` when
    /// it carries no `session_id` or names an event the tracker ignores. `pid`
    /// (the [`agent_pid`]) rides along on both ops when known.
    fn to_op(&self, agent: HookAgent, pid: Option<u32>) -> Option<(&'static str, Value)> {
        let session_id = self.session_id.clone().filter(|s| !s.trim().is_empty())?;
        let event_name = self.hook_event_name.as_deref()?;
        if event_name == "SessionEnd" {
            let mut payload = json!({ "session_id": session_id });
            if let Some(reason) = self.reason.as_ref().or(self.message.as_ref()) {
                payload["reason"] = Value::String(reason.clone());
            }
            if let Some(pid) = pid {
                payload["pid"] = Value::from(pid);
            }
            return Some(("end", payload));
        }
        let event = match agent {
            HookAgent::Claude => session_event_for(
                event_name,
                self.source.as_deref(),
                self.notification_type.as_deref(),
                self.message.as_deref(),
            )?,
            HookAgent::Codex => codex_session_event_for(
                event_name,
                self.source.as_deref(),
                self.tool_name.as_deref(),
            )?,
        };
        let request = ObserveRequest {
            agent: agent.agent(),
            session_id,
            cwd: self.cwd.clone(),
            transcript_path: self.transcript_path.clone(),
            event,
            repo: None,
            model: self.model.clone(),
            pid,
        };
        Some(("observe", serde_json::to_value(request).ok()?))
    }
}

/// Maps a Claude Code hook event name to the [`SessionEvent`] it implies, or
/// `None` for an event the tracker does not act on. `SessionEnd` is handled
/// separately (it maps to the `end` op, not `observe`).
///
/// Everything lands on the **existing** events, so the engine's state machine
/// is unchanged (the Codex precedent, ADR-0087):
///
/// - `PermissionRequest` is the dedicated "a prompt is about to be shown"
///   event, so it sets the permission wait without matching `Notification`
///   text. The sink must never answer it — a `PermissionRequest` hook that
///   prints a decision approves or denies on the user's behalf — and it cannot:
///   [`HookCommand`] writes nothing to stdout;
/// - `Elicitation` (an MCP server asking the user a question) is a wait for
///   input, released by its `ElicitationResult`;
/// - `PostToolUseFailure` and `PermissionDenied` (auto mode refused a call and
///   the turn goes on) are post-tool signals: `working`;
/// - `StopFailure` ends a turn on an API error, which fires no `Stop`;
/// - `SubagentStart` means a subagent is being spawned inside a turn: `working`;
/// - `SubagentStop`, `PreCompact` and `PostCompact` can all fire while the
///   session is idle — a background subagent finishing after the turn's `Stop`,
///   a manual `/compact` — and nothing after them would release a `working`, so
///   they are the state-preserving
///   [`TranscriptDiscovered`](SessionEvent::TranscriptDiscovered) sighting,
///   which refreshes liveness alone;
/// - `SessionStart` with `source: "compact"` follows a compaction, which can run
///   mid-turn, and is not a new session. Since growth no longer moves a
///   `starting` session (#1946), mapping it to `SessionStart` would hold a
///   working session at `starting` until its next tool hook, so it is the same
///   state-preserving sighting. Every other source (`startup`, `resume`,
///   `clear`) is a real start.
///
/// `PostToolBatch` is deliberately not installed: every tool in a batch has
/// already reported its own `PostToolUse` / `PostToolUseFailure`, so it would be
/// a process spawn per batch carrying no new state.
///
/// No event fires when the user answers a permission prompt, either way; the
/// wait is released by whatever hook comes next.
fn session_event_for(
    event_name: &str,
    source: Option<&str>,
    notification_type: Option<&str>,
    message: Option<&str>,
) -> Option<SessionEvent> {
    Some(match event_name {
        "SessionStart" if source == Some("compact") => SessionEvent::TranscriptDiscovered,
        "SessionStart" => SessionEvent::SessionStart,
        "UserPromptSubmit" => SessionEvent::UserPromptSubmit,
        "PreToolUse" => SessionEvent::PreToolUse,
        "PostToolUse" | "PostToolUseFailure" | "PermissionDenied" | "ElicitationResult"
        | "SubagentStart" => SessionEvent::PostToolUse,
        "SubagentStop" | "PreCompact" | "PostCompact" => SessionEvent::TranscriptDiscovered,
        "Stop" | "StopFailure" => SessionEvent::Stop,
        "PermissionRequest" => SessionEvent::Notification(NotificationKind::PermissionPrompt),
        "Elicitation" => SessionEvent::Notification(NotificationKind::AgentNeedsInput),
        "Notification" => {
            SessionEvent::Notification(classify_notification(notification_type, message))
        }
        _ => return None,
    })
}

/// Maps a Codex hook event name to the [`SessionEvent`] it implies, or `None` for
/// an event the tracker does not act on (ADR-0087). Codex's payload uses Claude
/// Code's field names, but adds events Claude lacks, and all of them land on the
/// **existing** events so the engine's state machine is unchanged:
///
/// - `PermissionRequest` is Codex's dedicated "waiting on an approval" event, so
///   it is the reliable permission signal (its resolution is inferred from the
///   next `PostToolUse`, or from the turn ending);
/// - `PreToolUse` of the `request_user_input` tool is the clarifying-question UI,
///   a wait for input until its `PostToolUse`;
/// - `Interrupt` (Esc / Ctrl-C mid-turn, or a declined approval in the TUI) ends
///   the turn without a `Stop`, so it is one;
/// - the compaction and subagent events carry the parent session's id and mean
///   only that the session is busy — a working-state heartbeat;
/// - a `SessionStart` with `source: "compact"` is not a new session, so, as for
///   Claude, it preserves the state rather than resetting it to `starting`
///   (#1946).
///
/// `SessionEnd` is handled by the caller, as for Claude.
fn codex_session_event_for(
    event_name: &str,
    source: Option<&str>,
    tool_name: Option<&str>,
) -> Option<SessionEvent> {
    Some(match event_name {
        "SessionStart" if source == Some("compact") => SessionEvent::TranscriptDiscovered,
        "SessionStart" => SessionEvent::SessionStart,
        "UserPromptSubmit" => SessionEvent::UserPromptSubmit,
        "PreToolUse" if tool_name == Some("request_user_input") => {
            SessionEvent::Notification(NotificationKind::AgentNeedsInput)
        }
        "PreToolUse" => SessionEvent::PreToolUse,
        "PostToolUse" | "SubagentStart" | "SubagentStop" | "PreCompact" | "PostCompact" => {
            SessionEvent::PostToolUse
        }
        "PermissionRequest" => SessionEvent::Notification(NotificationKind::PermissionPrompt),
        "Stop" | "Interrupt" => SessionEvent::Stop,
        _ => return None,
    })
}

/// Classifies a `Notification` into a [`NotificationKind`]. A version that sends
/// Claude Code's own `notification_type` is classified by it alone — a type we
/// do not map (`auth_success`, …) is [`NotificationKind::Other`] rather than a
/// guess at its message, which could mention "allow" or "permission" without
/// asking for anything. Only an older version that sends no type falls back to
/// best-effort substring matching on the version-unstable message text, where an
/// unrecognised message is likewise [`NotificationKind::Other`] (which carries no
/// state signal and leaves the session's state unchanged).
fn classify_notification(
    notification_type: Option<&str>,
    message: Option<&str>,
) -> NotificationKind {
    match notification_type {
        Some("permission_prompt") => return NotificationKind::PermissionPrompt,
        Some("idle_prompt") => return NotificationKind::IdlePrompt,
        Some("elicitation_dialog") => return NotificationKind::AgentNeedsInput,
        Some(_) => return NotificationKind::Other,
        None => {}
    }
    let Some(message) = message else {
        return NotificationKind::Other;
    };
    let lower = message.to_lowercase();
    if lower.contains("permission") || lower.contains("approve") || lower.contains("allow") {
        NotificationKind::PermissionPrompt
    } else if lower.contains("waiting for your input")
        || lower.contains("idle")
        || lower.contains("needs your input")
    {
        NotificationKind::IdlePrompt
    } else {
        NotificationKind::Other
    }
}

// --- install-hooks / uninstall-hooks ----------------------------------------

/// Installs the sessions-tracker hooks into `~/.claude/settings.json`, plus the
/// pi.dev extension when pi is installed and the Codex hooks when Codex is.
#[derive(Parser)]
pub struct InstallHooksCommand {
    /// Path to the Claude settings file. Defaults to `~/.claude/settings.json`
    /// (respecting `$CLAUDE_CONFIG_DIR`).
    #[arg(long, value_name = "PATH")]
    pub settings: Option<PathBuf>,

    /// pi.dev's agent directory. Defaults to `$PI_CODING_AGENT_DIR`, else
    /// `~/.pi/agent`. Passing it installs the pi extension even when pi is not
    /// detected.
    #[arg(long, value_name = "PATH")]
    pub pi_agent_dir: Option<PathBuf>,

    /// Codex's home directory, holding `hooks.json`. Defaults to `$CODEX_HOME`,
    /// else `~/.codex`. Passing it installs the Codex hooks even when Codex is
    /// not detected.
    #[arg(long, value_name = "PATH")]
    pub codex_home: Option<PathBuf>,
}

impl InstallHooksCommand {
    /// Executes the install: merges the hook block idempotently, preserving any
    /// hooks already present, then writes the pi extension if pi is present and
    /// the Codex hooks if Codex is.
    pub fn execute(self) -> Result<()> {
        let path = settings_path(self.settings)?;
        let mut settings = read_settings(&path)?;
        let command = hook_command();
        let added = merge_hooks(&mut settings, &command, HOOK_EVENTS);
        write_settings(&path, &settings)?;
        if added == 0 {
            println!(
                "sessions hooks already installed in {} (no change)",
                path.display()
            );
        } else {
            println!(
                "installed {added} sessions hook event(s) into {}\ncommand: {command}",
                path.display()
            );
        }
        install_pi_extension(self.pi_agent_dir)?;
        install_codex_hooks(self.codex_home)
    }
}

/// Removes the sessions-tracker hooks from `~/.claude/settings.json`, the pi.dev
/// extension, and the Codex hooks.
#[derive(Parser)]
pub struct UninstallHooksCommand {
    /// Path to the Claude settings file. Defaults to `~/.claude/settings.json`
    /// (respecting `$CLAUDE_CONFIG_DIR`).
    #[arg(long, value_name = "PATH")]
    pub settings: Option<PathBuf>,

    /// pi.dev's agent directory. Defaults to `$PI_CODING_AGENT_DIR`, else
    /// `~/.pi/agent`.
    #[arg(long, value_name = "PATH")]
    pub pi_agent_dir: Option<PathBuf>,

    /// Codex's home directory, holding `hooks.json`. Defaults to `$CODEX_HOME`,
    /// else `~/.codex`.
    #[arg(long, value_name = "PATH")]
    pub codex_home: Option<PathBuf>,
}

impl UninstallHooksCommand {
    /// Executes the uninstall: removes any hook entries whose command is ours,
    /// leaving every other hook untouched, then removes our pi extension and
    /// Codex hooks.
    pub fn execute(self) -> Result<()> {
        let path = settings_path(self.settings)?;
        if path.exists() {
            let mut settings = read_settings(&path)?;
            let removed = remove_hooks(&mut settings, &hook_command());
            write_settings(&path, &settings)?;
            println!(
                "removed {removed} sessions hook entry(ies) from {}",
                path.display()
            );
        } else {
            println!("no settings file at {} (nothing to remove)", path.display());
        }
        uninstall_pi_extension(self.pi_agent_dir)?;
        uninstall_codex_hooks(self.codex_home)
    }
}

// --- pi.dev extension ---------------------------------------------------------

/// Filename of the generated extension inside pi's global extensions directory.
const PI_EXTENSION_NAME: &str = "omni-dev-sessions.ts";

/// The generated extension's source, with [`PI_SOCKET_PLACEHOLDER`] still in it.
const PI_EXTENSION_TEMPLATE: &str = include_str!("../templates/pi-sessions-extension.ts");

/// Stands in for the socket path (a JSON string literal) in the template.
const PI_SOCKET_PLACEHOLDER: &str = "__OMNI_DEV_SOCKET__";

/// The template's first line. A file that does not start with it is not ours, so
/// install refuses to overwrite it and uninstall leaves it alone.
fn pi_extension_marker() -> &'static str {
    PI_EXTENSION_TEMPLATE.lines().next().unwrap_or_default()
}

/// The extension source with the daemon socket baked in, as `install-hooks`
/// bakes in the absolute exe path: pi's Node process has no other way to find
/// the socket, and resolving it there would duplicate `dirs::data_dir()`.
fn render_pi_extension(socket: &Path) -> String {
    let literal =
        serde_json::to_string(&socket.display().to_string()).unwrap_or_else(|_| "\"\"".to_string());
    PI_EXTENSION_TEMPLATE.replace(PI_SOCKET_PLACEHOLDER, &literal)
}

/// pi's default agent directory: `$PI_CODING_AGENT_DIR`, else `~/.pi/agent`.
fn default_pi_agent_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("PI_CODING_AGENT_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = dirs::home_dir().context("could not resolve the home directory")?;
    Ok(home.join(".pi").join("agent"))
}

/// Whether pi.dev is installed: its agent directory exists (pi has run), or a
/// `pi` executable is on `path` (installed but never run).
fn pi_is_present(agent_dir: &Path, path: Option<&std::ffi::OsStr>) -> bool {
    agent_dir.is_dir()
        || path.is_some_and(|path| {
            std::env::split_paths(path).any(|dir| is_executable(&dir.join("pi")))
        })
}

/// Whether `path` is a file with an execute bit set.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// What [`write_pi_extension`] did.
#[derive(Debug, PartialEq, Eq)]
enum PiInstall {
    /// Wrote a new or updated extension.
    Written,
    /// The file already had exactly this content.
    Unchanged,
}

/// Writes `contents` to `<agent_dir>/extensions/omni-dev-sessions.ts`, creating
/// the directory. Refuses to replace a file of that name that is not ours.
fn write_pi_extension(agent_dir: &Path, contents: &str) -> Result<(PathBuf, PiInstall)> {
    let dir = agent_dir.join("extensions");
    let file = dir.join(PI_EXTENSION_NAME);
    if file.exists() {
        let existing = std::fs::read_to_string(&file)
            .with_context(|| format!("failed to read {}", file.display()))?;
        if existing == contents {
            return Ok((file, PiInstall::Unchanged));
        }
        if !existing.starts_with(pi_extension_marker()) {
            bail!(
                "{} exists and was not written by omni-dev; refusing to overwrite it",
                file.display()
            );
        }
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    std::fs::write(&file, contents)
        .with_context(|| format!("failed to write {}", file.display()))?;
    Ok((file, PiInstall::Written))
}

/// Removes our extension from `<agent_dir>/extensions/`, returning its path when
/// it removed it. A file of that name that is not ours is left alone.
fn remove_pi_extension(agent_dir: &Path) -> Result<Option<PathBuf>> {
    let file = agent_dir.join("extensions").join(PI_EXTENSION_NAME);
    let Ok(existing) = std::fs::read_to_string(&file) else {
        return Ok(None);
    };
    if !existing.starts_with(pi_extension_marker()) {
        return Ok(None);
    }
    std::fs::remove_file(&file).with_context(|| format!("failed to remove {}", file.display()))?;
    Ok(Some(file))
}

/// The pi half of `install-hooks`. Without `--pi-agent-dir` it installs only when
/// pi is present, so a machine without pi gets nothing under `~/.pi`.
fn install_pi_extension(explicit: Option<PathBuf>) -> Result<()> {
    let agent_dir = if let Some(dir) = explicit {
        dir
    } else {
        let dir = default_pi_agent_dir()?;
        if !pi_is_present(&dir, std::env::var_os("PATH").as_deref()) {
            println!(
                "pi.dev not found (no {} and no `pi` on PATH); skipped its extension",
                dir.display()
            );
            return Ok(());
        }
        dir
    };
    let socket = server::resolve_socket(None)?;
    let (file, outcome) = write_pi_extension(&agent_dir, &render_pi_extension(&socket))?;
    match outcome {
        PiInstall::Unchanged => println!(
            "pi.dev extension already installed at {} (no change)",
            file.display()
        ),
        PiInstall::Written => {
            println!(
                "installed the pi.dev sessions extension at {}",
                file.display()
            );
            println!("pi sessions started after this report their state");
        }
    }
    Ok(())
}

/// The pi half of `uninstall-hooks`. Runs whether or not pi is present, so an
/// uninstalled pi does not strand the file.
fn uninstall_pi_extension(explicit: Option<PathBuf>) -> Result<()> {
    let agent_dir = match explicit {
        Some(dir) => dir,
        None => default_pi_agent_dir()?,
    };
    if let Some(file) = remove_pi_extension(&agent_dir)? {
        println!("removed {}", file.display());
    }
    Ok(())
}

// --- install-wrapper / uninstall-wrapper -------------------------------------

/// The VS Code setting the Claude Code extension reads to decide what to launch
/// Claude through. Machine-scoped, so it belongs in the *user* settings file.
const WRAPPER_SETTING_KEY: &str = "claudeCode.claudeProcessWrapper";

/// Filename of the shim written into omni-dev's runtime directory.
const SHIM_NAME: &str = "claude-wrap";

/// Installs the `claude-wrap` shim and points VS Code's Claude extension at it.
#[derive(Parser)]
pub struct InstallWrapperCommand {
    /// Path to the VS Code user settings file. Defaults to the platform's
    /// `Code/User/settings.json`.
    #[arg(long, value_name = "PATH")]
    pub settings: Option<PathBuf>,

    /// Path to write the shim to. Defaults to `<data-dir>/omni-dev/claude-wrap`.
    #[arg(long, value_name = "PATH")]
    pub shim: Option<PathBuf>,
}

impl InstallWrapperCommand {
    /// Executes the install: writes the shim, then idempotently sets the
    /// extension's wrapper setting to point at it.
    pub fn execute(self) -> Result<()> {
        let shim = shim_path(self.shim)?;
        write_shim(&shim)?;
        let path = vscode_settings_path(self.settings)?;
        let target = shim.display().to_string();

        let mut settings =
            read_settings(&path).map_err(|error| manual_setup_hint(&error, &target))?;
        let changed = set_wrapper(&mut settings, &target);
        write_settings(&path, &settings)?;

        println!("wrapper shim: {target}");
        if changed {
            println!("set {WRAPPER_SETTING_KEY} in {}", path.display());
            println!("reload VS Code; Claude tabs opened after that report their exact state");
        } else {
            println!(
                "{WRAPPER_SETTING_KEY} already points there in {} (no change)",
                path.display()
            );
        }
        Ok(())
    }
}

/// Removes the wrapper setting and the shim it points at.
#[derive(Parser)]
pub struct UninstallWrapperCommand {
    /// Path to the VS Code user settings file. Defaults to the platform's
    /// `Code/User/settings.json`.
    #[arg(long, value_name = "PATH")]
    pub settings: Option<PathBuf>,

    /// Path the shim was written to. Defaults to
    /// `<data-dir>/omni-dev/claude-wrap`.
    #[arg(long, value_name = "PATH")]
    pub shim: Option<PathBuf>,
}

impl UninstallWrapperCommand {
    /// Executes the uninstall: clears the setting when (and only when) it still
    /// points at our shim, then removes the shim file.
    pub fn execute(self) -> Result<()> {
        let shim = shim_path(self.shim)?;
        let target = shim.display().to_string();
        let path = vscode_settings_path(self.settings)?;

        if path.exists() {
            let mut settings =
                read_settings(&path).map_err(|error| manual_setup_hint(&error, &target))?;
            if clear_wrapper(&mut settings, &target) {
                write_settings(&path, &settings)?;
                println!("cleared {WRAPPER_SETTING_KEY} in {}", path.display());
            } else {
                println!(
                    "{WRAPPER_SETTING_KEY} in {} does not point at our shim; left alone",
                    path.display()
                );
            }
        } else {
            println!("no settings file at {} (nothing to clear)", path.display());
        }

        if shim.exists() {
            std::fs::remove_file(&shim)
                .with_context(|| format!("failed to remove {}", shim.display()))?;
            println!("removed {target}");
        }
        Ok(())
    }
}

/// The shim's path: an explicit `--shim`, else `claude-wrap` in omni-dev's
/// runtime directory (beside the daemon socket).
fn shim_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    match explicit {
        Some(path) => Ok(path),
        None => Ok(paths::runtime_dir()?.join(SHIM_NAME)),
    }
}

/// The VS Code **user** settings path: an explicit `--settings`, else
/// `<config-dir>/Code/User/settings.json` — `~/Library/Application Support/…` on
/// macOS and `~/.config/…` on Linux, which is where a machine-scoped setting has
/// to live. (The runtime directory is `data_dir()`; this one is deliberately
/// `config_dir()`.)
fn vscode_settings_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    let base = dirs::config_dir().context("could not determine the user config directory")?;
    Ok(base.join("Code").join("User").join("settings.json"))
}

/// The shim script's contents.
///
/// The extension spawns the configured wrapper directly — no shell, no argument
/// splitting — so the setting has to name a single executable file. This is that
/// file: a one-line `exec` into `omni-dev claude-wrap`, using the absolute path
/// of the running binary so it does not depend on VS Code's `PATH`.
fn shim_contents() -> String {
    let exe = std::env::current_exe()
        .map_or_else(|_| "omni-dev".to_string(), |exe| exe.display().to_string());
    format!("#!/bin/sh\nexec \"{exe}\" claude-wrap -- \"$@\"\n")
}

/// Writes the shim, creating its directory and marking it owner-executable.
fn write_shim(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(parent) = path.parent() {
        paths::ensure_dir_0700(parent)?;
    }
    std::fs::write(path, shim_contents())
        .with_context(|| format!("failed to write {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to make {} executable", path.display()))
}

/// Points the wrapper setting at `shim`, returning whether anything changed.
fn set_wrapper(settings: &mut Value, shim: &str) -> bool {
    let Some(root) = settings.as_object_mut() else {
        return false;
    };
    if root.get(WRAPPER_SETTING_KEY).and_then(Value::as_str) == Some(shim) {
        return false;
    }
    root.insert(WRAPPER_SETTING_KEY.to_string(), json!(shim));
    true
}

/// Clears the wrapper setting when it points at `shim`, returning whether it
/// did. A setting pointing somewhere else is someone else's and is left alone.
fn clear_wrapper(settings: &mut Value, shim: &str) -> bool {
    let Some(root) = settings.as_object_mut() else {
        return false;
    };
    if root.get(WRAPPER_SETTING_KEY).and_then(Value::as_str) != Some(shim) {
        return false;
    }
    root.remove(WRAPPER_SETTING_KEY);
    true
}

/// Turns an unparseable-settings error into one that tells the user what to do.
///
/// VS Code settings files are JSON**C** — comments and trailing commas are
/// legal there and common in practice, and [`read_settings`] rightly refuses to
/// rewrite what it cannot parse. The shim is already in place by then, so the
/// only thing left is one line to paste.
fn manual_setup_hint(error: &anyhow::Error, shim: &str) -> anyhow::Error {
    anyhow!(
        "{error:#}\n\n\
         VS Code settings files may contain comments or trailing commas, which cannot be \
         rewritten safely. The shim is installed, so add this line by hand instead:\n\n    \
         \"{WRAPPER_SETTING_KEY}\": \"{shim}\""
    )
}

/// One hook event the tracker installs.
#[derive(Debug, Clone, Copy)]
struct HookSpec {
    /// The event name, as the settings file keys it.
    event: &'static str,
    /// Whether the event's group gets the `*` tool `matcher` (the tool events —
    /// `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequest`,
    /// `PermissionDenied` — match on tool name; the rest are left unmatched,
    /// which matches every occurrence).
    matcher: bool,
    /// A per-hook `timeout` in seconds, where the agent caps the event below
    /// its default.
    timeout: Option<u64>,
}

impl HookSpec {
    const fn new(event: &'static str) -> Self {
        Self {
            event,
            matcher: false,
            timeout: None,
        }
    }

    const fn matched(event: &'static str) -> Self {
        Self {
            matcher: true,
            ..Self::new(event)
        }
    }

    const fn capped(event: &'static str, secs: u64) -> Self {
        Self {
            timeout: Some(secs),
            ..Self::new(event)
        }
    }
}

/// The Claude Code hook events the tracker installs. `SessionEnd` is included —
/// it maps to the `end` op in the sink. The events after `SessionEnd` narrow
/// the hooks-only state gaps (#1915). Claude Code 2.1.280 drops a hook event it
/// does not know with a warning rather than rejecting the settings file; how
/// older versions treat one is unverified (docs/sessions-service.md).
const HOOK_EVENTS: &[HookSpec] = &[
    HookSpec::new("SessionStart"),
    HookSpec::new("UserPromptSubmit"),
    HookSpec::matched("PreToolUse"),
    HookSpec::matched("PostToolUse"),
    HookSpec::new("Notification"),
    HookSpec::new("Stop"),
    HookSpec::new("SessionEnd"),
    HookSpec::matched("PermissionRequest"),
    HookSpec::matched("PermissionDenied"),
    HookSpec::matched("PostToolUseFailure"),
    HookSpec::new("StopFailure"),
    HookSpec::new("Elicitation"),
    HookSpec::new("ElicitationResult"),
    HookSpec::new("SubagentStart"),
    HookSpec::new("SubagentStop"),
    HookSpec::new("PreCompact"),
    HookSpec::new("PostCompact"),
];

/// The Codex hook events the tracker installs into `$CODEX_HOME/hooks.json`:
/// Claude's set minus `Notification` (Codex has none), plus Codex's
/// `PermissionRequest` and `Interrupt` and the compaction and subagent events
/// (ADR-0087). Codex caps `SessionEnd` and `Interrupt` hooks at 3 s.
const CODEX_HOOK_EVENTS: &[HookSpec] = &[
    HookSpec::new("SessionStart"),
    HookSpec::new("UserPromptSubmit"),
    HookSpec::matched("PreToolUse"),
    HookSpec::matched("PostToolUse"),
    HookSpec::new("PermissionRequest"),
    HookSpec::new("Stop"),
    HookSpec::capped("Interrupt", 3),
    HookSpec::capped("SessionEnd", 3),
    HookSpec::new("PreCompact"),
    HookSpec::new("PostCompact"),
    HookSpec::new("SubagentStart"),
    HookSpec::new("SubagentStop"),
];

/// The hook command string written into settings.json: the absolute path of the
/// running binary plus `sessions hook`, so Claude Code invokes *this* omni-dev
/// regardless of its hook `PATH`. Falls back to the bare `omni-dev sessions hook`
/// when the executable path cannot be resolved (documented as the portable form).
fn hook_command() -> String {
    match std::env::current_exe() {
        Ok(exe) => format!("{} sessions hook", exe.display()),
        Err(_) => "omni-dev sessions hook".to_string(),
    }
}

/// The Claude settings file path: an explicit `--settings`, else
/// `$CLAUDE_CONFIG_DIR/settings.json`, else `~/.claude/settings.json`.
fn settings_path(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(dir).join("settings.json"));
    }
    let home = dirs::home_dir().context("could not resolve the home directory")?;
    Ok(home.join(".claude").join("settings.json"))
}

/// Reads and parses `path` into a JSON object, treating a missing file as an
/// empty object. Errors (rather than clobbering) when the file exists but is not
/// valid JSON, or is valid JSON that is not an object.
fn read_settings(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is not valid JSON; refusing to overwrite it",
            path.display()
        )
    })?;
    if !value.is_object() {
        bail!(
            "{} is not a JSON object; refusing to overwrite it",
            path.display()
        );
    }
    Ok(value)
}

/// Serializes `settings` back to `path`, pretty-printed with a trailing newline,
/// creating the parent directory if needed.
fn write_settings(path: &Path, settings: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut text = serde_json::to_string_pretty(settings)?;
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

/// Merges the sessions-tracker hook `command` into a settings object under each
/// event in `specs`, returning how many events were newly added. Idempotent (an
/// event that already has a group running `command` is skipped) and additive (it
/// never touches other hooks, and appending moves no existing group). Creates
/// `hooks` and any per-event array as needed.
fn merge_hooks(settings: &mut Value, command: &str, specs: &[HookSpec]) -> usize {
    // `read_settings` guarantees an object, but degrade gracefully rather than
    // panic if a caller passes something else.
    let Some(root) = settings.as_object_mut() else {
        return 0;
    };
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut();
    let Some(hooks) = hooks else {
        // `hooks` exists but is not an object; leave the file alone rather than
        // clobber a user's unexpected shape.
        return 0;
    };
    let mut added = 0;
    for spec in specs {
        let groups = hooks
            .entry(spec.event.to_string())
            .or_insert_with(|| json!([]));
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        if groups.iter().any(|g| group_has_command(g, command)) {
            continue; // already installed for this event
        }
        groups.push(hook_group(command, spec));
        added += 1;
    }
    added
}

/// Removes every hook entry whose command is `command` from a settings object,
/// pruning any group and per-event array left empty, and returning how many hook
/// entries were removed. Leaves all other hooks in place.
fn remove_hooks(settings: &mut Value, command: &str) -> usize {
    let Some(root) = settings.as_object_mut() else {
        return 0;
    };
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return 0;
    };
    let mut removed = 0;
    let mut empty_events = Vec::new();
    for (event, groups) in hooks.iter_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        for group in groups.iter_mut() {
            if let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) {
                let before = inner.len();
                inner.retain(|h| !hook_has_command(h, command));
                removed += before - inner.len();
            }
        }
        // Drop groups whose hook list is now empty, then the event if no groups
        // remain, so an uninstall leaves no empty scaffolding behind.
        groups.retain(|g| {
            g.get("hooks")
                .and_then(Value::as_array)
                .is_none_or(|h| !h.is_empty())
        });
        if groups.is_empty() {
            empty_events.push(event.clone());
        }
    }
    for event in empty_events {
        hooks.remove(&event);
    }
    removed
}

/// One hook group as written into an event array: `{ "hooks": [{ "type":
/// "command", "command": … }] }`, with a `"matcher": "*"` when the event matches
/// on tool name and a `timeout` where the spec caps one.
fn hook_group(command: &str, spec: &HookSpec) -> Value {
    let mut group = json!({ "hooks": [hook_entry(command, spec)] });
    if spec.matcher {
        group["matcher"] = Value::String("*".to_string());
    }
    group
}

/// One hook entry: `{ "type": "command", "command": … }`, plus a `timeout`
/// where the spec caps one.
fn hook_entry(command: &str, spec: &HookSpec) -> Value {
    let mut hook = json!({ "type": "command", "command": command });
    if let Some(secs) = spec.timeout {
        hook["timeout"] = json!(secs);
    }
    hook
}

/// Whether a hook `group` already contains a hook running `command`.
fn group_has_command(group: &Value, command: &str) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| hooks.iter().any(|h| hook_has_command(h, command)))
}

/// Whether a single hook entry runs `command`.
fn hook_has_command(hook: &Value, command: &str) -> bool {
    hook.get("command").and_then(Value::as_str) == Some(command)
}

// --- Codex hooks.json ----------------------------------------------------------

/// The one-time trust step every Codex install needs. Codex skips an untrusted
/// hook without any message, and trusts a hook by its position *and* content, so
/// a changed command string needs trusting again.
const CODEX_TRUST_STEP: &str =
    "Codex skips untrusted hooks silently: open the Codex CLI, run `/hooks`, \
and trust the omni-dev entries (once, and again after the command changes). \
One approval covers the VS Code extension and Codex Desktop.";

/// The Codex hook command: [`hook_command`] tagged `--agent codex`.
fn codex_hook_command() -> String {
    format!("{} --agent codex", hook_command())
}

/// Codex's home directory: `$CODEX_HOME`, else `~/.codex`.
fn default_codex_home() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CODEX_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(dir));
    }
    let home = dirs::home_dir().context("could not resolve the home directory")?;
    Ok(home.join(".codex"))
}

/// Whether Codex is installed: its home directory exists (Codex has run), or a
/// `codex` executable is on `path`.
fn codex_is_present(codex_home: &Path, path: Option<&std::ffi::OsStr>) -> bool {
    codex_home.is_dir()
        || path.is_some_and(|path| {
            std::env::split_paths(path).any(|dir| is_executable(&dir.join("codex")))
        })
}

/// Whether `command` runs the `omni-dev sessions hook` sink, from any path to an
/// `omni-dev` binary and with any arguments. In Codex's `hooks.json` every such
/// entry other than the current [`codex_hook_command`] is stale: an untagged or
/// `--agent claude` one (a user who wired Codex by hand) reports Codex sessions
/// as Claude's, and a tagged one from another path is left by an upgrade that
/// moved the binary.
fn is_sessions_sink(command: &str) -> bool {
    let command = command.trim();
    let Some(at) = command.find(" sessions hook") else {
        return false;
    };
    let rest = &command[at + " sessions hook".len()..];
    (rest.is_empty() || rest.starts_with(char::is_whitespace))
        && Path::new(command[..at].trim())
            .file_name()
            .is_some_and(|name| name == "omni-dev")
}

/// Rewrites every stale sink entry (see [`is_sessions_sink`]) in `settings` to
/// `command` **where it sits**, returning how many it rewrote. Left beside the
/// tagged entry, an untagged one would race it to be a session's first sighting
/// and could fix the session's tag as `claude` for good; replacing it in place
/// moves no other hook, so nothing else loses its Codex trust (ADR-0087). The
/// replaced entry takes the spec's `timeout` for its event. Extra arguments on
/// the old entry (a `--socket`, say) are not carried over: the canonical command
/// is what makes a second install recognise it.
fn replace_stale_sinks(settings: &mut Value, command: &str, specs: &[HookSpec]) -> usize {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return 0;
    };
    let mut replaced = 0;
    for (event, groups) in hooks.iter_mut() {
        let spec = specs.iter().find(|s| s.event == event);
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        for hook in groups
            .iter_mut()
            .filter_map(|g| g.get_mut("hooks").and_then(Value::as_array_mut))
            .flatten()
        {
            let stale = hook
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|c| c != command && is_sessions_sink(c));
            if !stale {
                continue;
            }
            hook["command"] = Value::String(command.to_string());
            if let Some(secs) = spec.and_then(|s| s.timeout) {
                hook["timeout"] = json!(secs);
            }
            replaced += 1;
        }
    }
    replaced
}

/// What [`remove_hooks_stable`] did.
#[derive(Debug, Default, PartialEq, Eq)]
struct Removal {
    /// How many hook entries were removed.
    removed: usize,
    /// Whether a surviving hook moved to a lower index **within its group** — the
    /// one shift a group placeholder cannot prevent, since a hook list has no
    /// empty-entry form. That hook needs trusting again.
    shifted: bool,
}

/// Removes every hook entry `is_ours` matches, **without shifting any surviving
/// group** — the position-stable counterpart of [`remove_hooks`] for Codex, which
/// trusts a hook by its `<event>:<group>:<hook>` index. A group this empties keeps
/// its slot as an inert `{ "hooks": [] }` placeholder unless no group follows it,
/// so a later group never moves into its index and silently loses its trust;
/// trailing empty groups (and then an empty event) are dropped, since nothing
/// can shift into them.
fn remove_hooks_stable(settings: &mut Value, is_ours: impl Fn(&str) -> bool) -> Removal {
    let mut outcome = Removal::default();
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
        return outcome;
    };
    let mut empty_events = Vec::new();
    for (event, groups) in hooks.iter_mut() {
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        for group in groups.iter_mut() {
            let Some(inner) = group.get_mut("hooks").and_then(Value::as_array_mut) else {
                continue;
            };
            let mut removed_before = false;
            inner.retain(|h| {
                let ours = h
                    .get("command")
                    .and_then(Value::as_str)
                    .is_some_and(&is_ours);
                if ours {
                    removed_before = true;
                    outcome.removed += 1;
                } else if removed_before {
                    outcome.shifted = true;
                }
                !ours
            });
            if removed_before && inner.is_empty() {
                *group = json!({ "hooks": [] });
            }
        }
        while groups.last().is_some_and(is_empty_group) {
            groups.pop();
        }
        if groups.is_empty() {
            empty_events.push(event.clone());
        }
    }
    for event in empty_events {
        hooks.remove(&event);
    }
    outcome
}

/// Whether `group` is a group with an empty `hooks` list.
fn is_empty_group(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
}

/// The Codex half of `install-hooks`. Without `--codex-home` it installs only
/// when Codex is present, so a machine without Codex gets nothing under
/// `~/.codex`.
fn install_codex_hooks(explicit: Option<PathBuf>) -> Result<()> {
    let codex_home = if let Some(dir) = explicit {
        dir
    } else {
        let dir = default_codex_home()?;
        if !codex_is_present(&dir, std::env::var_os("PATH").as_deref()) {
            println!(
                "Codex not found (no {} and no `codex` on PATH); skipped its hooks",
                dir.display()
            );
            return Ok(());
        }
        dir
    };
    let path = codex_home.join("hooks.json");
    let mut hooks = read_settings(&path)?;
    let command = codex_hook_command();
    let replaced = replace_stale_sinks(&mut hooks, &command, CODEX_HOOK_EVENTS);
    let added = merge_hooks(&mut hooks, &command, CODEX_HOOK_EVENTS);
    if replaced == 0 && added == 0 {
        println!(
            "Codex sessions hooks already installed in {} (no change)",
            path.display()
        );
        return Ok(());
    }
    write_settings(&path, &hooks)?;
    if replaced > 0 {
        println!(
            "rewrote {replaced} existing `sessions hook` entry(ies) in {} in place",
            path.display()
        );
    }
    if added > 0 {
        println!(
            "installed {added} Codex sessions hook event(s) into {}",
            path.display()
        );
    }
    println!("command: {command}\n{CODEX_TRUST_STEP}");
    Ok(())
}

/// The Codex half of `uninstall-hooks`. Runs whether or not Codex is present, so
/// an uninstalled Codex does not strand the entries. Removes every sink entry,
/// tagged or not and from any path (install would have rewritten it),
/// position-stably.
fn uninstall_codex_hooks(explicit: Option<PathBuf>) -> Result<()> {
    let codex_home = match explicit {
        Some(dir) => dir,
        None => default_codex_home()?,
    };
    let path = codex_home.join("hooks.json");
    if !path.exists() {
        return Ok(());
    }
    let mut hooks = read_settings(&path)?;
    // The exact current command too, in case this binary is not named `omni-dev`.
    let current = codex_hook_command();
    let Removal { removed, shifted } =
        remove_hooks_stable(&mut hooks, |c| c == current || is_sessions_sink(c));
    if removed == 0 {
        return Ok(());
    }
    write_settings(&path, &hooks)?;
    println!(
        "removed {removed} Codex sessions hook entry(ies) from {}",
        path.display()
    );
    if shifted {
        println!(
            "a hook that shared a group with ours moved up within it, so Codex no \
             longer trusts it: run `/hooks` in the Codex CLI to trust it again"
        );
    } else {
        println!("no other Codex hook moved, so their `/hooks` trust stands");
    }
    Ok(())
}

// --- shared socket + rendering ----------------------------------------------

/// Sends one `sessions` service op over the control socket, returning its
/// payload or turning an `ok: false` reply into an error.
async fn call(socket: &Path, op: &str, payload: Value) -> Result<Value> {
    let reply = DaemonClient::new(socket)
        .request(DaemonEnvelope::service(SERVICE, op, payload))
        .await?;
    reply_payload(reply)
}

/// Unwraps a daemon reply into its payload, turning an `ok: false` reply into an
/// error. Pure (no socket), so both mappings are unit-testable.
fn reply_payload(reply: DaemonReply) -> Result<Value> {
    if reply.ok {
        Ok(reply.payload)
    } else {
        bail!(
            "daemon returned an error: {}",
            reply.error.as_deref().unwrap_or("unknown error")
        )
    }
}

/// Renders a `list` reply as a human-readable table: a header and one row per
/// live session (state, source, repo, working directory, and age). Returns a
/// placeholder line when nothing is running.
fn render_sessions(result: &Value) -> String {
    let sessions = result
        .get("sessions")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if sessions.is_empty() {
        return "No active agent sessions.".to_string();
    }
    // CWD is last so a long path never misaligns the columns after it.
    let mut out = format!(
        "{:<13} {:<6} {:<8} {:<20} {:>5}  {}",
        "STATE", "AGENT", "SOURCE", "REPO", "AGE", "CWD"
    );
    for session in sessions {
        let state = state_display(session.get("state").and_then(Value::as_str).unwrap_or("-"));
        let agent = agent_label(session);
        let source = source_label(session);
        let repo = sanitize(session.get("repo").and_then(Value::as_str).unwrap_or("-"));
        let cwd = sanitize(session.get("cwd").and_then(Value::as_str).unwrap_or("-"));
        let age = age_secs(session.get("last_seen").and_then(Value::as_str));
        out.push_str(&format!(
            "\n{state:<13} {agent:<6} {source:<8} {repo:<20} {age:>4}s  {cwd}"
        ));
    }
    out
}

/// A compact, fixed-width-friendly label for a session state, so the wide
/// `waiting_for_permission` does not overflow the STATE column. Falls through to
/// the raw (sanitized) string for any unexpected value.
fn state_display(state: &str) -> String {
    match state {
        "waiting_for_permission" => "waiting-perm".to_string(),
        "waiting_for_input" => "waiting-input".to_string(),
        other => sanitize(other),
    }
}

/// The agent label for a session: `pi` for pi.dev, `codex` for Codex, else
/// `claude` (a daemon that predates the `agent` field only ever reports Claude
/// Code sessions).
fn agent_label(session: &Value) -> &'static str {
    match session.get("agent").and_then(Value::as_str) {
        Some("pi") => "pi",
        Some("codex") => "codex",
        _ => "claude",
    }
}

/// The short source label for a session: `vscode` when embedded in a VS Code
/// window, else `terminal`.
fn source_label(session: &Value) -> &'static str {
    match session.pointer("/source/kind").and_then(Value::as_str) {
        Some("vs_code") => "vscode",
        _ => "terminal",
    }
}

/// Strips control characters from an untrusted registry string so a crafted
/// payload cannot inject terminal escape sequences into the rendered table
/// (#1137). The `--json` path stays verbatim.
fn sanitize(s: &str) -> String {
    sanitize_for_terminal(s)
}

/// Seconds elapsed since an RFC 3339 timestamp (0 if absent/unparseable).
fn age_secs(ts: Option<&str>) -> i64 {
    ts.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map_or(0, |t| {
            (Utc::now() - t.with_timezone(&Utc)).num_seconds().max(0)
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::sessions::SessionState;

    /// Mirrors the `omni-dev sessions` argv surface for parse tests.
    #[derive(Parser)]
    struct Wrapper {
        #[command(subcommand)]
        cmd: SessionsSubcommands,
    }

    fn parse(args: &[&str]) -> SessionsSubcommands {
        let mut full = vec!["omni-dev"];
        full.extend_from_slice(args);
        Wrapper::try_parse_from(full).unwrap().cmd
    }

    #[test]
    fn subcommands_parse() {
        assert!(matches!(parse(&["list"]), SessionsSubcommands::List(_)));
        assert!(matches!(parse(&["hook"]), SessionsSubcommands::Hook(_)));
        assert!(matches!(
            parse(&["install-hooks"]),
            SessionsSubcommands::InstallHooks(_)
        ));
        assert!(matches!(
            parse(&["uninstall-hooks"]),
            SessionsSubcommands::UninstallHooks(_)
        ));
        assert!(matches!(
            parse(&["install-wrapper"]),
            SessionsSubcommands::InstallWrapper(_)
        ));
        assert!(matches!(
            parse(&["uninstall-wrapper"]),
            SessionsSubcommands::UninstallWrapper(_)
        ));
    }

    #[test]
    fn list_parses_flags() {
        let cmd =
            ListCommand::try_parse_from(["list", "-o", "json", "--socket", "/tmp/d.sock"]).unwrap();
        assert_eq!(cmd.output, TableOrJson::Json);
        assert_eq!(cmd.socket.as_deref(), Some(Path::new("/tmp/d.sock")));
    }

    // --- hook mapping --------------------------------------------------------

    fn hook_op(json_str: &str) -> Option<(&'static str, Value)> {
        serde_json::from_str::<HookPayload>(json_str)
            .unwrap()
            .to_op(HookAgent::Claude, None)
    }

    #[test]
    fn hook_maps_lifecycle_events_to_observe() {
        let (op, payload) = hook_op(
            r#"{"session_id":"s1","cwd":"/p","transcript_path":"/t.jsonl","hook_event_name":"PreToolUse"}"#,
        )
        .unwrap();
        assert_eq!(op, "observe");
        assert_eq!(payload["session_id"], "s1");
        assert_eq!(payload["cwd"], "/p");
        assert_eq!(payload["event"], "pre_tool_use");
    }

    #[test]
    fn hook_maps_session_start_and_stop() {
        assert_eq!(
            hook_op(r#"{"session_id":"s1","hook_event_name":"SessionStart"}"#)
                .unwrap()
                .1["event"],
            "session_start"
        );
        assert_eq!(
            hook_op(r#"{"session_id":"s1","hook_event_name":"Stop"}"#)
                .unwrap()
                .1["event"],
            "stop"
        );
    }

    #[test]
    fn hook_maps_session_end_to_end_op() {
        let (op, payload) =
            hook_op(r#"{"session_id":"s1","hook_event_name":"SessionEnd","message":"exit"}"#)
                .unwrap();
        assert_eq!(op, "end");
        assert_eq!(payload["session_id"], "s1");
        assert_eq!(payload["reason"], "exit");
    }

    #[test]
    fn hook_sends_the_agent_pid_on_both_ops_when_known() {
        let hook = |json_str: &str, pid| {
            serde_json::from_str::<HookPayload>(json_str)
                .unwrap()
                .to_op(HookAgent::Claude, pid)
                .unwrap()
        };
        let start = r#"{"session_id":"s1","hook_event_name":"SessionStart"}"#;
        let end = r#"{"session_id":"s1","hook_event_name":"SessionEnd"}"#;
        assert_eq!(hook(start, Some(42)).1["pid"], 42);
        assert_eq!(hook(end, Some(42)).1["pid"], 42);
        // Unknown: omitted from the wire, exactly as before #1948.
        assert!(hook(start, None).1.get("pid").is_none());
        assert!(hook(end, None).1.get("pid").is_none());
    }

    #[test]
    fn agent_pid_treats_an_orphaned_hook_as_unknown() {
        assert_eq!(agent_pid(4242), Some(4242));
        assert_eq!(agent_pid(1), None);
        assert_eq!(agent_pid(0), None);
    }

    #[test]
    fn hook_classifies_notifications() {
        let permission = hook_op(
            r#"{"session_id":"s1","hook_event_name":"Notification","message":"Claude needs your permission to use Bash"}"#,
        )
        .unwrap();
        assert_eq!(permission.1["event"]["notification"], "permission_prompt");

        let idle = hook_op(
            r#"{"session_id":"s1","hook_event_name":"Notification","message":"Claude is waiting for your input"}"#,
        )
        .unwrap();
        assert_eq!(idle.1["event"]["notification"], "idle_prompt");

        let other = hook_op(
            r#"{"session_id":"s1","hook_event_name":"Notification","message":"something else"}"#,
        )
        .unwrap();
        assert_eq!(other.1["event"]["notification"], "other");
    }

    #[test]
    fn hook_ignores_unknown_events_and_missing_session_id() {
        // No session_id → no op.
        assert!(hook_op(r#"{"hook_event_name":"Stop"}"#).is_none());
        // Blank session_id → no op.
        assert!(hook_op(r#"{"session_id":"  ","hook_event_name":"Stop"}"#).is_none());
        // Unknown event → no op.
        assert!(hook_op(r#"{"session_id":"s1","hook_event_name":"CwdChanged"}"#).is_none());
        // Garbage that still parses as an (empty) payload → no op.
        assert!(hook_op("{}").is_none());
    }

    fn codex_op(json_str: &str) -> Option<(&'static str, Value)> {
        serde_json::from_str::<HookPayload>(json_str)
            .unwrap()
            .to_op(HookAgent::Codex, None)
    }

    fn codex_event(event: &str, tool: Option<&str>) -> Value {
        let mut hook = json!({ "session_id": "c1", "hook_event_name": event });
        if let Some(tool) = tool {
            hook["tool_name"] = json!(tool);
        }
        codex_op(&hook.to_string()).unwrap().1["event"].clone()
    }

    #[test]
    fn codex_hooks_are_tagged_from_the_first_event() {
        let (op, payload) = codex_op(
            r#"{"session_id":"c1","cwd":"/p","model":"gpt-5.5","hook_event_name":"SessionStart","source":"startup"}"#,
        )
        .unwrap();
        assert_eq!(op, "observe");
        assert_eq!(payload["agent"], "codex");
        assert_eq!(payload["model"], "gpt-5.5");
        assert_eq!(payload["event"], "session_start");
        // The Claude sink stays untagged on the wire.
        let claude = hook_op(r#"{"session_id":"s1","hook_event_name":"Stop"}"#).unwrap();
        assert!(claude.1.get("agent").is_none());
    }

    #[test]
    fn codex_events_map_onto_the_existing_session_events() {
        assert_eq!(codex_event("UserPromptSubmit", None), "user_prompt_submit");
        assert_eq!(codex_event("PreToolUse", Some("Bash")), "pre_tool_use");
        assert_eq!(codex_event("PostToolUse", Some("Bash")), "post_tool_use");
        assert_eq!(codex_event("Stop", None), "stop");
        let compact =
            json!({ "session_id": "c1", "hook_event_name": "SessionStart", "source": "compact" });
        assert_eq!(
            codex_op(&compact.to_string()).unwrap().1["event"],
            "transcript_discovered",
            "a compaction start is not a new session (#1946)"
        );
        assert_eq!(
            codex_event("PermissionRequest", Some("Bash"))["notification"],
            "permission_prompt"
        );
        assert_eq!(
            codex_event("PreToolUse", Some("request_user_input"))["notification"],
            "agent_needs_input"
        );
        // Its resolution is an ordinary PostToolUse.
        assert_eq!(
            codex_event("PostToolUse", Some("request_user_input")),
            "post_tool_use"
        );
        assert_eq!(codex_event("Interrupt", None), "stop");
        for heartbeat in ["SubagentStart", "SubagentStop", "PreCompact", "PostCompact"] {
            assert_eq!(codex_event(heartbeat, None), "post_tool_use", "{heartbeat}");
        }
        // Codex has no Notification event; an unknown one is ignored.
        assert!(codex_op(r#"{"session_id":"c1","hook_event_name":"Notification"}"#).is_none());
    }

    #[test]
    fn codex_session_end_is_the_end_op_with_its_reason() {
        let (op, payload) =
            codex_op(r#"{"session_id":"c1","hook_event_name":"SessionEnd","reason":"other"}"#)
                .unwrap();
        assert_eq!(op, "end");
        assert_eq!(payload["reason"], "other");
    }

    #[test]
    fn the_claude_sink_ignores_codex_only_events() {
        // Codex's `Interrupt` and its `request_user_input` tool have no Claude
        // equivalent, so the untagged sink keeps its own mapping for both.
        assert!(hook_op(r#"{"session_id":"s1","hook_event_name":"Interrupt"}"#).is_none());
        assert_eq!(
            hook_op(r#"{"session_id":"s1","hook_event_name":"PreToolUse","tool_name":"request_user_input"}"#)
                .unwrap()
                .1["event"],
            "pre_tool_use"
        );
    }

    fn claude_event(event: &str) -> Value {
        let hook = json!({ "session_id": "s1", "hook_event_name": event });
        hook_op(&hook.to_string()).unwrap().1["event"].clone()
    }

    #[test]
    fn claude_newer_events_map_onto_the_existing_session_events() {
        assert_eq!(
            claude_event("PermissionRequest")["notification"],
            "permission_prompt"
        );
        assert_eq!(
            claude_event("Elicitation")["notification"],
            "agent_needs_input"
        );
        for working in [
            "PostToolUseFailure",
            "PermissionDenied",
            "ElicitationResult",
            "SubagentStart",
        ] {
            assert_eq!(claude_event(working), "post_tool_use", "{working}");
        }
        assert_eq!(claude_event("StopFailure"), "stop");
        // Liveness-only sightings: each can fire while the session is idle (a
        // background subagent, a manual /compact) and must not turn it working.
        for preserving in ["SubagentStop", "PreCompact", "PostCompact"] {
            assert_eq!(
                claude_event(preserving),
                "transcript_discovered",
                "{preserving}"
            );
        }
    }

    #[test]
    fn every_installed_claude_event_is_mapped_by_the_sink() {
        // An installed event the sink ignores would spawn a process per event
        // for nothing.
        for HookSpec { event, .. } in HOOK_EVENTS {
            let hook = json!({ "session_id": "s1", "hook_event_name": event });
            assert!(hook_op(&hook.to_string()).is_some(), "{event} is unmapped");
        }
    }

    #[test]
    fn a_finished_subagent_or_compaction_leaves_an_idle_session_idle() {
        for preserving in ["SubagentStop", "PreCompact", "PostCompact"] {
            let hook = json!({ "session_id": "s1", "hook_event_name": preserving });
            let (_, payload) = hook_op(&hook.to_string()).unwrap();
            let event: SessionEvent = serde_json::from_value(payload["event"].clone()).unwrap();
            assert_eq!(
                SessionState::for_event(&event, Some(SessionState::Idle)),
                SessionState::Idle,
                "{preserving}"
            );
            assert_eq!(
                SessionState::for_event(&event, Some(SessionState::WaitingForPermission)),
                SessionState::WaitingForPermission,
                "{preserving}"
            );
        }
    }

    #[test]
    fn a_compaction_session_start_preserves_state() {
        let event_for = |source: Option<&str>| {
            let mut hook = json!({ "session_id": "s1", "hook_event_name": "SessionStart" });
            if let Some(source) = source {
                hook["source"] = json!(source);
            }
            let (_, payload) = hook_op(&hook.to_string()).unwrap();
            serde_json::from_value::<SessionEvent>(payload["event"].clone()).unwrap()
        };
        // Claude fires it after a compaction, possibly mid-turn: a working
        // session must stay working, an idle one idle (#1946).
        let compact = event_for(Some("compact"));
        assert_eq!(compact, SessionEvent::TranscriptDiscovered);
        for current in [SessionState::Working, SessionState::Idle] {
            assert_eq!(SessionState::for_event(&compact, Some(current)), current);
        }
        for start in [Some("startup"), Some("resume"), Some("clear"), None] {
            assert_eq!(event_for(start), SessionEvent::SessionStart, "{start:?}");
        }
    }

    #[test]
    fn hook_classifies_notifications_by_type_before_message() {
        let typed = |ty: &str, msg: &str| {
            let hook = json!({
                "session_id": "s1",
                "hook_event_name": "Notification",
                "notification_type": ty,
                "message": msg,
            });
            hook_op(&hook.to_string()).unwrap().1["event"]["notification"].clone()
        };
        // The type wins over a message the substring match would misread.
        assert_eq!(
            typed("permission_prompt", "something else"),
            "permission_prompt"
        );
        assert_eq!(typed("idle_prompt", "Please approve"), "idle_prompt");
        assert_eq!(typed("elicitation_dialog", "x"), "agent_needs_input");
        // A type we do not map is `other`, never a guess at its message.
        assert_eq!(typed("auth_success", "You can now allow access"), "other");
        // With no type at all (an older version), the message is classified.
        let untyped = hook_op(
            r#"{"session_id":"s1","hook_event_name":"Notification","message":"Claude needs your permission"}"#,
        )
        .unwrap();
        assert_eq!(untyped.1["event"]["notification"], "permission_prompt");
    }

    #[test]
    fn hook_parses_the_agent_flag() {
        match parse(&["hook", "--agent", "codex"]) {
            SessionsSubcommands::Hook(cmd) => assert_eq!(cmd.agent, HookAgent::Codex),
            _ => panic!("expected hook"),
        }
        match parse(&["hook"]) {
            SessionsSubcommands::Hook(cmd) => assert_eq!(cmd.agent, HookAgent::Claude),
            _ => panic!("expected hook"),
        }
        match parse(&["install-hooks", "--codex-home", "/x/codex"]) {
            SessionsSubcommands::InstallHooks(cmd) => {
                assert_eq!(cmd.codex_home, Some(PathBuf::from("/x/codex")));
            }
            _ => panic!("expected install-hooks"),
        }
    }

    // --- Codex hooks.json ---------------------------------------------------

    const TAGGED: &str = "/bin/omni-dev sessions hook --agent codex";

    /// The `(group index, commands)` layout of one event, for position checks.
    fn layout(hooks: &Value, event: &str) -> Vec<Vec<String>> {
        hooks["hooks"][event]
            .as_array()
            .map(|groups| {
                groups
                    .iter()
                    .map(|g| {
                        g["hooks"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|h| h["command"].as_str().unwrap().to_string())
                            .collect()
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn is_sessions_sink_matches_any_omni_dev_sink_and_nothing_else() {
        assert!(is_sessions_sink(
            "/Users/me/.cargo/bin/omni-dev sessions hook"
        ));
        assert!(is_sessions_sink("omni-dev sessions hook"));
        assert!(is_sessions_sink(TAGGED));
        assert!(is_sessions_sink("omni-dev sessions hook --socket /x"));
        assert!(is_sessions_sink(
            "/old/omni-dev sessions hook --agent codex"
        ));
        assert!(!is_sessions_sink("/bin/other sessions hook"));
        assert!(!is_sessions_sink("omni-dev sessions list"));
        assert!(!is_sessions_sink("omni-dev sessions hooks"));
        assert!(!is_sessions_sink("my-omni-dev sessions hook"));
    }

    #[test]
    fn codex_install_rewrites_a_moved_binarys_tagged_entry_in_place() {
        let mut hooks = json!({ "hooks": { "Stop": [
            { "hooks": [{ "type": "command", "command": "/old/omni-dev sessions hook --agent codex" }] },
            { "hooks": [{ "type": "command", "command": "omni-dev sessions hook --socket /x" }] },
            { "hooks": [{ "type": "command", "command": "mine" }] }
        ] } });
        assert_eq!(
            replace_stale_sinks(&mut hooks, TAGGED, CODEX_HOOK_EVENTS),
            2
        );
        assert_eq!(
            layout(&hooks, "Stop"),
            vec![vec![TAGGED], vec![TAGGED], vec!["mine"]]
        );
        // Already tagged by the current binary: nothing added under Stop.
        merge_hooks(&mut hooks, TAGGED, CODEX_HOOK_EVENTS);
        assert_eq!(layout(&hooks, "Stop").len(), 3);
    }

    #[test]
    fn codex_install_replaces_untagged_sinks_in_place_and_never_reorders() {
        let mut hooks = json!({ "hooks": {
            "PreToolUse": [
                { "matcher": "Bash", "hooks": [{ "type": "command", "command": "guard" }] },
                { "matcher": "*", "hooks": [{ "type": "command", "command": "/old/omni-dev sessions hook" }] },
                { "hooks": [{ "type": "command", "command": "audit" }] }
            ],
            "SessionEnd": [
                { "hooks": [{ "type": "command", "command": "omni-dev sessions hook" }] }
            ]
        }});
        let replaced = replace_stale_sinks(&mut hooks, TAGGED, CODEX_HOOK_EVENTS);
        assert_eq!(replaced, 2);
        let added = merge_hooks(&mut hooks, TAGGED, CODEX_HOOK_EVENTS);
        // Only the events without a (now tagged) sink got a new group.
        assert_eq!(added, CODEX_HOOK_EVENTS.len() - 2);
        // The untagged sink became the tagged one where it sat; nothing moved.
        assert_eq!(
            layout(&hooks, "PreToolUse"),
            vec![vec!["guard"], vec![TAGGED], vec!["audit"]]
        );
        assert_eq!(hooks["hooks"]["PreToolUse"][1]["matcher"], "*");
        // The capped event picked up its timeout in place.
        assert_eq!(hooks["hooks"]["SessionEnd"][0]["hooks"][0]["timeout"], 3);
        // Appended groups carry the matcher and timeout their event needs.
        assert_eq!(hooks["hooks"]["PostToolUse"][0]["matcher"], "*");
        assert_eq!(hooks["hooks"]["Interrupt"][0]["hooks"][0]["timeout"], 3);
        assert!(hooks["hooks"]["Stop"][0]["hooks"][0]
            .get("timeout")
            .is_none());
        assert!(hooks["hooks"].get("Notification").is_none());
        // Idempotent.
        assert_eq!(
            replace_stale_sinks(&mut hooks, TAGGED, CODEX_HOOK_EVENTS),
            0
        );
        assert_eq!(merge_hooks(&mut hooks, TAGGED, CODEX_HOOK_EVENTS), 0);
    }

    #[test]
    fn codex_uninstall_never_shifts_a_surviving_group() {
        let mut hooks = json!({ "hooks": {
            "Stop": [
                { "hooks": [{ "type": "command", "command": TAGGED }] },
                { "hooks": [{ "type": "command", "command": "user-a" }] },
                { "hooks": [{ "type": "command", "command": "omni-dev sessions hook" }] },
                { "hooks": [{ "type": "command", "command": "user-b" }] },
                { "hooks": [{ "type": "command", "command": TAGGED }] }
            ],
            "PreToolUse": [
                { "hooks": [
                    { "type": "command", "command": TAGGED },
                    { "type": "command", "command": "shared" }
                ] }
            ],
            "Interrupt": [
                { "hooks": [] },
                { "hooks": [{ "type": "command", "command": TAGGED }] }
            ],
            "PostToolUse": [
                { "hooks": [{ "type": "command", "command": "keep" }] }
            ]
        }});
        let removal = remove_hooks_stable(&mut hooks, is_sessions_sink);
        assert_eq!(removal.removed, 5);
        // `shared` moved from hook index 1 to 0 within its group.
        assert!(removal.shifted);
        // Emptied non-trailing groups stay as placeholders, so user-a and user-b
        // keep their indices (1 and 3); the trailing emptied group is dropped.
        let stop = hooks["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 4);
        assert_eq!(stop[0], json!({ "hooks": [] }));
        assert_eq!(layout(&hooks, "Stop")[1], vec!["user-a"]);
        assert_eq!(stop[2], json!({ "hooks": [] }));
        assert_eq!(layout(&hooks, "Stop")[3], vec!["user-b"]);
        // A group with other hooks keeps them.
        assert_eq!(layout(&hooks, "PreToolUse"), vec![vec!["shared"]]);
        // Trailing placeholders, then the empty event, are dropped.
        assert!(hooks["hooks"].get("Interrupt").is_none());
        assert_eq!(layout(&hooks, "PostToolUse"), vec![vec!["keep"]]);
        // A second pass removes nothing.
        assert_eq!(
            remove_hooks_stable(&mut hooks, is_sessions_sink),
            Removal::default()
        );
    }

    #[test]
    fn remove_hooks_stable_does_not_report_a_shift_for_a_trailing_hook() {
        let mut hooks = json!({ "hooks": { "Stop": [ { "hooks": [
            { "type": "command", "command": "shared" },
            { "type": "command", "command": TAGGED }
        ] } ] } });
        let removal = remove_hooks_stable(&mut hooks, is_sessions_sink);
        assert_eq!(
            removal,
            Removal {
                removed: 1,
                shifted: false
            }
        );
        assert_eq!(layout(&hooks, "Stop"), vec![vec!["shared"]]);
    }

    #[test]
    fn remove_hooks_stable_tolerates_malformed_shapes() {
        let none = Removal::default();
        assert_eq!(remove_hooks_stable(&mut json!([]), |_| true), none);
        assert_eq!(remove_hooks_stable(&mut json!({}), |_| true), none);
        let mut odd = json!({ "hooks": { "Stop": 5, "Start": [ { "matcher": "x" } ] } });
        assert_eq!(remove_hooks_stable(&mut odd, |_| true), none);
        assert_eq!(odd["hooks"]["Stop"], 5);
        assert_eq!(
            replace_stale_sinks(&mut json!({}), TAGGED, CODEX_HOOK_EVENTS),
            0
        );
        assert_eq!(
            replace_stale_sinks(
                &mut json!({ "hooks": { "Stop": 5 } }),
                TAGGED,
                CODEX_HOOK_EVENTS
            ),
            0
        );
    }

    #[test]
    fn codex_is_present_via_its_home_or_a_codex_executable_on_path() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("codex-home");
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = std::env::join_paths([&bin]).unwrap();
        assert!(!codex_is_present(&home, Some(&path)));
        assert!(!codex_is_present(&home, None));
        let exe = bin.join("codex");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(codex_is_present(&home, Some(&path)));
        std::fs::create_dir_all(&home).unwrap();
        assert!(codex_is_present(&home, None));
    }

    #[test]
    fn codex_hook_command_is_the_sink_tagged_codex() {
        assert_eq!(
            codex_hook_command(),
            format!("{} --agent codex", hook_command())
        );
        assert!(codex_hook_command().ends_with(" sessions hook --agent codex"));
    }

    #[test]
    fn codex_install_and_uninstall_round_trip_through_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("codex-home");
        std::fs::create_dir_all(&home).unwrap();
        let file = home.join("hooks.json");
        std::fs::write(
            &file,
            json!({ "hooks": { "Stop": [
                { "hooks": [{ "type": "command", "command": "omni-dev sessions hook" }] },
                { "hooks": [{ "type": "command", "command": "mine" }] }
            ] } })
            .to_string(),
        )
        .unwrap();
        install_codex_hooks(Some(home.clone())).unwrap();
        let tagged = codex_hook_command();
        let installed = read_settings(&file).unwrap();
        assert_eq!(
            layout(&installed, "Stop"),
            vec![vec![tagged.clone()], vec!["mine".to_string()]]
        );
        assert_eq!(layout(&installed, "Interrupt"), vec![vec![tagged]]);
        // A second install is the no-change branch and leaves the file as is.
        install_codex_hooks(Some(home.clone())).unwrap();
        assert_eq!(read_settings(&file).unwrap(), installed);

        uninstall_codex_hooks(Some(home.clone())).unwrap();
        let removed = read_settings(&file).unwrap();
        assert_eq!(
            removed,
            json!({ "hooks": { "Stop": [ { "hooks": [] }, { "hooks": [{ "type": "command", "command": "mine" }] } ] } })
        );
        // Nothing left to remove, and a missing home is a no-op.
        uninstall_codex_hooks(Some(home)).unwrap();
        uninstall_codex_hooks(Some(tmp.path().join("absent"))).unwrap();
    }

    #[test]
    fn install_codex_hooks_via_the_default_home_skips_absent_and_writes_present() {
        let _guard = PI_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved = ["CODEX_HOME", "PATH", "HOME"].map(|k| (k, std::env::var_os(k)));

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("codex-home");
        let empty_bin = tmp.path().join("bin");
        std::fs::create_dir_all(&empty_bin).unwrap();
        std::env::set_var("CODEX_HOME", &home);
        std::env::set_var("PATH", std::env::join_paths([&empty_bin]).unwrap());

        assert_eq!(default_codex_home().unwrap(), home);
        install_codex_hooks(None).unwrap();
        assert!(
            !home.exists(),
            "install must not create an absent Codex home"
        );
        uninstall_codex_hooks(None).unwrap();

        std::fs::create_dir_all(&home).unwrap();
        install_codex_hooks(None).unwrap();
        assert!(home.join("hooks.json").exists());
        uninstall_codex_hooks(None).unwrap();
        assert_eq!(
            read_settings(&home.join("hooks.json")).unwrap(),
            json!({ "hooks": {} })
        );

        // An empty CODEX_HOME falls back to `$HOME/.codex`.
        std::env::set_var("CODEX_HOME", "");
        std::env::set_var("HOME", "/home/tester");
        assert_eq!(
            default_codex_home().unwrap(),
            PathBuf::from("/home/tester/.codex")
        );

        for (k, v) in saved {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn classify_notification_covers_cases() {
        assert_eq!(
            classify_notification(None, Some("Please approve this")),
            NotificationKind::PermissionPrompt
        );
        assert_eq!(
            classify_notification(None, Some("Claude is idle")),
            NotificationKind::IdlePrompt
        );
        assert_eq!(classify_notification(None, None), NotificationKind::Other);
        assert_eq!(
            classify_notification(Some("idle_prompt"), None),
            NotificationKind::IdlePrompt
        );
    }

    // --- install / uninstall hooks ------------------------------------------

    #[test]
    fn merge_hooks_is_idempotent_and_additive() {
        // A pre-existing unrelated hook must survive the merge.
        let mut settings = json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "other-tool" }] }
                ]
            },
            "model": "sonnet"
        });
        let cmd = "/usr/bin/omni-dev sessions hook";
        let added = merge_hooks(&mut settings, cmd, HOOK_EVENTS);
        assert_eq!(added, HOOK_EVENTS.len());

        // Our command landed under every event, and the unrelated hook stands.
        for HookSpec { event, .. } in HOOK_EVENTS {
            let groups = settings["hooks"][event].as_array().unwrap();
            assert!(
                groups.iter().any(|g| group_has_command(g, cmd)),
                "missing under {event}"
            );
        }
        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert!(pre.iter().any(|g| group_has_command(g, "other-tool")));
        assert_eq!(settings["model"], "sonnet");

        // A second merge is a no-op.
        assert_eq!(merge_hooks(&mut settings, cmd, HOOK_EVENTS), 0);
    }

    #[test]
    fn reinstalling_over_the_original_seven_events_adds_only_the_new_ones() {
        // An install from before #1915 carries only the first seven events.
        let cmd = "/usr/bin/omni-dev sessions hook";
        let original = &HOOK_EVENTS[..7];
        assert_eq!(original.last().unwrap().event, "SessionEnd");
        let mut settings = json!({});
        merge_hooks(&mut settings, cmd, original);
        let before = settings.clone();

        let added = merge_hooks(&mut settings, cmd, HOOK_EVENTS);
        assert_eq!(added, HOOK_EVENTS.len() - 7);
        // The existing groups are untouched: one group each, as before.
        for HookSpec { event, .. } in original {
            assert_eq!(settings["hooks"][event], before["hooks"][event], "{event}");
        }
        // Tool events get the `*` matcher; the others none.
        assert_eq!(settings["hooks"]["PermissionRequest"][0]["matcher"], "*");
        assert!(settings["hooks"]["StopFailure"][0].get("matcher").is_none());

        // Uninstall removes every event, old and new.
        assert_eq!(remove_hooks(&mut settings, cmd), HOOK_EVENTS.len());
        assert!(settings["hooks"].as_object().unwrap().is_empty());
    }

    #[test]
    fn merge_then_remove_round_trips_and_preserves_others() {
        let mut settings = json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "keep-me" }] }
                ]
            }
        });
        let cmd = "/usr/bin/omni-dev sessions hook";
        merge_hooks(&mut settings, cmd, HOOK_EVENTS);
        let removed = remove_hooks(&mut settings, cmd);
        assert_eq!(removed, HOOK_EVENTS.len());

        // Every one of our entries is gone...
        for HookSpec { event, .. } in HOOK_EVENTS {
            let empty = settings["hooks"]
                .get(event)
                .and_then(Value::as_array)
                .is_none_or(|g| g.iter().all(|g| !group_has_command(g, cmd)));
            assert!(empty, "our hook survived under {event}");
        }
        // ...but the unrelated PreToolUse hook remains.
        let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
        assert!(pre.iter().any(|g| group_has_command(g, "keep-me")));
    }

    #[test]
    fn remove_hooks_prunes_empty_events_entirely() {
        let mut settings = json!({});
        let cmd = "cmd sessions hook";
        merge_hooks(&mut settings, cmd, HOOK_EVENTS);
        remove_hooks(&mut settings, cmd);
        // With no other hooks, every event array empties and is pruned.
        let hooks = settings["hooks"].as_object().unwrap();
        assert!(hooks.is_empty(), "expected all events pruned: {hooks:?}");
    }

    #[test]
    fn install_uninstall_via_files_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        // Install into a missing file, then uninstall.
        let mut settings = read_settings(&path).unwrap();
        merge_hooks(&mut settings, "cmd sessions hook", HOOK_EVENTS);
        write_settings(&path, &settings).unwrap();
        assert!(path.exists());

        let reloaded = read_settings(&path).unwrap();
        assert!(reloaded["hooks"]["Stop"].is_array());

        let mut settings = read_settings(&path).unwrap();
        remove_hooks(&mut settings, "cmd sessions hook");
        write_settings(&path, &settings).unwrap();
        let reloaded = read_settings(&path).unwrap();
        assert!(reloaded["hooks"].as_object().unwrap().is_empty());
    }

    #[test]
    fn read_settings_rejects_non_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        std::fs::write(&path, "not json {").unwrap();
        let err = read_settings(&path).unwrap_err();
        assert!(err.to_string().contains("not valid JSON"), "{err}");
    }

    #[test]
    fn read_settings_handles_empty_and_non_object() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        // An empty (or whitespace-only) file reads as an empty object.
        std::fs::write(&path, "   \n").unwrap();
        assert_eq!(read_settings(&path).unwrap(), json!({}));
        // Valid JSON that is not an object is refused rather than clobbered.
        std::fs::write(&path, "[1, 2, 3]").unwrap();
        let err = read_settings(&path).unwrap_err();
        assert!(err.to_string().contains("not a JSON object"), "{err}");
    }

    #[test]
    fn hook_command_targets_sessions_hook() {
        assert!(hook_command().ends_with("sessions hook"));
    }

    // --- rendering -----------------------------------------------------------

    #[test]
    fn render_sessions_handles_empty() {
        assert_eq!(
            render_sessions(&json!({ "sessions": [] })),
            "No active agent sessions."
        );
        assert_eq!(render_sessions(&json!({})), "No active agent sessions.");
    }

    #[test]
    fn render_sessions_renders_rows_and_source() {
        let result = json!({ "sessions": [{
            "session_id": "s1",
            "state": "working",
            "source": { "kind": "vs_code", "window_key": "w1" },
            "repo": "omni-dev",
            "cwd": "/home/me/omni-dev",
            "last_seen": "2000-01-01T00:00:00Z",
        }]});
        let table = render_sessions(&result);
        assert!(table.contains("working"), "{table}");
        assert!(table.contains("vscode"), "{table}");
        assert!(table.contains("omni-dev"), "{table}");
        assert!(table.contains("claude"), "{table}");
        // Header plus one data row.
        assert_eq!(table.lines().count(), 2, "{table}");
    }

    #[test]
    fn render_sessions_strips_control_bytes() {
        let result = json!({ "sessions": [{
            "session_id": "s1",
            "state": "wor\x1b[31mking",
            "source": { "kind": "terminal" },
            "repo": "ev\x07il",
            "cwd": "/tmp/a\rb",
            "last_seen": "2000-01-01T00:00:00Z",
        }]});
        let table = render_sessions(&result);
        assert!(
            !table.contains(|c: char| c.is_control() && c != '\n'),
            "{table:?}"
        );
        // Embedded CR cannot forge a row: header plus one data row.
        assert_eq!(table.lines().count(), 2, "{table:?}");
    }

    #[test]
    fn source_label_maps_kinds() {
        assert_eq!(
            source_label(&json!({ "source": { "kind": "vs_code", "window_key": "w" } })),
            "vscode"
        );
        assert_eq!(
            source_label(&json!({ "source": { "kind": "terminal" } })),
            "terminal"
        );
        assert_eq!(source_label(&json!({})), "terminal");
    }

    #[test]
    fn reply_payload_unwraps_ok_and_maps_errors() {
        assert_eq!(
            reply_payload(DaemonReply::ok(json!({ "a": 1 }))).unwrap(),
            json!({ "a": 1 })
        );
        let err = reply_payload(DaemonReply::err("boom")).unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }

    // --- pi.dev extension ----------------------------------------------------

    #[test]
    fn render_pi_extension_bakes_in_the_socket_as_a_json_string() {
        let rendered = render_pi_extension(Path::new("/tmp/we \"ird\"/daemon.sock"));
        assert!(
            !rendered.contains(PI_SOCKET_PLACEHOLDER),
            "placeholder left behind"
        );
        assert!(
            rendered.contains(r#"const SOCKET: string = "/tmp/we \"ird\"/daemon.sock";"#),
            "socket not escaped as a JSON string"
        );
        assert!(rendered.starts_with(pi_extension_marker()));
    }

    #[test]
    fn the_template_reports_only_state_and_tags_the_agent() {
        // Each state the mapping can produce, the agent tag, and the `end` op.
        for needle in [
            r#""working""#,
            r#""idle""#,
            r#""waiting_for_input""#,
            r#"agent: "pi""#,
            "stream_state: next",
            r#"send("end""#,
            r#""agent_settled""#,
            r#""ui_prompt_start""#,
            r#""session_shutdown""#,
        ] {
            assert!(
                PI_EXTENSION_TEMPLATE.contains(needle),
                "template lacks {needle}"
            );
        }
        // pi has no permission prompt, so the template must never claim one.
        assert!(!PI_EXTENSION_TEMPLATE.contains("waiting_for_permission"));
        // pi's chat input is already live at `session_start`, so the template
        // must report `idle` there, never the hook-only `starting` state, which
        // means "launched, chat surface not up yet" (reporting it once left every
        // pi session showing as busy from launch until the first prompt, #1905).
        assert!(!PI_EXTENSION_TEMPLATE.contains("\"starting\""));
        assert_eq!(
            PI_EXTENSION_TEMPLATE.matches(PI_SOCKET_PLACEHOLDER).count(),
            1
        );
    }

    #[test]
    fn pi_is_present_via_its_agent_dir_or_a_pi_executable_on_path() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("agent");
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = std::env::join_paths([bin.clone()]).unwrap();

        // Neither: absent.
        assert!(!pi_is_present(&agent, Some(&path)));
        assert!(!pi_is_present(&agent, None));

        // A non-executable `pi` does not count.
        let pi = bin.join("pi");
        std::fs::write(&pi, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&pi, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(!pi_is_present(&agent, Some(&path)));

        // An executable one does.
        std::fs::set_permissions(&pi, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(pi_is_present(&agent, Some(&path)));

        // So does the agent directory alone.
        std::fs::create_dir_all(&agent).unwrap();
        assert!(pi_is_present(&agent, None));
    }

    #[test]
    fn write_pi_extension_is_idempotent_and_updates_its_own_file() {
        let tmp = tempfile::tempdir().unwrap();
        let v1 = render_pi_extension(Path::new("/a.sock"));
        let (file, outcome) = write_pi_extension(tmp.path(), &v1).unwrap();
        assert_eq!(outcome, PiInstall::Written);
        assert_eq!(file, tmp.path().join("extensions").join(PI_EXTENSION_NAME));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), v1);

        assert_eq!(
            write_pi_extension(tmp.path(), &v1).unwrap().1,
            PiInstall::Unchanged
        );

        // A new socket path (or a newer template) replaces our own file.
        let v2 = render_pi_extension(Path::new("/b.sock"));
        assert_eq!(
            write_pi_extension(tmp.path(), &v2).unwrap().1,
            PiInstall::Written
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), v2);
    }

    #[test]
    fn pi_extension_install_and_remove_leave_foreign_files_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("extensions");
        std::fs::create_dir_all(&dir).unwrap();
        let other = dir.join("someone-elses.ts");
        std::fs::write(&other, "export default () => {};\n").unwrap();

        // A same-named file that is not ours is neither overwritten nor removed.
        let ours = dir.join(PI_EXTENSION_NAME);
        std::fs::write(&ours, "// hand-written\n").unwrap();
        let err = write_pi_extension(tmp.path(), &render_pi_extension(Path::new("/s")))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not written by omni-dev"), "{err}");
        assert_eq!(remove_pi_extension(tmp.path()).unwrap(), None);
        assert_eq!(std::fs::read_to_string(&ours).unwrap(), "// hand-written\n");

        // Ours is removed; the neighbour survives.
        std::fs::remove_file(&ours).unwrap();
        write_pi_extension(tmp.path(), &render_pi_extension(Path::new("/s"))).unwrap();
        assert_eq!(remove_pi_extension(tmp.path()).unwrap(), Some(ours.clone()));
        assert!(!ours.exists());
        assert!(other.exists());

        // Removing again, or from a directory that never existed, is a no-op.
        assert_eq!(remove_pi_extension(tmp.path()).unwrap(), None);
        assert_eq!(remove_pi_extension(&tmp.path().join("nope")).unwrap(), None);
    }

    #[test]
    fn hook_commands_parse_the_pi_agent_dir_flag() {
        match parse(&["install-hooks", "--pi-agent-dir", "/x/agent"]) {
            SessionsSubcommands::InstallHooks(cmd) => {
                assert_eq!(cmd.pi_agent_dir, Some(PathBuf::from("/x/agent")));
            }
            _ => panic!("expected install-hooks"), // omni-dev: coverage ignore-line reason="the arm above always matches: parse() above always parses an install-hooks argv into SessionsSubcommands::InstallHooks"
        }
        match parse(&["uninstall-hooks"]) {
            SessionsSubcommands::UninstallHooks(cmd) => assert_eq!(cmd.pi_agent_dir, None),
            _ => panic!("expected uninstall-hooks"), // omni-dev: coverage ignore-line reason="the arm above always matches: parse() above always parses an uninstall-hooks argv into SessionsSubcommands::UninstallHooks"
        }
    }

    /// Env-isolation lock for tests that mutate `PI_CODING_AGENT_DIR`, `PATH`,
    /// or `HOME` to exercise [`default_pi_agent_dir`]'s and
    /// [`install_pi_extension`]'s env-driven branches. Aliases the crate-wide
    /// [`crate::test_support::HOME_ENV_MUTEX`] per its own doc comment
    /// (issue #1465), rather than a module-local `Mutex<()>`.
    static PI_ENV_LOCK: &std::sync::Mutex<()> = &crate::test_support::HOME_ENV_MUTEX;

    fn snapshot_pi_env() -> [(&'static str, Option<std::ffi::OsString>); 3] {
        ["PI_CODING_AGENT_DIR", "PATH", "HOME"].map(|k| (k, std::env::var_os(k)))
    }

    fn restore_pi_env(snap: [(&'static str, Option<std::ffi::OsString>); 3]) {
        for (k, v) in snap {
            match v {
                Some(val) => std::env::set_var(k, val),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn default_pi_agent_dir_honors_the_env_var_and_falls_back_to_home() {
        let _guard = PI_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snap = snapshot_pi_env();

        // An explicit, non-empty env var wins.
        std::env::set_var("PI_CODING_AGENT_DIR", "/explicit/agent");
        assert_eq!(
            default_pi_agent_dir().unwrap(),
            PathBuf::from("/explicit/agent")
        );

        // An empty env var is treated as unset, falling back to `$HOME/.pi/agent`.
        std::env::set_var("PI_CODING_AGENT_DIR", "");
        std::env::set_var("HOME", "/home/tester");
        assert_eq!(
            default_pi_agent_dir().unwrap(),
            PathBuf::from("/home/tester/.pi/agent")
        );

        // A wholly unset env var falls back the same way.
        std::env::remove_var("PI_CODING_AGENT_DIR");
        assert_eq!(
            default_pi_agent_dir().unwrap(),
            PathBuf::from("/home/tester/.pi/agent")
        );

        restore_pi_env(snap);
    }

    #[test]
    fn install_pi_extension_via_the_default_dir_skips_absent_and_writes_present() {
        let _guard = PI_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let snap = snapshot_pi_env();

        let tmp = tempfile::tempdir().unwrap();
        let agent_dir = tmp.path().join("nonexistent-agent");
        let empty_bin = tmp.path().join("bin");
        std::fs::create_dir_all(&empty_bin).unwrap();

        std::env::set_var("PI_CODING_AGENT_DIR", &agent_dir);
        std::env::set_var("PATH", std::env::join_paths([&empty_bin]).unwrap());

        // Neither the agent dir exists nor is `pi` on PATH: install skips,
        // via default_pi_agent_dir()'s env-var branch.
        install_pi_extension(None).unwrap();
        assert!(
            !agent_dir.exists(),
            "install must not create the agent dir when pi is absent"
        );

        // Uninstall with no explicit dir also resolves via default_pi_agent_dir()
        // and is a no-op since nothing was ever installed there.
        uninstall_pi_extension(None).unwrap();

        // Once the agent dir exists, pi counts as present: install falls
        // through to actually write the extension, still via the env-resolved
        // default dir (no explicit `--pi-agent-dir`).
        std::fs::create_dir_all(&agent_dir).unwrap();
        install_pi_extension(None).unwrap();
        let extension = agent_dir.join("extensions").join(PI_EXTENSION_NAME);
        assert!(extension.exists());

        // Uninstall with no explicit dir removes what install just wrote.
        uninstall_pi_extension(None).unwrap();
        assert!(!extension.exists());

        restore_pi_env(snap);
    }

    #[test]
    fn agent_label_defaults_to_claude() {
        assert_eq!(agent_label(&json!({ "agent": "pi" })), "pi");
        assert_eq!(agent_label(&json!({ "agent": "codex" })), "codex");
        assert_eq!(agent_label(&json!({ "agent": "claude" })), "claude");
        assert_eq!(agent_label(&json!({})), "claude");
    }

    // --- command execute() paths -------------------------------------------

    #[test]
    fn install_then_uninstall_command_execute_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        // Install into a missing file: the hook block lands.
        InstallHooksCommand {
            settings: Some(path.clone()),
            pi_agent_dir: Some(tmp.path().join("pi-agent")),
            codex_home: Some(tmp.path().join("codex-home")),
        }
        .execute()
        .unwrap();
        assert!(read_settings(&path).unwrap()["hooks"]["Stop"].is_array());
        let extension = tmp
            .path()
            .join("pi-agent/extensions")
            .join(PI_EXTENSION_NAME);
        assert!(extension.exists());
        // A second install is the idempotent "no change" branch.
        InstallHooksCommand {
            settings: Some(path.clone()),
            pi_agent_dir: Some(tmp.path().join("pi-agent")),
            codex_home: Some(tmp.path().join("codex-home")),
        }
        .execute()
        .unwrap();
        // Uninstall removes our block, leaving an empty hooks object.
        UninstallHooksCommand {
            settings: Some(path.clone()),
            pi_agent_dir: Some(tmp.path().join("pi-agent")),
            codex_home: Some(tmp.path().join("codex-home")),
        }
        .execute()
        .unwrap();
        assert!(read_settings(&path).unwrap()["hooks"]
            .as_object()
            .unwrap()
            .is_empty());
        assert!(!extension.exists());
    }

    // --- install-wrapper / uninstall-wrapper --------------------------------

    /// An install/uninstall pair pointed entirely inside `tmp`, so nothing
    /// touches the developer's real data or config directories.
    fn wrapper_commands(tmp: &Path) -> (InstallWrapperCommand, UninstallWrapperCommand) {
        let settings = tmp.join("settings.json");
        let shim = tmp.join("bin").join("claude-wrap");
        (
            InstallWrapperCommand {
                settings: Some(settings.clone()),
                shim: Some(shim.clone()),
            },
            UninstallWrapperCommand {
                settings: Some(settings),
                shim: Some(shim),
            },
        )
    }

    #[test]
    fn install_wrapper_writes_an_executable_shim_and_sets_the_setting() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let (install, _) = wrapper_commands(tmp.path());
        let shim = install.shim.clone().unwrap();
        let settings = install.settings.clone().unwrap();
        install.execute().unwrap();

        // The shim is a one-line exec into `claude-wrap`, owner-executable, and
        // names an absolute binary so it does not depend on VS Code's PATH.
        let script = std::fs::read_to_string(&shim).unwrap();
        assert!(script.starts_with("#!/bin/sh\nexec \"/"), "{script}");
        assert!(script.contains(" claude-wrap -- \"$@\""), "{script}");
        let mode = std::fs::metadata(&shim).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);

        assert_eq!(
            read_settings(&settings).unwrap()[WRAPPER_SETTING_KEY],
            json!(shim.display().to_string())
        );
    }

    #[test]
    fn install_wrapper_is_idempotent_and_preserves_other_settings() {
        let tmp = tempfile::tempdir().unwrap();
        let (install, _) = wrapper_commands(tmp.path());
        let settings_path = install.settings.clone().unwrap();
        write_settings(&settings_path, &json!({ "editor.fontSize": 13 })).unwrap();

        install.execute().unwrap();
        let (again, _) = wrapper_commands(tmp.path());
        again.execute().unwrap();

        let settings = read_settings(&settings_path).unwrap();
        assert_eq!(settings["editor.fontSize"], json!(13));
        assert!(settings[WRAPPER_SETTING_KEY].is_string());
    }

    #[test]
    fn uninstall_wrapper_clears_only_our_own_setting() {
        let tmp = tempfile::tempdir().unwrap();
        let (install, uninstall) = wrapper_commands(tmp.path());
        let settings_path = install.settings.clone().unwrap();
        let shim = install.shim.clone().unwrap();
        install.execute().unwrap();
        uninstall.execute().unwrap();

        assert!(read_settings(&settings_path).unwrap()[WRAPPER_SETTING_KEY].is_null());
        assert!(!shim.exists());

        // A setting someone else owns survives, and so does their file.
        write_settings(
            &settings_path,
            &json!({ WRAPPER_SETTING_KEY: "/opt/someone-elses-wrapper" }),
        )
        .unwrap();
        let (_, uninstall) = wrapper_commands(tmp.path());
        uninstall.execute().unwrap();
        assert_eq!(
            read_settings(&settings_path).unwrap()[WRAPPER_SETTING_KEY],
            json!("/opt/someone-elses-wrapper")
        );
    }

    #[test]
    fn uninstall_wrapper_on_a_missing_settings_file_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, uninstall) = wrapper_commands(tmp.path());
        let settings = uninstall.settings.clone().unwrap();
        uninstall.execute().unwrap();
        assert!(!settings.exists());
    }

    #[test]
    fn install_wrapper_on_a_jsonc_settings_file_explains_the_manual_step() {
        let tmp = tempfile::tempdir().unwrap();
        let (install, _) = wrapper_commands(tmp.path());
        let settings = install.settings.clone().unwrap();
        let shim = install.shim.clone().unwrap();
        // Comments are legal in VS Code settings and cannot be rewritten safely.
        std::fs::write(
            &settings,
            "{\n  // a comment\n  \"editor.fontSize\": 13\n}\n",
        )
        .unwrap();

        let error = format!("{:#}", install.execute().unwrap_err());
        assert!(error.contains("not valid JSON"), "{error}");
        assert!(error.contains(WRAPPER_SETTING_KEY), "{error}");
        // The shim is written first, so the printed line is ready to paste.
        assert!(error.contains(&shim.display().to_string()), "{error}");
        assert!(shim.exists());
    }

    #[test]
    fn install_wrapper_fails_when_the_shim_directory_cannot_be_made() {
        let tmp = tempfile::tempdir().unwrap();
        // A regular file where the shim's parent directory would have to go, so
        // creating it cannot succeed — a mistyped `--shim` in practice.
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        let error = InstallWrapperCommand {
            settings: Some(tmp.path().join("settings.json")),
            shim: Some(blocker.join("claude-wrap")),
        }
        .execute()
        .unwrap_err();
        assert!(format!("{error:#}").contains("not-a-dir"), "{error:#}");
        // The settings file is left untouched when the shim cannot be written.
        assert!(!tmp.path().join("settings.json").exists());
    }

    #[test]
    fn set_and_clear_wrapper_tolerate_a_non_object_settings_value() {
        // `read_settings` guarantees an object, but these degrade rather than
        // panic if a caller hands them anything else (the `merge_hooks` rule).
        let mut scalar = json!("not an object");
        assert!(!set_wrapper(&mut scalar, "/shim"));
        assert!(!clear_wrapper(&mut scalar, "/shim"));
        assert_eq!(scalar, json!("not an object"));
    }

    #[test]
    fn the_wrapper_paths_default_outside_the_claude_config_dir() {
        // The shim lives beside the daemon socket; the setting lives in VS Code's
        // *config* directory, which is a different base on every platform.
        let shim = shim_path(None).unwrap();
        assert_eq!(shim.file_name().unwrap(), SHIM_NAME);
        assert_eq!(shim.parent().unwrap(), paths::runtime_dir().unwrap());
        let settings = vscode_settings_path(None).unwrap();
        assert!(
            settings.ends_with("Code/User/settings.json"),
            "{settings:?}"
        );
    }

    #[test]
    fn uninstall_command_on_a_missing_file_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.json");
        // The no-file branch: nothing to remove, still Ok, and no file created.
        UninstallHooksCommand {
            settings: Some(path.clone()),
            pi_agent_dir: Some(tmp.path().join("pi-agent")),
            codex_home: Some(tmp.path().join("codex-home")),
        }
        .execute()
        .unwrap();
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn sessions_command_dispatches_to_subcommands() {
        // Cover the outer dispatch for the two file-backed arms (no socket/stdin).
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        SessionsCommand {
            command: SessionsSubcommands::InstallHooks(InstallHooksCommand {
                settings: Some(path.clone()),
                pi_agent_dir: Some(tmp.path().join("pi-agent")),
                codex_home: Some(tmp.path().join("codex-home")),
            }),
        }
        .execute()
        .await
        .unwrap();
        SessionsCommand {
            command: SessionsSubcommands::UninstallHooks(UninstallHooksCommand {
                settings: Some(path.clone()),
                pi_agent_dir: Some(tmp.path().join("pi-agent")),
                codex_home: Some(tmp.path().join("codex-home")),
            }),
        }
        .execute()
        .await
        .unwrap();
        let (install, uninstall) = wrapper_commands(tmp.path());
        SessionsCommand {
            command: SessionsSubcommands::InstallWrapper(install),
        }
        .execute()
        .await
        .unwrap();
        SessionsCommand {
            command: SessionsSubcommands::UninstallWrapper(uninstall),
        }
        .execute()
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn hook_report_is_silent_when_the_daemon_is_down() {
        // A valid hook event but no daemon at the socket: the send fails and is
        // swallowed (never panics, never errors).
        let tmp = tempfile::tempdir_in("/tmp").unwrap();
        let sock = tmp.path().join("nope.sock");
        let cmd = HookCommand {
            socket: Some(sock),
            agent: HookAgent::Claude,
        };
        cmd.report(r#"{"session_id":"s1","hook_event_name":"Stop"}"#, None)
            .await;
        // Unmappable input returns before any socket work.
        cmd.report("not json", None).await;
        cmd.report(r#"{"hook_event_name":"Stop"}"#, None).await; // no session_id → no op
    }

    /// Spawns a minimal fake daemon on a short-path Unix socket that answers one
    /// request with `reply`. Returns the temp dir (kept alive), the socket path,
    /// and the server task.
    fn fake_daemon(reply: Value) -> (tempfile::TempDir, PathBuf, tokio::task::JoinHandle<()>) {
        use futures::{SinkExt, StreamExt};
        use tokio::net::UnixListener;
        use tokio_util::codec::{Framed, LinesCodec};

        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(stream, LinesCodec::new());
            let _req = framed.next().await.unwrap().unwrap();
            framed
                .send(serde_json::to_string(&reply).unwrap())
                .await
                .unwrap();
        });
        (dir, sock, server)
    }

    /// Like [`fake_daemon`] but **returns the request envelope it received** so a
    /// test can assert the exact op/payload wire shape the client sent.
    fn fake_daemon_capture(
        reply: Value,
    ) -> (tempfile::TempDir, PathBuf, tokio::task::JoinHandle<Value>) {
        use futures::{SinkExt, StreamExt};
        use tokio::net::UnixListener;
        use tokio_util::codec::{Framed, LinesCodec};

        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(stream, LinesCodec::new());
            let req = framed.next().await.unwrap().unwrap();
            framed
                .send(serde_json::to_string(&reply).unwrap())
                .await
                .unwrap();
            serde_json::from_str::<Value>(&req).unwrap()
        });
        (dir, sock, server)
    }

    #[tokio::test]
    async fn list_command_execute_renders_from_a_socket() {
        let payload = json!({
            "ok": true,
            "payload": { "sessions": [{
                "session_id": "s1", "state": "working",
                "source": { "kind": "terminal" }, "repo": "omni-dev",
                "cwd": "/home/me/omni-dev", "last_seen": "2000-01-01T00:00:00Z"
            }]}
        });
        // Table output, dispatched through the outer `SessionsCommand` so the
        // `List` arm of the dispatch is covered too.
        let (_dir, sock, server) = fake_daemon(payload.clone());
        SessionsCommand {
            command: SessionsSubcommands::List(ListCommand {
                socket: Some(sock),
                output: TableOrJson::Table,
            }),
        }
        .execute()
        .await
        .unwrap();
        server.await.unwrap();

        // JSON output goes through the other branch of the renderer.
        let (_dir, sock, server) = fake_daemon(payload);
        ListCommand {
            socket: Some(sock),
            output: TableOrJson::Json,
        }
        .execute()
        .await
        .unwrap();
        server.await.unwrap();
    }

    #[test]
    fn merge_and_remove_hooks_tolerate_malformed_shapes() {
        let cmd = "cmd sessions hook";
        // Non-object settings: both are no-ops rather than panics.
        assert_eq!(merge_hooks(&mut json!([]), cmd, HOOK_EVENTS), 0);
        assert_eq!(remove_hooks(&mut json!([]), cmd), 0);
        // `hooks` present but not an object → merge leaves it alone.
        assert_eq!(merge_hooks(&mut json!({ "hooks": 5 }), cmd, HOOK_EVENTS), 0);
        // No `hooks` key → remove has nothing to do.
        assert_eq!(remove_hooks(&mut json!({}), cmd), 0);
        // A per-event value that is not an array is skipped, not indexed.
        assert_eq!(
            merge_hooks(&mut json!({ "hooks": { "Stop": 5 } }), cmd, HOOK_EVENTS),
            HOOK_EVENTS.len() - 1
        );
        assert_eq!(remove_hooks(&mut json!({ "hooks": { "Stop": 5 } }), cmd), 0);
    }

    // --- #1361 typed window feed commands -----------------------------------

    #[test]
    fn window_subcommands_route_and_require_key() {
        assert!(matches!(
            parse(&["window", "--key", "w1"]),
            SessionsSubcommands::Window(_)
        ));
        assert!(matches!(
            parse(&["window-unregister", "--key", "w1"]),
            SessionsSubcommands::WindowUnregister(_)
        ));
        // `--key` is required for both.
        assert!(WindowCommand::try_parse_from(["window"]).is_err());
        assert!(WindowUnregisterCommand::try_parse_from(["window-unregister"]).is_err());
    }

    #[test]
    fn window_parses_counts_and_folders() {
        let cmd = WindowCommand::try_parse_from([
            "window",
            "--key",
            "w1",
            "--folder",
            "/a",
            "--folder",
            "/b",
            "--tabs",
            "2",
            "--terminals",
            "3",
        ])
        .unwrap();
        assert_eq!(cmd.key, "w1");
        assert_eq!(cmd.folders, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(cmd.tabs, 2);
        assert_eq!(cmd.terminals, 3);
        // Counts default to zero.
        let cmd = WindowCommand::try_parse_from(["window", "--key", "w1"]).unwrap();
        assert_eq!(cmd.tabs, 0);
        assert_eq!(cmd.terminals, 0);
    }

    #[tokio::test]
    async fn window_and_window_unregister_send_their_ops() {
        let (_dir, sock, server) =
            fake_daemon_capture(json!({ "ok": true, "payload": { "ok": true } }));
        // Routed through the outer `SessionsCommand::execute` so the `Window`
        // dispatch arm is covered too.
        SessionsCommand {
            command: SessionsSubcommands::Window(WindowCommand {
                key: "w1".to_string(),
                folders: vec![PathBuf::from("/a")],
                tabs: 1,
                terminals: 0,
                socket: Some(sock),
            }),
        }
        .execute()
        .await
        .unwrap();
        // The WindowReport wire shape: op + every field the daemon reads.
        let req = server.await.unwrap();
        assert_eq!(req["op"], "window");
        assert_eq!(req["payload"]["key"], json!("w1"));
        assert_eq!(req["payload"]["folders"], json!(["/a"]));
        assert_eq!(req["payload"]["tabs"], json!(1));
        assert_eq!(req["payload"]["terminals"], json!(0));

        // `window-unregister` replies `{removed}` (not `{ok}`); the client reads it.
        // Also routed through the wrapper to cover the `WindowUnregister` arm.
        let (_dir, sock, server) =
            fake_daemon_capture(json!({ "ok": true, "payload": { "removed": true } }));
        SessionsCommand {
            command: SessionsSubcommands::WindowUnregister(WindowUnregisterCommand {
                key: "w1".to_string(),
                socket: Some(sock),
            }),
        }
        .execute()
        .await
        .unwrap();
        let req = server.await.unwrap();
        assert_eq!(req["op"], "window-unregister");
        assert_eq!(req["payload"]["key"], json!("w1"));
    }
}
