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
use super::glyph::GlyphMode;
use super::hub::{HubCommand, ViewModelHandle};
use super::keys::{self, ChromeKey, KeyRoute};
use super::mouse::{self, DragOrigin, Hit, RegionMap, TreeClick, WheelRoute};
use super::panes;
use super::session_layout;
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
    /// The terminal side: a vertical stack of tab groups (Phase 4b).
    panes: panes::PaneLayout,
    /// `q` with a live child asks first; the answer lands here.
    quit_confirm: bool,
    /// A one-line message for the status bar, cleared on the next key.
    notice: Option<String>,
    pty_tx: mpsc::UnboundedSender<(TabId, TermEvent)>,
    commands: mpsc::UnboundedSender<HubCommand>,
    /// Unicode or ASCII row cues, resolved once at startup.
    glyphs: GlyphMode,
    /// The scrollback-find or command-palette prompt, when one is open.
    prompt: Option<Prompt>,
    /// The worktree paths the tree pane last rendered, so the hub only
    /// fetches ahead/behind for rows that are actually on screen.
    reported_visible: Vec<PathBuf>,
    /// The hit-testable regions of the last drawn frame.
    regions: RegionMap,
    /// The terminal side's rect as of the last frame — what a splitter drag
    /// recomputes weights against.
    terminal_area: Option<Rect>,
    /// The drag in progress, if any, and the region it is clamped to.
    drag: DragOrigin,
    clicks: mouse::ClickTracker,
}

/// A one-line text prompt: either the scrollback find (`alt-f`) or the
/// command palette (`:`). Both are a typed string plus a status line, so
/// they share one state and one renderer rather than two near-copies.
struct Prompt {
    kind: PromptKind,
    input: String,
    status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptKind {
    /// Search the focused tab's scrollback.
    Find,
    /// Run a command by name.
    Palette,
}

impl PromptKind {
    fn title(self) -> &'static str {
        match self {
            Self::Find => " find in scrollback ",
            Self::Palette => " command ",
        }
    }
}

/// One entry in the command palette: the name typed to match it, and the
/// action it runs. Built from the same [`ActionKind`] set the action menu
/// uses, so the palette can never drift from the menu.
fn palette_entries(targets: &[Target]) -> Vec<popup::MenuItem> {
    actions::applicable_actions(targets)
        .into_iter()
        .map(|(action, label)| popup::MenuItem::action(action, label))
        .collect()
}

/// The applicable actions as menu entries, with a separator between each
/// pair of groups. `applicable_action_groups` drops empty groups, so this can
/// never emit a leading, trailing or doubled rule.
fn grouped_menu_entries(targets: &[Target]) -> Vec<popup::MenuEntry> {
    let mut entries = Vec::new();
    for group in actions::applicable_action_groups(targets) {
        if !entries.is_empty() {
            entries.push(popup::MenuEntry::Separator);
        }
        entries.extend(
            group.into_iter().map(|(action, label)| {
                popup::MenuEntry::Item(popup::MenuItem::action(action, label))
            }),
        );
    }
    entries
}

