//! Action dispatch for the worktrees UI (issue #1585 Phase 2): the
//! daemon-free parity commands ported from the VS Code companion (Open
//! GitHub Repository, Copy Directory, Copy Pull Request URL(s), Move/Copy
//! Claude Session Here), Focus/Open, and the two-phase Close flow — plus the
//! render-layer's [`ActionFlow`] state machine that drives them.
//!
//! Everything that talks to the daemon goes through [`Dispatcher`], which is
//! a plain struct with one match arm per [`ActionKind`] in `check`/`execute`
//! — not a trait-per-action. Every existing two-phase precedent in this
//! crate (`CloseCommand::execute_with`, `RebaseCommand::execute_with` in
//! `src/cli/worktrees.rs`) is a flat match/if-chain, not a strategy-object
//! hierarchy, and `ActionKind` needs to stay a cheap `Copy` for the
//! action-menu's filtering — boxing each action as a trait object would
//! fight that for no benefit here.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use tokio::sync::mpsc;

use super::client::WorktreesClient;
use super::hub::HubCommand;
use super::row_colors::RowColorKey;
use super::view_model::{GithubIdentity, PrBadgeRow, RepoRow, SessionBadge, WorktreeRow};
use super::wire::{CloseNoteWire, SafetyReportWire};
use crate::sessions::relocate::{self, RelocationMode};
use crate::sessions::SessionState;

/// One selectable row an action can target. Deliberately **not** `RepoRow`/
/// `WorktreeRow` themselves — those borrow from one frame's view-model
/// snapshot, but `Dispatcher::execute` is async and outlives a frame, so a
/// target is a small owned copy of just the fields an action might need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Repo {
        root: PathBuf,
        github: Option<(String, String)>,
    },
    Worktree {
        path: PathBuf,
        is_main: bool,
        /// The parent repo's GitHub identity (Open GitHub Repository).
        github: Option<(String, String)>,
        /// `Some(url)` when this row has an open PR (Copy Pull Request
        /// URL(s)); `None` renders a placeholder comment instead.
        pr_url: Option<String>,
        branch: Option<String>,
        sessions: Vec<SessionSummary>,
    },
}

impl Target {
    pub fn from_repo(repo: &RepoRow) -> Self {
        Self::Repo {
            root: repo.root.clone(),
            github: repo.github.as_ref().map(github_pair),
        }
    }

    pub fn from_worktree(wt: &WorktreeRow, parent_github: Option<&GithubIdentity>) -> Self {
        Self::Worktree {
            path: wt.path.clone(),
            is_main: wt.is_main,
            github: parent_github.map(github_pair),
            pr_url: wt.pr.as_ref().map(pr_url),
            branch: wt.branch.clone(),
            sessions: wt.sessions.iter().map(SessionSummary::from).collect(),
        }
    }

    fn github(&self) -> Option<&(String, String)> {
        match self {
            Self::Repo { github, .. } | Self::Worktree { github, .. } => github.as_ref(),
        }
    }

    fn directory(&self) -> &Path {
        match self {
            Self::Repo { root, .. } => root,
            Self::Worktree { path, .. } => path,
        }
    }
}

fn github_pair(id: &GithubIdentity) -> (String, String) {
    (id.owner.clone(), id.name.clone())
}

fn pr_url(pr: &PrBadgeRow) -> String {
    pr.url.clone()
}

/// A worktree row's live session, slimmed to what a picker label needs —
/// pulled from [`SessionBadge`], dropping the render-only `source` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub id: String,
    pub model: Option<String>,
    pub state: SessionState,
    pub last_seen: DateTime<Utc>,
}

impl From<&SessionBadge> for SessionSummary {
    fn from(badge: &SessionBadge) -> Self {
        Self {
            id: badge.session_id.clone(),
            model: badge.model.clone(),
            state: badge.state,
            last_seen: badge.last_seen,
        }
    }
}

