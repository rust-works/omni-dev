//! The mouse/selection contract (issue #1585 §5, Phase 4): where a mouse
//! event landed, what it means there, and — for the terminal grid — whether
//! the child gets it instead.
//!
//! With `EnableMouseCapture` on, the host terminal stops doing its own
//! selection and every button, drag and wheel event arrives here. Selection
//! is therefore something the app *models*, and the contract is structural:
//!
//! 1. **Only the terminal grid is selectable.** A `Selection` lives on the
//!    emulator's `Term` alone (`TerminalTab`); the tree, borders, status bar
//!    and popups have no selection type, so no code path can produce a
//!    selectable string from chrome.
//! 2. **A drag is clamped to the region it started in** for its whole
//!    lifetime ([`RegionMap::clamp_to_grid`] / [`RegionMap::tree_row_clamped`]):
//!    dragging past a boundary resolves against the last in-bounds cell.
//! 3. **Copy comes from the emulator's grid** (`Term::selection_to_string`,
//!    soft-wrap aware), never from the rendered frame.
//! 4. **A child that asked for mouse reporting wins** ([`forwards_to_child`]):
//!    its events are encoded for it instead of starting a selection —
//!    unless `Alt` is held, which takes them back for TUI selection.
//!    Re-evaluated on every mouse-down.
//! 5. **The host's selection escape hatch is implemented by omission.**
//!    Terminals that honour the convention keep the gesture and do their own
//!    selection, never delivering it to the app. Nothing here detects or
//!    special-cases the modifier; doing so would re-implement a suppression
//!    the host already performed. **Do not add it.**
//!
//!    **The modifier is terminal-specific, and this was measured** (#1602
//!    Phase 0 spike, macOS): Ghostty keeps **shift**-drag and does its own
//!    selection; **iTerm2 keeps option-drag** and *forwards* shift-drag to
//!    the app; VS Code forwards both and has no host hatch at all, relying
//!    on §4's `⌥`-reclaim instead. Earlier revisions of this clause said the
//!    gesture was universally shift and named iTerm2 as an example — both
//!    halves were wrong. Every terminal tested does provide *some* working
//!    path to select out of a mouse-reporting child; which gesture it is, is
//!    not ours to know or to encode.
//! 6. **The right button opens a menu, never a selection or a paste.** No
//!    right-button path produces selectable text, reads the clipboard, or
//!    writes to a PTY other than §4's forward to a child that asked for the
//!    mouse. A right-click's only outcomes are: open a menu, forward to a
//!    reporting child, or nothing.
//!
//!    **It never inspects modifiers, and must not start** — also measured.
//!    Four terminals, four conventions: Terminal.app forwards a right-click
//!    with or without shift; Ghostty forwards a plain right-click and keeps
//!    shift-right for its own menu; **iTerm2 does exactly the reverse**; VS
//!    Code forwards both and shows its own menu as well. Acting on whatever
//!    the host chose to forward is correct in all four *because* nothing
//!    branches on a modifier — a `Shift` test in either direction would
//!    break precisely one of them. ([`classify_tree_click`] carries the same
//!    note where it would be "fixed".)
//! 7. **A menu is modal over the region beneath it, and inert beyond it.**
//!    While a context menu is open its rect out-ranks every other region in
//!    [`RegionMap::hit`]; the first click outside dismisses it and is
//!    *consumed*, never applied to what it landed on. This is the one
//!    exception to §1's "chrome is not interactive", and it is bounded to
//!    menus by `app::popup_mouse_enabled`.
//!
//! Nothing here touches PTY bytes or selection text: this module decides
//! and encodes, `app.rs` applies.

use std::time::{Duration, Instant};

use alacritty_terminal::term::TermMode;
use crossterm::event::{KeyModifiers, MouseButton, MouseEventKind};
use ratatui::layout::Rect;

/// Two clicks closer together than this, on the same cell, are a
/// double-click (three, a triple-click).
pub const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// Lines a wheel notch scrolls the terminal's scrollback by, or arrow keys
/// it sends a full-screen child that asked for alternate-scroll.
const WHEEL_LINES: i32 = 3;

