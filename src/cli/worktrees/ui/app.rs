//! The worktrees UI's event loop and mutable state (issue #1585 Phases 2–3):
//! the tree pane's cursor/marks and action popups, one embedded terminal
//! tab, and the single `tokio::select!` that merges crossterm input, PTY
//! events, and hub redraw signals.
//!
//! Extracted from `mod.rs` in Phase 3 — the point where PTY state gave the
//! type enough weight to justify its own file. Tabs/splits and mouse handling
//! (Phase 4) extend `App` further.

use std::path::{Path, PathBuf};
use std::time::Duration;

use alacritty_terminal::event::Event as TermEvent;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::TermMode;
use anyhow::Result;
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Block, Borders};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;

use super::actions::{self, ActionFlow, ActionKind, CheckReport, Dispatcher, Target};
use super::hub::{HubCommand, ViewModelHandle};
use super::keys::{self, ChromeKey, KeyRoute};
use super::terminal::{GridSize, TabEffect, TabId, TabKind, TerminalTab};
use super::tree::TreeState;
use super::view_model::WorktreesViewModel;
use super::{clipboard, popup, render, row_colors};
use crate::sessions::relocate::{self, RelocationMode};

/// Which pane receives keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Focus {
    #[default]
    Tree,
    Terminal,
}

