//! Output rendering for the Confluence `compare` and `compare_section` tools.
//!
//! Turns a [`Diff`] (from [`super::diff`]) plus surrounding metadata into the
//! YAML output schema described in issue #706. Three detail levels:
//!
//! - **Summary** — counts only.
//! - **Outline** — per-section change kind + one-line summaries + cursors.
//! - **Full** — embeds per-section deltas, budget-truncated with continuation.
//!
//! Cursors are stateless: an opaque base64url-encoded JSON record carrying
//! `{page_id, from_v, to_v, section_path}` so `confluence_compare_section`
//! can re-fetch both sides without server state.

use anyhow::{Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};

use crate::atlassian::diff::{
    ChangeKind, Diff, DiffStats, NodeDelta, NodeSnapshot, ParagraphDelta, SectionDiff,
};

/// Detail level for the compare output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Detail {
    /// Counts only — no per-section information.
    Summary,
    /// Per-section change kind, one-line summaries, drill-in cursors.
    #[default]
    Outline,
    /// Embed per-section deltas. Budget-truncated.
    Full,
}

/// Which top-level fields to include in the output.
#[derive(Debug, Clone, Copy)]
pub struct Includes {
    /// Whether to include body diffs (sections, summary).
    pub body: bool,
    /// Whether to include the title-change record.
    pub title: bool,
    /// Whether to include the labels add/remove record.
    pub labels: bool,
    /// Whether to include the metadata (versions header) — almost always
    /// `true`; here for parity with the issue's `include` parameter.
    pub metadata: bool,
}

impl Default for Includes {
    fn default() -> Self {
        Self {
            body: true,
            title: true,
            labels: false,
            metadata: true,
        }
    }
}

/// Filter applied to the rendered output (post-diff).
#[derive(Debug, Clone, Default)]
pub struct Filter {
    /// Restrict to sections whose path matches one of the given strings.
    /// Empty = no path filter.
    pub sections: Vec<String>,
    /// Drop section deltas whose total `from + to` text is shorter than
    /// `min_change_chars`. `0` = no filter.
    pub min_change_chars: u32,
    /// Restrict to sections classified as one of the listed kinds. Empty
    /// = no filter.
    pub kinds: Vec<ChangeKind>,
}

// ── Output schema ────────────────────────────────────────────────────

/// Top-level YAML output for `confluence_compare`.
#[derive(Debug, Clone, Serialize)]
pub struct CompareOutput {
    /// Page identity header.
    pub page: PageHeader,
    /// Version pair header.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub versions: Option<VersionPair>,
    /// Aggregate counts.
    pub summary: SummaryBlock,
    /// Title-change record (None when titles are identical or when `title`
    /// is excluded from `include`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title_change: Option<TitleChange>,
    /// Label changes (None when `labels` is excluded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub labels: Option<LabelChange>,
    /// Section-level diffs (omitted when `detail` = `summary`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sections: Vec<SectionRecord>,
    /// Whether output was truncated by the budget.
    pub truncated: bool,
    /// Continuation cursor when `truncated` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation: Option<Continuation>,
}

/// Page identity header for the compare output.
#[derive(Debug, Clone, Serialize)]
pub struct PageHeader {
    /// Page ID.
    pub id: String,
    /// Page title (the `to` version's title).
    pub title: String,
    /// Optional rendered URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Pair of version metadata records (`from` / `to`).
#[derive(Debug, Clone, Serialize)]
pub struct VersionPair {
    /// `from` side of the diff.
    pub from: VersionInfo,
    /// `to` side of the diff.
    pub to: VersionInfo,
}

/// Per-version metadata.
#[derive(Debug, Clone, Serialize, Default)]
pub struct VersionInfo {
    /// 1-based version number.
    pub number: u32,
    /// ISO 8601 creation timestamp.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub created_at: String,
    /// Author display name or account id.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub author: String,
    /// Version comment.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
}

/// Aggregate counts across all sections.
#[derive(Debug, Clone, Serialize)]
pub struct SummaryBlock {
    /// Sum of all change-kind counters in [`ByKind`].
    pub total_changes: u32,
    /// Counts grouped by change kind.
    pub by_kind: ByKind,
    /// Net character / word changes.
    pub net: NetCounts,
}

/// Counts grouped by change kind.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ByKind {
    /// Sections present only in `to`.
    pub sections_added: u32,
    /// Sections present only in `from`.
    pub sections_removed: u32,
    /// Sections present on both sides with content edits.
    pub sections_modified: u32,
    /// Sections present on both sides at different positions.
    pub sections_moved: u32,
    /// Number of paragraph-shaped block edits.
    pub paragraphs_modified: u32,
    /// Number of tables with at least one modified cell.
    pub tables_modified: u32,
}

/// Net character and word change counts across the entire diff.
#[derive(Debug, Clone, Default, Serialize)]
pub struct NetCounts {
    /// Total characters added across all prose deltas.
    pub chars_added: u32,
    /// Total characters removed across all prose deltas.
    pub chars_removed: u32,
    /// Total words added across all prose deltas.
    pub words_added: u32,
    /// Total words removed across all prose deltas.
    pub words_removed: u32,
}

/// Title-change record. Emitted only when titles differ.
#[derive(Debug, Clone, Serialize)]
pub struct TitleChange {
    /// Title before.
    pub from: String,
    /// Title after.
    pub to: String,
}

/// Label changes between two versions.
#[derive(Debug, Clone, Default, Serialize)]
pub struct LabelChange {
    /// Labels in `to` but not `from`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<String>,
    /// Labels in `from` but not `to`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
}

/// One section's record in the output.
#[derive(Debug, Clone, Serialize)]
pub struct SectionRecord {
    /// Heading text (empty for the document preamble).
    pub heading: String,
    /// Heading-anchor path (e.g. `/h2#background`).
    pub path: String,
    /// Coarse change classification.
    pub change: ChangeKind,
    /// One-line human-readable summary.
    pub summary: String,
    /// Opaque drill-in cursor for `confluence_compare_section`.
    pub cursor: String,
    /// Per-block deltas (only emitted in `Full` detail).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub diff: Vec<NodeDelta>,
}

/// Continuation pointer when output was truncated.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Continuation {
    /// Cursor to pass to a follow-up call. None when there's no more data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

// ── Cursor encoding ──────────────────────────────────────────────────

/// Opaque cursor payload carried in `cursor` and `continuation.next_cursor`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    /// Confluence page ID.
    pub page_id: String,
    /// `from` version number.
    pub from_v: u32,
    /// `to` version number.
    pub to_v: u32,
    /// Section path (e.g. `/h2#background`).
    pub section_path: String,
}

impl Cursor {
    /// Encodes the cursor as a base64url string.
    pub fn encode(&self) -> Result<String> {
        let json = serde_json::to_vec(self).context("Failed to serialize cursor")?;
        Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json))
    }

    /// Decodes a base64url cursor string.
    pub fn decode(s: &str) -> Result<Self> {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s)
            .context("Cursor is not valid base64url")?;
        let cur: Self = serde_json::from_slice(&bytes).context("Cursor JSON is malformed")?;
        Ok(cur)
    }
}

// ── Render input ─────────────────────────────────────────────────────

