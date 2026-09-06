//! The pane stack (issue #1585 §4, Phase 4b): groups of terminal tabs, the
//! tab strip over each, and the focus/lifecycle rules that decide which tab
//! a key, a PTY event or a resize belongs to.
//!
//! [`super::layout`] owns the geometry (weights, splitters, drags);
//! this owns the *tabs*. The two are separate so the maths stays testable
//! with no PTY in sight, and this file's rules stay testable with scripted
//! children rather than a real terminal.
//!
//! A tab remembers the worktree it was opened in, which is what feeds the
//! tree's `here` cue without registering with the daemon
//! ([ADR-0072](../../../../docs/adrs/adr-0072.md) §1).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use alacritty_terminal::event::Event as TermEvent;
use anyhow::Result;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr as _;

use super::layout::{self, MIN_GROUP_HEIGHT};
use super::session_layout;
use super::terminal::{GridSize, TabEffect, TabId, TabKind, TerminalTab};

/// One vertical slice of the terminal side: a tab strip and the active
/// tab's grid under it.
pub struct PaneGroup {
    pub tabs: Vec<TerminalTab>,
    /// Index into `tabs`; always valid while `tabs` is non-empty.
    pub active: usize,
}

impl PaneGroup {
    fn new(tab: TerminalTab) -> Self {
        Self {
            tabs: vec![tab],
            active: 0,
        }
    }

    pub fn active_tab(&self) -> Option<&TerminalTab> {
        self.tabs.get(self.active)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut TerminalTab> {
        self.tabs.get_mut(self.active)
    }

    /// Clamps `active` back into range after a removal.
    fn clamp_active(&mut self) {
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len().saturating_sub(1);
        }
    }
}

/// Where a tab lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TabAddr {
    pub group: usize,
    pub tab: usize,
}

/// The stack of groups, their relative weights, and which group has focus.
///
/// Empty (no groups) is the normal starting state: the tree pane then takes
/// the full width, exactly as in Phases 1–3.
#[derive(Default)]
pub struct PaneLayout {
    pub groups: Vec<PaneGroup>,
    /// One weight per group, top to bottom — see [`layout::split_groups`].
    pub weights: Vec<u16>,
    pub focused: usize,
    next_id: TabId,
}

impl PaneLayout {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// The next tab id, unique for this process's lifetime — ids are never
    /// reused, so a PTY event from a closed tab can always be told apart
    /// from one for a live tab that took its slot.
    fn take_id(&mut self) -> TabId {
        self.next_id += 1;
        self.next_id
    }

    fn focused_group(&self) -> Option<&PaneGroup> {
        self.groups.get(self.focused)
    }

    /// The focused group's active tab — the one a key or a copy acts on.
    pub fn active_tab(&self) -> Option<&TerminalTab> {
        self.focused_group()?.active_tab()
    }

    /// Group `index`'s active tab — how a mouse event, which names the
    /// group it landed in, reaches a tab without changing focus first.
    pub fn group_tab(&self, index: usize) -> Option<&TerminalTab> {
        self.groups.get(index)?.active_tab()
    }

    /// Every tab, in stack order.
    pub fn tabs(&self) -> impl Iterator<Item = &TerminalTab> {
        self.groups.iter().flat_map(|g| g.tabs.iter())
    }

    /// Whether any tab still has a live child — what `q` asks about.
    pub fn any_alive(&self) -> bool {
        self.tabs().any(TerminalTab::is_alive)
    }

    /// Finds a tab by id.
    pub fn find(&self, id: TabId) -> Option<TabAddr> {
        self.groups.iter().enumerate().find_map(|(group, g)| {
            g.tabs
                .iter()
                .position(|t| t.id() == id)
                .map(|tab| TabAddr { group, tab })
        })
    }

    pub fn tab_mut(&mut self, addr: TabAddr) -> Option<&mut TerminalTab> {
        self.groups.get_mut(addr.group)?.tabs.get_mut(addr.tab)
    }

    /// The first tab opened in `worktree`, if any — so `enter` on a row
    /// that already has a tab focuses it instead of opening a second.
    pub fn find_in_worktree(&self, worktree: &Path, kind: TabKind) -> Option<TabAddr> {
        self.groups.iter().enumerate().find_map(|(group, g)| {
            g.tabs
                .iter()
                .position(|t| t.opened_in == worktree && t.kind == kind && t.is_alive())
                .map(|tab| TabAddr { group, tab })
        })
    }

    /// Focuses `addr`, making it its group's active tab.
    pub fn focus(&mut self, addr: TabAddr) {
        if let Some(group) = self.groups.get_mut(addr.group) {
            if addr.tab < group.tabs.len() {
                group.active = addr.tab;
                self.focused = addr.group;
            }
        }
    }