/// Minimum interval between redraws while PTY output is streaming: a burst
/// of `Wakeup` events produces one frame, not one frame per event.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// The render layer's mutable state: cursor/marks, the current action-flow
/// popup, the Move/Copy-Claude-Session-Here picker wizard when active, and
/// the terminal tab. Everything here is local UI state, not daemon truth —
/// see `tree::TreeState`'s own doc comment.
struct App {
    tree: TreeState,
    flow: ActionFlow,
    menu: Option<popup::ActionMenu>,
    relocate: Option<RelocateStep>,
    focus: Focus,
    terminal: Option<TerminalTab>,
    next_tab_id: TabId,
    /// `q` with a live child asks first; the answer lands here.
    quit_confirm: bool,
    /// A one-line message for the status bar, cleared on the next key.
    notice: Option<String>,
    pty_tx: mpsc::UnboundedSender<(TabId, TermEvent)>,
    commands: mpsc::UnboundedSender<HubCommand>,
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

/// The event loop: one `select!` over crossterm input, the shared PTY event
/// channel, and the hub's view updates, redrawing at most every
/// [`FRAME_INTERVAL`] while anything is dirty. Quits on `q` from the tree
/// (after a confirm when a child is live).
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    mut handle: ViewModelHandle,
    dispatcher: Dispatcher,
    commands: mpsc::UnboundedSender<HubCommand>,
) -> Result<()> {
    let mut events = EventStream::new();
    let (pty_tx, mut pty_rx) = mpsc::unbounded_channel();
    let mut app = App {
        tree: TreeState::default(),
        flow: ActionFlow::Idle,
        menu: None,
        relocate: None,
        focus: Focus::Tree,
        terminal: None,
        next_tab_id: 1,
        quit_confirm: false,
        notice: None,
        pty_tx,
        commands,
    };
    let mut last_drawn_generation: Option<u64> = None;
    let mut dirty = true;
    let mut last_draw = tokio::time::Instant::now() - FRAME_INTERVAL;

    loop {
        let view = handle.view.borrow_and_update().clone();
        let generation_moved = last_drawn_generation != Some(view.generation);
        if (dirty || generation_moved) && last_draw.elapsed() >= FRAME_INTERVAL {
            terminal.draw(|frame| draw(frame, &view, &mut app))?;
            last_drawn_generation = Some(view.generation);
            dirty = false;
            last_draw = tokio::time::Instant::now();
        }
        // Coalesce: while dirty, wait out the rest of the frame interval;
        // otherwise a slow fallback tick keeps the status bar's
        // reconnecting/polling state fresh between real events.
        let wait = if dirty || generation_moved {
            FRAME_INTERVAL.saturating_sub(last_draw.elapsed())
        } else {
            Duration::from_millis(500)
        };

        tokio::select! {
            biased;
            ev = events.next() => {
                match ev {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        app.notice = None;
                        if handle_key(&mut app, &view, &dispatcher, key).await {
                            break;
                        }
                        dirty = true;
                    }
                    Some(Ok(Event::Paste(text))) => {
                        if app.focus == Focus::Terminal {
                            if let Some(tab) = app.terminal.as_ref().filter(|t| t.is_alive()) {
                                tab.write_input(keys::paste_bytes(&text, tab.mode()));
                            }
                        }
                    }
                    Some(Ok(Event::FocusGained)) => app.forward_focus(true),
                    Some(Ok(Event::FocusLost)) => app.forward_focus(false),
                    Some(Ok(Event::Resize(..))) => dirty = true,
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
            pty = pty_rx.recv() => {
                match pty {
                    Some((tab_id, event)) => {
                        if app.handle_pty_event(tab_id, event) {
                            dirty = true;
                        }
                    }
                    None => break,
                }
            }
            changed = handle.view.changed() => {
                if changed.is_err() {
                    break; // the hub actor is gone
                }
            }
            () = tokio::time::sleep(wait) => {}
        }
    }

    if let Some(tab) = app.terminal.as_mut() {
        tab.shutdown();
    }
    Ok(())
}

impl App {
    /// Absorbs one emulator event; returns whether a redraw is needed.
    fn handle_pty_event(&mut self, tab_id: TabId, event: TermEvent) -> bool {
        let Some(tab) = self.terminal.as_mut().filter(|t| t.id() == tab_id) else {
            return false; // an event from a tab that has already been closed
        };
        match tab.handle_event(event) {
            TabEffect::None => false,
            TabEffect::Redraw => true,
            TabEffect::CopyToClipboard(text) => {
                if clipboard::copy_text(&text).is_err() {
                    self.notice = Some("clipboard unavailable".to_string());
                }
                false
            }
            TabEffect::Exited => {
                self.notice = Some("terminal exited — alt-w to close the pane".to_string());
                true
            }
        }
    }

    /// Forwards host focus in/out to a child that asked for it
    /// (`TermMode::FOCUS_IN_OUT`), the way a real terminal would.
    fn forward_focus(&self, gained: bool) {
        if let Some(tab) = self.terminal.as_ref().filter(|t| t.is_alive()) {
            if tab.mode().contains(TermMode::FOCUS_IN_OUT) {
                tab.write_input(if gained {
                    b"\x1b[I".to_vec()
                } else {
                    b"\x1b[O".to_vec()
                });
            }
        }
    }

    /// Opens a tab of `kind` in `worktree`, or focuses the existing tab if
    /// it is that worktree's. Phase 3 hosts exactly one tab (no tab strip
    /// yet), so a live tab for a *different* worktree has to be closed
    /// first; an exited one is replaced.
    fn open_tab(&mut self, kind: TabKind, worktree: PathBuf, size: GridSize) {
        if let Some(existing) = &self.terminal {
            if existing.is_alive() {
                if existing.opened_in == worktree && existing.kind == kind {
                    self.focus = Focus::Terminal;
                } else {
                    self.notice =
                        Some("one tab this phase — alt-w closes the current one".to_string());
                }
                return;
            }
            self.close_tab();
        }
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        match TerminalTab::spawn(id, kind, worktree.clone(), size, self.pty_tx.clone()) {
            Ok(tab) => {
                self.terminal = Some(tab);
                self.focus = Focus::Terminal;
                let _ = self.commands.send(HubCommand::SetOpenTab(worktree));
            }
            Err(e) => self.notice = Some(format!("{e:#}")),
        }
    }

    fn close_tab(&mut self) {
        if let Some(mut tab) = self.terminal.take() {
            tab.shutdown();
            let _ = self.commands.send(HubCommand::ClearOpenTab(tab.opened_in));
        }
        self.focus = Focus::Tree;
    }

    fn cursor_worktree(&self, view: &WorktreesViewModel) -> Option<PathBuf> {
        match self.tree.targets_for_cursor_only(view).into_iter().next()? {
            Target::Worktree { path, .. } => Some(path),
            Target::Repo { root, .. } => Some(root),
        }
    }
}

// --- Layout and drawing ---------------------------------------------------

struct Areas {
    tree: Rect,
    terminal: Option<Rect>,
    status: Rect,
}

/// The tree on the left, the terminal pane on the right when a tab exists
/// (full width otherwise), and a one-line status bar.
fn layout(area: Rect, has_terminal: bool) -> Areas {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    if !has_terminal {
        return Areas {
            tree: rows[0],
            terminal: None,
            status: rows[1],
        };
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Min(20)])
        .split(rows[0]);
    Areas {
        tree: cols[0],
        terminal: Some(cols[1]),
        status: rows[1],
    }
}