/// Every action this phase can dispatch. `MoveClaudeSessionHere`/
/// `CopyClaudeSessionHere` are listed here only for action-menu
/// identification/gating — the render layer intercepts a selection of
/// either one *before* calling [`Dispatcher::check`]/`execute`, routing
/// instead to [`Dispatcher::check_relocate_session`]/
/// [`Dispatcher::execute_relocate_session`], since a session relocation
/// needs a resolved source-session-id and destination that the generic
/// `targets: &[Target]` fan-out shape doesn't carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    OpenGithubRepository,
    CopyDirectory,
    CopyPullRequestUrls,
    MoveClaudeSessionHere,
    CopyClaudeSessionHere,
    Focus,
    /// `remove: false` — closes the owning window(s), deletes nothing.
    CloseWindow,
    /// `remove: true` — the destructive, two-phase delete.
    CloseWorktree,
    SetRowColor(&'static str),
    ClearRowColor,
}

/// What [`Dispatcher::check`] reports before anything happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckReport {
    /// No confirmation needed — the action either can't fail meaningfully
    /// (GitHub open, copy actions, row-colour writes) or the daemon has
    /// nothing to check (`CloseWindow`: non-destructive).
    ProceedWithoutConfirm,
    /// Needs a yes/no gate.
    NeedsConfirm {
        prompt: ConfirmPrompt,
        has_risk: bool,
    },
    /// The daemon-side check refused outright (e.g. every target
    /// non-removable) — render as an error line, never call `execute`.
    Refused { reason: String },
}

/// Rendered confirm-modal content — ratatui-agnostic (`Vec<String>` lines,
/// not a `Paragraph`), built by porting `render_safety_report`/
/// `render_notes`'s *logic* (`src/cli/worktrees.rs:1456-1508`), not their
/// stdout mechanism.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfirmPrompt {
    pub title: String,
    pub body_lines: Vec<String>,
    pub risk_lines: Vec<String>,
    pub info_lines: Vec<String>,
}

/// What [`Dispatcher::execute`] reports once the (possibly fanned-out)
/// daemon call(s) finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    /// A single aggregate result (GitHub open, copy actions, row colour).
    Done {
        summary: String,
    },
    /// One result per target — Focus/CloseWindow/CloseWorktree's fan-out.
    /// One target's failure never suppresses the others' results.
    BatchDone {
        results: Vec<(PathBuf, Result<(), String>)>,
    },
    Failed {
        error: String,
    },
}

/// Render-layer UI state driving which popup is on screen; not part of the
/// daemon-facing flow above. `Dispatcher` itself is stateless per call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ActionFlow {
    #[default]
    Idle,
    Checking {
        action: ActionKind,
        targets: Vec<Target>,
    },
    AwaitingConfirm {
        action: ActionKind,
        targets: Vec<Target>,
        prompt: ConfirmPrompt,
    },
    Executing {
        action: ActionKind,
        targets: Vec<Target>,
    },
    Done {
        outcome: ActionOutcome,
    },
    Failed {
        error: String,
    },
}

/// Dispatches an [`ActionKind`] against a set of [`Target`]s: the daemon
/// calls, the OS-boundary calls (browser open, clipboard write), and the
/// row-colour writes (via [`HubCommand`], the same channel Phase 1's
/// `Hub::apply` already handles every variant of).
pub struct Dispatcher {
    client: WorktreesClient,
    hub_commands: mpsc::UnboundedSender<HubCommand>,
}

impl Dispatcher {
    pub fn new(client: WorktreesClient, hub_commands: mpsc::UnboundedSender<HubCommand>) -> Self {
        Self {
            client,
            hub_commands,
        }
    }

    pub async fn check(&self, action: ActionKind, targets: &[Target]) -> CheckReport {
        match action {
            ActionKind::OpenGithubRepository
            | ActionKind::CopyDirectory
            | ActionKind::CopyPullRequestUrls
            | ActionKind::Focus
            | ActionKind::CloseWindow
            | ActionKind::SetRowColor(_)
            | ActionKind::ClearRowColor => CheckReport::ProceedWithoutConfirm,
            ActionKind::CloseWorktree => self.check_close_worktree(targets).await,
            ActionKind::MoveClaudeSessionHere | ActionKind::CopyClaudeSessionHere => {
                CheckReport::Refused {
                    reason: "session relocation is resolved through its own picker flow, \
                             not the generic check path"
                        .to_string(),
                }
            }
        }
    }

