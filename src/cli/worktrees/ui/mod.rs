//! `omni-dev worktrees ui` — a full-screen terminal UI for the worktrees tree
//! (issue #1585).
//!
//! Phase 1 was a read-only, live-updating view of every repo/worktree the
//! daemon knows about, superseding `worktrees tree` for interactive use.
//! Phase 2 (this code) adds row navigation/marking, an action menu, and the
//! daemon-free parity commands plus the two-phase close flow — pulling
//! forward the minimum slice of `tree.rs`/`popup.rs` needed to drive them
//! interactively. Embedded terminals, tabs/splits, mouse handling and the
//! full VS Code-parity surface stay in later phases — see the issue and
//! `/Users/jky/.claude/plans/unified-snacking-dragonfly.md` for the full plan.

mod actions;
mod ahead_behind;
mod client;
mod hub;
mod local_state;
mod popup;
mod render;
mod row_colors;
mod supervisor;
mod tree;
mod view_model;
mod wire;

use std::io::Stdout;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio_util::sync::CancellationToken;

use crate::daemon::server;
use crate::sessions::relocate::{self, RelocationMode};
use actions::{ActionFlow, ActionKind, CheckReport, Dispatcher, Target};
use client::WorktreesClient;
use hub::ViewModelHandle;
use tree::TreeState;

/// Launches the full-screen terminal UI for the worktrees tree.
#[derive(Parser)]
pub struct UiCommand {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl UiCommand {
    pub async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        let cancel = CancellationToken::new();
        let handle = hub::spawn(socket.clone(), cancel.clone());
        let dispatcher = Dispatcher::new(WorktreesClient::new(socket), handle.commands.clone());

        let mut guard = TerminalGuard::enter()?;
        let result = run(&mut guard.terminal, handle, dispatcher).await;
        drop(guard); // always restores the terminal, even on an error path
        cancel.cancel();
        result
    }
}

/// RAII guard: enters raw mode, the alternate screen, and mouse capture on
/// construction, and unconditionally restores the terminal on drop — so a
/// panic or an early `?` return never leaves the user's shell in raw mode.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    /// Undoes every step of `enter()` that already succeeded before a later
    /// step fails — `Drop` only runs for a fully-constructed `Self`, so a
    /// partial failure here (e.g. `execute!` after raw mode is already on)
    /// would otherwise leave the invoking shell in raw mode / the alternate
    /// screen with no guard left to clean it up.
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = std::io::stdout();
        if let Err(e) = execute!(stdout, EnterAlternateScreen, EnableMouseCapture) {
            let _ = disable_raw_mode();
            return Err(e.into());
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(e) => {
                let _ = execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
                let _ = disable_raw_mode();
                return Err(e.into());
            }
        };
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = self.terminal.show_cursor();
    }
}

/// The render layer's mutable state: cursor/marks, the current action-flow
/// popup, and (when active) the Move/Copy-Claude-Session-Here picker wizard.
/// Everything here is local UI state, not daemon truth — see
/// `tree::TreeState`'s own doc comment.
#[derive(Default)]
struct App {
    tree: TreeState,
    flow: ActionFlow,
    menu: Option<popup::ActionMenu>,
    relocate: Option<RelocateStep>,
}

/// The Move/Copy-Claude-Session-Here wizard's steps (issue #1585 Phase 2
/// §5): pick which session (skipped when the source row has exactly one),
/// then pick a destination worktree, then confirm. Kept out of the generic
/// [`ActionFlow`] since a relocation's parameters (a resolved session id
/// plus a source/destination pair) don't fit the uniform `targets: &[Target]`
/// fan-out shape every other action uses.
enum RelocateStep {
    PickSession {
        mode: RelocationMode,
        source_worktree: PathBuf,
        source_dir: PathBuf,
        sessions: Vec<actions::relocate_types::SessionInfo>,
        selected: usize,
    },
    PickDestination {
        mode: RelocationMode,
        source_dir: PathBuf,
        session: actions::relocate_types::SessionInfo,
        candidates: Vec<PathBuf>,
        selected: usize,
    },
    Confirm {
        mode: RelocationMode,
        source_dir: PathBuf,
        session: actions::relocate_types::SessionInfo,
        dest_worktree: PathBuf,
        prompt: actions::ConfirmPrompt,
    },
}