/// Where a mouse event landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// A selectable row of the open context menu — `index` into its entries.
    PopupItem { index: usize },
    /// Inside the open context menu but not on a selectable row: its border,
    /// a separator, a `…` affordance. Consumed, but does nothing — a menu is
    /// modal over what it covers, so this must not fall through to the region
    /// underneath.
    PopupInert,
    /// A selectable row of the open **submenu** — `index` into its items.
    PopupSubmenuItem { index: usize },
    /// Outside the open context menu, which dismisses it. The event is
    /// *consumed* by the dismissal and never applied to whatever it landed
    /// on (contract §7).
    PopupOutside,
    /// Inside the tree pane: `row` is the flattened row index (the pane's
    /// scroll offset already applied). It may lie past the last row.
    Tree { row: usize },
    /// A group's tab strip. `tab` is the tab under the pointer, or `None`
    /// for the empty space after the last one.
    TabStrip { group: usize, tab: Option<usize> },
    /// Inside a group's terminal grid: 0-based grid coordinates.
    Terminal { group: usize, col: u16, line: u16 },
    /// The boundary between group `index` and `index + 1` — a drag here
    /// resizes the pair.
    Splitter { index: usize },
    /// Borders, the status bar, gaps — inert.
    Chrome,
}

/// One rendered group's hit-testable parts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupRegion {
    /// The one-row tab strip above the group.
    pub strip: Rect,
    /// The grid inside the group's border.
    pub grid: Rect,
    /// Each tab's `[start, end)` columns on the strip, in tab order —
    /// produced by the same code that renders them, so the two cannot
    /// disagree about where a tab is.
    pub tab_spans: Vec<(u16, u16)>,
}

impl GroupRegion {
    fn tab_at(&self, col: u16) -> Option<usize> {
        self.tab_spans
            .iter()
            .position(|(start, end)| col >= *start && col < *end)
    }
}

/// The open submenu's rect and its item rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubmenuRegion {
    pub rect: Rect,
    /// `(item index, row rect)` for each visible submenu item.
    pub items: Vec<(usize, Rect)>,
}

/// An open context menu's hit-testable parts, produced by the same `draw`
/// pass that renders it (`popup::draw_menu`), so the map and the frame cannot
/// disagree about where an item is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PopupRegion {
    /// The menu's outer rect, borders included.
    pub rect: Rect,
    /// `(entry index, row rect)` for each **selectable** item currently on
    /// screen. Separators and the `…` affordances are deliberately absent, so
    /// no click can land on one.
    pub items: Vec<(usize, Rect)>,
    /// The open submenu, when one is open.
    pub submenu: Option<SubmenuRegion>,
}

impl PopupRegion {
    fn hit(&self, col: u16, row: u16) -> Option<Hit> {
        // The submenu is drawn above the parent, so it is tested first —
        // otherwise a submenu overlapping its parent would be unclickable.
        if let Some(sub) = &self.submenu {
            if contains(sub.rect, col, row) {
                return Some(
                    sub.items
                        .iter()
                        .find(|(_, rect)| contains(*rect, col, row))
                        .map_or(Hit::PopupInert, |(index, _)| Hit::PopupSubmenuItem {
                            index: *index,
                        }),
                );
            }
        }
        if !contains(self.rect, col, row) {
            return None;
        }
        Some(
            self.items
                .iter()
                .find(|(_, rect)| contains(*rect, col, row))
                .map_or(Hit::PopupInert, |(index, _)| Hit::PopupItem {
                    index: *index,
                }),
        )
    }
}

/// The hit-testable regions of the last drawn frame — rebuilt by every
/// `draw`, so a stale map can only ever be one frame old.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RegionMap {
    /// The open context menu, when one is up. Out-ranks every other region.
    pub popup: Option<PopupRegion>,
    /// The tree pane's inner area (inside its border).
    pub tree: Rect,
    /// The tree's first visible row index.
    pub tree_offset: usize,
    /// One entry per *rendered* group, top to bottom. A group the layout
    /// dropped for want of height simply is not here.
    pub groups: Vec<GroupRegion>,
    /// The screen row of each splitter, one per adjacent pair of groups.
    pub splitters: Vec<u16>,
    /// The horizontal extent splitters are draggable over — the terminal
    /// side's columns.
    pub splitter_cols: (u16, u16),
}