    pub async fn execute(&self, action: ActionKind, targets: &[Target]) -> ActionOutcome {
        match action {
            ActionKind::OpenGithubRepository => self.execute_open_github_repository(targets),
            ActionKind::CopyDirectory => self.execute_copy_directory(targets),
            ActionKind::CopyPullRequestUrls => self.execute_copy_pull_request_urls(targets),
            ActionKind::Focus => self.execute_focus(targets).await,
            ActionKind::CloseWindow => self.execute_close(targets, false).await,
            ActionKind::CloseWorktree => self.execute_close(targets, true).await,
            ActionKind::SetRowColor(color) => self.execute_set_row_color(targets, color),
            ActionKind::ClearRowColor => self.execute_clear_row_color(targets),
            ActionKind::MoveClaudeSessionHere | ActionKind::CopyClaudeSessionHere => {
                ActionOutcome::Failed {
                    error: "session relocation is resolved through its own picker flow, \
                            not the generic execute path"
                        .to_string(),
                }
            }
        }
    }

    // --- Open GitHub Repository ---------------------------------------

    fn execute_open_github_repository(&self, targets: &[Target]) -> ActionOutcome {
        let urls = github_urls(targets);
        if urls.is_empty() {
            return ActionOutcome::Failed {
                error: "no target has a GitHub remote".to_string(),
            };
        }
        let mut opened = 0usize;
        let mut errors = Vec::new();
        for url in &urls {
            match open_in_os(url) {
                Ok(()) => opened += 1,
                Err(e) => errors.push(format!("{url}: {e:#}")),
            }
        }
        if opened == 0 {
            return ActionOutcome::Failed {
                error: errors.join("; "),
            };
        }
        ActionOutcome::Done {
            summary: format!("Opened {opened} repository page(s)"),
        }
    }

    // --- Copy Directory --------------------------------------------------

    fn execute_copy_directory(&self, targets: &[Target]) -> ActionOutcome {
        let dirs = directory_paths(targets);
        if dirs.is_empty() {
            return ActionOutcome::Failed {
                error: "nothing to copy".to_string(),
            };
        }
        let text = dirs
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("\n");
        match write_clipboard(&text) {
            Ok(()) => ActionOutcome::Done {
                summary: if dirs.len() == 1 {
                    format!("Copied {}", dirs[0].display())
                } else {
                    format!("Copied {} directories", dirs.len())
                },
            },
            Err(e) => ActionOutcome::Failed {
                error: format!("{e:#}"),
            },
        }
    }

    // --- Copy Pull Request URL(s) ----------------------------------------

    fn execute_copy_pull_request_urls(&self, targets: &[Target]) -> ActionOutcome {
        let lines = pull_request_lines(targets);
        if lines.is_empty() {
            return ActionOutcome::Failed {
                error: "no worktree row selected".to_string(),
            };
        }
        let url_count = lines.iter().filter(|l| !l.starts_with('#')).count();
        let placeholder_count = lines.len() - url_count;
        match write_clipboard(&lines.join("\n")) {
            Ok(()) => ActionOutcome::Done {
                summary: pr_copy_summary(url_count, placeholder_count),
            },
            Err(e) => ActionOutcome::Failed {
                error: format!("{e:#}"),
            },
        }
    }

    // --- Focus/Open --------------------------------------------------------

    async fn execute_focus(&self, targets: &[Target]) -> ActionOutcome {
        let mut futures = Vec::new();
        for target in targets {
            let Target::Worktree { path, .. } = target else {
                continue;
            };
            let path = path.clone();
            let client = self.client.clone();
            futures.push(async move {
                let result = async {
                    let canonical = std::fs::canonicalize(&path)
                        .with_context(|| format!("failed to resolve {}", path.display()))?;
                    client.open(&canonical).await
                }
                .await;
                (path, result.map_err(|e| format!("{e:#}")))
            });
        }
        let results = futures_util_join_all(futures).await;
        ActionOutcome::BatchDone { results }
    }

    // --- Close ---------------------------------------------------------

