//! One embedded terminal tab (issue #1585 Phase 3): the emulator state a
//! child process draws into, the channel that feeds it input, and the
//! ratatui view of its grid.
//!
//! The pane model is deliberately minimal this phase — exactly one tab, no
//! tab strip, no splits. Phase 4 adds `PaneGroup`s, the tab strip, the
//! splitter, and the mouse/selection contract on top of this type; a tab
//! already remembers the worktree it was opened in (`opened_in`), which is
//! what lets the tree's "you are here" cue work without registering with the
//! daemon (issue #1585 §7).
//!
//! **Security:** nothing here logs or persists grid contents, `PtyWrite`
//! replies, selection text, or clipboard text — see
//! `no_pty_content_is_ever_logged` below.

pub mod cells;
pub mod pty;

use std::borrow::Cow;
use std::path::PathBuf;
use std::process::ExitStatus;

use alacritty_terminal::event::Event as TermEvent;
use alacritty_terminal::event_loop::Msg;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{point_to_viewport, viewport_to_point, Term, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape, Rgb};
use anyhow::Result;
use ratatui::buffer::CellDiffOption;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders};
use ratatui::Frame;
use tokio::sync::mpsc;

pub use pty::{GridSize, TabId};

/// What a tab runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabKind {
    /// The user's default shell.
    Shell,
    /// `claude`, launched through `omni-dev claude-wrap` so the session
    /// reports authoritative state to the daemon (ADR-0057) — the one thing
    /// the TUI can do that the VS Code companion cannot.
    Claude,
}

impl TabKind {
    fn label(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Claude => "claude",
        }
    }
}

/// What the app should do after a tab has absorbed one emulator event.
#[derive(Debug, PartialEq, Eq)]
pub enum TabEffect {
    /// Nothing visible changed.
    None,
    /// New content — schedule a redraw (coalesced by the app, not per event).
    Redraw,
    /// The child asked to store text on the clipboard (OSC 52).
    CopyToClipboard(String),
    /// The child exited; the grid stays visible until the tab is closed.
    Exited,
}

/// One live (or just-exited) terminal tab.
pub struct TerminalTab {
    id: TabId,
    pub kind: TabKind,
    /// The worktree this tab was opened in — the local, daemon-free source
    /// of the tree's `here` cue.
    pub opened_in: PathBuf,
    handle: Option<pty::PtyHandle>,
    pub title: Option<String>,
    pub exit_status: Option<ExitStatus>,
    size: GridSize,
}

impl TerminalTab {
    /// Spawns `kind` in `opened_in` at `size`, reporting emulator events for
    /// `id` on `tx`.
    pub fn spawn(
        id: TabId,
        kind: TabKind,
        opened_in: PathBuf,
        size: GridSize,
        tx: mpsc::UnboundedSender<(TabId, TermEvent)>,
    ) -> Result<Self> {
        let program = match kind {
            TabKind::Shell => None,
            TabKind::Claude => {
                let exe = std::env::current_exe().map_or_else(
                    |_| "omni-dev".to_string(),
                    |p| p.to_string_lossy().into_owned(),
                );
                Some((
                    exe,
                    vec![
                        "claude-wrap".to_string(),
                        "--".to_string(),
                        "claude".to_string(),
                    ],
                ))
            }
        };
        let request = pty::SpawnRequest {
            tab: id,
            program,
            cwd: opened_in,
            size,
            extra_env: Vec::new(),
        };
        Self::from_request(kind, request, tx)
    }

    /// Spawns whatever `request` names — the seam that lets tests run a
    /// scripted `/bin/sh` through the exact code path `spawn` uses.
    pub(crate) fn from_request(
        kind: TabKind,
        request: pty::SpawnRequest,
        tx: mpsc::UnboundedSender<(TabId, TermEvent)>,
    ) -> Result<Self> {
        let handle = pty::spawn(&request, tx)?;
        Ok(Self {
            id: request.tab,
            kind,
            opened_in: request.cwd,
            handle: Some(handle),
            title: None,
            exit_status: None,
            size: request.size,
        })
    }

    pub fn id(&self) -> TabId {
        self.id
    }

    pub fn is_alive(&self) -> bool {
        self.handle.is_some() && self.exit_status.is_none()
    }

