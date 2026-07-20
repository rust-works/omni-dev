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
