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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
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
            "list" => Ok(json!({ "windows": self.registry.list() })),
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
        ServiceStatus {
            name: SERVICE_NAME.to_string(),
            healthy: true,
            summary: format!("{} window(s) across {} repo(s)", entries.len(), repos.len()),
            detail: json!({ "windows": entries }),
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

/// Builds the tray items for a non-empty window list: a label per window, a
/// separator, then a "Focus" action per window that has a folder to open.
fn window_menu_items(entries: &[WindowEntry]) -> Vec<MenuItem> {
    let mut items = Vec::new();
    for entry in entries {
        let name = display_name(entry);
        let label = match &entry.title {
            Some(title) if title != &name => format!("{name} · {title}"),
            _ => name,
        };
        items.push(MenuItem::Label(label));
    }
    items.push(MenuItem::Separator);
    for entry in entries {
        // No folder means nothing for `code` to open, so omit the action.
        if !entry.folders.is_empty() {
            items.push(MenuItem::Action(MenuAction {
                id: format!("focus:{}", entry.key),
                label: format!("Focus {}", display_name(entry)),
                enabled: true,
            }));
        }
    }
    items
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
    fn window_menu_items_label_omits_redundant_title_and_skips_folderless_actions() {
        let now = Utc::now();
        let entries = vec![
            // Title differs from the repo name → "name · title".
            WindowEntry {
                key: "k1".to_string(),
                folders: vec![PathBuf::from("/tmp/a")],
                repo: Some("repo".to_string()),
                title: Some("a branch".to_string()),
                pid: None,
                last_seen: now,
            },
            // Title equals the display name → label is just the name (the
            // `_ => name` arm), and no folder means no Focus action.
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
        let labels: Vec<&str> = items
            .iter()
            .filter_map(|i| match i {
                MenuItem::Label(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(labels.contains(&"repo · a branch"));
        assert!(labels.contains(&"solo")); // not "solo · solo"

        let action_ids: Vec<&str> = items
            .iter()
            .filter_map(|i| match i {
                MenuItem::Action(a) => Some(a.id.as_str()),
                _ => None,
            })
            .collect();
        // Only the folder-bearing window gets a Focus action.
        assert_eq!(action_ids, vec!["focus:k1"]);
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
        // Two labels + a separator + two focus actions.
        assert!(menu.items.iter().any(|i| matches!(i, MenuItem::Separator)));
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
}