    /// Opens a tab in the focused group (or the first group when there is
    /// none yet), and focuses it. `spawn` builds the tab from the id this
    /// allocates — injected so tests can script the child.
    pub fn open_tab(
        &mut self,
        spawn: impl FnOnce(TabId) -> Result<TerminalTab>,
    ) -> Result<TabAddr> {
        let id = self.take_id();
        let tab = spawn(id)?;
        if self.groups.is_empty() {
            self.groups.push(PaneGroup::new(tab));
            self.weights = layout::even_weights(1);
            self.focused = 0;
            return Ok(TabAddr { group: 0, tab: 0 });
        }
        let group = self.focused.min(self.groups.len() - 1);
        self.groups[group].tabs.push(tab);
        let tab = self.groups[group].tabs.len() - 1;
        self.groups[group].active = tab;
        self.focused = group;
        Ok(TabAddr { group, tab })
    }

    /// Opens a tab in a **new group** below the focused one — `alt-s`.
    pub fn split(&mut self, spawn: impl FnOnce(TabId) -> Result<TerminalTab>) -> Result<TabAddr> {
        if self.groups.is_empty() {
            return self.open_tab(spawn);
        }
        let id = self.take_id();
        let tab = spawn(id)?;
        let at = (self.focused + 1).min(self.groups.len());
        self.groups.insert(at, PaneGroup::new(tab));
        self.weights.insert(at, average_weight(&self.weights));
        self.focused = at;
        Ok(TabAddr { group: at, tab: 0 })
    }

    /// Closes the tab at `addr`, shutting its child down. Returns the
    /// worktree it was opened in, so the caller can update the `here` cue.
    /// An emptied group is removed, and its weight with it.
    pub fn close_tab(&mut self, addr: TabAddr) -> Option<PathBuf> {
        let group = self.groups.get_mut(addr.group)?;
        if addr.tab >= group.tabs.len() {
            return None;
        }
        let mut tab = group.tabs.remove(addr.tab);
        tab.shutdown();
        group.clamp_active();
        if group.tabs.is_empty() {
            self.groups.remove(addr.group);
            if addr.group < self.weights.len() {
                self.weights.remove(addr.group);
            }
            if self.focused >= self.groups.len() {
                self.focused = self.groups.len().saturating_sub(1);
            }
        } else {
            self.focused = addr.group;
        }
        Some(tab.opened_in)
    }

    /// Closes every tab in `addr.group` except `addr.tab`, shutting each one
    /// down exactly as [`Self::close_tab`] does. Returns the `opened_in` of
    /// every tab closed, in close order — a `Vec` rather than an `Option`
    /// because closing several tabs at once (the tab-strip menu's "Close
    /// Other Tabs") can retire several different worktrees' `here` cues, and
    /// a caller that only saw the last one would leave the others lit for a
    /// tab that no longer exists.
    ///
    /// The anchor tab always survives, so unlike `close_tab` this can never
    /// empty the group — there is deliberately no group-removal or
    /// weight-vector handling here.
    pub fn close_other_tabs(&mut self, addr: TabAddr) -> Vec<PathBuf> {
        let Some(group) = self.groups.get_mut(addr.group) else {
            return Vec::new();
        };
        if addr.tab >= group.tabs.len() {
            return Vec::new();
        }
        let mut closed = Vec::with_capacity(group.tabs.len() - 1);
        // Remove from the back so earlier indices — including the anchor —
        // never shift out from under us.
        for index in (0..group.tabs.len()).rev() {
            if index == addr.tab {
                continue;
            }
            let mut tab = group.tabs.remove(index);
            tab.shutdown();
            closed.push(tab.opened_in);
        }
        closed.reverse(); // report in close order, not removal order
                          // Only the anchor is left, so it is now the group's sole tab; run it
                          // through clamp_active anyway rather than assuming, since a future
                          // reader would otherwise have to re-derive that 0 is always in range.
        group.active = 0;
        group.clamp_active();
        self.focused = addr.group;
        closed
    }

    /// Closes every tab in `addr.group` after index `addr.tab`, shutting
    /// each one down exactly as [`Self::close_tab`] does. Returns the
    /// `opened_in` of every tab closed, in close order — see
    /// [`Self::close_other_tabs`] for why a `Vec` rather than an `Option`.
    ///
    /// The anchor tab and everything left of it always survive, so unlike
    /// `close_tab` this can never empty the group — there is deliberately
    /// no group-removal or weight-vector handling here.
    pub fn close_tabs_to_right(&mut self, addr: TabAddr) -> Vec<PathBuf> {
        let Some(group) = self.groups.get_mut(addr.group) else {
            return Vec::new();
        };
        if addr.tab >= group.tabs.len() {
            return Vec::new();
        }
        let mut closed = Vec::with_capacity(group.tabs.len() - addr.tab - 1);
        for index in (addr.tab + 1..group.tabs.len()).rev() {
            let mut tab = group.tabs.remove(index);
            tab.shutdown();
            closed.push(tab.opened_in);
        }
        closed.reverse(); // report in close order, not removal order
        group.clamp_active();
        self.focused = addr.group;
        closed
    }