/// Phase 2's event loop: redraws on every hub update or local UI-state
/// change, quits on `q`/`Esc` (tree-focused, no popup open). Embedded
/// terminals, tabs/splits and mouse handling are later phases (issue #1585
/// §3-§5).
async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut handle: ViewModelHandle,
    dispatcher: Dispatcher,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut app = App::default();
    // The hub bumps `generation` on every rebuild, and `dirty` covers every
    // local UI-state change (cursor, marks, popups) that isn't reflected in
    // it — either one is enough reason to redraw.
    let mut last_drawn_generation: Option<u64> = None;
    let mut dirty = true;
    loop {
        let view = handle.view.borrow_and_update().clone();
        if dirty || last_drawn_generation != Some(view.generation) {
            terminal.draw(|frame| {
                render::draw(frame, &view, &app.tree);
                draw_popups(frame, &app);
            })?;
            last_drawn_generation = Some(view.generation);
            dirty = false;
        }

        tokio::select! {
            biased;
            ev = events.next() => {
                match ev {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        if handle_key(&mut app, &view, &dispatcher, key.code).await {
                            break;
                        }
                        dirty = true;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
            changed = handle.view.changed() => {
                if changed.is_err() {
                    break; // the hub actor is gone
                }
            }
            // A fallback tick so a "reconnecting"/"polling" status bar redraws
            // even between snapshots and keypresses.
            () = tokio::time::sleep(Duration::from_millis(500)) => {}
        }
    }
    Ok(())
}

fn draw_popups(frame: &mut ratatui::Frame<'_>, app: &App) {
    let area = frame.area();
    if let Some(menu) = &app.menu {
        popup::draw_action_menu(frame, area, menu);
        return;
    }
    match &app.relocate {
        Some(RelocateStep::PickSession {
            sessions, selected, ..
        }) => {
            let labels: Vec<String> = sessions
                .iter()
                .map(|s| format!("{}  ({})", s.id, relative_mtime(s.modified)))
                .collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            popup::draw_list_popup(frame, area, "Move which Claude session?", &refs, *selected);
            return;
        }
        Some(RelocateStep::PickDestination {
            candidates,
            selected,
            ..
        }) => {
            let labels: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            popup::draw_list_popup(frame, area, "Move to which worktree?", &refs, *selected);
            return;
        }
        Some(RelocateStep::Confirm { prompt, .. }) => {
            popup::draw_confirm_modal(
                frame,
                area,
                &popup::ConfirmModal {
                    prompt: prompt.clone(),
                },
            );
            return;
        }
        None => {}
    }
    if let ActionFlow::AwaitingConfirm { prompt, .. } = &app.flow {
        popup::draw_confirm_modal(
            frame,
            area,
            &popup::ConfirmModal {
                prompt: prompt.clone(),
            },
        );
    }
    // `Done`/`Failed` render as a one-line status footer rather than a modal
    // — see `handle_key`'s "any key dismisses" handling below.
}

fn relative_mtime(modified: std::time::SystemTime) -> String {
    match std::time::SystemTime::now().duration_since(modified) {
        Ok(elapsed) => format!("{}s ago", elapsed.as_secs()),
        Err(_) => "just now".to_string(),
    }
}

/// Handles one key press. Returns `true` when the app should quit.
async fn handle_key(
    app: &mut App,
    view: &view_model::WorktreesViewModel,
    dispatcher: &Dispatcher,
    code: KeyCode,
) -> bool {
    if app.relocate.is_some() {
        handle_relocate_key(app, view, dispatcher, code);
        return false;
    }
    if let Some(menu) = &mut app.menu {
        match code {
            KeyCode::Up | KeyCode::Char('k') => menu.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => menu.move_selection(1),
            KeyCode::Esc => app.menu = None,
            KeyCode::Enter => {
                if let Some(action) = menu.selected_action() {
                    app.menu = None;
                    match action {
                        ActionKind::MoveClaudeSessionHere | ActionKind::CopyClaudeSessionHere => {
                            let mode = if action == ActionKind::MoveClaudeSessionHere {
                                RelocationMode::Move
                            } else {
                                RelocationMode::Copy
                            };
                            start_relocate_flow(app, view, mode);
                        }
                        _ => {
                            let targets = app.tree.targets(view);
                            run_action(app, dispatcher, action, targets).await;
                        }
                    }
                }
            }
            _ => {}
        }
        return false;
    }

    match &app.flow {
        ActionFlow::AwaitingConfirm {
            action, targets, ..
        } => {
            match code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    let action = *action;
                    let targets = targets.clone();
                    app.flow = ActionFlow::Executing {
                        action,
                        targets: targets.clone(),
                    };
                    let outcome = dispatcher.execute(action, &targets).await;
                    app.flow = ActionFlow::Done { outcome };
                }
                KeyCode::Char('n') | KeyCode::Esc => app.flow = ActionFlow::Idle,
                _ => {}
            }
            return false;
        }
        ActionFlow::Done { .. } | ActionFlow::Failed { .. } => {
            app.flow = ActionFlow::Idle; // any key dismisses
            return false;
        }
        _ => {}
    }

    // Tree-focused, no popup: navigation, marking, and the entry points into
    // the action menu / row-colour pickers.
    match code {
        KeyCode::Char('q') => return true,
        // Esc clears an active multi-select first; only quits once nothing
        // is marked, so a stray Esc while reviewing a selection can't lose it.
        KeyCode::Esc => {
            if app.tree.marked.is_empty() {
                return true;
            }
            app.tree.clear_marks();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            let rows = TreeState::visible_rows(view).len();
            app.tree.move_cursor(-1, rows);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            let rows = TreeState::visible_rows(view).len();
            app.tree.move_cursor(1, rows);
        }
        KeyCode::Char(' ') => {
            if let Some(path) = cursor_row_path(&app.tree, view) {
                app.tree.toggle_mark(path);
            }
        }
        KeyCode::Char('a') => {
            let targets = app.tree.targets(view);
            let items = actions::applicable_actions(&targets)
                .into_iter()
                .map(|(action, label)| popup::MenuItem { action, label })
                .collect();
            app.menu = Some(popup::ActionMenu::new(items));
        }
        KeyCode::Char('c') => {
            let items = row_colors::KNOWN_ROW_COLORS
                .iter()
                .map(|&color| popup::MenuItem {
                    action: ActionKind::SetRowColor(color),
                    label: color,
                })
                .collect();
            app.menu = Some(popup::ActionMenu::new(items));
        }
        KeyCode::Char('C') => {
            let targets = app.tree.targets(view);
            let outcome = dispatcher
                .execute(ActionKind::ClearRowColor, &targets)
                .await;
            app.flow = ActionFlow::Done { outcome };
        }
        _ => {}
    }
    false
}

