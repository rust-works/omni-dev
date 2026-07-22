//! `omni-dev sessions` — track the Claude Code sessions running across every
//! terminal and VS Code window, via the daemon's `sessions` service.
//!
//! Four subcommands, split by role:
//! - `list` is a **read** client (like `omni-dev worktrees list`): it asks the
//!   daemon's `sessions` service for the live set and renders it.
//! - `hook` is the **feed sink**: Claude Code runs it per hook event; it reads
//!   the hook JSON on stdin, maps it to an `observe`/`end` op, and fire-and-forgets
//!   it to the daemon socket. It must **never** block or fail a Claude turn — a
//!   missing daemon, a bad payload, or any other error is swallowed and it always
//!   exits 0.
//! - `install-hooks` / `uninstall-hooks` idempotently merge (or remove) the hook
//!   block in `~/.claude/settings.json`, preserving any hooks already there.
//!
//! The register/heartbeat feed from the companion VS Code extension talks to the
//! socket directly (like the worktrees companion), not through this CLI.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::cli::format::TableOrJson;
use crate::daemon::client::DaemonClient;
use crate::daemon::protocol::{DaemonEnvelope, DaemonReply};
use crate::daemon::server;
use crate::sessions::{NotificationKind, ObserveRequest, SessionEvent};

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
    /// List the Claude Code sessions currently running across all windows.
    List(ListCommand),
    /// Claude Code hook sink: read a hook event on stdin and report it to the
    /// daemon (run by Claude Code, not by hand).
    Hook(HookCommand),
    /// Install the Claude Code hooks that feed the sessions tracker into
    /// `~/.claude/settings.json` (idempotent).
    InstallHooks(InstallHooksCommand),
    /// Remove the sessions-tracker hooks from `~/.claude/settings.json`.
    UninstallHooks(UninstallHooksCommand),
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

/// The Claude Code hook sink: reads one hook event's JSON on stdin and reports it
/// to the daemon. Fire-and-forget and infallible-by-design.
#[derive(Parser)]
pub struct HookCommand {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl HookCommand {
    /// Executes the hook sink. Always returns `Ok(())` (exit 0): a hook must
    /// never block or fail a Claude turn, so every error — no daemon, bad JSON,
    /// an unknown event — is swallowed after a best-effort report.
    pub async fn execute(self) -> Result<()> {
        let mut input = String::new();
        if std::io::stdin().read_to_string(&mut input).is_err() {
            return Ok(());
        }
        self.report(&input).await;
        Ok(())
    }

