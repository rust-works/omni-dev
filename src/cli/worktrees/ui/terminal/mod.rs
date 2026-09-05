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
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{point_to_viewport, TermMode};
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
            cwd: opened_in.clone(),
            size,
            extra_env: Vec::new(),
        };
        let handle = pty::spawn(&request, tx)?;
        Ok(Self {
            id,
            kind,
            opened_in,
            handle: Some(handle),
            title: None,
            exit_status: None,
            size,
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

    /// Stops the PTY thread and reaps the child. The join (and the `Pty`
    /// drop that `SIGHUP`s the child) runs on a blocking thread so a slow
    /// exit never stalls the event loop.
    pub fn shutdown(&mut self) {
        let Some(mut handle) = self.handle.take() else {
            return;
        };
        let _ = handle.sender.send(Msg::Shutdown);
        if let Some(thread) = handle.take_thread() {
            tokio::task::spawn_blocking(move || {
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

    fn pane_title(&self) -> String {
        let name = self.opened_in.file_name().map_or_else(
            || self.opened_in.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
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
    use alacritty_terminal::index::{Column, Line, Point, Side};
    use alacritty_terminal::selection::{Selection, SelectionType};
    use alacritty_terminal::term::{Config, Term};

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