    /// Closes the focused group's active tab.
    pub fn close_active(&mut self) -> Option<PathBuf> {
        let addr = TabAddr {
            group: self.focused,
            tab: self.focused_group()?.active,
        };
        self.close_tab(addr)
    }

    /// The set of worktrees that still have a tab open — the `here` cue's
    /// truth after any close, since one worktree may have several tabs.
    pub fn open_worktrees(&self) -> HashSet<PathBuf> {
        self.tabs().map(|t| t.opened_in.clone()).collect()
    }

    /// Cycles the focused group's active tab by `delta` (`alt-[` / `alt-]`),
    /// wrapping.
    pub fn cycle_tab(&mut self, delta: isize) {
        let Some(group) = self.groups.get_mut(self.focused) else {
            return;
        };
        let len = group.tabs.len();
        if len == 0 {
            return;
        }
        let len_i = isize::try_from(len).unwrap_or(isize::MAX);
        let current = isize::try_from(group.active).unwrap_or(0);
        group.active = usize::try_from((current + delta).rem_euclid(len_i)).unwrap_or(0);
    }

    /// Selects the focused group's `index`-th tab (`alt-1`…`alt-9`).
    pub fn select_tab(&mut self, index: usize) -> bool {
        match self.groups.get_mut(self.focused) {
            Some(group) if index < group.tabs.len() => {
                group.active = index;
                true
            }
            _ => false,
        }
    }

    /// Moves focus between groups by `delta`, clamped.
    pub fn cycle_group(&mut self, delta: isize) {
        if self.groups.is_empty() {
            return;
        }
        let max = isize::try_from(self.groups.len() - 1).unwrap_or(0);
        let current = isize::try_from(self.focused).unwrap_or(0);
        self.focused = usize::try_from((current + delta).clamp(0, max)).unwrap_or(0);
    }

    /// Moves the active tab into the adjacent group (`alt-⇧↑` / `alt-⇧↓`),
    /// creating no groups and removing one that empties. Returns whether
    /// anything moved.
    pub fn move_tab_to_group(&mut self, delta: isize) -> bool {
        if self.groups.len() < 2 {
            return false;
        }
        let from = self.focused;
        let Ok(target) = usize::try_from(isize::try_from(from).unwrap_or(0) + delta) else {
            return false;
        };
        if target >= self.groups.len() {
            return false;
        }
        let Some(group) = self.groups.get_mut(from) else {
            return false;
        };
        if group.tabs.is_empty() {
            return false;
        }
        let tab = group.tabs.remove(group.active);
        group.clamp_active();
        let emptied = group.tabs.is_empty();
        // Removing the source group shifts every later index down by one.
        let target = if emptied && target > from {
            target - 1
        } else {
            target
        };
        if emptied {
            self.groups.remove(from);
            if from < self.weights.len() {
                self.weights.remove(from);
            }
        }
        let Some(dest) = self.groups.get_mut(target) else {
            return false; // unreachable: the target existed a moment ago
        };
        dest.tabs.push(tab);
        dest.active = dest.tabs.len() - 1;
        self.focused = target;
        true
    }

    /// Resets the stack to equal weights — `alt-0`.
    pub fn reset_weights(&mut self) {
        self.weights = layout::even_weights(self.groups.len());
    }

    /// The stack's persistable shape: which worktree each tab runs in, how
    /// the groups are weighted, and where focus is. No process state — a
    /// restored tab is a new child (ADR-0072 §2).
    pub fn to_saved(&self) -> session_layout::SavedLayout {
        session_layout::SavedLayout {
            version: 1,
            groups: self
                .groups
                .iter()
                .enumerate()
                .map(|(index, group)| session_layout::SavedGroup {
                    tabs: group
                        .tabs
                        .iter()
                        .map(|tab| session_layout::SavedTab {
                            path: tab.opened_in.clone(),
                            kind: tab.kind.into(),
                        })
                        .collect(),
                    active: group.active,
                    weight: self.weights.get(index).copied().unwrap_or(1),
                })
                .collect(),
            focused: self.focused,
        }
    }