/// Side-channel data the renderer needs beyond the [`Diff`] itself.
#[derive(Debug, Clone, Default)]
pub struct CompareContext {
    /// Confluence page ID.
    pub page_id: String,
    /// Page title (the `to` side's title).
    pub page_title: String,
    /// Optional rendered page URL.
    pub page_url: Option<String>,
    /// `from`-side version metadata.
    pub from_version: VersionInfo,
    /// `to`-side version metadata.
    pub to_version: VersionInfo,
    /// `from`-side page title (used for title-change detection).
    pub from_title: String,
    /// `to`-side page title.
    pub to_title: String,
    /// `from`-side labels.
    pub from_labels: Vec<String>,
    /// `to`-side labels.
    pub to_labels: Vec<String>,
}

/// Approximate output budget, in bytes of YAML. The default of ~16 KiB
/// corresponds to roughly 4000 tokens at 4 chars/token.
pub const DEFAULT_OUTPUT_BUDGET: usize = 16 * 1024;

// ── Render entry points ──────────────────────────────────────────────

/// Renders a [`Diff`] into a [`CompareOutput`] at the given detail level.
///
/// The renderer is "render-then-trim": it builds the full output, then
/// drops trailing sections (and sets `truncated = true`) until the
/// serialized YAML fits within `budget_bytes`. Section ordering is
/// preserved, so sections at the head of the list are kept and the
/// tail is shed first.
pub fn render(
    mut diff: Diff,
    ctx: &CompareContext,
    detail: Detail,
    include: Includes,
    filter: &Filter,
    budget_bytes: usize,
) -> Result<CompareOutput> {
    apply_filter(&mut diff, filter);

    let mut sections = if matches!(detail, Detail::Summary) {
        Vec::new()
    } else {
        build_section_records(&diff, ctx, detail)?
    };

    let mut truncated = false;
    let mut continuation: Option<Continuation> = None;

    if !sections.is_empty() {
        let (kept, drop_first_idx) = trim_to_budget(&diff, ctx, &sections, include, budget_bytes)?;
        if kept < sections.len() {
            truncated = true;
            let next_cursor = sections.get(drop_first_idx).map(|s| s.cursor.clone());
            continuation = Some(Continuation { next_cursor });
            sections.truncate(kept);
        }
    }

    Ok(build_compare_output(
        &diff,
        ctx,
        sections,
        include,
        truncated,
        continuation,
    ))
}

/// Renders a single section diff in the requested format. Used by
/// `confluence_compare_section`.
pub fn render_section(diff: &Diff, cursor: &Cursor, format: SectionFormat) -> Result<String> {
    let section = diff
        .sections
        .iter()
        .find(|s| s.path == cursor.section_path)
        .with_context(|| {
            format!(
                "Section not found for cursor path \"{}\"",
                cursor.section_path
            )
        })?;
    Ok(match format {
        SectionFormat::Unified => render_section_unified(section),
        SectionFormat::SideBySide => render_section_side_by_side(section),
        SectionFormat::MarkdownInline => render_section_markdown_inline(section),
    })
}

/// Output format for `render_section`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SectionFormat {
    /// Unified diff (`+`/`-` line markers).
    #[default]
    Unified,
    /// Side-by-side: `from` on the left, `to` on the right.
    SideBySide,
    /// Markdown with inline `+added+`/`-removed-` markers.
    MarkdownInline,
}

// ── Filter ───────────────────────────────────────────────────────────

/// Applies a [`Filter`] in place. Sections that fail the predicate are
/// removed from the diff.
pub fn apply_filter(diff: &mut Diff, filter: &Filter) {
    let path_filter = !filter.sections.is_empty();
    let kind_filter = !filter.kinds.is_empty();
    let min_chars = filter.min_change_chars;

    diff.sections.retain(|s| {
        if path_filter && !filter.sections.iter().any(|p| p == &s.path) {
            return false;
        }
        if kind_filter && !filter.kinds.contains(&s.change) {
            return false;
        }
        if min_chars > 0 && section_change_chars(s) < min_chars {
            return false;
        }
        true
    });

    // Recompute aggregate stats over the surviving sections.
    diff.stats = aggregate_stats(&diff.sections);
}

fn section_change_chars(s: &SectionDiff) -> u32 {
    let mut total: u32 = 0;
    for delta in &s.deltas {
        match delta {
            NodeDelta::Paragraph(p) => {
                total += p.from_text.chars().count() as u32;
                total += p.to_text.chars().count() as u32;
            }
            NodeDelta::CodeBlock(c) => {
                total += c.from_text.chars().count() as u32;
                total += c.to_text.chars().count() as u32;
            }
            NodeDelta::Added(n) | NodeDelta::Removed(n) => {
                total += n.text.chars().count() as u32;
            }
            NodeDelta::Table(t) => {
                for cell in &t.cells {
                    total += cell.from_text.chars().count() as u32;
                    total += cell.to_text.chars().count() as u32;
                }
            }
            NodeDelta::List(l) => {
                for s in &l.items_added {
                    total += s.chars().count() as u32;
                }
                for s in &l.items_removed {
                    total += s.chars().count() as u32;
                }
                for (a, b) in &l.items_modified {
                    total += a.chars().count() as u32;
                    total += b.chars().count() as u32;
                }
            }
            NodeDelta::Opaque(o) => {
                total += o.from_summary.chars().count() as u32;
                total += o.to_summary.chars().count() as u32;
            }
        }
    }
    total
}

fn aggregate_stats(sections: &[SectionDiff]) -> DiffStats {
    let mut stats = DiffStats::default();
    for s in sections {
        match s.change {
            ChangeKind::Added => stats.sections_added += 1,
            ChangeKind::Removed => stats.sections_removed += 1,
            ChangeKind::Modified => stats.sections_modified += 1,
            ChangeKind::Moved => stats.sections_moved += 1,
            ChangeKind::Unchanged => {}
        }
        for delta in &s.deltas {
            accumulate_stats(&mut stats, delta);
        }
    }
    stats
}

fn accumulate_stats(stats: &mut DiffStats, delta: &NodeDelta) {
    match delta {
        NodeDelta::Paragraph(p) => {
            stats.paragraphs_modified += 1;
            stats.words_added += p.words_added;
            stats.words_removed += p.words_removed;
        }
        NodeDelta::Table(_) => stats.tables_modified += 1,
        NodeDelta::Added(s) => {
            stats.chars_added += s.text.chars().count() as u32;
            stats.words_added += s.text.split_whitespace().count() as u32;
        }
        NodeDelta::Removed(s) => {
            stats.chars_removed += s.text.chars().count() as u32;
            stats.words_removed += s.text.split_whitespace().count() as u32;
        }
        NodeDelta::CodeBlock(_) | NodeDelta::List(_) | NodeDelta::Opaque(_) => {}
    }
}

// ── Section record assembly ──────────────────────────────────────────

fn build_section_records(
    diff: &Diff,
    ctx: &CompareContext,
    detail: Detail,
) -> Result<Vec<SectionRecord>> {
    let mut records = Vec::with_capacity(diff.sections.len());
    for section in &diff.sections {
        let cursor = Cursor {
            page_id: ctx.page_id.clone(),
            from_v: ctx.from_version.number,
            to_v: ctx.to_version.number,
            section_path: section.path.clone(),
        }
        .encode()?;
        let summary = summarize_section(section);
        let diff_payload = if matches!(detail, Detail::Full) {
            section.deltas.clone()
        } else {
            Vec::new()
        };
        records.push(SectionRecord {
            heading: section.heading.clone(),
            path: section.path.clone(),
            change: section.change,
            summary,
            cursor,
            diff: diff_payload,
        });
    }
    Ok(records)
}