    async fn check_close_worktree(&self, targets: &[Target]) -> CheckReport {
        let mut futures = Vec::new();
        for target in targets {
            let Target::Worktree { path, .. } = target else {
                continue;
            };
            let path = path.clone();
            let client = self.client.clone();
            futures.push(async move {
                let result = client.close_check(&path).await;
                (path, result)
            });
        }
        if futures.is_empty() {
            return CheckReport::Refused {
                reason: "no worktree row selected".to_string(),
            };
        }
        let outcomes = futures_util_join_all(futures).await;

        let mut prompt = ConfirmPrompt {
            title: if outcomes.len() == 1 {
                "Close worktree?".to_string()
            } else {
                format!("Close {} worktrees?", outcomes.len())
            },
            ..Default::default()
        };
        let mut any_removable = false;
        for (path, result) in &outcomes {
            match result {
                Ok(report) => {
                    if report.removable {
                        any_removable = true;
                    }
                    prompt.body_lines.extend(safety_report_body(path, report));
                    prompt.risk_lines.extend(note_lines(path, &report.risks));
                    prompt.info_lines.extend(note_lines(path, &report.info));
                }
                Err(e) => {
                    prompt
                        .risk_lines
                        .push(format!("{}: [check-failed] {e:#}", path.display()));
                }
            }
        }
        if !any_removable {
            return CheckReport::Refused {
                reason: format!(
                    "none of the {} selected worktree(s) are removable",
                    outcomes.len()
                ),
            };
        }
        let has_risk = !prompt.risk_lines.is_empty();
        CheckReport::NeedsConfirm { prompt, has_risk }
    }

    async fn execute_close(&self, targets: &[Target], remove: bool) -> ActionOutcome {
        let mut futures = Vec::new();
        for target in targets {
            let Target::Worktree { path, .. } = target else {
                continue;
            };
            let path = path.clone();
            let client = self.client.clone();
            futures.push(async move {
                let result = client.close_execute(&path, remove).await;
                (path, result.map_err(|e| format!("{e:#}")))
            });
        }
        let results = futures_util_join_all(futures).await;
        ActionOutcome::BatchDone { results }
    }

    // --- Row colour --------------------------------------------------------

    fn execute_set_row_color(&self, targets: &[Target], color: &'static str) -> ActionOutcome {
        for target in targets {
            let key = row_color_key(target);
            let _ = self
                .hub_commands
                .send(HubCommand::SetRowColor(key, color.to_string()));
        }
        ActionOutcome::Done {
            summary: format!("Set colour {color}"),
        }
    }

    fn execute_clear_row_color(&self, targets: &[Target]) -> ActionOutcome {
        for target in targets {
            let key = row_color_key(target);
            let _ = self.hub_commands.send(HubCommand::ClearRowColor(key));
        }
        ActionOutcome::Done {
            summary: "Cleared colour".to_string(),
        }
    }

    // --- Move/Copy Claude session here --------------------------------

    /// Checks whether relocating `session_id` from `source_dir` to
    /// `dest_worktree` is safe: refuses a session written moments ago (may
    /// still be live) and a destination collision (never clobbers an
    /// existing session of the same id).
    pub fn check_relocate_session(
        &self,
        source_dir: &Path,
        session: &relocate_types::SessionInfo,
        dest_worktree: &Path,
    ) -> CheckReport {
        if relocate::is_likely_live(session.modified, std::time::SystemTime::now()) {
            return CheckReport::Refused {
                reason: "that session was written moments ago and may still be live; \
                         close its terminal/window first, then try again"
                    .to_string(),
            };
        }
        let Some(dest_dir) = relocate::project_dir_for(dest_worktree) else {
            return CheckReport::Refused {
                reason: "could not resolve the Claude projects directory".to_string(),
            };
        };
        if let Some(collision) = relocate::destination_collision(session, &dest_dir) {
            return CheckReport::Refused {
                reason: format!(
                    "a session with this id already exists in the target worktree ({collision})"
                ),
            };
        }
        let artifacts = if session.has_sidecar {
            format!(
                "{}.jsonl and its {}/ sidecar (subagents, tool results)",
                session.id, session.id
            )
        } else {
            format!("{}.jsonl", session.id)
        };
        CheckReport::NeedsConfirm {
            prompt: ConfirmPrompt {
                title: format!(
                    "Relocate this Claude session into \"{}\"?",
                    dest_worktree.display()
                ),
                body_lines: vec![
                    format!("From: {}", source_dir.display()),
                    format!("To:   {}", dest_worktree.display()),
                    String::new(),
                    format!("Artifacts: {artifacts}"),
                ],
                risk_lines: Vec::new(),
                info_lines: Vec::new(),
            },
            has_risk: false,
        }
    }