    /// Parses the hook JSON, maps it to an op, and best-effort sends it. Split
    /// out so tests can exercise the send path against a fake socket.
    async fn report(&self, input: &str) {
        let Ok(hook) = serde_json::from_str::<HookPayload>(input) else {
            return;
        };
        let Some((op, payload)) = hook.to_op() else {
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
    /// [`NotificationKind`].
    #[serde(default)]
    message: Option<String>,
    /// Best-effort model id, when a payload carries one.
    #[serde(default)]
    model: Option<String>,
}

impl HookPayload {
    /// Maps this hook payload to a `(op, payload)` for the daemon, or `None` when
    /// it carries no `session_id` or names an event the tracker ignores.
    fn to_op(&self) -> Option<(&'static str, Value)> {
        let session_id = self.session_id.clone().filter(|s| !s.trim().is_empty())?;
        let event_name = self.hook_event_name.as_deref()?;
        if event_name == "SessionEnd" {
            let mut payload = json!({ "session_id": session_id });
            if let Some(reason) = &self.message {
                payload["reason"] = Value::String(reason.clone());
            }
            return Some(("end", payload));
        }
        let event = session_event_for(event_name, self.message.as_deref())?;
        let request = ObserveRequest {
            session_id,
            cwd: self.cwd.clone(),
            transcript_path: self.transcript_path.clone(),
            event,
            repo: None,
            model: self.model.clone(),
        };
        Some(("observe", serde_json::to_value(request).ok()?))
    }
}

/// Maps a Claude Code hook event name to the [`SessionEvent`] it implies, or
/// `None` for an event the tracker does not act on. `SessionEnd` is handled
/// separately (it maps to the `end` op, not `observe`).
fn session_event_for(event_name: &str, message: Option<&str>) -> Option<SessionEvent> {
    Some(match event_name {
        "SessionStart" => SessionEvent::SessionStart,
        "UserPromptSubmit" => SessionEvent::UserPromptSubmit,
        "PreToolUse" => SessionEvent::PreToolUse,
        "PostToolUse" => SessionEvent::PostToolUse,
        "Stop" => SessionEvent::Stop,
        "Notification" => SessionEvent::Notification(classify_notification(message)),
        _ => return None,
    })
}

/// Classifies a `Notification` message into a [`NotificationKind`]. Best-effort
/// substring matching — the message text is version-unstable, so an unrecognised
/// message falls back to [`NotificationKind::Other`] (which carries no state
/// signal and leaves the session's state unchanged).
fn classify_notification(message: Option<&str>) -> NotificationKind {
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

/// Installs the sessions-tracker hooks into `~/.claude/settings.json`.
#[derive(Parser)]
pub struct InstallHooksCommand {
    /// Path to the Claude settings file. Defaults to `~/.claude/settings.json`
    /// (respecting `$CLAUDE_CONFIG_DIR`).
    #[arg(long, value_name = "PATH")]
    pub settings: Option<PathBuf>,
}

impl InstallHooksCommand {
    /// Executes the install: merges the hook block idempotently, preserving any
    /// hooks already present.
    pub fn execute(self) -> Result<()> {
        let path = settings_path(self.settings)?;
        let mut settings = read_settings(&path)?;
        let command = hook_command();
        let added = merge_hooks(&mut settings, &command);
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
        Ok(())
    }
}

/// Removes the sessions-tracker hooks from `~/.claude/settings.json`.
#[derive(Parser)]
pub struct UninstallHooksCommand {
    /// Path to the Claude settings file. Defaults to `~/.claude/settings.json`
    /// (respecting `$CLAUDE_CONFIG_DIR`).
    #[arg(long, value_name = "PATH")]
    pub settings: Option<PathBuf>,
}

impl UninstallHooksCommand {
    /// Executes the uninstall: removes any hook entries whose command is ours,
    /// leaving every other hook untouched.
    pub fn execute(self) -> Result<()> {
        let path = settings_path(self.settings)?;
        if !path.exists() {
            println!("no settings file at {} (nothing to remove)", path.display());
            return Ok(());
        }
        let mut settings = read_settings(&path)?;
        let removed = remove_hooks(&mut settings, &hook_command());
        write_settings(&path, &settings)?;
        println!(
            "removed {removed} sessions hook entry(ies) from {}",
            path.display()
        );
        Ok(())
    }
}

/// The Claude Code hook events the tracker installs, paired with whether the
/// event's hook group needs a tool `matcher` (`PreToolUse`/`PostToolUse` match on
/// tool name; the rest have no matcher). `SessionEnd` is included — it maps to
/// the `end` op in the sink.
const HOOK_EVENTS: &[(&str, bool)] = &[
    ("SessionStart", false),
    ("UserPromptSubmit", false),
    ("PreToolUse", true),
    ("PostToolUse", true),
    ("Notification", false),
    ("Stop", false),
    ("SessionEnd", false),
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
/// event in [`HOOK_EVENTS`], returning how many events were newly added.
/// Idempotent (an event that already has a group running `command` is skipped)
/// and additive (it never touches other hooks). Creates `hooks` and any per-event
/// array as needed.
fn merge_hooks(settings: &mut Value, command: &str) -> usize {
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
    for (event, needs_matcher) in HOOK_EVENTS {
        let groups = hooks
            .entry((*event).to_string())
            .or_insert_with(|| json!([]));
        let Some(groups) = groups.as_array_mut() else {
            continue;
        };
        if groups.iter().any(|g| group_has_command(g, command)) {
            continue; // already installed for this event
        }
        groups.push(hook_group(command, *needs_matcher));
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
                .map_or(true, |h| !h.is_empty())
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
/// on tool name.
fn hook_group(command: &str, needs_matcher: bool) -> Value {
    let mut group = json!({
        "hooks": [{ "type": "command", "command": command }],
    });
    if needs_matcher {
        group["matcher"] = Value::String("*".to_string());
    }
    group
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
        return "No active Claude Code sessions.".to_string();
    }
    // CWD is last so a long path never misaligns the columns after it.
    let mut out = format!(
        "{:<13} {:<8} {:<20} {:>5}  {}",
        "STATE", "SOURCE", "REPO", "AGE", "CWD"
    );
    for session in sessions {
        let state = state_display(session.get("state").and_then(Value::as_str).unwrap_or("-"));
        let source = source_label(session);
        let repo = sanitize(session.get("repo").and_then(Value::as_str).unwrap_or("-"));
        let cwd = sanitize(session.get("cwd").and_then(Value::as_str).unwrap_or("-"));
        let age = age_secs(session.get("last_seen").and_then(Value::as_str));
        out.push_str(&format!(
            "\n{state:<13} {source:<8} {repo:<20} {age:>4}s  {cwd}"
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

/// The short source label for a session: `vscode` when embedded in a VS Code
/// window, else `terminal`.
fn source_label(session: &Value) -> &'static str {
    match session.pointer("/source/kind").and_then(Value::as_str) {
        Some("vs_code") => "vscode",
        _ => "terminal",
    }
}

/// Strips control characters from an untrusted registry string so a crafted
/// payload cannot inject terminal escape sequences into the rendered table (the
/// worktrees `sanitize` precedent, #1137). The `--json` path stays verbatim.
fn sanitize(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
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
            .to_op()
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
        assert!(hook_op(r#"{"session_id":"s1","hook_event_name":"PreCompact"}"#).is_none());
        // Garbage that still parses as an (empty) payload → no op.
        assert!(hook_op("{}").is_none());
    }

    #[test]
    fn classify_notification_covers_cases() {
        assert_eq!(
            classify_notification(Some("Please approve this")),
            NotificationKind::PermissionPrompt
        );
        assert_eq!(
            classify_notification(Some("Claude is idle")),
            NotificationKind::IdlePrompt
        );
        assert_eq!(classify_notification(None), NotificationKind::Other);
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
        let added = merge_hooks(&mut settings, cmd);
        assert_eq!(added, HOOK_EVENTS.len());

        // Our command landed under every event, and the unrelated hook stands.
        for (event, _) in HOOK_EVENTS {
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
        assert_eq!(merge_hooks(&mut settings, cmd), 0);
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
        merge_hooks(&mut settings, cmd);
        let removed = remove_hooks(&mut settings, cmd);
        assert_eq!(removed, HOOK_EVENTS.len());

        // Every one of our entries is gone...
        for (event, _) in HOOK_EVENTS {
            let empty = settings["hooks"]
                .get(event)
                .and_then(Value::as_array)
                .map_or(true, |g| g.iter().all(|g| !group_has_command(g, cmd)));
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
        merge_hooks(&mut settings, cmd);
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
        merge_hooks(&mut settings, "cmd sessions hook");
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
            "No active Claude Code sessions."
        );
        assert_eq!(
            render_sessions(&json!({})),
            "No active Claude Code sessions."
        );
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

    // --- command execute() paths -------------------------------------------

    #[test]
    fn install_then_uninstall_command_execute_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        // Install into a missing file: the hook block lands.
        InstallHooksCommand {
            settings: Some(path.clone()),
        }
        .execute()
        .unwrap();
        assert!(read_settings(&path).unwrap()["hooks"]["Stop"].is_array());
        // A second install is the idempotent "no change" branch.
        InstallHooksCommand {
            settings: Some(path.clone()),
        }
        .execute()
        .unwrap();
        // Uninstall removes our block, leaving an empty hooks object.
        UninstallHooksCommand {
            settings: Some(path.clone()),
        }
        .execute()
        .unwrap();
        assert!(read_settings(&path).unwrap()["hooks"]
            .as_object()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn uninstall_command_on_a_missing_file_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("does-not-exist.json");
        // The no-file branch: nothing to remove, still Ok, and no file created.
        UninstallHooksCommand {
            settings: Some(path.clone()),
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
            }),
        }
        .execute()
        .await
        .unwrap();
        SessionsCommand {
            command: SessionsSubcommands::UninstallHooks(UninstallHooksCommand {
                settings: Some(path.clone()),
            }),
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
        let cmd = HookCommand { socket: Some(sock) };
        cmd.report(r#"{"session_id":"s1","hook_event_name":"Stop"}"#)
            .await;
        // Unmappable input returns before any socket work.
        cmd.report("not json").await;
        cmd.report(r#"{"hook_event_name":"Stop"}"#).await; // no session_id → no op
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
        assert_eq!(merge_hooks(&mut json!([]), cmd), 0);
        assert_eq!(remove_hooks(&mut json!([]), cmd), 0);
        // `hooks` present but not an object → merge leaves it alone.
        assert_eq!(merge_hooks(&mut json!({ "hooks": 5 }), cmd), 0);
        // No `hooks` key → remove has nothing to do.
        assert_eq!(remove_hooks(&mut json!({}), cmd), 0);
        // A per-event value that is not an array is skipped, not indexed.
        assert_eq!(merge_hooks(&mut json!({ "hooks": { "Stop": 5 } }), cmd), 6);
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
        let (_dir, sock, server) = fake_daemon(json!({ "ok": true, "payload": { "ok": true } }));
        WindowCommand {
            key: "w1".to_string(),
            folders: vec![PathBuf::from("/a")],
            tabs: 1,
            terminals: 0,
            socket: Some(sock),
        }
        .execute()
        .await
        .unwrap();
        server.await.unwrap();

        // `window-unregister` replies `{removed}` (not `{ok}`); the client reads it.
        let (_dir, sock, server) =
            fake_daemon(json!({ "ok": true, "payload": { "removed": true } }));
        WindowUnregisterCommand {
            key: "w1".to_string(),
            socket: Some(sock),
        }
        .execute()
        .await
        .unwrap();
        server.await.unwrap();
    }
}
