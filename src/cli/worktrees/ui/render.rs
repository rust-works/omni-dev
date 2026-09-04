//! Phase 1's tree-pane renderer: a plain, read-only listing of every
//! repo/worktree from the merged [`WorktreesViewModel`]. Superseded by the
//! full glyph-table/mouse-aware tree widget in later phases (issue #1585
//! §2/§4) — this exists so `omni-dev worktrees ui` is a real, useful,
//! live-updating view from the moment the data layer lands, matching the
//! plan's "Phase 1: tree only ... supersedes `worktrees tree`" scope.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use super::tree::TreeState;
use super::view_model::{
    AheadBehindState, FeedStatus, GlyphCue, RowEmphasis, SessionBadge, SessionSourceRow, Severity,
    WorktreeRow, WorktreesViewModel,
};

pub fn draw(frame: &mut Frame<'_>, view: &WorktreesViewModel, tree: &TreeState) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    draw_tree(frame, chunks[0], view, tree);
    draw_status_bar(frame, chunks[1], view, tree);
}

fn draw_tree(frame: &mut Frame<'_>, area: Rect, view: &WorktreesViewModel, tree: &TreeState) {
    let mut items: Vec<ListItem> = Vec::new();
    if view.repos.is_empty() {
        items.push(ListItem::new("No repositories open."));
    }
    for repo in &view.repos {
        let mut header = repo.main_repo.clone();
        if let Some(gh) = &repo.github {
            header.push_str(&format!("  (github: {}/{})", gh.owner, gh.name));
        }
        if repo.polling_enabled {
            header.push_str("  [polling]");
        }
        if let Some(tag) = &repo.row_color {
            header.push_str(&format!("  ({tag})"));
        }
        header.push_str(&format!("  {}", repo.root.display()));
        let header_line = Line::from(Span::styled(
            header,
            Style::default().add_modifier(Modifier::BOLD),
        ));
        items.push(gutter_item(header_line, tree.marked.contains(&repo.root)));
        for wt in &repo.worktrees {
            items.push(gutter_item(
                worktree_line(wt),
                tree.marked.contains(&wt.path),
            ));
        }
    }
    let mut state = ListState::default();
    if !items.is_empty() {
        state.select(Some(tree.cursor.min(items.len() - 1)));
    }
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("WORKTREES"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut state);
}

/// Prepends the multi-select gutter (`▌ ` when marked, two spaces
/// otherwise — the mockup's marked-row marker, issue #1585 §2) to a row's
/// existing styled line, preserving that line's own per-span styling.
fn gutter_item(line: Line<'static>, marked: bool) -> ListItem<'static> {
    let gutter = if marked { "▌ " } else { "  " };
    let mut spans = vec![Span::raw(gutter)];
    spans.extend(line.spans);
    ListItem::new(Line::from(spans))
}

fn worktree_line(wt: &WorktreeRow) -> Line<'static> {
    let glyph = match wt.glyph_cue() {
        GlyphCue::Here => '*',
        GlyphCue::Pushing | GlyphCue::Rebasing => '~',
        GlyphCue::Operation => '!',
        GlyphCue::Open => 'o',
        GlyphCue::Closed => '.',
    };
    let main_marker = if wt.is_main { '*' } else { ' ' };
    let branch = wt
        .branch
        .clone()
        .unwrap_or_else(|| "(detached)".to_string());

    let mut fields = Vec::new();
    match wt.ahead_behind {
        AheadBehindState::Known {
            ahead,
            behind,
            main_behind,
        } => {
            let mut s = format!("+{ahead} -{behind}");
            if let Some(mb) = main_behind {
                s.push_str(&format!(" main-{mb}"));
            }
            fields.push(s);
        }
        AheadBehindState::Loading => fields.push("...".to_string()),
        AheadBehindState::Unknown | AheadBehindState::Unavailable => {}
    }
    if let Some(pr) = &wt.pr {
        let draft = if pr.is_draft { " draft" } else { "" };
        fields.push(format!("#{}{draft}", pr.number));
    }
    if !wt.sessions.is_empty() {
        fields.push(sessions_summary(&wt.sessions));
    }

    let severity_marker = match wt.badge_severity() {
        Severity::Red => " [!]",
        Severity::Yellow => " [~]",
        Severity::Green => " [ok]",
        Severity::Muted => "",
    };
    let mut text = format!(" {main_marker}{glyph} {branch}");
    if !fields.is_empty() {
        text.push_str("  ");
        text.push_str(&fields.join("  "));
    }
    text.push_str("  ");
    text.push_str(&wt.path.display().to_string());
    text.push_str(severity_marker);

    let color = match wt.emphasis() {
        RowEmphasis::Operation => Color::Yellow,
        RowEmphasis::UserTag(tag) => color_for_tag(&tag),
        // Neither an in-flight operation nor a user tag overrides the row:
        // fall through to the automatic PR-check/session severity colour
        // (`tree.ts::rowColorId`'s red > yellow > green ranking), and only
        // then to the plain "open" green / reset defaults.
        RowEmphasis::Open | RowEmphasis::Default => match wt.badge_severity() {
            Severity::Red => Color::Red,
            Severity::Yellow => Color::Yellow,
            Severity::Green => Color::Green,
            Severity::Muted if wt.open => Color::Green,
            Severity::Muted => Color::Reset,
        },
    };
    Line::from(Span::styled(text, Style::default().fg(color)))
}

