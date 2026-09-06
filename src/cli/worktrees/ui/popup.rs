//! The action-menu and confirm-modal popups (issue #1585 Phase 2, extended
//! by #1602 Phase 1 into the context-menu machinery).
//!
//! Two things here are load-bearing for #1602:
//!
//! * **Placement is a pure function** ([`place`]) of anchor, content size and
//!   frame size, so the flip-up/flip-left/clamp rules are unit-tested with no
//!   terminal at all.
//! * **The renderer produces the hit map.** [`draw_menu`] returns the
//!   [`PopupRegion`] describing what it just drew, so `RegionMap` and the
//!   frame cannot disagree about where an item is — the same invariant
//!   `GroupRegion::tab_spans` already keeps for tab strips.

use std::path::PathBuf;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use super::actions::{ActionKind, ConfirmPrompt};
use super::glyph::{Glyph, GlyphMode};
use super::mouse::{PopupRegion, SubmenuRegion};

/// What invoking a menu entry does.
///
/// Only [`Self::Action`] exists today because it is the only variant with a
/// caller: the tree-row menu, the `c` row-colour picker and the `:` palette
/// all dispatch an [`ActionKind`]. #1602 Phase 3 adds `Chrome(ChromeKey)` and
/// a `Ui(..)` variant for the tab-strip and terminal-grid menus, whose
/// entries are not actions at all. Those are pure additions — every call site
/// already speaks `MenuCommand`, so the field-type churn happens once, here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuCommand {
    Action(ActionKind),
}

/// One selectable entry — the command it dispatches and its display label.
///
/// `enabled: false` renders dimmed and refuses invocation. The tree menu never
/// uses it (`applicable_actions` *hides* rather than greys out, and that
/// filter is what keeps *Close Worktree* off a main working tree); it exists
/// for the Phase 3 grid menu, where *Copy* must be visible-but-inert with no
/// selection so its keyboard shortcut stays discoverable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MenuItem {
    pub command: MenuCommand,
    pub label: &'static str,
    pub enabled: bool,
}

impl MenuItem {
    /// An enabled item — the common case.
    pub fn new(command: MenuCommand, label: &'static str) -> Self {
        Self {
            command,
            label,
            enabled: true,
        }
    }

    /// An item dispatching `action`, enabled.
    pub fn action(action: ActionKind, label: &'static str) -> Self {
        Self::new(MenuCommand::Action(action), label)
    }
}

/// A row of a menu. Separators carry no command and are skipped by keyboard
/// navigation and by hit-testing alike, so neither route can select one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    Item(MenuItem),
    Separator,
    /// One level of submenu — *Row Colour ▸*. Deliberately holds a flat
    /// `Vec<MenuItem>` and not `Vec<MenuEntry>`: arbitrary nesting was
    /// rejected in #1602, and a flat list makes "one level only" structural
    /// rather than a convention someone can quietly break.
    Submenu {
        label: &'static str,
        items: Vec<MenuItem>,
    },
}

impl MenuEntry {
    fn item(&self) -> Option<&MenuItem> {
        match self {
            Self::Item(item) => Some(item),
            Self::Separator | Self::Submenu { .. } => None,
        }
    }

    /// Whether the keyboard and the mouse can land on this row. A submenu
    /// row is selectable but carries no command of its own — activating it
    /// opens the submenu instead.
    fn is_selectable(&self) -> bool {
        !matches!(self, Self::Separator)
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Item(item) => item.label,
            Self::Submenu { label, .. } => label,
            Self::Separator => "",
        }
    }
}

/// Where a popup wants to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    /// Centred in the frame — the `a` menu and both relocation pickers.
    #[default]
    Centered,
    /// Top-left corner at this screen cell — a context menu at the pointer.
    At { col: u16, row: u16 },
}