    /// Executes an already-confirmed relocation.
    pub fn execute_relocate_session(
        &self,
        source_dir: &Path,
        session: &relocate_types::SessionInfo,
        dest_worktree: &Path,
        mode: RelocationMode,
    ) -> ActionOutcome {
        let Some(dest_dir) = relocate::project_dir_for(dest_worktree) else {
            return ActionOutcome::Failed {
                error: "could not resolve the Claude projects directory".to_string(),
            };
        };
        let plan = relocate::plan_relocation(
            &session.id,
            source_dir,
            &dest_dir,
            session.has_sidecar,
            mode,
        );
        match relocate::execute_relocation(&plan, &dest_dir) {
            Ok(()) => ActionOutcome::Done {
                summary: format!(
                    "{} session {} to {}",
                    if mode == RelocationMode::Move {
                        "Moved"
                    } else {
                        "Copied"
                    },
                    session.id,
                    dest_worktree.display()
                ),
            },
            Err(e) => ActionOutcome::Failed {
                error: format!("{e:#}"),
            },
        }
    }
}

/// Re-exports so callers outside this module (the render layer's session
/// picker) can name [`relocate::SessionInfo`] without reaching into
/// `crate::sessions::relocate` directly — keeps the picker's imports scoped
/// to `actions`, the same boundary every other action's types live behind.
pub(crate) mod relocate_types {
    pub(crate) use crate::sessions::relocate::SessionInfo;
}

/// The action menu's filtered, ordered item list for the current selection —
/// mirrors the VS Code extension's own `0_open`/`1_pr`/`2_claude`/`3_copy`/
/// `9_close` menu-group order (verified against `editors/vscode/package.json`'s
/// `view/item/context` entries). `2_claude` (Move/Copy session here) has no
/// VS Code equivalent — placed beside `1_pr` as another row-detail action.
pub fn applicable_actions(targets: &[Target]) -> Vec<(ActionKind, &'static str)> {
    let mut items = Vec::new();

    // 0_open
    if targets.iter().any(|t| matches!(t, Target::Worktree { .. })) {
        items.push((ActionKind::Focus, "Open Worktree"));
    }
    if !github_urls(targets).is_empty() {
        items.push((ActionKind::OpenGithubRepository, "Open GitHub Repository"));
    }

    // 1_pr
    if targets.iter().any(|t| matches!(t, Target::Worktree { .. })) {
        items.push((ActionKind::CopyPullRequestUrls, "Copy Pull Request URL(s)"));
    }

    // 2_claude — only for a single worktree row with at least one session;
    // relocating needs one unambiguous source row (see check_relocate_session).
    if let [Target::Worktree { sessions, .. }] = targets {
        if !sessions.is_empty() {
            items.push((
                ActionKind::MoveClaudeSessionHere,
                "Move Claude Session Here",
            ));
            items.push((
                ActionKind::CopyClaudeSessionHere,
                "Copy Claude Session Here (Fork)",
            ));
        }
    }

    // 3_copy
    if !targets.is_empty() {
        items.push((ActionKind::CopyDirectory, "Copy Directory"));
    }

    // 9_close
    if targets.iter().any(|t| matches!(t, Target::Worktree { .. })) {
        items.push((ActionKind::CloseWindow, "Close Window"));
        if targets
            .iter()
            .any(|t| matches!(t, Target::Worktree { is_main: false, .. }))
        {
            items.push((ActionKind::CloseWorktree, "Close Worktree"));
        }
    }

    items
}