/// Maps a row-colour id — one of `row_colors::KNOWN_ROW_COLORS`, or an
/// unrecognized future one the store tolerates on read — to a terminal
/// colour. An unrecognized id falls back to the terminal's default
/// foreground rather than erroring, consistent with that same tolerance.
fn color_for_tag(tag: &str) -> Color {
    match tag {
        "charts.red" | "terminal.ansiRed" => Color::Red,
        "charts.orange" => Color::Rgb(0xff, 0xa5, 0x00),
        "charts.yellow" | "terminal.ansiYellow" => Color::Yellow,
        "charts.green" | "terminal.ansiGreen" => Color::Green,
        "charts.blue" | "terminal.ansiBlue" => Color::Blue,
        "charts.purple" | "terminal.ansiMagenta" => Color::Magenta,
        "terminal.ansiCyan" => Color::Cyan,
        "charts.foreground" | "descriptionForeground" => Color::Gray,
        _ => Color::Reset,
    }
}

/// Summarizes a worktree's live Claude sessions as `"N session(s) (model,
/// source)"` off the most recently active one — a compact stand-in for the
/// full per-session badge layer a later phase adds (issue #1585 §2's
/// `[s o *]` model-family marker and `!`/`⚙`/`◦` state glyphs).
fn sessions_summary(sessions: &[SessionBadge]) -> String {
    let Some(latest) = sessions.iter().max_by_key(|s| s.last_seen) else {
        return String::new();
    };
    let source = match &latest.source {
        SessionSourceRow::Terminal => "terminal",
        SessionSourceRow::VsCode { .. } => "vscode",
    };
    let model = latest.model.as_deref().unwrap_or("?");
    format!("{} session(s) ({model}, {source})", sessions.len())
}

fn draw_status_bar(frame: &mut Frame<'_>, area: Rect, view: &WorktreesViewModel, tree: &TreeState) {
    let closed = if view.show_closed { "shown" } else { "hidden" };
    let marked = if tree.marked.is_empty() {
        String::new()
    } else {
        format!("{} marked   ", tree.marked.len())
    };
    let status = format!(
        "{marked}worktrees: {}  sessions: {}  closed worktrees {closed}   \
         space mark  a actions  c/C colour  q quit",
        feed_status_label(view.worktrees_status),
        feed_status_label(view.sessions_status),
    );
    frame.render_widget(Paragraph::new(status), area);
}

fn feed_status_label(status: FeedStatus) -> String {
    match status {
        FeedStatus::Connecting => "connecting".to_string(),
        FeedStatus::Live => "live".to_string(),
        FeedStatus::Reconnecting { attempt, retry_in } => {
            format!(
                "reconnecting (attempt {attempt}, retry in {}s)",
                retry_in.as_secs()
            )
        }
        FeedStatus::Polling => "polling (daemon predates live updates)".to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::PathBuf;

    use crate::pr_status::PrCheckState;

    use super::super::view_model::PrBadgeRow;
    use super::*;

    fn worktree_row() -> WorktreeRow {
        WorktreeRow {
            path: PathBuf::from("/repo/wt"),
            branch: None,
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
            ahead_behind: AheadBehindState::Unknown,
            sessions: Vec::new(),
            row_color: None,
            here: false,
        }
    }

    fn line_color(wt: &WorktreeRow) -> Color {
        match worktree_line(wt).spans.first() {
            Some(span) => span.style.fg.unwrap_or(Color::Reset),
            None => Color::Reset,
        }
    }

    #[test]
    fn color_for_tag_maps_every_known_row_color() {
        for tag in super::super::row_colors::KNOWN_ROW_COLORS {
            // Must not silently fall back to Reset for a colour the store
            // actually accepts on write.
            assert_ne!(color_for_tag(tag), Color::Reset, "tag: {tag}");
        }
    }

    #[test]
    fn color_for_tag_falls_back_to_reset_for_an_unrecognized_id() {
        assert_eq!(color_for_tag("not-a-real-color"), Color::Reset);
    }

    #[test]
    fn a_user_row_tag_is_rendered_in_its_mapped_color() {
        let mut wt = worktree_row();
        wt.row_color = Some("charts.blue".to_string());
        assert_eq!(line_color(&wt), Color::Blue);
    }

    #[test]
    fn failing_pr_checks_color_the_row_red_even_with_no_operation_or_tag() {
        let mut wt = worktree_row();
        wt.pr = Some(PrBadgeRow {
            number: 1,
            is_draft: false,
            checks: PrCheckState::Failure,
            url: String::new(),
        });
        assert_eq!(line_color(&wt), Color::Red);
    }

    #[test]
    fn an_in_flight_operation_still_wins_over_severity() {
        let mut wt = worktree_row();
        wt.rebasing = true;
        wt.pr = Some(PrBadgeRow {
            number: 1,
            is_draft: false,
            checks: PrCheckState::Failure,
            url: String::new(),
        });
        assert_eq!(line_color(&wt), Color::Yellow);
    }

    #[test]
    fn open_with_no_severity_or_tag_stays_green() {
        let mut wt = worktree_row();
        wt.open = true;
        assert_eq!(line_color(&wt), Color::Green);
    }

    #[test]
    fn default_row_with_no_signal_is_reset() {
        assert_eq!(line_color(&worktree_row()), Color::Reset);
    }
}
