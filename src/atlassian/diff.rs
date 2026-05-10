//! Structural diff over [`AdfDocument`].
//!
//! Produces an in-memory IR (`Diff`) that the `diff_format` module renders
//! into the YAML output for `confluence_compare`. The diff is structurally
//! aware: it walks the ADF tree, splits documents into heading-delimited
//! sections, and emits per-block change records rather than character-level
//! deltas over a serialization. See the design notes in issue #706.
//!
//! Node identity uses a three-tier matcher:
//!
//! 1. **Natural-key**: `attrs.localId` for `table` / `tableRow` / `tableCell`,
//!    `attrs.id` for `media` / `mention`, `attrs.url` for `inlineCard` /
//!    `blockCard`, top-level `localId` for `expand` / `nestedExpand`.
//! 2. **Content-hash**: stable hash of the canonicalized subtree, bucketed
//!    by node type. Catches "moved without edit" cases.
//! 3. **Positional**: index-based pairing of the residual.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use serde::Serialize;
use similar::{ChangeTag, TextDiff};

use crate::atlassian::adf::{AdfDocument, AdfNode};

// ── Public IR ────────────────────────────────────────────────────────

/// Diff between two ADF documents.
#[derive(Debug, Clone, Serialize)]
pub struct Diff {
    /// Sections present in either document, in `to` order with `Removed`
    /// sections appended at the end.
    pub sections: Vec<SectionDiff>,
    /// Aggregate change statistics.
    pub stats: DiffStats,
}

/// Aggregate counts across the diff.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct DiffStats {
    /// Sections that exist only in `to`.
    pub sections_added: u32,
    /// Sections that exist only in `from`.
    pub sections_removed: u32,
    /// Sections present on both sides with at least one delta.
    pub sections_modified: u32,
    /// Sections present on both sides at different positions.
    pub sections_moved: u32,
    /// Paragraph-shaped block edits (paragraph, blockquote leaves, etc.).
    pub paragraphs_modified: u32,
    /// Table edits (one or more cell changes).
    pub tables_modified: u32,
    /// Total characters added across all prose deltas.
    pub chars_added: u32,
    /// Total characters removed across all prose deltas.
    pub chars_removed: u32,
    /// Total words added across all prose deltas.
    pub words_added: u32,
    /// Total words removed across all prose deltas.
    pub words_removed: u32,
}

/// A single section-level diff entry.
#[derive(Debug, Clone, Serialize)]
pub struct SectionDiff {
    /// Heading text (empty for the document preamble preceding the first heading).
    pub heading: String,
    /// Heading-anchor path, e.g. `/h2#background`. Empty path for the preamble.
    pub path: String,
    /// Coarse change classification.
    pub change: ChangeKind,
    /// Per-block deltas inside the section.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub deltas: Vec<NodeDelta>,
}

/// Coarse change classification.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// Section exists only in `to`.
    Added,
    /// Section exists only in `from`.
    Removed,
    /// Section exists on both sides with content edits.
    Modified,
    /// Section exists on both sides at the same position with no content edits.
    Unchanged,
    /// Section exists on both sides but at different positions.
    Moved,
}

/// A per-block delta inside a section.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeDelta {
    /// A whole block was added.
    Added(NodeSnapshot),
    /// A whole block was removed.
    Removed(NodeSnapshot),
    /// A paragraph or other prose-leaf block was modified.
    Paragraph(ParagraphDelta),
    /// A code block was modified (line-level).
    CodeBlock(CodeBlockDelta),
    /// A table was modified (cell-level).
    Table(TableDelta),
    /// A list was modified (item-level).
    List(ListDelta),
    /// A block changed but no specialized renderer is wired up.
    Opaque(OpaqueDelta),
}

/// Snapshot of a block as plain text, used for added/removed entries.
#[derive(Debug, Clone, Serialize)]
pub struct NodeSnapshot {
    /// ADF node type (`paragraph`, `codeBlock`, ...).
    pub node_type: String,
    /// Plain-text rendering of the node.
    pub text: String,
}

/// Paragraph-level prose change.
#[derive(Debug, Clone, Serialize)]
pub struct ParagraphDelta {
    /// Plain-text content before.
    pub from_text: String,
    /// Plain-text content after.
    pub to_text: String,
    /// Words added in this delta (for stats roll-up).
    pub words_added: u32,
    /// Words removed in this delta.
    pub words_removed: u32,
}

/// Code-block change.
#[derive(Debug, Clone, Serialize)]
pub struct CodeBlockDelta {
    /// Code language attribute, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Code text before.
    pub from_text: String,
    /// Code text after.
    pub to_text: String,
}

/// Table change (one or more cells modified).
#[derive(Debug, Clone, Serialize)]
pub struct TableDelta {
    /// Modified cells.
    pub cells: Vec<CellDelta>,
}

/// A single modified cell.
#[derive(Debug, Clone, Serialize)]
pub struct CellDelta {
    /// 0-based row.
    pub row: usize,
    /// 0-based column.
    pub col: usize,
    /// Cell text before.
    pub from_text: String,
    /// Cell text after.
    pub to_text: String,
}

/// List change.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ListDelta {
    /// New list items, as plain text.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items_added: Vec<String>,
    /// Removed list items, as plain text.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items_removed: Vec<String>,
    /// Items modified in place: `(from, to)` pairs.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items_modified: Vec<(String, String)>,
}

/// Fallback delta for block types without a specialized renderer.
#[derive(Debug, Clone, Serialize)]
pub struct OpaqueDelta {
    /// ADF node type that changed.
    pub node_type: String,
    /// Plain-text snapshot of the `from` side.
    pub from_summary: String,
    /// Plain-text snapshot of the `to` side.
    pub to_summary: String,
}

/// Diff configuration.
#[derive(Debug, Clone, Default)]
pub struct DiffOptions {
    /// When true, runs of whitespace inside text nodes are collapsed to a
    /// single space before diffing. Eliminates whitespace-only edits.
    pub ignore_whitespace: bool,
}

// ── Entry point ──────────────────────────────────────────────────────