fn row_color_key(target: &Target) -> RowColorKey {
    match target {
        Target::Repo { root, .. } => RowColorKey::Repo(root.clone()),
        Target::Worktree { path, .. } => RowColorKey::Worktree(path.clone()),
    }
}

/// Deduped GitHub repo URLs across a selection, preserving first-seen order
/// (port of `repoWebUrlsForNodes`'s dedup, `editors/vscode/src/github.ts`).
fn github_urls(targets: &[Target]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut urls = Vec::new();
    for target in targets {
        if let Some((owner, name)) = target.github() {
            let url = format!("https://github.com/{owner}/{name}");
            if seen.insert(url.clone()) {
                urls.push(url);
            }
        }
    }
    urls
}

/// Deduped absolute directory paths across a selection, preserving
/// first-seen order (port of `nodeDirectories`, `editors/vscode/src/tree.ts`
/// — a repo row and its main worktree can map to the same directory).
fn directory_paths(targets: &[Target]) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    let mut dirs = Vec::new();
    for target in targets {
        let dir = target.directory().to_path_buf();
        if seen.insert(dir.clone()) {
            dirs.push(dir);
        }
    }
    dirs
}

/// One line per worktree target: a bare PR URL (deduped across the whole
/// block) or a `#`-prefixed placeholder comment for a row with no PR.
/// Placeholders are never deduped — every row still gets its own. Repo
/// targets contribute nothing (port of the non-lookup branches of
/// `editors/vscode/src/prClipboard.ts`; there is no "lookup failed" case to
/// port since Phase 1's tree snapshot already carries `pr` per row — no live
/// lookup exists in this v1).
fn pull_request_lines(targets: &[Target]) -> Vec<String> {
    let mut seen_urls = std::collections::HashSet::new();
    let mut lines = Vec::new();
    for target in targets {
        let Target::Worktree {
            path,
            pr_url,
            branch,
            ..
        } = target
        else {
            continue;
        };
        if let Some(url) = pr_url {
            if seen_urls.insert(url.clone()) {
                lines.push(url.clone());
            }
        } else {
            let branch = branch.as_deref().unwrap_or("(detached)");
            lines.push(format!("# No PR for {branch} in {}", path.display()));
        }
    }
    lines
}

fn pr_copy_summary(url_count: usize, placeholder_count: usize) -> String {
    match (url_count, placeholder_count) {
        (0, p) => format!("Copied {p} placeholder(s), no open PRs"),
        (u, 0) => format!("Copied {u} PR URL(s)"),
        (u, p) => format!("Copied {u} PR URL(s) and {p} placeholder(s)"),
    }
}

/// Renders one target's `removable`/`is_main`/`open` summary — the body
/// lines of `render_safety_report`'s logic (`src/cli/worktrees.rs:1456-
/// 1489`), minus the notes (kept separate as `risk_lines`/`info_lines`).
fn safety_report_body(path: &Path, report: &SafetyReportWire) -> Vec<String> {
    let mut lines = vec![format!("Worktree: {}", path.display())];
    lines.push(format!("  removable:         {}", report.removable));
    lines.push(format!("  main working tree: {}", report.is_main));
    if report.open {
        let key = report.window_key.as_deref().unwrap_or("-");
        lines.push(format!(
            "  open in a window:  yes (key {key}, {} folder(s))",
            report.window_folder_count
        ));
    } else {
        lines.push("  open in a window:  no".to_string());
    }
    lines
}

/// One `<path>: [kind] detail` line per note — the multi-target analogue of
/// `render_notes` (`src/cli/worktrees.rs:1493-1508`), path-prefixed since a
/// batch confirm aggregates more than one target's notes into one modal.
fn note_lines(path: &Path, notes: &[CloseNoteWire]) -> Vec<String> {
    notes
        .iter()
        .map(|n| format!("{}: [{}] {}", path.display(), n.kind, n.detail))
        .collect()
}