/// Runs the generic check→(confirm)→execute flow for every [`ActionKind`]
/// except the two session-relocation variants (routed separately — see
/// [`start_relocate_flow`]).
async fn run_action(
    app: &mut App,
    dispatcher: &Dispatcher,
    action: ActionKind,
    targets: Vec<Target>,
) {
    app.flow = ActionFlow::Checking {
        action,
        targets: targets.clone(),
    };
    match dispatcher.check(action, &targets).await {
        CheckReport::ProceedWithoutConfirm => {
            app.flow = ActionFlow::Executing {
                action,
                targets: targets.clone(),
            };
            let outcome = dispatcher.execute(action, &targets).await;
            app.flow = ActionFlow::Done { outcome };
        }
        CheckReport::NeedsConfirm { prompt, .. } => {
            app.flow = ActionFlow::AwaitingConfirm {
                action,
                targets,
                prompt,
            };
        }
        CheckReport::Refused { reason } => {
            app.flow = ActionFlow::Failed { error: reason };
        }
    }
}

/// The cursor row's path — a repo root or a worktree path — regardless of
/// mark state (used for `space` to toggle a mark on the row under the
/// cursor).
fn cursor_row_path(tree: &TreeState, view: &view_model::WorktreesViewModel) -> Option<PathBuf> {
    match tree.targets_for_cursor_only(view).first()? {
        Target::Repo { root, .. } => Some(root.clone()),
        Target::Worktree { path, .. } => Some(path.clone()),
    }
}

/// Starts the Move/Copy-Claude-Session-Here wizard from the cursor row (the
/// action menu only offers this action for a single worktree target with at
/// least one session — see `actions::applicable_actions` — so the cursor row
/// is unambiguous here).
fn start_relocate_flow(app: &mut App, view: &view_model::WorktreesViewModel, mode: RelocationMode) {
    let Some(Target::Worktree { path, .. }) = tree_only_target(&app.tree, view) else {
        app.flow = ActionFlow::Failed {
            error: "no worktree row selected".to_string(),
        };
        return;
    };
    let Some(source_dir) = relocate::project_dir_for(&path) else {
        app.flow = ActionFlow::Failed {
            error: "could not resolve the Claude projects directory".to_string(),
        };
        return;
    };
    let mut sessions = match relocate::enumerate_sessions(&source_dir) {
        Ok(sessions) => sessions,
        Err(e) => {
            app.flow = ActionFlow::Failed {
                error: format!("{e:#}"),
            };
            return;
        }
    };
    if sessions.is_empty() {
        app.flow = ActionFlow::Failed {
            error: "no Claude sessions found here".to_string(),
        };
        return;
    }
    if sessions.len() == 1 {
        let session = sessions.remove(0);
        let candidates = other_worktree_paths(view, &path);
        app.relocate = Some(RelocateStep::PickDestination {
            mode,
            source_dir,
            session,
            candidates,
            selected: 0,
        });
    } else {
        app.relocate = Some(RelocateStep::PickSession {
            mode,
            source_worktree: path,
            source_dir,
            sessions,
            selected: 0,
        });
    }
}