fn draw(frame: &mut Frame<'_>, view: &WorktreesViewModel, app: &mut App) {
    let areas = layout(frame.area(), app.terminal.is_some());
    render::draw_tree_pane(frame, areas.tree, view, &app.tree, app.focus == Focus::Tree);
    if let (Some(area), Some(tab)) = (areas.terminal, app.terminal.as_mut()) {
        // Keep the emulator sized to the pane it is drawn in — a host resize
        // or a layout change lands here before the grid is read.
        let inner = Block::default().borders(Borders::ALL).inner(area);
        tab.resize(GridSize {
            cols: inner.width,
            lines: inner.height,
        });
        tab.draw(frame, area, app.focus == Focus::Terminal);
    }
    render::draw_status_bar(frame, areas.status, view, &app.tree, &status_hint(app));
    draw_popups(frame, app);
}

fn status_hint(app: &App) -> String {
    if let Some(notice) = &app.notice {
        return notice.clone();
    }
    match (&app.flow, app.focus) {
        (ActionFlow::Done { outcome }, _) => match outcome {
            actions::ActionOutcome::Done { summary } => summary.clone(),
            actions::ActionOutcome::BatchDone { results } => {
                let failed = results.iter().filter(|(_, r)| r.is_err()).count();
                if failed == 0 {
                    format!("done: {} target(s)", results.len())
                } else {
                    format!("{failed} of {} target(s) failed", results.len())
                }
            }
            actions::ActionOutcome::Failed { error } => format!("failed: {error}"),
        },
        (ActionFlow::Failed { error }, _) => format!("failed: {error}"),
        (_, Focus::Terminal) => {
            "alt-e tree  alt-w close tab  alt-c copy  ⇧PgUp/⇧PgDn scrollback".to_string()
        }
        (_, Focus::Tree) => {
            "↑↓ move  space mark  enter/alt-t shell tab  alt-⇧t claude tab  a actions  c/C colour  q quit".to_string()
        }
    }
}

fn draw_popups(frame: &mut Frame<'_>, app: &App) {
    let area = frame.area();
    if app.quit_confirm {
        let modal = popup::ConfirmModal {
            prompt: actions::ConfirmPrompt {
                title: "Quit?".to_string(),
                body_lines: vec![
                    "A terminal tab is still running; quitting sends it SIGHUP.".to_string()
                ],
                ..Default::default()
            },
        };
        popup::draw_confirm_modal(frame, area, &modal);
        return;
    }
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
}

fn relative_mtime(modified: std::time::SystemTime) -> String {
    match std::time::SystemTime::now().duration_since(modified) {
        Ok(elapsed) => format!("{}s ago", elapsed.as_secs()),
        Err(_) => "just now".to_string(),
    }
}

// --- Key handling -----------------------------------------------------------

