//! The worktrees UI's event loop and mutable state (issue #1585 Phases 2–3):
//! the tree pane's cursor/marks and action popups, one embedded terminal
//! tab, and the single `tokio::select!` that merges crossterm input, PTY
//! events, and hub redraw signals.
//!
//! Extracted from `mod.rs` in Phase 3 — the point where PTY state gave the
//! type enough weight to justify its own file. Phase 4 adds the mouse
//! contract (`handle_mouse`, applying what `mouse.rs` decides); tabs/splits
//! extend `App` further.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alacritty_terminal::event::Event as TermEvent;
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::TermMode;
use anyhow::Result;
use crossterm::event::{
    Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Block, Borders};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;

use super::actions::{self, ActionFlow, ActionKind, CheckReport, Dispatcher, Target};
use super::hub::{HubCommand, ViewModelHandle};
use super::keys::{self, ChromeKey, KeyRoute};
use super::mouse::{self, DragOrigin, Hit, RegionMap, TreeClick, WheelRoute};
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
    /// The hit-testable regions of the last drawn frame.
    regions: RegionMap,
    /// The drag in progress, if any, and the region it is clamped to.
    drag: DragOrigin,
    clicks: mouse::ClickTracker,
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
        regions: RegionMap::default(),
        drag: DragOrigin::None,
        clicks: mouse::ClickTracker::default(),
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
                    Some(Ok(Event::Mouse(mouse))) => {
                        if handle_mouse(&mut app, &view, mouse) {
                            dirty = true;
                        }
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
    render::draw_tree_pane(
        frame,
        areas.tree,
        view,
        &mut app.tree,
        app.focus == Focus::Tree,
    );
    let terminal_inner = match (areas.terminal, app.terminal.as_mut()) {
        (Some(area), Some(tab)) => {
            // Keep the emulator sized to the pane it is drawn in — a host
            // resize or a layout change lands here before the grid is read.
            let inner = Block::default().borders(Borders::ALL).inner(area);
            tab.resize(GridSize {
                cols: inner.width,
                lines: inner.height,
            });
            tab.draw(frame, area, app.focus == Focus::Terminal);
            Some(inner)
        }
        _ => None,
    };
    // The region map mirrors exactly what was just drawn, offset included.
    app.regions = RegionMap {
        tree: Block::default().borders(Borders::ALL).inner(areas.tree),
        tree_offset: app.tree.offset,
        terminal: terminal_inner,
    };
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

// --- Mouse handling ---------------------------------------------------------

/// Handles one mouse event under the contract in `mouse.rs`. Returns
/// whether a redraw is needed. Popups are chrome, so the mouse is inert
/// while one is open.
fn handle_mouse(app: &mut App, view: &WorktreesViewModel, mouse: MouseEvent) -> bool {
    if app.quit_confirm
        || app.menu.is_some()
        || app.relocate.is_some()
        || matches!(app.flow, ActionFlow::AwaitingConfirm { .. })
    {
        return false;
    }
    let (col, row, mods) = (mouse.column, mouse.row, mouse.modifiers);
    match mouse.kind {
        MouseEventKind::Down(button) => {
            app.notice = None;
            match app.regions.hit(col, row) {
                Hit::Tree { row: tree_row } => {
                    tree_mouse_down(app, view, tree_row, button, mods, (col, row))
                }
                Hit::Terminal { col: gc, line } => {
                    terminal_mouse_down(app, button, mods, (gc, line), (col, row))
                }
                Hit::Chrome => false,
            }
        }
        MouseEventKind::Drag(button) => match app.drag {
            DragOrigin::Tree { anchor } => {
                let rows = TreeState::visible_rows(view).len();
                app.tree.set_cursor(app.regions.tree_row_clamped(row), rows);
                app.tree.mark_range(view, anchor, app.tree.cursor);
                true
            }
            DragOrigin::Terminal => {
                let Some((gc, line)) = app.regions.clamp_to_terminal(col, row) else {
                    return false;
                };
                if let Some(tab) = &app.terminal {
                    tab.selection_update(gc, line);
                }
                true
            }
            DragOrigin::Child => {
                forward_to_child(app, MouseEventKind::Drag(button), mods, (col, row));
                false
            }
            DragOrigin::None => false,
        },
        MouseEventKind::Up(button) => {
            if std::mem::take(&mut app.drag) == DragOrigin::Child {
                forward_to_child(app, MouseEventKind::Up(button), mods, (col, row));
            }
            false
        }
        MouseEventKind::Moved => {
            // Only a child that asked for all-motion reporting cares.
            if matches!(app.regions.hit(col, row), Hit::Terminal { .. }) {
                forward_to_child(app, MouseEventKind::Moved, mods, (col, row));
            }
            false
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let up = mouse.kind == MouseEventKind::ScrollUp;
            match app.regions.hit(col, row) {
                Hit::Tree { .. } => {
                    let rows = TreeState::visible_rows(view).len();
                    app.tree.move_cursor(if up { -1 } else { 1 }, rows);
                    true
                }
                Hit::Terminal { col: gc, line } => {
                    let Some(tab) = &app.terminal else {
                        return false;
                    };
                    match mouse::route_wheel(up, mods, gc, line, tab.mode()) {
                        WheelRoute::Forward(bytes) | WheelRoute::ArrowKeys(bytes) => {
                            if tab.is_alive() {
                                tab.write_input(bytes);
                            }
                            false
                        }
                        WheelRoute::ScrollDisplay(lines) => {
                            tab.scroll(Scroll::Delta(lines));
                            true
                        }
                    }
                }
                Hit::Chrome => false,
            }
        }
        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
            if matches!(app.regions.hit(col, row), Hit::Terminal { .. }) {
                forward_to_child(app, mouse.kind, mods, (col, row));
            }
            false
        }
    }
}