/// Computes a structural diff between two ADF documents.
#[must_use]
pub fn diff_documents(from: &AdfDocument, to: &AdfDocument, opts: &DiffOptions) -> Diff {
    let from_sections = split_into_sections(&from.content, opts);
    let to_sections = split_into_sections(&to.content, opts);

    // Build path → original_index lookup tables for O(1) match.
    let mut from_by_path: HashMap<String, usize> = HashMap::with_capacity(from_sections.len());
    for (idx, s) in from_sections.iter().enumerate() {
        from_by_path.insert(s.path.clone(), idx);
    }

    let mut sections: Vec<SectionDiff> = Vec::new();
    let mut stats = DiffStats::default();
    let mut matched_from: HashSet<usize> = HashSet::new();

    for (to_idx, to_section) in to_sections.iter().enumerate() {
        if let Some(&from_idx) = from_by_path.get(&to_section.path) {
            matched_from.insert(from_idx);
            let from_section = &from_sections[from_idx];
            let deltas = diff_blocks(&from_section.blocks, &to_section.blocks, opts);
            for delta in &deltas {
                accumulate_delta(&mut stats, delta);
            }
            let change = if !deltas.is_empty() {
                stats.sections_modified += 1;
                ChangeKind::Modified
            } else if from_idx != to_idx {
                stats.sections_moved += 1;
                ChangeKind::Moved
            } else {
                ChangeKind::Unchanged
            };
            sections.push(SectionDiff {
                heading: to_section.heading.clone(),
                path: to_section.path.clone(),
                change,
                deltas,
            });
        } else {
            stats.sections_added += 1;
            sections.push(SectionDiff {
                heading: to_section.heading.clone(),
                path: to_section.path.clone(),
                change: ChangeKind::Added,
                deltas: snapshot_blocks(&to_section.blocks),
            });
        }
    }

    // Append removed sections in their original order.
    for (from_idx, from_section) in from_sections.iter().enumerate() {
        if !matched_from.contains(&from_idx) {
            stats.sections_removed += 1;
            sections.push(SectionDiff {
                heading: from_section.heading.clone(),
                path: from_section.path.clone(),
                change: ChangeKind::Removed,
                deltas: snapshot_blocks(&from_section.blocks),
            });
        }
    }

    Diff { sections, stats }
}

// ── Section split ────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct RawSection {
    /// Original heading node (None for the document preamble).
    heading_node: Option<AdfNode>,
    /// Plain heading text (empty for the preamble).
    heading: String,
    /// Stable section path (e.g. `/h2#background`), or empty for preamble.
    path: String,
    /// Body blocks of the section (excluding the heading itself).
    blocks: Vec<AdfNode>,
}

fn split_into_sections(content: &[AdfNode], opts: &DiffOptions) -> Vec<RawSection> {
    let mut sections: Vec<RawSection> = Vec::new();
    let mut occurrences: HashMap<(u8, String), u32> = HashMap::new();
    let mut current_blocks: Vec<AdfNode> = Vec::new();
    let mut current_heading: Option<AdfNode> = None;
    let mut current_level: u8 = 0;

    for node in content {
        if node.node_type == "heading" {
            // Close the previous section.
            sections.push(build_section(
                current_heading.take(),
                current_level,
                std::mem::take(&mut current_blocks),
                &mut occurrences,
                opts,
            ));
            current_level = heading_level(node).unwrap_or(0);
            current_heading = Some(node.clone());
        } else {
            current_blocks.push(node.clone());
        }
    }
    sections.push(build_section(
        current_heading,
        current_level,
        current_blocks,
        &mut occurrences,
        opts,
    ));

    // Drop a leading empty preamble (no heading and no blocks): common case.
    if let Some(first) = sections.first() {
        if first.heading_node.is_none() && first.blocks.is_empty() {
            sections.remove(0);
        }
    }
    sections
}

fn build_section(
    heading_node: Option<AdfNode>,
    level: u8,
    blocks: Vec<AdfNode>,
    occurrences: &mut HashMap<(u8, String), u32>,
    opts: &DiffOptions,
) -> RawSection {
    let heading_text = heading_node
        .as_ref()
        .map(|n| extract_text(n, opts))
        .unwrap_or_default();
    let heading_text = heading_text.trim().to_string();
    let path = if heading_node.is_some() {
        let slug = slugify(&heading_text);
        let key = (level, slug.clone());
        let count = occurrences.entry(key).or_insert(0);
        *count += 1;
        section_path(level, &slug, *count)
    } else {
        String::new()
    };
    RawSection {
        heading_node,
        heading: heading_text,
        path,
        blocks,
    }
}

fn heading_level(node: &AdfNode) -> Option<u8> {
    let attrs = node.attrs.as_ref()?;
    attrs
        .get("level")
        .and_then(serde_json::Value::as_u64)
        .and_then(|n| u8::try_from(n).ok())
}

fn section_path(level: u8, slug: &str, occurrence: u32) -> String {
    if occurrence <= 1 {
        format!("/h{level}#{slug}")
    } else {
        format!("/h{level}#{slug}-{occurrence}")
    }
}

fn slugify(text: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = true;
    for c in text.chars() {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                out.push(lc);
            }
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        out.push_str("section");
    }
    out
}

// ── Text extraction & whitespace normalization ──────────────────────

fn extract_text(node: &AdfNode, opts: &DiffOptions) -> String {
    let mut out = String::new();
    collect_text(node, &mut out);
    if opts.ignore_whitespace {
        normalize_whitespace(&out)
    } else {
        out
    }
}

fn collect_text(node: &AdfNode, out: &mut String) {
    if let Some(t) = &node.text {
        out.push_str(t);
    }
    if let Some(children) = &node.content {
        for child in children {
            collect_text(child, out);
        }
    }
}

fn normalize_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_ws = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_ws {
                out.push(' ');
                prev_ws = true;
            }
        } else {
            out.push(c);
            prev_ws = false;
        }
    }
    out.trim().to_string()
}

// ── Block-level diff with three-tier matching ───────────────────────

fn diff_blocks(from: &[AdfNode], to: &[AdfNode], opts: &DiffOptions) -> Vec<NodeDelta> {
    let pairs = match_nodes(from, to);
    let mut deltas: Vec<NodeDelta> = Vec::new();

    for pair in pairs {
        match pair {
            MatchPair::Both(fi, ti) => {
                if let Some(delta) = diff_node(&from[fi], &to[ti], opts) {
                    deltas.push(delta);
                }
            }
            MatchPair::OnlyFrom(fi) => {
                deltas.push(NodeDelta::Removed(snapshot_node(&from[fi], opts)));
            }
            MatchPair::OnlyTo(ti) => {
                deltas.push(NodeDelta::Added(snapshot_node(&to[ti], opts)));
            }
        }
    }
    deltas
}

#[derive(Debug, Clone, Copy)]
enum MatchPair {
    Both(usize, usize),
    OnlyFrom(usize),
    OnlyTo(usize),
}