    /// Rebuilds the stack from a saved layout, spawning one child per tab.
    /// A tab whose spawn fails is skipped and counted rather than aborting
    /// the restore: a missing shell should cost you that tab, not the
    /// session. Returns how many tabs failed to spawn.
    pub fn restore(
        &mut self,
        saved: &session_layout::SavedLayout,
        mut spawn: impl FnMut(TabId, TabKind, &Path) -> Result<TerminalTab>,
    ) -> usize {
        let mut failed = 0;
        for group in &saved.groups {
            let mut tabs = Vec::with_capacity(group.tabs.len());
            for saved_tab in &group.tabs {
                let id = self.take_id();
                match spawn(id, saved_tab.kind.into(), &saved_tab.path) {
                    Ok(tab) => tabs.push(tab),
                    Err(_) => failed += 1,
                }
            }
            if tabs.is_empty() {
                continue;
            }
            let active = group.active.min(tabs.len() - 1);
            self.groups.push(PaneGroup { tabs, active });
            self.weights.push(group.weight.max(1));
        }
        self.focused = saved.focused.min(self.groups.len().saturating_sub(1));
        failed
    }

    /// Routes one emulator event to the tab it belongs to. Returns `None`
    /// when no live tab has that id (an event from a tab already closed).
    pub fn handle_event(&mut self, id: TabId, event: TermEvent) -> Option<TabEffect> {
        let addr = self.find(id)?;
        Some(self.tab_mut(addr)?.handle_event(event))
    }

    /// Lays the stack out in `area` and resizes every visible group's
    /// active tab to the grid it will be drawn into. Returns one
    /// [`GroupRects`] per rendered group.
    pub fn arrange(&mut self, area: Rect) -> Vec<GroupRects> {
        let rects = layout::split_groups(area, &self.weights);
        let mut out = Vec::with_capacity(rects.len());
        for (index, rect) in rects.into_iter().enumerate() {
            let strip = Rect::new(rect.x, rect.y, rect.width, 1);
            let body = Rect::new(
                rect.x,
                rect.y + 1,
                rect.width,
                rect.height.saturating_sub(1),
            );
            let grid = Block::default().borders(Borders::ALL).inner(body);
            if let Some(tab) = self
                .groups
                .get_mut(index)
                .and_then(PaneGroup::active_tab_mut)
            {
                tab.resize(GridSize {
                    cols: grid.width,
                    lines: grid.height,
                });
            }
            let tab_spans = self
                .groups
                .get(index)
                .map(|g| strip_cells(g, strip).iter().map(|c| c.span).collect())
                .unwrap_or_default();
            out.push(GroupRects {
                strip,
                body,
                grid,
                index,
                tab_spans,
            });
        }
        out
    }

    /// Draws every group: its tab strip, then its active tab.
    pub fn draw(&self, frame: &mut Frame<'_>, rects: &[GroupRects], focused_pane: bool) {
        for group_rects in rects {
            let Some(group) = self.groups.get(group_rects.index) else {
                continue;
            };
            let is_focused = focused_pane && group_rects.index == self.focused;
            draw_tab_strip(frame, group_rects.strip, group, is_focused);
            if let Some(tab) = group.active_tab() {
                tab.draw(frame, group_rects.body, is_focused);
            }
        }
    }
}

/// A group's three rects — its tab strip, the bordered body, and the grid
/// inside that border — plus where each tab sits on the strip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupRects {
    pub strip: Rect,
    pub body: Rect,
    pub grid: Rect,
    pub index: usize,
    /// Each tab's `[start, end)` columns, from the same computation that
    /// renders them — so a click can never resolve to a different tab than
    /// the one under the pointer.
    pub tab_spans: Vec<(u16, u16)>,
}

/// One tab's strip cell: the text drawn and the columns it occupies.
struct StripCell {
    text: String,
    span: (u16, u16),
}

/// Lays the tab strip out for `group` within `area`, in display columns.
/// The single source of truth for both rendering and hit-testing.
fn strip_cells(group: &PaneGroup, area: Rect) -> Vec<StripCell> {
    let mut cells = Vec::with_capacity(group.tabs.len());
    let mut x = area.x;
    let end = area.x.saturating_add(area.width);
    for (index, tab) in group.tabs.iter().enumerate() {
        let text = format!(" {} {} ", index + 1, tab.strip_label());
        let width = u16::try_from(text.width()).unwrap_or(u16::MAX);
        if x >= end {
            break; // the strip is full; the rest are not drawn or clickable
        }
        let stop = x.saturating_add(width).min(end);
        cells.push(StripCell {
            text,
            span: (x, stop),
        });
        // One column of gap between tabs.
        x = stop.saturating_add(1);
    }
    cells
}