impl RegionMap {
    pub fn hit(&self, col: u16, row: u16) -> Hit {
        // An open context menu is modal over everything it covers, so it is
        // tested before any other region (contract §7). Anything outside it
        // is a dismissal, not a click on what lies there.
        if let Some(popup) = &self.popup {
            return popup.hit(col, row).unwrap_or(Hit::PopupOutside);
        }
        // Tab strips win over the splitter. A group's strip sits directly
        // under the boundary above it, inside the splitter's 3-row box; if
        // the splitter took that row, every group below the first would
        // have unclickable tabs. The splitter keeps the boundary row and
        // the row above, which is still a 2-row target.
        for (index, group) in self.groups.iter().enumerate() {
            if contains(group.strip, col, row) {
                return Hit::TabStrip {
                    group: index,
                    tab: group.tab_at(col),
                };
            }
        }
        // Splitters next: they sit on a group's border row, which is
        // outside every grid, so this steals nothing selectable.
        if col >= self.splitter_cols.0 && col < self.splitter_cols.1 {
            if let Some((index, _)) = self
                .splitters
                .iter()
                .enumerate()
                .filter(|(_, r)| r.abs_diff(row) <= SPLITTER_REACH)
                .min_by_key(|(_, r)| r.abs_diff(row))
            {
                return Hit::Splitter { index };
            }
        }
        for (index, group) in self.groups.iter().enumerate() {
            if contains(group.grid, col, row) {
                return Hit::Terminal {
                    group: index,
                    col: col - group.grid.x,
                    line: row - group.grid.y,
                };
            }
        }
        if contains(self.tree, col, row) {
            return Hit::Tree {
                row: self.tree_offset + usize::from(row - self.tree.y),
            };
        }
        Hit::Chrome
    }

    /// Clamps a screen position into group `index`'s grid — the drag rule
    /// (contract §2) for a selection drag that started in that grid.
    pub fn clamp_to_grid(&self, index: usize, col: u16, row: u16) -> Option<(u16, u16)> {
        let grid = self
            .groups
            .get(index)
            .map(|g| g.grid)
            .filter(|r| r.width > 0 && r.height > 0)?;
        let col = col.clamp(grid.x, grid.x + grid.width - 1) - grid.x;
        let line = row.clamp(grid.y, grid.y + grid.height - 1) - grid.y;
        Some((col, line))
    }

    /// The tree row a screen row maps to, clamped to the visible rows — the
    /// drag rule for a range-mark drag that started in the tree.
    pub fn tree_row_clamped(&self, row: u16) -> usize {
        if self.tree.height == 0 {
            return self.tree_offset;
        }
        let row = row.clamp(self.tree.y, self.tree.y + self.tree.height - 1);
        self.tree_offset + usize::from(row - self.tree.y)
    }
}

/// How far either side of its row a splitter answers to — half of
/// [`super::layout::SPLITTER_HIT_HEIGHT`]'s 3-row box (a 1-row target is unusable
/// with a mouse).
const SPLITTER_REACH: u16 = super::layout::SPLITTER_HIT_HEIGHT / 2;

fn contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// What a button-down in the tree means (issue #1585 §5's tree column).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeClick {
    /// Plain click: move the cursor there.
    Focus,
    /// `⇧`-click: mark every row between the cursor and here.
    ExtendRange,
    /// `^`-click (`⌘` never reaches a terminal): toggle this row's mark.
    ToggleMark,
    /// Double-click: open a shell tab here.
    Open,
    /// Any right-button press: open the context menu here.
    ContextMenu,
}

/// Classifies a button-down on a tree row given its multi-click `count`.
///
/// **The right button opens the menu whatever modifiers are held, and this
/// must not change** (contract §6). The #1602 spike measured four terminals
/// with four different conventions: Ghostty forwards a plain right-click and
/// keeps shift-right for its own menu; iTerm2 does exactly the reverse;
/// VS Code forwards both; Terminal.app forwards both and has no menu of its
/// own. Acting on whatever the host chose to forward is correct in all four
/// *because* nothing here branches on a modifier. A `Shift` test in either
/// direction would break precisely one of them.
pub fn classify_tree_click(button: MouseButton, modifiers: KeyModifiers, count: u8) -> TreeClick {
    if button == MouseButton::Right {
        return TreeClick::ContextMenu;
    }
    if button != MouseButton::Left {
        return TreeClick::Focus;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        TreeClick::ToggleMark
    } else if modifiers.contains(KeyModifiers::SHIFT) {
        TreeClick::ExtendRange
    } else if count >= 2 {
        TreeClick::Open
    } else {
        TreeClick::Focus
    }
}