/// Three-tier matcher: natural keys first, then content hash, then position.
fn match_nodes(from: &[AdfNode], to: &[AdfNode]) -> Vec<MatchPair> {
    let mut from_used = vec![false; from.len()];
    let mut to_used = vec![false; to.len()];
    let mut pairs: Vec<MatchPair> = Vec::new();

    // Tier 1: natural-key match. Pair only when the same key occurs in both
    // and node types agree.
    let mut from_keys: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (i, n) in from.iter().enumerate() {
        if let Some(k) = natural_key(n) {
            from_keys
                .entry((n.node_type.clone(), k))
                .or_default()
                .push(i);
        }
    }
    for (i, n) in to.iter().enumerate() {
        if let Some(k) = natural_key(n) {
            if let Some(slots) = from_keys.get_mut(&(n.node_type.clone(), k)) {
                if let Some(fi) = slots.pop() {
                    pairs.push(MatchPair::Both(fi, i));
                    from_used[fi] = true;
                    to_used[i] = true;
                }
            }
        }
    }

    // Tier 2: content-hash match on the residual, bucketed by node type.
    let mut from_hashes: HashMap<(String, u64), Vec<usize>> = HashMap::new();
    for (i, n) in from.iter().enumerate() {
        if from_used[i] {
            continue;
        }
        from_hashes
            .entry((n.node_type.clone(), content_hash(n)))
            .or_default()
            .push(i);
    }
    for (i, n) in to.iter().enumerate() {
        if to_used[i] {
            continue;
        }
        let h = content_hash(n);
        if let Some(slots) = from_hashes.get_mut(&(n.node_type.clone(), h)) {
            if let Some(fi) = slots.pop() {
                pairs.push(MatchPair::Both(fi, i));
                from_used[fi] = true;
                to_used[i] = true;
            }
        }
    }

    // Tier 3: positional pairing of the remainder (only when node types match).
    let from_residual: Vec<usize> = (0..from.len()).filter(|&i| !from_used[i]).collect();
    let to_residual: Vec<usize> = (0..to.len()).filter(|&i| !to_used[i]).collect();
    let mut fi = 0;
    let mut ti = 0;
    while fi < from_residual.len() && ti < to_residual.len() {
        let f = from_residual[fi];
        let t = to_residual[ti];
        if from[f].node_type == to[t].node_type {
            pairs.push(MatchPair::Both(f, t));
            from_used[f] = true;
            to_used[t] = true;
            fi += 1;
            ti += 1;
        } else {
            // Type mismatch — emit removal of the earlier residual side.
            // Heuristic: drop the smaller index so we keep aligning forward.
            if from_residual[fi] <= to_residual[ti] {
                pairs.push(MatchPair::OnlyFrom(f));
                from_used[f] = true;
                fi += 1;
            } else {
                pairs.push(MatchPair::OnlyTo(t));
                to_used[t] = true;
                ti += 1;
            }
        }
    }
    while fi < from_residual.len() {
        pairs.push(MatchPair::OnlyFrom(from_residual[fi]));
        fi += 1;
    }
    while ti < to_residual.len() {
        pairs.push(MatchPair::OnlyTo(to_residual[ti]));
        ti += 1;
    }

    // Sort `Both` pairs by `to` index for stable output ordering.
    pairs.sort_by_key(|p| match p {
        MatchPair::Both(_, t) | MatchPair::OnlyTo(t) => (*t, 0),
        MatchPair::OnlyFrom(f) => (usize::MAX, *f),
    });
    pairs
}

fn natural_key(node: &AdfNode) -> Option<String> {
    if let Some(id) = &node.local_id {
        return Some(id.clone());
    }
    let attrs = node.attrs.as_ref()?;
    let key_attr: Option<&str> = match node.node_type.as_str() {
        "table" | "tableRow" | "tableCell" | "tableHeader" | "expand" | "nestedExpand" => {
            Some("localId")
        }
        "media" | "mention" => Some("id"),
        "inlineCard" | "blockCard" => Some("url"),
        _ => None,
    };
    let key_attr = key_attr?;
    attrs
        .get(key_attr)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn content_hash(node: &AdfNode) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    hash_node(node, &mut hasher);
    hasher.finish()
}

fn hash_node(node: &AdfNode, hasher: &mut impl Hasher) {
    node.node_type.hash(hasher);
    if let Some(t) = &node.text {
        t.hash(hasher);
    }
    if let Some(children) = &node.content {
        for c in children {
            hash_node(c, hasher);
        }
    }
}

// ── Per-node diff dispatch ──────────────────────────────────────────

fn diff_node(from: &AdfNode, to: &AdfNode, opts: &DiffOptions) -> Option<NodeDelta> {
    match from.node_type.as_str() {
        "paragraph" | "blockquote" => diff_paragraph(from, to, opts),
        "codeBlock" => diff_code_block(from, to, opts),
        "table" => diff_table(from, to, opts),
        "bulletList" | "orderedList" | "taskList" => diff_list(from, to, opts),
        _ => diff_opaque(from, to, opts),
    }
}

fn diff_paragraph(from: &AdfNode, to: &AdfNode, opts: &DiffOptions) -> Option<NodeDelta> {
    let from_text = extract_text(from, opts);
    let to_text = extract_text(to, opts);
    if from_text == to_text {
        return None;
    }
    let (words_added, words_removed) = word_counts(&from_text, &to_text);
    Some(NodeDelta::Paragraph(ParagraphDelta {
        from_text,
        to_text,
        words_added,
        words_removed,
    }))
}

fn diff_code_block(from: &AdfNode, to: &AdfNode, opts: &DiffOptions) -> Option<NodeDelta> {
    let from_text = extract_text(from, opts);
    let to_text = extract_text(to, opts);
    if from_text == to_text {
        return None;
    }
    let language = code_language(from).or_else(|| code_language(to));
    Some(NodeDelta::CodeBlock(CodeBlockDelta {
        language,
        from_text,
        to_text,
    }))
}