fn summarize_section(section: &SectionDiff) -> String {
    if section.deltas.is_empty() {
        return change_label(section.change).to_string();
    }
    let parts: Vec<String> = section.deltas.iter().take(3).map(summarize_delta).collect();
    let mut text = parts.join("; ");
    if section.deltas.len() > 3 {
        text.push_str(&format!(" (+{} more)", section.deltas.len() - 3));
    }
    text
}

fn summarize_delta(delta: &NodeDelta) -> String {
    match delta {
        NodeDelta::Paragraph(p) => summarize_paragraph(p),
        NodeDelta::CodeBlock(_) => "code edit".to_string(),
        NodeDelta::Table(t) => format!("table: {} cell(s) changed", t.cells.len()),
        NodeDelta::List(l) => {
            let mut parts: Vec<String> = Vec::new();
            if !l.items_added.is_empty() {
                parts.push(format!("+{} item(s)", l.items_added.len()));
            }
            if !l.items_removed.is_empty() {
                parts.push(format!("-{} item(s)", l.items_removed.len()));
            }
            if !l.items_modified.is_empty() {
                parts.push(format!("~{} item(s)", l.items_modified.len()));
            }
            if parts.is_empty() {
                "list edit".to_string()
            } else {
                format!("list: {}", parts.join(", "))
            }
        }
        NodeDelta::Added(s) => format!("+{}", truncate(&s.text, 60)),
        NodeDelta::Removed(s) => format!("-{}", truncate(&s.text, 60)),
        NodeDelta::Opaque(o) => format!("{} changed", o.node_type),
    }
}

fn summarize_paragraph(p: &ParagraphDelta) -> String {
    let from = truncate(&p.from_text, 30);
    let to = truncate(&p.to_text, 30);
    format!("\"{from}\" → \"{to}\"")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

fn change_label(change: ChangeKind) -> &'static str {
    match change {
        ChangeKind::Added => "added",
        ChangeKind::Removed => "removed",
        ChangeKind::Modified => "modified",
        ChangeKind::Moved => "moved",
        ChangeKind::Unchanged => "unchanged",
    }
}

// ── Output assembly ──────────────────────────────────────────────────

fn build_compare_output(
    diff: &Diff,
    ctx: &CompareContext,
    sections: Vec<SectionRecord>,
    include: Includes,
    truncated: bool,
    continuation: Option<Continuation>,
) -> CompareOutput {
    let title_change = if include.title && ctx.from_title != ctx.to_title {
        Some(TitleChange {
            from: ctx.from_title.clone(),
            to: ctx.to_title.clone(),
        })
    } else {
        None
    };
    let labels = if include.labels {
        Some(diff_labels(&ctx.from_labels, &ctx.to_labels))
    } else {
        None
    };
    let versions = if include.metadata {
        Some(VersionPair {
            from: ctx.from_version.clone(),
            to: ctx.to_version.clone(),
        })
    } else {
        None
    };
    let stats = &diff.stats;
    let summary = SummaryBlock {
        total_changes: stats.sections_added
            + stats.sections_removed
            + stats.sections_modified
            + stats.sections_moved
            + stats.paragraphs_modified
            + stats.tables_modified,
        by_kind: ByKind {
            sections_added: stats.sections_added,
            sections_removed: stats.sections_removed,
            sections_modified: stats.sections_modified,
            sections_moved: stats.sections_moved,
            paragraphs_modified: stats.paragraphs_modified,
            tables_modified: stats.tables_modified,
        },
        net: NetCounts {
            chars_added: stats.chars_added,
            chars_removed: stats.chars_removed,
            words_added: stats.words_added,
            words_removed: stats.words_removed,
        },
    };

    CompareOutput {
        page: PageHeader {
            id: ctx.page_id.clone(),
            title: ctx.page_title.clone(),
            url: ctx.page_url.clone(),
        },
        versions,
        summary,
        title_change,
        labels,
        sections: if include.body { sections } else { Vec::new() },
        truncated,
        continuation,
    }
}

fn diff_labels(from: &[String], to: &[String]) -> LabelChange {
    let from_set: std::collections::BTreeSet<&String> = from.iter().collect();
    let to_set: std::collections::BTreeSet<&String> = to.iter().collect();
    LabelChange {
        added: to_set.difference(&from_set).map(|s| (*s).clone()).collect(),
        removed: from_set.difference(&to_set).map(|s| (*s).clone()).collect(),
    }
}

// ── Budget trimming ─────────────────────────────────────────────────

fn trim_to_budget(
    diff: &Diff,
    ctx: &CompareContext,
    sections: &[SectionRecord],
    include: Includes,
    budget_bytes: usize,
) -> Result<(usize, usize)> {
    // Render with all sections; if it fits, no truncation.
    let full = build_compare_output(diff, ctx, sections.to_vec(), include, false, None);
    let yaml = crate::data::yaml::to_yaml(&full)?;
    if yaml.len() <= budget_bytes {
        return Ok((sections.len(), sections.len()));
    }

    // Otherwise, find the largest prefix that fits via binary search.
    let (mut lo, mut hi) = (0usize, sections.len());
    let mut best = 0usize;
    while lo <= hi {
        let mid = (lo + hi) / 2;
        let trial = build_compare_output(
            diff,
            ctx,
            sections.iter().take(mid).cloned().collect(),
            include,
            true,
            Some(Continuation {
                next_cursor: sections.get(mid).map(|s| s.cursor.clone()),
            }),
        );
        let yaml = crate::data::yaml::to_yaml(&trial)?;
        if yaml.len() <= budget_bytes {
            best = mid;
            lo = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    Ok((best, best))
}

// ── Section formatters (unified / side-by-side / markdown_inline) ────

fn render_section_unified(section: &SectionDiff) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "@@ {} ({}) @@\n",
        section.path,
        change_label(section.change)
    ));
    for delta in &section.deltas {
        push_unified_delta(delta, &mut out);
    }
    out
}

fn push_unified_delta(delta: &NodeDelta, out: &mut String) {
    match delta {
        NodeDelta::Paragraph(p) => {
            for line in p.from_text.lines() {
                out.push_str(&format!("- {line}\n"));
            }
            for line in p.to_text.lines() {
                out.push_str(&format!("+ {line}\n"));
            }
        }
        NodeDelta::CodeBlock(c) => {
            out.push_str(&format!(
                "  ```{}\n",
                c.language.as_deref().unwrap_or_default()
            ));
            push_text_diff(&c.from_text, &c.to_text, out);
            out.push_str("  ```\n");
        }
        NodeDelta::Table(t) => {
            for cell in &t.cells {
                out.push_str(&format!(
                    "  table[{}][{}]:\n- {}\n+ {}\n",
                    cell.row, cell.col, cell.from_text, cell.to_text
                ));
            }
        }
        NodeDelta::List(l) => {
            for s in &l.items_removed {
                out.push_str(&format!("- {s}\n"));
            }
            for s in &l.items_added {
                out.push_str(&format!("+ {s}\n"));
            }
            for (a, b) in &l.items_modified {
                out.push_str(&format!("- {a}\n+ {b}\n"));
            }
        }
        NodeDelta::Added(NodeSnapshot { text, .. }) => {
            for line in text.lines() {
                out.push_str(&format!("+ {line}\n"));
            }
        }
        NodeDelta::Removed(NodeSnapshot { text, .. }) => {
            for line in text.lines() {
                out.push_str(&format!("- {line}\n"));
            }
        }
        NodeDelta::Opaque(o) => {
            out.push_str(&format!("  ({} changed)\n", o.node_type));
            for line in o.from_summary.lines() {
                out.push_str(&format!("- {line}\n"));
            }
            for line in o.to_summary.lines() {
                out.push_str(&format!("+ {line}\n"));
            }
        }
    }
}