/// Places a `width`x`height` popup for `anchor` inside `area`.
///
/// Anchored placement follows the universal convention: prefer down-and-right
/// of the anchor, **flip** to the other side of it when that would overflow,
/// and **clamp** into the frame when even the flip does not fit (a popup
/// taller or wider than the frame, which scrolling then handles). Pure: no
/// frame, no terminal, no state.
pub fn place(anchor: Anchor, width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    match anchor {
        Anchor::Centered => Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        },
        Anchor::At { col, row } => {
            // Flip rather than merely clamp, so a menu opened near the right
            // or bottom edge still sits beside the pointer instead of under
            // it.
            let x = if col.saturating_add(width) <= area.right() {
                col
            } else {
                col.saturating_sub(width)
            };
            let y = if row.saturating_add(height) <= area.bottom() {
                row
            } else {
                row.saturating_sub(height)
            };
            // `width <= area.width` above guarantees `right() - width >= x`,
            // so these clamps can never invert.
            Rect {
                x: x.clamp(area.x, area.right().saturating_sub(width)),
                y: y.clamp(area.y, area.bottom().saturating_sub(height)),
                width,
                height,
            }
        }
    }
}

/// A menu popup: the `a` action menu, the `c` row-colour picker, and (from
/// Phase 2) the per-surface context menus.
pub struct ActionMenu {
    pub title: &'static str,
    pub entries: Vec<MenuEntry>,
    /// Index into `entries`. Always on an item, never a separator, provided
    /// the menu has at least one item.
    pub selected: usize,
    /// First entry rendered — non-zero only when the menu is taller than the
    /// frame allows.
    pub scroll: usize,
    pub anchor: Anchor,
    /// Whether a press has begun *inside* this menu.
    ///
    /// A context menu opens on `Down(Right)` at the pointer, so the matching
    /// `Up(Right)` lands on the menu a moment later. Without this latch that
    /// release would invoke whatever item the menu happened to open under.
    /// Only a press that starts inside the menu arms it, and only an armed
    /// menu can be invoked by a release.
    pub armed: bool,
    /// The tree row the menu was opened from, when it was opened from one.
    ///
    /// A context menu names a *subject*, so it must not outlive it. The tree
    /// is live — a daemon push can remove the worktree the menu is acting on
    /// while it is open — and a menu still offering *Close Worktree* for a
    /// row that has gone would act on whatever row slid into its place.
    /// `None` for menus with no row subject (the `c` picker).
    pub origin_row: Option<PathBuf>,
    /// The entry index of the open submenu, if any. At most one, ever.
    pub open_submenu: Option<usize>,
    /// Index into the open submenu's items.
    pub submenu_selected: usize,
}

impl ActionMenu {
    /// A centred menu of plain items — the `a` menu, the `c` picker and the
    /// palette's post-filter menu.
    pub fn new(items: Vec<MenuItem>) -> Self {
        Self::with_entries("Actions", items.into_iter().map(MenuEntry::Item).collect())
    }

    pub fn with_entries(title: &'static str, entries: Vec<MenuEntry>) -> Self {
        let mut menu = Self {
            title,
            entries,
            selected: 0,
            scroll: 0,
            anchor: Anchor::Centered,
            armed: false,
            origin_row: None,
            open_submenu: None,
            submenu_selected: 0,
        };
        // Opening on a separator would make the first `Enter` do nothing.
        if menu.entry_is_selectable(0) {
            return menu;
        }
        menu.selected = menu.next_selectable(0, 1).unwrap_or(0);
        menu
    }

    fn entry_is_selectable(&self, index: usize) -> bool {
        self.entries
            .get(index)
            .is_some_and(MenuEntry::is_selectable)
    }

    /// The open submenu's items, if one is open.
    fn submenu_items(&self) -> Option<&[MenuItem]> {
        match self.entries.get(self.open_submenu?)? {
            MenuEntry::Submenu { items, .. } => Some(items),
            MenuEntry::Item(_) | MenuEntry::Separator => None,
        }
    }

    /// Opens the submenu on the selected row, if it is one. `→`, and what
    /// `Enter` does on a submenu row.
    pub fn open_selected_submenu(&mut self) -> bool {
        if matches!(
            self.entries.get(self.selected),
            Some(MenuEntry::Submenu { .. })
        ) {
            self.open_submenu = Some(self.selected);
            self.submenu_selected = 0;
            return true;
        }
        false
    }

    /// Closes the open submenu, returning whether one was open — `←`, and
    /// what `Esc` does *first*, so one `Esc` never closes both levels.
    pub fn close_submenu(&mut self) -> bool {
        self.open_submenu.take().is_some()
    }

    /// The next selectable index from `from` walking by `step`, or `None`
    /// when there is none in that direction.
    fn next_selectable(&self, from: usize, step: isize) -> Option<usize> {
        let mut index = from as isize;
        loop {
            index += step;
            if index < 0 || index as usize >= self.entries.len() {
                return None;
            }
            if self.entry_is_selectable(index as usize) {
                return Some(index as usize);
            }
        }
    }

