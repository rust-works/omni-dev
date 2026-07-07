//! The worktrees daemon service.
//!
//! A thin adapter that hosts the cross-window [`WorktreesRegistry`] under the
//! daemon's lifecycle and exposes register/heartbeat/unregister/list over the
//! control socket, plus a tray submenu with a per-window "focus" action.
//!
//! All registry state and liveness logic (the `Mutex<HashMap>`, TTL reaping, the
//! entry cap/eviction) lives in [`crate::worktrees`]; this adapter only routes
//! ops, renders the menu/status, and drives the VS Code launcher. Like the
//! Snowflake service it is a cheap, in-memory adapter — no async setup, no
//! secret persisted.
//!
//! The adapter also computes the **per-worktree git enrichment** (current
//! branch, ahead/behind counts, and the parent repository a linked worktree
//! belongs to) on read via `git2` (#1186), keeping the companion a thin reporter
//! of raw folder paths (ADR-0040). The engine stores only what the companion
//! sends; disk I/O for the enrichment lives here, alongside the launcher, never
//! under the registry lock.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use git2::Repository;
use serde::Serialize;
use serde_json::{json, Value};

use crate::daemon::service::{DaemonService, MenuAction, MenuItem, MenuSnapshot, ServiceStatus};
use crate::worktrees::{RegisterRequest, WindowEntry, WorktreesRegistry};

/// The worktrees service name (the control-socket routing key).
pub const SERVICE_NAME: &str = "worktrees";

/// Environment override for the VS Code launcher used by the "focus" tray
/// action, for when the daemon runs under launchd with a minimal `PATH`.
const VSCODE_BIN_ENV: &str = "OMNI_DEV_VSCODE_BIN";

/// Hosts the cross-window [`WorktreesRegistry`] as a [`DaemonService`].
pub struct WorktreesService {
    /// The cross-window registry this adapter routes ops to.
    registry: WorktreesRegistry,
}

impl WorktreesService {
    /// Creates the service with an empty registry. Cheap — no I/O.
    #[must_use]
    pub fn new() -> Self {
        Self {
            registry: WorktreesRegistry::new(),
        }
    }
}

impl Default for WorktreesService {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl DaemonService for WorktreesService {
    fn name(&self) -> &'static str {
        SERVICE_NAME
    }

    async fn handle(&self, op: &str, payload: Value) -> Result<Value> {
        match op {
            "register" => {
                let req: RegisterRequest =
                    serde_json::from_value(payload).context("invalid `register` payload")?;
                if req.key.trim().is_empty() {
                    bail!("`register` requires a non-empty `key`");
                }
                self.registry.register(req);
                Ok(json!({ "ok": true }))
            }
            "heartbeat" => {
                let key = require_key(&payload, "heartbeat")?;
                Ok(json!({ "known": self.registry.heartbeat(key) }))
            }
            "unregister" => {
                let key = require_key(&payload, "unregister")?;
                Ok(json!({ "removed": self.registry.unregister(key) }))
            }
            "list" => Ok(json!({ "windows": enriched_windows(self.registry.list()).await })),
            other => bail!("unknown worktrees op: {other}"),
        }
    }

    fn menu(&self) -> MenuSnapshot {
        let entries = self.registry.list();
        let items = if entries.is_empty() {
            vec![MenuItem::Label("No open windows".to_string())]
        } else {
            window_menu_items(&entries)
        };
        MenuSnapshot {
            title: "Worktrees".to_string(),
            items,
        }
    }

    async fn menu_action(&self, action_id: &str) -> Result<()> {
        if let Some(key) = action_id.strip_prefix("focus:") {
            // The registry resolves the folder under its own lock and clones it
            // out, so the mutex is never held across the process launch.
            let folder = self
                .registry
                .first_folder(key)
                .ok_or_else(|| anyhow!("no open window with key {key} (it may have closed)"))?;
            focus_window(&folder)?;
            return Ok(());
        }
        bail!("unknown worktrees menu action: {action_id}")
    }

    async fn status(&self) -> ServiceStatus {
        let entries = self.registry.list();
        let repos: BTreeSet<&str> = entries.iter().filter_map(|e| e.repo.as_deref()).collect();
        let summary = format!("{} window(s) across {} repo(s)", entries.len(), repos.len());
        let windows = enriched_windows(entries).await;
        ServiceStatus {
            name: SERVICE_NAME.to_string(),
            healthy: true,
            summary,
            detail: json!({ "windows": windows }),
        }
    }

    async fn shutdown(&self) {
        // In-memory only; nothing to drain or persist.
    }
}

/// Extracts a required string `key` from an op payload, erroring with the op
/// name when it is absent or not a string.
fn require_key<'a>(payload: &'a Value, op: &str) -> Result<&'a str> {
    payload
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("`{op}` requires `key`"))
}

