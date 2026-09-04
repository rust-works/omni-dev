//! Phase 1's tree-pane renderer: a plain, read-only listing of every
//! repo/worktree from the merged [`WorktreesViewModel`]. Superseded by the
//! full glyph-table/mouse-aware tree widget in later phases (issue #1585
//! §2/§4) — this exists so `omni-dev worktrees ui` is a real, useful,
//! live-updating view from the moment the data layer lands, matching the
//! plan's "Phase 1: tree only ... supersedes `worktrees tree`" scope.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};
use ratatui::Frame;

use super::view_model::{
    AheadBehindState, FeedStatus, GlyphCue, RowEmphasis, SessionBadge, SessionSourceRow, Severity,
    WorktreeRow, WorktreesViewModel,
};

pub fn draw(frame: &mut Frame<'_>, view: &WorktreesViewModel) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);
    draw_tree(frame, chunks[0], view);
    draw_status_bar(frame, chunks[1], view);
}

fn draw_tree(frame: &mut Frame<'_>, area: Rect, view: &WorktreesViewModel) {
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
        items.push(ListItem::new(Line::from(Span::styled(
            header,
            Style::default().add_modifier(Modifier::BOLD),
        ))));
        for wt in &repo.worktrees {
            items.push(ListItem::new(worktree_line(wt)));
        }
    }
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title("WORKTREES"));
    frame.render_widget(list, area);
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
        RowEmphasis::Open => Color::Green,
        RowEmphasis::UserTag(_) | RowEmphasis::Default => Color::Reset,
    };
    Line::from(Span::styled(text, Style::default().fg(color)))
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

fn draw_status_bar(frame: &mut Frame<'_>, area: Rect, view: &WorktreesViewModel) {
    let closed = if view.show_closed { "shown" } else { "hidden" };
    let status = format!(
        "worktrees: {}  sessions: {}  closed worktrees {closed}   q quit",
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