    /// Absorbs one emulator event. The `PtyWrite` arm is the load-bearing
    /// one (issue #1585 §5b): the emulator's replies to a child's queries
    /// must go back to the child or it hangs.
    pub fn handle_event(&mut self, event: TermEvent) -> TabEffect {
        match event {
            TermEvent::PtyWrite(reply) => {
                self.write_input(reply.into_bytes());
                TabEffect::None
            }
            TermEvent::ClipboardStore(_, text) => TabEffect::CopyToClipboard(text),
            // With the emulator's default `Osc52::OnlyCopy`, a paste request
            // is never surfaced; if that ever changes this stays a no-op —
            // the TUI must not add a path from the clipboard into the child
            // without a user gesture (issue #1585, Security).
            TermEvent::ClipboardLoad(..) => TabEffect::None,
            TermEvent::ColorRequest(index, format) => {
                self.write_input(format(default_palette_rgb(index)).into_bytes());
                TabEffect::None
            }
            TermEvent::TextAreaSizeRequest(format) => {
                self.write_input(format(self.size.window_size()).into_bytes());
                TabEffect::None
            }
            TermEvent::Title(title) => {
                self.title = Some(title);
                TabEffect::Redraw
            }
            TermEvent::ResetTitle => {
                self.title = None;
                TabEffect::Redraw
            }
            TermEvent::ChildExit(status) => {
                self.exit_status = Some(status);
                TabEffect::Exited
            }
            TermEvent::Exit => TabEffect::Exited,
            TermEvent::Wakeup => TabEffect::Redraw,
            TermEvent::Bell | TermEvent::MouseCursorDirty | TermEvent::CursorBlinkingChange => {
                TabEffect::None
            }
        }
    }

    /// Queues `bytes` for the child's stdin.
    pub fn write_input(&self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if let Some(handle) = &self.handle {
            let _ = handle.sender.send(Msg::Input(Cow::Owned(bytes)));
        }
    }

    /// Resizes the grid **and** the PTY. Both are required and neither is
    /// sufficient: `Term::resize` rewraps the in-memory grid, `Msg::Resize`
    /// is the `TIOCSWINSZ`/`SIGWINCH` that tells the child. A splitter drag
    /// (Phase 4) goes through this same path.
    pub fn resize(&mut self, size: GridSize) {
        if size == self.size || size.cols < 2 || size.lines < 1 {
            return;
        }
        self.size = size;
        if let Some(handle) = &self.handle {
            handle.term.lock().resize(size);
            let _ = handle.sender.send(Msg::Resize(size.window_size()));
        }
    }

    /// Searches the scrollback for `needle`, case-insensitively, and
    /// scrolls the display so the nearest match above the current viewport
    /// is on screen. Returns whether a match was found.
    ///
    /// The search reads the emulator's grid line by line, so a soft-wrapped
    /// match is found on the row it starts on. **The scanned text is never
    /// logged or returned** — only the fact of a match and the scroll it
    /// caused leave this method (issue #1585, Security).
    pub fn find_in_scrollback(&self, needle: &str) -> bool {
        if needle.is_empty() {
            return false;
        }
        let Some(handle) = &self.handle else {
            return false;
        };
        let needle = needle.to_lowercase();
        let mut term = handle.term.lock();
        let grid = term.grid();
        let columns = grid.columns();
        // With display offset `d` the viewport's top line is `Line(-d)`,
        // and history runs back to `Line(-history)`.
        let history = grid.total_lines() - grid.screen_lines();
        let current = grid.display_offset();

        // Walk back from just above the current viewport towards the oldest
        // line, so repeated searches step further through history.
        let found = ((current + 1)..=history).find(|offset| {
            let line = Line(-i32::try_from(*offset).unwrap_or(i32::MAX));
            let text: String = (0..columns).map(|col| grid[line][Column(col)].c).collect();
            text.to_lowercase().contains(&needle)
        });
        match found {
            Some(offset) => {
                let delta =
                    i32::try_from(offset).unwrap_or(i32::MAX) - i32::try_from(current).unwrap_or(0);
                term.scroll_display(Scroll::Delta(delta));
                true
            }
            None => false,
        }
    }

    pub fn scroll(&self, scroll: Scroll) {
        if let Some(handle) = &self.handle {
            handle.term.lock().scroll_display(scroll);
        }
    }

    pub fn mode(&self) -> TermMode {
        self.handle
            .as_ref()
            .map(|h| *h.term.lock().mode())
            .unwrap_or_default()
    }

    /// The current selection's text (soft-wrap-aware, from the emulator's
    /// grid — never from the rendered frame).
    pub fn selection_to_string(&self) -> Option<String> {
        self.handle
            .as_ref()
            .and_then(|h| h.term.lock().selection_to_string())
    }