/// The live git state of a worktree folder: the checked-out branch and how far
/// it has diverged from its upstream. Computed on read from the on-disk repo
/// (#1186), so `list`/`status`/`menu` reflect the current branch rather than a
/// snapshot taken at registration.
///
/// Every field is optional and degrades independently: a folder that is not a
/// git repo, is on a detached HEAD, or whose branch tracks no upstream is still
/// listed — just without the fields it cannot supply. The `skip_serializing_if`
/// attributes let it flatten cleanly onto an entry (see [`EnrichedEntry`]),
/// omitting each absent field on the wire.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
struct GitStatus {
    /// The checked-out branch, or `None` when detached or not in a repo.
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    /// Commits the branch is ahead of its upstream (`None` without an upstream).
    #[serde(skip_serializing_if = "Option::is_none")]
    ahead: Option<usize>,
    /// Commits the branch is behind its upstream (`None` without an upstream).
    #[serde(skip_serializing_if = "Option::is_none")]
    behind: Option<usize>,
    /// The main repository's directory name — the parent repo for a linked
    /// worktree, the checkout's own directory otherwise. Derived from git's
    /// common dir so a worktree names the repo it belongs to rather than its
    /// worktree-folder basename. `None` when not in a repo.
    #[serde(skip_serializing_if = "Option::is_none")]
    main_repo: Option<String>,
    /// Whether the enriched folder is a **linked** git worktree rather than the
    /// repository's main working tree. Omitted (false) for a normal checkout.
    #[serde(skip_serializing_if = "is_false")]
    is_worktree: bool,
}

/// `skip_serializing_if` predicate for a `bool` defaulting to `false`, so the
/// field is dropped on the wire unless set — keeping older clients byte-identical
/// (the protocol's forward-compatibility convention).
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(b: &bool) -> bool {
    !*b
}

/// Computes the [`GitStatus`] of `folder` by discovering the repository that
/// contains it — so a subdirectory or a linked worktree both resolve — and
/// reading HEAD. Every failure mode degrades to an empty status rather than
/// erroring: the enrichment is best-effort and must never sink a `list`.
fn git_status(folder: &Path) -> GitStatus {
    let Ok(repo) = Repository::discover(folder) else {
        return GitStatus::default();
    };
    // Repo identity applies even when HEAD is unborn or detached, so a worktree
    // still names its parent repo (and is flagged as a worktree) in those states.
    let base = GitStatus {
        main_repo: main_repo_name(repo.commondir()),
        is_worktree: repo.is_worktree(),
        ..GitStatus::default()
    };
    let Ok(head) = repo.head() else {
        // An unborn branch (fresh repo, no commits) or an unreadable HEAD.
        return base;
    };
    // A branch HEAD has a UTF-8 shorthand; anything else — a detached HEAD
    // (mid-rebase or a checked-out tag/commit), or the rare non-UTF-8 branch
    // name — degrades to no branch through this one path.
    let Some(name) = head
        .shorthand()
        .ok()
        .filter(|_| head.is_branch())
        .map(str::to_string)
    else {
        return base;
    };
    let branch = git2::Branch::wrap(head);
    let (ahead, behind) = match upstream_ahead_behind(&repo, &branch) {
        Some((ahead, behind)) => (Some(ahead), Some(behind)),
        None => (None, None),
    };
    GitStatus {
        branch: Some(name),
        ahead,
        behind,
        ..base
    }
}

/// The main repository's directory name from git's common dir. For the usual
/// `<repo>/.git` layout — shared by a checkout and all its linked worktrees —
/// that is the working-tree directory's name; for a bare repo (`<name>.git`) it
/// is that directory with a trailing `.git` stripped. Best-effort: `None` when
/// no name can be derived.
fn main_repo_name(commondir: &Path) -> Option<String> {
    let file_name = commondir.file_name()?.to_string_lossy().into_owned();
    if file_name == ".git" {
        // Normal layout: the repo is the directory that contains `.git`.
        commondir
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
    } else {
        // A bare repo: use its own directory name, without any `.git` suffix.
        Some(
            file_name
                .strip_suffix(".git")
                .unwrap_or(&file_name)
                .to_string(),
        )
    }
}

/// Ahead/behind commit counts of `branch` versus its configured upstream, or
/// `None` when the branch tracks no upstream (or either tip is unresolvable).
fn upstream_ahead_behind(repo: &Repository, branch: &git2::Branch<'_>) -> Option<(usize, usize)> {
    let upstream = branch.upstream().ok()?;
    let local_oid = branch.get().target()?;
    let upstream_oid = upstream.get().target()?;
    repo.graph_ahead_behind(local_oid, upstream_oid).ok()
}

/// The wire shape of an enriched window: the stored entry fields plus the
/// daemon-computed git state, flattened into one JSON object. Serializing
/// through a single struct (rather than mutating a `Value`) keeps every present
/// field on one code path and lets `skip_serializing_if` on [`GitStatus`] drop
/// the absent git fields — no manual per-field insertion.
#[derive(Serialize)]
struct EnrichedEntry<'a> {
    #[serde(flatten)]
    entry: &'a WindowEntry,
    #[serde(flatten)]
    git: GitStatus,
}