/// Counts consecutive clicks on one cell within [`MULTI_CLICK_WINDOW`].
#[derive(Debug, Default)]
pub struct ClickTracker {
    last: Option<(Instant, u16, u16)>,
    count: u8,
}

impl ClickTracker {
    /// Records a button-down at (`col`, `row`) and returns its click count:
    /// 1 for a fresh click, 2 for a double, 3 for a triple (then wraps).
    pub fn click(&mut self, col: u16, row: u16, now: Instant) -> u8 {
        let repeat = self.last.is_some_and(|(at, c, r)| {
            c == col && r == row && now.saturating_duration_since(at) <= MULTI_CLICK_WINDOW
        });
        self.count = if repeat && self.count < 3 {
            self.count + 1
        } else {
            1
        };
        self.last = Some((now, col, row));
        self.count
    }
}

/// Where a drag that is in progress started — the region it stays clamped
/// to until the button is released.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DragOrigin {
    #[default]
    None,
    /// A range-mark drag from tree row `anchor`.
    Tree { anchor: usize },
    /// A selection drag in group `group`'s terminal grid.
    Terminal { group: usize },
    /// A splitter drag resizing the pair either side of `index`.
    Splitter { index: usize },
    /// The child in group `group` owns this drag: motion and release are
    /// forwarded to it.
    Child { group: usize },
}

/// Contract §4: the focused child receives the mouse when it has requested
/// reporting and `Alt` is not held.
pub fn forwards_to_child(mode: TermMode, modifiers: KeyModifiers) -> bool {
    mode.intersects(TermMode::MOUSE_MODE) && !modifiers.contains(KeyModifiers::ALT)
}

/// Encodes one mouse event at 0-based grid (`col`, `line`) for a child
/// running in `mode`, in the xterm form the child negotiated (SGR when it
/// asked, else the legacy `CSI M` triple — UTF-8-widened when it asked for
/// that). `None` when `mode` does not report this kind of event (a drag
/// under click-only reporting, motion without drag reporting) or when the
/// legacy form cannot express the coordinate.
pub fn encode_mouse(
    kind: MouseEventKind,
    modifiers: KeyModifiers,
    col: u16,
    line: u16,
    mode: TermMode,
) -> Option<Vec<u8>> {
    let (button, release) = match kind {
        MouseEventKind::Down(b) => (button_code(b), false),
        MouseEventKind::Up(b) => (button_code(b), true),
        MouseEventKind::Drag(b) => {
            if !mode.intersects(TermMode::MOUSE_DRAG | TermMode::MOUSE_MOTION) {
                return None;
            }
            (button_code(b) + 32, false)
        }
        MouseEventKind::Moved => {
            if !mode.contains(TermMode::MOUSE_MOTION) {
                return None;
            }
            (3 + 32, false)
        }
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
    };
    let mut code = button;
    if modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    let (x, y) = (u32::from(col) + 1, u32::from(line) + 1);

    if mode.contains(TermMode::SGR_MOUSE) {
        let suffix = if release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{x};{y}{suffix}").into_bytes());
    }
    // Legacy: a release does not say which button.
    let code = if release { 3 } else { code };
    let mut out = b"\x1b[M".to_vec();
    for value in [code + 32, x + 32, y + 32] {
        if mode.contains(TermMode::UTF8_MOUSE) && value > 127 {
            let ch = char::from_u32(value)?;
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
        } else {
            out.push(u8::try_from(value).ok()?);
        }
    }
    Some(out)
}

fn button_code(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// What a wheel notch over the terminal grid does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WheelRoute {
    /// The child asked for mouse reporting: send it the encoded wheel event.
    Forward(Vec<u8>),
    /// A full-screen child with alternate-scroll on (`less`, `vim`): send it
    /// arrow keys, the xterm `alternateScroll` convention.
    ArrowKeys(Vec<u8>),
    /// Scroll the emulator's own scrollback by this many lines (positive =
    /// back in history).
    ScrollDisplay(i32),
}