fn push_text_diff(from: &str, to: &str, out: &mut String) {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(from, to);
    for change in diff.iter_all_changes() {
        let prefix = match change.tag() {
            ChangeTag::Insert => '+',
            ChangeTag::Delete => '-',
            ChangeTag::Equal => ' ',
        };
        let text = change.value();
        out.push_str(&format!("{prefix} {text}"));
        if !text.ends_with('\n') {
            out.push('\n');
        }
    }
}

fn render_section_side_by_side(section: &SectionDiff) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "── {} ({}) ──\n",
        section.path,
        change_label(section.change)
    ));
    let width: usize = 40;
    for delta in &section.deltas {
        match delta {
            NodeDelta::Paragraph(p) => {
                push_columns(&p.from_text, &p.to_text, width, &mut out);
            }
            NodeDelta::CodeBlock(c) => {
                push_columns(&c.from_text, &c.to_text, width, &mut out);
            }
            NodeDelta::Table(t) => {
                for cell in &t.cells {
                    push_columns(
                        &format!("[{},{}] {}", cell.row, cell.col, cell.from_text),
                        &format!("[{},{}] {}", cell.row, cell.col, cell.to_text),
                        width,
                        &mut out,
                    );
                }
            }
            NodeDelta::List(l) => {
                for s in &l.items_removed {
                    push_columns(s, "", width, &mut out);
                }
                for s in &l.items_added {
                    push_columns("", s, width, &mut out);
                }
                for (a, b) in &l.items_modified {
                    push_columns(a, b, width, &mut out);
                }
            }
            NodeDelta::Added(s) => push_columns("", &s.text, width, &mut out),
            NodeDelta::Removed(s) => push_columns(&s.text, "", width, &mut out),
            NodeDelta::Opaque(o) => push_columns(&o.from_summary, &o.to_summary, width, &mut out),
        }
    }
    out
}

fn push_columns(left: &str, right: &str, width: usize, out: &mut String) {
    out.push_str(&format!(
        "{:<width$} | {}\n",
        truncate(left, width),
        truncate(right, width)
    ));
}