    /// Moves the selection by `delta` items, skipping separators and stopping
    /// at either end rather than wrapping. Drives the open submenu when there
    /// is one, since that is what the eye is on.
    pub fn move_selection(&mut self, delta: isize) {
        if let Some(len) = self.submenu_items().map(<[MenuItem]>::len) {
            if len == 0 {
                return;
            }
            let max = (len - 1) as isize;
            let current = (self.submenu_selected.min(len - 1)) as isize;
            self.submenu_selected = (current + delta).clamp(0, max) as usize;
            return;
        }
        let step = if delta < 0 { -1 } else { 1 };
        for _ in 0..delta.abs() {
            match self.next_selectable(self.selected, step) {
                Some(next) => self.selected = next,
                None => break,
            }
        }
    }

    /// Selects the first/last item — `Home`/`End`. Walks inward from just
    /// outside the range, so the end entry itself is considered and a
    /// trailing separator is skipped.
    pub fn select_end(&mut self, last: bool) {
        let (from, step) = if last {
            (self.entries.len() as isize, -1)
        } else {
            (-1, 1)
        };
        let mut index = from;
        loop {
            index += step;
            if index < 0 || index as usize >= self.entries.len() {
                return;
            }
            if self.entry_is_selectable(index as usize) {
                self.selected = index as usize;
                return;
            }
        }
    }

    /// Selects entry `index` if it is a selectable item. Used by the mouse:
    /// hover moves the highlight, a click invokes what is under it.
    pub fn select_index(&mut self, index: usize) -> bool {
        if self.entry_is_selectable(index) {
            self.selected = index;
            return true;
        }
        false
    }

    /// The command the selection would dispatch — `None` on an empty menu, a
    /// disabled item, or a submenu row (which opens rather than dispatches).
    /// That `None` is what makes a disabled entry inert to `Enter` as well as
    /// to the mouse.
    pub fn selected_command(&self) -> Option<MenuCommand> {
        if let Some(items) = self.submenu_items() {
            return items
                .get(self.submenu_selected)
                .filter(|item| item.enabled)
                .map(|item| item.command);
        }
        self.entries
            .get(self.selected)?
            .item()
            .filter(|item| item.enabled)
            .map(|item| item.command)
    }

    /// What `Enter` (or a click) does: opens the submenu on a submenu row,
    /// otherwise yields the command to dispatch. Keeping this one method
    /// stops the key path and the mouse path from disagreeing about which
    /// rows dispatch and which expand.
    pub fn activate(&mut self) -> Option<MenuCommand> {
        if self.open_submenu.is_none() && self.open_selected_submenu() {
            return None;
        }
        self.selected_command()
    }

