//! `omni-dev worktrees recent` / `reopen` — the CLI face of the daemon's
//! recently-closed list (#2211, [ADR-0096]).
//!
//! The same two ops the VS Code extension drives (`recent-closed`, `reopen`),
//! with the same safety shape: a closed *window* just opens, while a **removed**
//! worktree is recreated only after the plan has been shown and confirmed. All
//! git logic stays in the daemon; the CLI adds no authority of its own.
//!
//! [ADR-0096]: https://github.com/rust-works/omni-dev/blob/main/docs/adrs/adr-0096.md

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use clap::Parser;
use serde_json::{json, Value};

use super::{age_secs, answer_is_yes, call, read_stdin_line, sanitize};
use crate::cli::format::TableOrJson;
use crate::daemon::server;

/// Lists the worktrees whose windows were recently closed, newest first,
/// including worktrees that have since been removed from disk.
#[derive(Parser)]
pub struct RecentCommand {
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = TableOrJson::Table)]
    pub output: TableOrJson,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl RecentCommand {
    /// Executes the recent command.
    pub async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        let reply = call(&socket, "recent-closed", Value::Null).await?;
        match self.output {
            TableOrJson::Json => println!("{}", serde_json::to_string_pretty(&reply)?),
            TableOrJson::Table => println!("{}", render_recent(&reply)),
        }
        Ok(())
    }
}

/// Reopens a recently closed worktree: opens its window, or — if it was removed
/// from disk — recreates it first.
///
/// Recreating is the only part that creates anything, so it is two-phase like
/// `close` and `rebase`: the daemon's plan (what it will check out, and that
/// uncommitted changes cannot be recovered) is printed, then confirmed. `--dry-run`
/// stops after the plan; `-y` skips the prompt. The path must be one `worktrees
/// recent` lists — the daemon accepts nothing else.
#[derive(Parser)]
pub struct ReopenCommand {
    /// The worktree folder, as `worktrees recent` shows it.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
    /// Print what reopening would do, but open and create nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip the interactive confirmation before recreating a removed worktree.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl ReopenCommand {
    /// Executes the reopen command, confirming a recreate interactively via stdin.
    pub async fn execute(self) -> Result<()> {
        self.execute_with(|| async { read_stdin_line().await })
            .await
    }

    /// The reopen core, with the answer to the recreate prompt injected, so the
    /// abort and confirmed branches are testable without real stdin.
    async fn execute_with<F, Fut>(self, read_answer: F) -> Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Option<String>>,
    {
        let socket = server::resolve_socket(self.socket)?;
        let listed = call(&socket, "recent-closed", Value::Null).await?;
        let Some(entry) = find_entry(&listed, &self.path) else {
            bail!(
                "{} is not in the recently closed list (see `omni-dev worktrees recent`)",
                self.path.display()
            );
        };
        // Use the daemon's spelling of the path from here on: it is the key the
        // `reopen` op matches, and a removed worktree has no canonical form.
        let path = entry["path"].as_str().unwrap_or_default().to_string();
        let removed = entry["removed"].as_bool().unwrap_or(false);

        if !removed {
            if self.dry_run {
                println!("Would open {} (dry run; nothing opened)", sanitize(&path));
                return Ok(());
            }
            let reply = call(&socket, "reopen", json!({ "path": path })).await?;
            ensure_reopened(&reply, &path)?;
            println!("Reopened {}", sanitize(&path));
            return Ok(());
        }

        // A removed worktree: the first request is side-effect free and returns
        // the plan (or, if it is back on disk after all, just opens it).
        let reply = call(&socket, "reopen", json!({ "path": path })).await?;
        let Some(plan) = reply.get("plan") else {
            ensure_reopened(&reply, &path)?;
            println!("Reopened {}", sanitize(&path));
            return Ok(());
        };
        println!("{}", render_plan(&path, entry, plan));
        if plan["restorable"].as_bool() != Some(true) {
            bail!(
                "{} cannot be restored: {}",
                sanitize(&path),
                sanitize(plan["reason"].as_str().unwrap_or("unknown reason"))
            );
        }
        if self.dry_run {
            return Ok(());
        }
        if !self.yes && !confirm_recreate(&path, read_answer).await {
            println!("Aborted; nothing was created.");
            return Ok(());
        }
        let done = call(
            &socket,
            "reopen",
            json!({ "path": path, "confirmed": true }),
        )
        .await?;
        ensure_reopened(&done, &path)?;
        println!("Recreated and reopened {}", sanitize(&path));
        if done["opened"].as_bool() == Some(false) {
            println!(
                "The worktree is back on disk, but its window could not be opened: {}",
                sanitize(done["open_error"].as_str().unwrap_or("unknown error"))
            );
        }
        Ok(())
    }
}