/// Opens `url` with the platform's default handler — the `BrowserLaunch::
/// Auto` pattern already live in `src/gmail/auth.rs::open_browser`, without
/// that module's Gmail-specific `Manual`/`Command` variants (this action has
/// no configured-browser concept, only "open the OS default").
fn open_in_os(url: &str) -> Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    let mut command = Command::new(program);
    command.arg(url);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .with_context(|| format!("failed to launch a handler for {url}"))
}

fn write_clipboard(text: &str) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new().context("failed to open the clipboard")?;
    clipboard
        .set_text(text)
        .context("failed to write to the clipboard")
}

/// A tiny `join_all` so this module doesn't need a direct `futures` import
/// beyond what's already a transitive workspace dependency (`futures::
/// StreamExt` is used elsewhere in `mod.rs`) — avoids pulling in the whole
/// `futures::future` module for one call site.
async fn futures_util_join_all<T>(futures: Vec<impl std::future::Future<Output = T>>) -> Vec<T> {
    let mut results = Vec::with_capacity(futures.len());
    for fut in futures {
        results.push(fut.await);
    }
    results
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn worktree_target(path: &str, pr_url: Option<&str>) -> Target {
        Target::Worktree {
            path: PathBuf::from(path),
            is_main: false,
            github: None,
            pr_url: pr_url.map(str::to_string),
            branch: Some("main".to_string()),
            sessions: Vec::new(),
        }
    }

    fn repo_target(root: &str, github: Option<(&str, &str)>) -> Target {
        Target::Repo {
            root: PathBuf::from(root),
            github: github.map(|(o, n)| (o.to_string(), n.to_string())),
        }
    }

    #[test]
    fn github_urls_dedupes_and_preserves_order() {
        let targets = vec![
            repo_target("/repo/a", Some(("acme", "widgets"))),
            worktree_target("/repo/a/wt", None),
            repo_target("/repo/b", Some(("acme", "widgets"))), // duplicate URL
            repo_target("/repo/c", None),
        ];
        assert_eq!(
            github_urls(&targets),
            vec!["https://github.com/acme/widgets".to_string()]
        );
    }

    #[test]
    fn directory_paths_dedupes_repo_and_worktree_sharing_a_path() {
        let targets = vec![
            repo_target("/repo/a", None),
            worktree_target("/repo/a", None), // main worktree, same dir as repo root
            worktree_target("/repo/a/wt-2", None),
        ];
        let dirs = directory_paths(&targets);
        assert_eq!(
            dirs,
            vec![PathBuf::from("/repo/a"), PathBuf::from("/repo/a/wt-2"),]
        );
    }

    #[test]
    fn pull_request_lines_emits_a_placeholder_for_a_worktree_with_no_pr() {
        let targets = vec![worktree_target("/repo/wt", None)];
        assert_eq!(
            pull_request_lines(&targets),
            vec!["# No PR for main in /repo/wt".to_string()]
        );
    }

    #[test]
    fn pull_request_lines_dedupes_urls_but_never_placeholders() {
        let targets = vec![
            worktree_target("/repo/wt-1", Some("https://github.com/o/r/pull/1")),
            worktree_target("/repo/wt-2", Some("https://github.com/o/r/pull/1")), // dup
            worktree_target("/repo/wt-3", None),
            worktree_target("/repo/wt-4", None),
        ];
        let lines = pull_request_lines(&targets);
        assert_eq!(
            lines,
            vec![
                "https://github.com/o/r/pull/1".to_string(),
                "# No PR for main in /repo/wt-3".to_string(),
                "# No PR for main in /repo/wt-4".to_string(),
            ]
        );
    }

    #[test]
    fn pull_request_lines_ignores_repo_targets() {
        let targets = vec![repo_target("/repo/a", None)];
        assert!(pull_request_lines(&targets).is_empty());
    }

    #[test]
    fn pr_copy_summary_covers_every_combination() {
        assert_eq!(pr_copy_summary(3, 0), "Copied 3 PR URL(s)");
        assert_eq!(
            pr_copy_summary(0, 2),
            "Copied 2 placeholder(s), no open PRs"
        );
        assert_eq!(
            pr_copy_summary(1, 1),
            "Copied 1 PR URL(s) and 1 placeholder(s)"
        );
    }

    #[test]
    fn safety_report_body_renders_removable_main_and_open_fields() {
        let report = SafetyReportWire {
            removable: true,
            is_main: false,
            open: true,
            window_key: Some("w1".to_string()),
            window_folder_count: 2,
            risks: Vec::new(),
            info: Vec::new(),
        };
        let lines = safety_report_body(Path::new("/repo/wt"), &report);
        assert!(lines[0].contains("/repo/wt"));
        assert!(lines.iter().any(|l| l.contains("removable:         true")));
        assert!(lines
            .iter()
            .any(|l| l.contains("open in a window:  yes (key w1, 2 folder(s))")));
    }

    #[test]
    fn note_lines_prefixes_each_note_with_its_target_path() {
        let notes = vec![CloseNoteWire {
            kind: "dirty".to_string(),
            detail: "uncommitted changes".to_string(),
        }];
        let lines = note_lines(Path::new("/repo/wt"), &notes);
        assert_eq!(
            lines,
            vec!["/repo/wt: [dirty] uncommitted changes".to_string()]
        );
    }

    #[test]
    fn action_flow_defaults_to_idle() {
        assert_eq!(ActionFlow::default(), ActionFlow::Idle);
    }

    fn session_summary(id: &str) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            model: None,
            state: SessionState::Idle,
            last_seen: Utc::now(),
        }
    }

    #[test]
    fn applicable_actions_for_a_repo_row_excludes_worktree_only_actions() {
        let targets = vec![repo_target("/repo/a", Some(("acme", "widgets")))];
        let items = applicable_actions(&targets);
        let kinds: Vec<_> = items.iter().map(|(k, _)| *k).collect();
        assert!(kinds.contains(&ActionKind::OpenGithubRepository));
        assert!(kinds.contains(&ActionKind::CopyDirectory));
        assert!(!kinds.contains(&ActionKind::Focus));
        assert!(!kinds.contains(&ActionKind::CopyPullRequestUrls));
        assert!(!kinds.contains(&ActionKind::CloseWindow));
    }

    #[test]
    fn applicable_actions_hides_close_worktree_for_the_main_working_tree() {
        let mut main = worktree_target("/repo/main", None);
        if let Target::Worktree { is_main, .. } = &mut main {
            *is_main = true;
        }
        let items = applicable_actions(&[main]);
        let kinds: Vec<_> = items.iter().map(|(k, _)| *k).collect();
        assert!(kinds.contains(&ActionKind::CloseWindow));
        assert!(!kinds.contains(&ActionKind::CloseWorktree));
    }

    #[test]
    fn applicable_actions_offers_close_worktree_when_any_target_is_not_main() {
        let mut main = worktree_target("/repo/main", None);
        if let Target::Worktree { is_main, .. } = &mut main {
            *is_main = true;
        }
        let linked = worktree_target("/repo/linked", None);
        let items = applicable_actions(&[main, linked]);
        assert!(items.iter().any(|(k, _)| *k == ActionKind::CloseWorktree));
    }

    #[test]
    fn applicable_actions_offers_session_relocation_only_for_a_single_worktree_with_sessions() {
        let mut with_session = worktree_target("/repo/wt", None);
        if let Target::Worktree { sessions, .. } = &mut with_session {
            sessions.push(session_summary("s1"));
        }
        let items = applicable_actions(&[with_session.clone()]);
        let kinds: Vec<_> = items.iter().map(|(k, _)| *k).collect();
        assert!(kinds.contains(&ActionKind::MoveClaudeSessionHere));
        assert!(kinds.contains(&ActionKind::CopyClaudeSessionHere));

        // No sessions on the row: neither action offered.
        let without_session = worktree_target("/repo/wt-2", None);
        let items = applicable_actions(&[without_session]);
        assert!(!items
            .iter()
            .any(|(k, _)| *k == ActionKind::MoveClaudeSessionHere));

        // Multi-select even with sessions: still not offered (ambiguous source).
        let other = worktree_target("/repo/wt-3", None);
        let items = applicable_actions(&[with_session, other]);
        assert!(!items
            .iter()
            .any(|(k, _)| *k == ActionKind::MoveClaudeSessionHere));
    }
}