    /// Scrolls so the selection is visible in a window `rows` tall.
    fn sync_scroll(&mut self, rows: usize) {
        if rows == 0 {
            return;
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + rows {
            self.scroll = self.selected + 1 - rows;
        }
        let max_scroll = self.entries.len().saturating_sub(rows);
        self.scroll = self.scroll.min(max_scroll);
    }
}

/// The confirm modal — renders a [`ConfirmPrompt`] verbatim (body, then
/// risks, then info), with the yes/no footer hint.
pub struct ConfirmModal {
    pub prompt: ConfirmPrompt,
}

/// Width of a menu popup. Fixed rather than content-derived so a menu does
/// not resize as its filtered contents change under the palette.
const MENU_WIDTH: u16 = 50;

/// Draws `menu` and returns the region describing what was drawn, for
/// `RegionMap`.
pub fn draw_menu(
    frame: &mut Frame<'_>,
    area: Rect,
    menu: &mut ActionMenu,
    glyphs: GlyphMode,
) -> PopupRegion {
    let wanted_height = menu.entries.len().saturating_add(2).min(u16::MAX as usize) as u16;
    let popup_area = place(menu.anchor, MENU_WIDTH, wanted_height, area);
    let rows = usize::from(popup_area.height.saturating_sub(2));
    menu.sync_scroll(rows);

    let truncated = menu.entries.len() > rows;
    let mut lines: Vec<ListItem> = Vec::with_capacity(rows);
    let mut items: Vec<(usize, Rect)> = Vec::new();
    // The screen row of the open submenu's parent, once we reach it — the
    // submenu is drawn beside it, so it must be laid out after the parent.
    let mut submenu_row: Option<u16> = None;
    let inner_x = popup_area.x + 1;
    let inner_width = popup_area.width.saturating_sub(2);

    for offset in 0..rows {
        let index = menu.scroll + offset;
        let row_y = popup_area.y + 1 + offset as u16;
        // The first and last visible rows become `…` affordances when there
        // is more menu beyond them, rather than silently truncating.
        let more_above = truncated && offset == 0 && menu.scroll > 0;
        let more_below = truncated && offset + 1 == rows && menu.scroll + rows < menu.entries.len();
        if more_above || more_below {
            lines.push(ListItem::new(Line::from(Span::styled(
                "  …",
                Style::default().fg(Color::DarkGray),
            ))));
            continue;
        }
        let Some(entry) = menu.entries.get(index) else {
            break;
        };
        match entry {
            MenuEntry::Separator => lines.push(ListItem::new(Line::from(Span::styled(
                "─".repeat(usize::from(inner_width)),
                Style::default().fg(Color::DarkGray),
            )))),
            MenuEntry::Item(_) | MenuEntry::Submenu { .. } => {
                let enabled = entry.item().is_none_or(|item| item.enabled);
                let style = if !enabled {
                    Style::default().fg(Color::DarkGray)
                } else if index == menu.selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                // A submenu row is marked with a trailing arrow, right-aligned
                // so the column reads down the menu.
                let text = match entry {
                    MenuEntry::Submenu { label, .. } => {
                        let arrow = Glyph::SubmenuArrow.as_str(glyphs);
                        let pad =
                            usize::from(inner_width).saturating_sub(label.chars().count() + 1);
                        format!("{label}{}{arrow}", " ".repeat(pad))
                    }
                    _ => entry.label().to_string(),
                };
                lines.push(ListItem::new(Line::from(Span::styled(text, style))));
                items.push((
                    index,
                    Rect {
                        x: inner_x,
                        y: row_y,
                        width: inner_width,
                        height: 1,
                    },
                ));
                if menu.open_submenu == Some(index) {
                    submenu_row = Some(row_y);
                }
            }
        }
    }

    frame.render_widget(Clear, popup_area);
    let list = List::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(menu.title.to_string()),
    );
    frame.render_widget(list, popup_area);

    // The submenu is drawn last so it sits above the parent, and only when
    // its parent row is actually on screen — a submenu whose row scrolled
    // out of view would otherwise float unattached.
    let submenu = submenu_row.and_then(|row| {
        let items = menu.submenu_items()?;
        let height = (items.len() as u16).saturating_add(2);
        // Beside the parent, flipping to its left when there is no room.
        let col = if popup_area.right().saturating_add(MENU_WIDTH) <= area.right() {
            popup_area.right()
        } else {
            popup_area.x.saturating_sub(MENU_WIDTH)
        };
        let rect = place(Anchor::At { col, row }, MENU_WIDTH, height, area);
        frame.render_widget(Clear, rect);
        let rows: Vec<ListItem> = items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let style = if !item.enabled {
                    Style::default().fg(Color::DarkGray)
                } else if i == menu.submenu_selected {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(item.label, style)))
            })
            .collect();
        frame.render_widget(
            List::new(rows).block(Block::default().borders(Borders::ALL)),
            rect,
        );
        let item_rects = (0..items.len())
            .map(|i| {
                (
                    i,
                    Rect {
                        x: rect.x + 1,
                        y: rect.y + 1 + i as u16,
                        width: rect.width.saturating_sub(2),
                        height: 1,
                    },
                )
            })
            .filter(|(_, r)| r.y < rect.bottom().saturating_sub(1))
            .collect();
        Some(SubmenuRegion {
            rect,
            items: item_rects,
        })
    });

    PopupRegion {
        rect: popup_area,
        items,
        submenu,
    }
}