/// Serializes a registry entry and folds in the live [`git_status`] of its
/// primary (first) folder, producing the JSON object served on the wire
/// (`list`/`status`) and read by the extension UI. Only the primary folder is
/// enriched — it is the one the table shows and the "focus" action opens.
fn enriched_entry(entry: &WindowEntry) -> Value {
    let git = entry
        .folders
        .first()
        .map(|folder| git_status(folder))
        .unwrap_or_default();
    serde_json::to_value(EnrichedEntry { entry, git }).unwrap_or_else(|_| json!({}))
}

/// Enriches a batch of entries with their git state on a blocking thread, since
/// `git2` does synchronous disk I/O and this runs inside the async control-socket
/// handler. A join failure degrades to an empty list rather than erroring.
async fn enriched_windows(entries: Vec<WindowEntry>) -> Vec<Value> {
    tokio::task::spawn_blocking(move || entries.iter().map(enriched_entry).collect())
        .await
        .unwrap_or_default()
}

/// A short human name for a window: its repo, else its first folder's basename,
/// else a placeholder.
fn display_name(entry: &WindowEntry) -> String {
    if let Some(repo) = &entry.repo {
        return repo.clone();
    }
    if let Some(folder) = entry.folders.first() {
        return folder.file_name().map_or_else(
            || folder.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
    }
    "(no folder)".to_string()
}

/// Separator between the repo name and branch for a normal working tree.
const REPO_SEP: char = '·';
/// Separator marking a **linked worktree** (a git "fork" glyph), so a worktree
/// line is distinguishable at a glance from its parent repo's main checkout.
const WORKTREE_SEP: char = '⑂';

/// Builds the tray items for a non-empty window list: **one clickable line per
/// window** whose label carries the live git state and whose click focuses that
/// window. A window with no workspace folder has nothing for `code` to open, so
/// it stays a non-clickable status line. The labels read each worktree from disk
/// (via [`window_label`]) — cheap for a realistic window count and consistent
/// with reap-on-read.
fn window_menu_items(entries: &[WindowEntry]) -> Vec<MenuItem> {
    entries
        .iter()
        .map(|entry| {
            let label = window_label(entry);
            if entry.folders.is_empty() {
                MenuItem::Label(label)
            } else {
                MenuItem::Action(MenuAction {
                    id: format!("focus:{}", entry.key),
                    label,
                    enabled: true,
                })
            }
        })
        .collect()
}

/// The tray label for one window: the **main repository** name, then live branch
/// state (`omni-dev · branch (+2 -1)`) when the primary folder is a git repo. A
/// linked worktree is set off with the [`WORKTREE_SEP`] fork glyph
/// (`omni-dev ⑂ branch`) so it reads distinctly from the main checkout; a folder
/// that is not a repo falls back to its reported title.
fn window_label(entry: &WindowEntry) -> String {
    let status = entry
        .folders
        .first()
        .map(|folder| git_status(folder))
        .unwrap_or_default();
    // Prefer the git-derived main repo so a linked worktree names its parent
    // repository rather than its worktree-folder basename.
    let name = status
        .main_repo
        .clone()
        .unwrap_or_else(|| display_name(entry));
    if let Some(branch) = &status.branch {
        let sep = if status.is_worktree {
            WORKTREE_SEP
        } else {
            REPO_SEP
        };
        return match sync_indicator(status.ahead, status.behind) {
            Some(sync) => format!("{name} {sep} {branch} {sync}"),
            None => format!("{name} {sep} {branch}"),
        };
    }
    // No git branch (not a repo / detached): fall back to the reported title.
    match &entry.title {
        Some(title) if title != &name => format!("{name} {REPO_SEP} {title}"),
        _ => name,
    }
}

/// A compact `(+ahead -behind)` divergence indicator, or `None` when the branch
/// has no upstream to compare against.
fn sync_indicator(ahead: Option<usize>, behind: Option<usize>) -> Option<String> {
    match (ahead, behind) {
        (Some(ahead), Some(behind)) => Some(format!("(+{ahead} -{behind})")),
        _ => None,
    }
}

/// Well-known absolute locations for the VS Code launcher, tried in order so a
/// daemon running under launchd (with a minimal `PATH`) still finds it.
const CODE_BINARY_CANDIDATES: &[&str] = &[
    "/usr/local/bin/code",
    "/opt/homebrew/bin/code",
    "/Applications/Visual Studio Code.app/Contents/Resources/app/bin/code",
    "/usr/bin/code",
];

/// Focuses (or opens, since VS Code reuses an already-open window) `folder` in
/// VS Code by spawning its CLI, resolved via [`resolve_code_binary`].
fn focus_window(folder: &Path) -> Result<()> {
    focus_window_with(&resolve_code_binary(), folder)
}

/// Spawns `program` on `folder` after validating the folder. Split out from
/// [`focus_window`] so the validation and spawn paths are testable with an
/// explicit launcher (no environment or installed-editor dependency).
///
/// Best-effort and non-blocking: the spawned child is reaped on a detached
/// thread so a long-lived daemon does not accumulate zombies one per focus.
fn focus_window_with(program: &Path, folder: &Path) -> Result<()> {
    // Workspace-folder paths are absolute; requiring it also rules out a path
    // that begins with `-` being parsed by `code` as a flag.
    if !folder.is_absolute() {
        bail!(
            "refusing to focus a non-absolute folder path: {}",
            folder.display()
        );
    }
    if !folder.is_dir() {
        bail!("worktree folder no longer exists: {}", folder.display());
    }
    // Detach the launcher's stdio so its output never interleaves into the
    // long-lived daemon's own stdout/stderr (or the test harness's).
    let child = Command::new(program)
        .arg(folder)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "failed to launch `{}` to focus {}",
                program.display(),
                folder.display()
            )
        })?;
    // Reap the child without blocking so it never lingers as a zombie.
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