/// Routes a wheel notch (`up` = towards history) at grid (`col`, `line`).
pub fn route_wheel(
    up: bool,
    modifiers: KeyModifiers,
    col: u16,
    line: u16,
    mode: TermMode,
) -> WheelRoute {
    let kind = if up {
        MouseEventKind::ScrollUp
    } else {
        MouseEventKind::ScrollDown
    };
    if forwards_to_child(mode, modifiers) {
        if let Some(bytes) = encode_mouse(kind, modifiers, col, line, mode) {
            return WheelRoute::Forward(bytes);
        }
    }
    if mode.contains(TermMode::ALT_SCREEN) && mode.contains(TermMode::ALTERNATE_SCROLL) {
        let key: &[u8] = match (up, mode.contains(TermMode::APP_CURSOR)) {
            (true, true) => b"\x1bOA",
            (true, false) => b"\x1b[A",
            (false, true) => b"\x1bOB",
            (false, false) => b"\x1b[B",
        };
        return WheelRoute::ArrowKeys(key.repeat(WHEEL_LINES as usize));
    }
    WheelRoute::ScrollDisplay(if up { WHEEL_LINES } else { -WHEEL_LINES })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A tree on the left and two stacked groups on the right, each with a
    /// one-row strip above a grid, and a splitter between them.
    fn map() -> RegionMap {
        RegionMap {
            popup: None,
            tree: Rect::new(1, 1, 30, 10),
            tree_offset: 5,
            groups: vec![
                GroupRegion {
                    strip: Rect::new(40, 0, 41, 1),
                    grid: Rect::new(41, 2, 39, 4),
                    tab_spans: vec![(40, 49), (50, 60)],
                },
                GroupRegion {
                    strip: Rect::new(40, 8, 41, 1),
                    grid: Rect::new(41, 10, 39, 4),
                    tab_spans: vec![(40, 49)],
                },
            ],
            splitters: vec![7],
            splitter_cols: (40, 81),
        }
    }

    #[test]
    fn hit_maps_each_region_and_applies_the_tree_offset() {
        let map = map();
        assert_eq!(map.hit(1, 1), Hit::Tree { row: 5 });
        assert_eq!(map.hit(30, 10), Hit::Tree { row: 14 });
        assert_eq!(
            map.hit(41, 2),
            Hit::Terminal {
                group: 0,
                col: 0,
                line: 0
            }
        );
        assert_eq!(
            map.hit(79, 13),
            Hit::Terminal {
                group: 1,
                col: 38,
                line: 3
            }
        );
        assert_eq!(map.hit(0, 0), Hit::Chrome, "the border");
        assert_eq!(map.hit(35, 5), Hit::Chrome, "the gap between panes");
        assert_eq!(map.hit(5, 11), Hit::Chrome, "below the tree");
        assert_eq!(map.hit(41, 1), Hit::Chrome, "the group's top border");
        let no_groups = RegionMap {
            groups: Vec::new(),
            splitters: Vec::new(),
            ..map
        };
        assert_eq!(no_groups.hit(50, 5), Hit::Chrome);
    }

    #[test]
    fn tab_strips_hit_test_to_a_tab_or_the_empty_space_after_them() {
        let map = map();
        assert_eq!(
            map.hit(41, 0),
            Hit::TabStrip {
                group: 0,
                tab: Some(0)
            }
        );
        assert_eq!(
            map.hit(55, 0),
            Hit::TabStrip {
                group: 0,
                tab: Some(1)
            }
        );
        assert_eq!(
            map.hit(49, 0),
            Hit::TabStrip {
                group: 0,
                tab: None
            },
            "the gap between two tabs"
        );
        assert_eq!(
            map.hit(70, 0),
            Hit::TabStrip {
                group: 0,
                tab: None
            },
            "past the last tab"
        );
        assert_eq!(
            map.hit(42, 8),
            Hit::TabStrip {
                group: 1,
                tab: Some(0)
            },
            "group 1's strip is clickable despite the splitter above it"
        );
    }

    #[test]
    fn the_splitter_answers_beside_its_row_but_never_steals_a_tab_strip() {
        let map = map();
        for row in [6, 7] {
            assert_eq!(map.hit(60, row), Hit::Splitter { index: 0 }, "row {row}");
        }
        // Row 8 is inside the splitter's box *and* is group 1's strip. The
        // strip wins, or no group below the first could be clicked.
        assert_eq!(
            map.hit(60, 8),
            Hit::TabStrip {
                group: 1,
                tab: None
            }
        );
        assert_ne!(map.hit(60, 5), Hit::Splitter { index: 0 });
        assert_ne!(map.hit(60, 9), Hit::Splitter { index: 0 });
        // Over the tree, the same rows are not a splitter.
        assert_eq!(map.hit(5, 7), Hit::Tree { row: 11 });
    }

    /// The same map with a menu open over the tree/strip/grid/splitter.
    fn map_with_popup() -> RegionMap {
        RegionMap {
            popup: Some(PopupRegion {
                submenu: None,
                rect: Rect::new(20, 3, 30, 6),
                items: vec![
                    (0, Rect::new(21, 4, 28, 1)),
                    // entry 1 is a separator: no rect, deliberately
                    (2, Rect::new(21, 6, 28, 1)),
                ],
            }),
            ..map()
        }
    }

    #[test]
    fn an_open_menu_out_ranks_every_other_region() {
        let map = map_with_popup();
        // Over the tree, a tab strip, a grid and a splitter alike: all
        // dismissals, never a click on what lies underneath.
        for (col, row) in [(1, 1), (41, 0), (41, 2), (60, 7)] {
            assert_eq!(map.hit(col, row), Hit::PopupOutside, "at {col},{row}");
        }
        // And the menu's own rows resolve to entry indices.
        assert_eq!(map.hit(21, 4), Hit::PopupItem { index: 0 });
        assert_eq!(map.hit(48, 6), Hit::PopupItem { index: 2 });
    }

    #[test]
    fn inside_the_menu_borders_and_separators_are_inert_not_dismissals() {
        let map = map_with_popup();
        assert_eq!(map.hit(20, 3), Hit::PopupInert, "top-left border");
        assert_eq!(map.hit(21, 5), Hit::PopupInert, "the separator row");
        assert_eq!(map.hit(49, 8), Hit::PopupInert, "bottom-right border");
        // One cell outside the rect is a dismissal.
        assert_eq!(map.hit(50, 5), Hit::PopupOutside);
        assert_eq!(map.hit(19, 5), Hit::PopupOutside);
    }

    /// `map_with_popup`'s menu with an open submenu placed clear of the
    /// parent's own rect.
    fn map_with_submenu() -> RegionMap {
        let mut map = map_with_popup();
        map.popup.as_mut().unwrap().submenu = Some(SubmenuRegion {
            rect: Rect::new(60, 3, 20, 4),
            items: vec![(0, Rect::new(61, 4, 18, 1)), (1, Rect::new(61, 5, 18, 1))],
        });
        map
    }

    #[test]
    fn a_point_on_a_submenu_item_row_hits_popup_submenu_item_with_its_index() {
        let map = map_with_submenu();
        assert_eq!(map.hit(65, 4), Hit::PopupSubmenuItem { index: 0 });
        assert_eq!(map.hit(65, 5), Hit::PopupSubmenuItem { index: 1 });
    }

    #[test]
    fn inside_the_submenu_rect_but_off_an_item_row_is_inert() {
        let map = map_with_submenu();
        // Row 6 is inside the submenu's rect (y 3..7) but past its one
        // item row (y 5).
        assert_eq!(map.hit(65, 6), Hit::PopupInert);
    }

    #[test]
    fn the_submenu_is_tested_before_the_parent_menu_when_the_rects_overlap() {
        let mut map = map_with_popup();
        // Deliberately overlaps the parent menu's rect (20,3,30,6) and its
        // own item-0 row (21,4,28,1) at the exact point under test.
        map.popup.as_mut().unwrap().submenu = Some(SubmenuRegion {
            rect: Rect::new(25, 4, 10, 3),
            items: vec![(0, Rect::new(25, 4, 8, 1))],
        });
        assert_eq!(
            map.hit(26, 4),
            Hit::PopupSubmenuItem { index: 0 },
            "the submenu wins even though the parent's own item 0 covers the same cell"
        );
    }

    #[test]
    fn without_a_popup_the_other_regions_are_reached_exactly_as_before() {
        let map = map();
        assert_eq!(map.hit(21, 4), Hit::Tree { row: 8 });
        assert!(matches!(map_with_popup().hit(21, 4), Hit::PopupItem { .. }));
    }

    #[test]
    fn drags_are_clamped_to_the_region_they_started_in() {
        let map = map();
        assert_eq!(map.clamp_to_grid(0, 0, 0), Some((0, 0)));
        assert_eq!(map.clamp_to_grid(0, 200, 200), Some((38, 3)));
        assert_eq!(map.clamp_to_grid(0, 45, 4), Some((4, 2)));
        // A drag in group 1 clamps to group 1, never into group 0.
        assert_eq!(map.clamp_to_grid(1, 60, 0), Some((19, 0)));
        assert_eq!(map.clamp_to_grid(9, 1, 1), None, "no such group");
        assert_eq!(map.tree_row_clamped(0), 5);
        assert_eq!(map.tree_row_clamped(200), 14);
        assert_eq!(map.tree_row_clamped(3), 7);
        let empty = RegionMap::default();
        assert_eq!(empty.clamp_to_grid(0, 3, 3), None);
        assert_eq!(empty.tree_row_clamped(3), 0);
    }

    #[test]
    fn tree_clicks_classify_by_button_modifier_and_count() {
        use MouseButton::{Left, Middle, Right};
        let none = KeyModifiers::NONE;
        assert_eq!(classify_tree_click(Left, none, 1), TreeClick::Focus);
        assert_eq!(classify_tree_click(Left, none, 2), TreeClick::Open);
        assert_eq!(
            classify_tree_click(Left, KeyModifiers::SHIFT, 1),
            TreeClick::ExtendRange
        );
        assert_eq!(
            classify_tree_click(Left, KeyModifiers::CONTROL, 2),
            TreeClick::ToggleMark,
            "a modifier beats the click count"
        );
        assert_eq!(classify_tree_click(Right, none, 2), TreeClick::ContextMenu);
        // Contract §6: modifiers never change what the right button means,
        // because different terminals forward different ones.
        for mods in [
            KeyModifiers::SHIFT,
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
        ] {
            assert_eq!(classify_tree_click(Right, mods, 1), TreeClick::ContextMenu);
        }
        assert_eq!(
            classify_tree_click(Middle, KeyModifiers::SHIFT, 1),
            TreeClick::Focus
        );
    }

    #[test]
    fn click_tracker_counts_repeats_on_one_cell_within_the_window() {
        let mut tracker = ClickTracker::default();
        let t0 = Instant::now();
        assert_eq!(tracker.click(3, 4, t0), 1);
        assert_eq!(tracker.click(3, 4, t0 + Duration::from_millis(100)), 2);
        assert_eq!(tracker.click(3, 4, t0 + Duration::from_millis(200)), 3);
        assert_eq!(
            tracker.click(3, 4, t0 + Duration::from_millis(300)),
            1,
            "wraps"
        );
        assert_eq!(tracker.click(3, 4, t0 + Duration::from_millis(350)), 2);
        assert_eq!(
            tracker.click(9, 4, t0 + Duration::from_millis(360)),
            1,
            "another cell"
        );
        assert_eq!(
            tracker.click(9, 4, t0 + Duration::from_secs(5)),
            1,
            "too slow"
        );
    }

    #[test]
    fn a_child_gets_the_mouse_only_when_it_asked_and_alt_is_up() {
        assert!(!forwards_to_child(TermMode::empty(), KeyModifiers::NONE));
        assert!(forwards_to_child(
            TermMode::MOUSE_REPORT_CLICK,
            KeyModifiers::NONE
        ));
        assert!(forwards_to_child(
            TermMode::MOUSE_DRAG | TermMode::SGR_MOUSE,
            KeyModifiers::CONTROL
        ));
        assert!(!forwards_to_child(
            TermMode::MOUSE_MOTION,
            KeyModifiers::ALT
        ));
    }

    #[test]
    fn sgr_encoding_matches_xterm() {
        let sgr = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        let enc = |kind, mods, mode| encode_mouse(kind, mods, 4, 2, mode).unwrap();
        assert_eq!(
            enc(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::NONE,
                sgr
            ),
            b"\x1b[<0;5;3M"
        );
        assert_eq!(
            enc(
                MouseEventKind::Up(MouseButton::Left),
                KeyModifiers::NONE,
                sgr
            ),
            b"\x1b[<0;5;3m",
            "SGR names the button on release"
        );
        assert_eq!(
            enc(
                MouseEventKind::Down(MouseButton::Right),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
                sgr
            ),
            b"\x1b[<22;5;3M"
        );
        assert_eq!(
            enc(MouseEventKind::ScrollUp, KeyModifiers::NONE, sgr),
            b"\x1b[<64;5;3M"
        );
        assert_eq!(
            enc(MouseEventKind::ScrollDown, KeyModifiers::ALT, sgr),
            b"\x1b[<73;5;3M"
        );
        assert_eq!(
            enc(
                MouseEventKind::Drag(MouseButton::Middle),
                KeyModifiers::NONE,
                sgr | TermMode::MOUSE_DRAG
            ),
            b"\x1b[<33;5;3M"
        );
        assert_eq!(
            enc(
                MouseEventKind::Moved,
                KeyModifiers::NONE,
                sgr | TermMode::MOUSE_MOTION
            ),
            b"\x1b[<35;5;3M"
        );
    }

    #[test]
    fn events_the_child_did_not_ask_for_are_not_encoded() {
        let click_only = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            encode_mouse(
                MouseEventKind::Drag(MouseButton::Left),
                KeyModifiers::NONE,
                0,
                0,
                click_only
            ),
            None
        );
        assert_eq!(
            encode_mouse(
                MouseEventKind::Moved,
                KeyModifiers::NONE,
                0,
                0,
                click_only | TermMode::MOUSE_DRAG
            ),
            None
        );
        assert!(encode_mouse(
            MouseEventKind::Drag(MouseButton::Left),
            KeyModifiers::NONE,
            0,
            0,
            click_only | TermMode::MOUSE_DRAG
        )
        .is_some());
    }

    #[test]
    fn legacy_encoding_uses_the_offset_triple_and_widens_only_under_utf8() {
        let legacy = TermMode::MOUSE_REPORT_CLICK;
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::NONE,
                4,
                2,
                legacy
            )
            .unwrap(),
            b"\x1b[M\x20\x25\x23"
        );
        assert_eq!(
            encode_mouse(
                MouseEventKind::Up(MouseButton::Left),
                KeyModifiers::NONE,
                4,
                2,
                legacy
            )
            .unwrap(),
            b"\x1b[M\x23\x25\x23",
            "legacy release is button 3"
        );
        // Column 200 → 233: representable as a raw byte, not valid UTF-8 as
        // a single byte, so the UTF-8 form widens it.
        let raw = encode_mouse(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
            200,
            0,
            legacy,
        )
        .unwrap();
        assert_eq!(raw[4], 233);
        let utf8 = encode_mouse(
            MouseEventKind::Down(MouseButton::Left),
            KeyModifiers::NONE,
            200,
            0,
            legacy | TermMode::UTF8_MOUSE,
        )
        .unwrap();
        assert_eq!(&utf8[4..6], "é".as_bytes());
        // Past 223 the raw triple cannot express the coordinate.
        assert_eq!(
            encode_mouse(
                MouseEventKind::Down(MouseButton::Left),
                KeyModifiers::NONE,
                300,
                0,
                legacy
            ),
            None
        );
    }

    #[test]
    fn wheel_routes_to_child_then_alt_screen_arrows_then_scrollback() {
        let none = KeyModifiers::NONE;
        let reporting = TermMode::MOUSE_REPORT_CLICK | TermMode::SGR_MOUSE;
        assert_eq!(
            route_wheel(true, none, 0, 0, reporting),
            WheelRoute::Forward(b"\x1b[<64;1;1M".to_vec())
        );
        assert_eq!(
            route_wheel(true, KeyModifiers::ALT, 0, 0, reporting),
            WheelRoute::ScrollDisplay(WHEEL_LINES),
            "alt takes the wheel back"
        );
        let alt_screen = TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL;
        assert_eq!(
            route_wheel(true, none, 0, 0, alt_screen),
            WheelRoute::ArrowKeys(b"\x1b[A\x1b[A\x1b[A".to_vec())
        );
        assert_eq!(
            route_wheel(false, none, 0, 0, alt_screen | TermMode::APP_CURSOR),
            WheelRoute::ArrowKeys(b"\x1bOB\x1bOB\x1bOB".to_vec())
        );
        assert_eq!(
            route_wheel(false, none, 0, 0, TermMode::ALT_SCREEN),
            WheelRoute::ScrollDisplay(-WHEEL_LINES),
            "alt screen without alternate-scroll is plain scrollback"
        );
    }
}
