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
    /// Batch-rebase onto each repo's remote default branch, via the
    /// **daemon's** `rebase` op (ADR-0059) — never the CLI's local engine
    /// (ADR-0072 §9). Two-phase: plan, confirm, execute.
    Rebase,
    /// Publish the selected branches, via the daemon's `push` op
    /// (ADR-0061). Every force it issues is leased; there is no force
    /// option anywhere in this surface.
    Push,
    /// Enqueue the selected worktrees' PRs, via the daemon's `merge-queue`
    /// op (ADR-0056).
    MergeQueue,
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
            ActionKind::Rebase => self.check_rebase(targets).await,
            ActionKind::Push => self.check_push(targets).await,
            ActionKind::MergeQueue => self.check_merge_queue(targets).await,
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
            ActionKind::Rebase => self.execute_rebase(targets).await,
            ActionKind::Push => self.execute_push(targets).await,
            ActionKind::MergeQueue => self.execute_merge_queue(targets).await,
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

    // --- Git write actions (daemon ops) --------------------------------
    //
    // All three follow `close`'s two-phase shape: phase 1 asks the daemon
    // to plan, the plan is rendered into the confirm modal verbatim, and
    // phase 2 executes only after the user agrees. The daemon re-plans from
    // scratch on execute, so what is confirmed is advisory, not a token.
    //
    // Every safety rule lives in the daemon, deliberately: leased-force
    // only, `--force-if-includes`, and never force-pushing a repository's
    // default branch (ADR-0059/ADR-0061). Nothing here can weaken them, and
    // no force option is offered anywhere in this surface.

    async fn check_rebase(&self, targets: &[Target]) -> CheckReport {
        let paths = worktree_paths(targets);
        if paths.is_empty() {
            return CheckReport::Refused {
                reason: "select a worktree row to rebase".to_string(),
            };
        }
        match self.client.rebase(&paths, true).await {
            Err(e) => CheckReport::Refused {
                reason: format!("{e:#}"),
            },
            Ok(plan) => {
                let pending: Vec<&super::wire::RebaseWorktreeWire> = plan
                    .worktrees
                    .iter()
                    .filter(|w| w.result == "would-rebase")
                    .collect();
                if pending.is_empty() {
                    return CheckReport::Refused {
                        reason: rebase_nothing_to_do(&plan),
                    };
                }
                let mut body_lines = Vec::new();
                for w in &pending {
                    let behind = w.behind.unwrap_or(0);
                    body_lines.push(format!(
                        "{} ({}) — {behind} behind {}",
                        w.path.display(),
                        w.branch.as_deref().unwrap_or("(detached)"),
                        w.onto
                    ));
                }
                let mut risk_lines = vec![
                    "Rebasing rewrites these branches' history.".to_string(),
                    "A conflict leaves that worktree mid-rebase to resolve in place.".to_string(),
                ];
                for f in plan.fetches.iter().filter(|f| !f.ok) {
                    risk_lines.push(format!(
                        "fetch of {} failed for {}: {}",
                        f.onto,
                        f.repo_root.display(),
                        f.error.as_deref().unwrap_or("unknown error")
                    ));
                }
                // A repository whose onto ref was resolved locally is never
                // fetched; say so, since "not fetched" reads as a failure.
                for f in plan.fetches.iter().filter(|f| f.ok && !f.fetched) {
                    body_lines.push(format!("{} resolved locally (no fetch)", f.onto));
                }
                CheckReport::NeedsConfirm {
                    prompt: ConfirmPrompt {
                        title: format!("Rebase {} worktree(s)?", pending.len()),
                        body_lines,
                        risk_lines,
                        info_lines: skipped_lines(&plan),
                    },
                    has_risk: true,
                }
            }
        }
    }

    async fn execute_rebase(&self, targets: &[Target]) -> ActionOutcome {
        let paths = worktree_paths(targets);
        match self.client.rebase(&paths, false).await {
            Err(e) => ActionOutcome::Failed {
                error: format!("{e:#}"),
            },
            Ok(reply) => ActionOutcome::BatchDone {
                results: reply
                    .worktrees
                    .iter()
                    .map(|w| {
                        let outcome = match w.result.as_str() {
                            "rebased" | "up-to-date" => Ok(()),
                            "conflict" => Err(format!(
                                "conflict{}: {}",
                                if w.left_in_place {
                                    " (left mid-rebase)"
                                } else {
                                    " (aborted)"
                                },
                                w.detail.as_deref().unwrap_or("")
                            )),
                            "fetch-failed" => Err(format!(
                                "fetch failed: {}",
                                w.detail.as_deref().unwrap_or("")
                            )),
                            "skipped" => {
                                Err(format!("skipped: {}", w.reason.as_deref().unwrap_or("")))
                            }
                            other => Err(other.to_string()),
                        };
                        (w.path.clone(), outcome)
                    })
                    .collect(),
            },
        }
    }

    async fn check_push(&self, targets: &[Target]) -> CheckReport {
        let paths = worktree_paths(targets);
        if paths.is_empty() {
            return CheckReport::Refused {
                reason: "select a worktree row to push".to_string(),
            };
        }
        match self.client.push(&paths, true).await {
            Err(e) => CheckReport::Refused {
                reason: format!("{e:#}"),
            },
            Ok(plan) => {
                let pending: Vec<&super::wire::PushWorktreeWire> = plan
                    .worktrees
                    .iter()
                    .filter(|w| {
                        matches!(
                            w.result.as_str(),
                            "would-fast-forward" | "would-force" | "would-create"
                        )
                    })
                    .collect();
                if pending.is_empty() {
                    return CheckReport::Refused {
                        reason: "nothing to push: every selected branch is up to date or skipped"
                            .to_string(),
                    };
                }
                let mut body_lines = Vec::new();
                let mut forced = 0usize;
                for w in &pending {
                    let branch = w.branch.as_deref().unwrap_or("(detached)");
                    let target = format!("{}/{}", w.remote, w.remote_branch);
                    body_lines.push(match w.result.as_str() {
                        "would-force" => {
                            forced += 1;
                            format!(
                                "{branch} → {target} — force-with-lease (+{} -{})",
                                w.ahead.unwrap_or(0),
                                w.behind.unwrap_or(0)
                            )
                        }
                        "would-create" => format!("{branch} → {target} — new upstream"),
                        _ => format!(
                            "{branch} → {target} — fast-forward (+{})",
                            w.ahead.unwrap_or(0)
                        ),
                    });
                }
                let mut risk_lines = Vec::new();
                if forced > 0 {
                    risk_lines.push(format!(
                        "{forced} branch(es) publish rewritten history, with a lease."
                    ));
                    risk_lines.push(
                        "The lease is enforced by git: a remote that moved is refused, \
                         never overwritten."
                            .to_string(),
                    );
                }
                CheckReport::NeedsConfirm {
                    prompt: ConfirmPrompt {
                        title: format!("Push {} branch(es)?", pending.len()),
                        body_lines,
                        risk_lines,
                        info_lines: push_skipped_lines(&plan),
                    },
                    has_risk: forced > 0,
                }
            }
        }
    }

    async fn execute_push(&self, targets: &[Target]) -> ActionOutcome {
        let paths = worktree_paths(targets);
        match self.client.push(&paths, false).await {
            Err(e) => ActionOutcome::Failed {
                error: format!("{e:#}"),
            },
            Ok(reply) => ActionOutcome::BatchDone {
                results: reply
                    .worktrees
                    .iter()
                    .map(|w| {
                        let outcome = match w.result.as_str() {
                            // `forced` distinguishes a leased rewrite from an
                            // ordinary fast-forward; both succeeded, but the
                            // user should know which happened.
                            "pushed" if w.forced => Ok(()),
                            "pushed" | "created" | "up-to-date" => Ok(()),
                            "rejected" if w.stale => Err(format!(
                                "lease refused — the remote moved; fetch and rebase: {}",
                                w.detail.as_deref().unwrap_or("")
                            )),
                            "rejected" => {
                                Err(format!("rejected: {}", w.detail.as_deref().unwrap_or("")))
                            }
                            "skipped" => {
                                Err(format!("skipped: {}", w.reason.as_deref().unwrap_or("")))
                            }
                            other => Err(other.to_string()),
                        };
                        (w.path.clone(), outcome)
                    })
                    .collect(),
            },
        }
    }

    async fn check_merge_queue(&self, targets: &[Target]) -> CheckReport {
        let paths = worktree_paths(targets);
        if paths.is_empty() {
            return CheckReport::Refused {
                reason: "select a worktree row to enqueue".to_string(),
            };
        }
        match self.client.merge_queue(&paths, true).await {
            Err(e) => CheckReport::Refused {
                reason: format!("{e:#}"),
            },
            Ok(report) => {
                if report.eligible.is_empty() {
                    return CheckReport::Refused {
                        reason: merge_queue_nothing_eligible(&report),
                    };
                }
                CheckReport::NeedsConfirm {
                    prompt: ConfirmPrompt {
                        title: format!("Enqueue {} pull request(s)?", report.eligible.len()),
                        body_lines: report
                            .eligible
                            .iter()
                            .map(|pr| {
                                format!(
                                    "#{} {} — {}{}",
                                    pr.number,
                                    pr.branch,
                                    pr.url,
                                    if pr.already_queued {
                                        " (already queued)"
                                    } else {
                                        ""
                                    }
                                )
                            })
                            .collect(),
                        risk_lines: Vec::new(),
                        info_lines: report
                            .skipped
                            .iter()
                            .map(|s| {
                                let pr = s.number.map(|n| format!("#{n} ")).unwrap_or_default();
                                format!("skipped {pr}{}: {} ({})", s.path, s.detail, s.kind)
                            })
                            .collect(),
                    },
                    has_risk: false,
                }
            }
        }
    }

    async fn execute_merge_queue(&self, targets: &[Target]) -> ActionOutcome {
        let paths = worktree_paths(targets);
        match self.client.merge_queue(&paths, false).await {
            Err(e) => ActionOutcome::Failed {
                error: format!("{e:#}"),
            },
            Ok(reply) => {
                let mut results: Vec<(PathBuf, Result<(), String>)> = reply
                    .queued
                    .iter()
                    .map(|pr| (PathBuf::from(&pr.path), Ok(())))
                    .collect();
                results.extend(
                    reply
                        .failed
                        .iter()
                        .map(|f| (PathBuf::from(&f.path), Err(f.detail.clone()))),
                );
                ActionOutcome::BatchDone { results }
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
/// The worktree paths in `targets`, ignoring repo header rows — what the
/// three git ops address (they act on worktrees, not repositories).
fn worktree_paths(targets: &[Target]) -> Vec<PathBuf> {
    targets
        .iter()
        .filter_map(|t| match t {
            Target::Worktree { path, .. } => Some(path.clone()),
            Target::Repo { .. } => None,
        })
        .collect()
}

/// Why a rebase plan has nothing pending — the difference between "already
/// up to date" and "every fetch failed" matters to the user.
fn rebase_nothing_to_do(plan: &super::wire::RebaseReplyWire) -> String {
    let failed: Vec<&super::wire::RebaseFetchWire> =
        plan.fetches.iter().filter(|f| !f.ok).collect();
    if !failed.is_empty() && plan.fetches.len() == failed.len() {
        return format!(
            "could not fetch: {}",
            failed
                .iter()
                .map(|f| f.error.as_deref().unwrap_or("unknown error"))
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    "nothing to rebase: every selected worktree is already up to date or skipped".to_string()
}

/// The skipped/blocked rows of a rebase plan, as confirm-modal info lines.
fn skipped_lines(plan: &super::wire::RebaseReplyWire) -> Vec<String> {
    plan.worktrees
        .iter()
        .filter(|w| w.result != "would-rebase")
        .map(|w| {
            format!(
                "{}: {}{}",
                w.path.display(),
                w.result,
                w.reason
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default()
            )
        })
        .collect()
}

/// The non-pending rows of a push plan, as confirm-modal info lines.
fn push_skipped_lines(plan: &super::wire::PushReplyWire) -> Vec<String> {
    plan.worktrees
        .iter()
        .filter(|w| {
            !matches!(
                w.result.as_str(),
                "would-fast-forward" | "would-force" | "would-create"
            )
        })
        .map(|w| {
            format!(
                "{}: {}{}",
                w.path.display(),
                w.result,
                w.reason
                    .as_deref()
                    .map(|r| format!(" ({r})"))
                    .unwrap_or_default()
            )
        })
        .collect()
}

/// Why a merge-queue check found nothing eligible, naming the reasons the
/// daemon gave rather than a bare "nothing to do".
fn merge_queue_nothing_eligible(report: &super::wire::MergeQueueReplyWire) -> String {
    if report.skipped.is_empty() {
        return "no eligible pull requests in the selection".to_string();
    }
    format!(
        "no eligible pull requests: {}",
        report
            .skipped
            .iter()
            .map(|s| format!("{} ({})", s.detail, s.kind))
            .collect::<Vec<_>>()
            .join("; ")
    )
}

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

    // 4_git — the group Phase 2 left deliberately empty. Every one of these
    // drives a daemon op and is two-phase; the safety rules that matter
    // (leased force only, never force the default branch) live in the
    // daemon, not here.
    if targets.iter().any(|t| matches!(t, Target::Worktree { .. })) {
        items.push((ActionKind::Rebase, "Rebase on main"));
        items.push((ActionKind::Push, "Push (force-with-lease)"));
        items.push((ActionKind::MergeQueue, "Add to Merge Queue"));
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

    use serde_json::json;

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

    /// A fake daemon that records the request line it received and answers
    /// with `reply`. The shared `fake_daemon_reply` discards the request,
    /// but the payload is exactly what the force-option tests assert on.
    fn capturing_daemon(
        reply: serde_json::Value,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        tokio::sync::oneshot::Receiver<String>,
    ) {
        use futures::{SinkExt, StreamExt};
        use tokio::net::UnixListener;
        use tokio_util::codec::{Framed, LinesCodec};

        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(stream, LinesCodec::new());
            let req = framed.next().await.unwrap().unwrap();
            let _ = tx.send(req);
            framed
                .send(serde_json::to_string(&reply).unwrap())
                .await
                .unwrap();
        });
        (dir, sock, rx)
    }

    fn dispatcher_on(sock: &Path) -> (Dispatcher, mpsc::UnboundedReceiver<HubCommand>) {
        let (commands, rx) = mpsc::unbounded_channel();
        (
            Dispatcher::new(WorktreesClient::new(sock.to_path_buf()), commands),
            rx,
        )
    }

    #[tokio::test]
    async fn rebase_check_plans_and_lists_only_the_pending_worktrees() {
        let reply = json!({ "ok": true, "payload": {
            "fetches": [{ "repo_root": "/repo", "onto": "origin/main", "fetched": true, "ok": true }],
            "worktrees": [
                { "path": "/repo/a", "branch": "feat-a", "onto": "origin/main",
                  "result": "would-rebase", "behind": 3 },
                { "path": "/repo/b", "branch": "feat-b", "onto": "origin/main",
                  "result": "up-to-date" },
                { "path": "/repo/c", "branch": "feat-c", "onto": "origin/main",
                  "result": "skipped", "reason": "dirty" }
            ]
        }});
        let (_dir, sock, req) = capturing_daemon(reply);
        let (dispatcher, _cmds) = dispatcher_on(&sock);
        let targets = vec![
            worktree_target("/repo/a", None),
            worktree_target("/repo/b", None),
            worktree_target("/repo/c", None),
        ];
        let report = dispatcher.check(ActionKind::Rebase, &targets).await;
        match report {
            CheckReport::NeedsConfirm { prompt, has_risk } => {
                assert!(has_risk, "rewriting history is a risk");
                assert!(prompt.title.contains("1 worktree"));
                assert_eq!(prompt.body_lines.len(), 1, "only the pending one");
                assert!(prompt.body_lines[0].contains("feat-a"));
                assert!(prompt.body_lines[0].contains("3 behind"));
                assert!(prompt.risk_lines.iter().any(|l| l.contains("rewrites")));
                // The non-pending rows are reported, not hidden.
                assert_eq!(prompt.info_lines.len(), 2);
                assert!(prompt.info_lines.iter().any(|l| l.contains("dirty")));
            }
            other => panic!("expected NeedsConfirm, got {other:?}"),
        }

        // The plan phase must be check-only, and must never ask for force.
        let sent: serde_json::Value = serde_json::from_str(&req.await.unwrap()).unwrap();
        let payload = &sent["payload"];
        assert_eq!(sent["op"], "rebase");
        assert_eq!(payload["check"], true);
        assert_eq!(payload["confirmed"], false);
        assert!(
            payload.get("force").is_none(),
            "no force field is ever sent"
        );
        assert!(payload.get("onto").is_none(), "the UI never overrides onto");
    }

    #[tokio::test]
    async fn rebase_check_refuses_when_every_fetch_failed_and_says_why() {
        let reply = json!({ "ok": true, "payload": {
            "fetches": [{ "repo_root": "/repo", "onto": "origin/main", "fetched": true,
                          "ok": false, "error": "ssh: no key" }],
            "worktrees": [{ "path": "/repo/a", "onto": "origin/main",
                            "result": "fetch-failed", "detail": "ssh: no key" }]
        }});
        let (_dir, sock, _req) = capturing_daemon(reply);
        let (dispatcher, _cmds) = dispatcher_on(&sock);
        let targets = vec![worktree_target("/repo/a", None)];
        match dispatcher.check(ActionKind::Rebase, &targets).await {
            CheckReport::Refused { reason } => assert!(reason.contains("ssh: no key"), "{reason}"),
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn rebase_execute_reports_per_target_outcomes_including_a_left_conflict() {
        let reply = json!({ "ok": true, "payload": { "fetches": [], "worktrees": [
            { "path": "/repo/a", "onto": "origin/main", "result": "rebased", "behind": 2 },
            { "path": "/repo/b", "onto": "origin/main", "result": "conflict",
              "detail": "CONFLICT in x.rs", "left_in_place": true }
        ]}});
        let (_dir, sock, req) = capturing_daemon(reply);
        let (dispatcher, _cmds) = dispatcher_on(&sock);
        let targets = vec![
            worktree_target("/repo/a", None),
            worktree_target("/repo/b", None),
        ];
        match dispatcher.execute(ActionKind::Rebase, &targets).await {
            ActionOutcome::BatchDone { results } => {
                assert_eq!(results.len(), 2);
                assert!(results[0].1.is_ok());
                let err = results[1].1.as_ref().unwrap_err();
                assert!(err.contains("left mid-rebase"), "{err}");
                assert!(err.contains("CONFLICT in x.rs"), "{err}");
            }
            other => panic!("expected BatchDone, got {other:?}"),
        }
        let sent: serde_json::Value = serde_json::from_str(&req.await.unwrap()).unwrap();
        assert_eq!(sent["payload"]["confirmed"], true);
        assert_eq!(sent["payload"]["check"], false);
        // Conflicts are left in place to resolve, matching the tree view.
        assert_eq!(sent["payload"]["keep_conflicts"], true);
    }

    #[tokio::test]
    async fn push_check_distinguishes_a_lease_from_a_fast_forward() {
        let reply = json!({ "ok": true, "payload": { "worktrees": [
            { "path": "/repo/a", "branch": "feat-a", "remote": "origin",
              "remote_branch": "feat-a", "result": "would-force", "ahead": 4, "behind": 2 },
            { "path": "/repo/b", "branch": "feat-b", "remote": "origin",
              "remote_branch": "feat-b", "result": "would-fast-forward", "ahead": 1 },
            { "path": "/repo/c", "branch": "feat-c", "remote": "origin",
              "remote_branch": "feat-c", "result": "up-to-date" }
        ]}});
        let (_dir, sock, req) = capturing_daemon(reply);
        let (dispatcher, _cmds) = dispatcher_on(&sock);
        let targets = vec![
            worktree_target("/repo/a", None),
            worktree_target("/repo/b", None),
            worktree_target("/repo/c", None),
        ];
        match dispatcher.check(ActionKind::Push, &targets).await {
            CheckReport::NeedsConfirm { prompt, has_risk } => {
                assert!(has_risk, "a leased force is a risk");
                assert_eq!(prompt.body_lines.len(), 2);
                assert!(prompt.body_lines[0].contains("force-with-lease"));
                assert!(prompt.body_lines[1].contains("fast-forward"));
                assert!(prompt
                    .risk_lines
                    .iter()
                    .any(|l| l.contains("lease is enforced by git")));
                assert!(prompt.info_lines.iter().any(|l| l.contains("up-to-date")));
            }
            other => panic!("expected NeedsConfirm, got {other:?}"),
        }

        // The load-bearing assertion of ADR-0061: the UI cannot ask for a
        // bare force, because it never sends one.
        let sent: serde_json::Value = serde_json::from_str(&req.await.unwrap()).unwrap();
        let payload = &sent["payload"];
        assert_eq!(sent["op"], "push");
        assert!(payload.get("force").is_none());
        assert!(payload.get("force_with_lease").is_none());
        assert!(payload.get("remote").is_none(), "no remote override");
    }

    #[tokio::test]
    async fn push_execute_names_a_refused_lease_as_such() {
        let reply = json!({ "ok": true, "payload": { "worktrees": [
            { "path": "/repo/a", "branch": "a", "remote": "origin", "remote_branch": "a",
              "result": "pushed", "forced": true },
            { "path": "/repo/b", "branch": "b", "remote": "origin", "remote_branch": "b",
              "result": "rejected", "detail": "stale info", "stale": true }
        ]}});
        let (_dir, sock, _req) = capturing_daemon(reply);
        let (dispatcher, _cmds) = dispatcher_on(&sock);
        let targets = vec![
            worktree_target("/repo/a", None),
            worktree_target("/repo/b", None),
        ];
        match dispatcher.execute(ActionKind::Push, &targets).await {
            ActionOutcome::BatchDone { results } => {
                assert!(results[0].1.is_ok());
                let err = results[1].1.as_ref().unwrap_err();
                // The user is told the fix is a fetch and rebase, not a
                // harder push — there is no harder push to reach for.
                assert!(err.contains("lease refused"), "{err}");
                assert!(err.contains("fetch and rebase"), "{err}");
            }
            other => panic!("expected BatchDone, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn merge_queue_check_lists_eligible_prs_and_refuses_when_none_are() {
        let reply = json!({ "ok": true, "payload": {
            "eligible": [{ "path": "/repo/a", "number": 42, "url": "u", "branch": "feat-a" }],
            "skipped": [{ "path": "/repo/b", "kind": "no-pr", "detail": "no open PR" }]
        }});
        let (_dir, sock, _req) = capturing_daemon(reply);
        let (dispatcher, _cmds) = dispatcher_on(&sock);
        let targets = vec![
            worktree_target("/repo/a", None),
            worktree_target("/repo/b", None),
        ];
        match dispatcher.check(ActionKind::MergeQueue, &targets).await {
            CheckReport::NeedsConfirm { prompt, has_risk } => {
                assert!(!has_risk, "enqueueing is not destructive");
                assert!(prompt.body_lines[0].contains("#42"));
                assert!(prompt.info_lines[0].contains("no open PR"));
            }
            other => panic!("expected NeedsConfirm, got {other:?}"),
        }

        let none = json!({ "ok": true, "payload": { "eligible": [],
            "skipped": [{ "path": "/repo/b", "kind": "no-pr", "detail": "no open PR" }] }});
        let (_dir2, sock2, _req2) = capturing_daemon(none);
        let (dispatcher, _cmds) = dispatcher_on(&sock2);
        match dispatcher.check(ActionKind::MergeQueue, &targets).await {
            CheckReport::Refused { reason } => assert!(reason.contains("no open PR"), "{reason}"),
            other => panic!("expected Refused, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_git_actions_refuse_a_selection_with_no_worktree_rows() {
        // A repo header alone is not a valid target for any of the three:
        // they act on worktrees. No daemon is contacted at all.
        let (dispatcher, _cmds) = dispatcher_on(Path::new("/tmp/nonexistent-4d.sock"));
        let targets = vec![repo_target("/repo", None)];
        for action in [ActionKind::Rebase, ActionKind::Push, ActionKind::MergeQueue] {
            match dispatcher.check(action, &targets).await {
                CheckReport::Refused { reason } => {
                    assert!(reason.contains("select a worktree"), "{action:?}: {reason}");
                }
                other => panic!("{action:?}: expected Refused, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_git_actions_are_offered_only_for_worktree_rows() {
        let git = [ActionKind::Rebase, ActionKind::Push, ActionKind::MergeQueue];
        let repo_only = applicable_actions(&[repo_target("/repo", None)]);
        for action in git {
            assert!(
                !repo_only.iter().any(|(a, _)| *a == action),
                "{action:?} offered on a repo row"
            );
        }
        let with_worktree = applicable_actions(&[worktree_target("/repo/a", None)]);
        for action in git {
            assert!(
                with_worktree.iter().any(|(a, _)| *a == action),
                "{action:?} missing for a worktree row"
            );
        }
    }

    /// The surface-level half of ADR-0061's guarantee: no force escape
    /// hatch exists anywhere in this module or the client it calls. The
    /// daemon enforces the lease; this keeps the *UI* from ever asking to
    /// bypass it, which is the part a reviewer of this crate can check.
    #[test]
    fn no_force_escape_hatch_exists_in_the_ui_surface() {
        let sources = [
            ("actions.rs", include_str!("actions.rs")),
            ("client.rs", include_str!("client.rs")),
            ("wire.rs", include_str!("wire.rs")),
        ];
        for (name, source) in sources {
            // Only production code: the tests below deliberately assert on
            // the *absence* of a force field, so they name it.
            let code_only = source.split("#[cfg(test)]").next().unwrap_or(source);
            for (number, line) in code_only.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") || code.starts_with("///") {
                    continue; // prose may discuss force; code may not request it
                }
                let asks_for_force = code.contains("\"force\"")
                    || code.contains("--force\"")
                    || code.contains("force: true")
                    || code.contains("\"no_verify\"");
                assert!(
                    !asks_for_force,
                    "{name}:{}: the UI must never request a force: {line}",
                    number + 1
                );
            }
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