fn code_language(node: &AdfNode) -> Option<String> {
    node.attrs
        .as_ref()?
        .get("language")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn diff_table(from: &AdfNode, to: &AdfNode, opts: &DiffOptions) -> Option<NodeDelta> {
    let from_rows = table_rows(from);
    let to_rows = table_rows(to);
    let row_count = from_rows.len().max(to_rows.len());
    let mut cells: Vec<CellDelta> = Vec::new();
    for r in 0..row_count {
        let from_cells = from_rows.get(r).map_or(&[][..], Vec::as_slice);
        let to_cells = to_rows.get(r).map_or(&[][..], Vec::as_slice);
        let col_count = from_cells.len().max(to_cells.len());
        for c in 0..col_count {
            let from_text = from_cells
                .get(c)
                .map(|n| extract_text(n, opts))
                .unwrap_or_default();
            let to_text = to_cells
                .get(c)
                .map(|n| extract_text(n, opts))
                .unwrap_or_default();
            if from_text != to_text {
                cells.push(CellDelta {
                    row: r,
                    col: c,
                    from_text,
                    to_text,
                });
            }
        }
    }
    if cells.is_empty() {
        None
    } else {
        Some(NodeDelta::Table(TableDelta { cells }))
    }
}

fn table_rows(node: &AdfNode) -> Vec<Vec<&AdfNode>> {
    let mut rows: Vec<Vec<&AdfNode>> = Vec::new();
    if let Some(children) = &node.content {
        for row in children {
            if row.node_type == "tableRow" {
                let mut cells: Vec<&AdfNode> = Vec::new();
                if let Some(row_children) = &row.content {
                    for cell in row_children {
                        if cell.node_type == "tableCell" || cell.node_type == "tableHeader" {
                            cells.push(cell);
                        }
                    }
                }
                rows.push(cells);
            }
        }
    }
    rows
}

fn diff_list(from: &AdfNode, to: &AdfNode, opts: &DiffOptions) -> Option<NodeDelta> {
    let mut from_remaining = list_items(from, opts);
    let mut to_remaining = list_items(to, opts);
    let mut delta = ListDelta::default();

    // Pair identical items by content first.
    from_remaining.retain(|f| {
        if let Some(pos) = to_remaining.iter().position(|t| t == f) {
            to_remaining.remove(pos);
            false
        } else {
            true
        }
    });

    // Pair the rest by position as "modified".
    let pair_count = from_remaining.len().min(to_remaining.len());
    for i in 0..pair_count {
        delta
            .items_modified
            .push((from_remaining[i].clone(), to_remaining[i].clone()));
    }
    delta
        .items_removed
        .extend(from_remaining.iter().skip(pair_count).cloned());
    delta
        .items_added
        .extend(to_remaining.iter().skip(pair_count).cloned());

    if delta.items_added.is_empty()
        && delta.items_removed.is_empty()
        && delta.items_modified.is_empty()
    {
        None
    } else {
        Some(NodeDelta::List(delta))
    }
}

fn list_items(node: &AdfNode, opts: &DiffOptions) -> Vec<String> {
    node.content
        .as_ref()
        .map(|children| {
            children
                .iter()
                .map(|item| extract_text(item, opts))
                .collect()
        })
        .unwrap_or_default()
}

fn diff_opaque(from: &AdfNode, to: &AdfNode, opts: &DiffOptions) -> Option<NodeDelta> {
    let from_text = extract_text(from, opts);
    let to_text = extract_text(to, opts);
    if from_text == to_text && content_hash(from) == content_hash(to) {
        return None;
    }
    Some(NodeDelta::Opaque(OpaqueDelta {
        node_type: from.node_type.clone(),
        from_summary: from_text,
        to_summary: to_text,
    }))
}

// ── Snapshot helpers (for added/removed blocks) ─────────────────────

fn snapshot_node(node: &AdfNode, opts: &DiffOptions) -> NodeSnapshot {
    NodeSnapshot {
        node_type: node.node_type.clone(),
        text: extract_text(node, opts),
    }
}

fn snapshot_blocks(blocks: &[AdfNode]) -> Vec<NodeDelta> {
    // Snapshots use no normalization — we just want plain text.
    let opts = DiffOptions::default();
    blocks
        .iter()
        .map(|n| NodeDelta::Added(snapshot_node(n, &opts)))
        .collect()
}

// ── Word-counting helper using `similar` ────────────────────────────

fn word_counts(from: &str, to: &str) -> (u32, u32) {
    let diff = TextDiff::from_words(from, to);
    let mut added: u32 = 0;
    let mut removed: u32 = 0;
    for change in diff.iter_all_changes() {
        let val = change.value();
        if val.trim().is_empty() {
            continue;
        }
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

// ── Stats accumulation ───────────────────────────────────────────────

fn accumulate_delta(stats: &mut DiffStats, delta: &NodeDelta) {
    match delta {
        NodeDelta::Paragraph(p) => {
            stats.paragraphs_modified += 1;
            stats.words_added += p.words_added;
            stats.words_removed += p.words_removed;
            let (ca, cr) = char_counts(&p.from_text, &p.to_text);
            stats.chars_added += ca;
            stats.chars_removed += cr;
        }
        NodeDelta::CodeBlock(c) => {
            let (ca, cr) = char_counts(&c.from_text, &c.to_text);
            stats.chars_added += ca;
            stats.chars_removed += cr;
        }
        NodeDelta::Table(_) => {
            stats.tables_modified += 1;
        }
        NodeDelta::Added(s) => {
            stats.chars_added += s.text.chars().count() as u32;
            stats.words_added += s.text.split_whitespace().count() as u32;
        }
        NodeDelta::Removed(s) => {
            stats.chars_removed += s.text.chars().count() as u32;
            stats.words_removed += s.text.split_whitespace().count() as u32;
        }
        NodeDelta::List(_) | NodeDelta::Opaque(_) => {}
    }
}

fn char_counts(from: &str, to: &str) -> (u32, u32) {
    let diff = TextDiff::from_chars(from, to);
    let mut added: u32 = 0;
    let mut removed: u32 = 0;
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => added += 1,
            ChangeTag::Delete => removed += 1,
            ChangeTag::Equal => {}
        }
    }
    (added, removed)
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

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

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Background"), "background");
        assert_eq!(slugify("Hello World"), "hello-world");
        assert_eq!(slugify("Foo, Bar & Baz!"), "foo-bar-baz");
        assert_eq!(slugify("   spaced   "), "spaced");
        assert_eq!(slugify("!!!"), "section");
    }

    #[test]
    fn section_path_includes_occurrence_for_collisions() {
        assert_eq!(section_path(2, "background", 1), "/h2#background");
        assert_eq!(section_path(2, "background", 2), "/h2#background-2");
    }

    #[test]
    fn split_into_sections_groups_by_heading() {
        let document = doc(vec![
            p("preamble"),
            h(2, "Background"),
            p("paragraph A"),
            h(2, "Architecture"),
            p("paragraph B"),
        ]);
        let sections = split_into_sections(&document.content, &DiffOptions::default());
        assert_eq!(sections.len(), 3);
        assert!(sections[0].path.is_empty());
        assert_eq!(sections[1].path, "/h2#background");
        assert_eq!(sections[2].path, "/h2#architecture");
    }

    #[test]
    fn duplicate_heading_gets_occurrence_suffix() {
        let document = doc(vec![h(2, "Notes"), p("a"), h(2, "Notes"), p("b")]);
        let sections = split_into_sections(&document.content, &DiffOptions::default());
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].path, "/h2#notes");
        assert_eq!(sections[1].path, "/h2#notes-2");
    }

    #[test]
    fn diff_paragraph_text_change_classifies_section_modified() {
        let from = doc(vec![h(2, "Background"), p("We use database version 12.")]);
        let to = doc(vec![h(2, "Background"), p("We use database version 14.")]);
        let d = diff_documents(&from, &to, &DiffOptions::default());
        assert_eq!(d.sections.len(), 1);
        assert_eq!(d.sections[0].change, ChangeKind::Modified);
        assert_eq!(d.stats.sections_modified, 1);
        assert_eq!(d.stats.paragraphs_modified, 1);
        assert!(d.stats.words_added > 0 || d.stats.words_removed > 0);
        match &d.sections[0].deltas[0] {
            NodeDelta::Paragraph(p) => {
                assert!(p.from_text.contains("12"));
                assert!(p.to_text.contains("14"));
            }
            other => panic!("expected paragraph delta, got {other:?}"),
        }
    }

    #[test]
    fn added_section_classified() {
        let from = doc(vec![h(2, "A"), p("a")]);
        let to = doc(vec![h(2, "A"), p("a"), h(2, "B"), p("b")]);
        let d = diff_documents(&from, &to, &DiffOptions::default());
        assert_eq!(d.stats.sections_added, 1);
        assert_eq!(d.stats.sections_removed, 0);
        let added = d
            .sections
            .iter()
            .find(|s| s.path == "/h2#b")
            .expect("section B should appear");
        assert_eq!(added.change, ChangeKind::Added);
    }

    #[test]
    fn removed_section_classified() {
        let from = doc(vec![h(2, "A"), p("a"), h(2, "B"), p("b")]);
        let to = doc(vec![h(2, "A"), p("a")]);
        let d = diff_documents(&from, &to, &DiffOptions::default());
        assert_eq!(d.stats.sections_removed, 1);
        let removed = d
            .sections
            .iter()
            .find(|s| s.path == "/h2#b")
            .expect("section B should appear");
        assert_eq!(removed.change, ChangeKind::Removed);
    }

    #[test]
    fn moved_section_classified() {
        let from = doc(vec![h(2, "A"), p("a"), h(2, "B"), p("b")]);
        let to = doc(vec![h(2, "B"), p("b"), h(2, "A"), p("a")]);
        let d = diff_documents(&from, &to, &DiffOptions::default());
        assert_eq!(d.stats.sections_moved, 2);
        for s in &d.sections {
            assert_eq!(s.change, ChangeKind::Moved);
        }
    }

    #[test]
    fn unchanged_when_documents_identical() {
        let from = doc(vec![h(2, "A"), p("a"), h(2, "B"), p("b")]);
        let to = from.clone();
        let d = diff_documents(&from, &to, &DiffOptions::default());
        for s in &d.sections {
            assert_eq!(s.change, ChangeKind::Unchanged);
        }
        assert_eq!(d.stats.sections_modified, 0);
        assert_eq!(d.stats.sections_added, 0);
        assert_eq!(d.stats.sections_removed, 0);
    }

    #[test]
    fn whitespace_normalization_suppresses_trivial_diff() {
        let from = doc(vec![h(2, "A"), p("hello world")]);
        let to = doc(vec![h(2, "A"), p("hello   world")]);
        let opts = DiffOptions {
            ignore_whitespace: true,
        };
        let d = diff_documents(&from, &to, &opts);
        assert_eq!(d.sections[0].change, ChangeKind::Unchanged);
    }

    #[test]
    fn whitespace_normalization_off_keeps_diff() {
        let from = doc(vec![h(2, "A"), p("hello world")]);
        let to = doc(vec![h(2, "A"), p("hello   world")]);
        let d = diff_documents(&from, &to, &DiffOptions::default());
        assert_eq!(d.sections[0].change, ChangeKind::Modified);
    }

    #[test]
    fn code_block_diff_emits_delta() {
        let from = doc(vec![
            h(2, "Code"),
            AdfNode::code_block(Some("rust"), "fn a() {}"),
        ]);
        let to = doc(vec![
            h(2, "Code"),
            AdfNode::code_block(Some("rust"), "fn a() { 1 }"),
        ]);
        let d = diff_documents(&from, &to, &DiffOptions::default());
        match &d.sections[0].deltas[0] {
            NodeDelta::CodeBlock(c) => {
                assert_eq!(c.language.as_deref(), Some("rust"));
                assert!(c.to_text.contains('1'));
            }
            other => panic!("expected code block delta, got {other:?}"),
        }
    }

    #[test]
    fn table_cell_edit_emits_cell_delta() {
        let from_table = AdfNode::table(vec![AdfNode::table_row(vec![
            AdfNode::table_cell(vec![p("alpha")]),
            AdfNode::table_cell(vec![p("beta")]),
        ])]);
        let to_table = AdfNode::table(vec![AdfNode::table_row(vec![
            AdfNode::table_cell(vec![p("alpha")]),
            AdfNode::table_cell(vec![p("BETA")]),
        ])]);
        let from = doc(vec![h(2, "T"), from_table]);
        let to = doc(vec![h(2, "T"), to_table]);
        let d = diff_documents(&from, &to, &DiffOptions::default());
        assert_eq!(d.stats.tables_modified, 1);
        match &d.sections[0].deltas[0] {
            NodeDelta::Table(t) => {
                assert_eq!(t.cells.len(), 1);
                assert_eq!(t.cells[0].row, 0);
                assert_eq!(t.cells[0].col, 1);
                assert_eq!(t.cells[0].from_text, "beta");
                assert_eq!(t.cells[0].to_text, "BETA");
            }
            other => panic!("expected table delta, got {other:?}"),
        }
    }

    #[test]
    fn list_item_add_remove_emits_list_delta() {
        let from = doc(vec![
            h(2, "L"),
            AdfNode::bullet_list(vec![
                AdfNode::list_item(vec![p("one")]),
                AdfNode::list_item(vec![p("two")]),
            ]),
        ]);
        let to = doc(vec![
            h(2, "L"),
            AdfNode::bullet_list(vec![
                AdfNode::list_item(vec![p("one")]),
                AdfNode::list_item(vec![p("three")]),
            ]),
        ]);
        let d = diff_documents(&from, &to, &DiffOptions::default());
        match &d.sections[0].deltas[0] {
            NodeDelta::List(l) => {
                // The matcher pairs the unchanged "one" first, then the
                // residual "two"/"three" become a modified pair.
                assert_eq!(l.items_modified.len(), 1);
                assert_eq!(l.items_modified[0].0, "two");
                assert_eq!(l.items_modified[0].1, "three");
            }
            other => panic!("expected list delta, got {other:?}"),
        }
    }

    #[test]
    fn natural_key_localid_pairs_moved_table_row() {
        let make_row = |local_id: &str, text: &str| AdfNode {
            node_type: "tableRow".to_string(),
            attrs: Some(json!({"localId": local_id})),
            content: Some(vec![AdfNode::table_cell(vec![p(text)])]),
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        };
        let from = vec![make_row("r1", "alpha"), make_row("r2", "beta")];
        let to = vec![make_row("r2", "beta"), make_row("r1", "ALPHA")];
        let pairs = match_nodes(&from, &to);
        // Both rows pair via natural-key, even though one was edited.
        let both = pairs
            .iter()
            .filter(|p| matches!(p, MatchPair::Both(_, _)))
            .count();
        assert_eq!(both, 2);
    }

    #[test]
    fn content_hash_pairs_moved_paragraph_without_localid() {
        let from = vec![p("alpha"), p("beta")];
        let to = vec![p("beta"), p("alpha")];
        let pairs = match_nodes(&from, &to);
        // Both paragraphs pair via content hash (tier 2).
        let both = pairs
            .iter()
            .filter(|p| matches!(p, MatchPair::Both(_, _)))
            .count();
        assert_eq!(both, 2);
    }

    #[test]
    fn position_pairs_residual_when_types_match() {
        let from = vec![p("one"), p("two")];
        let to = vec![p("uno"), p("dos")];
        // Neither hashes match, so both pair positionally.
        let pairs = match_nodes(&from, &to);
        let both = pairs
            .iter()
            .filter(|p| matches!(p, MatchPair::Both(_, _)))
            .count();
        assert_eq!(both, 2);
    }

    #[test]
    fn opaque_delta_fallback() {
        let from_panel = AdfNode {
            node_type: "panel".to_string(),
            attrs: Some(json!({"panelType": "info"})),
            content: Some(vec![p("note A")]),
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        };
        let to_panel = AdfNode {
            node_type: "panel".to_string(),
            attrs: Some(json!({"panelType": "info"})),
            content: Some(vec![p("note B")]),
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        };
        let from = doc(vec![h(2, "P"), from_panel]);
        let to = doc(vec![h(2, "P"), to_panel]);
        let d = diff_documents(&from, &to, &DiffOptions::default());
        match &d.sections[0].deltas[0] {
            NodeDelta::Opaque(o) => assert_eq!(o.node_type, "panel"),
            other => panic!("expected opaque delta, got {other:?}"),
        }
    }

    #[test]
    fn preamble_diff_works_without_heading() {
        let from = doc(vec![p("intro old")]);
        let to = doc(vec![p("intro new")]);
        let d = diff_documents(&from, &to, &DiffOptions::default());
        assert_eq!(d.sections.len(), 1);
        assert_eq!(d.sections[0].path, "");
        assert_eq!(d.sections[0].change, ChangeKind::Modified);
    }

    #[test]
    fn empty_documents_produce_empty_diff() {
        let from = doc(vec![]);
        let to = doc(vec![]);
        let d = diff_documents(&from, &to, &DiffOptions::default());
        assert_eq!(d.sections.len(), 0);
        assert_eq!(d.stats, DiffStats::default());
    }

    #[test]
    fn heading_with_no_text_uses_section_slug() {
        let from = doc(vec![AdfNode::heading(2, vec![]), p("a")]);
        let to = doc(vec![AdfNode::heading(2, vec![]), p("b")]);
        let d = diff_documents(&from, &to, &DiffOptions::default());
        assert_eq!(d.sections.len(), 1);
        assert_eq!(d.sections[0].path, "/h2#section");
    }

    // ── Three-tier matcher edge cases ─────────────────────────────

    #[test]
    fn match_nodes_residual_more_from_emits_only_from() {
        // Three from-only paragraphs; no to nodes — every block becomes OnlyFrom.
        let from = vec![p("a"), p("b"), p("c")];
        let to: Vec<AdfNode> = Vec::new();
        let pairs = match_nodes(&from, &to);
        assert_eq!(pairs.len(), 3);
        assert!(pairs.iter().all(|p| matches!(p, MatchPair::OnlyFrom(_))));
    }

    #[test]
    fn match_nodes_residual_more_to_emits_only_to() {
        let from: Vec<AdfNode> = Vec::new();
        let to = vec![p("a"), p("b"), p("c")];
        let pairs = match_nodes(&from, &to);
        assert_eq!(pairs.len(), 3);
        assert!(pairs.iter().all(|p| matches!(p, MatchPair::OnlyTo(_))));
    }

    #[test]
    fn match_nodes_type_mismatch_in_residual() {
        // Different node types at the same position force tier 3 to drop
        // one side rather than pair across types. With from=[paragraph,
        // codeBlock] and to=[codeBlock, paragraph]:
        //   fi=0 (para) vs ti=0 (code) — type mismatch → OnlyFrom(0)
        //   fi=1 (code) vs ti=0 (code) — match → Both(1, 0)
        //   leftover ti=1 (para) → OnlyTo(1)
        let from = vec![p("alpha"), AdfNode::code_block(None, "old code")];
        let to = vec![AdfNode::code_block(None, "new code"), p("beta")];
        let pairs = match_nodes(&from, &to);
        let both = pairs
            .iter()
            .filter(|p| matches!(p, MatchPair::Both(_, _)))
            .count();
        let only_from = pairs
            .iter()
            .filter(|p| matches!(p, MatchPair::OnlyFrom(_)))
            .count();
        let only_to = pairs
            .iter()
            .filter(|p| matches!(p, MatchPair::OnlyTo(_)))
            .count();
        // One typed pair plus one drop on each side — exercises the
        // type-mismatch branch of the residual loop.
        assert_eq!(both, 1);
        assert_eq!(only_from, 1);
        assert_eq!(only_to, 1);
    }

    #[test]
    fn match_nodes_type_mismatch_drops_to_when_to_index_smaller() {
        // Force the `else` branch of the type-mismatch heuristic where
        // `from_residual[fi] > to_residual[ti]` — tier 1 pairs nothing,
        // and the type mismatch sits with from-residual indices ahead of
        // to-residual indices. Construct: from = [code, code], to = [para, code].
        //   tier 2/3: from[0]=code matches from-residual[0]=0; to[1]=code
        //   gives the to-residual=[0,1], from-residual=[0,1]
        //
        // Instead, use natural keys to "anchor" early indices on one side:
        // from = [keyed-table, code], to = [para, keyed-table, code]
        //   tier 1 pairs from[0]<->to[1] → from_residual=[1], to_residual=[0,2]
        //   fi=0 (from[1]=code), ti=0 (to[0]=para): mismatch
        //     to_residual[0]=0 < from_residual[0]=1 → OnlyTo branch fires
        let make_keyed_table = |key: &str| AdfNode {
            node_type: "table".to_string(),
            attrs: Some(serde_json::json!({"localId": key})),
            content: None,
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        };
        let from = vec![make_keyed_table("t1"), AdfNode::code_block(None, "code")];
        let to = vec![
            p("orphan-para"),
            make_keyed_table("t1"),
            AdfNode::code_block(None, "code"),
        ];
        let pairs = match_nodes(&from, &to);
        let only_to = pairs
            .iter()
            .filter(|p| matches!(p, MatchPair::OnlyTo(_)))
            .count();
        assert!(only_to >= 1, "expected at least one OnlyTo, got {pairs:?}");
    }

    #[test]
    fn diff_blocks_emits_only_from_and_only_to_deltas() {
        // Force OnlyFrom (a block exists in from but not in to) and OnlyTo
        // (a block exists in to but not in from) by giving each side only
        // one block of an unmatched type.
        let from = vec![p("only-from")];
        let to = vec![AdfNode::code_block(None, "only-to")];
        let deltas = diff_blocks(&from, &to, &DiffOptions::default());
        let has_removed = deltas.iter().any(|d| matches!(d, NodeDelta::Removed(_)));
        let has_added = deltas.iter().any(|d| matches!(d, NodeDelta::Added(_)));
        assert!(has_removed && has_added, "got {deltas:?}");
    }

    // ── Per-block diff "no change" returns None ───────────────────

    #[test]
    fn diff_code_block_returns_none_when_text_matches() {
        let from = AdfNode::code_block(Some("rust"), "fn a() {}");
        let to = AdfNode::code_block(Some("rust"), "fn a() {}");
        assert!(diff_code_block(&from, &to, &DiffOptions::default()).is_none());
    }

    #[test]
    fn diff_table_returns_none_when_no_cells_changed() {
        let make_t = || {
            AdfNode::table(vec![AdfNode::table_row(vec![
                AdfNode::table_cell(vec![p("a")]),
                AdfNode::table_cell(vec![p("b")]),
            ])])
        };
        assert!(diff_table(&make_t(), &make_t(), &DiffOptions::default()).is_none());
    }

    #[test]
    fn diff_list_returns_none_when_items_match() {
        let make_l = || {
            AdfNode::bullet_list(vec![
                AdfNode::list_item(vec![p("one")]),
                AdfNode::list_item(vec![p("two")]),
            ])
        };
        assert!(diff_list(&make_l(), &make_l(), &DiffOptions::default()).is_none());
    }

    #[test]
    fn diff_opaque_returns_none_when_identical() {
        let panel = AdfNode {
            node_type: "panel".to_string(),
            attrs: Some(serde_json::json!({"panelType": "info"})),
            content: Some(vec![p("note")]),
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        };
        assert!(diff_opaque(&panel, &panel, &DiffOptions::default()).is_none());
    }

    // ── Table edge cases ──────────────────────────────────────────

    #[test]
    fn diff_table_with_unequal_row_counts() {
        let from = AdfNode::table(vec![AdfNode::table_row(vec![AdfNode::table_cell(vec![
            p("a"),
        ])])]);
        let to = AdfNode::table(vec![
            AdfNode::table_row(vec![AdfNode::table_cell(vec![p("a")])]),
            AdfNode::table_row(vec![AdfNode::table_cell(vec![p("b")])]),
        ]);
        let delta = diff_table(&from, &to, &DiffOptions::default()).unwrap();
        if let NodeDelta::Table(t) = delta {
            assert!(t.cells.iter().any(|c| c.row == 1));
        } else {
            panic!("expected table delta");
        }
    }

    #[test]
    fn diff_table_with_table_header_cells() {
        // table_rows accepts both tableCell and tableHeader.
        let from = AdfNode::table(vec![AdfNode::table_row(vec![AdfNode::table_header(vec![
            p("h1"),
        ])])]);
        let to = AdfNode::table(vec![AdfNode::table_row(vec![AdfNode::table_header(vec![
            p("h2"),
        ])])]);
        let delta = diff_table(&from, &to, &DiffOptions::default()).unwrap();
        if let NodeDelta::Table(t) = delta {
            assert_eq!(t.cells.len(), 1);
        } else {
            panic!("expected table delta");
        }
    }

    // ── snapshot_blocks ───────────────────────────────────────────

    #[test]
    fn snapshot_blocks_renders_each_block_as_added_delta() {
        let blocks = vec![p("alpha"), p("beta")];
        let snaps = snapshot_blocks(&blocks);
        assert_eq!(snaps.len(), 2);
        for s in snaps {
            assert!(matches!(s, NodeDelta::Added(_)));
        }
    }

    // ── Word/char counters ────────────────────────────────────────

    #[test]
    fn word_counts_skips_pure_whitespace_changes() {
        let (added, removed) = word_counts("hello world", "hello   world");
        // Whitespace changes shouldn't bump word counts (the trim filter).
        assert_eq!(added, 0);
        assert_eq!(removed, 0);
    }

    #[test]
    fn char_counts_handles_full_replacement() {
        let (added, removed) = char_counts("foo", "bar");
        assert!(added >= 3 && removed >= 3);
    }

    // ── List with only additions (skip > items_modified path) ────

    #[test]
    fn diff_list_pure_addition() {
        let from = AdfNode::bullet_list(vec![]);
        let to = AdfNode::bullet_list(vec![
            AdfNode::list_item(vec![p("a")]),
            AdfNode::list_item(vec![p("b")]),
        ]);
        let delta = diff_list(&from, &to, &DiffOptions::default()).unwrap();
        if let NodeDelta::List(l) = delta {
            assert_eq!(l.items_added.len(), 2);
            assert!(l.items_removed.is_empty());
            assert!(l.items_modified.is_empty());
        } else {
            panic!("expected list delta");
        }
    }

    #[test]
    fn diff_list_pure_removal() {
        let from = AdfNode::bullet_list(vec![
            AdfNode::list_item(vec![p("a")]),
            AdfNode::list_item(vec![p("b")]),
        ]);
        let to = AdfNode::bullet_list(vec![]);
        let delta = diff_list(&from, &to, &DiffOptions::default()).unwrap();
        if let NodeDelta::List(l) = delta {
            assert_eq!(l.items_removed.len(), 2);
            assert!(l.items_added.is_empty());
            assert!(l.items_modified.is_empty());
        } else {
            panic!("expected list delta");
        }
    }

    // ── code_language: missing attrs / missing field ─────────────

    #[test]
    fn code_language_returns_none_when_attrs_missing() {
        let n = AdfNode {
            node_type: "codeBlock".to_string(),
            attrs: None,
            content: None,
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        };
        assert!(code_language(&n).is_none());
    }

    #[test]
    fn code_language_returns_none_when_attrs_lack_language() {
        let n = AdfNode {
            node_type: "codeBlock".to_string(),
            attrs: Some(serde_json::json!({"other": "x"})),
            content: None,
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        };
        assert!(code_language(&n).is_none());
    }

    // ── natural_key fallbacks ─────────────────────────────────────

    #[test]
    fn natural_key_uses_id_attr_for_media_node() {
        let n = AdfNode {
            node_type: "media".to_string(),
            attrs: Some(serde_json::json!({"id": "media-uuid-1"})),
            content: None,
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        };
        assert_eq!(natural_key(&n).as_deref(), Some("media-uuid-1"));
    }

    #[test]
    fn natural_key_uses_url_attr_for_inline_card() {
        let n = AdfNode {
            node_type: "inlineCard".to_string(),
            attrs: Some(serde_json::json!({"url": "https://example.com/x"})),
            content: None,
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        };
        assert_eq!(natural_key(&n).as_deref(), Some("https://example.com/x"));
    }

    #[test]
    fn natural_key_returns_none_for_unknown_node_type() {
        let n = AdfNode {
            node_type: "unknown".to_string(),
            attrs: Some(serde_json::json!({"some": "value"})),
            content: None,
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        };
        assert!(natural_key(&n).is_none());
    }

    #[test]
    fn natural_key_returns_none_when_node_has_no_attrs() {
        let n = AdfNode {
            node_type: "table".to_string(),
            attrs: None,
            content: None,
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        };
        assert!(natural_key(&n).is_none());
    }

    // ── heading_level: missing attrs ──────────────────────────────

    #[test]
    fn paragraph_added_within_matched_section_accumulates_into_stats() {
        // Same heading on both sides + an extra paragraph in `to`. The
        // matcher emits an `Added` block delta inside the modified section,
        // which exercises the `NodeDelta::Added` arm of `accumulate_delta`.
        let from = doc(vec![h(2, "S"), p("kept")]);
        let to = doc(vec![h(2, "S"), p("kept"), p("hello world")]);
        let d = diff_documents(&from, &to, &DiffOptions::default());
        // `chars_added` and `words_added` should reflect the snapshot of the
        // newly-added paragraph.
        assert!(d.stats.chars_added >= 11, "got {:?}", d.stats);
        assert_eq!(d.stats.words_added, 2);
    }

    #[test]
    fn paragraph_removed_within_matched_section_accumulates_into_stats() {
        let from = doc(vec![h(2, "S"), p("kept"), p("removed text")]);
        let to = doc(vec![h(2, "S"), p("kept")]);
        let d = diff_documents(&from, &to, &DiffOptions::default());
        assert!(d.stats.chars_removed >= 12, "got {:?}", d.stats);
        assert_eq!(d.stats.words_removed, 2);
    }

    #[test]
    fn table_rows_skips_non_row_children() {
        // A table whose direct children include a non-tableRow node
        // exercises the false branch of `if row.node_type == "tableRow"`
        // inside `table_rows`.
        let from = AdfNode::table(vec![
            p("not-a-row"), // skipped by the row-type filter
            AdfNode::table_row(vec![AdfNode::table_cell(vec![p("alpha")])]),
        ]);
        let to = AdfNode::table(vec![
            p("not-a-row"),
            AdfNode::table_row(vec![AdfNode::table_cell(vec![p("beta")])]),
        ]);
        let delta = diff_table(&from, &to, &DiffOptions::default()).unwrap();
        // Inspect via the serialized form to avoid an unreachable
        // destructuring branch — `delta` is structurally guaranteed to be
        // `Table` by the call above.
        let json = serde_json::to_value(&delta).unwrap();
        assert_eq!(json["kind"], "table");
        assert_eq!(json["cells"].as_array().unwrap().len(), 1);
        assert_eq!(json["cells"][0]["from_text"], "alpha");
        assert_eq!(json["cells"][0]["to_text"], "beta");
    }

    #[test]
    fn table_rows_handles_table_with_no_content() {
        // A `table` node with no children at all exercises the false branch
        // of `if let Some(children)` inside `table_rows`.
        let empty_table = AdfNode {
            node_type: "table".to_string(),
            attrs: None,
            content: None,
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        };
        let result = diff_table(&empty_table, &empty_table, &DiffOptions::default());
        assert!(result.is_none());
    }

    #[test]
    fn table_rows_skips_non_cell_children() {
        // A `tableRow` with a non-cell child (here a paragraph) exercises
        // the false branch of the cell-type filter inside `table_rows`.
        let from = AdfNode::table(vec![AdfNode::table_row(vec![
            AdfNode::table_cell(vec![p("alpha")]),
            p("ignored"),
        ])]);
        let to = AdfNode::table(vec![AdfNode::table_row(vec![
            AdfNode::table_cell(vec![p("beta")]),
            p("ignored"),
        ])]);
        let delta = diff_table(&from, &to, &DiffOptions::default()).unwrap();
        if let NodeDelta::Table(t) = delta {
            // Only the real cell shows up.
            assert_eq!(t.cells.len(), 1);
        } else {
            panic!("expected table delta");
        }
    }

    #[test]
    fn table_rows_skips_rows_without_content() {
        // A `tableRow` without any children exercises the early-skip branch
        // (`if let Some(row_children)` false).
        let from = AdfNode::table(vec![AdfNode {
            node_type: "tableRow".to_string(),
            attrs: None,
            content: None,
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        }]);
        let to = from.clone();
        // Identical empty rows produce no cells, hence no delta.
        assert!(diff_table(&from, &to, &DiffOptions::default()).is_none());
    }

    #[test]
    fn heading_level_returns_none_when_attrs_missing() {
        let n = AdfNode {
            node_type: "heading".to_string(),
            attrs: None,
            content: None,
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        };
        assert!(heading_level(&n).is_none());
    }
}