/// Filters palette entries by a case-insensitive substring of their label.
fn filter_palette(entries: Vec<popup::MenuItem>, query: &str) -> Vec<popup::MenuItem> {
    if query.is_empty() {
        return entries;
    }
    let needle = query.to_lowercase();
    entries
        .into_iter()
        .filter(|item| item.label.to_lowercase().contains(&needle))
        .collect()
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
    glyphs: GlyphMode,
) -> Result<()> {
    let mut events = EventStream::new();
    let (pty_tx, mut pty_rx) = mpsc::unbounded_channel();
    let mut app = App {
        tree: TreeState::default(),
        flow: ActionFlow::Idle,
        menu: None,
        relocate: None,
        focus: Focus::Tree,
        panes: panes::PaneLayout::default(),
        quit_confirm: false,
        notice: None,
        pty_tx,
        commands,
        glyphs,
        prompt: None,
        reported_visible: Vec::new(),
        regions: RegionMap::default(),
        terminal_area: None,
        drag: DragOrigin::None,
        clicks: mouse::ClickTracker::default(),
    };
    // Restore the previous run's pane shape (Phase 5). Tabs are respawned,
    // never resumed — the children died with the last process — and a tab
    // that will not spawn is dropped rather than failing the start.
    let saved = session_layout::load(None);
    if !saved.is_empty() {
        let tx = app.pty_tx.clone();
        let size = GridSize {
            cols: 80,
            lines: 24,
        };
        let failed = app.panes.restore(&saved, |id, kind, path| {
            TerminalTab::spawn(id, kind, path.to_path_buf(), size, tx.clone())
        });
        for worktree in app.panes.open_worktrees() {
            let _ = app.commands.send(HubCommand::SetOpenTab(worktree));
        }
        if !app.panes.is_empty() {
            app.focus = Focus::Terminal;
        }
        if failed > 0 {
            app.notice = Some(format!("{failed} saved tab(s) could not be reopened"));
        }
    }

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
                        if handle_mouse(&mut app, &view, &dispatcher, mouse).await {
                            dirty = true;
                        }
                    }
                    Some(Ok(Event::Paste(text))) => {
                        if app.focus == Focus::Terminal {
                            if let Some(tab) = app.panes.active_tab().filter(|t| t.is_alive()) {
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

    // Persist the pane shape before tearing the children down, so the next
    // run opens the same workspace. Best-effort: failing to write a
    // convenience file must not fail the command.
    if let Err(e) = session_layout::save(&app.panes.to_saved(), None) {
        tracing::debug!("worktrees ui: could not save the layout: {e:#}");
    }
    for group in &mut app.panes.groups {
        for tab in &mut group.tabs {
            tab.shutdown();
        }
    }
    Ok(())
}

impl App {
    /// Absorbs one emulator event; returns whether a redraw is needed.
    fn handle_pty_event(&mut self, tab_id: TabId, event: TermEvent) -> bool {
        let Some(effect) = self.panes.handle_event(tab_id, event) else {
            return false; // an event from a tab that has already been closed
        };
        match effect {
            TabEffect::None => false,
            TabEffect::Redraw => true,
            TabEffect::CopyToClipboard(text) => {
                if clipboard::copy_text(&text).is_err() {
                    self.notice = Some("clipboard unavailable".to_string());
                }
                false
            }
            TabEffect::Exited => {
                self.notice = Some("terminal exited — alt-w to close the tab".to_string());
                true
            }
        }
    }

    /// Forwards host focus in/out to a child that asked for it
    /// (`TermMode::FOCUS_IN_OUT`), the way a real terminal would. Only the
    /// focused tab hears about it — a background tab never had focus to
    /// gain or lose.
    fn forward_focus(&self, gained: bool) {
        if let Some(tab) = self.panes.active_tab().filter(|t| t.is_alive()) {
            if tab.mode().contains(TermMode::FOCUS_IN_OUT) {
                tab.write_input(if gained {
                    b"\x1b[I".to_vec()
                } else {
                    b"\x1b[O".to_vec()
                });
            }
        }
    }

    /// Opens a tab of `kind` in `worktree`, focusing an existing live one
    /// for the same worktree and kind rather than opening a duplicate. With
    /// `split`, the tab lands in a new group below the focused one.
    fn open_tab(&mut self, kind: TabKind, worktree: PathBuf, size: GridSize, split: bool) {
        // Reusing an existing tab is the *open* gesture's convenience. A
        // split is an explicit "give me another pane", so it always makes
        // one — including on a worktree that already has a tab.
        if !split {
            if let Some(addr) = self.panes.find_in_worktree(&worktree, kind) {
                self.panes.focus(addr);
                self.focus = Focus::Terminal;
                return;
            }
        }
        let tx = self.pty_tx.clone();
        let target = worktree.clone();
        let spawn = move |id| TerminalTab::spawn(id, kind, target, size, tx);
        let opened = if split {
            self.panes.split(spawn)
        } else {
            self.panes.open_tab(spawn)
        };
        match opened {
            Ok(_) => {
                self.focus = Focus::Terminal;
                let _ = self.commands.send(HubCommand::SetOpenTab(worktree));
            }
            Err(e) => self.notice = Some(format!("{e:#}")),
        }
    }

    /// Closes the focused tab. The `here` cue is only cleared once the
    /// *last* tab on that worktree has gone, since several tabs may share
    /// one worktree.
    fn close_tab(&mut self) {
        let Some(worktree) = self.panes.close_active() else {
            return;
        };
        if !self.panes.open_worktrees().contains(&worktree) {
            let _ = self.commands.send(HubCommand::ClearOpenTab(worktree));
        }
        if self.panes.is_empty() {
            self.focus = Focus::Tree;
        }
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
    let areas = layout(frame.area(), !app.panes.is_empty());
    render::draw_tree_pane(
        frame,
        areas.tree,
        view,
        &mut app.tree,
        app.focus == Focus::Tree,
        app.glyphs,
    );
    report_visible_rows(app, view, areas.tree);
    // `arrange` both lays the stack out and resizes each visible group's
    // active tab to the grid it is about to be drawn into, so the emulator
    // is never read at a size it was not rendered at.
    app.terminal_area = areas.terminal;
    let group_rects = match areas.terminal {
        Some(area) => app.panes.arrange(area),
        None => Vec::new(),
    };
    app.panes
        .draw(frame, &group_rects, app.focus == Focus::Terminal);

    // The region map mirrors exactly what was just drawn, offsets included.
    let splitters = group_rects
        .iter()
        .take(group_rects.len().saturating_sub(1))
        .map(|r| super::layout::boundary_row(r.body))
        .collect();
    app.regions = RegionMap {
        // Filled in by `draw_popups` below, which is the pass that knows
        // where the menu actually landed.
        popup: None,
        tree: Block::default().borders(Borders::ALL).inner(areas.tree),
        tree_offset: app.tree.offset,
        groups: group_rects
            .iter()
            .map(|r| mouse::GroupRegion {
                strip: r.strip,
                grid: r.grid,
                tab_spans: r.tab_spans.clone(),
            })
            .collect(),
        splitters,
        splitter_cols: areas
            .terminal
            .map_or((0, 0), |a| (a.x, a.x.saturating_add(a.width))),
    };
    render::draw_status_bar(frame, areas.status, view, &app.tree, &status_hint(app));
    let area = frame.area();
    app.regions.popup = draw_popups(frame, area, app);
}

/// Tells the hub which worktree rows are actually on screen, so ahead/behind
/// is fetched for those and not for every row in the snapshot — the dominant
/// per-worktree cost (#1306), and the reason `SetVisibleRows` exists.
///
/// Sent only when the set changes: the command triggers fetches, and firing
/// it every frame would restart them ~60 times a second.
fn report_visible_rows(app: &mut App, view: &WorktreesViewModel, tree_area: Rect) {
    let inner = Block::default().borders(Borders::ALL).inner(tree_area);
    let rows = TreeState::visible_rows(view);
    let visible: Vec<PathBuf> = rows
        .iter()
        .skip(app.tree.offset)
        .take(usize::from(inner.height))
        .filter_map(|row| row.worktree_path(view))
        .collect();
    if visible != app.reported_visible {
        app.reported_visible.clone_from(&visible);
        let _ = app.commands.send(HubCommand::SetVisibleRows(visible));
    }
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
            "alt-e tree  alt-t tab  alt-s split  alt-[/] cycle  alt-w close  alt-c copy".to_string()
        }
        (_, Focus::Tree) => {
            "↑↓ move  space mark  enter/alt-t shell tab  alt-⇧t claude tab  a actions  c/C colour  q quit".to_string()
        }
    }
}

/// Draws whichever popup is open, and returns the hit map for it when that
/// popup takes the mouse (see [`popup_mouse_enabled`]). Every other popup
/// returns `None` and so stays inert, exactly as before #1602.
fn draw_popups(frame: &mut Frame<'_>, area: Rect, app: &mut App) -> Option<mouse::PopupRegion> {
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
        return None;
    }
    if let Some(prompt) = &app.prompt {
        popup::draw_prompt(
            frame,
            area,
            prompt.kind.title(),
            &prompt.input,
            &prompt.status,
        );
        return None;
    }
    if let Some(menu) = &mut app.menu {
        return Some(popup::draw_menu(frame, area, menu));
    }
    match &app.relocate {
        Some(RelocateStep::PickSession {
            sessions, selected, ..
        }) => {
            // Prefer the transcript's first prompt over the bare UUID: an
            // id says nothing about which session you are moving.
            let labels: Vec<String> = sessions
                .iter()
                .map(|s| {
                    let when = relative_mtime(s.modified);
                    match relocate::transcript_preview(&s.jsonl_path, 48) {
                        Some(preview) => format!("{preview}  ({when})"),
                        None => format!("{}  ({when})", s.id),
                    }
                })
                .collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            popup::draw_list_popup(frame, area, "Move which Claude session?", &refs, *selected);
            return None;
        }
        Some(RelocateStep::PickDestination {
            candidates,
            selected,
            ..
        }) => {
            let labels: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            popup::draw_list_popup(frame, area, "Move to which worktree?", &refs, *selected);
            return None;
        }
        Some(RelocateStep::Confirm { prompt, .. }) => {
            popup::draw_confirm_modal(
                frame,
                area,
                &popup::ConfirmModal {
                    prompt: prompt.clone(),
                },
            );
            return None;
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
    None
}

fn relative_mtime(modified: std::time::SystemTime) -> String {
    match std::time::SystemTime::now().duration_since(modified) {
        Ok(elapsed) => format!("{}s ago", elapsed.as_secs()),
        Err(_) => "just now".to_string(),
    }
}

// --- Mouse handling ---------------------------------------------------------

/// Whether the popup currently open takes the mouse.
///
/// The Phase 4a rule was "popups are chrome and chrome is inert". #1602
/// narrows that to exactly one exception — a menu you cannot click is not a
/// menu — and this predicate *is* that exception, so which popups are
/// clickable stays one legible decision rather than a scatter of
/// `is_some()` checks. Everything else keeps the old behaviour on purpose:
/// the confirm modal's keyboard-only `y`/`n` is a feature, not an oversight,
/// since it gates destructive actions.
fn popup_mouse_enabled(app: &App) -> bool {
    app.menu.is_some()
        && !app.quit_confirm
        && app.prompt.is_none()
        && app.relocate.is_none()
        && !matches!(app.flow, ActionFlow::AwaitingConfirm { .. })
}

/// Handles one mouse event under the contract in `mouse.rs`. Returns
/// whether a redraw is needed.
async fn handle_mouse(
    app: &mut App,
    view: &WorktreesViewModel,
    dispatcher: &Dispatcher,
    mouse: MouseEvent,
) -> bool {
    if popup_mouse_enabled(app) {
        return handle_menu_mouse(app, view, dispatcher, mouse).await;
    }
    if app.quit_confirm
        || app.menu.is_some()
        || app.prompt.is_some()
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
                Hit::Terminal {
                    group,
                    col: gc,
                    line,
                } => terminal_mouse_down(app, group, button, mods, (gc, line), (col, row)),
                Hit::TabStrip { group, tab } => strip_mouse_down(app, group, tab, button),
                Hit::Splitter { index } => {
                    app.drag = DragOrigin::Splitter { index };
                    false
                }
                // The popup arms are unreachable here: `popup_mouse_enabled`
                // took every event while a menu was open, and with no menu
                // the map has no popup region to hit.
                Hit::PopupItem { .. } | Hit::PopupInert | Hit::PopupOutside | Hit::Chrome => false,
            }
        }
        MouseEventKind::Drag(button) => match app.drag {
            DragOrigin::Tree { anchor } => {
                let rows = TreeState::visible_rows(view).len();
                app.tree.set_cursor(app.regions.tree_row_clamped(row), rows);
                app.tree.mark_range(view, anchor, app.tree.cursor);
                true
            }
            DragOrigin::Terminal { group } => {
                let Some((gc, line)) = app.regions.clamp_to_grid(group, col, row) else {
                    return false;
                };
                if let Some(tab) = app.panes.group_tab(group) {
                    tab.selection_update(gc, line);
                }
                true
            }
            DragOrigin::Splitter { index } => drag_splitter(app, index, row),
            DragOrigin::Child { group } => {
                forward_to_child(app, group, MouseEventKind::Drag(button), mods, (col, row));
                false
            }
            DragOrigin::None => false,
        },
        MouseEventKind::Up(button) => {
            if let DragOrigin::Child { group } = std::mem::take(&mut app.drag) {
                forward_to_child(app, group, MouseEventKind::Up(button), mods, (col, row));
            }
            false
        }
        MouseEventKind::Moved => {
            // Only a child that asked for all-motion reporting cares.
            if let Hit::Terminal { group, .. } = app.regions.hit(col, row) {
                forward_to_child(app, group, MouseEventKind::Moved, mods, (col, row));
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
                // The wheel over a tab strip cycles that group's tabs.
                Hit::TabStrip { group, .. } => {
                    app.panes.focused = group;
                    app.panes.cycle_tab(if up { -1 } else { 1 });
                    true
                }
                Hit::Terminal {
                    group,
                    col: gc,
                    line,
                } => {
                    let Some(tab) = app.panes.group_tab(group) else {
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
                Hit::PopupItem { .. }
                | Hit::PopupInert
                | Hit::PopupOutside
                | Hit::Splitter { .. }
                | Hit::Chrome => false,
            }
        }
        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => {
            if let Hit::Terminal { group, .. } = app.regions.hit(col, row) {
                forward_to_child(app, group, mouse.kind, mods, (col, row));
            }
            false
        }
    }
}

/// Handles one mouse event while a context menu is open (contract §7: the
/// menu is modal over what it covers and inert beyond it).
///
/// Every event here is **consumed** — none of them reaches the region under
/// the menu, including the click that dismisses it.
async fn handle_menu_mouse(
    app: &mut App,
    view: &WorktreesViewModel,
    dispatcher: &Dispatcher,
    mouse: MouseEvent,
) -> bool {
    // A menu that has not been drawn yet has no region, so every position is
    // "outside" it. Without this a click before the first frame would arm the
    // menu instead of dismissing it.
    let hit = if app.regions.popup.is_some() {
        app.regions.hit(mouse.column, mouse.row)
    } else {
        Hit::PopupOutside
    };
    match mouse.kind {
        // Hover follows the pointer, as every other menu on the platform
        // does — but only redraws when the highlight actually moves, or the
        // UI would repaint at pointer-motion rate.
        MouseEventKind::Moved | MouseEventKind::Drag(_) => {
            let Hit::PopupItem { index } = hit else {
                return false;
            };
            let Some(menu) = app.menu.as_mut() else {
                return false;
            };
            if menu.selected == index {
                return false;
            }
            menu.select_index(index)
        }
        MouseEventKind::Down(_) => {
            if matches!(hit, Hit::PopupOutside) {
                app.menu = None;
                return true;
            }
            // Arm on a press that starts *inside* the menu. The press that
            // opened the menu happened outside it, so its release cannot
            // invoke anything — see `ActionMenu::armed`.
            if let Some(menu) = app.menu.as_mut() {
                menu.armed = true;
                if let Hit::PopupItem { index } = hit {
                    menu.select_index(index);
                }
            }
            true
        }
        MouseEventKind::Up(_) => {
            let Hit::PopupItem { index } = hit else {
                return false;
            };
            let Some(menu) = app.menu.as_mut() else {
                return false;
            };
            if !menu.armed {
                return false; // the opening click's own release
            }
            if !menu.select_index(index) {
                return false;
            }
            let Some(command) = menu.selected_command() else {
                return true; // a disabled item: highlighted, not invoked
            };
            invoke_menu_command(app, view, dispatcher, command).await;
            true
        }
        // The wheel scrolls the menu rather than the pane beneath it.
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let up = mouse.kind == MouseEventKind::ScrollUp;
            if let Some(menu) = app.menu.as_mut() {
                menu.move_selection(if up { -1 } else { 1 });
            }
            true
        }
        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight => false,
    }
}

