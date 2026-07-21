//! `omni-dev daemon webhook` — manage the GitHub webhook that feeds the
//! non-polling PR-status buffer (#1384).
//!
//! These are operator helpers for the `webhook` PR source: install / list /
//! remove the webhook that delivers `check_run`/`check_suite`/`status`/
//! `pull_request` events to the buffer Worker (`deploy/webhook-buffer/`), plus a
//! `config` summary of what to set. They shell out to `gh` (reusing
//! [`crate::pr_status::resolve_gh_binary`]) and do **not** need the daemon running
//! — installing a webhook needs repo **admin**, so this is owned-repos-only; the
//! daemon's reconcile poll covers everything else.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

use crate::cli::format::TableOrJson;
use crate::daemon::server;

/// The webhook events the daemon's buffer source consumes.
const HOOK_EVENTS: &[&str] = &["check_run", "check_suite", "status", "pull_request"];

/// Webhook: install/list/remove the buffer webhook on a repo you own (#1384).
#[derive(Parser)]
pub struct WebhookCommand {
    /// The webhook subcommand to execute.
    #[command(subcommand)]
    pub command: WebhookSubcommands,
}

/// Webhook subcommands.
#[derive(Subcommand)]
pub enum WebhookSubcommands {
    /// Install the buffer webhook on a repo you own (needs repo admin).
    Register(RegisterArgs),
    /// List the webhooks installed on a repo.
    List(RepoArg),
    /// Remove a webhook from a repo by its numeric id (see `list`).
    Remove(RemoveArgs),
    /// Show, per repo, whether the daemon is receiving webhook events (queries the
    /// daemon only — no GitHub calls).
    Status(StatusArgs),
    /// Print the buffer-deployment and daemon-configuration summary.
    Config,
}

impl WebhookCommand {
    /// Executes the webhook command. The `gh` calls block briefly (a one-shot CLI,
    /// not a server); `register`/`remove` then notify the daemon over the socket.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            WebhookSubcommands::Register(args) => args.execute().await,
            WebhookSubcommands::Remove(args) => args.execute().await,
            WebhookSubcommands::List(args) => list_hooks(&args.repo),
            WebhookSubcommands::Status(args) => args.execute().await,
            WebhookSubcommands::Config => {
                print!("{}", config_summary());
                Ok(())
            }
        }
    }
}