/// Resolves the VS Code launcher from the real environment: the
/// `OMNI_DEV_VSCODE_BIN` override, then [`CODE_BINARY_CANDIDATES`], then bare
/// `code` on `PATH`. The pure resolution logic lives in
/// [`resolve_code_binary_from`] for testing.
fn resolve_code_binary() -> PathBuf {
    resolve_code_binary_from(std::env::var_os(VSCODE_BIN_ENV), CODE_BINARY_CANDIDATES)
}

/// Pure launcher resolution: `env_override` wins; otherwise the first existing
/// `candidate`; otherwise bare `code`.
fn resolve_code_binary_from(
    env_override: Option<std::ffi::OsString>,
    candidates: &[&str],
) -> PathBuf {
    if let Some(path) = env_override {
        return PathBuf::from(path);
    }
    for candidate in candidates {
        let path = Path::new(candidate);
        if path.exists() {
            return path.to_path_buf();
        }
    }
    PathBuf::from("code")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn register_payload(key: &str, repo: Option<&str>, folder: &str) -> Value {
        json!({
            "key": key,
            "folders": [folder],
            "repo": repo,
            "title": format!("{key}-title"),
            "pid": 1234,
        })
    }

    /// Pulls the `windows` array out of a `list`/`status` payload.
    fn windows_of(payload: &Value) -> &Vec<Value> {
        payload
            .get("windows")
            .and_then(Value::as_array)
            .expect("windows array")
    }

    #[tokio::test]
    async fn name_and_unknown_op() {
        let svc = WorktreesService::new();
        assert_eq!(svc.name(), "worktrees");
        assert!(svc.handle("frobnicate", Value::Null).await.is_err());
    }

    #[tokio::test]
    async fn handle_routes_ops_and_shapes_payloads() {
        let svc = WorktreesService::new();
        // Empty to start.
        let payload = svc.handle("list", Value::Null).await.unwrap();
        assert_eq!(payload, json!({ "windows": [] }));

        // register → { ok: true }, then it shows up in list.
        let reply = svc
            .handle("register", register_payload("w1", Some("repo-a"), "/tmp/a"))
            .await
            .unwrap();
        assert_eq!(reply, json!({ "ok": true }));
        let windows = windows_of(&svc.handle("list", Value::Null).await.unwrap()).clone();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].get("key").and_then(Value::as_str), Some("w1"));
        assert!(windows[0].get("last_seen").is_some());

        // heartbeat known/unknown.
        let known = svc
            .handle("heartbeat", json!({ "key": "w1" }))
            .await
            .unwrap();
        assert_eq!(known, json!({ "known": true }));
        let unknown = svc
            .handle("heartbeat", json!({ "key": "nope" }))
            .await
            .unwrap();
        assert_eq!(unknown, json!({ "known": false }));

        // unregister removes, then repeats as a no-op success.
        let gone = svc
            .handle("unregister", json!({ "key": "w1" }))
            .await
            .unwrap();
        assert_eq!(gone, json!({ "removed": true }));
        let again = svc
            .handle("unregister", json!({ "key": "w1" }))
            .await
            .unwrap();
        assert_eq!(again, json!({ "removed": false }));
    }

    #[tokio::test]
    async fn handle_rejects_missing_or_empty_key() {
        let svc = WorktreesService::new();
        // register validates a present, non-blank key.
        assert!(svc.handle("register", json!({})).await.is_err());
        assert!(svc
            .handle("register", json!({ "key": "  " }))
            .await
            .is_err());
        // heartbeat/unregister require the key via `require_key`.
        assert!(svc.handle("heartbeat", json!({})).await.is_err());
        assert!(svc.handle("unregister", json!({})).await.is_err());
    }

    #[test]
    fn display_name_prefers_repo_then_folder_basename() {
        let base = WindowEntry {
            key: "k".to_string(),
            folders: vec![PathBuf::from("/home/me/project")],
            repo: Some("my-repo".to_string()),
            title: None,
            pid: None,
            last_seen: Utc::now(),
        };
        assert_eq!(display_name(&base), "my-repo");

        let no_repo = WindowEntry {
            repo: None,
            ..base.clone()
        };
        assert_eq!(display_name(&no_repo), "project");

        let nothing = WindowEntry {
            repo: None,
            folders: vec![],
            ..base.clone()
        };
        assert_eq!(display_name(&nothing), "(no folder)");

        // A folder with no basename (the filesystem root) falls back to its
        // displayed path rather than panicking or yielding an empty name.
        let rootish = WindowEntry {
            repo: None,
            folders: vec![PathBuf::from("/")],
            ..base
        };
        assert_eq!(display_name(&rootish), "/");
    }

    #[test]
    fn window_menu_items_merge_stats_and_focus_into_one_clickable_line() {
        let now = Utc::now();
        let entries = vec![
            // A folder-bearing, non-repo window: one clickable Action whose label
            // is the stats line ("name · title", since /tmp is not a git repo).
            WindowEntry {
                key: "k1".to_string(),
                folders: vec![PathBuf::from("/tmp/a")],
                repo: Some("repo".to_string()),
                title: Some("a branch".to_string()),
                pid: None,
                last_seen: now,
            },
            // A folderless window has nothing to focus, so it stays a plain
            // Label; a title equal to the name collapses to just the name.
            WindowEntry {
                key: "k2".to_string(),
                folders: vec![],
                repo: Some("solo".to_string()),
                title: Some("solo".to_string()),
                pid: None,
                last_seen: now,
            },
        ];
        let items = window_menu_items(&entries);
        // Exactly one item per window — no duplicate label, no separator.
        assert_eq!(items.len(), 2);
        assert!(!items.iter().any(|i| matches!(i, MenuItem::Separator)));

        // The folder-bearing window is a single clickable action carrying the
        // stats label (the old label + Focus action, merged).
        let action = items
            .iter()
            .find_map(|i| match i {
                MenuItem::Action(a) => Some(a),
                _ => None,
            })
            .expect("a focus action");
        assert_eq!(action.id, "focus:k1");
        assert_eq!(action.label, "repo · a branch");

        // The folderless window is a non-clickable label (not "solo · solo").
        let labels: Vec<&str> = items
            .iter()
            .filter_map(|i| match i {
                MenuItem::Label(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(labels, vec!["solo"]);
    }

    #[tokio::test]
    async fn menu_and_status_shapes() {
        let svc = WorktreesService::new();
        // Empty.
        let menu = svc.menu();
        assert_eq!(menu.title, "Worktrees");
        assert!(matches!(
            menu.items.first(),
            Some(MenuItem::Label(text)) if text == "No open windows"
        ));
        let status = svc.status().await;
        assert_eq!(status.name, "worktrees");
        assert!(status.healthy);
        assert_eq!(status.summary, "0 window(s) across 0 repo(s)");

        // With two windows in the same repo.
        svc.handle("register", register_payload("w1", Some("repo-a"), "/tmp/a"))
            .await
            .unwrap();
        svc.handle("register", register_payload("w2", Some("repo-a"), "/tmp/b"))
            .await
            .unwrap();
        let status = svc.status().await;
        assert_eq!(status.summary, "2 window(s) across 1 repo(s)");

        let menu = svc.menu();
        // One clickable item per window — no separator, no duplicate label.
        assert_eq!(menu.items.len(), 2);
        assert!(!menu.items.iter().any(|i| matches!(i, MenuItem::Separator)));
        let action_ids: Vec<&str> = menu
            .items
            .iter()
            .filter_map(|i| match i {
                MenuItem::Action(a) => Some(a.id.as_str()),
                _ => None,
            })
            .collect();
        assert!(action_ids.contains(&"focus:w1"));
        assert!(action_ids.contains(&"focus:w2"));
    }

    #[tokio::test]
    async fn default_constructs_an_empty_service() {
        let svc = WorktreesService::default();
        let payload = svc.handle("list", Value::Null).await.unwrap();
        assert_eq!(payload, json!({ "windows": [] }));
    }

    #[tokio::test]
    async fn menu_action_rejects_unknown_and_missing_window() {
        let svc = WorktreesService::new();
        assert!(svc.menu_action("bogus").await.is_err());
        // A focus for a key with no registration errors rather than spawning.
        assert!(svc.menu_action("focus:nope").await.is_err());
        svc.shutdown().await;
    }

    /// Restores `OMNI_DEV_VSCODE_BIN` on drop. Only this test reads the variable
    /// (via `resolve_code_binary` → `focus_window`), so there is no cross-test
    /// race despite the process-global mutation.
    struct VscodeBinGuard(Option<std::ffi::OsString>);
    impl Drop for VscodeBinGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var(VSCODE_BIN_ENV, v),
                None => std::env::remove_var(VSCODE_BIN_ENV),
            }
        }
    }

    #[tokio::test]
    async fn menu_action_focus_resolves_folder_and_spawns() {
        let dir = tempfile::tempdir().unwrap();
        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w1", "folders": [dir.path()], "repo": "r" }),
        )
        .await
        .unwrap();

        // Point the launcher at a harmless binary so the spawn deterministically
        // succeeds and the focus path returns Ok.
        let _g = VscodeBinGuard(std::env::var_os(VSCODE_BIN_ENV));
        std::env::set_var(VSCODE_BIN_ENV, "/bin/sh");
        svc.menu_action("focus:w1").await.unwrap();
    }

    #[test]
    fn focus_window_with_validates_folder_then_spawns() {
        let dir = tempfile::tempdir().unwrap();
        // Non-absolute and missing-directory folders are rejected before spawn.
        assert!(focus_window_with(Path::new("/bin/sh"), Path::new("relative/dir")).is_err());
        assert!(
            focus_window_with(Path::new("/bin/sh"), Path::new("/no/such/abs/dir/xyzzy")).is_err()
        );
        // A valid absolute directory spawns the launcher successfully.
        focus_window_with(Path::new("/bin/sh"), dir.path()).unwrap();
        // A missing launcher surfaces the spawn error (with context), not Ok.
        assert!(focus_window_with(Path::new("/no/such/launcher/xyzzy"), dir.path()).is_err());
    }

    #[test]
    fn resolve_code_binary_from_prefers_env_then_candidate_then_fallback() {
        // Env override wins outright.
        assert_eq!(
            resolve_code_binary_from(Some("/custom/code".into()), &["/usr/bin/code"]),
            PathBuf::from("/custom/code")
        );
        // No override: the first existing candidate is chosen.
        let existing = tempfile::NamedTempFile::new().unwrap();
        let existing_path = existing.path().to_str().unwrap();
        assert_eq!(
            resolve_code_binary_from(None, &["/no/such/candidate/xyzzy", existing_path]),
            PathBuf::from(existing_path)
        );
        // Nothing exists: fall back to bare `code` on PATH.
        assert_eq!(
            resolve_code_binary_from(None, &["/no/such/candidate/xyzzy"]),
            PathBuf::from("code")
        );
        // The real-env wrapper resolves without panicking.
        let _ = resolve_code_binary();
    }

    // --- Git enrichment (#1186) --------------------------------------------

    /// Initializes a fresh repo with a deterministic identity so `commit()`
    /// works without depending on a global git config.
    fn init_repo(dir: &Path) -> Repository {
        let repo = Repository::init(dir).unwrap();
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Test").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();
        repo
    }

    /// Writes an empty-tree commit (file content is irrelevant to ahead/behind),
    /// optionally moving `refname` to it, and returns its oid.
    fn empty_commit(
        repo: &Repository,
        refname: Option<&str>,
        parents: &[&git2::Commit<'_>],
        msg: &str,
    ) -> git2::Oid {
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        repo.commit(refname, &sig, &sig, msg, &tree, parents)
            .unwrap()
    }

    /// Builds a repo whose `main` is 1 commit ahead of and 1 behind a configured
    /// `origin/main` upstream, so enrichment reports `ahead: 1, behind: 1`.
    fn diverging_repo(dir: &Path) -> Repository {
        let repo = init_repo(dir);
        // A: the shared base on `main`.
        let a = empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        let a_commit = repo.find_commit(a).unwrap();
        // origin/main diverges to C, a sibling of the local tip.
        let c = empty_commit(&repo, None, &[&a_commit], "C");
        repo.reference("refs/remotes/origin/main", c, true, "origin main")
            .unwrap();
        // Local `main` advances to B → 1 ahead of / 1 behind origin/main.
        empty_commit(&repo, Some("refs/heads/main"), &[&a_commit], "B");
        // Release the commit's borrow of `repo` so it can be returned.
        drop(a_commit);
        repo.set_head("refs/heads/main").unwrap();
        // Configure the tracking relationship so `upstream()` resolves.
        let mut cfg = repo.config().unwrap();
        cfg.set_str("remote.origin.url", "https://example.invalid/x.git")
            .unwrap();
        cfg.set_str("remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*")
            .unwrap();
        cfg.set_str("branch.main.remote", "origin").unwrap();
        cfg.set_str("branch.main.merge", "refs/heads/main").unwrap();
        repo
    }

    #[test]
    fn git_status_reads_branch_and_ahead_behind() {
        let dir = tempfile::tempdir().unwrap();
        let _repo = diverging_repo(dir.path());
        let status = git_status(dir.path());
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.ahead, Some(1));
        assert_eq!(status.behind, Some(1));
        // A normal checkout names itself and is not flagged a worktree.
        assert_eq!(
            status.main_repo.as_deref(),
            dir.path().file_name().and_then(|n| n.to_str())
        );
        assert!(!status.is_worktree);
    }

    #[test]
    fn git_status_empty_repo_is_unborn() {
        // A repo with no commits has an unborn HEAD, so `head()` errors and the
        // branch/sync fields stay empty rather than panicking — but the repo
        // identity is still resolved from the common dir.
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let status = git_status(dir.path());
        assert_eq!(status.branch, None);
        assert_eq!(status.ahead, None);
        assert_eq!(status.behind, None);
        assert_eq!(
            status.main_repo.as_deref(),
            dir.path().file_name().and_then(|n| n.to_str())
        );
        assert!(!status.is_worktree);
    }

    #[test]
    fn git_status_no_upstream_reports_branch_only() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();
        let status = git_status(dir.path());
        assert_eq!(status.branch.as_deref(), Some("main"));
        // No upstream → ahead/behind stay absent rather than zero.
        assert_eq!(status.ahead, None);
        assert_eq!(status.behind, None);
    }

    #[test]
    fn git_status_non_repo_is_empty_detached_reports_repo_without_branch() {
        // A plain directory that is not a git repo yields nothing at all.
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(git_status(plain.path()), GitStatus::default());

        // A detached HEAD reports no branch (and thus no sync), but the repo
        // identity is still resolved from the common dir.
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        let a = empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head_detached(a).unwrap();
        let status = git_status(dir.path());
        assert_eq!(status.branch, None);
        assert_eq!(status.ahead, None);
        assert_eq!(status.behind, None);
        assert_eq!(
            status.main_repo.as_deref(),
            dir.path().file_name().and_then(|n| n.to_str())
        );
        assert!(!status.is_worktree);
    }

    #[test]
    fn sync_indicator_formats_only_with_upstream() {
        assert_eq!(sync_indicator(Some(2), Some(1)).as_deref(), Some("(+2 -1)"));
        assert_eq!(sync_indicator(Some(0), Some(0)).as_deref(), Some("(+0 -0)"));
        assert_eq!(sync_indicator(None, None), None);
        // A partial pair (no real upstream) yields nothing.
        assert_eq!(sync_indicator(Some(1), None), None);
    }

    #[tokio::test]
    async fn list_enriches_entries_with_git_status() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();

        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w1", "folders": [dir.path()], "repo": "r" }),
        )
        .await
        .unwrap();
        let payload = svc.handle("list", Value::Null).await.unwrap();
        let windows = windows_of(&payload);
        assert_eq!(windows.len(), 1);
        assert_eq!(
            windows[0].get("branch").and_then(Value::as_str),
            Some("main")
        );
        // No upstream configured → the ahead/behind keys are absent, not zero.
        assert!(windows[0].get("ahead").is_none());
        assert!(windows[0].get("behind").is_none());
        // The main repo name is enriched onto the entry.
        assert_eq!(
            windows[0].get("main_repo").and_then(Value::as_str),
            dir.path().file_name().and_then(|n| n.to_str())
        );

        // A non-repo folder is still listed, just without a branch or main repo.
        let plain = tempfile::tempdir().unwrap();
        svc.handle(
            "register",
            json!({ "key": "w2", "folders": [plain.path()], "repo": "plain" }),
        )
        .await
        .unwrap();
        let windows = windows_of(&svc.handle("list", Value::Null).await.unwrap()).clone();
        let w2 = windows
            .iter()
            .find(|w| w.get("key").and_then(Value::as_str) == Some("w2"))
            .unwrap();
        assert!(w2.get("branch").is_none());
        assert!(w2.get("main_repo").is_none());
    }

    #[test]
    fn window_label_prefers_git_branch_over_title() {
        let dir = tempfile::tempdir().unwrap();
        let repo = init_repo(dir.path());
        empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();
        let repo_name = dir.path().file_name().unwrap().to_str().unwrap();
        let entry = WindowEntry {
            key: "k".to_string(),
            folders: vec![dir.path().to_path_buf()],
            // Both the companion `repo` and `title` are overridden by the
            // git-derived main repo name and computed branch.
            repo: Some("companion-repo".to_string()),
            title: Some("ignored title".to_string()),
            pid: None,
            last_seen: Utc::now(),
        };
        // Main checkout: `repo · branch`, and with no upstream there is no sync.
        assert_eq!(window_label(&entry), format!("{repo_name} · main"));
    }

    #[tokio::test]
    async fn list_includes_ahead_behind_for_tracking_branch() {
        let dir = tempfile::tempdir().unwrap();
        let _repo = diverging_repo(dir.path());

        let svc = WorktreesService::new();
        svc.handle(
            "register",
            json!({ "key": "w1", "folders": [dir.path()], "repo": "r" }),
        )
        .await
        .unwrap();
        let payload = svc.handle("list", Value::Null).await.unwrap();
        let windows = windows_of(&payload);
        // A tracking branch serializes branch plus both divergence counts.
        assert_eq!(
            windows[0].get("branch").and_then(Value::as_str),
            Some("main")
        );
        assert_eq!(windows[0].get("ahead").and_then(Value::as_u64), Some(1));
        assert_eq!(windows[0].get("behind").and_then(Value::as_u64), Some(1));
    }

    #[test]
    fn window_label_includes_sync_for_tracking_branch() {
        let dir = tempfile::tempdir().unwrap();
        let _repo = diverging_repo(dir.path());
        let repo_name = dir.path().file_name().unwrap().to_str().unwrap();
        let entry = WindowEntry {
            key: "k".to_string(),
            folders: vec![dir.path().to_path_buf()],
            repo: Some("companion-repo".to_string()),
            title: None,
            pid: None,
            last_seen: Utc::now(),
        };
        // A tracking branch appends the `(+ahead -behind)` sync indicator.
        assert_eq!(window_label(&entry), format!("{repo_name} · main (+1 -1)"));
    }

    /// Adds a linked worktree of `repo` at `wt_path` checked out on a new
    /// `branch` pointed at `base`, mirroring `git worktree add -b <branch>
    /// <wt_path>`.
    fn add_worktree(repo: &Repository, base: git2::Oid, wt_path: &Path, branch: &str) {
        let commit = repo.find_commit(base).unwrap();
        repo.branch(branch, &commit, false).unwrap();
        let reference = repo
            .find_reference(&format!("refs/heads/{branch}"))
            .unwrap();
        let mut opts = git2::WorktreeAddOptions::new();
        opts.reference(Some(&reference));
        repo.worktree(branch, wt_path, Some(&opts)).unwrap();
    }

    #[test]
    fn git_status_marks_linked_worktree_and_names_parent_repo() {
        let main_dir = tempfile::tempdir().unwrap();
        let repo = init_repo(main_dir.path());
        let a = empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();

        // A linked worktree checked out on a new `feature` branch, in a
        // directory whose basename is deliberately *not* the repo name.
        let wt_parent = tempfile::tempdir().unwrap();
        let wt_path = wt_parent.path().join("feature-wt");
        add_worktree(&repo, a, &wt_path, "feature");

        let status = git_status(&wt_path);
        assert!(status.is_worktree);
        assert_eq!(status.branch.as_deref(), Some("feature"));
        // The worktree names its *parent* repo, not its worktree-folder basename.
        assert_eq!(
            status.main_repo.as_deref(),
            main_dir.path().file_name().and_then(|n| n.to_str())
        );

        // The main checkout resolves the same repo name and is not a worktree.
        let main_status = git_status(main_dir.path());
        assert!(!main_status.is_worktree);
        assert_eq!(main_status.main_repo, status.main_repo);
    }

    #[test]
    fn window_label_marks_worktree_with_fork_glyph() {
        let main_dir = tempfile::tempdir().unwrap();
        let repo = init_repo(main_dir.path());
        let a = empty_commit(&repo, Some("refs/heads/main"), &[], "A");
        repo.set_head("refs/heads/main").unwrap();
        let wt_parent = tempfile::tempdir().unwrap();
        let wt_path = wt_parent.path().join("feature-wt");
        add_worktree(&repo, a, &wt_path, "feature");

        let repo_name = main_dir.path().file_name().unwrap().to_str().unwrap();
        let entry = WindowEntry {
            key: "k".to_string(),
            folders: vec![wt_path],
            repo: Some("feature-wt".to_string()),
            title: None,
            pid: None,
            last_seen: Utc::now(),
        };
        // A worktree line: parent repo, the fork glyph, then the branch (no
        // upstream here, so no sync suffix).
        assert_eq!(window_label(&entry), format!("{repo_name} ⑂ feature"));
    }

    #[test]
    fn main_repo_name_derives_from_common_dir() {
        // Normal layout: the repo is the directory that contains `.git`.
        assert_eq!(
            main_repo_name(Path::new("/home/me/omni-dev/.git")).as_deref(),
            Some("omni-dev")
        );
        // A trailing slash on the common dir does not change the answer.
        assert_eq!(
            main_repo_name(Path::new("/home/me/omni-dev/.git/")).as_deref(),
            Some("omni-dev")
        );
        // A bare repo: its own directory name, without the `.git` suffix.
        assert_eq!(
            main_repo_name(Path::new("/srv/git/omni-dev.git")).as_deref(),
            Some("omni-dev")
        );
        // A `.git` at the filesystem root has no parent name to use.
        assert_eq!(main_repo_name(Path::new("/.git")), None);
    }
}