/// Dispatches one menu command and closes the menu — the single path shared
/// by `Enter` and by a mouse click, so the two entry points cannot drift.
async fn invoke_menu_command(
    app: &mut App,
    view: &WorktreesViewModel,
    dispatcher: &Dispatcher,
    command: popup::MenuCommand,
) {
    app.menu = None;
    match command {
        popup::MenuCommand::Action(
            action @ (ActionKind::MoveClaudeSessionHere | ActionKind::CopyClaudeSessionHere),
        ) => {
            let mode = if action == ActionKind::MoveClaudeSessionHere {
                RelocationMode::Move
            } else {
                RelocationMode::Copy
            };
            start_relocate_flow(app, view, mode);
        }
        popup::MenuCommand::Action(action) => {
            let targets = app.tree.targets(view);
            run_action(app, dispatcher, action, targets).await;
        }
    }
}

/// A button-down on a tab strip: left activates the tab (or focuses the
/// group when the click is past the last one), middle closes it — the
/// conventional tab-strip gestures.
fn strip_mouse_down(app: &mut App, group: usize, tab: Option<usize>, button: MouseButton) -> bool {
    app.focus = Focus::Terminal;
    app.drag = DragOrigin::None;
    app.panes.focused = group.min(app.panes.group_count().saturating_sub(1));
    let Some(tab) = tab else {
        return true;
    };
    match button {
        MouseButton::Middle => {
            let addr = panes::TabAddr { group, tab };
            if let Some(worktree) = app.panes.close_tab(addr) {
                if !app.panes.open_worktrees().contains(&worktree) {
                    let _ = app.commands.send(HubCommand::ClearOpenTab(worktree));
                }
                if app.panes.is_empty() {
                    app.focus = Focus::Tree;
                }
            }
        }
        MouseButton::Left | MouseButton::Right => {
            app.panes.focus(panes::TabAddr { group, tab });
        }
    }
    true
}

/// Drags splitter `index` to screen row `row`, resizing only the two groups
/// either side of it.
fn drag_splitter(app: &mut App, index: usize, row: u16) -> bool {
    let Some(area) = app.terminal_area else {
        return false;
    };
    let next = super::layout::drag_splitter(area, &app.panes.weights, index, row);
    if next == app.panes.weights {
        return false;
    }
    app.panes.weights = next;
    true
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
            open_tab_for_cursor(app, view, TabKind::Shell, false);
        }
        TreeClick::ContextMenu => {
            // The universal convention: right-clicking inside the marked set
            // acts on the whole set; right-clicking outside it moves the
            // cursor and acts on that row alone. Both end at
            // `tree.targets`, so `applicable_actions` filters unchanged.
            let inside_marks =
                row_path(view, tree_row).is_some_and(|path| app.tree.marked.contains(&path));
            if !inside_marks {
                app.tree.set_cursor(tree_row, rows);
                app.tree.clear_marks();
            }
            let targets = app.tree.targets(view);
            let mut menu = popup::ActionMenu::with_entries(
                if inside_marks { "Marked" } else { "Actions" },
                grouped_menu_entries(&targets),
            );
            menu.anchor = popup::Anchor::At { col, row };
            app.menu = Some(menu);
        }
    }
    if button == MouseButton::Left {
        app.drag = DragOrigin::Tree { anchor: tree_row };
    }
    true
}

/// A button-down on group `group`'s grid at (`gc`, `line`): the child's if
/// it asked for the mouse (contract §4), else the start of a selection —
/// simple, word or line by click count. Middle/right buttons neither paste
/// nor select: there is no clipboard→child path here by design.
fn terminal_mouse_down(
    app: &mut App,
    group: usize,
    button: MouseButton,
    mods: KeyModifiers,
    (gc, line): (u16, u16),
    (col, row): (u16, u16),
) -> bool {
    app.focus = Focus::Terminal;
    app.drag = DragOrigin::None;
    // Clicking a group focuses it, so the next key goes where the eye is.
    app.panes.focused = group.min(app.panes.group_count().saturating_sub(1));
    let click = app.clicks.click(col, row, Instant::now());
    let Some(tab) = app.panes.group_tab(group) else {
        return true;
    };
    let mode = tab.mode();
    if tab.is_alive() && mouse::forwards_to_child(mode, mods) {
        if let Some(bytes) = mouse::encode_mouse(MouseEventKind::Down(button), mods, gc, line, mode)
        {
            tab.write_input(bytes);
        }
        app.drag = DragOrigin::Child { group };
        return true;
    }
    if button == MouseButton::Left {
        let ty = match click {
            1 => SelectionType::Simple,
            2 => SelectionType::Semantic,
            _ => SelectionType::Lines,
        };
        tab.selection_start(gc, line, ty);
        app.drag = DragOrigin::Terminal { group };
    }
    true
}