/// `register` arguments: the repo plus the Worker URL and HMAC secret.
#[derive(Parser)]
pub struct RegisterArgs {
    /// The repository as `OWNER/REPO`.
    #[arg(value_name = "OWNER/REPO")]
    pub repo: String,
    /// The buffer Worker's webhook URL (e.g. `https://<name>.<you>.workers.dev/webhook`).
    #[arg(long, value_name = "URL")]
    pub url: String,
    /// The webhook HMAC secret (the Worker's `WEBHOOK_SECRET`). GitHub signs each
    /// delivery with it and the Worker verifies before storing.
    #[arg(long, value_name = "SECRET")]
    pub secret: String,
    /// Daemon control-socket path (to mark the repo webhook-backed). Defaults to
    /// the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

/// A bare `OWNER/REPO` positional argument.
#[derive(Parser)]
pub struct RepoArg {
    /// The repository as `OWNER/REPO`.
    #[arg(value_name = "OWNER/REPO")]
    pub repo: String,
}

/// `remove` arguments: the repo plus the numeric hook id.
#[derive(Parser)]
pub struct RemoveArgs {
    /// The repository as `OWNER/REPO`.
    #[arg(value_name = "OWNER/REPO")]
    pub repo: String,
    /// The numeric hook id to remove (from `daemon webhook list`).
    #[arg(long, value_name = "ID")]
    pub id: u64,
    /// Daemon control-socket path (to unmark the repo webhook-backed). Defaults to
    /// the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

/// `status` arguments: the socket override plus the output format.
#[derive(Parser)]
pub struct StatusArgs {
    /// Daemon control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = TableOrJson::Table)]
    pub output: TableOrJson,
}

impl StatusArgs {
    async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        let status =
            super::call_service(&socket, "worktrees", "webhook-status", Value::Null).await?;
        match self.output {
            TableOrJson::Json => println!("{}", serde_json::to_string_pretty(&status)?),
            TableOrJson::Table => print!("{}", render_status(&status)),
        }
        Ok(())
    }
}

impl RegisterArgs {
    async fn execute(self) -> Result<()> {
        let (owner, name) = split_repo_owned(&self.repo)?;
        let mut args: Vec<String> = vec![
            "api".into(),
            "-X".into(),
            "POST".into(),
            format!("repos/{owner}/{name}/hooks"),
            "-f".into(),
            "name=web".into(),
            "-F".into(),
            "active=true".into(),
        ];
        for event in HOOK_EVENTS {
            args.push("-f".into());
            args.push(format!("events[]={event}"));
        }
        args.push("-f".into());
        args.push(format!("config[url]={}", self.url));
        args.push("-f".into());
        args.push("config[content_type]=json".into());
        args.push("-f".into());
        args.push(format!("config[secret]={}", self.secret));

        let out = run_gh(&args)?;
        let id = serde_json::from_str::<Value>(&out)
            .ok()
            .and_then(|v| v.get("id").and_then(Value::as_u64));
        match id {
            Some(id) => println!("registered webhook {id} on {owner}/{name} → {}", self.url),
            None => println!("registered webhook on {owner}/{name} → {}", self.url),
        }
        println!("events: {}", HOOK_EVENTS.join(", "));
        // Mark the repo webhook-backed on the daemon so `webhook` mode's reconcile
        // skips its expensive rollup query (#1384). Best-effort.
        notify_webhook_backed(self.socket, &owner, &name, true).await;
        Ok(())
    }
}

impl RemoveArgs {
    async fn execute(self) -> Result<()> {
        let (owner, name) = split_repo_owned(&self.repo)?;
        run_gh(&[
            "api".to_string(),
            "-X".to_string(),
            "DELETE".to_string(),
            format!("repos/{owner}/{name}/hooks/{}", self.id),
        ])?;
        println!("removed webhook {} from {owner}/{name}", self.id);
        notify_webhook_backed(self.socket, &owner, &name, false).await;
        Ok(())
    }
}

/// Best-effort: tell the daemon a repo's webhook-backed state changed (#1384) via
/// the `set-webhook-backed` op, so `webhook` mode's reconcile routes it to the
/// cheap or full query. A down/unreachable daemon prints a note, not an error —
/// the hook itself was already (un)installed; re-run with the daemon up to sync it.
async fn notify_webhook_backed(socket: Option<PathBuf>, owner: &str, name: &str, backed: bool) {
    let socket = match server::resolve_socket(socket) {
        Ok(socket) => socket,
        Err(err) => {
            eprintln!("note: could not resolve the daemon socket: {err:#}");
            return;
        }
    };
    let payload = json!({ "owner": owner, "name": name, "backed": backed });
    if let Err(err) = super::call_service(&socket, "worktrees", "set-webhook-backed", payload).await
    {
        eprintln!(
            "note: could not mark {owner}/{name} webhook-backed on the daemon ({err:#}); \
             run this again with the daemon running to sync it."
        );
    }
}

/// Lists a repo's webhooks, printing a compact `id  active  url  events` line each.
fn list_hooks(repo: &str) -> Result<()> {
    let (owner, name) = split_repo(repo)?;
    let out = run_gh(&["api".to_string(), format!("repos/{owner}/{name}/hooks")])?;
    let hooks: Value = serde_json::from_str(&out).context("parsing `gh api .../hooks` output")?;
    let Some(hooks) = hooks.as_array() else {
        bail!("unexpected hooks response (not an array)");
    };
    if hooks.is_empty() {
        println!("no webhooks installed on {owner}/{name}");
        return Ok(());
    }
    for hook in hooks {
        let id = hook.get("id").and_then(Value::as_u64).unwrap_or(0);
        let active = hook.get("active").and_then(Value::as_bool).unwrap_or(false);
        let url = hook
            .get("config")
            .and_then(|c| c.get("url"))
            .and_then(Value::as_str)
            .unwrap_or("(no url)");
        let events = hook
            .get("events")
            .and_then(Value::as_array)
            .map(|evs| {
                evs.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let state = if active { "active" } else { "inactive" };
        println!("{id}\t{state}\t{url}\t[{events}]");
    }
    Ok(())
}

/// Renders the `webhook-status` payload as a table.
fn render_status(status: &Value) -> String {
    let mode = status
        .get("pr_source")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let configured = status
        .get("configured")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let now = now_ms();

    // The honest connectivity signal: did the last pull succeed? A successful pull
    // with zero events still counts, so a quiet CI period reads as connected.
    let buffer = if !configured {
        "NOT configured — reconcile-only".to_string()
    } else if let Some(err) = status.get("last_pull_error").and_then(Value::as_str) {
        format!("configured, last pull FAILED: {err}")
    } else if let Some(ms) = status.get("last_pull_ms").and_then(Value::as_u64) {
        format!("connected (last pull {})", humanize_since(now, ms))
    } else {
        "configured, no successful pull yet".to_string()
    };

    let mut out = String::new();
    out.push_str(&format!("mode: {mode}    buffer: {buffer}\n"));
    if mode != "webhook" {
        out.push_str("(the webhook source is inactive: prStatusSource is not \"webhook\")\n");
    }

    let repos = status
        .get("repos")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if repos.is_empty() {
        out.push_str("\nno webhook-relevant repos yet\n");
        return out;
    }
    out.push_str(&format!(
        "\n{:<40} {:<7} {:<11} {}\n",
        "REPO", "BACKED", "DELIVERING", "LAST EVENT"
    ));
    for repo in &repos {
        let owner = repo.get("owner").and_then(Value::as_str).unwrap_or("");
        let name = repo.get("name").and_then(Value::as_str).unwrap_or("");
        let last = repo.get("last_event_ms").and_then(Value::as_u64);
        out.push_str(&format!(
            "{:<40} {:<7} {:<11} {}\n",
            format!("{owner}/{name}"),
            if repo.get("backed").and_then(Value::as_bool) == Some(true) {
                "yes"
            } else {
                "no"
            },
            if last.is_some() { "yes" } else { "no" },
            last.map_or_else(|| "—".to_string(), |ms| humanize_since(now, ms)),
        ));
    }
    out.push_str(
        "\nnote: the `buffer:` line above is the connection signal (last successful pull).\n\
         \"DELIVERING\" is per-repo and only counts events seen since this daemon started,\n\
         so a repo reads \"no\" during quiet CI even when connected. `daemon webhook list`\n\
         hits GitHub to confirm a hook actually exists.\n",
    );
    out
}

/// Current time in epoch-ms (0 if the clock is before the epoch).
fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// A compact "Ns/Nm/Nh/Nd ago" for an epoch-ms instant relative to `now`.
fn humanize_since(now: u64, then: u64) -> String {
    let secs = now.saturating_sub(then) / 1000;
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}

/// Runs `gh` with `args`, returning stdout on success or an error carrying
/// `gh`'s stderr. Reuses the daemon's `gh` resolution so it works under a minimal
/// launchd/systemd `PATH`.
fn run_gh<S: AsRef<OsStr>>(args: &[S]) -> Result<String> {
    let bin = crate::pr_status::resolve_gh_binary();
    let output = Command::new(&bin)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {}", bin.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh command failed: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Splits `OWNER/REPO`, rejecting empty parts or extra slashes.
fn split_repo(spec: &str) -> Result<(&str, &str)> {
    let (owner, name) = spec
        .split_once('/')
        .with_context(|| format!("expected OWNER/REPO, got '{spec}'"))?;
    if owner.is_empty() || name.is_empty() || name.contains('/') {
        bail!("invalid OWNER/REPO '{spec}'");
    }
    Ok((owner, name))
}

/// [`split_repo`] returning owned strings, for the async paths that keep the parts
/// past the borrow of `self`.
fn split_repo_owned(spec: &str) -> Result<(String, String)> {
    let (owner, name) = split_repo(spec)?;
    Ok((owner.to_string(), name.to_string()))
}

/// The operator summary printed by `daemon webhook config`.
fn config_summary() -> String {
    "\
Webhook (non-polling) PR-status source — setup summary (#1384)

1. Deploy the buffer Worker (deploy/webhook-buffer/README.md):
     cd deploy/webhook-buffer && npm install && npx wrangler login
     npx wrangler kv namespace create GITHUB_EVENTS   # paste id into wrangler.toml
     openssl rand -hex 32 | npx wrangler secret put WEBHOOK_SECRET
     openssl rand -hex 32 | npx wrangler secret put READ_TOKEN
     npx wrangler deploy                              # prints your Worker URL

2. Install the webhook on each repo you own:
     omni-dev daemon webhook register <owner>/<repo> \\
       --url https://<name>.<you>.workers.dev/webhook --secret <WEBHOOK_SECRET>

3. Point the daemon at the buffer, then set the VS Code setting
   omniDevWorktrees.prStatusSource to \"webhook\":
     OMNI_DEV_WEBHOOK_BUFFER_URL   the Worker base URL
     OMNI_DEV_WEBHOOK_READ_TOKEN   the READ_TOKEN (or *_PATH to a file)
     OMNI_DEV_DAEMON_WEBHOOK_PULL       buffer-pull seconds (default 10)
     OMNI_DEV_DAEMON_WEBHOOK_RECONCILE  reconcile seconds  (default 900)

The HMAC secret stays only in the Worker; the daemon holds only the read token,
persisted 0600 at <data-dir>/omni-dev/webhook.token, and pulls outbound-only.
"
    .to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn render_status_reports_the_buffer_connection_state() {
        // Not configured → reconcile-only.
        let s = render_status(&json!({ "pr_source": "webhook", "configured": false, "repos": [] }));
        assert!(s.contains("NOT configured"), "{s}");

        // Configured + a recent successful pull → connected, even with no events.
        let s = render_status(&json!({
            "pr_source": "webhook", "configured": true, "last_pull_ms": now_ms(), "repos": []
        }));
        assert!(s.contains("connected (last pull"), "{s}");

        // Configured but the most recent pull failed.
        let s = render_status(&json!({
            "pr_source": "webhook", "configured": true,
            "last_pull_error": "401 unauthorized", "repos": []
        }));
        assert!(s.contains("last pull FAILED: 401 unauthorized"), "{s}");

        // Configured, no pull completed yet.
        let s = render_status(&json!({ "pr_source": "webhook", "configured": true, "repos": [] }));
        assert!(s.contains("no successful pull yet"), "{s}");
    }

    #[test]
    fn render_status_lists_repos_with_backed_and_delivering_columns() {
        let s = render_status(&json!({
            "pr_source": "webhook", "configured": true, "last_pull_ms": now_ms(),
            "repos": [
                { "owner": "rust-works", "name": "succinctly", "backed": true, "last_event_ms": now_ms() },
                { "owner": "rust-works", "name": "omni-dev", "backed": false, "last_event_ms": null },
            ]
        }));
        assert!(s.contains("rust-works/succinctly"), "{s}");
        assert!(s.contains("rust-works/omni-dev"), "{s}");
    }
}