fn tree_only_target(tree: &TreeState, view: &view_model::WorktreesViewModel) -> Option<Target> {
    tree.targets_for_cursor_only(view).into_iter().next()
}

fn other_worktree_paths(view: &view_model::WorktreesViewModel, exclude: &Path) -> Vec<PathBuf> {
    view.repos
        .iter()
        .flat_map(|repo| repo.worktrees.iter())
        .map(|wt| wt.path.clone())
        .filter(|p| p != exclude)
        .collect()
}

fn handle_relocate_key(
    app: &mut App,
    view: &view_model::WorktreesViewModel,
    dispatcher: &Dispatcher,
    code: KeyCode,
) {
    match code {
        KeyCode::Esc => app.relocate = None,
        KeyCode::Up | KeyCode::Char('k') => move_relocate_selection(app, -1),
        KeyCode::Down | KeyCode::Char('j') => move_relocate_selection(app, 1),
        KeyCode::Enter | KeyCode::Char('y') => advance_relocate(app, view, dispatcher),
        KeyCode::Char('n') => {
            if matches!(app.relocate, Some(RelocateStep::Confirm { .. })) {
                app.relocate = None;
            }
        }
        _ => {}
    }
}

fn move_relocate_selection(app: &mut App, delta: isize) {
    match &mut app.relocate {
        Some(RelocateStep::PickSession {
            sessions, selected, ..
        }) => *selected = clamp_selection(*selected, delta, sessions.len()),
        Some(RelocateStep::PickDestination {
            candidates,
            selected,
            ..
        }) => *selected = clamp_selection(*selected, delta, candidates.len()),
        Some(RelocateStep::Confirm { .. }) | None => {}
    }
}

fn clamp_selection(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let max = len - 1;
    let current = current.min(max) as isize;
    (current + delta).clamp(0, max as isize) as usize
}

fn advance_relocate(app: &mut App, view: &view_model::WorktreesViewModel, dispatcher: &Dispatcher) {
    match app.relocate.take() {
        Some(RelocateStep::PickSession {
            mode,
            source_worktree,
            source_dir,
            mut sessions,
            selected,
        }) => {
            if selected >= sessions.len() {
                return;
            }
            let session = sessions.swap_remove(selected);
            let candidates = other_worktree_paths(view, &source_worktree);
            app.relocate = Some(RelocateStep::PickDestination {
                mode,
                source_dir,
                session,
                candidates,
                selected: 0,
            });
        }
        Some(RelocateStep::PickDestination {
            mode,
            source_dir,
            session,
            candidates,
            selected,
            ..
        }) => {
            let Some(dest_worktree) = candidates.into_iter().nth(selected) else {
                return;
            };
            match dispatcher.check_relocate_session(&source_dir, &session, &dest_worktree) {
                CheckReport::NeedsConfirm { prompt, .. } => {
                    app.relocate = Some(RelocateStep::Confirm {
                        mode,
                        source_dir,
                        session,
                        dest_worktree,
                        prompt,
                    });
                }
                CheckReport::Refused { reason } => {
                    app.flow = ActionFlow::Failed { error: reason };
                }
                CheckReport::ProceedWithoutConfirm => {
                    let outcome = dispatcher.execute_relocate_session(
                        &source_dir,
                        &session,
                        &dest_worktree,
                        mode,
                    );
                    app.flow = ActionFlow::Done { outcome };
                }
            }
        }
        Some(RelocateStep::Confirm {
            mode,
            source_dir,
            session,
            dest_worktree,
            ..
        }) => {
            let outcome =
                dispatcher.execute_relocate_session(&source_dir, &session, &dest_worktree, mode);
            app.flow = ActionFlow::Done { outcome };
        }
        None => {}
    }
}