/// A button-down on tree row `tree_row` (which may be past the last row).
fn tree_mouse_down(
    app: &mut App,
    view: &WorktreesViewModel,
    tree_row: usize,
    button: MouseButton,
    mods: KeyModifiers,
    (col, row): (u16, u16),
) -> bool {
    app.focus = Focus::Tree;
    app.drag = DragOrigin::None;
    let rows = TreeState::visible_rows(view).len();
    if tree_row >= rows {
        return true; // below the last row: just the focus change
    }
    let count = app.clicks.click(col, row, Instant::now());
    match mouse::classify_tree_click(button, mods, count) {
        TreeClick::Focus => app.tree.set_cursor(tree_row, rows),
        TreeClick::ToggleMark => {
            app.tree.set_cursor(tree_row, rows);
            if let Some(path) = cursor_row_path(&app.tree, view) {
                app.tree.toggle_mark(path);
            }
        }
        TreeClick::ExtendRange => {
            let from = app.tree.cursor;
            app.tree.set_cursor(tree_row, rows);
            app.tree.mark_range(view, from, tree_row);
        }
        TreeClick::Open => {
            app.tree.set_cursor(tree_row, rows);
            open_tab_for_cursor(app, view, TabKind::Shell);
        }
    }
    if button == MouseButton::Left {
        app.drag = DragOrigin::Tree { anchor: tree_row };
    }
    true
}

/// A button-down on the terminal grid at (`gc`, `line`): the child's if it
/// asked for the mouse (contract §4), else the start of a selection —
/// simple, word or line by click count. Middle/right buttons neither paste
/// nor select: there is no clipboard→child path here by design.
fn terminal_mouse_down(
    app: &mut App,
    button: MouseButton,
    mods: KeyModifiers,
    (gc, line): (u16, u16),
    (col, row): (u16, u16),
) -> bool {
    app.focus = Focus::Terminal;
    app.drag = DragOrigin::None;
    let Some(tab) = &app.terminal else {
        return true;
    };
    let mode = tab.mode();
    if tab.is_alive() && mouse::forwards_to_child(mode, mods) {
        if let Some(bytes) = mouse::encode_mouse(MouseEventKind::Down(button), mods, gc, line, mode)
        {
            tab.write_input(bytes);
        }
        app.drag = DragOrigin::Child;
        return true;
    }
    if button == MouseButton::Left {
        let ty = match app.clicks.click(col, row, Instant::now()) {
            1 => SelectionType::Simple,
            2 => SelectionType::Semantic,
            _ => SelectionType::Lines,
        };
        tab.selection_start(gc, line, ty);
        app.drag = DragOrigin::Terminal;
    }
    true
}