    /// Starts a selection of type `ty` at viewport cell (`col`, `line`) —
    /// `Simple` for a drag, `Semantic` (word) for a double-click, `Lines`
    /// for a triple — replacing any existing one. The viewport position is
    /// resolved against the current scrollback offset so a selection made
    /// while scrolled back lands on the right history line.
    pub fn selection_start(&self, col: u16, line: u16, ty: SelectionType) {
        let Some(handle) = &self.handle else {
            return;
        };
        let mut term = handle.term.lock();
        let point = viewport_point(&term, col, line);
        term.selection = Some(Selection::new(ty, point, Side::Left));
    }

    /// Extends the selection in progress to viewport cell (`col`, `line`).
    pub fn selection_update(&self, col: u16, line: u16) {
        let Some(handle) = &self.handle else {
            return;
        };
        let mut term = handle.term.lock();
        let point = viewport_point(&term, col, line);
        if let Some(selection) = term.selection.as_mut() {
            selection.update(point, Side::Right);
        }
    }

    pub fn clear_selection(&self) {
        if let Some(handle) = &self.handle {
            handle.term.lock().selection = None;
        }
    }

    /// Selects the entire scrollback and screen content. The selection spans
    /// from the topmost scrollback line at column 0 to the last cell of the
    /// last screen line, and can be extracted via `selection_to_string()`.
    pub fn select_all(&self) {
        let Some(handle) = &self.handle else {
            return;
        };
        let mut term = handle.term.lock();
        let grid = term.grid();

        // Start at the topmost line of scrollback, column 0.
        let history = grid.total_lines() - grid.screen_lines();
        let start = Point::new(Line(-i32::try_from(history).unwrap_or(i32::MAX)), Column(0));

        // End at the last cell of the last screen line.
        let end = Point::new(
            Line(i32::try_from(grid.screen_lines().saturating_sub(1)).unwrap_or(0)),
            Column(grid.columns().saturating_sub(1)),
        );

        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(end, Side::Right);
        term.selection = Some(selection);
    }

    /// Resets the display offset to the live edge, scrolling the viewport to
    /// the bottom of the scrollback and live input area.
    pub fn scroll_to_bottom(&self) {
        self.scroll(Scroll::Bottom);
    }

    /// Returns the cursor's 0-based (column, line) position within the current
    /// viewport, or `None` if there is no live terminal or the cursor is
    /// scrolled out of view.
    pub fn cursor_position(&self) -> Option<(u16, u16)> {
        let handle = self.handle.as_ref()?;
        let term = handle.term.lock();
        let content = term.renderable_content();

        if content.cursor.shape == CursorShape::Hidden {
            return None;
        }

        let cursor = point_to_viewport(content.display_offset, content.cursor.point)?;
        let col = u16::try_from(cursor.column.0).ok()?;
        let row = u16::try_from(cursor.line).ok()?;

        Some((col, row))
    }

    /// Stops the PTY thread and reaps the child.
    ///
    /// The join — and the `Pty` drop inside it, which `SIGHUP`s the child and
    /// then `wait()`s — runs on a plain, detached OS thread so a slow exit
    /// never stalls the event loop. But "off the event loop" was never the
    /// whole problem (#1605): that `wait()` has no deadline, and dropping the
    /// `tokio` runtime waits for blocking tasks, so a child that ignores
    /// `SIGHUP` could keep the *process* alive after the UI had already
    /// restored the terminal and handed back the prompt.
    ///
    /// So the group is reaped first, with escalation, and only then is the
    /// thread joined — by which point the drop's `wait()` returns at once.
    ///
    /// **Why a raw `std::thread`, not `tokio::task::spawn_blocking` (#1611).**
    /// The obvious choice is `spawn_blocking`, and that is what this used to
    /// do — but its future is tracked by the runtime even though nobody ever
    /// awaits it, so `Runtime::drop()` still blocks until it finishes (in a
    /// `#[tokio::test]`'s per-test runtime, and in `main.rs`'s). That
    /// reintroduces exactly the class of hang #1605 fixed, this time from a
    /// second, independent cause: on macOS, a `TabKind::Shell` tab's child is
    /// `/usr/bin/login` (see `spawn` above), and a killed `login` can land in
    /// the kernel's own exit teardown (`ps` shows state `Es+`) and never
    /// finish it — confirmed live via `lldb`, reading the actual pid argument
    /// of the blocked `wait4()` syscall. The reap escalation above still
    /// genuinely kills it; it is the **kernel's own reap** that stalls, which
    /// nothing in this process can bound. So instead of trying to bound the
    /// unboundable, the join and drop move to a plain OS thread that is
    /// spawned and immediately detached (its `JoinHandle` dropped) — nothing
    /// ever waits on it, so a stuck kernel-side teardown can no longer wedge
    /// a test's runtime, or the real CLI's, on shutdown.
    pub fn shutdown(&mut self) {
        let Some(mut handle) = self.handle.take() else {
            return;
        };
        let _ = handle.sender.send(Msg::Shutdown);
        let pid = handle.child_pid;
        if let Some(thread) = handle.take_thread() {
            let _ = std::thread::Builder::new()
                .name("terminal-tab-reap".into())
                .spawn(move || {
                    pty::reap_child_group(pid, pty::REAP_GRACE);
                    let _ = thread.join();
                });
        }
    }