/// Encodes `kind` at the screen position (clamped into group `group`'s grid
/// — a child drag stays in the grid like any other) for a live child that
/// asked for the mouse, and writes it.
fn forward_to_child(
    app: &App,
    group: usize,
    kind: MouseEventKind,
    mods: KeyModifiers,
    (col, row): (u16, u16),
) {
    let Some(tab) = app.panes.group_tab(group).filter(|t| t.is_alive()) else {
        return;
    };
    let Some((gc, line)) = app.regions.clamp_to_grid(group, col, row) else {
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

    // An open prompt takes every remaining key: it is a text field, so a
    // bare letter must type rather than trigger a tree command.
    if app.prompt.is_some() {
        handle_prompt_key(app, view, key).await;
        return false;
    }

    // A focused, live terminal takes everything else verbatim.
    if app.focus == Focus::Terminal {
        match app.panes.active_tab().filter(|t| t.is_alive()) {
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
            KeyCode::Home => menu.select_end(false),
            KeyCode::End => menu.select_end(true),
            KeyCode::Enter => {
                if let Some(command) = menu.selected_command() {
                    invoke_menu_command(app, view, dispatcher, command).await;
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
            if app.panes.any_alive() {
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
            } else if app.panes.any_alive() {
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
        KeyCode::Enter => open_tab_for_cursor(app, view, TabKind::Shell, false),
        KeyCode::Char('a') => {
            let targets = app.tree.targets(view);
            app.menu = Some(popup::ActionMenu::with_entries(
                "Actions",
                grouped_menu_entries(&targets),
            ));
        }
        KeyCode::Char('c') => {
            let items = row_colors::KNOWN_ROW_COLORS
                .iter()
                .map(|&color| popup::MenuItem::action(ActionKind::SetRowColor(color), color))
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
        // The command palette, the issue footer's `:` hint.
        KeyCode::Char(':') => {
            app.prompt = Some(Prompt {
                kind: PromptKind::Palette,
                input: String::new(),
                status: String::new(),
            });
        }
        _ => {}
    }
    false
}

/// Handles one key while a prompt is open. `Esc` cancels, `Enter` runs,
/// `Backspace` deletes, and any printable character types.
async fn handle_prompt_key(app: &mut App, view: &WorktreesViewModel, key: KeyEvent) {
    let Some(prompt) = app.prompt.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Esc => app.prompt = None,
        KeyCode::Backspace => {
            prompt.input.pop();
            prompt.status.clear();
        }
        KeyCode::Char(c) => {
            prompt.input.push(c);
            prompt.status.clear();
        }
        KeyCode::Enter => match prompt.kind {
            PromptKind::Find => {
                let needle = prompt.input.clone();
                let found = app
                    .panes
                    .active_tab()
                    .is_some_and(|tab| tab.find_in_scrollback(&needle));
                // Stay open so `Enter` again steps to the next match back.
                if let Some(prompt) = app.prompt.as_mut() {
                    prompt.status = if found {
                        String::new()
                    } else {
                        "no match".to_string()
                    };
                }
            }
            PromptKind::Palette => {
                let query = prompt.input.clone();
                app.prompt = None;
                let targets = app.tree.targets(view);
                let matches = filter_palette(palette_entries(&targets), &query);
                match matches.first() {
                    Some(item) => {
                        let popup::MenuCommand::Action(action) = item.command;
                        if matches!(
                            action,
                            ActionKind::MoveClaudeSessionHere | ActionKind::CopyClaudeSessionHere
                        ) {
                            let mode = if action == ActionKind::MoveClaudeSessionHere {
                                RelocationMode::Move
                            } else {
                                RelocationMode::Copy
                            };
                            start_relocate_flow(app, view, mode);
                        } else {
                            // The palette opens the menu at the match rather
                            // than running it outright, so a destructive
                            // action still goes through its confirm.
                            app.menu = Some(popup::ActionMenu::new(matches));
                        }
                    }
                    None => app.notice = Some(format!("no command matching {query:?}")),
                }
            }
        },
        _ => {}
    }
}

fn handle_chrome_key(app: &mut App, view: &WorktreesViewModel, chrome: ChromeKey) {
    match chrome {
        ChromeKey::FocusTree => app.focus = Focus::Tree,
        ChromeKey::FocusTerminal => {
            if app.panes.is_empty() {
                app.notice = Some("no terminal tab — enter or alt-t opens one".to_string());
            } else {
                app.focus = Focus::Terminal;
            }
        }
        ChromeKey::NewShellTab => open_tab_for_cursor(app, view, TabKind::Shell, false),
        ChromeKey::NewClaudeTab => open_tab_for_cursor(app, view, TabKind::Claude, false),
        ChromeKey::SplitShellTab => open_tab_for_cursor(app, view, TabKind::Shell, true),
        ChromeKey::CloseTab => app.close_tab(),
        ChromeKey::NextTab => app.panes.cycle_tab(1),
        ChromeKey::PrevTab => app.panes.cycle_tab(-1),
        ChromeKey::SelectTab(index) => {
            if !app.panes.select_tab(index) {
                app.notice = Some(format!("no tab {}", index + 1));
            }
        }
        ChromeKey::NextGroup => app.panes.cycle_group(1),
        ChromeKey::PrevGroup => app.panes.cycle_group(-1),
        ChromeKey::MoveTabDown => {
            if !app.panes.move_tab_to_group(1) {
                app.notice = Some("no group below to move this tab to".to_string());
            }
        }
        ChromeKey::MoveTabUp => {
            if !app.panes.move_tab_to_group(-1) {
                app.notice = Some("no group above to move this tab to".to_string());
            }
        }
        ChromeKey::ResetLayout => app.panes.reset_weights(),
        ChromeKey::ClearAllRowColors => {
            // Non-destructive and re-doable by hand, so no confirm — the
            // same reasoning `C` (clear one row) already follows.
            let _ = app.commands.send(HubCommand::ClearAllRowColors);
            app.notice = Some("cleared every row colour".to_string());
        }
        ChromeKey::Find => {
            if app.panes.active_tab().is_some() {
                app.prompt = Some(Prompt {
                    kind: PromptKind::Find,
                    input: String::new(),
                    status: String::new(),
                });
            } else {
                app.notice = Some("no terminal tab to search".to_string());
            }
        }
        ChromeKey::Copy => {
            let text = app
                .panes
                .active_tab()
                .and_then(TerminalTab::selection_to_string);
            app.notice = Some(match text {
                Some(text) if clipboard::copy_text(&text).is_ok() => {
                    // Copying consumes the selection, as in tmux/screen.
                    if let Some(tab) = app.panes.active_tab() {
                        tab.clear_selection();
                    }
                    "copied".to_string()
                }
                Some(_) => "clipboard unavailable".to_string(),
                None => "nothing selected".to_string(),
            });
        }
        ChromeKey::ScrollPageUp => {
            if let Some(tab) = app.panes.active_tab() {
                tab.scroll(Scroll::PageUp);
            }
        }
        ChromeKey::ScrollPageDown => {
            if let Some(tab) = app.panes.active_tab() {
                tab.scroll(Scroll::PageDown);
            }
        }
    }
}

/// Opens (or focuses) a `kind` tab in the cursor worktree, in a new group
/// below when `split`. The initial size is a placeholder — the first `draw`
/// resizes the emulator to the real pane before its grid is ever read.
fn open_tab_for_cursor(app: &mut App, view: &WorktreesViewModel, kind: TabKind, split: bool) {
    let Some(worktree) = app.cursor_worktree(view) else {
        app.notice = Some("no row selected".to_string());
        return;
    };
    // Refuse a split that could not be drawn rather than silently dropping
    // the new group off the bottom of the pane.
    if split {
        let height = app.terminal_area.map_or(0, |a| a.height);
        if height < panes::min_height_for(app.panes.group_count() + 1) {
            app.notice = Some("not enough height to split — close a group first".to_string());
            return;
        }
    }
    app.open_tab(
        kind,
        worktree,
        GridSize {
            cols: 80,
            lines: 24,
        },
        split,
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

/// The path of tree row `index`, or `None` when it is past the last row.
fn row_path(view: &WorktreesViewModel, index: usize) -> Option<PathBuf> {
    TreeState::visible_rows(view)
        .get(index)?
        .path(view)
        .map(std::path::Path::to_path_buf)
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
            panes: panes::PaneLayout::default(),
            glyphs: GlyphMode::Unicode,
            prompt: None,
            reported_visible: Vec::new(),
            quit_confirm: false,
            notice: None,
            pty_tx,
            commands,
            regions: RegionMap::default(),
            terminal_area: None,
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

    fn alt_key(code: KeyCode, extra: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT | extra)
    }

    /// Installs an already-spawned tab as the app's only tab — the pane
    /// stack's equivalent of Phase 3's `app.terminal = Some(tab)`.
    #[cfg(unix)]
    fn install_tab(app: &mut App, tab: TerminalTab) {
        app.panes
            .open_tab(move |_| Ok(tab))
            .expect("installing a prebuilt tab cannot fail");
    }

    /// The first rendered group's grid rect.
    fn grid_rect(app: &App) -> Rect {
        app.regions
            .groups
            .first()
            .map(|g| g.grid)
            .expect("a terminal region")
    }

    /// The app's single active tab, for assertions.
    fn only_tab(app: &App) -> &TerminalTab {
        app.panes.active_tab().expect("a tab")
    }

    fn only_tab_mut(app: &mut App) -> Option<&mut TerminalTab> {
        let focused = app.panes.focused;
        app.panes.groups.get_mut(focused)?.active_tab_mut()
    }

    /// Shuts every tab down, so a test's children never outlive it.
    fn shutdown_all(app: &mut App) {
        for group in &mut app.panes.groups {
            for tab in &mut group.tabs {
                tab.shutdown();
            }
        }
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
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::End)).await;
        let end_selected = app.menu.as_ref().unwrap().selected;
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Home)).await;
        assert_eq!(
            app.menu.as_ref().unwrap().selected,
            0,
            "Home lands on the first item"
        );
        assert_ne!(end_selected, 0, "End had landed elsewhere first");
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
        let mut picker_terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        picker_terminal
            .draw(|frame| draw(frame, &view, &mut app))
            .unwrap();
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Down)).await;
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Up)).await;
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Enter)).await;
        assert!(matches!(
            app.relocate,
            Some(RelocateStep::PickDestination { .. })
        ));
        picker_terminal
            .draw(|frame| draw(frame, &view, &mut app))
            .unwrap();
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
        install_tab(&mut app, tab);
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

        // Opening the same worktree again focuses the tab it already has
        // rather than opening a duplicate (Phase 4b: a second worktree now
        // opens a second tab instead of being refused).
        app.focus = Focus::Tree;
        app.open_tab(
            TabKind::Shell,
            here.clone(),
            GridSize {
                cols: 80,
                lines: 24,
            },
            false,
        );
        assert_eq!(app.focus, Focus::Terminal);
        assert_eq!(app.panes.groups[0].tabs.len(), 1, "no duplicate tab");

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
        only_tab_mut(&mut app).unwrap().exit_status = None; // pretend it is still live
        handle_key(&mut app, &view, &dispatcher, alt('w')).await;
        assert!(app.panes.is_empty());
        assert_eq!(app.focus, Focus::Tree);
        assert!(
            drained_clear_open_tab(&mut commands, &here),
            "closing the last tab clears the here cue"
        );
    }

    /// Drains the hub channel looking for `ClearOpenTab(path)`. Drawing also
    /// posts `SetVisibleRows`, so the wanted command is not necessarily
    /// first in the queue.
    fn drained_clear_open_tab(
        commands: &mut mpsc::UnboundedReceiver<HubCommand>,
        path: &Path,
    ) -> bool {
        std::iter::from_fn(|| commands.try_recv().ok())
            .any(|cmd| matches!(cmd, HubCommand::ClearOpenTab(p) if p == path))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn open_tab_for_cursor_spawns_and_reports_here_beside_an_exited_tab() {
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
        install_tab(&mut app, tab);

        // A missing cursor row is a notice, not a spawn.
        let empty = WorktreesViewModel::default();
        open_tab_for_cursor(&mut app, &empty, TabKind::Shell, false);
        assert_eq!(app.notice.as_deref(), Some("no row selected"));

        // With a row: an exited tab is not reused (it keeps its grid and
        // exit status on screen until closed), so the user's shell is
        // spawned as a second tab beside it and reported open.
        open_tab_for_cursor(&mut app, &view, TabKind::Shell, false);
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
        shutdown_all(&mut app);
    }

    #[tokio::test]
    async fn mouse_on_the_tree_focuses_marks_ranges_scrolls_and_ignores_chrome() {
        let (mut app, dispatcher, _commands) = test_app();
        let view = view_with(&["/repo/a", "/repo/b", "/repo/c"]); // 4 rows with the header
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        draw_into(&mut terminal, &view, &mut app);
        let tree = app.regions.tree;
        assert_eq!((tree.x, tree.y), (1, 1), "inside the border");
        let none = KeyModifiers::NONE;
        let down = |row: u16, mods| mouse_at(MouseEventKind::Down(MouseButton::Left), 5, row, mods);

        // A drag with no drag in progress (no preceding press, or one that
        // did not start a drag) is inert.
        assert_eq!(app.drag, DragOrigin::None);
        assert!(
            !handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(MouseEventKind::Drag(MouseButton::Left), 5, 3, none)
            )
            .await
        );

        // The border is chrome; a click there does nothing.
        assert!(!handle_mouse(&mut app, &view, &dispatcher, down(0, none)).await);
        // A click on row 2 moves the cursor there.
        assert!(handle_mouse(&mut app, &view, &dispatcher, down(tree.y + 2, none)).await);
        assert_eq!(app.tree.cursor, 2);
        assert_eq!(app.drag, DragOrigin::Tree { anchor: 2 });
        assert!(
            !handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(MouseEventKind::Up(MouseButton::Left), 5, 3, none)
            )
            .await
        );
        assert_eq!(app.drag, DragOrigin::None);
        assert!(app.tree.marked.is_empty(), "a click alone marks nothing");

        // ^-click toggles a mark; ⇧-click marks the range from the cursor.
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                down(tree.y + 3, KeyModifiers::CONTROL)
            )
            .await
        );
        assert!(app.tree.marked.contains(&PathBuf::from("/repo/c")));
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                down(tree.y + 1, KeyModifiers::SHIFT)
            )
            .await
        );
        assert_eq!(app.tree.cursor, 1);
        assert_eq!(app.tree.marked.len(), 3, "rows 1..=3 marked");
        app.tree.clear_marks();

        // A drag from row 1 to below the pane range-marks down to the last
        // row and stops there (clamped to the tree).
        assert!(handle_mouse(&mut app, &view, &dispatcher, down(tree.y + 1, none)).await);
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(MouseEventKind::Drag(MouseButton::Left), 200, 200, none)
            )
            .await
        );
        assert_eq!(app.tree.cursor, 3);
        assert_eq!(app.tree.marked.len(), 3);
        handle_mouse(
            &mut app,
            &view,
            &dispatcher,
            mouse_at(MouseEventKind::Up(MouseButton::Left), 200, 200, none),
        )
        .await;

        // The wheel moves the cursor; below the last row only focus changes.
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(MouseEventKind::ScrollUp, 5, tree.y, none)
            )
            .await
        );
        assert_eq!(app.tree.cursor, 2);
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(MouseEventKind::ScrollDown, 5, tree.y, none)
            )
            .await
        );
        assert_eq!(app.tree.cursor, 3);
        assert!(handle_mouse(&mut app, &view, &dispatcher, down(tree.y + 8, none)).await);
        assert_eq!(app.tree.cursor, 3);
        // A right-click focuses without starting a drag.
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(MouseEventKind::Down(MouseButton::Right), 5, tree.y, none)
            )
            .await
        );
        assert_eq!(app.tree.cursor, 0);
        assert_eq!(app.drag, DragOrigin::None);
        assert!(
            !handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(MouseEventKind::Drag(MouseButton::Right), 5, 3, none)
            )
            .await
        );
        // Moves and horizontal wheel are inert with no terminal.
        assert!(
            !handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(MouseEventKind::Moved, 5, 3, none)
            )
            .await
        );
        assert!(
            !handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(MouseEventKind::ScrollLeft, 5, 3, none)
            )
            .await
        );

        // #1602 contract §7: a menu takes the mouse. A click outside it
        // dismisses it, and that click is *consumed* — it must not also move
        // the cursor to the row it landed on.
        app.menu = Some(popup::ActionMenu::new(Vec::new()));
        let cursor_before = app.tree.cursor;
        assert!(handle_mouse(&mut app, &view, &dispatcher, down(tree.y, none)).await);
        assert!(app.menu.is_none(), "the outside click dismissed the menu");
        assert_eq!(
            app.tree.cursor, cursor_before,
            "and was consumed, not applied to the row underneath"
        );

        // Every *other* popup stays inert, exactly as before: the confirm
        // modal's keyboard-only y/n is a feature, not an oversight.
        app.quit_confirm = true;
        assert!(!handle_mouse(&mut app, &view, &dispatcher, down(tree.y, none)).await);
        app.quit_confirm = false;
    }

    #[tokio::test]
    async fn menu_mouse_hovers_arms_scrolls_and_invokes_under_the_pointer() {
        let (mut app, dispatcher, mut commands) = test_app();
        let view = view_with(&["/repo/a"]);
        let none = KeyModifiers::NONE;
        let colors = row_colors::KNOWN_ROW_COLORS;
        app.menu = Some(popup::ActionMenu::new(vec![
            popup::MenuItem::action(ActionKind::SetRowColor(colors[0]), "a"),
            popup::MenuItem::action(ActionKind::SetRowColor(colors[1]), "b"),
        ]));
        // A hand-built region, standing in for what `draw_menu` would have
        // reported: two one-row items inside a small popup rect.
        app.regions.popup = Some(mouse::PopupRegion {
            rect: Rect::new(9, 4, 12, 4),
            items: vec![(0, Rect::new(10, 5, 10, 1)), (1, Rect::new(10, 6, 10, 1))],
        });
        let at = |kind, row| mouse_at(kind, 10, row, none);

        // Hovering a different item moves the highlight and asks for a
        // redraw; hovering the one already selected does not.
        assert!(handle_mouse(&mut app, &view, &dispatcher, at(MouseEventKind::Moved, 6)).await);
        assert_eq!(app.menu.as_ref().unwrap().selected, 1);
        assert!(!handle_mouse(&mut app, &view, &dispatcher, at(MouseEventKind::Moved, 6)).await);

        // The release that opened the menu lands here too, unarmed — it must
        // be ignored rather than invoking whatever is under it.
        assert!(
            !handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                at(MouseEventKind::Up(MouseButton::Right), 6)
            )
            .await
        );
        assert!(app.menu.is_some(), "an unarmed release does not dismiss");

        // The wheel moves the selection without arming anything.
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                at(MouseEventKind::ScrollUp, 6)
            )
            .await
        );
        assert_eq!(app.menu.as_ref().unwrap().selected, 0);
        assert!(!app.menu.as_ref().unwrap().armed);
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                at(MouseEventKind::ScrollDown, 6)
            )
            .await
        );
        assert_eq!(app.menu.as_ref().unwrap().selected, 1);

        // A press that starts inside the menu arms it and selects under the
        // pointer; the matching release then invokes that item and closes
        // the menu.
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                at(MouseEventKind::Down(MouseButton::Right), 5)
            )
            .await
        );
        assert!(app.menu.as_ref().unwrap().armed);
        assert_eq!(app.menu.as_ref().unwrap().selected, 0);
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                at(MouseEventKind::Up(MouseButton::Right), 5)
            )
            .await
        );
        assert!(app.menu.is_none(), "invoking a command closes the menu");
        assert!(matches!(
            commands.try_recv(),
            Ok(HubCommand::SetRowColor(..))
        ));
        assert!(matches!(app.flow, ActionFlow::Done { .. }));
    }

    #[tokio::test]
    async fn menu_command_move_and_copy_claude_session_start_the_relocate_wizard() {
        let (mut app, dispatcher, _commands) = test_app();
        let view = view_with(&["/repo/a"]);

        // Both variants route through the same relocate wizard — with no
        // Claude sessions for this row, it fails cleanly rather than hanging
        // (mirrors `relocate_wizard_fails_cleanly_without_sessions_and_cancels`).
        app.menu = Some(popup::ActionMenu::new(Vec::new()));
        invoke_menu_command(
            &mut app,
            &view,
            &dispatcher,
            popup::MenuCommand::Action(ActionKind::MoveClaudeSessionHere),
        )
        .await;
        assert!(app.menu.is_none());
        assert!(matches!(app.flow, ActionFlow::Failed { .. }));
        app.flow = ActionFlow::Idle;

        app.menu = Some(popup::ActionMenu::new(Vec::new()));
        invoke_menu_command(
            &mut app,
            &view,
            &dispatcher,
            popup::MenuCommand::Action(ActionKind::CopyClaudeSessionHere),
        )
        .await;
        assert!(app.menu.is_none());
        assert!(matches!(app.flow, ActionFlow::Failed { .. }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_double_click_on_a_row_opens_a_shell_tab_there() {
        let (mut app, dispatcher, _commands) = test_app();
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
        handle_mouse(&mut app, &view, &dispatcher, click).await;
        handle_mouse(
            &mut app,
            &view,
            &dispatcher,
            mouse_at(
                MouseEventKind::Up(MouseButton::Left),
                5,
                y,
                KeyModifiers::NONE,
            ),
        )
        .await;
        handle_mouse(&mut app, &view, &dispatcher, click).await;
        assert_eq!(app.tree.cursor, 1);
        // Either the login shell spawned (and took focus) or, on a minimal
        // image, the failure is reported — never silence.
        assert!(
            !app.panes.is_empty() || app.notice.is_some(),
            "double-click neither opened a tab nor reported a failure"
        );
        if !app.panes.is_empty() {
            assert_eq!(app.focus, Focus::Terminal);
        }
        app.close_tab();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn mouse_on_the_terminal_selects_by_drag_word_and_line_and_scrolls() {
        let (mut app, dispatcher, _commands) = test_app();
        let dir = tempfile::tempdir().unwrap();
        let view = view_with(&[&dir.path().to_string_lossy()]);
        let tab = scripted_tab(
            &app,
            "printf 'hello world\\nsecond line\\n'; sleep 3",
            dir.path().to_path_buf(),
        );
        install_tab(&mut app, tab);
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        draw_into(&mut terminal, &view, &mut app);
        let grid = grid_rect(&app);
        assert!(
            wait_until(|| only_tab(&app).selection_to_string().is_none() && {
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
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                at(MouseEventKind::Down(MouseButton::Left), 0, 0)
            )
            .await
        );
        assert_eq!(app.focus, Focus::Terminal);
        assert_eq!(app.drag, DragOrigin::Terminal { group: 0 });
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                at(MouseEventKind::Drag(MouseButton::Left), 4, 0)
            )
            .await
        );
        assert!(
            !handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                at(MouseEventKind::Up(MouseButton::Left), 4, 0)
            )
            .await
        );
        assert_eq!(app.drag, DragOrigin::None);
        assert_eq!(
            only_tab(&app).selection_to_string().as_deref(),
            Some("hello")
        );
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                at(MouseEventKind::Down(MouseButton::Left), 0, 1)
            )
            .await
        );
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(MouseEventKind::Drag(MouseButton::Left), 0, 0, none)
            )
            .await
        );
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(MouseEventKind::Drag(MouseButton::Left), 500, 500, none)
            )
            .await
        );
        let clamped = only_tab(&app).selection_to_string().unwrap();
        assert!(clamped.starts_with("second line"), "got {clamped:?}");
        handle_mouse(
            &mut app,
            &view,
            &dispatcher,
            at(MouseEventKind::Up(MouseButton::Left), 0, 1),
        )
        .await;

        // Double-click selects the word, triple-click the line.
        let dbl = at(MouseEventKind::Down(MouseButton::Left), 7, 0);
        handle_mouse(&mut app, &view, &dispatcher, dbl).await;
        handle_mouse(
            &mut app,
            &view,
            &dispatcher,
            at(MouseEventKind::Up(MouseButton::Left), 7, 0),
        )
        .await;
        handle_mouse(&mut app, &view, &dispatcher, dbl).await;
        assert_eq!(
            only_tab(&app).selection_to_string().as_deref(),
            Some("world")
        );
        handle_mouse(
            &mut app,
            &view,
            &dispatcher,
            at(MouseEventKind::Up(MouseButton::Left), 7, 0),
        )
        .await;
        handle_mouse(&mut app, &view, &dispatcher, dbl).await;
        // A line selection's text carries the line's own trailing newline.
        let line = only_tab(&app).selection_to_string();
        assert_eq!(line.as_deref().map(str::trim_end), Some("hello world"));
        handle_mouse(
            &mut app,
            &view,
            &dispatcher,
            at(MouseEventKind::Up(MouseButton::Left), 7, 0),
        )
        .await;
        draw_into(&mut terminal, &view, &mut app); // the selection renders
                                                   // alt-c copies (OSC 52 fallback in CI) and consumes the selection.
        handle_chrome_key(&mut app, &view, ChromeKey::Copy);
        assert_eq!(app.notice.as_deref(), Some("copied"));
        assert!(only_tab(&app).selection_to_string().is_none());

        // The wheel scrolls the emulator's history (no child mouse mode, not
        // the alt screen); a middle click neither pastes nor selects.
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                at(MouseEventKind::ScrollUp, 1, 1)
            )
            .await
        );
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                at(MouseEventKind::ScrollDown, 1, 1)
            )
            .await
        );
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                at(MouseEventKind::Down(MouseButton::Middle), 1, 1)
            )
            .await
        );
        assert_eq!(app.drag, DragOrigin::None);
        assert!(
            !handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                at(MouseEventKind::Moved, 1, 1)
            )
            .await
        );
        assert!(
            !handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                at(MouseEventKind::ScrollRight, 1, 1)
            )
            .await
        );
        app.close_tab();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_child_that_asked_for_the_mouse_receives_it_unless_alt_is_held() {
        let (mut app, dispatcher, _commands) = test_app();
        let dir = tempfile::tempdir().unwrap();
        let view = view_with(&[&dir.path().to_string_lossy()]);
        // Enable SGR click reporting, then print the first nine bytes the
        // TUI sends (exactly one SGR press) in a form the grid can show.
        let script = "stty -echo -icanon min 1 time 0; printf '\\033[?1000h\\033[?1006h'; \
                      dd bs=1 count=9 2>/dev/null | od -An -c; sleep 2";
        let tab = scripted_tab(&app, script, dir.path().to_path_buf());
        install_tab(&mut app, tab);
        let mut terminal = Terminal::new(TestBackend::new(100, 12)).unwrap();
        draw_into(&mut terminal, &view, &mut app);
        let grid = grid_rect(&app);
        assert!(
            wait_until(|| only_tab(&app).mode().contains(TermMode::SGR_MOUSE)).await,
            "the child never enabled mouse reporting"
        );
        let none = KeyModifiers::NONE;

        // Alt-click: the TUI keeps the mouse and starts a selection.
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(
                    MouseEventKind::Down(MouseButton::Left),
                    grid.x,
                    grid.y,
                    KeyModifiers::ALT
                )
            )
            .await
        );
        assert_eq!(app.drag, DragOrigin::Terminal { group: 0 });
        handle_mouse(
            &mut app,
            &view,
            &dispatcher,
            mouse_at(
                MouseEventKind::Up(MouseButton::Left),
                grid.x,
                grid.y,
                KeyModifiers::ALT,
            ),
        )
        .await;

        // A plain click goes to the child, as does its release and any
        // motion in between; the child prints what it got.
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(
                    MouseEventKind::Down(MouseButton::Left),
                    grid.x,
                    grid.y,
                    none
                )
            )
            .await
        );
        assert_eq!(app.drag, DragOrigin::Child { group: 0 });
        assert!(
            !handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(
                    MouseEventKind::Drag(MouseButton::Left),
                    grid.x + 1,
                    grid.y,
                    none
                )
            )
            .await
        );
        assert!(
            !handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(
                    MouseEventKind::Up(MouseButton::Left),
                    grid.x + 1,
                    grid.y,
                    none
                )
            )
            .await
        );
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
        assert!(
            !handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(MouseEventKind::ScrollUp, grid.x, grid.y, none)
            )
            .await
        );
        app.close_tab();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tab_chords_split_cycle_select_move_and_reset_the_stack() {
        let (mut app, dispatcher, mut commands) = test_app();
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_path_buf();
        let view = view_with(&[&here.to_string_lossy(), "/repo/other"]);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

        // Seed one tab, then draw so a terminal area exists for splits.
        let tab = scripted_tab(&app, "sleep 5", here.clone());
        install_tab(&mut app, tab);
        draw_into(&mut terminal, &view, &mut app);
        assert_eq!(app.panes.group_count(), 1);

        // alt-s splits into a second group; alt-↑/↓ move focus between them.
        app.tree.cursor = 1;
        handle_key(&mut app, &view, &dispatcher, alt('s')).await;
        assert_eq!(app.panes.group_count(), 2, "alt-s split the stack");
        assert_eq!(app.panes.focused, 1);
        draw_into(&mut terminal, &view, &mut app);
        handle_key(
            &mut app,
            &view,
            &dispatcher,
            alt_key(KeyCode::Up, KeyModifiers::NONE),
        )
        .await;
        assert_eq!(app.panes.focused, 0);
        handle_key(
            &mut app,
            &view,
            &dispatcher,
            alt_key(KeyCode::Down, KeyModifiers::NONE),
        )
        .await;
        assert_eq!(app.panes.focused, 1);

        // alt-⇧↑ moves the tab into the group above, emptying its own.
        handle_key(
            &mut app,
            &view,
            &dispatcher,
            alt_key(KeyCode::Up, KeyModifiers::SHIFT),
        )
        .await;
        assert_eq!(app.panes.group_count(), 1);
        assert_eq!(app.panes.groups[0].tabs.len(), 2);
        // With one group there is nowhere to move to, and it says so.
        handle_key(
            &mut app,
            &view,
            &dispatcher,
            alt_key(KeyCode::Down, KeyModifiers::SHIFT),
        )
        .await;
        assert!(app.notice.as_deref().unwrap_or("").contains("no group"));

        // alt-[ / alt-] cycle tabs, alt-1..9 select, out of range notices.
        assert_eq!(app.panes.groups[0].active, 1);
        handle_key(&mut app, &view, &dispatcher, alt(']')).await;
        assert_eq!(app.panes.groups[0].active, 0, "wraps");
        handle_key(&mut app, &view, &dispatcher, alt('[')).await;
        assert_eq!(app.panes.groups[0].active, 1);
        handle_key(&mut app, &view, &dispatcher, alt('1')).await;
        assert_eq!(app.panes.groups[0].active, 0);
        handle_key(&mut app, &view, &dispatcher, alt('9')).await;
        assert!(app.notice.as_deref().unwrap_or("").contains("no tab 9"));

        // alt-0 resets the weights.
        app.panes.weights = vec![7];
        handle_key(&mut app, &view, &dispatcher, alt('0')).await;
        assert_eq!(app.panes.weights, vec![1]);

        // Both tabs are on one worktree, so the cue only clears at the last
        // close — the ref-counted `here` rule.
        while let Ok(cmd) = commands.try_recv() {
            drop(cmd); // discard the opens
        }
        handle_key(&mut app, &view, &dispatcher, alt('w')).await;
        assert!(app.panes.any_alive(), "one tab left");
        assert!(
            !drained_clear_open_tab(&mut commands, &here),
            "the cue stays while a tab remains"
        );
        handle_key(&mut app, &view, &dispatcher, alt('w')).await;
        assert!(app.panes.is_empty());
        assert!(
            drained_clear_open_tab(&mut commands, &here),
            "the last close clears the cue"
        );
        assert_eq!(app.focus, Focus::Tree);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_split_is_refused_when_the_pane_is_too_short_to_draw_it() {
        let (mut app, dispatcher, _commands) = test_app();
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_path_buf();
        let view = view_with(&[&here.to_string_lossy()]);
        let tab = scripted_tab(&app, "sleep 5", here);
        install_tab(&mut app, tab);
        // Six rows of terminal cannot hold two four-row groups.
        let mut terminal = Terminal::new(TestBackend::new(100, 7)).unwrap();
        draw_into(&mut terminal, &view, &mut app);
        app.tree.cursor = 1;
        handle_key(&mut app, &view, &dispatcher, alt('s')).await;
        assert_eq!(app.panes.group_count(), 1, "the split was refused");
        assert!(app
            .notice
            .as_deref()
            .unwrap_or("")
            .contains("not enough height"));
        app.close_tab();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_tab_strip_activates_closes_and_the_splitter_resizes() {
        let (mut app, dispatcher, _commands) = test_app();
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_path_buf();
        let view = view_with(&[&here.to_string_lossy()]);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();

        // Two tabs in one group, then a second group below.
        for _ in 0..2 {
            let tab = scripted_tab(&app, "sleep 5", here.clone());
            install_tab(&mut app, tab);
        }
        let tab = scripted_tab(&app, "sleep 5", here);
        app.panes.split(move |_| Ok(tab)).unwrap();
        draw_into(&mut terminal, &view, &mut app);
        assert_eq!(app.regions.groups.len(), 2);
        assert_eq!(app.regions.splitters.len(), 1);

        // Clicking tab 1 on the first strip activates it and focuses group 0.
        let strip = app.regions.groups[0].strip;
        let spans = app.regions.groups[0].tab_spans.clone();
        assert_eq!(spans.len(), 2, "two tabs on the strip");
        let none = KeyModifiers::NONE;
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(
                    MouseEventKind::Down(MouseButton::Left),
                    spans[0].0,
                    strip.y,
                    none
                )
            )
            .await
        );
        assert_eq!(app.panes.focused, 0);
        assert_eq!(app.panes.groups[0].active, 0);
        assert_eq!(app.focus, Focus::Terminal);

        // The wheel over a strip cycles that group's tabs.
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(MouseEventKind::ScrollDown, spans[0].0, strip.y, none)
            )
            .await
        );
        assert_eq!(app.panes.groups[0].active, 1);

        // Dragging the splitter up shrinks the first group; only the pair
        // either side changes, and it never goes below the minimum.
        let splitter = app.regions.splitters[0];
        let before = app.panes.weights.clone();
        // Pressing on a splitter changes nothing visible — it only arms the
        // drag — so it asks for no redraw.
        assert!(
            !handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(MouseEventKind::Down(MouseButton::Left), 60, splitter, none)
            )
            .await
        );
        assert_eq!(app.drag, DragOrigin::Splitter { index: 0 });
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(
                    MouseEventKind::Drag(MouseButton::Left),
                    60,
                    splitter - 4,
                    none
                )
            )
            .await
        );
        assert_ne!(app.panes.weights, before, "the drag moved the boundary");
        draw_into(&mut terminal, &view, &mut app);
        let rects = app.panes.arrange(app.terminal_area.unwrap());
        assert!(rects.iter().all(|r| r.body.height >= 3));
        handle_mouse(
            &mut app,
            &view,
            &dispatcher,
            mouse_at(
                MouseEventKind::Up(MouseButton::Left),
                60,
                splitter - 4,
                none,
            ),
        )
        .await;
        assert_eq!(app.drag, DragOrigin::None);

        // A middle click on a tab closes just that tab.
        draw_into(&mut terminal, &view, &mut app);
        let spans = app.regions.groups[0].tab_spans.clone();
        let strip_y = app.regions.groups[0].strip.y;
        let before = app.panes.groups[0].tabs.len();
        assert!(
            handle_mouse(
                &mut app,
                &view,
                &dispatcher,
                mouse_at(
                    MouseEventKind::Down(MouseButton::Middle),
                    spans[0].0,
                    strip_y,
                    none
                )
            )
            .await
        );
        assert_eq!(app.panes.groups[0].tabs.len(), before - 1);

        shutdown_all(&mut app);
    }

    #[tokio::test]
    async fn the_command_palette_filters_by_label_and_opens_the_menu_at_the_match() {
        let (mut app, dispatcher, _commands) = test_app();
        let view = view_with(&["/repo/a"]);
        app.tree.cursor = 1;

        // `:` opens the palette; letters type into it rather than running
        // tree commands (`c` would otherwise open the colour picker).
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char(':'))).await;
        assert!(app.prompt.is_some());
        for c in "copy dir".chars() {
            handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char(c))).await;
        }
        assert!(app.menu.is_none(), "typing did not trigger a tree command");
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        draw_into(&mut terminal, &view, &mut app);
        assert!(buffer_text(&terminal).contains("copy dir"));

        // Backspace deletes; Enter opens the menu at the filtered match so
        // the action still goes through its normal confirm path.
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Backspace)).await;
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Enter)).await;
        assert!(app.prompt.is_none());
        assert!(app.menu.is_some(), "the palette opened the action menu");
        draw_into(&mut terminal, &view, &mut app);
        app.menu = None;

        // A query matching nothing says so instead of doing something else.
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char(':'))).await;
        for c in "zzzz".chars() {
            handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char(c))).await;
        }
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Enter)).await;
        assert!(app.menu.is_none());
        assert!(app.notice.as_deref().unwrap_or("").contains("no command"));

        // Esc cancels without running anything.
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char(':'))).await;
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Esc)).await;
        assert!(app.prompt.is_none());
    }

    #[test]
    fn filter_palette_matches_case_insensitively_and_keeps_everything_when_empty() {
        let items = vec![
            popup::MenuItem::action(ActionKind::CopyDirectory, "Copy Directory"),
            popup::MenuItem::action(ActionKind::Focus, "Focus Worktree"),
        ];
        assert_eq!(filter_palette(items.clone(), "").len(), 2);
        assert_eq!(filter_palette(items.clone(), "copy").len(), 1);
        assert_eq!(filter_palette(items.clone(), "COPY").len(), 1);
        assert_eq!(filter_palette(items.clone(), "work").len(), 1);
        assert!(filter_palette(items, "nothing").is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn find_searches_the_scrollback_and_reports_a_miss() {
        let (mut app, dispatcher, _commands) = test_app();
        let dir = tempfile::tempdir().unwrap();
        let here = dir.path().to_path_buf();
        let view = view_with(&[&here.to_string_lossy()]);

        // Without a tab there is nothing to search, and it says so.
        handle_key(&mut app, &view, &dispatcher, alt('f')).await;
        assert!(app.prompt.is_none());
        assert!(app.notice.as_deref().unwrap_or("").contains("no terminal"));

        // Print enough lines to push the needle into scrollback.
        let tab = scripted_tab(
            &app,
            "i=0; while [ $i -lt 60 ]; do echo line-$i; i=$((i+1)); done; \
             echo NEEDLE-HERE; i=0; while [ $i -lt 60 ]; do echo tail-$i; i=$((i+1)); done; \
             sleep 3",
            here,
        );
        install_tab(&mut app, tab);
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        assert!(
            wait_until(|| {
                draw_into(&mut terminal, &view, &mut app);
                buffer_text(&terminal).contains("tail-59")
            })
            .await,
            "the child never finished printing"
        );

        handle_key(&mut app, &view, &dispatcher, alt('f')).await;
        assert!(app.prompt.is_some());
        for c in "needle".chars() {
            handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char(c))).await;
        }
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Enter)).await;
        // A hit scrolls the display back and leaves no error status.
        let prompt = app.prompt.as_ref().expect("the find prompt stays open");
        assert_eq!(prompt.status, "", "case-insensitive match should be found");
        draw_into(&mut terminal, &view, &mut app);
        assert!(
            buffer_text(&terminal).contains("NEEDLE-HERE"),
            "the match was scrolled into view"
        );

        // A miss reports it rather than scrolling somewhere arbitrary.
        for _ in 0..6 {
            handle_key(&mut app, &view, &dispatcher, press(KeyCode::Backspace)).await;
        }
        for c in "nowhere".chars() {
            handle_key(&mut app, &view, &dispatcher, press(KeyCode::Char(c))).await;
        }
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Enter)).await;
        assert_eq!(app.prompt.as_ref().unwrap().status, "no match");
        handle_key(&mut app, &view, &dispatcher, press(KeyCode::Esc)).await;
        assert!(app.prompt.is_none());
        shutdown_all(&mut app);
    }

    #[tokio::test]
    async fn visible_rows_are_reported_once_per_change_not_once_per_frame() {
        let (mut app, _dispatcher, mut commands) = test_app();
        let view = view_with(&["/repo/a", "/repo/b"]);
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();

        draw_into(&mut terminal, &view, &mut app);
        let first = commands.try_recv();
        match first {
            Ok(HubCommand::SetVisibleRows(rows)) => {
                assert_eq!(rows.len(), 2, "both worktree rows are visible");
                assert!(rows.iter().all(|p| p.starts_with("/repo/")));
                assert!(
                    !rows.contains(&PathBuf::from("/repo")),
                    "the repo header is not a worktree row"
                );
            }
            other => panic!("expected SetVisibleRows, got {other:?}"),
        }
        // Redrawing the same view must not re-send: the command triggers
        // ahead/behind fetches, and repeating it every frame would restart
        // them ~60 times a second.
        for _ in 0..5 {
            draw_into(&mut terminal, &view, &mut app);
        }
        assert!(
            commands.try_recv().is_err(),
            "an unchanged visible set was re-reported"
        );

        // A pane too short to show every row reports only what fits.
        let mut small = Terminal::new(TestBackend::new(80, 4)).unwrap();
        draw_into(&mut small, &view, &mut app);
        match commands.try_recv() {
            Ok(HubCommand::SetVisibleRows(rows)) => assert!(
                rows.len() < 2,
                "a 4-row pane cannot show both worktrees: {rows:?}"
            ),
            other => panic!("expected a new SetVisibleRows, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn alt_shift_c_clears_every_row_colour() {
        let (mut app, dispatcher, mut commands) = test_app();
        let view = view_with(&["/repo/a"]);
        app.tree.cursor = 1;

        handle_key(
            &mut app,
            &view,
            &dispatcher,
            KeyEvent::new(KeyCode::Char('C'), KeyModifiers::ALT),
        )
        .await;
        assert!(matches!(
            commands.try_recv(),
            Ok(HubCommand::ClearAllRowColors)
        ));
        assert_eq!(app.notice.as_deref(), Some("cleared every row colour"));

        // The shifted spelling with the SHIFT bit set reaches it too.
        handle_key(
            &mut app,
            &view,
            &dispatcher,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::ALT | KeyModifiers::SHIFT),
        )
        .await;
        assert!(matches!(
            commands.try_recv(),
            Ok(HubCommand::ClearAllRowColors)
        ));

        // Plain alt-c is still Copy, not a clear-all.
        handle_key(&mut app, &view, &dispatcher, alt('c')).await;
        assert_eq!(app.notice.as_deref(), Some("nothing selected"));
        assert!(commands.try_recv().is_err());
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