/// Fails unless the daemon's reply says it reopened `path`. The daemon re-plans on
/// every call, so a worktree can stop being restorable between the plan the user
/// saw and the confirmed call (its branch checked out elsewhere, say); that comes
/// back as an `ok` reply carrying a plan, and must not be reported as success.
fn ensure_reopened(reply: &Value, path: &str) -> Result<()> {
    if reply["reopened"].as_bool() == Some(true) {
        return Ok(());
    }
    let why = reply
        .get("plan")
        .and_then(|plan| plan["reason"].as_str())
        .unwrap_or("the daemon did not reopen it");
    bail!("{} was not reopened: {}", sanitize(path), sanitize(why))
}

/// Prints the recreate prompt on stderr and reads the answer; anything but an
/// affirmative — including a closed stdin — declines, so a create never proceeds
/// unattended.
async fn confirm_recreate<F, Fut>(path: &str, read_answer: F) -> bool
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Option<String>>,
{
    use std::io::Write;
    eprint!("Recreate the worktree at {}? [y/N] ", sanitize(path));
    let _ = std::io::stderr().flush();
    read_answer().await.as_deref().is_some_and(answer_is_yes)
}

/// The listed entry for `path`, matched as given or by its canonical form (a
/// closed worktree that still exists may be named through a symlink).
fn find_entry<'a>(listed: &'a Value, path: &Path) -> Option<&'a Value> {
    let canonical = std::fs::canonicalize(path).ok();
    listed
        .get("closed")
        .and_then(Value::as_array)?
        .iter()
        .find(|entry| {
            entry["path"].as_str().is_some_and(|p| {
                Path::new(p) == path || canonical.as_deref().is_some_and(|c| Path::new(p) == c)
            })
        })
}