/// Encodes `kind` at the screen position (clamped into the grid — a child
/// drag stays in the grid like any other) for a live child that asked for
/// the mouse, and writes it.
fn forward_to_child(app: &App, kind: MouseEventKind, mods: KeyModifiers, (col, row): (u16, u16)) {
    let Some(tab) = app.terminal.as_ref().filter(|t| t.is_alive()) else {
        return;
    };
    let Some((gc, line)) = app.regions.clamp_to_terminal(col, row) else {
        return;
    };
    let mode = tab.mode();
    if !mouse::forwards_to_child(mode, mods) {
        return;
    }
    if let Some(bytes) = mouse::encode_mouse(kind, mods, gc, line, mode) {
        tab.write_input(bytes);
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
                Some(text) if clipboard::copy_text(&text).is_ok() => {
                    // Copying consumes the selection, as in tmux/screen.
                    if let Some(tab) = &app.terminal {
                        tab.clear_selection();
                    }
                    "copied".to_string()
                }
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

    use crossterm::event::KeyModifiers;
    use ratatui::backend::TestBackend;

    use super::super::client::WorktreesClient;
    use super::super::view_model::{RepoRow, WorktreeRow};

    fn view_with(paths: &[&str]) -> WorktreesViewModel {
        let worktrees = paths
            .iter()
            .map(|p| WorktreeRow {
                path: PathBuf::from(p),
                branch: Some("main".to_string()),
                head_sha: None,
                upstream_sha: None,
                is_main: false,
                open: false,
                window_key: None,
                pr: None,
                pr_none: false,
                operation: None,
                rebasing: false,
                pushing: false,
                ahead_behind: super::super::view_model::AheadBehindState::Unknown,
                sessions: Vec::new(),
                row_color: None,
                here: false,
            })
            .collect();
        WorktreesViewModel {
            repos: vec![RepoRow {
                main_repo: "repo".to_string(),
                github: None,
                root: PathBuf::from("/repo"),
                polling_enabled: false,
                row_color: None,
                worktrees,
            }],
            ..Default::default()
        }
    }

    /// An app wired to a nonexistent daemon socket (no action here reaches
    /// it) with the hub-command receiver returned for assertions.
    fn test_app() -> (App, Dispatcher, mpsc::UnboundedReceiver<HubCommand>) {
        let (commands, commands_rx) = mpsc::unbounded_channel();
        let (pty_tx, _pty_rx) = mpsc::unbounded_channel();
        let dispatcher = Dispatcher::new(
            WorktreesClient::new("/tmp/nonexistent-omni-dev-app-test.sock"),
            commands.clone(),
        );
        let app = App {
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
            regions: RegionMap::default(),
            drag: DragOrigin::None,
            clicks: mouse::ClickTracker::default(),
        };
        (app, dispatcher, commands_rx)
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn alt(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
    }

    fn mouse_at(kind: MouseEventKind, col: u16, row: u16, modifiers: KeyModifiers) -> MouseEvent {
        MouseEvent {
            kind,
            column: col,
            row,
            modifiers,
        }
    }

    fn draw_into(terminal: &mut Terminal<TestBackend>, view: &WorktreesViewModel, app: &mut App) {
        terminal.draw(|frame| draw(frame, view, app)).unwrap();
    }

    /// Polls `cond` for up to ten seconds.
    async fn wait_until(mut cond: impl FnMut() -> bool) -> bool {
        for _ in 0..100 {
            if cond() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    #[cfg(unix)]
    fn scripted_tab(app: &App, script: &str, cwd: PathBuf) -> TerminalTab {
        let request = super::super::terminal::pty::SpawnRequest {
            tab: 1,
            program: Some((
                "/bin/sh".to_string(),
                vec!["-c".to_string(), script.to_string()],
            )),
            cwd,
            size: GridSize { cols: 40, lines: 6 },
            extra_env: Vec::new(),
        };
        TerminalTab::from_request(TabKind::Shell, request, app.pty_tx.clone()).unwrap()
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[tokio::test]
    async fn tree_keys_move_mark_open_menus_and_quit() {
        let (mut app, dispatcher, mut commands) = test_app();
        let view = view_with(&["/repo/a", "/repo/b"]);

        assert!(!handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('j'))).await);
        assert_eq!(app.tree.cursor, 1);
        assert!(!handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('k'))).await);
        assert_eq!(app.tree.cursor, 0);
        assert!(!handle_key(&mut app, &view, &dispatcher, press(KeyCode::Down)).await);

        // Space marks the cursor row; Esc clears marks before it quits.
        assert!(!handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char(' '))).await);
        assert_eq!(app.tree.marked.len(), 1);
        assert!(!handle_key(&mut app, &view, &dispatcher, press(KeyCode::Esc)).await);
        assert!(app.tree.marked.is_empty());
        assert!(handle_key(&mut app, &view, &dispatcher, press(KeyCode::Esc)).await);
        assert!(handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('q'))).await);

        // The action menu opens on `a`, navigates, and closes on Esc.
        assert!(!handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('a'))).await);
        assert!(app.menu.is_some());
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Down)).await;
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('k'))).await;
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Esc)).await;
        assert!(app.menu.is_none());

        // The colour picker dispatches a local SetRowColor through the hub.
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('c'))).await;
        assert!(app.menu.is_some());
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Enter)).await;
        assert!(matches!(app.flow, ActionFlow::Done { .. }));
        assert!(matches!(
            commands.try_recv(),
            Ok(HubCommand::SetRowColor(..))
        ));
        assert!(status_hint(&app).starts_with("Set colour"));
        // Any key dismisses the outcome.
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('x'))).await;
        assert_eq!(app.flow, ActionFlow::Idle);

        // `C` clears directly, no popup.
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('C'))).await;
        assert!(matches!(
            commands.try_recv(),
            Ok(HubCommand::ClearRowColor(_))
        ));
        assert!(matches!(app.flow, ActionFlow::Done { .. }));
        app.flow = ActionFlow::Idle;

        // An unknown key is ignored.
        assert!(!handle_key(&mut app, &view, &dispatcher, press(KeyCode::F(9))).await);
    }

    #[tokio::test]
    async fn an_action_that_needs_the_daemon_fails_visibly_when_it_is_down() {
        let (mut app, dispatcher, _commands) = test_app();
        let view = view_with(&["/repo/a"]);
        app.tree.cursor = 1; // the worktree row, not the repo header
                             // Open the menu and pick "Close Window" (ProceedWithoutConfirm, one
                             // daemon call) — the socket does not exist, so the batch reports it.
        let targets = app.tree.targets(&view);
        run_action(&mut app, &dispatcher, ActionKind::CloseWindow, targets).await;
        match &app.flow {
            ActionFlow::Done {
                outcome: actions::ActionOutcome::BatchDone { results },
            } => assert!(results.iter().all(|(_, r)| r.is_err())),
            other => panic!("unexpected flow: {other:?}"),
        }
        assert!(status_hint(&app).contains("failed"));

        // A two-phase check against a dead daemon is refused, not confirmed.
        let targets = app.tree.targets(&view);
        run_action(&mut app, &dispatcher, ActionKind::CloseWorktree, targets).await;
        assert!(matches!(app.flow, ActionFlow::Failed { .. }));
        assert!(status_hint(&app).starts_with("failed:"));

        // A confirm that arrives is answered by y/n.
        app.flow = ActionFlow::AwaitingConfirm {
            action: ActionKind::CloseWorktree,
            targets: Vec::new(),
            prompt: actions::ConfirmPrompt::default(),
        };
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('n'))).await;
        assert_eq!(app.flow, ActionFlow::Idle);
        app.flow = ActionFlow::AwaitingConfirm {
            action: ActionKind::CloseWorktree,
            targets: Vec::new(),
            prompt: actions::ConfirmPrompt::default(),
        };
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('y'))).await;
        assert!(matches!(app.flow, ActionFlow::Done { .. }));
    }

    #[tokio::test]
    async fn chrome_chords_without_a_terminal_leave_notices() {
        let (mut app, dispatcher, _commands) = test_app();
        let view = view_with(&["/repo/a"]);
        handle_key(&mut app, &view, &dispatcher, alt('l')).await;
        assert!(app
            .notice
            .as_deref()
            .unwrap_or("")
            .contains("no terminal tab"));
        handle_key(&mut app, &view, &dispatcher, alt('c')).await;
        assert_eq!(app.notice.as_deref(), Some("nothing selected"));
        handle_key(&mut app, &view, &dispatcher, alt('w')).await; // no tab: no-op
        assert_eq!(app.focus, Focus::Tree);
        handle_key(&mut app, &view, &dispatcher, alt('e')).await;
        assert_eq!(app.focus, Focus::Tree);
        handle_key(
            &mut app,
            &view,
            &dispatcher,
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT),
        )
        .await;
        handle_key(
            &mut app,
            &view,
            &dispatcher,
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::SHIFT),
        )
        .await;
        // A stray PTY event for a tab that no longer exists is ignored.
        assert!(!app.handle_pty_event(99, TermEvent::Wakeup));
        app.forward_focus(true); // no terminal: no-op
    }

    #[tokio::test]
    async fn relocate_wizard_fails_cleanly_without_sessions_and_cancels() {
        let (mut app, dispatcher, _commands) = test_app();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().into_owned();
        let view = view_with(&[&path, "/repo/other"]);

        // No Claude sessions exist for a fresh temp dir.
        start_relocate_flow(&mut app, &view, RelocationMode::Move);
        assert!(matches!(app.flow, ActionFlow::Failed { .. }));
        app.flow = ActionFlow::Idle;

        // Navigating and cancelling each picker step.
        let session = actions::relocate_types::SessionInfo {
            id: "s1".to_string(),
            jsonl_path: dir.path().join("s1.jsonl"),
            modified: std::time::SystemTime::UNIX_EPOCH,
            has_sidecar: false,
        };
        app.relocate = Some(RelocateStep::PickSession {
            mode: RelocationMode::Copy,
            source_worktree: dir.path().to_path_buf(),
            source_dir: dir.path().to_path_buf(),
            sessions: vec![session.clone(), session.clone()],
            selected: 0,
        });
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Down)).await;
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Up)).await;
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Enter)).await;
        assert!(matches!(
            app.relocate,
            Some(RelocateStep::PickDestination { .. })
        ));
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('j'))).await;
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('n'))).await; // no-op here
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Esc)).await;
        assert!(app.relocate.is_none());

        // The confirm step can be declined.
        app.relocate = Some(RelocateStep::Confirm {
            mode: RelocationMode::Copy,
            source_dir: dir.path().to_path_buf(),
            session,
            dest_worktree: PathBuf::from("/repo/other"),
            prompt: actions::ConfirmPrompt::default(),
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| draw(frame, &view, &mut app)).unwrap();
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('n'))).await;
        assert!(app.relocate.is_none());
        assert_eq!(clamp_selection(5, 1, 0), 0);
        assert!(relative_mtime(std::time::SystemTime::now()).ends_with("ago"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_terminal_tab_takes_focus_keys_and_closes_cleanly() {
        let (mut app, dispatcher, mut commands) = test_app();
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_path_buf();
        let view = view_with(&[&here.to_string_lossy(), "/repo/other"]);

        // Inject a scripted shell as the tab, exactly as open_tab would.
        let request = super::super::terminal::pty::SpawnRequest {
            tab: 1,
            program: Some((
                "/bin/sh".to_string(),
                vec!["-c".to_string(), "sleep 5".to_string()],
            )),
            cwd: here.clone(),
            size: GridSize { cols: 40, lines: 6 },
            extra_env: Vec::new(),
        };
        let tab = TerminalTab::from_request(TabKind::Shell, request, app.pty_tx.clone()).unwrap();
        app.terminal = Some(tab);
        app.focus = Focus::Terminal;

        // Keys go to the child, Esc included; chords are still chrome.
        assert!(!handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('x'))).await);
        assert!(!handle_key(&mut app, &view, &dispatcher, press(KeyCode::Esc)).await);
        assert!(!handle_key(&mut app, &view, &dispatcher, press(KeyCode::CapsLock)).await);
        handle_key(&mut app, &view, &dispatcher, alt('e')).await;
        assert_eq!(app.focus, Focus::Tree);
        handle_key(&mut app, &view, &dispatcher, alt('l')).await;
        assert_eq!(app.focus, Focus::Terminal);
        assert!(status_hint(&app).contains("alt-e tree"));
        app.forward_focus(false);
        app.forward_focus(true);

        // Opening the same worktree again just focuses; another is refused.
        app.focus = Focus::Tree;
        app.open_tab(
            TabKind::Shell,
            here.clone(),
            GridSize {
                cols: 80,
                lines: 24,
            },
        );
        assert_eq!(app.focus, Focus::Terminal);
        app.open_tab(
            TabKind::Shell,
            PathBuf::from("/repo/other"),
            GridSize {
                cols: 80,
                lines: 24,
            },
        );
        assert!(app.notice.as_deref().unwrap_or("").contains("one tab"));

        // Drawing lays out both panes, resizes the emulator to the pane, and
        // shows the quit confirm when `q` is pressed with a live child.
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| draw(frame, &view, &mut app)).unwrap();
        assert!(buffer_text(&terminal).contains("WORKTREES"));
        app.focus = Focus::Tree;
        assert!(!handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('q'))).await);
        assert!(app.quit_confirm);
        terminal.draw(|frame| draw(frame, &view, &mut app)).unwrap();
        assert!(buffer_text(&terminal).contains("Quit?"));
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('n'))).await;
        assert!(!app.quit_confirm);
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Esc)).await;
        assert!(app.quit_confirm);
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('x'))).await; // ignored
        assert!(handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char('y'))).await);
        app.quit_confirm = false;

        // Live PTY events flow through the tab; then alt-w closes it.
        assert!(app.handle_pty_event(1, TermEvent::Wakeup));
        assert!(!app.handle_pty_event(1, TermEvent::Bell));
        assert!(app.handle_pty_event(1, TermEvent::ChildExit(std::process::ExitStatus::default())));
        assert!(app
            .notice
            .as_deref()
            .unwrap_or("")
            .contains("terminal exited"));
        app.terminal.as_mut().unwrap().exit_status = None; // pretend it is still live
        handle_key(&mut app, &view, &dispatcher, alt('w')).await;
        assert!(app.terminal.is_none());
        assert_eq!(app.focus, Focus::Tree);
        assert!(matches!(commands.try_recv(), Ok(HubCommand::ClearOpenTab(p)) if p == here));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn open_tab_for_cursor_spawns_and_reports_here_then_replaces_an_exited_tab() {
        let (mut app, _dispatcher, mut commands) = test_app();
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_path_buf();
        let view = view_with(&[&here.to_string_lossy()]);
        app.tree.cursor = 1; // the worktree row, not the repo header

        // Route the real spawn through a scripted child by pre-seeding an
        // exited tab: open_tab must close it and spawn afresh.
        let request = super::super::terminal::pty::SpawnRequest {
            tab: 1,
            program: Some((
                "/bin/sh".to_string(),
                vec!["-c".to_string(), "true".to_string()],
            )),
            cwd: here.clone(),
            size: GridSize { cols: 40, lines: 6 },
            extra_env: Vec::new(),
        };
        let mut tab =
            TerminalTab::from_request(TabKind::Shell, request, app.pty_tx.clone()).unwrap();
        tab.exit_status = Some(std::process::ExitStatus::default());
        app.terminal = Some(tab);

        // A missing cursor row is a notice, not a spawn.
        let empty = WorktreesViewModel::default();
        open_tab_for_cursor(&mut app, &empty, TabKind::Shell);
        assert_eq!(app.notice.as_deref(), Some("no row selected"));

        // With a row: the exited tab is closed (ClearOpenTab) and the user's
        // shell is spawned in its place (SetOpenTab).
        open_tab_for_cursor(&mut app, &view, TabKind::Shell);
        assert!(matches!(
            commands.try_recv(),
            Ok(HubCommand::ClearOpenTab(_))
        ));
        match commands.try_recv() {
            Ok(HubCommand::SetOpenTab(p)) => assert_eq!(p, here),
            other => {
                // Spawning the login shell can legitimately fail on a
                // minimal CI image; then the notice carries the error.
                assert!(
                    app.notice.is_some(),
                    "neither a tab nor a notice: {other:?}"
                );
            }
        }
        if let Some(tab) = app.terminal.as_mut() {
            tab.shutdown();
        }
    }

    #[tokio::test]
    async fn mouse_on_the_tree_focuses_marks_ranges_scrolls_and_ignores_chrome() {
        let (mut app, _dispatcher, _commands) = test_app();
        let view = view_with(&["/repo/a", "/repo/b", "/repo/c"]); // 4 rows with the header
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        draw_into(&mut terminal, &view, &mut app);
        let tree = app.regions.tree;
        assert_eq!((tree.x, tree.y), (1, 1), "inside the border");
        let none = KeyModifiers::NONE;
        let down = |row: u16, mods| mouse_at(MouseEventKind::Down(MouseButton::Left), 5, row, mods);

        // The border is chrome; a click there does nothing.
        assert!(!handle_mouse(&mut app, &view, down(0, none)));
        // A click on row 2 moves the cursor there.
        assert!(handle_mouse(&mut app, &view, down(tree.y + 2, none)));
        assert_eq!(app.tree.cursor, 2);
        assert_eq!(app.drag, DragOrigin::Tree { anchor: 2 });
        assert!(!handle_mouse(
            &mut app,
            &view,
            mouse_at(MouseEventKind::Up(MouseButton::Left), 5, 3, none)
        ));
        assert_eq!(app.drag, DragOrigin::None);
        assert!(app.tree.marked.is_empty(), "a click alone marks nothing");

        // ^-click toggles a mark; ⇧-click marks the range from the cursor.
        assert!(handle_mouse(
            &mut app,
            &view,
            down(tree.y + 3, KeyModifiers::CONTROL)
        ));
        assert!(app.tree.marked.contains(&PathBuf::from("/repo/c")));
        assert!(handle_mouse(
            &mut app,
            &view,
            down(tree.y + 1, KeyModifiers::SHIFT)
        ));
        assert_eq!(app.tree.cursor, 1);
        assert_eq!(app.tree.marked.len(), 3, "rows 1..=3 marked");
        app.tree.clear_marks();

        // A drag from row 1 to below the pane range-marks down to the last
        // row and stops there (clamped to the tree).
        assert!(handle_mouse(&mut app, &view, down(tree.y + 1, none)));
        assert!(handle_mouse(
            &mut app,
            &view,
            mouse_at(MouseEventKind::Drag(MouseButton::Left), 200, 200, none)
        ));
        assert_eq!(app.tree.cursor, 3);
        assert_eq!(app.tree.marked.len(), 3);
        handle_mouse(
            &mut app,
            &view,
            mouse_at(MouseEventKind::Up(MouseButton::Left), 200, 200, none),
        );

        // The wheel moves the cursor; below the last row only focus changes.
        assert!(handle_mouse(
            &mut app,
            &view,
            mouse_at(MouseEventKind::ScrollUp, 5, tree.y, none)
        ));
        assert_eq!(app.tree.cursor, 2);
        assert!(handle_mouse(
            &mut app,
            &view,
            mouse_at(MouseEventKind::ScrollDown, 5, tree.y, none)
        ));
        assert_eq!(app.tree.cursor, 3);
        assert!(handle_mouse(&mut app, &view, down(tree.y + 8, none)));
        assert_eq!(app.tree.cursor, 3);
        // A right-click focuses without starting a drag.
        assert!(handle_mouse(
            &mut app,
            &view,
            mouse_at(MouseEventKind::Down(MouseButton::Right), 5, tree.y, none)
        ));
        assert_eq!(app.tree.cursor, 0);
        assert_eq!(app.drag, DragOrigin::None);
        assert!(!handle_mouse(
            &mut app,
            &view,
            mouse_at(MouseEventKind::Drag(MouseButton::Right), 5, 3, none)
        ));
        // Moves and horizontal wheel are inert with no terminal.
        assert!(!handle_mouse(
            &mut app,
            &view,
            mouse_at(MouseEventKind::Moved, 5, 3, none)
        ));
        assert!(!handle_mouse(
            &mut app,
            &view,
            mouse_at(MouseEventKind::ScrollLeft, 5, 3, none)
        ));

        // With a popup open the mouse is inert.
        app.menu = Some(popup::ActionMenu::new(Vec::new()));
        assert!(!handle_mouse(&mut app, &view, down(tree.y, none)));
        app.menu = None;
        app.quit_confirm = true;
        assert!(!handle_mouse(&mut app, &view, down(tree.y, none)));
        app.quit_confirm = false;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_double_click_on_a_row_opens_a_shell_tab_there() {
        let (mut app, _dispatcher, _commands) = test_app();
        let dir = tempfile::tempdir().unwrap();
        let view = view_with(&[&dir.path().to_string_lossy()]);
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        draw_into(&mut terminal, &view, &mut app);
        let y = app.regions.tree.y + 1; // the worktree row
        let click = mouse_at(
            MouseEventKind::Down(MouseButton::Left),
            5,
            y,
            KeyModifiers::NONE,
        );
        handle_mouse(&mut app, &view, click);
        handle_mouse(
            &mut app,
            &view,
            mouse_at(
                MouseEventKind::Up(MouseButton::Left),
                5,
                y,
                KeyModifiers::NONE,
            ),
        );
        handle_mouse(&mut app, &view, click);
        assert_eq!(app.tree.cursor, 1);
        // Either the login shell spawned (and took focus) or, on a minimal
        // image, the failure is reported — never silence.
        assert!(
            app.terminal.is_some() || app.notice.is_some(),
            "double-click neither opened a tab nor reported a failure"
        );
        if app.terminal.is_some() {
            assert_eq!(app.focus, Focus::Terminal);
        }
        app.close_tab();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mouse_on_the_terminal_selects_by_drag_word_and_line_and_scrolls() {
        let (mut app, _dispatcher, _commands) = test_app();
        let dir = tempfile::tempdir().unwrap();
        let view = view_with(&[&dir.path().to_string_lossy()]);
        app.terminal = Some(scripted_tab(
            &app,
            "printf 'hello world\\nsecond line\\n'; sleep 3",
            dir.path().to_path_buf(),
        ));
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        draw_into(&mut terminal, &view, &mut app);
        let grid = app.regions.terminal.expect("a terminal region");
        assert!(
            wait_until(|| app
                .terminal
                .as_ref()
                .unwrap()
                .selection_to_string()
                .is_none()
                && {
                    draw_into(&mut terminal, &view, &mut app);
                    buffer_text(&terminal).contains("second line")
                })
            .await,
            "the child's output never rendered"
        );
        let none = KeyModifiers::NONE;
        let at = |kind, col: u16, line: u16| mouse_at(kind, grid.x + col, grid.y + line, none);

        // Click focuses the terminal; drag selects; the drag is clamped so
        // overshooting the pane still ends on its last cell.
        app.focus = Focus::Tree;
        assert!(handle_mouse(
            &mut app,
            &view,
            at(MouseEventKind::Down(MouseButton::Left), 0, 0)
        ));
        assert_eq!(app.focus, Focus::Terminal);
        assert_eq!(app.drag, DragOrigin::Terminal);
        assert!(handle_mouse(
            &mut app,
            &view,
            at(MouseEventKind::Drag(MouseButton::Left), 4, 0)
        ));
        assert!(!handle_mouse(
            &mut app,
            &view,
            at(MouseEventKind::Up(MouseButton::Left), 4, 0)
        ));
        assert_eq!(app.drag, DragOrigin::None);
        assert_eq!(
            app.terminal
                .as_ref()
                .unwrap()
                .selection_to_string()
                .as_deref(),
            Some("hello")
        );
        assert!(handle_mouse(
            &mut app,
            &view,
            at(MouseEventKind::Down(MouseButton::Left), 0, 1)
        ));
        assert!(handle_mouse(
            &mut app,
            &view,
            mouse_at(MouseEventKind::Drag(MouseButton::Left), 0, 0, none)
        ));
        assert!(handle_mouse(
            &mut app,
            &view,
            mouse_at(MouseEventKind::Drag(MouseButton::Left), 500, 500, none)
        ));
        let clamped = app
            .terminal
            .as_ref()
            .unwrap()
            .selection_to_string()
            .unwrap();
        assert!(clamped.starts_with("second line"), "got {clamped:?}");
        handle_mouse(
            &mut app,
            &view,
            at(MouseEventKind::Up(MouseButton::Left), 0, 1),
        );

        // Double-click selects the word, triple-click the line.
        let dbl = at(MouseEventKind::Down(MouseButton::Left), 7, 0);
        handle_mouse(&mut app, &view, dbl);
        handle_mouse(
            &mut app,
            &view,
            at(MouseEventKind::Up(MouseButton::Left), 7, 0),
        );
        handle_mouse(&mut app, &view, dbl);
        assert_eq!(
            app.terminal
                .as_ref()
                .unwrap()
                .selection_to_string()
                .as_deref(),
            Some("world")
        );
        handle_mouse(
            &mut app,
            &view,
            at(MouseEventKind::Up(MouseButton::Left), 7, 0),
        );
        handle_mouse(&mut app, &view, dbl);
        // A line selection's text carries the line's own trailing newline.
        let line = app.terminal.as_ref().unwrap().selection_to_string();
        assert_eq!(line.as_deref().map(str::trim_end), Some("hello world"));
        handle_mouse(
            &mut app,
            &view,
            at(MouseEventKind::Up(MouseButton::Left), 7, 0),
        );
        draw_into(&mut terminal, &view, &mut app); // the selection renders
                                                   // alt-c copies (OSC 52 fallback in CI) and consumes the selection.
        handle_chrome_key(&mut app, &view, ChromeKey::Copy);
        assert_eq!(app.notice.as_deref(), Some("copied"));
        assert!(app
            .terminal
            .as_ref()
            .unwrap()
            .selection_to_string()
            .is_none());

        // The wheel scrolls the emulator's history (no child mouse mode, not
        // the alt screen); a middle click neither pastes nor selects.
        assert!(handle_mouse(
            &mut app,
            &view,
            at(MouseEventKind::ScrollUp, 1, 1)
        ));
        assert!(handle_mouse(
            &mut app,
            &view,
            at(MouseEventKind::ScrollDown, 1, 1)
        ));
        assert!(handle_mouse(
            &mut app,
            &view,
            at(MouseEventKind::Down(MouseButton::Middle), 1, 1)
        ));
        assert_eq!(app.drag, DragOrigin::None);
        assert!(!handle_mouse(
            &mut app,
            &view,
            at(MouseEventKind::Moved, 1, 1)
        ));
        assert!(!handle_mouse(
            &mut app,
            &view,
            at(MouseEventKind::ScrollRight, 1, 1)
        ));
        app.close_tab();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_child_that_asked_for_the_mouse_receives_it_unless_alt_is_held() {
        let (mut app, _dispatcher, _commands) = test_app();
        let dir = tempfile::tempdir().unwrap();
        let view = view_with(&[&dir.path().to_string_lossy()]);
        // Enable SGR click reporting, then print the first nine bytes the
        // TUI sends (exactly one SGR press) in a form the grid can show.
        let script = "stty -echo -icanon min 1 time 0; printf '\\033[?1000h\\033[?1006h'; \
                      dd bs=1 count=9 2>/dev/null | od -An -c; sleep 2";
        app.terminal = Some(scripted_tab(&app, script, dir.path().to_path_buf()));
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        draw_into(&mut terminal, &view, &mut app);
        let grid = app.regions.terminal.expect("a terminal region");
        assert!(
            wait_until(|| app
                .terminal
                .as_ref()
                .unwrap()
                .mode()
                .contains(TermMode::SGR_MOUSE))
            .await,
            "the child never enabled mouse reporting"
        );
        let none = KeyModifiers::NONE;

        // Alt-click: the TUI keeps the mouse and starts a selection.
        assert!(handle_mouse(
            &mut app,
            &view,
            mouse_at(
                MouseEventKind::Down(MouseButton::Left),
                grid.x,
                grid.y,
                KeyModifiers::ALT
            )
        ));
        assert_eq!(app.drag, DragOrigin::Terminal);
        handle_mouse(
            &mut app,
            &view,
            mouse_at(
                MouseEventKind::Up(MouseButton::Left),
                grid.x,
                grid.y,
                KeyModifiers::ALT,
            ),
        );

        // A plain click goes to the child, as does its release and any
        // motion in between; the child prints what it got.
        assert!(handle_mouse(
            &mut app,
            &view,
            mouse_at(
                MouseEventKind::Down(MouseButton::Left),
                grid.x,
                grid.y,
                none
            )
        ));
        assert_eq!(app.drag, DragOrigin::Child);
        assert!(!handle_mouse(
            &mut app,
            &view,
            mouse_at(
                MouseEventKind::Drag(MouseButton::Left),
                grid.x + 1,
                grid.y,
                none
            )
        ));
        assert!(!handle_mouse(
            &mut app,
            &view,
            mouse_at(
                MouseEventKind::Up(MouseButton::Left),
                grid.x + 1,
                grid.y,
                none
            )
        ));
        assert_eq!(app.drag, DragOrigin::None);
        assert!(
            wait_until(|| {
                draw_into(&mut terminal, &view, &mut app);
                let text: String = buffer_text(&terminal).split_whitespace().collect();
                text.contains("033[<0;1;1M")
            })
            .await,
            "the child never echoed the SGR press: {}",
            buffer_text(&terminal)
        );
        // A wheel notch is forwarded too (no redraw needed for that).
        assert!(!handle_mouse(
            &mut app,
            &view,
            mouse_at(MouseEventKind::ScrollUp, grid.x, grid.y, none)
        ));
        app.close_tab();
    }

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