    /// Draws the tab into `area`: a bordered pane whose title names the tab
    /// and its worktree, with the grid inside.
    pub fn draw(&self, frame: &mut Frame<'_>, area: Rect, focused: bool) {
        let border_style = if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(self.pane_title());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width < 2 || inner.height < 1 {
            return;
        }
        let Some(handle) = &self.handle else {
            return;
        };
        let term = handle.term.lock();
        let content = term.renderable_content();
        let colors = content.colors;
        let selection = content.selection;
        let display_offset = content.display_offset;
        let buf = frame.buffer_mut();

        for indexed in content.display_iter {
            let Some(view_point) = point_to_viewport(display_offset, indexed.point) else {
                continue;
            };
            let (col, row) = (view_point.column.0, view_point.line);
            if col >= usize::from(inner.width) || row >= usize::from(inner.height) {
                continue;
            }
            let x = inner.x + col as u16;
            let y = inner.y + row as u16;
            let Some(out) = buf.cell_mut((x, y)) else {
                continue;
            };
            let cell = indexed.cell;
            if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                // The glyph to the left already covers this column.
                out.set_diff_option(CellDiffOption::Skip);
                continue;
            }
            let mut style = cells::cell_style(cell, colors);
            if selection.is_some_and(|range| range.contains(indexed.point)) {
                style = style.add_modifier(Modifier::REVERSED);
            }
            let symbol = if cell.flags.contains(Flags::HIDDEN)
                || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
                || cell.c == '\t'
            {
                " ".to_string()
            } else {
                let mut s = String::with_capacity(4);
                s.push(cell.c);
                for zw in cell.zerowidth().into_iter().flatten() {
                    s.push(*zw);
                }
                s
            };
            out.set_diff_option(CellDiffOption::None);
            out.set_symbol(&symbol);
            out.set_style(style);
        }

        // The cursor: the real one when focused (so a child's own cursor
        // shape/blink applies), a reversed cell when not.
        if content.cursor.shape != CursorShape::Hidden {
            if let Some(cursor) = point_to_viewport(display_offset, content.cursor.point) {
                let (col, row) = (cursor.column.0, cursor.line);
                if col < usize::from(inner.width) && row < usize::from(inner.height) {
                    let x = inner.x + col as u16;
                    let y = inner.y + row as u16;
                    if focused {
                        frame.set_cursor_position((x, y));
                    } else if let Some(out) = frame.buffer_mut().cell_mut((x, y)) {
                        out.set_style(out.style().add_modifier(Modifier::REVERSED | Modifier::DIM));
                    }
                }
            }
        }
    }

    /// The tab strip's short label: the kind and the worktree's basename,
    /// with the child's own title preferred when it set one. Truncated so
    /// one tab cannot crowd out the rest of the strip.
    pub fn strip_label(&self) -> String {
        let name = self.worktree_name();
        let label = match &self.title {
            Some(t) if !t.is_empty() => format!("{}·{t}", self.kind.label()),
            _ => format!("{}·{name}", self.kind.label()),
        };
        truncate_middle(&label, 24)
    }

    fn worktree_name(&self) -> String {
        self.opened_in.file_name().map_or_else(
            || self.opened_in.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    }

    fn pane_title(&self) -> String {
        let name = self.worktree_name();
        let mut title = match &self.title {
            Some(t) if !t.is_empty() => format!(" {} · {name} · {t} ", self.kind.label()),
            _ => format!(" {} · {name} ", self.kind.label()),
        };
        if let Some(status) = self.exit_status {
            title.push_str(&match status.code() {
                Some(code) => format!("[exited {code}] "),
                None => "[exited by signal] ".to_string(),
            });
        } else if let Some(handle) = &self.handle {
            let offset = handle.term.lock().grid().display_offset();
            if offset > 0 {
                title.push_str(&format!("[scrollback -{offset}] "));
            }
        }
        title
    }
}