/// The mean of the current weights — what a newly split group starts at, so
/// a split neither dominates nor vanishes.
fn average_weight(weights: &[u16]) -> u16 {
    if weights.is_empty() {
        return 1;
    }
    let total: u32 = weights.iter().map(|w| u32::from(*w)).sum();
    u16::try_from(total / u32::try_from(weights.len()).unwrap_or(1))
        .unwrap_or(1)
        .max(1)
}

/// One line of `[1 shell·repo] [2 claude·repo]`, the active tab reversed.
fn draw_tab_strip(frame: &mut Frame<'_>, area: Rect, group: &PaneGroup, focused: bool) {
    if area.height == 0 {
        return;
    }
    let mut spans = Vec::with_capacity(group.tabs.len() * 2);
    for (index, cell) in strip_cells(group, area).into_iter().enumerate() {
        let mut style = Style::default();
        if index == group.active {
            style = style.add_modifier(Modifier::REVERSED);
            if focused {
                style = style.fg(Color::Cyan);
            }
        } else {
            style = style.fg(Color::DarkGray);
        }
        spans.push(Span::styled(cell.text, style));
        spans.push(Span::raw(" "));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The minimum terminal-side height that can host `count` groups — used to
/// refuse a split that would not fit rather than silently dropping a group.
pub fn min_height_for(count: usize) -> u16 {
    u16::try_from(count)
        .unwrap_or(u16::MAX)
        .saturating_mul(MIN_GROUP_HEIGHT)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[cfg_attr(not(unix), allow(unused_imports))]
    use tokio::sync::mpsc;

    #[cfg(unix)]
    fn spawner(
        script: &'static str,
        cwd: PathBuf,
        tx: mpsc::UnboundedSender<(TabId, TermEvent)>,
    ) -> impl FnOnce(TabId) -> Result<TerminalTab> {
        move |id| {
            let request = super::super::terminal::pty::SpawnRequest {
                tab: id,
                program: Some((
                    "/bin/sh".to_string(),
                    vec!["-c".to_string(), script.to_string()],
                )),
                cwd,
                size: GridSize { cols: 40, lines: 6 },
                extra_env: Vec::new(),
            };
            TerminalTab::from_request(TabKind::Shell, request, tx)
        }
    }

    #[cfg(unix)]
    fn layout_with(count: usize) -> (PaneLayout, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut panes = PaneLayout::default();
        for _ in 0..count {
            panes
                .split(spawner("sleep 5", dir.path().to_path_buf(), tx.clone()))
                .unwrap();
        }
        (panes, dir)
    }

    #[test]
    fn an_empty_layout_has_no_groups_and_no_active_tab() {
        let panes = PaneLayout::default();
        assert!(panes.is_empty());
        assert_eq!(panes.group_count(), 0);
        assert!(panes.active_tab().is_none());
        assert!(!panes.any_alive());
        assert!(panes.find(1).is_none());
        assert!(panes.group_tab(0).is_none());
        assert!(panes.open_worktrees().is_empty());
    }

    #[test]
    fn average_weight_of_nothing_is_one() {
        assert_eq!(average_weight(&[]), 1);
        assert_eq!(average_weight(&[2, 4]), 3);
        assert_eq!(average_weight(&[0, 0]), 1, "never zero");
    }

    #[test]
    fn min_height_scales_with_the_group_count() {
        assert_eq!(min_height_for(1), MIN_GROUP_HEIGHT);
        assert_eq!(min_height_for(3), MIN_GROUP_HEIGHT * 3);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn opening_adds_to_the_focused_group_and_splitting_makes_a_new_one() {
        let (mut panes, dir) = layout_with(0);
        let (tx, _rx) = mpsc::unbounded_channel();
        let spawn = || spawner("sleep 5", dir.path().to_path_buf(), tx.clone());

        // The first open creates the one group.
        let first = panes.open_tab(spawn()).unwrap();
        assert_eq!(first, TabAddr { group: 0, tab: 0 });
        assert_eq!(panes.group_count(), 1);
        assert_eq!(panes.weights, vec![1]);
        assert!(panes.any_alive());

        // The second joins it as a tab.
        let second = panes.open_tab(spawn()).unwrap();
        assert_eq!(second, TabAddr { group: 0, tab: 1 });
        assert_eq!(panes.group_count(), 1);
        assert_eq!(panes.groups[0].tabs.len(), 2);
        assert_eq!(panes.groups[0].active, 1, "a new tab takes focus");

        // A split makes a second group below, and focuses it.
        let split = panes.split(spawn()).unwrap();
        assert_eq!(split, TabAddr { group: 1, tab: 0 });
        assert_eq!(panes.group_count(), 2);
        assert_eq!(panes.weights.len(), 2);
        assert_eq!(panes.focused, 1);

        // Ids are unique and never reused.
        let ids: Vec<TabId> = panes.tabs().map(TerminalTab::id).collect();
        let unique: HashSet<TabId> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len());
        for tab in panes.groups.iter_mut().flat_map(|g| g.tabs.iter_mut()) {
            tab.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn closing_removes_the_tab_then_its_group_and_reports_the_worktree() {
        let (mut panes, dir) = layout_with(2);
        let here = dir.path().to_path_buf();
        assert_eq!(panes.group_count(), 2);

        // One worktree, two tabs: closing one leaves the cue set.
        let closed = panes.close_active().unwrap();
        assert_eq!(closed, here);
        assert_eq!(panes.group_count(), 1, "the emptied group went with it");
        assert_eq!(panes.weights.len(), 1);
        assert_eq!(panes.focused, 0);
        assert!(panes.open_worktrees().contains(&here), "one tab remains");

        // Closing the last one empties the layout.
        assert_eq!(panes.close_active(), Some(here));
        assert!(panes.is_empty());
        assert!(panes.open_worktrees().is_empty());
        assert_eq!(panes.focused, 0);
        assert!(panes.close_active().is_none(), "nothing left to close");
        assert!(panes.close_tab(TabAddr { group: 9, tab: 9 }).is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn close_other_tabs_leaves_the_anchor_and_reports_every_closed_worktree() {
        let (mut panes, _dir) = layout_with(0);
        let (tx, _rx) = mpsc::unbounded_channel();
        let dirs: Vec<tempfile::TempDir> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
        for d in &dirs {
            panes
                .open_tab(spawner("sleep 5", d.path().to_path_buf(), tx.clone()))
                .unwrap();
        }
        assert_eq!(panes.groups[0].tabs.len(), 4);
        let anchor = dirs[1].path().to_path_buf();

        let closed = panes.close_other_tabs(TabAddr { group: 0, tab: 1 });

        assert_eq!(panes.group_count(), 1, "never empties the group");
        assert_eq!(panes.groups[0].tabs.len(), 1, "only the anchor remains");
        assert_eq!(panes.groups[0].tabs[0].opened_in, anchor);
        assert_eq!(
            panes.groups[0].active, 0,
            "the survivor is now the active tab"
        );
        assert_eq!(panes.focused, 0);
        let expected: HashSet<PathBuf> = [
            dirs[0].path().to_path_buf(),
            dirs[2].path().to_path_buf(),
            dirs[3].path().to_path_buf(),
        ]
        .into_iter()
        .collect();
        let got: HashSet<PathBuf> = closed.iter().cloned().collect();
        assert_eq!(
            got, expected,
            "every non-anchor tab is reported, not just one"
        );
        assert_eq!(closed.len(), 3, "no duplicates, no anchor");

        // Out-of-range addresses are no-ops.
        assert!(panes
            .close_other_tabs(TabAddr { group: 9, tab: 0 })
            .is_empty());
        assert!(panes
            .close_other_tabs(TabAddr { group: 0, tab: 9 })
            .is_empty());
        assert_eq!(
            panes.groups[0].tabs.len(),
            1,
            "the bad addresses touched nothing"
        );

        for tab in panes.groups.iter_mut().flat_map(|g| g.tabs.iter_mut()) {
            tab.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn close_tabs_to_right_leaves_the_anchor_and_everything_left_of_it() {
        let (mut panes, _dir) = layout_with(0);
        let (tx, _rx) = mpsc::unbounded_channel();
        let dirs: Vec<tempfile::TempDir> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
        for d in &dirs {
            panes
                .open_tab(spawner("sleep 5", d.path().to_path_buf(), tx.clone()))
                .unwrap();
        }
        // The last-opened tab holds focus already; closing to its right of
        // the anchor leaves `active` pointing past the surviving tabs, so
        // this also exercises the clamp.
        assert_eq!(panes.groups[0].active, 3);

        let closed = panes.close_tabs_to_right(TabAddr { group: 0, tab: 1 });

        assert_eq!(panes.group_count(), 1, "never empties the group");
        assert_eq!(
            panes.groups[0].tabs.len(),
            2,
            "the anchor and everything left of it survive"
        );
        assert_eq!(panes.groups[0].tabs[0].opened_in, dirs[0].path());
        assert_eq!(panes.groups[0].tabs[1].opened_in, dirs[1].path());
        assert_eq!(
            panes.groups[0].active, 1,
            "active is clamped back into range"
        );
        assert_eq!(panes.focused, 0);
        assert_eq!(
            closed,
            vec![dirs[2].path().to_path_buf(), dirs[3].path().to_path_buf()],
            "every tab to the right is reported, in close order, not just one"
        );

        // Out-of-range addresses are no-ops.
        assert!(panes
            .close_tabs_to_right(TabAddr { group: 9, tab: 0 })
            .is_empty());
        assert!(panes
            .close_tabs_to_right(TabAddr { group: 0, tab: 9 })
            .is_empty());
        assert_eq!(
            panes.groups[0].tabs.len(),
            2,
            "the bad addresses touched nothing"
        );

        for tab in panes.groups.iter_mut().flat_map(|g| g.tabs.iter_mut()) {
            tab.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tabs_and_groups_cycle_select_and_move() {
        let (mut panes, dir) = layout_with(1);
        let (tx, _rx) = mpsc::unbounded_channel();
        let spawn = || spawner("sleep 5", dir.path().to_path_buf(), tx.clone());
        panes.open_tab(spawn()).unwrap();
        panes.open_tab(spawn()).unwrap(); // group 0 now has three tabs
        assert_eq!(panes.groups[0].tabs.len(), 3);

        // Cycling wraps in both directions.
        assert_eq!(panes.groups[0].active, 2);
        panes.cycle_tab(1);
        assert_eq!(panes.groups[0].active, 0, "wraps forward");
        panes.cycle_tab(-1);
        assert_eq!(panes.groups[0].active, 2, "wraps back");
        assert!(panes.select_tab(1));
        assert_eq!(panes.groups[0].active, 1);
        assert!(!panes.select_tab(9), "out of range selects nothing");
        assert_eq!(panes.groups[0].active, 1);

        // A second group: focus moves between them, clamped at the ends.
        panes.split(spawn()).unwrap();
        assert_eq!(panes.focused, 1);
        panes.cycle_group(-1);
        assert_eq!(panes.focused, 0);
        panes.cycle_group(-1);
        assert_eq!(panes.focused, 0, "clamped, not wrapped");
        panes.cycle_group(5);
        assert_eq!(panes.focused, 1);

        // Moving the only tab out of a group removes that group.
        assert!(panes.move_tab_to_group(-1));
        assert_eq!(panes.group_count(), 1);
        assert_eq!(panes.focused, 0);
        assert_eq!(panes.groups[0].tabs.len(), 4);
        assert!(
            !panes.move_tab_to_group(-1),
            "one group: nothing to move to"
        );

        panes.reset_weights();
        assert_eq!(panes.weights, vec![1]);
        for tab in panes.groups.iter_mut().flat_map(|g| g.tabs.iter_mut()) {
            tab.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn events_route_by_tab_id_and_unknown_ids_are_ignored() {
        let (mut panes, _dir) = layout_with(2);
        let ids: Vec<TabId> = panes.tabs().map(TerminalTab::id).collect();
        assert_eq!(
            panes.handle_event(ids[0], TermEvent::Title("t".to_string())),
            Some(TabEffect::Redraw)
        );
        assert_eq!(panes.groups[0].tabs[0].title.as_deref(), Some("t"));
        assert!(
            panes.groups[1].tabs[0].title.is_none(),
            "the other tab is untouched"
        );
        assert_eq!(panes.handle_event(9999, TermEvent::Wakeup), None);
        for tab in panes.groups.iter_mut().flat_map(|g| g.tabs.iter_mut()) {
            tab.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn arrange_sizes_every_group_and_draw_renders_the_strip() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let (mut panes, _dir) = layout_with(2);
        let area = Rect::new(0, 0, 50, 24);
        let rects = panes.arrange(area);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].strip.height, 1);
        assert_eq!(rects[1].body.y, rects[0].body.y + rects[0].body.height + 1);
        // Every group's grid is inside its body's border.
        for r in &rects {
            assert!(r.grid.width < r.body.width);
            assert_eq!(r.grid.y, r.body.y + 1);
        }

        let mut terminal = Terminal::new(TestBackend::new(50, 24)).unwrap();
        terminal
            .draw(|frame| {
                let rects = panes.arrange(frame.area());
                panes.draw(frame, &rects, true);
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("1 shell"), "the tab strip names the tab");

        // Too short for two groups: only what fits is arranged and drawn.
        let short = panes.arrange(Rect::new(0, 0, 50, 5));
        assert_eq!(short.len(), 1);
        for tab in panes.groups.iter_mut().flat_map(|g| g.tabs.iter_mut()) {
            tab.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_layout_survives_a_save_and_restore_round_trip() {
        let (mut panes, dir) = layout_with(2);
        let here = dir.path().to_path_buf();
        // Give the first group a second tab and an uneven weight, so the
        // round trip has something to get wrong.
        let (tx, _rx) = mpsc::unbounded_channel();
        panes.focused = 0;
        panes
            .open_tab(spawner("sleep 5", here.clone(), tx.clone()))
            .unwrap();
        panes.weights = vec![3, 1];
        panes.focused = 1;

        let saved = panes.to_saved();
        assert_eq!(saved.groups.len(), 2);
        assert_eq!(saved.groups[0].tabs.len(), 2);
        assert_eq!(saved.groups[0].weight, 3);
        assert_eq!(saved.focused, 1);
        assert_eq!(saved.groups[0].tabs[0].path, here);

        for tab in panes.groups.iter_mut().flat_map(|g| g.tabs.iter_mut()) {
            tab.shutdown();
        }

        // Restoring rebuilds the same shape with fresh children.
        let mut restored = PaneLayout::default();
        let failed = restored.restore(&saved, |id, kind, path| {
            let request = super::super::terminal::pty::SpawnRequest {
                tab: id,
                program: Some((
                    "/bin/sh".to_string(),
                    vec!["-c".to_string(), "sleep 5".to_string()],
                )),
                cwd: path.to_path_buf(),
                size: GridSize { cols: 40, lines: 6 },
                extra_env: Vec::new(),
            };
            TerminalTab::from_request(kind, request, tx.clone())
        });
        assert_eq!(failed, 0);
        assert_eq!(restored.group_count(), 2);
        assert_eq!(restored.groups[0].tabs.len(), 2);
        assert_eq!(restored.weights, vec![3, 1]);
        assert_eq!(restored.focused, 1);
        assert!(restored.any_alive(), "restored tabs are live children");
        // Ids are freshly allocated, never carried over.
        assert!(restored.tabs().all(|t| t.id() > 0));
        for tab in restored.groups.iter_mut().flat_map(|g| g.tabs.iter_mut()) {
            tab.shutdown();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn restore_skips_tabs_that_will_not_spawn_and_drops_emptied_groups() {
        let dir = tempfile::tempdir().unwrap();
        let saved = session_layout::SavedLayout {
            version: 1,
            groups: vec![
                session_layout::SavedGroup {
                    tabs: vec![session_layout::SavedTab {
                        path: dir.path().to_path_buf(),
                        kind: session_layout::SavedTabKind::Shell,
                    }],
                    active: 0,
                    weight: 1,
                },
                session_layout::SavedGroup {
                    tabs: vec![session_layout::SavedTab {
                        path: dir.path().to_path_buf(),
                        kind: session_layout::SavedTabKind::Claude,
                    }],
                    active: 0,
                    weight: 1,
                },
            ],
            focused: 1,
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut panes = PaneLayout::default();
        // Fail every Claude tab, as a missing binary would.
        let failed = panes.restore(&saved, |id, kind, path| {
            if kind == TabKind::Claude {
                anyhow::bail!("no such program");
            }
            let request = super::super::terminal::pty::SpawnRequest {
                tab: id,
                program: Some((
                    "/bin/sh".to_string(),
                    vec!["-c".to_string(), "sleep 5".to_string()],
                )),
                cwd: path.to_path_buf(),
                size: GridSize { cols: 40, lines: 6 },
                extra_env: Vec::new(),
            };
            TerminalTab::from_request(kind, request, tx.clone())
        });
        assert_eq!(failed, 1, "the failure is counted, not swallowed");
        assert_eq!(panes.group_count(), 1, "its emptied group is dropped");
        assert_eq!(panes.focused, 0, "focus is clamped back into range");
        for tab in panes.groups.iter_mut().flat_map(|g| g.tabs.iter_mut()) {
            tab.shutdown();
        }
    }

    #[test]
    fn an_empty_layout_saves_as_empty() {
        let panes = PaneLayout::default();
        assert!(panes.to_saved().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn find_in_worktree_matches_kind_and_liveness() {
        let (mut panes, dir) = layout_with(1);
        let here = dir.path().to_path_buf();
        assert_eq!(
            panes.find_in_worktree(&here, TabKind::Shell),
            Some(TabAddr { group: 0, tab: 0 })
        );
        assert!(panes.find_in_worktree(&here, TabKind::Claude).is_none());
        assert!(panes
            .find_in_worktree(Path::new("/nowhere"), TabKind::Shell)
            .is_none());

        // An exited tab is not a match — the row should open a fresh one.
        panes.groups[0].tabs[0].exit_status = Some(std::process::ExitStatus::default());
        assert!(panes.find_in_worktree(&here, TabKind::Shell).is_none());
        assert!(!panes.any_alive());

        // Focusing an address out of range is a no-op, not a panic.
        panes.focus(TabAddr { group: 9, tab: 9 });
        assert_eq!(panes.focused, 0);
        panes.focus(TabAddr { group: 0, tab: 0 });
        for tab in panes.groups.iter_mut().flat_map(|g| g.tabs.iter_mut()) {
            tab.shutdown();
        }
    }
}