/// A generic "pick one of these labelled items" popup — the Move/Copy-Claude-
/// Session-Here flow's session and destination pickers. Kept generic over
/// `&str` labels rather than tied to [`ActionKind`] since those pick a
/// session id / a worktree path, not an action. Always centred, and inert to
/// the mouse (see `app::popup_mouse_enabled`).
pub fn draw_list_popup(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    items: &[&str],
    selected: usize,
) {
    let popup_height = (items.len() as u16 + 2).min(area.height);
    let popup_area = place(Anchor::Centered, MENU_WIDTH, popup_height, area);
    frame.render_widget(Clear, popup_area);
    let list_items: Vec<ListItem> = items
        .iter()
        .enumerate()
        .map(|(i, label)| {
            let style = if i == selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(Span::styled(*label, style)))
        })
        .collect();
    let list = List::new(list_items).block(Block::default().borders(Borders::ALL).title(title));
    frame.render_widget(list, popup_area);
}

/// A one-line text prompt — the scrollback find (`alt-f`) and the command
/// palette (`:`). `hint` names what the input does; `status` carries the
/// last result ("no match", the filtered command count).
pub fn draw_prompt(frame: &mut Frame<'_>, area: Rect, title: &str, input: &str, status: &str) {
    let width = area.width.saturating_sub(8).clamp(20, 70);
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + area.height / 3,
        width,
        height: 3,
    };
    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(title.to_string());
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let line = if status.is_empty() {
        Line::from(vec![
            Span::raw(input.to_string()),
            Span::styled("_", Style::default().add_modifier(Modifier::REVERSED)),
        ])
    } else {
        Line::from(vec![
            Span::raw(input.to_string()),
            Span::styled("_", Style::default().add_modifier(Modifier::REVERSED)),
            Span::styled(format!("   {status}"), Style::default().fg(Color::DarkGray)),
        ])
    };
    frame.render_widget(Paragraph::new(line), inner);
}