/// Shortens `text` to `max` characters by eliding its middle with `…`,
/// keeping both ends legible (a branch-shaped label's distinguishing part
/// is as often at the end as the start). Counts `char`s, not bytes, so a
/// multi-byte label is never split mid-character.
fn truncate_middle(text: &str, max: usize) -> String {
    let count = text.chars().count();
    if count <= max || max < 3 {
        return text.to_string();
    }
    let keep = max - 1; // one cell for the ellipsis
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let mut out: String = text.chars().take(head).collect();
    out.push('…');
    out.extend(text.chars().skip(count - tail));
    out
}

/// Resolves a viewport cell (clamped into the grid) to a grid point,
/// accounting for the scrollback offset.
fn viewport_point<T>(term: &Term<T>, col: u16, line: u16) -> Point {
    let col = usize::from(col).min(term.columns().saturating_sub(1));
    let line = usize::from(line).min(term.screen_lines().saturating_sub(1));
    viewport_to_point(term.grid().display_offset(), Point::new(line, Column(col)))
}

/// The answer to a child's OSC 4/10/11 colour query when nothing has
/// overridden the palette: the xterm defaults, which is what the child would
/// see from the host terminal in the common case anyway.
fn default_palette_rgb(index: usize) -> Rgb {
    const BASE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    let (r, g, b) = match index {
        i if i < 16 => BASE[i],
        // 6x6x6 colour cube.
        i if (16..232).contains(&i) => {
            let i = i - 16;
            let level = |v: usize| if v == 0 { 0 } else { (55 + 40 * v) as u8 };
            (level(i / 36), level((i / 6) % 6), level(i % 6))
        }
        // Greyscale ramp.
        i if (232..256).contains(&i) => {
            let v = (8 + 10 * (i - 232)) as u8;
            (v, v, v)
        }
        // 256 = foreground, 257 = background, 258 = cursor.
        256 | 258 => (229, 229, 229),
        _ => (0, 0, 0),
    };
    Rgb { r, g, b }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::index::Line;
    use alacritty_terminal::term::Config;

    use super::*;

    /// Pins the emulator behaviour the Phase 4 selection model depends on
    /// (issue #1585, test coverage): a rows-only resize keeps the selection,
    /// a column-changing resize clears it (rewrap invalidates every offset).
    #[test]
    fn resize_keeps_a_selection_on_rows_only_change_and_clears_it_on_column_change() {
        let mut term = Term::new(
            Config::default(),
            &GridSize { cols: 20, lines: 5 },
            VoidListener,
        );
        let start = Point::new(Line(1), Column(2));
        let mut selection = Selection::new(SelectionType::Simple, start, Side::Left);
        selection.update(Point::new(Line(1), Column(6)), Side::Right);
        term.selection = Some(selection);

        term.resize(GridSize { cols: 20, lines: 8 });
        assert!(
            term.selection.is_some(),
            "rows-only resize must keep the selection"
        );

        term.resize(GridSize { cols: 30, lines: 8 });
        assert!(
            term.selection.is_none(),
            "a column change must clear the selection"
        );
    }

    fn buffer_text(terminal: &ratatui::Terminal<ratatui::backend::TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[cfg(unix)]
    fn sh_tab(script: &str, tx: mpsc::UnboundedSender<(TabId, TermEvent)>) -> TerminalTab {
        let request = pty::SpawnRequest {
            tab: 7,
            program: Some((
                "/bin/sh".to_string(),
                vec!["-c".to_string(), script.to_string()],
            )),
            cwd: std::env::temp_dir(),
            size: GridSize { cols: 40, lines: 6 },
            extra_env: Vec::new(),
        };
        TerminalTab::from_request(TabKind::Shell, request, tx).unwrap()
    }

    /// Feeds every event into the tab until `done` or the timeout; returns
    /// the effects seen.
    #[cfg(unix)]
    async fn pump(
        tab: &mut TerminalTab,
        rx: &mut mpsc::UnboundedReceiver<(TabId, TermEvent)>,
        mut done: impl FnMut(&TabEffect, &TerminalTab) -> bool,
    ) -> Vec<TabEffect> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut effects = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some((_, event))) => {
                    let effect = tab.handle_event(event);
                    let finished = done(&effect, tab);
                    effects.push(effect);
                    if finished {
                        return effects;
                    }
                }
                _ => return effects,
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_live_tab_renders_its_grid_title_and_cursor_then_reports_exit() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let (tx, mut rx) = mpsc::unbounded_channel();
        // Output, a wide char, a title, a colour query and a text-area query
        // (both answered by the emulator through PtyWrite), then linger so
        // the tab can be drawn while alive.
        let script = "printf 'hi 你好\\n'; printf '\\033]2;tab-title\\a'; \
                      printf '\\033]10;?\\a\\033[18t'; sleep 1";
        let mut tab = sh_tab(script, tx);
        assert_eq!(tab.id(), 7);
        assert!(tab.is_alive());
        assert!(tab.selection_to_string().is_none());
        assert!(!tab.mode().contains(TermMode::ALT_SCREEN));

        pump(&mut tab, &mut rx, |_, t| {
            t.title.as_deref() == Some("tab-title")
        })
        .await;
        assert_eq!(tab.title.as_deref(), Some("tab-title"));

        // Resize (both the grid and the PTY) before drawing.
        tab.resize(GridSize { cols: 60, lines: 8 });
        tab.resize(GridSize { cols: 60, lines: 8 }); // no-op on the same size
        tab.resize(GridSize { cols: 1, lines: 0 }); // refused: below the minimum

        // Give the emulator a moment to have parsed the output, then draw.
        pump(&mut tab, &mut rx, |e, _| *e == TabEffect::Redraw).await;
        let mut terminal = Terminal::new(TestBackend::new(64, 10)).unwrap();
        terminal
            .draw(|frame| tab.draw(frame, frame.area(), true))
            .unwrap();
        let text = buffer_text(&terminal);
        // The wide char's spacer cell is skipped for diffing but still
        // contributes its blank symbol here, so check the halves.
        assert!(text.contains("hi 你"), "grid text was: {text}");
        assert!(text.contains("好"), "grid text was: {text}");
        assert!(text.contains("shell"), "pane title names the kind");
        assert!(
            text.contains("tab-title"),
            "pane title carries the child's title"
        );

        // Unfocused draw takes the reversed-cell cursor path.
        terminal
            .draw(|frame| tab.draw(frame, frame.area(), false))
            .unwrap();
        // A too-small area is a no-op rather than a panic.
        terminal
            .draw(|frame| tab.draw(frame, Rect::new(0, 0, 2, 2), true))
            .unwrap();

        // Scrollback controls and input are accepted while alive.
        tab.scroll(Scroll::PageUp);
        tab.scroll(Scroll::Bottom);
        tab.write_input(Vec::new()); // empty input is dropped
        tab.write_input(b"\n".to_vec());

        // A selection made through the mouse-facing API is rendered and
        // extractable; out-of-range coordinates clamp into the grid.
        tab.selection_start(0, 0, SelectionType::Simple);
        tab.selection_update(1, 0);
        assert_eq!(tab.selection_to_string().as_deref(), Some("hi"));
        terminal
            .draw(|frame| tab.draw(frame, frame.area(), true))
            .unwrap();
        tab.selection_start(1, 0, SelectionType::Semantic);
        assert_eq!(tab.selection_to_string().as_deref(), Some("hi"));
        tab.selection_start(0, 0, SelectionType::Lines);
        assert!(tab
            .selection_to_string()
            .is_some_and(|s| s.starts_with("hi 你好")));
        tab.selection_update(500, 500); // clamped, not a panic
        tab.clear_selection();
        assert!(tab.selection_to_string().is_none());
        tab.selection_update(1, 0); // no selection to extend: a no-op
        assert!(tab.selection_to_string().is_none());

        // The child exits after its sleep.
        let effects = pump(&mut tab, &mut rx, |e, _| *e == TabEffect::Exited).await;
        assert!(effects.contains(&TabEffect::Exited));
        assert!(tab.exit_status.is_some());
        assert!(!tab.is_alive());
        terminal
            .draw(|frame| tab.draw(frame, frame.area(), false))
            .unwrap();
        assert!(buffer_text(&terminal).contains("[exited 0]"));

        // Shutdown reaps the thread; the tab is inert afterwards.
        tab.shutdown();
        tab.shutdown(); // idempotent
        assert!(!tab.is_alive());
        assert_eq!(tab.mode(), TermMode::default());
        assert!(tab.selection_to_string().is_none());
        tab.write_input(b"x".to_vec());
        tab.scroll(Scroll::PageUp);
        tab.selection_start(0, 0, SelectionType::Simple);
        tab.selection_update(1, 1);
        tab.clear_selection();
        tab.resize(GridSize { cols: 20, lines: 5 });
        terminal
            .draw(|frame| tab.draw(frame, frame.area(), true))
            .unwrap();
    }

    /// The structural property `shutdown` relies on (#1611): a plain,
    /// detached `std::thread` is never joined by anything, so it cannot hold
    /// up a `tokio::Runtime`'s drop no matter how long it runs — unlike a
    /// `tokio::task::spawn_blocking` task, which the runtime always joins on
    /// drop even when nobody awaits it (see the counter-example below). This
    /// can't be pinned against the real macOS failure (a `login` process
    /// stuck in the kernel's own exit teardown) on demand, so it is pinned
    /// against a thread that deliberately never returns instead.
    #[test]
    fn a_never_finishing_detached_thread_does_not_block_runtime_drop() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let _ = std::thread::Builder::new()
            .name("never-finishing".into())
            .spawn(|| loop {
                std::thread::sleep(std::time::Duration::from_secs(3600));
            });

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let watchdog = std::thread::spawn(move || {
            drop(rt);
            let _ = done_tx.send(());
        });
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .is_ok(),
            "Runtime::drop must not wait on a detached OS thread"
        );
        let _ = watchdog.join();
    }

    /// The counterpart to the test above: `spawn_blocking` is what `shutdown`
    /// used to use, and this is why it had to change. Left `#[ignore]`d as
    /// living documentation of the regression #1611 fixed — running it hangs
    /// the process, which is exactly the bug.
    #[test]
    #[ignore = "demonstrates the pre-#1611 hang; would block the test binary forever if run"]
    fn a_never_finishing_spawn_blocking_task_blocks_runtime_drop() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.spawn_blocking(|| loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        });

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let watchdog = std::thread::spawn(move || {
            drop(rt);
            let _ = done_tx.send(());
        });
        assert!(
            done_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .is_err(),
            "Runtime::drop was expected to hang waiting on the blocking task"
        );
        let _ = watchdog.join();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn synthetic_events_map_to_the_documented_effects() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut tab = sh_tab("sleep 1", tx);
        assert_eq!(
            tab.handle_event(TermEvent::ClipboardStore(
                alacritty_terminal::term::ClipboardType::Clipboard,
                "copied".to_string()
            )),
            TabEffect::CopyToClipboard("copied".to_string())
        );
        assert_eq!(
            tab.handle_event(TermEvent::Title("t".to_string())),
            TabEffect::Redraw
        );
        assert_eq!(tab.handle_event(TermEvent::ResetTitle), TabEffect::Redraw);
        assert_eq!(tab.title, None);
        assert_eq!(tab.handle_event(TermEvent::Bell), TabEffect::None);
        assert_eq!(
            tab.handle_event(TermEvent::MouseCursorDirty),
            TabEffect::None
        );
        assert_eq!(
            tab.handle_event(TermEvent::CursorBlinkingChange),
            TabEffect::None
        );
        assert_eq!(tab.handle_event(TermEvent::Wakeup), TabEffect::Redraw);
        assert_eq!(
            tab.handle_event(TermEvent::ClipboardLoad(
                alacritty_terminal::term::ClipboardType::Clipboard,
                std::sync::Arc::new(|s: &str| s.to_string())
            )),
            TabEffect::None
        );
        assert_eq!(
            tab.handle_event(TermEvent::PtyWrite("\x1b[1;1R".to_string())),
            TabEffect::None
        );
        assert_eq!(tab.handle_event(TermEvent::Exit), TabEffect::Exited);
        pump(&mut tab, &mut rx, |e, _| *e == TabEffect::Exited).await;
        tab.shutdown();
    }

    #[test]
    fn truncate_middle_elides_only_when_it_has_to_and_never_splits_a_char() {
        assert_eq!(truncate_middle("short", 24), "short");
        assert_eq!(truncate_middle("exactly-ten", 11), "exactly-ten");
        let long = "shell·issue-1585-worktrees-ui-phase-4";
        let out = truncate_middle(long, 20);
        assert_eq!(out.chars().count(), 20);
        assert!(out.contains('…'));
        assert!(out.starts_with("shell·"), "the head survives: {out}");
        assert!(out.ends_with('4'), "the tail survives: {out}");
        // Multi-byte throughout: still exactly `max` chars, still valid.
        let cjk = "你好你好你好你好你好你好";
        assert_eq!(truncate_middle(cjk, 5).chars().count(), 5);
        // A max too small to elide into returns the text untouched.
        assert_eq!(truncate_middle("abcdef", 2), "abcdef");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn strip_label_names_the_kind_and_worktree_then_the_childs_title() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut tab = sh_tab("printf '\\033]2;my-title\\a'; sleep 1", tx);
        assert!(tab.strip_label().starts_with("shell·"));
        pump(&mut tab, &mut rx, |_, t| {
            t.title.as_deref() == Some("my-title")
        })
        .await;
        assert_eq!(tab.strip_label(), "shell·my-title");
        tab.shutdown();
    }

    #[test]
    fn default_palette_covers_named_cube_and_greyscale_ranges() {
        assert_eq!(default_palette_rgb(1), Rgb { r: 205, g: 0, b: 0 });
        assert_eq!(default_palette_rgb(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(
            default_palette_rgb(231),
            Rgb {
                r: 255,
                g: 255,
                b: 255
            }
        );
        assert_eq!(default_palette_rgb(232), Rgb { r: 8, g: 8, b: 8 });
        assert_eq!(
            default_palette_rgb(255),
            Rgb {
                r: 238,
                g: 238,
                b: 238
            }
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn select_all_scroll_to_bottom_and_cursor_position_work_as_expected() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        // The grid is 6 lines (`sh_tab`), so the output must exceed that or
        // there is no scrollback at all and `PageUp` has nowhere to go — the
        // reason an earlier three-line version of this script could never
        // have passed. 30 lines guarantees history on any run.
        let script = "i=1; while [ $i -le 30 ]; do printf 'line%d\\n' $i; \
                      i=$((i+1)); done; \
                      printf '\\033]2;test-title\\a'; \
                      sleep 2";
        let mut tab = sh_tab(script, tx);

        // Wait for the output and title to be set.
        pump(&mut tab, &mut rx, |_, t| {
            t.title.as_deref() == Some("test-title")
        })
        .await;

        // Give the emulator a moment to parse everything.
        pump(&mut tab, &mut rx, |e, _| *e == TabEffect::Redraw).await;

        // Test select_all: should select all content including scrollback.
        tab.select_all();
        let selected = tab.selection_to_string();
        assert!(
            selected.is_some(),
            "select_all must create a selection with content"
        );
        let selected_text = selected.unwrap();
        assert!(
            selected_text.contains("line1") && selected_text.contains("line2"),
            "select_all must select scrollback content: {selected_text}"
        );

        // Test scroll_to_bottom: scroll up first, then reset to bottom.
        tab.scroll(Scroll::PageUp);
        let term = tab.handle.as_ref().unwrap().term.lock();
        let offset_after_scroll_up = term.grid().display_offset();
        drop(term);
        assert!(
            offset_after_scroll_up > 0,
            "page up must increase display offset"
        );

        tab.scroll_to_bottom();
        let term = tab.handle.as_ref().unwrap().term.lock();
        let offset_after_scroll_to_bottom = term.grid().display_offset();
        drop(term);
        assert_eq!(
            offset_after_scroll_to_bottom, 0,
            "scroll_to_bottom must reset display offset to 0"
        );

        // Test cursor_position: should return valid coordinates while alive.
        let pos = tab.cursor_position();
        assert!(
            pos.is_some(),
            "cursor_position must return Some for a live terminal"
        );
        if let Some((col, row)) = pos {
            // The cursor should be at a reasonable position within the grid.
            assert!(col < 40, "cursor column must be within grid width");
            assert!(row < 6, "cursor row must be within grid height");
        }

        // Test cursor_position on a shutdown tab: should return None.
        tab.shutdown();
        let pos_after_shutdown = tab.cursor_position();
        assert!(
            pos_after_shutdown.is_none(),
            "cursor_position must return None when tab is shut down"
        );

        // select_all on a dead tab (no handle) is a no-op, not a panic.
        tab.select_all();
        assert!(
            tab.selection_to_string().is_none(),
            "a shutdown tab has no handle to select in"
        );
    }

    /// The security invariant from issue #1585: PTY traffic, the emulator's
    /// query replies, selection text and clipboard text are never logged.
    /// A grep guard over the code lines (comments excluded), so a stray
    /// debug log of a query reply or a selection added in a hurry fails a
    /// test rather than shipping.
    #[test]
    fn no_pty_content_is_ever_logged() {
        let sources = [
            ("terminal/mod.rs", include_str!("mod.rs")),
            ("terminal/pty.rs", include_str!("pty.rs")),
            ("terminal/cells.rs", include_str!("cells.rs")),
            ("app.rs", include_str!("../app.rs")),
            ("keys.rs", include_str!("../keys.rs")),
            ("mouse.rs", include_str!("../mouse.rs")),
            ("clipboard.rs", include_str!("../clipboard.rs")),
        ];
        let sensitive = [
            "PtyWrite",
            "selection_to_string",
            "ClipboardStore",
            "write_input",
            "copy_text",
        ];
        for (name, source) in sources {
            for (number, line) in source.lines().enumerate() {
                let code = line.trim_start();
                if code.starts_with("//") {
                    continue;
                }
                let logs = code.contains("tracing::")
                    || code.contains("println!")
                    || code.contains("eprintln!");
                assert!(
                    !(logs && sensitive.iter().any(|s| code.contains(s))),
                    "{name}:{}: logs PTY/clipboard content: {line}",
                    number + 1
                );
            }
        }
    }
}
