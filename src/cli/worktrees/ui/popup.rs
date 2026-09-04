//! The action-menu and confirm-modal popups (issue #1585 Phase 2) — the
//! minimum slice of the plan's eventual `popup.rs` pulled forward from
//! Phase 4. `CommandPalette`/`FindInScrollback` stay Phase 4; they need
//! terminal panes this phase doesn't have.

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use super::actions::{ActionKind, ConfirmPrompt};

/// One entry in the action menu — the action it dispatches, and its
/// display label. Grouping (the VS Code extension's own `0_open`/`1_pr`/
/// `2_claude`/`3_copy`/`9_close` order) is expressed purely by the caller's
/// item order; there is no separate group-header row in v1.
pub struct MenuItem {
    pub action: ActionKind,
    pub label: &'static str,
}

/// The action menu popup (`a` key): a filtered, ordered list of applicable
/// actions for the current selection.
pub struct ActionMenu {
    pub items: Vec<MenuItem>,
    pub selected: usize,
}

impl ActionMenu {
    pub fn new(items: Vec<MenuItem>) -> Self {
        Self { items, selected: 0 }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.items.is_empty() {
            return;
        }
        let max = self.items.len() - 1;
        let current = self.selected.min(max) as isize;
        self.selected = (current + delta).clamp(0, max as isize) as usize;
    }

    pub fn selected_action(&self) -> Option<ActionKind> {
        self.items.get(self.selected).map(|item| item.action)
    }
}

/// The confirm modal — renders a [`ConfirmPrompt`] verbatim (body, then
/// risks, then info), with the yes/no footer hint.
pub struct ConfirmModal {
    pub prompt: ConfirmPrompt,
}

/// Centers a `width`x`height` rect inside `area`, clamped to fit.
fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect {
        x,
        y,
        width,
        height,
    }
}

pub fn draw_action_menu(frame: &mut Frame<'_>, area: Rect, menu: &ActionMenu) {
    let labels: Vec<&str> = menu.items.iter().map(|item| item.label).collect();
    draw_list_popup(frame, area, "Actions", &labels, menu.selected);
}

/// A generic "pick one of these labelled items" popup — backs the action
/// menu above and, in `mod.rs`, the Move/Copy-Claude-Session-Here flow's
/// session and destination pickers (issue #1585 Phase 2, §5). Kept generic
/// over `&str` labels rather than tied to [`ActionKind`] since those two
/// pickers select a session id / a worktree path, not an action.
pub fn draw_list_popup(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    items: &[&str],
    selected: usize,
) {
    let popup_height = (items.len() as u16 + 2).min(area.height);
    let popup_area = centered_rect(50, popup_height, area);
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
    let popup_area = centered_rect(70, (content_lines as u16).min(area.height), area);
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

    #[test]
    fn action_menu_move_selection_clamps_at_both_ends() {
        let mut menu = ActionMenu::new(vec![
            MenuItem {
                action: ActionKind::Focus,
                label: "Open Worktree",
            },
            MenuItem {
                action: ActionKind::CopyDirectory,
                label: "Copy Directory",
            },
        ]);
        menu.move_selection(-5);
        assert_eq!(menu.selected, 0);
        menu.move_selection(5);
        assert_eq!(menu.selected, 1);
        assert_eq!(menu.selected_action(), Some(ActionKind::CopyDirectory));
    }

    #[test]
    fn action_menu_selected_action_is_none_when_empty() {
        let menu = ActionMenu::new(Vec::new());
        assert_eq!(menu.selected_action(), None);
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
    fn draw_action_menu_renders_every_item_label() {
        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        let menu = ActionMenu::new(vec![
            MenuItem {
                action: ActionKind::Focus,
                label: "Open Worktree",
            },
            MenuItem {
                action: ActionKind::CloseWorktree,
                label: "Close Worktree",
            },
        ]);
        terminal
            .draw(|frame| draw_action_menu(frame, frame.area(), &menu))
            .unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Open Worktree"));
        assert!(text.contains("Close Worktree"));
        assert!(text.contains("Actions"));
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