/// Handles one key press. Returns `true` when the app should quit.
async fn handle_key(
    app: &mut App,
    view: &WorktreesViewModel,
    dispatcher: &Dispatcher,
    key: KeyEvent,
) -> bool {
    if app.quit_confirm {
        match key.code {
            KeyCode::Char('y') | KeyCode::Enter => return true,
            KeyCode::Char('n') | KeyCode::Esc => app.quit_confirm = false,
            _ => {}
        }
        return false;
    }

    // Chrome chords first, in every focus state and over every popup.
    if let Some(chrome) = keys::chrome_key(&key) {
        handle_chrome_key(app, view, chrome);
        return false;
    }

    // A focused, live terminal takes everything else verbatim.
    if app.focus == Focus::Terminal {
        match app.terminal.as_ref().filter(|t| t.is_alive()) {
            Some(tab) => {
                if let KeyRoute::Passthrough(bytes) = keys::route(&key, tab.mode()) {
                    tab.write_input(bytes);
                }
                return false;
            }
            None => app.focus = Focus::Tree, // the child exited; fall through
        }
    }

    let code = key.code;
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
    // the action menu / row-colour pickers / terminal tabs.
    match code {
        KeyCode::Char('q') => {
            if app.terminal.as_ref().is_some_and(TerminalTab::is_alive) {
                app.quit_confirm = true;
            } else {
                return true;
            }
        }
        // Esc clears an active multi-select first; only quits once nothing
        // is marked, so a stray Esc while reviewing a selection can't lose it.
        KeyCode::Esc => {
            if !app.tree.marked.is_empty() {
                app.tree.clear_marks();
            } else if app.terminal.as_ref().is_some_and(TerminalTab::is_alive) {
                app.quit_confirm = true;
            } else {
                return true;
            }
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
        KeyCode::Enter => open_tab_for_cursor(app, view, TabKind::Shell),
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

fn handle_chrome_key(app: &mut App, view: &WorktreesViewModel, chrome: ChromeKey) {
    match chrome {
        ChromeKey::FocusTree => app.focus = Focus::Tree,
        ChromeKey::FocusTerminal => {
            if app.terminal.is_some() {
                app.focus = Focus::Terminal;
            } else {
                app.notice = Some("no terminal tab — enter or alt-t opens one".to_string());
            }
        }
        ChromeKey::NewShellTab => open_tab_for_cursor(app, view, TabKind::Shell),
        ChromeKey::NewClaudeTab => open_tab_for_cursor(app, view, TabKind::Claude),
        ChromeKey::CloseTab => app.close_tab(),
        ChromeKey::Copy => {
            let text = app
                .terminal
                .as_ref()
                .and_then(TerminalTab::selection_to_string);
            app.notice = Some(match text {
                Some(text) if clipboard::copy_text(&text).is_ok() => "copied".to_string(),
                Some(_) => "clipboard unavailable".to_string(),
                None => "nothing selected".to_string(),
            });
        }
        ChromeKey::ScrollPageUp => {
            if let Some(tab) = &app.terminal {
                tab.scroll(Scroll::PageUp);
            }
        }
        ChromeKey::ScrollPageDown => {
            if let Some(tab) = &app.terminal {
                tab.scroll(Scroll::PageDown);
            }
        }
    }
}

/// Opens (or focuses) a `kind` tab in the cursor worktree. The initial size
/// is a placeholder — the first `draw` resizes the emulator to the real
/// pane before its grid is ever read.
fn open_tab_for_cursor(app: &mut App, view: &WorktreesViewModel, kind: TabKind) {
    let Some(worktree) = app.cursor_worktree(view) else {
        app.notice = Some("no row selected".to_string());
        return;
    };
    app.open_tab(
        kind,
        worktree,
        GridSize {
            cols: 80,
            lines: 24,
        },
    );
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
fn cursor_row_path(tree: &TreeState, view: &WorktreesViewModel) -> Option<PathBuf> {
    match tree.targets_for_cursor_only(view).first()? {
        Target::Repo { root, .. } => Some(root.clone()),
        Target::Worktree { path, .. } => Some(path.clone()),
    }
}

/// Starts the Move/Copy-Claude-Session-Here wizard from the cursor row (the
/// action menu only offers this action for a single worktree target with at
/// least one session — see `actions::applicable_actions` — so the cursor row
/// is unambiguous here).
fn start_relocate_flow(app: &mut App, view: &WorktreesViewModel, mode: RelocationMode) {
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

fn tree_only_target(tree: &TreeState, view: &WorktreesViewModel) -> Option<Target> {
    tree.targets_for_cursor_only(view).into_iter().next()
}

fn other_worktree_paths(view: &WorktreesViewModel, exclude: &Path) -> Vec<PathBuf> {
    view.repos
        .iter()
        .flat_map(|repo| repo.worktrees.iter())
        .map(|wt| wt.path.clone())
        .filter(|p| p != exclude)
        .collect()
}

fn handle_relocate_key(
    app: &mut App,
    view: &WorktreesViewModel,
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

fn advance_relocate(app: &mut App, view: &WorktreesViewModel, dispatcher: &Dispatcher) {
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn layout_gives_the_tree_the_full_width_until_a_tab_exists() {
        let area = Rect::new(0, 0, 100, 30);
        let without = layout(area, false);
        assert_eq!(without.tree.width, 100);
        assert!(without.terminal.is_none());
        assert_eq!(without.status.height, 1);

        let with = layout(area, true);
        let terminal = with.terminal.expect("a terminal pane");
        assert_eq!(with.tree.width + terminal.width, 100);
        assert!(terminal.width >= 20);
        assert_eq!(with.tree.height, 29);
    }
}