/// A short human age: `45s`, `12m`, `3h`, `5d`.
fn human_age(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// Renders a `recent-closed` reply as a table, newest first, or a placeholder
/// line when there is nothing to reopen. Every daemon string is sanitized: a
/// path or branch name is untrusted terminal input.
fn render_recent(reply: &Value) -> String {
    let entries = reply
        .get("closed")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if entries.is_empty() {
        return "No recently closed worktrees.".to_string();
    }
    let mut out = format!(
        "{:<5} {:<8} {:<22} {:<28} PATH",
        "AGE", "STATE", "REPO", "BRANCH"
    );
    for entry in entries {
        let field = |name: &str| sanitize(entry.get(name).and_then(Value::as_str).unwrap_or("-"));
        let state = if entry["removed"].as_bool() == Some(true) {
            "removed"
        } else {
            "closed"
        };
        out.push_str(&format!(
            "\n{:<5} {:<8} {:<22} {:<28} {}",
            human_age(age_secs(entry.get("closed_at").and_then(Value::as_str))),
            state,
            field("main_repo"),
            field("branch"),
            field("path"),
        ));
    }
    out
}

/// Renders the daemon's recreate plan for a removed worktree.
fn render_plan(path: &str, entry: &Value, plan: &Value) -> String {
    let branch = sanitize(entry["branch"].as_str().unwrap_or("-"));
    let mut out = format!("{} was removed from disk.", sanitize(path));
    if plan["restorable"].as_bool() == Some(true) {
        let from = match plan["source"].as_str() {
            Some("head-sha") => "the commit it was on (the branch no longer exists)",
            _ => "its branch",
        };
        out.push_str(&format!("\nIt can be recreated on `{branch}` from {from}."));
        for warning in plan["warnings"].as_array().into_iter().flatten() {
            if let Some(text) = warning.as_str() {
                out.push_str(&format!("\n  ! {}", sanitize(text)));
            }
        }
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A fake daemon answering one request per connection, in order, and
    /// returning every request it saw.
    fn fake_daemon(
        replies: Vec<Value>,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        tokio::task::JoinHandle<Vec<Value>>,
    ) {
        use futures::{SinkExt, StreamExt};
        use tokio::net::UnixListener;
        use tokio_util::codec::{Framed, LinesCodec};

        // A short base path keeps the socket under the 104-byte `sockaddr_un` limit.
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for reply in replies {
                let (stream, _) = listener.accept().await.unwrap();
                let mut framed = Framed::new(stream, LinesCodec::new());
                let req = framed.next().await.unwrap().unwrap();
                seen.push(serde_json::from_str::<Value>(&req).unwrap());
                framed
                    .send(serde_json::to_string(&reply).unwrap())
                    .await
                    .unwrap();
            }
            seen
        });
        (dir, sock, server)
    }

    fn entry(path: &str, removed: bool) -> Value {
        json!({
            "path": path, "repo_root": "/r", "main_repo": "repo", "branch": "feature",
            "is_main": false, "removed": removed, "closed_at": "2026-10-07T11:00:00Z",
        })
    }

    fn listing(entries: Vec<Value>) -> Value {
        json!({ "ok": true, "payload": { "closed": entries } })
    }

    fn plan(restorable: bool) -> Value {
        if restorable {
            json!({ "ok": true, "payload": { "reopened": false, "plan": {
                "restorable": true, "source": "branch", "branch": "feature",
                "warnings": ["Uncommitted changes in the removed worktree cannot be recovered."],
            }}})
        } else {
            json!({ "ok": true, "payload": { "reopened": false, "plan": {
                "restorable": false, "reason": "the repository at /r is gone", "warnings": [],
            }}})
        }
    }

    fn reopen_cmd(socket: PathBuf, dry_run: bool, yes: bool) -> ReopenCommand {
        ReopenCommand {
            path: PathBuf::from("/wt/a"),
            dry_run,
            yes,
            socket: Some(socket),
        }
    }

    fn ops(seen: &[Value]) -> Vec<&str> {
        seen.iter().map(|r| r["op"].as_str().unwrap()).collect()
    }

    #[test]
    fn recent_and_reopen_parse() {
        let cmd = ReopenCommand::try_parse_from(["reopen", "/wt/a", "--dry-run", "-y"]).unwrap();
        assert!(cmd.dry_run && cmd.yes);
        assert!(ReopenCommand::try_parse_from(["reopen"]).is_err());
        let cmd = RecentCommand::try_parse_from(["recent", "-o", "json"]).unwrap();
        assert_eq!(cmd.output, TableOrJson::Json);
    }

    #[test]
    fn render_recent_lists_state_and_sanitizes() {
        assert_eq!(
            render_recent(&json!({ "closed": [] })),
            "No recently closed worktrees."
        );
        let mut hostile = entry("/wt/\u{1b}[31mred", true);
        hostile["branch"] = json!("b\u{7}ell");
        let out = render_recent(&json!({ "closed": [entry("/wt/a", false), hostile] }));
        assert!(out.contains("closed"), "{out}");
        assert!(out.contains("removed"), "{out}");
        assert!(!out.contains('\u{1b}') && !out.contains('\u{7}'), "{out:?}");
    }

    #[test]
    fn human_age_picks_the_largest_unit() {
        assert_eq!(human_age(5), "5s");
        assert_eq!(human_age(120), "2m");
        assert_eq!(human_age(7200), "2h");
        assert_eq!(human_age(3 * 86_400), "3d");
    }

    #[tokio::test]
    async fn recent_asks_for_the_list() {
        let (_dir, sock, server) = fake_daemon(vec![listing(vec![entry("/wt/a", false)])]);
        RecentCommand {
            output: TableOrJson::Json,
            socket: Some(sock),
        }
        .execute()
        .await
        .unwrap();
        assert_eq!(ops(&server.await.unwrap()), ["recent-closed"]);
    }

    #[tokio::test]
    async fn reopening_a_closed_window_just_opens_it() {
        let (_dir, sock, server) = fake_daemon(vec![
            listing(vec![entry("/wt/a", false)]),
            json!({ "ok": true, "payload": { "reopened": true, "recreated": false } }),
        ]);
        reopen_cmd(sock, false, false)
            .execute_with(|| async { panic!("a closed window needs no confirmation") })
            .await
            .unwrap();
        let seen = server.await.unwrap();
        assert_eq!(ops(&seen), ["recent-closed", "reopen"]);
        assert!(seen[1]["payload"].get("confirmed").is_none());
    }

    #[tokio::test]
    async fn dry_run_of_a_closed_window_contacts_nothing_but_the_list() {
        let (_dir, sock, server) = fake_daemon(vec![listing(vec![entry("/wt/a", false)])]);
        reopen_cmd(sock, true, false)
            .execute_with(|| async { None })
            .await
            .unwrap();
        assert_eq!(ops(&server.await.unwrap()), ["recent-closed"]);
    }

    #[tokio::test]
    async fn a_removed_worktree_is_recreated_only_after_confirmation() {
        let (_dir, sock, server) = fake_daemon(vec![
            listing(vec![entry("/wt/a", true)]),
            plan(true),
            json!({ "ok": true, "payload": { "reopened": true, "recreated": true, "opened": true } }),
        ]);
        reopen_cmd(sock, false, false)
            .execute_with(|| async { Some("y\n".to_string()) })
            .await
            .unwrap();
        let seen = server.await.unwrap();
        assert_eq!(ops(&seen), ["recent-closed", "reopen", "reopen"]);
        assert!(seen[1]["payload"].get("confirmed").is_none());
        assert_eq!(seen[2]["payload"]["confirmed"], true);
        assert_eq!(seen[2]["payload"]["path"], "/wt/a");
    }

    #[tokio::test]
    async fn declining_creates_nothing() {
        for answer in [Some("n".to_string()), None] {
            let (_dir, sock, server) =
                fake_daemon(vec![listing(vec![entry("/wt/a", true)]), plan(true)]);
            reopen_cmd(sock, false, false)
                .execute_with(|| async move { answer })
                .await
                .unwrap();
            assert_eq!(ops(&server.await.unwrap()), ["recent-closed", "reopen"]);
        }
    }

    #[tokio::test]
    async fn yes_skips_the_prompt_and_dry_run_stops_at_the_plan() {
        let (_dir, sock, server) = fake_daemon(vec![
            listing(vec![entry("/wt/a", true)]),
            plan(true),
            json!({ "ok": true, "payload": { "reopened": true, "recreated": true, "opened": false, "open_error": "no code" } }),
        ]);
        reopen_cmd(sock, false, true)
            .execute_with(|| async { panic!("-y must not prompt") })
            .await
            .unwrap();
        assert_eq!(ops(&server.await.unwrap()).len(), 3);

        let (_dir, sock, server) =
            fake_daemon(vec![listing(vec![entry("/wt/a", true)]), plan(true)]);
        reopen_cmd(sock, true, true)
            .execute_with(|| async { panic!("--dry-run must not prompt") })
            .await
            .unwrap();
        assert_eq!(ops(&server.await.unwrap()), ["recent-closed", "reopen"]);
    }

    #[tokio::test]
    async fn a_confirmed_call_the_daemon_refuses_is_an_error_not_a_success() {
        let (_dir, sock, _server) = fake_daemon(vec![
            listing(vec![entry("/wt/a", true)]),
            plan(true),
            // Re-planned at execute time and no longer restorable.
            plan(false),
        ]);
        let err = reopen_cmd(sock, false, true)
            .execute_with(|| async { None })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("was not reopened"), "{err}");
        assert!(err.to_string().contains("is gone"), "{err}");
    }

    #[tokio::test]
    async fn an_unrestorable_worktree_is_an_error_and_creates_nothing() {
        let (_dir, sock, server) =
            fake_daemon(vec![listing(vec![entry("/wt/a", true)]), plan(false)]);
        let err = reopen_cmd(sock, false, true)
            .execute_with(|| async { None })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot be restored"), "{err}");
        assert!(err.to_string().contains("is gone"), "{err}");
        assert_eq!(ops(&server.await.unwrap()), ["recent-closed", "reopen"]);
    }

    #[tokio::test]
    async fn a_path_not_in_the_list_is_refused_before_any_reopen() {
        let (_dir, sock, server) = fake_daemon(vec![listing(vec![entry("/wt/other", false)])]);
        let err = reopen_cmd(sock, false, true)
            .execute_with(|| async { None })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not in the recently closed list"));
        assert_eq!(ops(&server.await.unwrap()), ["recent-closed"]);
    }

    #[tokio::test]
    async fn a_removed_worktree_back_on_disk_is_just_opened() {
        let (_dir, sock, server) = fake_daemon(vec![
            listing(vec![entry("/wt/a", true)]),
            json!({ "ok": true, "payload": { "reopened": true, "recreated": false } }),
        ]);
        reopen_cmd(sock, false, false)
            .execute_with(|| async { panic!("nothing to recreate, so nothing to confirm") })
            .await
            .unwrap();
        assert_eq!(ops(&server.await.unwrap()), ["recent-closed", "reopen"]);
    }
}