pub fn draw_confirm_modal(frame: &mut Frame<'_>, area: Rect, modal: &ConfirmModal) {
    // Body, plus a header+lines block each for risks/info only when
    // non-empty, plus a blank line and the footer hint, plus the block's
    // own top/bottom border rows.
    let risk_block = if modal.prompt.risk_lines.is_empty() {
        0
    } else {
        1 + modal.prompt.risk_lines.len()
    };
    let info_block = if modal.prompt.info_lines.is_empty() {
        0
    } else {
        1 + modal.prompt.info_lines.len()
    };
    let content_lines = modal.prompt.body_lines.len() + risk_block + info_block + 2 + 2;
    let popup_area = place(
        Anchor::Centered,
        70,
        (content_lines as u16).min(area.height),
        area,
    );
    frame.render_widget(Clear, popup_area);

    let mut lines: Vec<Line> = Vec::new();
    for line in &modal.prompt.body_lines {
        lines.push(Line::from(line.as_str()));
    }
    if !modal.prompt.risk_lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "risks:",
            Style::default().add_modifier(Modifier::BOLD).fg(Color::Red),
        )));
        for line in &modal.prompt.risk_lines {
            lines.push(Line::from(Span::styled(
                format!("  {line}"),
                Style::default().fg(Color::Red),
            )));
        }
    }
    if !modal.prompt.info_lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "info:",
            Style::default().add_modifier(Modifier::BOLD),
        )));
        for line in &modal.prompt.info_lines {
            lines.push(Line::from(format!("  {line}")));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from("y/enter confirm   n/esc cancel"));

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .title(modal.prompt.title.as_str()),
    );
    frame.render_widget(paragraph, popup_area);
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    use super::*;

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    fn frame() -> Rect {
        Rect::new(0, 0, 80, 24)
    }

    #[test]
    fn anchored_placement_puts_the_popup_at_the_pointer() {
        let at = place(Anchor::At { col: 10, row: 4 }, 20, 6, frame());
        assert_eq!(at, Rect::new(10, 4, 20, 6));
    }

    #[test]
    fn placement_flips_up_at_the_bottom_edge_and_left_at_the_right_edge() {
        // Bottom: 20 + 6 > 24, so the popup sits *above* the pointer.
        let up = place(Anchor::At { col: 0, row: 20 }, 20, 6, frame());
        assert_eq!(up, Rect::new(0, 14, 20, 6));
        // Right: 70 + 20 > 80, so it sits *left* of the pointer.
        let left = place(Anchor::At { col: 70, row: 0 }, 20, 6, frame());
        assert_eq!(left, Rect::new(50, 0, 20, 6));
        // Both at once.
        let both = place(Anchor::At { col: 78, row: 23 }, 20, 6, frame());
        assert_eq!(both, Rect::new(58, 17, 20, 6));
    }

    #[test]
    fn placement_clamps_instead_of_flipping_when_the_popup_exceeds_the_frame() {
        let small = Rect::new(0, 0, 10, 4);
        // Wider and taller than the frame: clamped to it, never negative.
        let placed = place(Anchor::At { col: 8, row: 3 }, 40, 20, small);
        assert_eq!(placed, small);
    }

    #[test]
    fn centered_placement_is_unchanged_by_the_anchor_parameter() {
        assert_eq!(
            place(Anchor::Centered, 20, 6, frame()),
            Rect::new(30, 9, 20, 6)
        );
    }

    #[test]
    fn navigation_skips_separators_and_clamps_at_both_ends() {
        let mut menu = ActionMenu::with_entries(
            "t",
            vec![
                MenuEntry::Item(MenuItem::action(ActionKind::Focus, "a")),
                MenuEntry::Separator,
                MenuEntry::Item(MenuItem::action(ActionKind::CopyDirectory, "b")),
            ],
        );
        assert_eq!(menu.selected, 0);
        menu.move_selection(1);
        assert_eq!(menu.selected, 2, "the separator is stepped over");
        menu.move_selection(5);
        assert_eq!(menu.selected, 2, "clamps at the end");
        menu.move_selection(-5);
        assert_eq!(menu.selected, 0, "clamps at the start");
    }

    #[test]
    fn a_menu_opening_on_a_separator_selects_the_first_real_item() {
        let menu = ActionMenu::with_entries(
            "t",
            vec![
                MenuEntry::Separator,
                MenuEntry::Item(MenuItem::action(ActionKind::Focus, "a")),
            ],
        );
        assert_eq!(menu.selected, 1);
        assert_eq!(
            menu.selected_command(),
            Some(MenuCommand::Action(ActionKind::Focus))
        );
    }

    #[test]
    fn home_and_end_land_on_real_items() {
        let mut menu = ActionMenu::with_entries(
            "t",
            vec![
                MenuEntry::Item(MenuItem::action(ActionKind::Focus, "a")),
                MenuEntry::Item(MenuItem::action(ActionKind::CopyDirectory, "b")),
                MenuEntry::Separator,
            ],
        );
        menu.select_end(true);
        assert_eq!(menu.selected, 1, "End skips the trailing separator");
        menu.select_end(false);
        assert_eq!(menu.selected, 0);
    }

    #[test]
    fn a_separator_cannot_be_selected_by_index_and_a_disabled_item_is_inert() {
        let mut menu = ActionMenu::with_entries(
            "t",
            vec![
                MenuEntry::Item(MenuItem::action(ActionKind::Focus, "a")),
                MenuEntry::Separator,
                MenuEntry::Item(MenuItem {
                    command: MenuCommand::Action(ActionKind::CopyDirectory),
                    label: "b",
                    enabled: false,
                }),
            ],
        );
        assert!(!menu.select_index(1), "separator");
        assert_eq!(menu.selected, 0);
        assert!(
            menu.select_index(2),
            "a disabled item still takes the highlight"
        );
        assert_eq!(
            menu.selected_command(),
            None,
            "but dispatches nothing, by key or by mouse"
        );
    }

    #[test]
    fn selected_command_is_none_when_empty() {
        let menu = ActionMenu::new(Vec::new());
        assert_eq!(menu.selected_command(), None);
    }

    /// A plain item beside a *Row Colour* submenu, mirroring `app.rs`'s
    /// real context-menu shape.
    fn submenu_entries() -> Vec<MenuEntry> {
        vec![
            MenuEntry::Item(MenuItem::action(ActionKind::Focus, "Open Worktree")),
            MenuEntry::Submenu {
                label: "Row Colour",
                items: vec![
                    MenuItem::action(ActionKind::SetRowColor("red"), "red"),
                    MenuItem::action(ActionKind::SetRowColor("blue"), "blue"),
                ],
            },
        ]
    }

    #[test]
    fn activate_on_a_submenu_row_opens_it_and_a_second_activate_dispatches_the_selected_item() {
        let mut menu = ActionMenu::with_entries("t", submenu_entries());
        menu.selected = 1;
        assert_eq!(menu.activate(), None, "opening must not dispatch");
        assert_eq!(menu.open_submenu, Some(1));
        assert_eq!(menu.submenu_selected, 0);
        assert_eq!(
            menu.activate(),
            Some(MenuCommand::Action(ActionKind::SetRowColor("red"))),
            "the second activate dispatches the selected submenu item"
        );
    }

    #[test]
    fn close_submenu_reports_whether_one_was_open() {
        let mut menu = ActionMenu::with_entries("t", submenu_entries());
        menu.selected = 1;
        assert!(!menu.close_submenu(), "nothing open yet");
        menu.open_selected_submenu();
        assert!(menu.close_submenu(), "one was open");
        assert!(!menu.close_submenu(), "already closed");
    }

    #[test]
    fn move_selection_drives_the_submenu_while_open_and_the_parent_once_closed() {
        let mut menu = ActionMenu::with_entries("t", submenu_entries());
        menu.selected = 1;
        menu.open_selected_submenu();
        menu.move_selection(1);
        assert_eq!(menu.submenu_selected, 1, "submenu selection moved");
        assert_eq!(menu.selected, 1, "parent selection untouched while open");
        menu.close_submenu();
        menu.move_selection(-1);
        assert_eq!(
            menu.selected, 0,
            "with nothing open, the parent moves again"
        );
    }

    #[test]
    fn selected_command_is_none_for_a_disabled_item_inside_an_open_submenu() {
        let mut menu = ActionMenu::with_entries(
            "t",
            vec![MenuEntry::Submenu {
                label: "Row Colour",
                items: vec![MenuItem {
                    command: MenuCommand::Action(ActionKind::SetRowColor("red")),
                    label: "red",
                    enabled: false,
                }],
            }],
        );
        menu.selected = 0;
        menu.open_selected_submenu();
        assert_eq!(
            menu.selected_command(),
            None,
            "disabled items are inert inside a submenu too"
        );
    }

    #[test]
    fn home_and_end_are_inert_with_nothing_selectable() {
        let mut menu =
            ActionMenu::with_entries("t", vec![MenuEntry::Separator, MenuEntry::Separator]);
        assert_eq!(menu.selected, 0, "no item to land on when opening");
        menu.select_end(true);
        assert_eq!(menu.selected, 0, "End finds nothing selectable either");
        menu.select_end(false);
        assert_eq!(menu.selected, 0, "nor Home");
    }

    #[test]
    fn draw_menu_reports_one_rect_per_visible_item_and_none_for_separators() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut menu = ActionMenu::with_entries(
            "Actions",
            vec![
                MenuEntry::Item(MenuItem::action(ActionKind::Focus, "Open Worktree")),
                MenuEntry::Separator,
                MenuEntry::Item(MenuItem::action(
                    ActionKind::CloseWorktree,
                    "Close Worktree",
                )),
            ],
        );
        menu.anchor = Anchor::At { col: 4, row: 2 };
        let mut region = None;
        terminal
            .draw(|frame| {
                region = Some(draw_menu(
                    frame,
                    frame.area(),
                    &mut menu,
                    GlyphMode::Unicode,
                ));
            })
            .unwrap();
        let region = region.unwrap();
        assert_eq!(region.rect, Rect::new(4, 2, MENU_WIDTH, 5));
        // Two items, at entry indices 0 and 2; the separator has no rect.
        let indices: Vec<usize> = region.items.iter().map(|(i, _)| *i).collect();
        assert_eq!(indices, vec![0, 2]);
        assert_eq!(region.items[0].1.y, 3, "first item is below the border");
        assert_eq!(region.items[1].1.y, 5);
        let text = buffer_text(&terminal);
        assert!(text.contains("Open Worktree"));
        assert!(text.contains("Close Worktree"));
    }

    #[test]
    fn draw_menu_marks_a_submenu_row_with_the_arrow_and_reports_its_item_rects() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut menu = ActionMenu::with_entries("Actions", submenu_entries());
        menu.selected = 1;
        menu.open_selected_submenu();
        menu.anchor = Anchor::At { col: 4, row: 2 };
        let mut region = None;
        terminal
            .draw(|frame| {
                region = Some(draw_menu(
                    frame,
                    frame.area(),
                    &mut menu,
                    GlyphMode::Unicode,
                ));
            })
            .unwrap();
        let region = region.unwrap();
        let submenu = region.submenu.expect("the submenu row is open");
        assert_eq!(submenu.items.len(), 2, "one rect per submenu item");
        let text = buffer_text(&terminal);
        assert!(text.contains('▸'), "the submenu row is arrow-marked");
        assert!(text.contains("red"));
        assert!(text.contains("blue"));
    }

    #[test]
    fn a_menu_taller_than_the_frame_scrolls_and_shows_the_affordances() {
        let backend = TestBackend::new(60, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let entries: Vec<MenuEntry> = (0..20)
            .map(|_| MenuEntry::Item(MenuItem::action(ActionKind::Focus, "row")))
            .collect();
        let mut menu = ActionMenu::with_entries("Actions", entries);
        menu.selected = 19;
        let mut region = None;
        terminal
            .draw(|frame| {
                region = Some(draw_menu(
                    frame,
                    frame.area(),
                    &mut menu,
                    GlyphMode::Unicode,
                ));
            })
            .unwrap();
        let region = region.unwrap();
        assert_eq!(
            region.rect.height, 6,
            "clamped to the frame, not overflowed"
        );
        assert!(menu.scroll > 0, "scrolled to reveal the selection");
        assert!(buffer_text(&terminal).contains('…'), "affordance drawn");
        // Every reported rect lies inside the popup.
        for (_, rect) in &region.items {
            assert!(rect.y > region.rect.y && rect.y < region.rect.bottom() - 1);
        }

        // Moving the selection back above the current scroll window (Home,
        // in effect) must scroll back up to follow it, not leave it hidden
        // above the visible range.
        let scrolled_down_to = menu.scroll;
        menu.selected = 0;
        terminal
            .draw(|frame| {
                draw_menu(frame, frame.area(), &mut menu, GlyphMode::Unicode);
            })
            .unwrap();
        assert!(
            menu.scroll < scrolled_down_to,
            "scrolled back up to follow the selection"
        );
        assert_eq!(menu.scroll, 0);
    }

    #[test]
    fn a_disabled_entry_renders_dimmed_rather_than_highlighted() {
        let backend = TestBackend::new(60, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut menu = ActionMenu::with_entries(
            "Actions",
            vec![MenuEntry::Item(MenuItem {
                command: MenuCommand::Action(ActionKind::CopyDirectory),
                label: "b",
                enabled: false,
            })],
        );
        // Force the selection onto the disabled entry: disabled items still
        // take the highlight (see `select_index`), so the dimmed style must
        // win over the "selected" reversed style.
        menu.selected = 0;
        let mut region = None;
        terminal
            .draw(|frame| {
                region = Some(draw_menu(
                    frame,
                    frame.area(),
                    &mut menu,
                    GlyphMode::Unicode,
                ));
            })
            .unwrap();
        let region = region.unwrap();
        let (_, rect) = region.items[0];
        let cell = &terminal.backend().buffer()[(rect.x, rect.y)];
        assert_eq!(cell.fg, Color::DarkGray, "dimmed, not reversed");
        assert!(!cell.modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn draw_list_popup_renders_a_custom_title_and_labels_with_the_selection_highlighted() {
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                draw_list_popup(
                    frame,
                    frame.area(),
                    "Move to which worktree?",
                    &["/repo/wt-1", "/repo/wt-2"],
                    1,
                );
            })
            .unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Move to which worktree?"));
        assert!(text.contains("/repo/wt-1"));
        assert!(text.contains("/repo/wt-2"));
    }

    #[test]
    fn draw_confirm_modal_renders_body_risks_and_info_verbatim() {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        let modal = ConfirmModal {
            prompt: ConfirmPrompt {
                title: "Close 2 worktrees?".to_string(),
                body_lines: vec!["Worktree: /repo/wt-1".to_string()],
                risk_lines: vec!["/repo/wt-1: [dirty] uncommitted changes".to_string()],
                info_lines: vec!["/repo/wt-1: [unpushed] 2 unpushed commits".to_string()],
            },
        };
        terminal
            .draw(|frame| draw_confirm_modal(frame, frame.area(), &modal))
            .unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Close 2 worktrees?"));
        assert!(text.contains("Worktree: /repo/wt-1"));
        assert!(text.contains("dirty"));
        assert!(text.contains("uncommitted changes"));
        assert!(text.contains("unpushed"));
        assert!(text.contains("confirm"));
    }
}