fn render_section_markdown_inline(section: &SectionDiff) -> String {
    use similar::{ChangeTag, TextDiff};
    let mut out = String::new();
    out.push_str(&format!(
        "### {} ({})\n\n",
        if section.heading.is_empty() {
            "Preamble"
        } else {
            section.heading.as_str()
        },
        change_label(section.change)
    ));
    for delta in &section.deltas {
        match delta {
            NodeDelta::Paragraph(p) => {
                let diff = TextDiff::from_words(&p.from_text, &p.to_text);
                for change in diff.iter_all_changes() {
                    let val = change.value();
                    match change.tag() {
                        ChangeTag::Insert => out.push_str(&format!("**+{val}**")),
                        ChangeTag::Delete => out.push_str(&format!("~~-{val}~~")),
                        ChangeTag::Equal => out.push_str(val),
                    }
                }
                out.push_str("\n\n");
            }
            NodeDelta::CodeBlock(c) => {
                out.push_str(&format!(
                    "```{}\n",
                    c.language.as_deref().unwrap_or_default()
                ));
                push_text_diff(&c.from_text, &c.to_text, &mut out);
                out.push_str("```\n\n");
            }
            NodeDelta::Table(t) => {
                for cell in &t.cells {
                    out.push_str(&format!(
                        "- table[{},{}]: ~~{}~~ → **{}**\n",
                        cell.row, cell.col, cell.from_text, cell.to_text
                    ));
                }
                out.push('\n');
            }
            NodeDelta::List(l) => {
                for s in &l.items_added {
                    out.push_str(&format!("- **+{s}**\n"));
                }
                for s in &l.items_removed {
                    out.push_str(&format!("- ~~{s}~~\n"));
                }
                for (a, b) in &l.items_modified {
                    out.push_str(&format!("- ~~{a}~~ → **{b}**\n"));
                }
                out.push('\n');
            }
            NodeDelta::Added(s) => {
                out.push_str(&format!("**+ {}**\n\n", s.text));
            }
            NodeDelta::Removed(s) => {
                out.push_str(&format!("~~- {}~~\n\n", s.text));
            }
            NodeDelta::Opaque(o) => {
                out.push_str(&format!(
                    "**{} changed:** ~~{}~~ → **{}**\n\n",
                    o.node_type, o.from_summary, o.to_summary
                ));
            }
        }
    }
    out
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::adf::{AdfDocument, AdfNode};
    use crate::atlassian::diff::{
        diff_documents, CellDelta, CodeBlockDelta, DiffOptions, ListDelta, OpaqueDelta,
        ParagraphDelta, TableDelta,
    };

    fn doc(content: Vec<AdfNode>) -> AdfDocument {
        AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content,
        }
    }
    fn p(text: &str) -> AdfNode {
        AdfNode::paragraph(vec![AdfNode::text(text)])
    }
    fn h(level: u8, text: &str) -> AdfNode {
        AdfNode::heading(level, vec![AdfNode::text(text)])
    }

    fn ctx() -> CompareContext {
        CompareContext {
            page_id: "12345".to_string(),
            page_title: "Spec".to_string(),
            page_url: Some("https://example.atlassian.net/wiki/...".to_string()),
            from_version: VersionInfo {
                number: 4,
                created_at: "2026-05-08T10:00:00Z".to_string(),
                author: "alice".to_string(),
                message: String::new(),
            },
            to_version: VersionInfo {
                number: 5,
                created_at: "2026-05-09T10:00:00Z".to_string(),
                author: "bob".to_string(),
                message: "rev".to_string(),
            },
            from_title: "Spec v0.9".to_string(),
            to_title: "Spec v1.0".to_string(),
            from_labels: vec!["draft".to_string(), "wip".to_string()],
            to_labels: vec!["draft".to_string(), "approved".to_string()],
        }
    }

    #[test]
    fn cursor_round_trip() {
        let cur = Cursor {
            page_id: "12345".to_string(),
            from_v: 4,
            to_v: 5,
            section_path: "/h2#background".to_string(),
        };
        let s = cur.encode().unwrap();
        let back = Cursor::decode(&s).unwrap();
        assert_eq!(back.page_id, cur.page_id);
        assert_eq!(back.from_v, cur.from_v);
        assert_eq!(back.to_v, cur.to_v);
        assert_eq!(back.section_path, cur.section_path);
    }

    #[test]
    fn cursor_decode_rejects_garbage() {
        assert!(Cursor::decode("!!!").is_err());
        assert!(Cursor::decode("not-json").is_err());
    }

    #[test]
    fn render_summary_omits_sections() {
        let from = doc(vec![h(2, "A"), p("a")]);
        let to = doc(vec![h(2, "A"), p("a edited")]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let out = render(
            diff,
            &ctx(),
            Detail::Summary,
            Includes::default(),
            &Filter::default(),
            DEFAULT_OUTPUT_BUDGET,
        )
        .unwrap();
        assert!(out.sections.is_empty());
        assert_eq!(out.summary.by_kind.sections_modified, 1);
    }

    #[test]
    fn render_outline_includes_sections_without_diff_payload() {
        let from = doc(vec![h(2, "A"), p("a")]);
        let to = doc(vec![h(2, "A"), p("a edited"), h(2, "B"), p("b")]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let out = render(
            diff,
            &ctx(),
            Detail::Outline,
            Includes::default(),
            &Filter::default(),
            DEFAULT_OUTPUT_BUDGET,
        )
        .unwrap();
        assert_eq!(out.sections.len(), 2);
        for section in &out.sections {
            assert!(section.diff.is_empty());
            assert!(!section.cursor.is_empty());
        }
    }

    #[test]
    fn render_full_includes_per_section_deltas() {
        let from = doc(vec![h(2, "A"), p("alpha")]);
        let to = doc(vec![h(2, "A"), p("alpha edited")]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let out = render(
            diff,
            &ctx(),
            Detail::Full,
            Includes::default(),
            &Filter::default(),
            DEFAULT_OUTPUT_BUDGET,
        )
        .unwrap();
        assert!(!out.sections[0].diff.is_empty());
    }

    #[test]
    fn title_change_emitted_when_titles_differ() {
        let from = doc(vec![]);
        let to = doc(vec![]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let out = render(
            diff,
            &ctx(),
            Detail::Outline,
            Includes::default(),
            &Filter::default(),
            DEFAULT_OUTPUT_BUDGET,
        )
        .unwrap();
        let tc = out.title_change.unwrap();
        assert_eq!(tc.from, "Spec v0.9");
        assert_eq!(tc.to, "Spec v1.0");
    }

    #[test]
    fn title_change_omitted_when_titles_match() {
        let mut c = ctx();
        c.from_title = c.to_title.clone();
        let from = doc(vec![]);
        let to = doc(vec![]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let out = render(
            diff,
            &c,
            Detail::Outline,
            Includes::default(),
            &Filter::default(),
            DEFAULT_OUTPUT_BUDGET,
        )
        .unwrap();
        assert!(out.title_change.is_none());
    }

    #[test]
    fn label_change_diff_added_and_removed() {
        let from = doc(vec![]);
        let to = doc(vec![]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let inc = Includes {
            labels: true,
            ..Includes::default()
        };
        let out = render(
            diff,
            &ctx(),
            Detail::Outline,
            inc,
            &Filter::default(),
            DEFAULT_OUTPUT_BUDGET,
        )
        .unwrap();
        let lc = out.labels.unwrap();
        assert_eq!(lc.added, vec!["approved".to_string()]);
        assert_eq!(lc.removed, vec!["wip".to_string()]);
    }

    #[test]
    fn filter_by_path_drops_other_sections() {
        let from = doc(vec![h(2, "A"), p("a"), h(2, "B"), p("b")]);
        let to = doc(vec![h(2, "A"), p("a edit"), h(2, "B"), p("b edit")]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let filter = Filter {
            sections: vec!["/h2#a".to_string()],
            ..Filter::default()
        };
        let out = render(
            diff,
            &ctx(),
            Detail::Outline,
            Includes::default(),
            &filter,
            DEFAULT_OUTPUT_BUDGET,
        )
        .unwrap();
        assert_eq!(out.sections.len(), 1);
        assert_eq!(out.sections[0].path, "/h2#a");
    }

    #[test]
    fn filter_by_kind_drops_unchanged() {
        let from = doc(vec![h(2, "A"), p("a"), h(2, "B"), p("b")]);
        let to = doc(vec![h(2, "A"), p("a edit"), h(2, "B"), p("b")]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let filter = Filter {
            kinds: vec![ChangeKind::Modified],
            ..Filter::default()
        };
        let out = render(
            diff,
            &ctx(),
            Detail::Outline,
            Includes::default(),
            &filter,
            DEFAULT_OUTPUT_BUDGET,
        )
        .unwrap();
        assert_eq!(out.sections.len(), 1);
        assert_eq!(out.sections[0].path, "/h2#a");
    }

    #[test]
    fn filter_min_change_chars_drops_small_edits() {
        let from = doc(vec![h(2, "A"), p("ab"), h(2, "B"), p("aaaaaaaaaa")]);
        let to = doc(vec![h(2, "A"), p("ac"), h(2, "B"), p("bbbbbbbbbb")]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let filter = Filter {
            min_change_chars: 10,
            ..Filter::default()
        };
        let out = render(
            diff,
            &ctx(),
            Detail::Outline,
            Includes::default(),
            &filter,
            DEFAULT_OUTPUT_BUDGET,
        )
        .unwrap();
        // Section A's text is 4 chars total ("ab" + "ac"); below 10. B is 20.
        assert_eq!(out.sections.len(), 1);
        assert_eq!(out.sections[0].path, "/h2#b");
    }

    #[test]
    fn budget_truncates_and_sets_continuation_cursor() {
        // Make many sections with substantial diffs so the output blows the budget.
        let mut from_blocks: Vec<AdfNode> = Vec::new();
        let mut to_blocks: Vec<AdfNode> = Vec::new();
        for i in 0..50 {
            from_blocks.push(h(2, &format!("section-{i}")));
            from_blocks.push(p(&"alpha ".repeat(20)));
            to_blocks.push(h(2, &format!("section-{i}")));
            to_blocks.push(p(&"beta ".repeat(20)));
        }
        let diff = diff_documents(&doc(from_blocks), &doc(to_blocks), &DiffOptions::default());
        let out = render(
            diff,
            &ctx(),
            Detail::Full,
            Includes::default(),
            &Filter::default(),
            2048, // tiny budget
        )
        .unwrap();
        assert!(out.truncated);
        let cont = out.continuation.as_ref().expect("continuation present");
        assert!(cont.next_cursor.is_some());
        // Decode the cursor and verify it points to a real section.
        let cur = Cursor::decode(cont.next_cursor.as_ref().unwrap()).unwrap();
        assert_eq!(cur.page_id, "12345");
        assert!(cur.section_path.starts_with("/h2#"));
    }

    #[test]
    fn budget_not_truncated_when_output_fits() {
        let from = doc(vec![h(2, "A"), p("a")]);
        let to = doc(vec![h(2, "A"), p("a edit")]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let out = render(
            diff,
            &ctx(),
            Detail::Full,
            Includes::default(),
            &Filter::default(),
            DEFAULT_OUTPUT_BUDGET,
        )
        .unwrap();
        assert!(!out.truncated);
        assert!(out.continuation.is_none());
    }

    #[test]
    fn render_section_unified_format() {
        let from = doc(vec![h(2, "A"), p("hello world")]);
        let to = doc(vec![h(2, "A"), p("hello there")]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let cur = Cursor {
            page_id: "p".to_string(),
            from_v: 1,
            to_v: 2,
            section_path: "/h2#a".to_string(),
        };
        let out = render_section(&diff, &cur, SectionFormat::Unified).unwrap();
        assert!(out.contains("- hello world"));
        assert!(out.contains("+ hello there"));
        assert!(out.contains("/h2#a"));
    }

    #[test]
    fn render_section_side_by_side_format() {
        let from = doc(vec![h(2, "A"), p("alpha")]);
        let to = doc(vec![h(2, "A"), p("beta")]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let cur = Cursor {
            page_id: "p".to_string(),
            from_v: 1,
            to_v: 2,
            section_path: "/h2#a".to_string(),
        };
        let out = render_section(&diff, &cur, SectionFormat::SideBySide).unwrap();
        assert!(out.contains("alpha"));
        assert!(out.contains("beta"));
        assert!(out.contains('|'));
    }

    #[test]
    fn render_section_markdown_inline_format() {
        let from = doc(vec![h(2, "A"), p("hello world")]);
        let to = doc(vec![h(2, "A"), p("hello universe")]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let cur = Cursor {
            page_id: "p".to_string(),
            from_v: 1,
            to_v: 2,
            section_path: "/h2#a".to_string(),
        };
        let out = render_section(&diff, &cur, SectionFormat::MarkdownInline).unwrap();
        assert!(out.contains("hello"));
        assert!(out.contains("universe"));
    }

    #[test]
    fn render_section_unknown_path_errors() {
        let from = doc(vec![h(2, "A"), p("a")]);
        let to = doc(vec![h(2, "A"), p("b")]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let cur = Cursor {
            page_id: "p".to_string(),
            from_v: 1,
            to_v: 2,
            section_path: "/h2#nope".to_string(),
        };
        let err = render_section(&diff, &cur, SectionFormat::Unified).unwrap_err();
        assert!(err.to_string().contains("Section not found"));
    }

    #[test]
    fn body_excluded_when_include_body_false() {
        let from = doc(vec![h(2, "A"), p("a")]);
        let to = doc(vec![h(2, "A"), p("a edit")]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let inc = Includes {
            body: false,
            ..Includes::default()
        };
        let out = render(
            diff,
            &ctx(),
            Detail::Outline,
            inc,
            &Filter::default(),
            DEFAULT_OUTPUT_BUDGET,
        )
        .unwrap();
        assert!(out.sections.is_empty());
    }

    #[test]
    fn truncate_helper_handles_unicode() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 5), "hell…");
        // Multibyte: ensure char count, not byte count.
        let s = "ééééé"; // 5 chars, 10 bytes
        assert_eq!(truncate(s, 5), s);
    }

    // ── Section formatters: every NodeDelta variant ───────────────

    /// Builds a single-section `Diff` whose only delta is the one supplied.
    /// Used to exercise format renderers without going through the full
    /// `diff_documents` pipeline.
    fn diff_with_single_delta(delta: NodeDelta) -> Diff {
        let section = SectionDiff {
            heading: "S".to_string(),
            path: "/h2#s".to_string(),
            change: ChangeKind::Modified,
            deltas: vec![delta],
        };
        Diff {
            sections: vec![section],
            stats: DiffStats::default(),
        }
    }

    fn cursor() -> Cursor {
        Cursor {
            page_id: "p".to_string(),
            from_v: 1,
            to_v: 2,
            section_path: "/h2#s".to_string(),
        }
    }

    fn cell_delta() -> CellDelta {
        CellDelta {
            row: 1,
            col: 2,
            from_text: "old".to_string(),
            to_text: "new".to_string(),
        }
    }

    fn snapshot_delta(text: &str) -> NodeSnapshot {
        NodeSnapshot {
            node_type: "paragraph".to_string(),
            text: text.to_string(),
        }
    }

    #[test]
    fn unified_format_emits_table_cell_lines() {
        let diff = diff_with_single_delta(NodeDelta::Table(TableDelta {
            cells: vec![cell_delta()],
        }));
        let out = render_section(&diff, &cursor(), SectionFormat::Unified).unwrap();
        assert!(out.contains("table[1][2]"));
        assert!(out.contains("- old"));
        assert!(out.contains("+ new"));
    }

    #[test]
    fn unified_format_emits_list_lines() {
        let diff = diff_with_single_delta(NodeDelta::List(ListDelta {
            items_added: vec!["new-item".to_string()],
            items_removed: vec!["old-item".to_string()],
            items_modified: vec![("a".to_string(), "b".to_string())],
        }));
        let out = render_section(&diff, &cursor(), SectionFormat::Unified).unwrap();
        assert!(out.contains("- old-item"));
        assert!(out.contains("+ new-item"));
        assert!(out.contains("- a"));
        assert!(out.contains("+ b"));
    }

    #[test]
    fn unified_format_emits_added_removed_snapshots() {
        let added = diff_with_single_delta(NodeDelta::Added(snapshot_delta("first\nsecond")));
        let out = render_section(&added, &cursor(), SectionFormat::Unified).unwrap();
        assert!(out.contains("+ first"));
        assert!(out.contains("+ second"));

        let removed = diff_with_single_delta(NodeDelta::Removed(snapshot_delta("gone\nbye")));
        let out = render_section(&removed, &cursor(), SectionFormat::Unified).unwrap();
        assert!(out.contains("- gone"));
        assert!(out.contains("- bye"));
    }

    #[test]
    fn unified_format_emits_opaque_block() {
        let diff = diff_with_single_delta(NodeDelta::Opaque(OpaqueDelta {
            node_type: "panel".to_string(),
            from_summary: "before".to_string(),
            to_summary: "after".to_string(),
        }));
        let out = render_section(&diff, &cursor(), SectionFormat::Unified).unwrap();
        assert!(out.contains("(panel changed)"));
        assert!(out.contains("- before"));
        assert!(out.contains("+ after"));
    }

    #[test]
    fn unified_format_emits_code_block_lines() {
        let diff = diff_with_single_delta(NodeDelta::CodeBlock(CodeBlockDelta {
            language: Some("rust".to_string()),
            from_text: "fn one() {}".to_string(),
            to_text: "fn one() {}\nfn two() {}".to_string(),
        }));
        let out = render_section(&diff, &cursor(), SectionFormat::Unified).unwrap();
        assert!(out.contains("```rust"));
        assert!(out.contains("+ fn two() {}"));
    }

    #[test]
    fn side_by_side_format_emits_all_variants() {
        let cases: Vec<NodeDelta> = vec![
            NodeDelta::Paragraph(ParagraphDelta {
                from_text: "alpha".to_string(),
                to_text: "beta".to_string(),
                words_added: 1,
                words_removed: 1,
            }),
            NodeDelta::CodeBlock(CodeBlockDelta {
                language: None,
                from_text: "old".to_string(),
                to_text: "new".to_string(),
            }),
            NodeDelta::Table(TableDelta {
                cells: vec![cell_delta()],
            }),
            NodeDelta::List(ListDelta {
                items_added: vec!["x".to_string()],
                items_removed: vec!["y".to_string()],
                items_modified: vec![("a".to_string(), "b".to_string())],
            }),
            NodeDelta::Added(snapshot_delta("plus")),
            NodeDelta::Removed(snapshot_delta("minus")),
            NodeDelta::Opaque(OpaqueDelta {
                node_type: "panel".to_string(),
                from_summary: "from".to_string(),
                to_summary: "to".to_string(),
            }),
        ];
        for delta in cases {
            let diff = diff_with_single_delta(delta);
            let out = render_section(&diff, &cursor(), SectionFormat::SideBySide).unwrap();
            assert!(out.contains('|'));
            assert!(out.contains("/h2#s"));
        }
    }

    #[test]
    fn markdown_inline_format_emits_all_variants() {
        let cases: Vec<NodeDelta> = vec![
            NodeDelta::CodeBlock(CodeBlockDelta {
                language: Some("rust".to_string()),
                from_text: "fn a() {}".to_string(),
                to_text: "fn a() { 1 }".to_string(),
            }),
            NodeDelta::Table(TableDelta {
                cells: vec![cell_delta()],
            }),
            NodeDelta::List(ListDelta {
                items_added: vec!["x".to_string()],
                items_removed: vec!["y".to_string()],
                items_modified: vec![("a".to_string(), "b".to_string())],
            }),
            NodeDelta::Added(snapshot_delta("plus")),
            NodeDelta::Removed(snapshot_delta("minus")),
            NodeDelta::Opaque(OpaqueDelta {
                node_type: "panel".to_string(),
                from_summary: "from".to_string(),
                to_summary: "to".to_string(),
            }),
        ];
        for delta in cases {
            let diff = diff_with_single_delta(delta);
            let out = render_section(&diff, &cursor(), SectionFormat::MarkdownInline).unwrap();
            assert!(out.contains("###"));
        }
    }

    #[test]
    fn markdown_inline_uses_preamble_label_when_heading_empty() {
        let section = SectionDiff {
            heading: String::new(),
            path: String::new(),
            change: ChangeKind::Modified,
            deltas: vec![NodeDelta::Paragraph(ParagraphDelta {
                from_text: "a".to_string(),
                to_text: "b".to_string(),
                words_added: 1,
                words_removed: 1,
            })],
        };
        let diff = Diff {
            sections: vec![section],
            stats: DiffStats::default(),
        };
        let cur = Cursor {
            page_id: "p".to_string(),
            from_v: 1,
            to_v: 2,
            section_path: String::new(),
        };
        let out = render_section(&diff, &cur, SectionFormat::MarkdownInline).unwrap();
        assert!(out.contains("Preamble"));
    }

    // ── Summarizers ───────────────────────────────────────────────

    #[test]
    fn summarize_section_truncates_long_delta_list() {
        let mk_para = |a: &str, b: &str| {
            NodeDelta::Paragraph(ParagraphDelta {
                from_text: a.to_string(),
                to_text: b.to_string(),
                words_added: 1,
                words_removed: 1,
            })
        };
        let section = SectionDiff {
            heading: "S".to_string(),
            path: "/h2#s".to_string(),
            change: ChangeKind::Modified,
            deltas: vec![
                mk_para("a1", "a2"),
                mk_para("b1", "b2"),
                mk_para("c1", "c2"),
                mk_para("d1", "d2"),
                mk_para("e1", "e2"),
            ],
        };
        let summary = summarize_section(&section);
        assert!(summary.contains("(+2 more)"));
    }

    #[test]
    fn summarize_section_returns_change_label_when_empty() {
        let section = SectionDiff {
            heading: "S".to_string(),
            path: "/h2#s".to_string(),
            change: ChangeKind::Added,
            deltas: vec![],
        };
        assert_eq!(summarize_section(&section), "added");
    }

    #[test]
    fn summarize_delta_covers_every_variant() {
        // Each variant exercises a distinct match arm in summarize_delta.
        let s = summarize_delta(&NodeDelta::CodeBlock(CodeBlockDelta {
            language: None,
            from_text: "a".to_string(),
            to_text: "b".to_string(),
        }));
        assert!(s.contains("code"));

        let s = summarize_delta(&NodeDelta::Table(TableDelta {
            cells: vec![cell_delta(), cell_delta()],
        }));
        assert!(s.contains("table") && s.contains("2 cell"));

        let s = summarize_delta(&NodeDelta::List(ListDelta {
            items_added: vec!["x".to_string()],
            items_removed: vec!["y".to_string()],
            items_modified: vec![("a".to_string(), "b".to_string())],
        }));
        assert!(s.contains("+1") && s.contains("-1") && s.contains("~1"));

        let s = summarize_delta(&NodeDelta::List(ListDelta::default()));
        assert_eq!(s, "list edit");

        let s = summarize_delta(&NodeDelta::Added(snapshot_delta("plus")));
        assert!(s.starts_with('+'));

        let s = summarize_delta(&NodeDelta::Removed(snapshot_delta("minus")));
        assert!(s.starts_with('-'));

        let s = summarize_delta(&NodeDelta::Opaque(OpaqueDelta {
            node_type: "panel".to_string(),
            from_summary: "x".to_string(),
            to_summary: "y".to_string(),
        }));
        assert!(s.contains("panel changed"));
    }

    #[test]
    fn change_label_covers_every_kind() {
        assert_eq!(change_label(ChangeKind::Added), "added");
        assert_eq!(change_label(ChangeKind::Removed), "removed");
        assert_eq!(change_label(ChangeKind::Modified), "modified");
        assert_eq!(change_label(ChangeKind::Moved), "moved");
        assert_eq!(change_label(ChangeKind::Unchanged), "unchanged");
    }

    // ── section_change_chars ──────────────────────────────────────

    #[test]
    fn section_change_chars_sums_every_delta_variant() {
        let section = SectionDiff {
            heading: "S".to_string(),
            path: "/h2#s".to_string(),
            change: ChangeKind::Modified,
            deltas: vec![
                NodeDelta::Paragraph(ParagraphDelta {
                    from_text: "a".to_string(), // 1
                    to_text: "bc".to_string(),  // 2
                    words_added: 1,
                    words_removed: 1,
                }),
                NodeDelta::CodeBlock(CodeBlockDelta {
                    language: None,
                    from_text: "xx".to_string(), // 2
                    to_text: "yyy".to_string(),  // 3
                }),
                NodeDelta::Added(snapshot_delta("foo")), // 3
                NodeDelta::Removed(snapshot_delta("bar")), // 3
                NodeDelta::Table(TableDelta {
                    cells: vec![CellDelta {
                        row: 0,
                        col: 0,
                        from_text: "12".to_string(), // 2
                        to_text: "34".to_string(),   // 2
                    }],
                }),
                NodeDelta::List(ListDelta {
                    items_added: vec!["a".to_string()],                         // 1
                    items_removed: vec!["bb".to_string()],                      // 2
                    items_modified: vec![("xx".to_string(), "yy".to_string())], // 2 + 2
                }),
                NodeDelta::Opaque(OpaqueDelta {
                    node_type: "panel".to_string(),
                    from_summary: "p".to_string(), // 1
                    to_summary: "qq".to_string(),  // 2
                }),
            ],
        };
        assert_eq!(
            section_change_chars(&section),
            1 + 2 + 2 + 3 + 3 + 3 + 2 + 2 + 1 + 2 + 2 + 2 + 1 + 2
        );
    }

    // ── aggregate_stats ───────────────────────────────────────────

    #[test]
    fn aggregate_stats_counts_each_change_kind_and_delta() {
        let mk_para = || {
            NodeDelta::Paragraph(ParagraphDelta {
                from_text: "a".to_string(),
                to_text: "b".to_string(),
                words_added: 2,
                words_removed: 1,
            })
        };
        let mk_table = || {
            NodeDelta::Table(TableDelta {
                cells: vec![cell_delta()],
            })
        };
        let mk_added = || NodeDelta::Added(snapshot_delta("hello world"));
        let mk_removed = || NodeDelta::Removed(snapshot_delta("bye now"));

        let sections = vec![
            SectionDiff {
                heading: "A".to_string(),
                path: "/h2#a".to_string(),
                change: ChangeKind::Added,
                deltas: vec![mk_added()],
            },
            SectionDiff {
                heading: "B".to_string(),
                path: "/h2#b".to_string(),
                change: ChangeKind::Removed,
                deltas: vec![mk_removed()],
            },
            SectionDiff {
                heading: "C".to_string(),
                path: "/h2#c".to_string(),
                change: ChangeKind::Modified,
                deltas: vec![mk_para(), mk_table()],
            },
            SectionDiff {
                heading: "D".to_string(),
                path: "/h2#d".to_string(),
                change: ChangeKind::Moved,
                deltas: vec![],
            },
            SectionDiff {
                heading: "E".to_string(),
                path: "/h2#e".to_string(),
                change: ChangeKind::Unchanged,
                deltas: vec![],
            },
        ];
        let stats = aggregate_stats(&sections);
        assert_eq!(stats.sections_added, 1);
        assert_eq!(stats.sections_removed, 1);
        assert_eq!(stats.sections_modified, 1);
        assert_eq!(stats.sections_moved, 1);
        assert_eq!(stats.paragraphs_modified, 1);
        assert_eq!(stats.tables_modified, 1);
        assert_eq!(stats.words_added, 2 + 2); // para 2 + added "hello world" 2
        assert!(stats.words_removed >= 1);
        assert!(stats.chars_added > 0);
        assert!(stats.chars_removed > 0);
    }

    #[test]
    fn apply_filter_recomputes_stats() {
        let mut diff = Diff {
            sections: vec![
                SectionDiff {
                    heading: "A".to_string(),
                    path: "/h2#a".to_string(),
                    change: ChangeKind::Modified,
                    deltas: vec![NodeDelta::Paragraph(ParagraphDelta {
                        from_text: "a".to_string(),
                        to_text: "b".to_string(),
                        words_added: 1,
                        words_removed: 1,
                    })],
                },
                SectionDiff {
                    heading: "B".to_string(),
                    path: "/h2#b".to_string(),
                    change: ChangeKind::Added,
                    deltas: vec![NodeDelta::Added(snapshot_delta("new"))],
                },
            ],
            stats: DiffStats {
                sections_modified: 1,
                sections_added: 1,
                paragraphs_modified: 1,
                ..DiffStats::default()
            },
        };
        let filter = Filter {
            sections: vec!["/h2#a".to_string()],
            ..Filter::default()
        };
        apply_filter(&mut diff, &filter);
        assert_eq!(diff.sections.len(), 1);
        assert_eq!(diff.stats.sections_added, 0);
        assert_eq!(diff.stats.sections_modified, 1);
    }

    // ── Budget edge cases ─────────────────────────────────────────

    #[test]
    fn budget_truncates_to_zero_when_no_section_fits() {
        let from = doc(vec![h(2, "A"), p(&"alpha ".repeat(100))]);
        let to = doc(vec![h(2, "A"), p(&"beta ".repeat(100))]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let out = render(
            diff,
            &ctx(),
            Detail::Full,
            Includes::default(),
            &Filter::default(),
            64, // way too small for even one section
        )
        .unwrap();
        assert!(out.truncated);
        assert!(out.sections.is_empty());
        // Continuation should still report the next cursor (the first section).
        let cont = out.continuation.as_ref().expect("continuation set");
        assert!(cont.next_cursor.is_some());
    }

    // ── render_section error path ─────────────────────────────────

    #[test]
    fn render_section_unknown_section_path_errors() {
        let diff = diff_with_single_delta(NodeDelta::Added(snapshot_delta("x")));
        let cur = Cursor {
            page_id: "p".to_string(),
            from_v: 1,
            to_v: 2,
            section_path: "/h2#missing".to_string(),
        };
        let err = render_section(&diff, &cur, SectionFormat::Unified).unwrap_err();
        assert!(err.to_string().contains("Section not found"));
    }

    // ── Includes / metadata exclusion ─────────────────────────────

    #[test]
    fn metadata_excluded_omits_versions() {
        let from = doc(vec![]);
        let to = doc(vec![]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let inc = Includes {
            metadata: false,
            ..Includes::default()
        };
        let out = render(
            diff,
            &ctx(),
            Detail::Outline,
            inc,
            &Filter::default(),
            DEFAULT_OUTPUT_BUDGET,
        )
        .unwrap();
        assert!(out.versions.is_none());
    }

    // ── Filter stat recomputation: kinds + min_change_chars ───────

    #[test]
    fn unified_format_emits_unchanged_lines_in_code_block_diff() {
        // Mixed code-block edit: keeps `fn one() {}`, adds `fn two() {}`,
        // exercising the `ChangeTag::Equal` branch of `push_text_diff`.
        let diff = diff_with_single_delta(NodeDelta::CodeBlock(CodeBlockDelta {
            language: Some("rust".to_string()),
            from_text: "fn one() {}\n".to_string(),
            to_text: "fn one() {}\nfn two() {}\n".to_string(),
        }));
        let out = render_section(&diff, &cursor(), SectionFormat::Unified).unwrap();
        // The "  " (Equal) prefix indicates an unchanged line in the
        // unified output.
        assert!(out.contains("  fn one"), "got: {out}");
        assert!(out.contains("+ fn two"));
    }

    #[test]
    fn aggregate_stats_handles_code_list_opaque_no_op_branch() {
        // CodeBlock / List / Opaque deltas don't bump any per-block counter
        // in `accumulate_stats`. Apply a filter that re-runs `aggregate_stats`
        // over a section containing one of each, and verify the no-op arm
        // is exercised (chars/words stay zero, sections_modified == 1).
        let mut diff = Diff {
            sections: vec![SectionDiff {
                heading: "S".to_string(),
                path: "/h2#s".to_string(),
                change: ChangeKind::Modified,
                deltas: vec![
                    NodeDelta::CodeBlock(CodeBlockDelta {
                        language: None,
                        from_text: "old".to_string(),
                        to_text: "new".to_string(),
                    }),
                    NodeDelta::List(ListDelta {
                        items_added: vec!["x".to_string()],
                        items_removed: Vec::new(),
                        items_modified: Vec::new(),
                    }),
                    NodeDelta::Opaque(OpaqueDelta {
                        node_type: "panel".to_string(),
                        from_summary: "a".to_string(),
                        to_summary: "b".to_string(),
                    }),
                ],
            }],
            stats: DiffStats::default(),
        };
        // No filter constraints, but apply_filter recomputes stats.
        apply_filter(&mut diff, &Filter::default());
        assert_eq!(diff.stats.sections_modified, 1);
        assert_eq!(diff.stats.paragraphs_modified, 0);
        assert_eq!(diff.stats.tables_modified, 0);
        assert_eq!(diff.stats.chars_added, 0);
        assert_eq!(diff.stats.chars_removed, 0);
    }

    #[test]
    fn filter_min_chars_recomputes_after_drop() {
        let from = doc(vec![h(2, "A"), p("x"), h(2, "B"), p(&"a".repeat(50))]);
        let to = doc(vec![h(2, "A"), p("y"), h(2, "B"), p(&"b".repeat(50))]);
        let diff = diff_documents(&from, &to, &DiffOptions::default());
        let filter = Filter {
            min_change_chars: 50,
            ..Filter::default()
        };
        let out = render(
            diff,
            &ctx(),
            Detail::Outline,
            Includes::default(),
            &filter,
            DEFAULT_OUTPUT_BUDGET,
        )
        .unwrap();
        // Only section B (>= 50 chars) survives.
        assert_eq!(out.sections.len(), 1);
        assert_eq!(out.sections[0].path, "/h2#b");
        assert_eq!(out.summary.by_kind.sections_modified, 1);
    }
}
