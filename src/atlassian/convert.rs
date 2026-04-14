//! Bidirectional conversion between markdown and Atlassian Document Format.
//!
//! Supports Tier 1 (standard GFM) constructs: headings, paragraphs, inline
//! marks (bold, italic, code, strikethrough, links), images, lists, code
//! blocks, blockquotes, horizontal rules, and tables.

use anyhow::Result;
use chrono::NaiveDate;
use tracing::{debug, warn};

use crate::atlassian::adf::{AdfDocument, AdfMark, AdfNode};
use crate::atlassian::attrs::parse_attrs;
use crate::atlassian::directive::{
    is_container_close, try_parse_container_open, try_parse_inline_directive,
    try_parse_leaf_directive,
};

// ── Markdown → ADF ──────────────────────────────────────────────────

/// Converts a markdown string to an ADF document.
pub fn markdown_to_adf(markdown: &str) -> Result<AdfDocument> {
    debug!(
        "markdown_to_adf: input {} bytes, {} lines",
        markdown.len(),
        markdown.lines().count()
    );
    let mut doc = AdfDocument::new();
    let mut parser = MarkdownParser::new(markdown);
    doc.content = parser.parse_blocks()?;
    debug!(
        "markdown_to_adf: produced {} top-level ADF nodes",
        doc.content.len()
    );
    Ok(doc)
}

/// Line-oriented state machine for parsing markdown into ADF block nodes.
struct MarkdownParser<'a> {
    lines: Vec<&'a str>,
    pos: usize,
}

impl<'a> MarkdownParser<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            lines: input.lines().collect(),
            pos: 0,
        }
    }

    fn at_end(&self) -> bool {
        self.pos >= self.lines.len()
    }

    fn current_line(&self) -> &'a str {
        self.lines[self.pos]
    }

    fn advance(&mut self) {
        self.pos += 1;
    }

    /// Collects indented continuation lines produced by hardBreaks (issue #402).
    ///
    /// When `full_text` ends with a hardBreak marker (trailing backslash or
    /// two trailing spaces), the next 2-space-indented line is appended as a
    /// continuation of the same paragraph.  The joined text is later fed to
    /// `parse_inline`, which converts the `\\\n` or `  \n` sequences back
    /// into `hardBreak` nodes.
    fn collect_hardbreak_continuations(&mut self, full_text: &mut String) {
        while has_trailing_hard_break(full_text) && !self.at_end() {
            let next = self.current_line();
            if let Some(stripped) = next.strip_prefix("  ") {
                full_text.push('\n');
                full_text.push_str(stripped);
                self.advance();
                continue;
            }
            break;
        }
    }

    fn parse_blocks(&mut self) -> Result<Vec<AdfNode>> {
        let mut blocks = Vec::new();

        while !self.at_end() {
            let line = self.current_line();

            if line.trim().is_empty() {
                self.advance();
                continue;
            }

            let mut node = if let Some(node) = self.try_heading() {
                node
            } else if let Some(node) = self.try_horizontal_rule() {
                node
            } else if let Some(node) = self.try_container_directive()? {
                node
            } else if let Some(node) = self.try_code_block()? {
                node
            } else if let Some(node) = self.try_table()? {
                node
            } else if let Some(node) = self.try_blockquote()? {
                node
            } else if let Some(node) = self.try_list()? {
                node
            } else if let Some(node) = self.try_leaf_directive() {
                node
            } else if let Some(node) = self.try_image() {
                node
            } else {
                self.parse_paragraph()?
            };

            // Check for trailing block-level {attrs} (align, indent, breakout)
            self.try_apply_block_attrs(&mut node);
            blocks.push(node);
        }

        Ok(blocks)
    }

    fn try_heading(&mut self) -> Option<AdfNode> {
        let line = self.current_line();
        let trimmed = line.trim_start();

        if !trimmed.starts_with('#') {
            return None;
        }

        let level = trimmed.chars().take_while(|&c| c == '#').count();
        if !(1..=6).contains(&level) || !trimmed[level..].starts_with(' ') {
            return None;
        }

        let mut full_text = trimmed[level + 1..].trim_start().to_string();
        self.advance();
        // Collect indented continuation lines produced by hardBreaks (issue #433).
        self.collect_hardbreak_continuations(&mut full_text);
        let inline_nodes = parse_inline(&full_text);

        #[allow(clippy::cast_possible_truncation)]
        Some(AdfNode::heading(level as u8, inline_nodes))
    }

    fn try_horizontal_rule(&mut self) -> Option<AdfNode> {
        let line = self.current_line().trim();
        let is_rule = (line.starts_with("---") && line.chars().all(|c| c == '-'))
            || (line.starts_with("***") && line.chars().all(|c| c == '*'))
            || (line.starts_with("___") && line.chars().all(|c| c == '_'));

        if is_rule && line.len() >= 3 {
            self.advance();
            Some(AdfNode::rule())
        } else {
            None
        }
    }

    fn try_code_block(&mut self) -> Result<Option<AdfNode>> {
        let line = self.current_line();
        if !line.starts_with("```") {
            return Ok(None);
        }

        let language = line[3..].trim();
        let language = if language.is_empty() {
            None
        } else {
            Some(language.to_string())
        };

        self.advance();
        let mut code_lines = Vec::new();

        while !self.at_end() {
            let line = self.current_line();
            if line.starts_with("```") {
                self.advance();
                break;
            }
            code_lines.push(line);
            self.advance();
        }

        let code_text = code_lines.join("\n");

        // If the language is "adf-unsupported", deserialize the JSON back to an AdfNode
        if language.as_deref() == Some("adf-unsupported") {
            if let Ok(node) = serde_json::from_str::<AdfNode>(&code_text) {
                return Ok(Some(node));
            }
        }

        Ok(Some(AdfNode::code_block(language.as_deref(), &code_text)))
    }

    fn try_blockquote(&mut self) -> Result<Option<AdfNode>> {
        let line = self.current_line();
        if !line.starts_with('>') {
            return Ok(None);
        }

        let mut quote_lines = Vec::new();
        while !self.at_end() {
            let line = self.current_line();
            if let Some(rest) = line.strip_prefix("> ") {
                quote_lines.push(rest);
                self.advance();
            } else if let Some(rest) = line.strip_prefix('>') {
                quote_lines.push(rest);
                self.advance();
            } else {
                break;
            }
        }

        let quote_text = quote_lines.join("\n");
        let mut inner_parser = MarkdownParser::new(&quote_text);
        let inner_blocks = inner_parser.parse_blocks()?;

        Ok(Some(AdfNode::blockquote(inner_blocks)))
    }

    fn try_list(&mut self) -> Result<Option<AdfNode>> {
        let line = self.current_line();
        let trimmed = line.trim_start();

        let is_bullet =
            trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ");
        let ordered_match = parse_ordered_list_marker(trimmed);

        if !is_bullet && ordered_match.is_none() {
            return Ok(None);
        }

        if is_bullet {
            self.parse_bullet_list()
        } else {
            let start = ordered_match.map_or(1, |(n, _)| n);
            self.parse_ordered_list(start)
        }
    }

    fn parse_bullet_list(&mut self) -> Result<Option<AdfNode>> {
        let mut items = Vec::new();
        let mut is_task_list = false;

        while !self.at_end() {
            let line = self.current_line();
            let trimmed = line.trim_start();

            if !(trimmed.starts_with("- ")
                || trimmed.starts_with("* ")
                || trimmed.starts_with("+ "))
            {
                break;
            }

            let after_marker = trimmed[2..].trim_start();

            // Detect task list items: - [ ] or - [x]
            if let Some((state, text)) = try_parse_task_marker(after_marker) {
                is_task_list = true;
                let (item_text, local_id, _para_local_id) = extract_trailing_local_id(text);
                let inline_nodes = parse_inline(item_text);
                let mut task = AdfNode::task_item(state, inline_nodes);
                // Override the placeholder localId if one was parsed
                if let Some(id) = local_id {
                    if let Some(ref mut attrs) = task.attrs {
                        attrs["localId"] = serde_json::Value::String(id);
                    }
                }
                items.push(task);
                self.advance();
            } else {
                let first_line = &trimmed[2..];
                self.advance();
                let mut full_text = first_line.to_string();
                self.collect_hardbreak_continuations(&mut full_text);
                let (item_text, local_id, para_local_id) = extract_trailing_local_id(&full_text);
                // Collect indented sub-content lines (2-space prefix).
                // This captures both nested lists and continuation
                // paragraphs that belong to the same list item.
                let mut sub_lines: Vec<String> = Vec::new();
                while !self.at_end() {
                    let next = self.current_line();
                    if let Some(stripped) = next.strip_prefix("  ") {
                        sub_lines.push(stripped.to_string());
                        self.advance();
                        continue;
                    }
                    break;
                }
                // If the first line is a block-level image, parse as mediaSingle
                // instead of wrapping in a paragraph (issue #430).
                let first_node = if let Some(media) = try_parse_media_single_from_line(item_text) {
                    media
                } else {
                    AdfNode::paragraph(parse_inline(item_text))
                };
                if sub_lines.is_empty() {
                    items.push(list_item_with_local_id(
                        vec![first_node],
                        local_id,
                        para_local_id,
                    ));
                } else {
                    let sub_text = sub_lines.join("\n");
                    let mut nested = MarkdownParser::new(&sub_text).parse_blocks()?;
                    let mut item_content = vec![first_node];
                    item_content.append(&mut nested);
                    items.push(list_item_with_local_id(
                        item_content,
                        local_id,
                        para_local_id,
                    ));
                }
            }
        }

        if items.is_empty() {
            Ok(None)
        } else if is_task_list {
            Ok(Some(AdfNode::task_list(items)))
        } else {
            Ok(Some(AdfNode::bullet_list(items)))
        }
    }

    fn parse_ordered_list(&mut self, start: u32) -> Result<Option<AdfNode>> {
        let mut items = Vec::new();

        while !self.at_end() {
            let line = self.current_line();
            let trimmed = line.trim_start();

            if let Some((_, rest)) = parse_ordered_list_marker(trimmed) {
                let first_line = rest.trim_start_matches(|c: char| c.is_ascii_whitespace());
                self.advance();
                let mut full_text = first_line.to_string();
                self.collect_hardbreak_continuations(&mut full_text);
                let (item_text, local_id, para_local_id) = extract_trailing_local_id(&full_text);
                // Collect indented sub-content lines (2-space prefix).
                let mut sub_lines: Vec<String> = Vec::new();
                while !self.at_end() {
                    let next = self.current_line();
                    if let Some(stripped) = next.strip_prefix("  ") {
                        sub_lines.push(stripped.to_string());
                        self.advance();
                        continue;
                    }
                    break;
                }
                // If the first line is a block-level image, parse as mediaSingle
                // instead of wrapping in a paragraph (issue #430).
                let first_node = if let Some(media) = try_parse_media_single_from_line(item_text) {
                    media
                } else {
                    AdfNode::paragraph(parse_inline(item_text))
                };
                if sub_lines.is_empty() {
                    items.push(list_item_with_local_id(
                        vec![first_node],
                        local_id,
                        para_local_id,
                    ));
                } else {
                    let sub_text = sub_lines.join("\n");
                    let mut nested = MarkdownParser::new(&sub_text).parse_blocks()?;
                    let mut item_content = vec![first_node];
                    item_content.append(&mut nested);
                    items.push(list_item_with_local_id(
                        item_content,
                        local_id,
                        para_local_id,
                    ));
                }
            } else {
                break;
            }
        }

        if items.is_empty() {
            Ok(None)
        } else {
            Ok(Some(AdfNode::ordered_list(items, Some(start))))
        }
    }

    fn try_apply_block_attrs(&mut self, node: &mut AdfNode) {
        if self.at_end() {
            return;
        }
        let line = self.current_line().trim();
        if !line.starts_with('{') {
            return;
        }
        let Some((_, attrs)) = parse_attrs(line, 0) else {
            return;
        };

        let mut marks = Vec::new();
        if let Some(align) = attrs.get("align") {
            marks.push(AdfMark::alignment(align));
        }
        if let Some(indent) = attrs.get("indent") {
            if let Ok(level) = indent.parse::<u32>() {
                marks.push(AdfMark::indentation(level));
            }
        }
        if let Some(mode) = attrs.get("breakout") {
            let width = attrs
                .get("breakoutWidth")
                .and_then(|w| w.parse::<u32>().ok());
            marks.push(AdfMark::breakout(mode, width));
        }

        // Parse localId from block attrs
        let local_id = attrs.get("localId").map(str::to_string);

        let has_attrs = !marks.is_empty() || local_id.is_some();
        if has_attrs {
            if !marks.is_empty() {
                let existing = node.marks.get_or_insert_with(Vec::new);
                existing.extend(marks);
            }
            if let Some(id) = local_id {
                let node_attrs = node.attrs.get_or_insert_with(|| serde_json::json!({}));
                node_attrs["localId"] = serde_json::Value::String(id);
            }
            self.advance(); // consume the attrs line
        }
    }

    fn try_container_directive(&mut self) -> Result<Option<AdfNode>> {
        let line = self.current_line();
        let Some((d, colon_count)) = try_parse_container_open(line) else {
            return Ok(None);
        };
        self.advance(); // past opening fence

        // Collect inner lines until the matching close fence, tracking nesting
        let mut inner_lines = Vec::new();
        let mut depth: usize = 0;
        while !self.at_end() {
            let current = self.current_line();
            if try_parse_container_open(current).is_some() {
                depth += 1;
            } else if depth == 0 && is_container_close(current, colon_count) {
                self.advance(); // past closing fence
                break;
            } else if depth > 0 && is_container_close(current, 3) {
                depth -= 1;
            }
            inner_lines.push(current.to_string());
            self.advance();
        }

        let inner_text = inner_lines.join("\n");

        let node = match d.name.as_str() {
            "panel" => {
                let panel_type = d
                    .attrs
                    .as_ref()
                    .and_then(|a| a.get("type"))
                    .unwrap_or("info");
                let inner_blocks = MarkdownParser::new(&inner_text).parse_blocks()?;
                let mut node = AdfNode::panel(panel_type, inner_blocks);
                // Pass through custom panel attrs (icon, color)
                if let Some(ref attrs) = d.attrs {
                    if let Some(ref mut node_attrs) = node.attrs {
                        if let Some(icon) = attrs.get("icon") {
                            node_attrs["panelIcon"] = serde_json::Value::String(icon.to_string());
                        }
                        if let Some(color) = attrs.get("color") {
                            node_attrs["panelColor"] = serde_json::Value::String(color.to_string());
                        }
                    }
                }
                node
            }
            "expand" => {
                let title = d.attrs.as_ref().and_then(|a| a.get("title"));
                let inner_blocks = MarkdownParser::new(&inner_text).parse_blocks()?;
                let mut node = AdfNode::expand(title, inner_blocks);
                pass_through_expand_params(&d.attrs, &mut node);
                node
            }
            "nested-expand" => {
                let title = d.attrs.as_ref().and_then(|a| a.get("title"));
                let inner_blocks = MarkdownParser::new(&inner_text).parse_blocks()?;
                let mut node = AdfNode::nested_expand(title, inner_blocks);
                pass_through_expand_params(&d.attrs, &mut node);
                node
            }
            "layout" => {
                // Parse inner content looking for :::column sub-containers
                let columns = self.parse_layout_columns(&inner_text)?;
                AdfNode::layout_section(columns)
            }
            "decisions" => {
                let items = parse_decision_items(&inner_text);
                AdfNode::decision_list(items)
            }
            "table" => {
                let rows = self.parse_directive_table_rows(&inner_text)?;
                let mut table_attrs = serde_json::json!({});
                if let Some(ref attrs) = d.attrs {
                    if let Some(layout) = attrs.get("layout") {
                        table_attrs["layout"] = serde_json::Value::String(layout.to_string());
                    }
                    if attrs.has_flag("numbered") {
                        table_attrs["isNumberColumnEnabled"] = serde_json::json!(true);
                    } else if attrs.get("numbered") == Some("false") {
                        table_attrs["isNumberColumnEnabled"] = serde_json::json!(false);
                    }
                    if let Some(tw) = attrs.get("width") {
                        if let Ok(w) = tw.parse::<f64>() {
                            table_attrs["width"] = serde_json::json!(w);
                        }
                    }
                    if let Some(local_id) = attrs.get("localId") {
                        table_attrs["localId"] = serde_json::Value::String(local_id.to_string());
                    }
                }
                if table_attrs == serde_json::json!({}) {
                    AdfNode::table(rows)
                } else {
                    AdfNode::table_with_attrs(rows, table_attrs)
                }
            }
            "extension" => {
                let ext_type = d.attrs.as_ref().and_then(|a| a.get("type")).unwrap_or("");
                let ext_key = d.attrs.as_ref().and_then(|a| a.get("key")).unwrap_or("");
                let inner_blocks = MarkdownParser::new(&inner_text).parse_blocks()?;
                let mut node = AdfNode::bodied_extension(ext_type, ext_key, inner_blocks);
                if let (Some(ref dir_attrs), Some(ref mut node_attrs)) = (&d.attrs, &mut node.attrs)
                {
                    if let Some(layout) = dir_attrs.get("layout") {
                        node_attrs["layout"] = serde_json::Value::String(layout.to_string());
                    }
                    if let Some(local_id) = dir_attrs.get("localId") {
                        node_attrs["localId"] = serde_json::Value::String(local_id.to_string());
                    }
                }
                node
            }
            _ => return Ok(None),
        };

        Ok(Some(node))
    }

    fn parse_layout_columns(&self, inner_text: &str) -> Result<Vec<AdfNode>> {
        let mut columns = Vec::new();
        let mut current_column_lines: Vec<String> = Vec::new();
        let mut current_width: f64 = 50.0;
        let mut in_column = false;
        let mut depth: usize = 0;

        let lines: Vec<&str> = inner_text.lines().collect();
        let mut i = 0;

        while i < lines.len() {
            let line = lines[i];
            if let Some((col_d, _)) = try_parse_container_open(line) {
                if col_d.name == "column" && depth == 0 {
                    // Flush previous column
                    if in_column && !current_column_lines.is_empty() {
                        let col_text = current_column_lines.join("\n");
                        let blocks = MarkdownParser::new(&col_text).parse_blocks()?;
                        columns.push(AdfNode::layout_column(current_width, blocks));
                        current_column_lines.clear();
                    }
                    current_width = col_d
                        .attrs
                        .as_ref()
                        .and_then(|a| a.get("width"))
                        .and_then(|w| w.parse::<f64>().ok())
                        .unwrap_or(50.0);
                    in_column = true;
                    i += 1;
                    continue;
                }
                if in_column {
                    depth += 1;
                }
            }
            if in_column && is_container_close(line, 3) {
                if depth > 0 {
                    depth -= 1;
                    current_column_lines.push(line.to_string());
                    i += 1;
                    continue;
                }
                // End of column
                let col_text = current_column_lines.join("\n");
                let blocks = MarkdownParser::new(&col_text).parse_blocks()?;
                columns.push(AdfNode::layout_column(current_width, blocks));
                current_column_lines.clear();
                in_column = false;
                i += 1;
                continue;
            }
            if in_column {
                current_column_lines.push(line.to_string());
            }
            i += 1;
        }

        // Flush last column if no closing fence
        if in_column && !current_column_lines.is_empty() {
            let col_text = current_column_lines.join("\n");
            let blocks = MarkdownParser::new(&col_text).parse_blocks()?;
            columns.push(AdfNode::layout_column(current_width, blocks));
        }

        Ok(columns)
    }

    /// Parses `:::tr` / `:::th` / `:::td` sub-containers inside a `:::table` directive.
    fn parse_directive_table_rows(&self, inner_text: &str) -> Result<Vec<AdfNode>> {
        debug!(
            "parse_directive_table_rows: {} lines of inner text",
            inner_text.lines().count()
        );
        let mut rows = Vec::new();
        let lines: Vec<&str> = inner_text.lines().collect();
        let mut i = 0;

        while i < lines.len() {
            let line = lines[i];
            if let Some((d, _)) = try_parse_container_open(line) {
                if d.name == "tr" {
                    let tr_attrs = d.attrs.clone();
                    i += 1;
                    let (mut row, next_i) = self.parse_directive_table_row(&lines, i)?;
                    // Pass through localId from :::tr{localId=...}
                    if let Some(ref attrs) = tr_attrs {
                        if let Some(local_id) = attrs.get("localId") {
                            let row_attrs = row.attrs.get_or_insert_with(|| serde_json::json!({}));
                            row_attrs["localId"] = serde_json::Value::String(local_id.to_string());
                        }
                    }
                    rows.push(row);
                    i = next_i;
                    continue;
                }
                if d.name == "caption" {
                    i += 1;
                    let mut caption_lines = Vec::new();
                    while i < lines.len() {
                        if is_container_close(lines[i], 3) {
                            i += 1;
                            break;
                        }
                        caption_lines.push(lines[i]);
                        i += 1;
                    }
                    let caption_text = caption_lines.join("\n");
                    let inline_nodes = parse_inline(&caption_text);
                    rows.push(AdfNode::caption(inline_nodes));
                    continue;
                }
            }
            i += 1;
        }

        Ok(rows)
    }

    /// Parses cells within a `:::tr` container until its closing fence.
    fn parse_directive_table_row(&self, lines: &[&str], start: usize) -> Result<(AdfNode, usize)> {
        let mut cells = Vec::new();
        let mut i = start;
        let mut depth: usize = 0;

        while i < lines.len() {
            let line = lines[i];
            if is_container_close(line, 3) {
                if depth == 0 {
                    // End of :::tr
                    i += 1;
                    break;
                }
                depth -= 1;
                i += 1;
                continue;
            }
            if let Some((d, _)) = try_parse_container_open(line) {
                if depth == 0 && (d.name == "th" || d.name == "td") {
                    let is_header = d.name == "th";
                    let cell_attrs = d.attrs.clone();
                    i += 1;
                    let (cell, next_i) =
                        self.parse_directive_table_cell(lines, i, is_header, cell_attrs)?;
                    cells.push(cell);
                    i = next_i;
                    continue;
                }
                depth += 1;
            }
            i += 1;
        }

        if cells.is_empty() {
            let context = lines[start.saturating_sub(1)..lines.len().min(start + 3)].to_vec();
            warn!(
                "Directive table row at line {start} has no cells — \
                 Confluence requires at least one. Nearby lines: {context:?}"
            );
        }
        debug!("Parsed directive table row: {} cells", cells.len());

        Ok((AdfNode::table_row(cells), i))
    }

    /// Parses the content of a `:::th` or `:::td` cell until its closing fence.
    fn parse_directive_table_cell(
        &self,
        lines: &[&str],
        start: usize,
        is_header: bool,
        cell_attrs: Option<crate::atlassian::attrs::Attrs>,
    ) -> Result<(AdfNode, usize)> {
        let mut cell_lines = Vec::new();
        let mut i = start;
        let mut depth: usize = 0;

        while i < lines.len() {
            let line = lines[i];
            if try_parse_container_open(line).is_some() {
                depth += 1;
            } else if is_container_close(line, 3) {
                if depth == 0 {
                    i += 1;
                    break;
                }
                depth -= 1;
            }
            cell_lines.push(line.to_string());
            i += 1;
        }

        let cell_text = cell_lines.join("\n");
        let blocks = MarkdownParser::new(&cell_text).parse_blocks()?;

        let adf_attrs = cell_attrs.map(|a| build_cell_attrs(&a));

        let cell = if is_header {
            if let Some(attrs) = adf_attrs {
                AdfNode::table_header_with_attrs(blocks, attrs)
            } else {
                AdfNode::table_header(blocks)
            }
        } else if let Some(attrs) = adf_attrs {
            AdfNode::table_cell_with_attrs(blocks, attrs)
        } else {
            AdfNode::table_cell(blocks)
        };

        Ok((cell, i))
    }

    fn try_leaf_directive(&mut self) -> Option<AdfNode> {
        let line = self.current_line();
        let d = try_parse_leaf_directive(line)?;

        let node = match d.name.as_str() {
            "card" => {
                let url = d.content.as_deref().unwrap_or("");
                let mut node = AdfNode::block_card(url);
                // Pass through layout/width attrs
                if let Some(ref attrs) = d.attrs {
                    if let Some(ref mut node_attrs) = node.attrs {
                        if let Some(layout) = attrs.get("layout") {
                            node_attrs["layout"] = serde_json::Value::String(layout.to_string());
                        }
                        if let Some(width) = attrs.get("width") {
                            if let Ok(w) = width.parse::<u64>() {
                                node_attrs["width"] = serde_json::json!(w);
                            }
                        }
                    }
                }
                node
            }
            "embed" => {
                let url = d.content.as_deref().unwrap_or("");
                let layout = d.attrs.as_ref().and_then(|a| a.get("layout"));
                let original_height = d
                    .attrs
                    .as_ref()
                    .and_then(|a| a.get("originalHeight"))
                    .and_then(|v| v.parse::<f64>().ok());
                let width = d
                    .attrs
                    .as_ref()
                    .and_then(|a| a.get("width"))
                    .and_then(|w| w.parse::<f64>().ok());
                AdfNode::embed_card(url, layout, original_height, width)
            }
            "extension" => {
                let ext_type = d.attrs.as_ref().and_then(|a| a.get("type")).unwrap_or("");
                let ext_key = d.attrs.as_ref().and_then(|a| a.get("key")).unwrap_or("");
                let params = d
                    .attrs
                    .as_ref()
                    .and_then(|a| a.get("params"))
                    .and_then(|p| serde_json::from_str(p).ok());
                let mut node = AdfNode::extension(ext_type, ext_key, params);
                if let (Some(ref dir_attrs), Some(ref mut node_attrs)) = (&d.attrs, &mut node.attrs)
                {
                    if let Some(layout) = dir_attrs.get("layout") {
                        node_attrs["layout"] = serde_json::Value::String(layout.to_string());
                    }
                    if let Some(local_id) = dir_attrs.get("localId") {
                        node_attrs["localId"] = serde_json::Value::String(local_id.to_string());
                    }
                }
                node
            }
            "paragraph" => {
                let mut node = if let Some(ref text) = d.content {
                    AdfNode::paragraph(parse_inline(text))
                } else {
                    AdfNode::paragraph(vec![])
                };
                pass_through_local_id(&d.attrs, &mut node);
                node
            }
            _ => return None,
        };

        self.advance();
        Some(node)
    }

    fn try_image(&mut self) -> Option<AdfNode> {
        let line = self.current_line().trim();
        let node = try_parse_media_single_from_line(line)?;
        self.advance();
        Some(node)
    }

    fn try_table(&mut self) -> Result<Option<AdfNode>> {
        let line = self.current_line();
        if !line.contains('|') || !line.trim_start().starts_with('|') {
            return Ok(None);
        }

        // Peek ahead to check for a separator row (indicates a table)
        if self.pos + 1 >= self.lines.len() {
            return Ok(None);
        }
        let next_line = self.lines[self.pos + 1];
        if !is_table_separator(next_line) {
            return Ok(None);
        }

        // Parse header row
        let header_cells = parse_table_row(line);
        self.advance(); // skip header

        // Parse separator row for column alignment
        let sep_line = self.current_line();
        let alignments = parse_table_alignments(sep_line);
        self.advance(); // skip separator

        let mut rows = Vec::new();

        // Header row — parse cell attrs and apply column alignment
        let header_adf_cells: Vec<AdfNode> = header_cells
            .iter()
            .enumerate()
            .map(|(col_idx, cell)| {
                let (cell_text, cell_attrs) = extract_cell_attrs(cell);
                let mut para = AdfNode::paragraph(parse_inline(&cell_text));
                apply_column_alignment(&mut para, alignments.get(col_idx).copied().flatten());
                if let Some(attrs) = cell_attrs {
                    AdfNode::table_header_with_attrs(vec![para], attrs)
                } else {
                    AdfNode::table_header(vec![para])
                }
            })
            .collect();
        if header_adf_cells.is_empty() {
            warn!(
                "Pipe table header row at line {} has no cells",
                self.pos - 1
            );
        }
        rows.push(AdfNode::table_row(header_adf_cells));

        // Body rows
        while !self.at_end() {
            let line = self.current_line();
            if !line.contains('|') || line.trim().is_empty() {
                break;
            }

            let cells = parse_table_row(line);
            let adf_cells: Vec<AdfNode> = cells
                .iter()
                .enumerate()
                .map(|(col_idx, cell)| {
                    let (cell_text, cell_attrs) = extract_cell_attrs(cell);
                    let mut para = AdfNode::paragraph(parse_inline(&cell_text));
                    apply_column_alignment(&mut para, alignments.get(col_idx).copied().flatten());
                    if let Some(attrs) = cell_attrs {
                        AdfNode::table_cell_with_attrs(vec![para], attrs)
                    } else {
                        AdfNode::table_cell(vec![para])
                    }
                })
                .collect();
            if adf_cells.is_empty() {
                warn!("Pipe table body row at line {} has no cells", self.pos);
            }
            rows.push(AdfNode::table_row(adf_cells));
            self.advance();
        }

        debug!("Parsed pipe table with {} rows", rows.len());
        let mut table = AdfNode::table(rows);

        // Check for trailing {attrs} on the next line
        if !self.at_end() {
            let next = self.current_line().trim();
            if next.starts_with('{') {
                if let Some((_, attrs)) = parse_attrs(next, 0) {
                    let mut table_attrs = serde_json::json!({});
                    if let Some(layout) = attrs.get("layout") {
                        table_attrs["layout"] = serde_json::Value::String(layout.to_string());
                    }
                    if attrs.has_flag("numbered") {
                        table_attrs["isNumberColumnEnabled"] = serde_json::json!(true);
                    } else if attrs.get("numbered") == Some("false") {
                        table_attrs["isNumberColumnEnabled"] = serde_json::json!(false);
                    }
                    if let Some(tw) = attrs.get("width") {
                        if let Ok(w) = tw.parse::<f64>() {
                            table_attrs["width"] = serde_json::json!(w);
                        }
                    }
                    if let Some(local_id) = attrs.get("localId") {
                        table_attrs["localId"] = serde_json::Value::String(local_id.to_string());
                    }
                    if table_attrs != serde_json::json!({}) {
                        table.attrs = Some(table_attrs);
                        self.advance(); // consume the attrs line
                    }
                }
            }
        }

        Ok(Some(table))
    }

    fn parse_paragraph(&mut self) -> Result<AdfNode> {
        let mut lines = Vec::new();

        while !self.at_end() {
            let line = self.current_line();
            // Only break on block-level patterns if we already have paragraph
            // content. This prevents infinite loops when a line looks like a
            // block starter but doesn't actually match any block parser (e.g.,
            // "#NoSpace" which is not a valid heading).
            if line.trim().is_empty()
                || line.starts_with("```")
                || (is_horizontal_rule(line) && !lines.is_empty())
            {
                break;
            }
            if !lines.is_empty()
                && (line.starts_with('#') || line.starts_with('>') || is_list_start(line))
            {
                break;
            }
            // Break on trailing block attrs like {align=center}
            if !lines.is_empty() && is_block_attrs_line(line) {
                break;
            }
            lines.push(line);
            self.advance();
        }

        let text = lines.join("\n");
        let inline_nodes = parse_inline(&text);
        Ok(AdfNode::paragraph(inline_nodes))
    }
}

/// Builds ADF cell attributes from JFM directive attrs.
/// Maps: `bg` → `background`, `colspan` → number, `rowspan` → number, `colwidth` → array.
fn build_cell_attrs(attrs: &crate::atlassian::attrs::Attrs) -> serde_json::Value {
    let mut adf = serde_json::json!({});
    if let Some(bg) = attrs.get("bg") {
        adf["background"] = serde_json::Value::String(bg.to_string());
    }
    if let Some(colspan) = attrs.get("colspan") {
        if let Ok(n) = colspan.parse::<u32>() {
            adf["colspan"] = serde_json::json!(n);
        }
    }
    if let Some(rowspan) = attrs.get("rowspan") {
        if let Ok(n) = rowspan.parse::<u32>() {
            adf["rowspan"] = serde_json::json!(n);
        }
    }
    if let Some(colwidth) = attrs.get("colwidth") {
        let widths: Vec<serde_json::Value> = colwidth
            .split(',')
            .filter_map(|s| s.trim().parse::<f64>().ok())
            .map(|n| serde_json::json!(n))
            .collect();
        if !widths.is_empty() {
            adf["colwidth"] = serde_json::Value::Array(widths);
        }
    }
    if let Some(local_id) = attrs.get("localId") {
        adf["localId"] = serde_json::Value::String(local_id.to_string());
    }
    adf
}

/// Converts an ISO 8601 date string (e.g., "2026-04-15") to epoch milliseconds string.
/// If the input is already numeric (epoch ms), returns it unchanged.
fn iso_date_to_epoch_ms(date_str: &str) -> String {
    // If it's already a numeric timestamp, pass through
    if date_str.chars().all(|c| c.is_ascii_digit()) {
        return date_str.to_string();
    }
    if let Ok(date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
        let epoch_ms = date
            .and_hms_opt(0, 0, 0)
            .map_or(0, |dt| dt.and_utc().timestamp_millis());
        epoch_ms.to_string()
    } else {
        // Fallback: pass through as-is
        date_str.to_string()
    }
}

/// Converts an epoch milliseconds string to an ISO 8601 date string.
/// If the input looks like an ISO date already, returns it unchanged.
fn epoch_ms_to_iso_date(timestamp: &str) -> String {
    // If it looks like an ISO date already, pass through
    if timestamp.contains('-') {
        return timestamp.to_string();
    }
    if let Ok(ms) = timestamp.parse::<i64>() {
        let secs = ms / 1000;
        if let Some(dt) = chrono::DateTime::from_timestamp(secs, 0) {
            return dt.format("%Y-%m-%d").to_string();
        }
    }
    // Fallback: pass through
    timestamp.to_string()
}

/// Checks if a line is a standalone block-level attrs line like `{align=center}`.
fn is_block_attrs_line(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('{') || !trimmed.ends_with('}') {
        return false;
    }
    if let Some((_, attrs)) = parse_attrs(trimmed, 0) {
        // Only consider it a block attrs line if it has recognized block attrs
        attrs.get("align").is_some()
            || attrs.get("indent").is_some()
            || attrs.get("breakout").is_some()
            || attrs.get("breakoutWidth").is_some()
            || attrs.get("localId").is_some()
    } else {
        false
    }
}

/// Parses decision items from the inner content of a `:::decisions` container.
/// Each item starts with `- <> ` prefix.
fn parse_decision_items(text: &str) -> Vec<AdfNode> {
    let mut items = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("- <> ") {
            let inline_nodes = parse_inline(rest);
            items.push(AdfNode::decision_item(
                "DECIDED",
                vec![AdfNode::paragraph(inline_nodes)],
            ));
        }
    }
    items
}

/// Tries to parse a task list marker `[ ] ` or `[x] ` at the start of text.
/// Returns `("TODO"|"DONE", remaining_text)` on success.
fn try_parse_task_marker(text: &str) -> Option<(&str, &str)> {
    if let Some(rest) = text.strip_prefix("[ ] ") {
        Some(("TODO", rest))
    } else if let Some(rest) = text
        .strip_prefix("[x] ")
        .or_else(|| text.strip_prefix("[X] "))
    {
        Some(("DONE", rest))
    } else {
        None
    }
}

/// Parses an ordered list marker like "1. " and returns (number, rest_of_line).
fn parse_ordered_list_marker(line: &str) -> Option<(u32, &str)> {
    let digit_end = line.find(|c: char| !c.is_ascii_digit())?;
    if digit_end == 0 {
        return None;
    }
    let rest = &line[digit_end..];
    let after_marker = rest.strip_prefix(". ")?;
    let num: u32 = line[..digit_end].parse().ok()?;
    Some((num, after_marker))
}

/// Returns true if a line ends with a hardBreak marker
/// (trailing backslash or two trailing spaces).
fn has_trailing_hard_break(line: &str) -> bool {
    line.ends_with('\\') || line.ends_with("  ")
}

/// Checks if a line starts a list item.
fn is_list_start(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
        || trimmed.starts_with("+ ")
        || parse_ordered_list_marker(trimmed).is_some()
}

/// Escapes asterisk sequences in text that would otherwise be parsed as
/// CommonMark emphasis (`*…*`) or strong emphasis (`**…**`).
///
/// Only sequences that could round-trip as emphasis are escaped: a `*` or
/// `**` that is followed (at the opening position) or preceded (at the
/// closing position) by a non-space character.  Lone asterisks that cannot
/// form a delimiter pair are left untouched.
fn escape_emphasis_markers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '*' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Escapes backtick characters in text that would otherwise be parsed as
/// inline code spans (`` `…` ``).
///
/// Each backtick is prefixed with a backslash so that the JFM parser treats
/// it as a literal character rather than an inline-code delimiter.
fn escape_backticks(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '`' {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Escapes a leading list-marker pattern on a line so it is not
/// re-parsed as a new list item.  `"2. text"` → `"2\. text"`,
/// `"- text"` → `"\- text"`.
fn escape_list_marker(line: &str) -> String {
    if let Some(dot_pos) = line.find(". ") {
        if parse_ordered_list_marker(line).is_some() {
            let mut s = String::with_capacity(line.len() + 1);
            s.push_str(&line[..dot_pos]);
            s.push('\\');
            s.push_str(&line[dot_pos..]);
            return s;
        }
    }
    for prefix in &["- ", "* ", "+ "] {
        if line.starts_with(prefix) {
            let mut s = String::with_capacity(line.len() + 1);
            s.push('\\');
            s.push_str(line);
            return s;
        }
    }
    line.to_string()
}

/// Checks if a line is a horizontal rule.
fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= 3
        && ((trimmed.starts_with("---") && trimmed.chars().all(|c| c == '-'))
            || (trimmed.starts_with("***") && trimmed.chars().all(|c| c == '*'))
            || (trimmed.starts_with("___") && trimmed.chars().all(|c| c == '_')))
}

/// Checks if a line is a GFM table separator (e.g., "|---|---|").
fn is_table_separator(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains('|')
        && trimmed
            .chars()
            .all(|c| c == '|' || c == '-' || c == ':' || c == ' ')
}

/// Parses a GFM table row into cell contents.
fn parse_table_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let trimmed = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix('|').unwrap_or(trimmed);

    trimmed
        .split('|')
        .map(|s| {
            // Strip exactly one leading and one trailing space (pipe table padding).
            // Preserve any additional whitespace as significant content.
            let s = s.strip_prefix(' ').unwrap_or(s);
            let s = s.strip_suffix(' ').unwrap_or(s);
            s.to_string()
        })
        .collect()
}

/// Parses column alignments from a GFM table separator row.
/// Returns a vec of `Option<&str>` where `Some("center")` or `Some("end")` indicate alignment.
fn parse_table_alignments(separator_line: &str) -> Vec<Option<&'static str>> {
    let trimmed = separator_line.trim();
    let trimmed = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let trimmed = trimmed.strip_suffix('|').unwrap_or(trimmed);

    trimmed
        .split('|')
        .map(|cell| {
            let cell = cell.trim();
            let starts_colon = cell.starts_with(':');
            let ends_colon = cell.ends_with(':');
            match (starts_colon, ends_colon) {
                (true, true) => Some("center"),
                (false, true) => Some("end"),
                _ => None, // left/default
            }
        })
        .collect()
}

/// Applies an alignment mark to a paragraph node if alignment is specified.
fn apply_column_alignment(para: &mut AdfNode, alignment: Option<&str>) {
    if let Some(align) = alignment {
        para.marks = Some(vec![AdfMark::alignment(align)]);
    }
}

/// Extracts `{attrs}` prefix from a pipe table cell text.
/// Returns `(remaining_text, Option<adf_attrs_json>)`.
fn extract_cell_attrs(cell_text: &str) -> (String, Option<serde_json::Value>) {
    let trimmed = cell_text.trim_start();
    if !trimmed.starts_with('{') {
        return (cell_text.to_string(), None);
    }
    if let Some((end_pos, attrs)) = parse_attrs(trimmed, 0) {
        let remaining = trimmed[end_pos..].trim_start().to_string();
        let adf_attrs = build_cell_attrs(&attrs);
        (remaining, Some(adf_attrs))
    } else {
        (cell_text.to_string(), None)
    }
}

/// Tries to parse a line as a block-level image and return a mediaSingle ADF node.
/// Used by both `try_image` (top-level blocks) and list item parsing.
fn try_parse_media_single_from_line(line: &str) -> Option<AdfNode> {
    let line = line.trim();
    if !line.starts_with("![") {
        return None;
    }

    let (alt, url) = parse_image_syntax(line)?;
    let alt_opt = if alt.is_empty() { None } else { Some(alt) };

    let img_end = line.find(')').unwrap_or(line.len()) + 1;
    let after_img = line[img_end..].trim_start();

    if after_img.starts_with('{') {
        if let Some((_, attrs)) = parse_attrs(after_img, 0) {
            // Confluence file attachment — reconstruct type:file media node
            if attrs.get("type") == Some("file") || attrs.get("id").is_some() {
                let mut media_attrs = serde_json::json!({"type": "file"});
                if let Some(id) = attrs.get("id") {
                    media_attrs["id"] = serde_json::Value::String(id.to_string());
                }
                if let Some(collection) = attrs.get("collection") {
                    media_attrs["collection"] = serde_json::Value::String(collection.to_string());
                }
                if let Some(height) = attrs.get("height") {
                    if let Ok(h) = height.parse::<u64>() {
                        media_attrs["height"] = serde_json::json!(h);
                    }
                }
                if let Some(width) = attrs.get("width") {
                    if let Ok(w) = width.parse::<u64>() {
                        media_attrs["width"] = serde_json::json!(w);
                    }
                }
                if let Some(alt_text) = alt_opt {
                    media_attrs["alt"] = serde_json::Value::String(alt_text.to_string());
                }
                if let Some(local_id) = attrs.get("localId") {
                    media_attrs["localId"] = serde_json::Value::String(local_id.to_string());
                }
                let mut ms_attrs = serde_json::json!({"layout": "center"});
                if let Some(layout) = attrs.get("layout") {
                    ms_attrs["layout"] = serde_json::Value::String(layout.to_string());
                }
                if let Some(ms_width) = attrs.get("mediaWidth") {
                    if let Ok(w) = ms_width.parse::<u64>() {
                        ms_attrs["width"] = serde_json::json!(w);
                    }
                }
                if let Some(wt) = attrs.get("widthType") {
                    ms_attrs["widthType"] = serde_json::Value::String(wt.to_string());
                }
                if let Some(mode) = attrs.get("mode") {
                    ms_attrs["mode"] = serde_json::Value::String(mode.to_string());
                }
                return Some(AdfNode {
                    node_type: "mediaSingle".to_string(),
                    attrs: Some(ms_attrs),
                    content: Some(vec![AdfNode {
                        node_type: "media".to_string(),
                        attrs: Some(media_attrs),
                        content: None,
                        text: None,
                        marks: None,
                        local_id: None,
                        parameters: None,
                    }]),
                    text: None,
                    marks: None,
                    local_id: None,
                    parameters: None,
                });
            }

            // External image — apply layout/width/widthType to mediaSingle attrs
            let mut node = AdfNode::media_single(url, alt_opt);
            if let Some(ref mut node_attrs) = node.attrs {
                if let Some(layout) = attrs.get("layout") {
                    node_attrs["layout"] = serde_json::Value::String(layout.to_string());
                }
                if let Some(width) = attrs.get("width") {
                    if let Ok(w) = width.parse::<u64>() {
                        node_attrs["width"] = serde_json::json!(w);
                    }
                }
                if let Some(wt) = attrs.get("widthType") {
                    node_attrs["widthType"] = serde_json::Value::String(wt.to_string());
                }
                if let Some(mode) = attrs.get("mode") {
                    node_attrs["mode"] = serde_json::Value::String(mode.to_string());
                }
            }
            if let Some(local_id) = attrs.get("localId") {
                if let Some(ref mut content) = node.content {
                    if let Some(media) = content.first_mut() {
                        if let Some(ref mut media_attrs) = media.attrs {
                            media_attrs["localId"] =
                                serde_json::Value::String(local_id.to_string());
                        }
                    }
                }
            }
            return Some(node);
        }
    }

    Some(AdfNode::media_single(url, alt_opt))
}

/// Parses `![alt](url)` image syntax.
fn parse_image_syntax(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if !line.starts_with("![") {
        return None;
    }

    let alt_end = line.find("](")?;
    let alt = &line[2..alt_end];
    let url_start = alt_end + 2;
    let url_end = line[url_start..].find(')')? + url_start;
    let url = &line[url_start..url_end];

    Some((alt, url))
}

// ── Inline Parsing ──────────────────────────────────────────────────

/// Parses inline markdown content into ADF inline nodes.
fn parse_inline(text: &str) -> Vec<AdfNode> {
    let mut nodes = Vec::new();
    let mut chars = text.char_indices().peekable();
    let mut plain_start = 0;

    while let Some(&(i, ch)) = chars.peek() {
        match ch {
            '*' | '_' => {
                if let Some((end, content, is_bold)) = try_parse_emphasis(text, i) {
                    flush_plain(text, plain_start, i, &mut nodes);
                    let mark = if is_bold {
                        AdfMark::strong()
                    } else {
                        AdfMark::em()
                    };
                    let inner = parse_inline(content);
                    for mut node in inner {
                        prepend_mark(&mut node, mark.clone());
                        nodes.push(node);
                    }
                    // Advance past the consumed characters
                    while chars.peek().is_some_and(|&(idx, _)| idx < end) {
                        chars.next();
                    }
                    plain_start = end;
                    continue;
                }
                // For underscores, skip the entire delimiter run so that
                // individual `_` chars within a `__` or `___` run are not
                // re-tried as separate emphasis openers (CommonMark treats
                // consecutive underscores as a single delimiter run).
                if ch == '_' {
                    while chars.peek().is_some_and(|&(_, c)| c == '_') {
                        chars.next();
                    }
                } else {
                    chars.next();
                }
            }
            '~' => {
                if let Some((end, content)) = try_parse_strikethrough(text, i) {
                    flush_plain(text, plain_start, i, &mut nodes);
                    let inner = parse_inline(content);
                    for mut node in inner {
                        prepend_mark(&mut node, AdfMark::strike());
                        nodes.push(node);
                    }
                    while chars.peek().is_some_and(|&(idx, _)| idx < end) {
                        chars.next();
                    }
                    plain_start = end;
                    continue;
                }
                chars.next();
            }
            '`' => {
                if let Some((end, content)) = try_parse_inline_code(text, i) {
                    flush_plain(text, plain_start, i, &mut nodes);
                    nodes.push(AdfNode::text_with_marks(content, vec![AdfMark::code()]));
                    while chars.peek().is_some_and(|&(idx, _)| idx < end) {
                        chars.next();
                    }
                    plain_start = end;
                    continue;
                }
                chars.next();
            }
            '[' => {
                if let Some((end, link_text, href)) = try_parse_link(text, i) {
                    flush_plain(text, plain_start, i, &mut nodes);
                    if link_text == href {
                        // Bare URL link [url](url): emit as text with link mark,
                        // not via parse_inline which would produce an inlineCard.
                        nodes.push(AdfNode::text_with_marks(
                            link_text,
                            vec![AdfMark::link(href)],
                        ));
                    } else {
                        let inner = parse_inline(link_text);
                        for mut node in inner {
                            prepend_mark(&mut node, AdfMark::link(href));
                            nodes.push(node);
                        }
                    }
                    while chars.peek().is_some_and(|&(idx, _)| idx < end) {
                        chars.next();
                    }
                    plain_start = end;
                    continue;
                }
                // Try bracketed span with attributes: [text]{underline}
                if let Some((end, span_nodes)) = try_parse_bracketed_span(text, i) {
                    flush_plain(text, plain_start, i, &mut nodes);
                    nodes.extend(span_nodes);
                    while chars.peek().is_some_and(|&(idx, _)| idx < end) {
                        chars.next();
                    }
                    plain_start = end;
                    continue;
                }
                chars.next();
            }
            ':' => {
                // Try generic inline directive (:card[url], :status[text]{attrs}, etc.)
                if let Some(node) = try_dispatch_inline_directive(text, i) {
                    flush_plain(text, plain_start, i, &mut nodes);
                    let end = node.1;
                    nodes.push(node.0);
                    while chars.peek().is_some_and(|&(idx, _)| idx < end) {
                        chars.next();
                    }
                    plain_start = end;
                    continue;
                }
                // Try emoji shortcode :name: with optional {attrs}
                if let Some((end, short_name)) = try_parse_emoji_shortcode(text, i) {
                    flush_plain(text, plain_start, i, &mut nodes);
                    let (final_end, emoji_node) = parse_emoji_with_attrs(text, end, short_name);
                    nodes.push(emoji_node);
                    while chars.peek().is_some_and(|&(idx, _)| idx < final_end) {
                        chars.next();
                    }
                    plain_start = final_end;
                    continue;
                }
                chars.next();
            }
            ' ' if text[i..].starts_with("  \n") => {
                // Trailing-space line break → hardBreak node.
                // Flush preceding text (without the trailing spaces).
                flush_plain(text, plain_start, i, &mut nodes);
                nodes.push(AdfNode::hard_break());
                // Skip past all spaces and the newline
                while chars.peek().is_some_and(|&(_, c)| c == ' ') {
                    chars.next();
                }
                // Skip the newline
                if chars.peek().is_some_and(|&(_, c)| c == '\n') {
                    chars.next();
                }
                plain_start = chars.peek().map_or(text.len(), |&(idx, _)| idx);
            }
            '!' if text[i..].starts_with("![") => {
                // Inline image — skip the ! and let [ handle it next iteration
                // (Images at block level are handled by try_image; inline images
                // degrade to link text in ADF since inline media is complex)
                chars.next();
            }
            'h' if text[i..].starts_with("http://") || text[i..].starts_with("https://") => {
                if let Some((end, url)) = try_parse_bare_url(text, i) {
                    flush_plain(text, plain_start, i, &mut nodes);
                    nodes.push(AdfNode::inline_card(url));
                    while chars.peek().is_some_and(|&(idx, _)| idx < end) {
                        chars.next();
                    }
                    plain_start = end;
                    continue;
                }
                chars.next();
            }
            '\\' if i + 1 < text.len()
                && !text[i..].starts_with("\\\n")
                && text.as_bytes()[i + 1] != b'\\' =>
            {
                // Backslash escape: skip the backslash and treat the next
                // character as literal text (e.g. `2\. text` → `2. text`,
                // `\*word\*` → `*word*` without emphasis).
                flush_plain(text, plain_start, i, &mut nodes);
                chars.next(); // consume the backslash
                              // Set plain_start to the escaped character so it is included
                              // in the next plain-text run, then advance past it so it is
                              // not re-interpreted as a special character (e.g. `*`, `_`).
                plain_start = chars.peek().map_or(text.len(), |&(idx, _)| idx);
                chars.next(); // consume the escaped character
            }
            '\\' if text[i..].starts_with("\\\n") => {
                // Backslash line break → hardBreak node.
                flush_plain(text, plain_start, i, &mut nodes);
                nodes.push(AdfNode::hard_break());
                chars.next(); // consume the '\'
                              // Skip the newline
                if chars.peek().is_some_and(|&(_, c)| c == '\n') {
                    chars.next();
                }
                plain_start = chars.peek().map_or(text.len(), |&(idx, _)| idx);
            }
            '\\' if i + 1 == text.len() => {
                // Trailing backslash at end of paragraph text → hardBreak node.
                flush_plain(text, plain_start, i, &mut nodes);
                nodes.push(AdfNode::hard_break());
                chars.next(); // consume the '\'
                plain_start = text.len();
            }
            _ => {
                chars.next();
            }
        }
    }

    // Flush remaining plain text
    if plain_start < text.len() {
        let remaining = &text[plain_start..];
        if !remaining.is_empty() {
            nodes.push(AdfNode::text(remaining));
        }
    }

    // Merge adjacent unmarked text nodes that can arise from backslash
    // escape handling (e.g. `"2"` + `". text"` → `"2. text"`).
    merge_adjacent_text(&mut nodes);

    nodes
}

/// Merges consecutive unmarked text nodes in-place.
fn merge_adjacent_text(nodes: &mut Vec<AdfNode>) {
    let mut i = 0;
    while i + 1 < nodes.len() {
        if nodes[i].node_type == "text"
            && nodes[i + 1].node_type == "text"
            && nodes[i].marks.is_none()
            && nodes[i + 1].marks.is_none()
        {
            let next_text = nodes[i + 1].text.clone().unwrap_or_default();
            if let Some(ref mut t) = nodes[i].text {
                t.push_str(&next_text);
            }
            nodes.remove(i + 1);
        } else {
            i += 1;
        }
    }
}

/// Flushes accumulated plain text as a text node.
fn flush_plain(text: &str, start: usize, end: usize, nodes: &mut Vec<AdfNode>) {
    if start < end {
        let plain = &text[start..end];
        if !plain.is_empty() {
            nodes.push(AdfNode::text(plain));
        }
    }
}

/// Adds a mark to a node (creates marks vec if needed).
#[cfg(test)]
fn add_mark(node: &mut AdfNode, mark: AdfMark) {
    if let Some(ref mut marks) = node.marks {
        marks.push(mark);
    } else {
        node.marks = Some(vec![mark]);
    }
}

/// Prepends a mark before existing marks to preserve outside-in ordering.
fn prepend_mark(node: &mut AdfNode, mark: AdfMark) {
    if let Some(ref mut marks) = node.marks {
        marks.insert(0, mark);
    } else {
        node.marks = Some(vec![mark]);
    }
}

/// Returns `true` when an underscore delimiter run of `len` bytes starting at
/// byte position `delim_pos` in `text` is flanked by alphanumeric characters on
/// **both** sides — meaning it sits inside a word and must NOT open or close an
/// emphasis span per CommonMark.
fn is_intraword_underscore(text: &str, delim_pos: usize, len: usize) -> bool {
    let before = text[..delim_pos]
        .chars()
        .next_back()
        .is_some_and(char::is_alphanumeric);
    let after = text[delim_pos + len..]
        .chars()
        .next()
        .is_some_and(char::is_alphanumeric);
    before && after
}

/// Finds the first occurrence of `needle` in `haystack`, skipping over
/// backslash-escaped characters (e.g. `\*` is not matched when searching
/// for `*`).
fn find_unescaped(haystack: &str, needle: &str) -> Option<usize> {
    let needle_bytes = needle.as_bytes();
    let hay_bytes = haystack.as_bytes();
    let mut i = 0;
    while i < hay_bytes.len() {
        if hay_bytes[i] == b'\\' {
            i += 2; // skip escaped character
            continue;
        }
        if hay_bytes[i..].starts_with(needle_bytes) {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Finds the first occurrence of a single byte `ch` in `haystack`, skipping
/// over backslash-escaped characters.
fn find_unescaped_char(haystack: &str, ch: u8) -> Option<usize> {
    let hay_bytes = haystack.as_bytes();
    let mut i = 0;
    while i < hay_bytes.len() {
        if hay_bytes[i] == b'\\' {
            i += 2;
            continue;
        }
        if hay_bytes[i] == ch {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Tries to parse ***bold+italic***, **bold**, *italic* (or underscore variants) starting at position `i`.
/// Returns (end_position, inner_content, is_bold).
///
/// The triple-delimiter case (`***` / `___`) is checked first so that `***text***` is parsed as
/// bold wrapping italic content, rather than having the `**` branch consume the wrong closing
/// delimiter and leave stray `*` characters in the text (see issue #401).
///
/// For underscore delimiters, intraword positions are rejected per CommonMark: a `_` flanked
/// by alphanumeric characters on both sides must not open or close emphasis (see issue #438).
fn try_parse_emphasis(text: &str, i: usize) -> Option<(usize, &str, bool)> {
    let rest = &text[i..];

    // Bold+italic: *** or ___
    // Parse as bold wrapping italic: the inner content will be recursively parsed and pick up
    // the inner * / _ as an em mark.
    if rest.starts_with("***") || rest.starts_with("___") {
        let is_underscore = rest.starts_with("___");
        if is_underscore && is_intraword_underscore(text, i, 3) {
            return None;
        }
        let triple = &rest[..3];
        let after = &rest[3..];
        if let Some(close) = find_unescaped(after, triple) {
            if close > 0 {
                let close_pos = i + 3 + close;
                if is_underscore && is_intraword_underscore(text, close_pos, 3) {
                    return None;
                }
                // Return a slice that includes the inner italic delimiters from the
                // original text: for `***text***`, return `*text*`.  The recursive
                // parse_inline call will then pick up the inner `*…*` as an em mark.
                let content = &rest[2..=3 + close];
                let end = i + 3 + close + 3;
                return Some((end, content, true));
            }
        }
    }

    // Bold: ** or __
    if rest.starts_with("**") || rest.starts_with("__") {
        let is_underscore = rest.starts_with("__");
        if is_underscore && is_intraword_underscore(text, i, 2) {
            return None;
        }
        let delimiter = &rest[..2];
        let after = &rest[2..];
        let close = find_unescaped(after, delimiter)?;
        if close == 0 {
            return None;
        }
        let close_pos = i + 2 + close;
        if is_underscore && is_intraword_underscore(text, close_pos, 2) {
            return None;
        }
        let content = &after[..close];
        let end = i + 2 + close + 2;
        return Some((end, content, true));
    }

    // Italic: * or _
    if rest.starts_with('*') || rest.starts_with('_') {
        let delim_char = rest.as_bytes()[0];
        let is_underscore = delim_char == b'_';
        if is_underscore && is_intraword_underscore(text, i, 1) {
            return None;
        }
        let after = &rest[1..];
        let close = find_unescaped_char(after, delim_char)?;
        if close == 0 {
            return None;
        }
        let close_pos = i + 1 + close;
        if is_underscore && is_intraword_underscore(text, close_pos, 1) {
            return None;
        }
        let content = &after[..close];
        let end = i + 1 + close + 1;
        return Some((end, content, false));
    }

    None
}

/// Tries to parse ~~strikethrough~~ starting at position `i`.
fn try_parse_strikethrough(text: &str, i: usize) -> Option<(usize, &str)> {
    let rest = &text[i..];
    if !rest.starts_with("~~") {
        return None;
    }
    let after = &rest[2..];
    let close = after.find("~~")?;
    if close == 0 {
        return None;
    }
    let content = &after[..close];
    Some((i + 2 + close + 2, content))
}

/// Tries to parse `inline code` starting at position `i`.
fn try_parse_inline_code(text: &str, i: usize) -> Option<(usize, &str)> {
    let rest = &text[i..];
    if !rest.starts_with('`') {
        return None;
    }
    let after = &rest[1..];
    let close = after.find('`')?;
    let content = &after[..close];
    Some((i + 1 + close + 1, content))
}

/// Tries to parse a bracketed span `[text]{attrs}` starting at position `i`.
/// Used for `[text]{underline}` and similar constructs.
fn try_parse_bracketed_span(text: &str, i: usize) -> Option<(usize, Vec<AdfNode>)> {
    let rest = &text[i..];
    if !rest.starts_with('[') {
        return None;
    }

    // Find the matching ] by counting bracket depth (supports nested brackets
    // such as [[text](url)]{underline} for underline-before-link ordering).
    let mut depth: usize = 0;
    let mut bracket_close = None;
    for (j, ch) in rest.char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    bracket_close = Some(j);
                    break;
                }
            }
            _ => {}
        }
    }
    let bracket_close = bracket_close?;
    // Make sure this isn't a link: next char after ] must be { not (
    let after_bracket = &rest[bracket_close + 1..];
    if !after_bracket.starts_with('{') {
        return None;
    }

    let span_text = &rest[1..bracket_close];
    let attrs_start = i + bracket_close + 1;
    let (attrs_end, attrs) = parse_attrs(text, attrs_start)?;

    let mut marks = Vec::new();
    if attrs.has_flag("underline") {
        marks.push(AdfMark::underline());
    }
    let ann_ids = attrs.get_all("annotation-id");
    let ann_types = attrs.get_all("annotation-type");
    for (idx, ann_id) in ann_ids.iter().enumerate() {
        let ann_type = ann_types.get(idx).copied().unwrap_or("inlineComment");
        marks.push(AdfMark::annotation(ann_id, ann_type));
    }

    if marks.is_empty() {
        return None; // no recognized marks
    }

    let inner = parse_inline(span_text);
    let result: Vec<AdfNode> = inner
        .into_iter()
        .map(|mut node| {
            // Prepend bracket marks before inner marks to preserve original
            // ADF mark ordering (e.g., [underline, strong] not [strong, underline]).
            let mut combined = marks.clone();
            if let Some(ref existing) = node.marks {
                combined.extend(existing.iter().cloned());
            }
            node.marks = Some(combined);
            node
        })
        .collect();

    Some((attrs_end, result))
}

/// Dispatches an inline directive to the appropriate ADF node constructor.
/// Returns `(AdfNode, end_pos)` on success.
fn try_dispatch_inline_directive(text: &str, pos: usize) -> Option<(AdfNode, usize)> {
    let d = try_parse_inline_directive(text, pos)?;
    let content = d.content.as_deref().unwrap_or("");

    let node = match d.name.as_str() {
        "card" => {
            let mut node = AdfNode::inline_card(content);
            pass_through_local_id(&d.attrs, &mut node);
            node
        }
        "status" => {
            let color = d
                .attrs
                .as_ref()
                .and_then(|a| a.get("color"))
                .unwrap_or("neutral");
            let mut node = AdfNode::status(content, color);
            // Pass through style and localId if present
            if let Some(ref attrs) = d.attrs {
                if let Some(ref mut node_attrs) = node.attrs {
                    if let Some(style) = attrs.get("style") {
                        node_attrs["style"] = serde_json::Value::String(style.to_string());
                    }
                    if let Some(local_id) = attrs.get("localId") {
                        node_attrs["localId"] = serde_json::Value::String(local_id.to_string());
                    }
                }
            }
            node
        }
        "date" => {
            let timestamp = d
                .attrs
                .as_ref()
                .and_then(|a| a.get("timestamp"))
                .map_or_else(|| iso_date_to_epoch_ms(content), ToString::to_string);
            let mut node = AdfNode::date(&timestamp);
            pass_through_local_id(&d.attrs, &mut node);
            node
        }
        "mention" => {
            let id = d.attrs.as_ref().and_then(|a| a.get("id")).unwrap_or("");
            let mut node = AdfNode::mention(id, content);
            // Pass through optional userType and accessLevel
            if let Some(ref attrs) = d.attrs {
                if let (Some(ref mut node_attrs), true) = (
                    &mut node.attrs,
                    attrs.get("userType").is_some() || attrs.get("accessLevel").is_some(),
                ) {
                    if let Some(ut) = attrs.get("userType") {
                        node_attrs["userType"] = serde_json::Value::String(ut.to_string());
                    }
                    if let Some(al) = attrs.get("accessLevel") {
                        node_attrs["accessLevel"] = serde_json::Value::String(al.to_string());
                    }
                }
            }
            pass_through_local_id(&d.attrs, &mut node);
            node
        }
        "span" => {
            let mut marks = Vec::new();
            if let Some(ref attrs) = d.attrs {
                if let Some(color) = attrs.get("color") {
                    marks.push(AdfMark::text_color(color));
                }
                if let Some(bg) = attrs.get("bg") {
                    marks.push(AdfMark::background_color(bg));
                }
                if attrs.has_flag("sub") {
                    marks.push(AdfMark::subsup("sub"));
                }
                if attrs.has_flag("sup") {
                    marks.push(AdfMark::subsup("sup"));
                }
            }
            if marks.is_empty() {
                AdfNode::text(content)
            } else {
                // Parse inner content to handle nested syntax (e.g., links).
                // Prepend span marks before inner marks to preserve ordering.
                let inner = parse_inline(content);
                let mut nodes: Vec<AdfNode> = inner
                    .into_iter()
                    .map(|mut node| {
                        let mut combined = marks.clone();
                        if let Some(ref existing) = node.marks {
                            combined.extend(existing.iter().cloned());
                        }
                        node.marks = Some(combined);
                        node
                    })
                    .collect();
                // Return the first marked node (typical case is a single node).
                nodes.remove(0)
            }
        }
        "placeholder" => AdfNode::placeholder(content),
        "extension" => {
            let ext_type = d.attrs.as_ref().and_then(|a| a.get("type")).unwrap_or("");
            let ext_key = d.attrs.as_ref().and_then(|a| a.get("key")).unwrap_or("");
            AdfNode::inline_extension(ext_type, ext_key, Some(content))
        }
        _ => return None, // unknown directive — fall through to plain text
    };

    Some((node, d.end_pos))
}

/// Tries to parse a bare URL (`http://` or `https://`) starting at position `i`.
/// Scans forward until whitespace, `)`, `]`, or end of string.
fn try_parse_bare_url(text: &str, i: usize) -> Option<(usize, &str)> {
    let rest = &text[i..];
    if !rest.starts_with("http://") && !rest.starts_with("https://") {
        return None;
    }
    // URL extends to the next whitespace or delimiter
    let end = rest
        .find(|c: char| c.is_whitespace() || c == ')' || c == ']' || c == '>')
        .unwrap_or(rest.len());
    // Strip trailing punctuation that's likely not part of the URL
    let url = rest[..end].trim_end_matches(['.', ',', ';', '!', '?']);
    if url.len() <= "https://".len() {
        return None; // too short to be a real URL
    }
    Some((i + url.len(), url))
}

/// Tries to parse an emoji shortcode `:name:` starting at position `i`.
/// The name must match `[a-zA-Z0-9_+-]+`.
fn try_parse_emoji_shortcode(text: &str, i: usize) -> Option<(usize, &str)> {
    let rest = &text[i..];
    if !rest.starts_with(':') {
        return None;
    }
    let after = &rest[1..];
    let name_end =
        after.find(|c: char| !c.is_alphanumeric() && c != '_' && c != '+' && c != '-')?;
    if name_end == 0 {
        return None;
    }
    if after.as_bytes().get(name_end) != Some(&b':') {
        return None;
    }
    let name = &after[..name_end];
    Some((i + 1 + name_end + 1, name))
}

/// Parses an emoji shortcode that has already been matched, then checks for
/// trailing `{id="..." text="..."}` attributes to preserve round-trip fidelity.
fn parse_emoji_with_attrs(text: &str, shortcode_end: usize, short_name: &str) -> (usize, AdfNode) {
    if let Some((attr_end, attrs)) = parse_attrs(text, shortcode_end) {
        // Use the explicit shortName attr if provided (preserves original form),
        // otherwise fall back to colon-wrapped name.
        let resolved_name = attrs
            .get("shortName")
            .map_or_else(|| format!(":{short_name}:"), str::to_string);
        let mut emoji_attrs = serde_json::json!({"shortName": resolved_name});
        if let Some(id) = attrs.get("id") {
            emoji_attrs["id"] = serde_json::Value::String(id.to_string());
        }
        if let Some(t) = attrs.get("text") {
            emoji_attrs["text"] = serde_json::Value::String(t.to_string());
        }
        if let Some(lid) = attrs.get("localId") {
            emoji_attrs["localId"] = serde_json::Value::String(lid.to_string());
        }
        (
            attr_end,
            AdfNode {
                node_type: "emoji".to_string(),
                attrs: Some(emoji_attrs),
                content: None,
                text: None,
                marks: None,
                local_id: None,
                parameters: None,
            },
        )
    } else {
        (shortcode_end, AdfNode::emoji(&format!(":{short_name}:")))
    }
}

/// Tries to parse [text](url) starting at position `i`.
///
/// Uses bracket depth counting to find the matching `]`, so that `[` characters
/// inside the text (e.g. `[Task] some text ([Link](url))`) don't cause a false
/// match on an earlier `](`.
fn try_parse_link(text: &str, i: usize) -> Option<(usize, &str, &str)> {
    let rest = &text[i..];
    if !rest.starts_with('[') {
        return None;
    }

    // Find the matching ] by counting bracket depth
    let mut depth: usize = 0;
    let mut text_end = None;
    for (j, ch) in rest.char_indices() {
        match ch {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    text_end = Some(j);
                    break;
                }
            }
            _ => {}
        }
    }

    let text_end = text_end?;
    let link_text = &rest[1..text_end];
    // Must be immediately followed by (
    let after_bracket = &rest[text_end + 1..];
    if !after_bracket.starts_with('(') {
        return None;
    }
    let url_start = text_end + 2;
    let url_end = rest[url_start..].find(')')? + url_start;
    let href = &rest[url_start..url_end];

    Some((i + url_end + 1, link_text, href))
}

// ── ADF → Markdown ──────────────────────────────────────────────────

/// Options for ADF-to-markdown rendering.
#[derive(Debug, Clone, Default)]
pub struct RenderOptions {
    /// When true, omit `localId` attributes from directive output.
    pub strip_local_ids: bool,
}

/// Converts an ADF document to a markdown string.
pub fn adf_to_markdown(doc: &AdfDocument) -> Result<String> {
    adf_to_markdown_with_options(doc, &RenderOptions::default())
}

/// Converts an ADF document to a markdown string with options.
pub fn adf_to_markdown_with_options(doc: &AdfDocument, opts: &RenderOptions) -> Result<String> {
    let mut output = String::new();

    for (i, node) in doc.content.iter().enumerate() {
        if i > 0 {
            output.push('\n');
        }
        render_block_node(node, &mut output, opts);
    }

    Ok(output)
}

/// Pushes a `localId=<value>` entry to an attribute parts vec,
/// unless `opts.strip_local_ids` is set or the value is a placeholder.
/// Copies `localId` from parsed directive attrs to an ADF node's attrs if present.
fn pass_through_local_id(dir_attrs: &Option<crate::atlassian::attrs::Attrs>, node: &mut AdfNode) {
    if let Some(ref attrs) = dir_attrs {
        if let Some(local_id) = attrs.get("localId") {
            if let Some(ref mut node_attrs) = node.attrs {
                node_attrs["localId"] = serde_json::Value::String(local_id.to_string());
            } else {
                node.attrs = Some(serde_json::json!({"localId": local_id}));
            }
        }
    }
}

/// Copies `localId` from directive attrs to the node's top-level `local_id` field,
/// and parses `params` JSON from directive attrs into the node's `parameters` field.
fn pass_through_expand_params(
    dir_attrs: &Option<crate::atlassian::attrs::Attrs>,
    node: &mut AdfNode,
) {
    if let Some(ref attrs) = dir_attrs {
        if let Some(local_id) = attrs.get("localId") {
            node.local_id = Some(local_id.to_string());
        }
        if let Some(params_str) = attrs.get("params") {
            if let Ok(params) = serde_json::from_str(params_str) {
                node.parameters = Some(params);
            }
        }
    }
}

// listItem localId is emitted as trailing inline attrs on the item line
// (e.g., `- item text {localId=...}`) and parsed back by extracting
// trailing attrs from the list item text. This avoids the block-attrs
// promotion issue where {localId=...} on a separate line would be
// applied to the parent list node.

/// Extracts trailing `{localId=... paraLocalId=...}` from list item text.
/// Returns the text without the trailing attrs, the listItem localId, and
/// the paragraph localId if found.
fn extract_trailing_local_id(text: &str) -> (&str, Option<String>, Option<String>) {
    let trimmed = text.trim_end();
    if !trimmed.ends_with('}') {
        return (text, None, None);
    }
    // Find the opening brace.  Only match a standalone `{…}` block that is
    // preceded by whitespace (or is at the start of the string).  A `{` that
    // immediately follows `]` is part of an inline directive (e.g.
    // `:mention[text]{id=… localId=…}`) and must NOT be consumed here.
    if let Some(brace_pos) = trimmed.rfind('{') {
        if brace_pos > 0 && !trimmed.as_bytes()[brace_pos - 1].is_ascii_whitespace() {
            return (text, None, None);
        }
        let attr_str = &trimmed[brace_pos..];
        if let Some((_, attrs)) = parse_attrs(attr_str, 0) {
            let local_id = attrs.get("localId").map(str::to_string);
            let para_local_id = attrs.get("paraLocalId").map(str::to_string);
            if local_id.is_some() || para_local_id.is_some() {
                let before = trimmed[..brace_pos]
                    .strip_suffix(' ')
                    .unwrap_or(&trimmed[..brace_pos]);
                return (before, local_id, para_local_id);
            }
        }
    }
    (text, None, None)
}

/// Creates a `listItem` node, optionally with a `localId` attribute
/// and a `paraLocalId` on its first paragraph child.
fn list_item_with_local_id(
    mut content: Vec<AdfNode>,
    local_id: Option<String>,
    para_local_id: Option<String>,
) -> AdfNode {
    if let Some(id) = &para_local_id {
        if let Some(first) = content.first_mut() {
            if first.node_type == "paragraph" {
                let node_attrs = first.attrs.get_or_insert_with(|| serde_json::json!({}));
                node_attrs["localId"] = serde_json::Value::String(id.clone());
            }
        }
    }
    let mut item = AdfNode::list_item(content);
    if let Some(id) = local_id {
        item.attrs = Some(serde_json::json!({"localId": id}));
    }
    item
}

fn maybe_push_local_id(attrs: &serde_json::Value, parts: &mut Vec<String>, opts: &RenderOptions) {
    if opts.strip_local_ids {
        return;
    }
    if let Some(local_id) = attrs.get("localId").and_then(serde_json::Value::as_str) {
        if !local_id.is_empty() && local_id != "00000000-0000-0000-0000-000000000000" {
            parts.push(format!("localId={local_id}"));
        }
    }
}

/// Renders a sequence of block nodes with blank-line separators between them.
fn render_block_children(children: &[AdfNode], output: &mut String, opts: &RenderOptions) {
    for (i, child) in children.iter().enumerate() {
        if i > 0 {
            output.push('\n');
        }
        render_block_node(child, output, opts);
    }
}

/// Formats a float as an integer string when it has no fractional part,
/// otherwise as a regular float string.
fn fmt_f64_attr(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        v.to_string()
    }
}

/// Renders a block-level ADF node to markdown.
fn render_block_node(node: &AdfNode, output: &mut String, opts: &RenderOptions) {
    match node.node_type.as_str() {
        "paragraph" => {
            let is_empty = node.content.as_ref().map_or(true, Vec::is_empty);
            // Build directive attr string for localId when using ::paragraph form
            let dir_attrs = {
                let mut parts = Vec::new();
                if let Some(ref attrs) = node.attrs {
                    maybe_push_local_id(attrs, &mut parts, opts);
                }
                if parts.is_empty() {
                    String::new()
                } else {
                    format!("{{{}}}", parts.join(" "))
                }
            };
            if is_empty {
                output.push_str(&format!("::paragraph{dir_attrs}\n"));
            } else {
                // Render to a buffer first to check if content is whitespace-only
                let mut buf = String::new();
                render_inline_content(node, &mut buf, opts);
                if buf.trim().is_empty() && !buf.is_empty() {
                    // Whitespace-only content (e.g. NBSP) would be lost as a plain
                    // line — use the ::paragraph[content]{attrs} directive form
                    output.push_str(&format!("::paragraph[{buf}]{dir_attrs}\n"));
                } else {
                    // Escape a leading list-marker pattern so paragraph
                    // text is not re-parsed as a list item (issue #402).
                    let first = buf.lines().next().unwrap_or("");
                    if is_list_start(first) {
                        output.push_str(&escape_list_marker(first));
                        output.push_str(&buf[first.len()..]);
                    } else {
                        output.push_str(&buf);
                    }
                    output.push('\n');
                }
            }
        }
        "heading" => {
            let level = node
                .attrs
                .as_ref()
                .and_then(|a| a.get("level"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(1);
            for _ in 0..level {
                output.push('#');
            }
            output.push(' ');
            let mut buf = String::new();
            render_inline_content(node, &mut buf, opts);
            // Indent continuation lines produced by hardBreaks so they stay
            // within the heading when re-parsed (issue #433).
            let mut is_first_line = true;
            for line in buf.split('\n') {
                if is_first_line {
                    output.push_str(line);
                    is_first_line = false;
                } else {
                    output.push('\n');
                    if !line.is_empty() {
                        output.push_str("  ");
                    }
                    output.push_str(line);
                }
            }
            output.push('\n');
        }
        "codeBlock" => {
            let language = node
                .attrs
                .as_ref()
                .and_then(|a| a.get("language"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            output.push_str("```");
            output.push_str(language);
            output.push('\n');
            if let Some(ref content) = node.content {
                for child in content {
                    if let Some(ref text) = child.text {
                        output.push_str(text);
                    }
                }
            }
            output.push_str("\n```\n");
        }
        "blockquote" => {
            if let Some(ref content) = node.content {
                for child in content {
                    let mut inner = String::new();
                    render_block_node(child, &mut inner, opts);
                    for line in inner.lines() {
                        output.push_str("> ");
                        output.push_str(line);
                        output.push('\n');
                    }
                }
            }
        }
        "bulletList" => {
            if let Some(ref items) = node.content {
                for item in items {
                    output.push_str("- ");
                    render_list_item_content(item, output, opts);
                }
            }
        }
        "orderedList" => {
            let start = node
                .attrs
                .as_ref()
                .and_then(|a| a.get("order"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(1);
            if let Some(ref items) = node.content {
                for (i, item) in items.iter().enumerate() {
                    let num = start + i as u64;
                    output.push_str(&format!("{num}. "));
                    render_list_item_content(item, output, opts);
                }
            }
        }
        "taskList" => {
            if let Some(ref items) = node.content {
                for item in items {
                    let state = item
                        .attrs
                        .as_ref()
                        .and_then(|a| a.get("state"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("TODO");
                    if state == "DONE" {
                        output.push_str("- [x] ");
                    } else {
                        output.push_str("- [ ] ");
                    }
                    render_list_item_content(item, output, opts);
                }
            }
        }
        "rule" => {
            output.push_str("---\n");
        }
        "table" => {
            render_table(node, output, opts);
        }
        "mediaSingle" => {
            if let Some(ref content) = node.content {
                for child in content {
                    if child.node_type == "media" {
                        render_media(child, node.attrs.as_ref(), output, opts);
                    }
                }
            }
        }
        "blockCard" => {
            if let Some(ref attrs) = node.attrs {
                let url = attrs
                    .get("url")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                output.push_str(&format!("::card[{url}]"));
                let mut attr_parts = Vec::new();
                if let Some(layout) = attrs.get("layout").and_then(serde_json::Value::as_str) {
                    attr_parts.push(format!("layout={layout}"));
                }
                if let Some(width) = attrs.get("width").and_then(serde_json::Value::as_u64) {
                    attr_parts.push(format!("width={width}"));
                }
                if !attr_parts.is_empty() {
                    output.push_str(&format!("{{{}}}", attr_parts.join(" ")));
                }
                output.push('\n');
            }
        }
        "embedCard" => {
            if let Some(ref attrs) = node.attrs {
                let url = attrs
                    .get("url")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                output.push_str(&format!("::embed[{url}]"));
                let mut attr_parts = Vec::new();
                if let Some(layout) = attrs.get("layout").and_then(serde_json::Value::as_str) {
                    attr_parts.push(format!("layout={layout}"));
                }
                if let Some(h) = attrs
                    .get("originalHeight")
                    .and_then(serde_json::Value::as_f64)
                {
                    attr_parts.push(format!("originalHeight={}", fmt_f64_attr(h)));
                }
                if let Some(w) = attrs.get("width").and_then(serde_json::Value::as_f64) {
                    attr_parts.push(format!("width={}", fmt_f64_attr(w)));
                }
                if !attr_parts.is_empty() {
                    output.push_str(&format!("{{{}}}", attr_parts.join(" ")));
                }
                output.push('\n');
            }
        }
        "extension" => {
            if let Some(ref attrs) = node.attrs {
                let ext_type = attrs
                    .get("extensionType")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let ext_key = attrs
                    .get("extensionKey")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let mut attr_parts = vec![format!("type={ext_type}"), format!("key={ext_key}")];
                if let Some(layout) = attrs.get("layout").and_then(serde_json::Value::as_str) {
                    attr_parts.push(format!("layout={layout}"));
                }
                if let Some(params) = attrs.get("parameters") {
                    if let Ok(json_str) = serde_json::to_string(params) {
                        attr_parts.push(format!("params='{json_str}'"));
                    }
                }
                maybe_push_local_id(attrs, &mut attr_parts, opts);
                output.push_str(&format!("::extension{{{}}}\n", attr_parts.join(" ")));
            }
        }
        "panel" => {
            let panel_type = node
                .attrs
                .as_ref()
                .and_then(|a| a.get("panelType"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("info");
            let mut attr_parts = vec![format!("type={panel_type}")];
            if let Some(ref attrs) = node.attrs {
                if let Some(icon) = attrs.get("panelIcon").and_then(serde_json::Value::as_str) {
                    attr_parts.push(format!("icon=\"{icon}\""));
                }
                if let Some(color) = attrs.get("panelColor").and_then(serde_json::Value::as_str) {
                    attr_parts.push(format!("color=\"{color}\""));
                }
            }
            output.push_str(&format!(":::panel{{{}}}\n", attr_parts.join(" ")));
            if let Some(ref content) = node.content {
                render_block_children(content, output, opts);
            }
            output.push_str(":::\n");
        }
        "expand" | "nestedExpand" => {
            let directive_name = if node.node_type == "nestedExpand" {
                "nested-expand"
            } else {
                "expand"
            };
            let mut attr_parts = Vec::new();
            if let Some(t) = node
                .attrs
                .as_ref()
                .and_then(|a| a.get("title"))
                .and_then(serde_json::Value::as_str)
            {
                attr_parts.push(format!("title=\"{t}\""));
            }
            // Check top-level localId first, then fall back to attrs.localId
            if let Some(ref lid) = node.local_id {
                if !opts.strip_local_ids && lid != "00000000-0000-0000-0000-000000000000" {
                    attr_parts.push(format!("localId={lid}"));
                }
            } else if let Some(ref attrs) = node.attrs {
                maybe_push_local_id(attrs, &mut attr_parts, opts);
            }
            // Emit top-level parameters as params='...'
            if let Some(ref params) = node.parameters {
                if let Ok(json_str) = serde_json::to_string(params) {
                    attr_parts.push(format!("params='{json_str}'"));
                }
            }
            if attr_parts.is_empty() {
                output.push_str(&format!(":::{directive_name}\n"));
            } else {
                output.push_str(&format!(
                    ":::{directive_name}{{{}}}\n",
                    attr_parts.join(" ")
                ));
            }
            if let Some(ref content) = node.content {
                render_block_children(content, output, opts);
            }
            output.push_str(":::\n");
        }
        "layoutSection" => {
            output.push_str("::::layout\n");
            if let Some(ref content) = node.content {
                for child in content {
                    if child.node_type == "layoutColumn" {
                        let width = child
                            .attrs
                            .as_ref()
                            .and_then(|a| a.get("width"))
                            .and_then(serde_json::Value::as_f64)
                            .unwrap_or(50.0);
                        output.push_str(&format!(":::column{{width={width}}}\n"));
                        if let Some(ref col_content) = child.content {
                            render_block_children(col_content, output, opts);
                        }
                        output.push_str(":::\n");
                    }
                }
            }
            output.push_str("::::\n");
        }
        "decisionList" => {
            output.push_str(":::decisions\n");
            if let Some(ref content) = node.content {
                for item in content {
                    output.push_str("- <> ");
                    render_list_item_content(item, output, opts);
                }
            }
            output.push_str(":::\n");
        }
        "bodiedExtension" => {
            if let Some(ref attrs) = node.attrs {
                let ext_type = attrs
                    .get("extensionType")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let ext_key = attrs
                    .get("extensionKey")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let mut attr_parts = vec![format!("type={ext_type}"), format!("key={ext_key}")];
                if let Some(layout) = attrs.get("layout").and_then(serde_json::Value::as_str) {
                    attr_parts.push(format!("layout={layout}"));
                }
                maybe_push_local_id(attrs, &mut attr_parts, opts);
                output.push_str(&format!(":::extension{{{}}}\n", attr_parts.join(" ")));
                if let Some(ref content) = node.content {
                    render_block_children(content, output, opts);
                }
                output.push_str(":::\n");
            }
        }
        _ => {
            // Preserve unsupported nodes as JSON in adf-unsupported code blocks
            if let Ok(json) = serde_json::to_string_pretty(node) {
                output.push_str("```adf-unsupported\n");
                output.push_str(&json);
                output.push_str("\n```\n");
            }
        }
    }

    // Emit block-level attribute marks (align, indent, breakout) and localId
    let mut parts = Vec::new();
    if let Some(ref marks) = node.marks {
        for mark in marks {
            match mark.mark_type.as_str() {
                "alignment" => {
                    if let Some(align) = mark
                        .attrs
                        .as_ref()
                        .and_then(|a| a.get("align"))
                        .and_then(serde_json::Value::as_str)
                    {
                        parts.push(format!("align={align}"));
                    }
                }
                "indentation" => {
                    if let Some(level) = mark
                        .attrs
                        .as_ref()
                        .and_then(|a| a.get("level"))
                        .and_then(serde_json::Value::as_u64)
                    {
                        parts.push(format!("indent={level}"));
                    }
                }
                "breakout" => {
                    if let Some(mode) = mark
                        .attrs
                        .as_ref()
                        .and_then(|a| a.get("mode"))
                        .and_then(serde_json::Value::as_str)
                    {
                        parts.push(format!("breakout={mode}"));
                    }
                    if let Some(width) = mark
                        .attrs
                        .as_ref()
                        .and_then(|a| a.get("width"))
                        .and_then(serde_json::Value::as_u64)
                    {
                        parts.push(format!("breakoutWidth={width}"));
                    }
                }
                _ => {}
            }
        }
    }
    // Skip localId for node types that already include it in their directive attrs.
    // For paragraphs, localId is included in the ::paragraph directive when the
    // paragraph uses directive form (empty or whitespace-only content).
    let para_used_directive = node.node_type == "paragraph" && {
        let is_empty = node.content.as_ref().map_or(true, Vec::is_empty);
        if is_empty {
            true
        } else {
            let mut buf = String::new();
            render_inline_content(node, &mut buf, opts);
            buf.trim().is_empty() && !buf.is_empty()
        }
    };
    if !matches!(node.node_type.as_str(), "expand" | "nestedExpand") && !para_used_directive {
        if let Some(ref attrs) = node.attrs {
            maybe_push_local_id(attrs, &mut parts, opts);
        }
    }
    if !parts.is_empty() {
        output.push_str(&format!("{{{}}}\n", parts.join(" ")));
    }
}

/// Renders the content of a list item (unwraps the paragraph layer).
/// Nested block children (e.g. sub-lists) are indented with two spaces.
///
/// Some ADF producers (e.g. Confluence) emit `taskItem` content without a
/// paragraph wrapper — the inline nodes sit directly inside the item.  We
/// detect this by checking whether the first child is an inline node type
/// and, if so, render *all* leading inline children on the first line.
fn render_list_item_content(item: &AdfNode, output: &mut String, opts: &RenderOptions) {
    let Some(ref content) = item.content else {
        // Still emit localId and newline for items with no content (e.g. empty taskItem).
        let bare = AdfNode::text("");
        emit_list_item_local_ids(item, &bare, output, opts);
        output.push('\n');
        return;
    };
    if content.is_empty() {
        let bare = AdfNode::text("");
        emit_list_item_local_ids(item, &bare, output, opts);
        output.push('\n');
        return;
    }
    let first = &content[0];
    let rest_start;
    if first.node_type == "paragraph" {
        let mut buf = String::new();
        render_inline_content(first, &mut buf, opts);
        // Indent continuation lines produced by hardBreaks so they stay
        // within the list item when re-parsed (issue #402).
        let mut is_first_line = true;
        for line in buf.split('\n') {
            if is_first_line {
                output.push_str(line);
                is_first_line = false;
            } else {
                output.push('\n');
                if !line.is_empty() {
                    output.push_str("  ");
                }
                output.push_str(line);
            }
        }
        // Emit paragraph + listItem localIds as trailing inline attrs on the first line
        emit_list_item_local_ids(item, first, output, opts);
        output.push('\n');
        rest_start = 1;
    } else if is_inline_node_type(&first.node_type) {
        // Inline nodes without a paragraph wrapper — render them directly.
        rest_start = content
            .iter()
            .position(|c| !is_inline_node_type(&c.node_type))
            .unwrap_or(content.len());
        for child in &content[..rest_start] {
            render_inline_node(child, output, opts);
        }
        // No paragraph wrapper — pass a bare node so paraLocalId is omitted.
        let bare = AdfNode::text("");
        emit_list_item_local_ids(item, &bare, output, opts);
        output.push('\n');
        // Any remaining children are block nodes — fall through to the
        // indented-block loop below.
    } else {
        render_block_node(first, output, opts);
        rest_start = 1;
    }
    for child in &content[rest_start..] {
        let mut nested = String::new();
        render_block_node(child, &mut nested, opts);
        for line in nested.lines() {
            output.push_str("  ");
            output.push_str(line);
            output.push('\n');
        }
    }
}

/// Returns `true` if the given ADF node type is an inline node.
fn is_inline_node_type(node_type: &str) -> bool {
    matches!(
        node_type,
        "text"
            | "hardBreak"
            | "inlineCard"
            | "emoji"
            | "mention"
            | "status"
            | "date"
            | "placeholder"
            | "mediaInline"
    )
}

/// Emits trailing `{localId=... paraLocalId=...}` on a list item line
/// for both the listItem and its first (unwrapped) paragraph.
fn emit_list_item_local_ids(
    item: &AdfNode,
    paragraph: &AdfNode,
    output: &mut String,
    opts: &RenderOptions,
) {
    if opts.strip_local_ids {
        return;
    }
    let mut parts = Vec::new();
    if let Some(ref attrs) = item.attrs {
        maybe_push_local_id(attrs, &mut parts, opts);
    }
    if let Some(ref attrs) = paragraph.attrs {
        if let Some(local_id) = attrs.get("localId").and_then(serde_json::Value::as_str) {
            if !local_id.is_empty() && local_id != "00000000-0000-0000-0000-000000000000" {
                parts.push(format!("paraLocalId={local_id}"));
            }
        }
    }
    if !parts.is_empty() {
        output.push_str(&format!(" {{{}}}", parts.join(" ")));
    }
}

/// Renders a table node, choosing between pipe table and directive table form.
fn render_table(node: &AdfNode, output: &mut String, opts: &RenderOptions) {
    let Some(ref rows) = node.content else {
        return;
    };

    if table_qualifies_for_pipe_syntax(rows) {
        render_pipe_table(node, rows, output, opts);
    } else {
        render_directive_table(node, rows, output, opts);
    }
}

/// Checks whether all cells qualify for GFM pipe table syntax:
/// - Every cell has exactly one paragraph child with only inline nodes
/// - All `tableHeader` nodes appear exclusively in the first row
/// - The first row must contain at least one `tableHeader` (pipe tables
///   always treat the first row as headers, so `tableCell`-only first rows
///   must use directive form to preserve the cell type)
fn table_qualifies_for_pipe_syntax(rows: &[AdfNode]) -> bool {
    // Tables with caption nodes must use directive form
    if rows.iter().any(|n| n.node_type == "caption") {
        return false;
    }
    let mut first_row_has_header = false;
    for (row_idx, row) in rows.iter().enumerate() {
        let Some(ref cells) = row.content else {
            continue;
        };
        for cell in cells {
            // Header cells outside first row → must use directive form
            if row_idx > 0 && cell.node_type == "tableHeader" {
                return false;
            }
            if row_idx == 0 && cell.node_type == "tableHeader" {
                first_row_has_header = true;
            }
            // Check cell has exactly one paragraph with only inline content
            let Some(ref content) = cell.content else {
                continue;
            };
            if content.len() != 1 || content[0].node_type != "paragraph" {
                return false;
            }
            // hardBreak inside a cell produces a newline that breaks pipe
            // table syntax — fall back to directive form
            if cell_contains_hard_break(&content[0]) {
                return false;
            }
            // Paragraph-level localId would be lost in pipe form (the paragraph
            // is unwrapped into the cell text) — fall back to directive form
            if content[0]
                .attrs
                .as_ref()
                .and_then(|a| a.get("localId"))
                .is_some()
            {
                return false;
            }
        }
    }
    // First row must have at least one tableHeader for pipe syntax;
    // otherwise the round-trip would convert tableCell → tableHeader.
    first_row_has_header
}

/// Returns true if a paragraph node contains any `hardBreak` inline nodes.
fn cell_contains_hard_break(paragraph: &AdfNode) -> bool {
    paragraph
        .content
        .as_ref()
        .is_some_and(|nodes| nodes.iter().any(|n| n.node_type == "hardBreak"))
}

/// Renders a table as GFM pipe syntax.
fn render_pipe_table(node: &AdfNode, rows: &[AdfNode], output: &mut String, opts: &RenderOptions) {
    for (row_idx, row) in rows.iter().enumerate() {
        let Some(ref cells) = row.content else {
            continue;
        };

        output.push('|');
        for cell in cells {
            output.push(' ');
            render_cell_attrs_prefix(cell, output);
            render_inline_content_from_first_paragraph(cell, output, opts);
            output.push_str(" |");
        }
        output.push('\n');

        // Add separator after header row
        if row_idx == 0 {
            output.push('|');
            for cell in cells {
                let align = get_cell_paragraph_alignment(cell);
                match align {
                    Some("center") => output.push_str(" :---: |"),
                    Some("end") => output.push_str(" ---: |"),
                    _ => output.push_str(" --- |"),
                }
            }
            output.push('\n');
        }
    }

    // Emit table-level attrs if present
    render_table_level_attrs(node, output, opts);
}

/// Renders a table as `::::table` directive syntax (block-content cells).
fn render_directive_table(
    node: &AdfNode,
    rows: &[AdfNode],
    output: &mut String,
    opts: &RenderOptions,
) {
    // Opening fence with attrs
    let mut attr_parts = Vec::new();
    if let Some(ref attrs) = node.attrs {
        if let Some(layout) = attrs.get("layout").and_then(serde_json::Value::as_str) {
            attr_parts.push(format!("layout={layout}"));
        }
        if let Some(numbered) = attrs
            .get("isNumberColumnEnabled")
            .and_then(serde_json::Value::as_bool)
        {
            if numbered {
                attr_parts.push("numbered".to_string());
            } else {
                attr_parts.push("numbered=false".to_string());
            }
        }
        if let Some(tw) = attrs.get("width").and_then(serde_json::Value::as_f64) {
            let tw_str = if tw.fract() == 0.0 {
                (tw as u64).to_string()
            } else {
                tw.to_string()
            };
            attr_parts.push(format!("width={tw_str}"));
        }
        maybe_push_local_id(attrs, &mut attr_parts, opts);
    }
    if attr_parts.is_empty() {
        output.push_str("::::table\n");
    } else {
        output.push_str(&format!("::::table{{{}}}\n", attr_parts.join(" ")));
    }

    for row in rows {
        if row.node_type == "caption" {
            output.push_str(":::caption\n");
            if let Some(ref content) = row.content {
                for child in content {
                    render_inline_node(child, output, opts);
                }
                output.push('\n');
            }
            output.push_str(":::\n");
            continue;
        }
        let Some(ref cells) = row.content else {
            continue;
        };
        // Emit :::tr with optional localId
        let mut tr_attrs = Vec::new();
        if let Some(ref attrs) = row.attrs {
            maybe_push_local_id(attrs, &mut tr_attrs, opts);
        }
        if tr_attrs.is_empty() {
            output.push_str(":::tr\n");
        } else {
            output.push_str(&format!(":::tr{{{}}}\n", tr_attrs.join(" ")));
        }
        for cell in cells {
            let directive_name = if cell.node_type == "tableHeader" {
                "th"
            } else {
                "td"
            };
            let mut cell_attr_str = build_cell_attrs_string(cell);
            // Append localId to cell attrs if present
            if let Some(ref attrs) = cell.attrs {
                let mut lid_parts = Vec::new();
                maybe_push_local_id(attrs, &mut lid_parts, opts);
                if !lid_parts.is_empty() {
                    if !cell_attr_str.is_empty() {
                        cell_attr_str.push(' ');
                    }
                    cell_attr_str.push_str(&lid_parts.join(" "));
                }
            }
            if cell_attr_str.is_empty() && cell.attrs.is_none() {
                output.push_str(&format!(":::{directive_name}\n"));
            } else {
                output.push_str(&format!(":::{directive_name}{{{cell_attr_str}}}\n"));
            }
            if let Some(ref content) = cell.content {
                render_block_children(content, output, opts);
            }
            output.push_str(":::\n");
        }
        output.push_str(":::\n");
    }

    output.push_str("::::\n");
}

/// Returns `true` when an attribute value must be quoted to survive round-trip
/// through the `{key=value}` attribute parser (which stops unquoted values at
/// whitespace or `}`).
fn needs_attr_quoting(value: &str) -> bool {
    value.contains(|c: char| c.is_whitespace() || c == '}' || c == '(' || c == ')' || c == ',')
}

/// Builds a JFM attribute string from ADF cell attributes.
fn build_cell_attrs_string(cell: &AdfNode) -> String {
    let Some(ref attrs) = cell.attrs else {
        return String::new();
    };
    let mut parts = Vec::new();
    if let Some(colspan) = attrs.get("colspan").and_then(serde_json::Value::as_u64) {
        parts.push(format!("colspan={colspan}"));
    }
    if let Some(rowspan) = attrs.get("rowspan").and_then(serde_json::Value::as_u64) {
        parts.push(format!("rowspan={rowspan}"));
    }
    if let Some(bg) = attrs.get("background").and_then(serde_json::Value::as_str) {
        if needs_attr_quoting(bg) {
            let escaped = bg.replace('\\', "\\\\").replace('"', "\\\"");
            parts.push(format!("bg=\"{escaped}\""));
        } else {
            parts.push(format!("bg={bg}"));
        }
    }
    if let Some(colwidth) = attrs.get("colwidth").and_then(serde_json::Value::as_array) {
        let widths: Vec<String> = colwidth
            .iter()
            .filter_map(serde_json::Value::as_f64)
            .map(|n| {
                // Always format as float to preserve Confluence's representation
                if n.fract() == 0.0 {
                    format!("{n:.1}")
                } else {
                    n.to_string()
                }
            })
            .collect();
        if !widths.is_empty() {
            parts.push(format!("colwidth={}", widths.join(",")));
        }
    }
    parts.join(" ")
}

/// Renders `{attrs}` prefix for a pipe table cell (background, colspan, etc.).
fn render_cell_attrs_prefix(cell: &AdfNode, output: &mut String) {
    let Some(ref _attrs) = cell.attrs else {
        return;
    };
    let attr_str = build_cell_attrs_string(cell);
    if attr_str.is_empty() {
        output.push_str("{} ");
    } else {
        output.push_str(&format!("{{{attr_str}}} "));
    }
}

/// Gets the alignment mark value from the paragraph inside a table cell.
fn get_cell_paragraph_alignment(cell: &AdfNode) -> Option<&str> {
    let content = cell.content.as_ref()?;
    let para = content.first()?;
    let marks = para.marks.as_ref()?;
    marks.iter().find_map(|m| {
        if m.mark_type == "alignment" {
            m.attrs
                .as_ref()
                .and_then(|a| a.get("align"))
                .and_then(serde_json::Value::as_str)
        } else {
            None
        }
    })
}

/// Emits table-level attributes if present.
fn render_table_level_attrs(node: &AdfNode, output: &mut String, opts: &RenderOptions) {
    if let Some(ref attrs) = node.attrs {
        let mut parts = Vec::new();
        if let Some(layout) = attrs.get("layout").and_then(serde_json::Value::as_str) {
            parts.push(format!("layout={layout}"));
        }
        if let Some(numbered) = attrs
            .get("isNumberColumnEnabled")
            .and_then(serde_json::Value::as_bool)
        {
            if numbered {
                parts.push("numbered".to_string());
            } else {
                parts.push("numbered=false".to_string());
            }
        }
        if let Some(tw) = attrs.get("width").and_then(serde_json::Value::as_f64) {
            let tw_str = if tw.fract() == 0.0 {
                (tw as u64).to_string()
            } else {
                tw.to_string()
            };
            parts.push(format!("width={tw_str}"));
        }
        maybe_push_local_id(attrs, &mut parts, opts);
        if !parts.is_empty() {
            output.push_str(&format!("{{{}}}\n", parts.join(" ")));
        }
    }
}

/// Renders inline content from the first paragraph child of a table cell.
fn render_inline_content_from_first_paragraph(
    cell: &AdfNode,
    output: &mut String,
    opts: &RenderOptions,
) {
    if let Some(ref content) = cell.content {
        if let Some(first) = content.first() {
            if first.node_type == "paragraph" {
                render_inline_content(first, output, opts);
            }
        }
    }
}

/// Renders a media node as a markdown image, with optional parent (mediaSingle) attrs.
fn render_media(
    node: &AdfNode,
    parent_attrs: Option<&serde_json::Value>,
    output: &mut String,
    opts: &RenderOptions,
) {
    if let Some(ref attrs) = node.attrs {
        let media_type = attrs
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("external");
        let alt = attrs
            .get("alt")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");

        if media_type == "file" {
            // Confluence file attachment — encode metadata in {attrs} block so it survives round-trip
            output.push_str(&format!("![{alt}]()"));
            let mut parts = vec!["type=file".to_string()];
            if let Some(id) = attrs.get("id").and_then(serde_json::Value::as_str) {
                parts.push(format!("id={id}"));
            }
            if let Some(collection) = attrs.get("collection").and_then(serde_json::Value::as_str) {
                parts.push(format!("collection={collection}"));
            }
            if let Some(height) = attrs.get("height").and_then(serde_json::Value::as_u64) {
                parts.push(format!("height={height}"));
            }
            if let Some(width) = attrs.get("width").and_then(serde_json::Value::as_u64) {
                parts.push(format!("width={width}"));
            }
            maybe_push_local_id(attrs, &mut parts, opts);
            // Encode mediaSingle layout/width/widthType if non-default
            if let Some(p_attrs) = parent_attrs {
                if let Some(layout) = p_attrs.get("layout").and_then(serde_json::Value::as_str) {
                    if layout != "center" {
                        parts.push(format!("layout={layout}"));
                    }
                }
                if let Some(ms_width) = p_attrs.get("width").and_then(serde_json::Value::as_u64) {
                    parts.push(format!("mediaWidth={ms_width}"));
                }
                if let Some(wt) = p_attrs.get("widthType").and_then(serde_json::Value::as_str) {
                    parts.push(format!("widthType={wt}"));
                }
                if let Some(mode) = p_attrs.get("mode").and_then(serde_json::Value::as_str) {
                    parts.push(format!("mode={mode}"));
                }
            }
            output.push_str(&format!("{{{}}}", parts.join(" ")));
        } else {
            // External image
            let url = attrs
                .get("url")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            output.push_str(&format!("![{alt}]({url})"));

            // Emit {layout=... width=... widthType=... mode=... localId=...} if non-default attrs present
            {
                let mut parts = Vec::new();
                if let Some(p_attrs) = parent_attrs {
                    let layout = p_attrs.get("layout").and_then(serde_json::Value::as_str);
                    let width = p_attrs.get("width").and_then(serde_json::Value::as_u64);
                    let width_type = p_attrs.get("widthType").and_then(serde_json::Value::as_str);
                    if let Some(l) = layout {
                        if l != "center" {
                            parts.push(format!("layout={l}"));
                        }
                    }
                    if let Some(w) = width {
                        parts.push(format!("width={w}"));
                    }
                    if let Some(wt) = width_type {
                        parts.push(format!("widthType={wt}"));
                    }
                    if let Some(mode) = p_attrs.get("mode").and_then(serde_json::Value::as_str) {
                        parts.push(format!("mode={mode}"));
                    }
                }
                maybe_push_local_id(attrs, &mut parts, opts);
                if !parts.is_empty() {
                    output.push_str(&format!("{{{}}}", parts.join(" ")));
                }
            }
        }

        output.push('\n');
    }
}

/// Renders inline content (text nodes with marks) from a block node's children.
fn render_inline_content(node: &AdfNode, output: &mut String, opts: &RenderOptions) {
    if let Some(ref content) = node.content {
        for child in content {
            render_inline_node(child, output, opts);
        }
    }
}

/// Renders a single inline ADF node to markdown.
fn render_inline_node(node: &AdfNode, output: &mut String, opts: &RenderOptions) {
    match node.node_type.as_str() {
        "text" => {
            let text = node.text.as_deref().unwrap_or("");
            let marks = node.marks.as_deref().unwrap_or(&[]);
            render_marked_text(text, marks, output);
        }
        "hardBreak" => {
            output.push_str("\\\n");
        }
        "inlineCard" => {
            if let Some(ref attrs) = node.attrs {
                if let Some(url) = attrs.get("url").and_then(serde_json::Value::as_str) {
                    output.push_str(":card[");
                    output.push_str(url);
                    output.push(']');
                    let mut attr_parts = Vec::new();
                    maybe_push_local_id(attrs, &mut attr_parts, opts);
                    if !attr_parts.is_empty() {
                        output.push('{');
                        output.push_str(&attr_parts.join(" "));
                        output.push('}');
                    }
                }
            }
        }
        "emoji" => {
            if let Some(ref attrs) = node.attrs {
                if let Some(short_name) = attrs.get("shortName").and_then(serde_json::Value::as_str)
                {
                    output.push(':');
                    let name = short_name.strip_prefix(':').unwrap_or(short_name);
                    let name = name.strip_suffix(':').unwrap_or(name);
                    output.push_str(name);
                    output.push(':');

                    let mut parts = Vec::new();
                    let escaped_sn = short_name.replace('\\', "\\\\").replace('"', "\\\"");
                    parts.push(format!("shortName=\"{escaped_sn}\""));
                    if let Some(id) = attrs.get("id").and_then(serde_json::Value::as_str) {
                        let escaped = id.replace('\\', "\\\\").replace('"', "\\\"");
                        parts.push(format!("id=\"{escaped}\""));
                    }
                    if let Some(text) = attrs.get("text").and_then(serde_json::Value::as_str) {
                        let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
                        parts.push(format!("text=\"{escaped}\""));
                    }
                    maybe_push_local_id(attrs, &mut parts, opts);
                    output.push('{');
                    output.push_str(&parts.join(" "));
                    output.push('}');
                }
            }
        }
        "status" => {
            if let Some(ref attrs) = node.attrs {
                let text = attrs
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let color = attrs
                    .get("color")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("neutral");
                let mut attr_parts = vec![format!("color={color}")];
                if let Some(style) = attrs.get("style").and_then(serde_json::Value::as_str) {
                    attr_parts.push(format!("style={style}"));
                }
                maybe_push_local_id(attrs, &mut attr_parts, opts);
                output.push_str(&format!(":status[{text}]{{{}}}", attr_parts.join(" ")));
            }
        }
        "date" => {
            if let Some(ref attrs) = node.attrs {
                if let Some(timestamp) = attrs.get("timestamp").and_then(serde_json::Value::as_str)
                {
                    let display = epoch_ms_to_iso_date(timestamp);
                    let mut attr_parts = vec![format!("timestamp={timestamp}")];
                    maybe_push_local_id(attrs, &mut attr_parts, opts);
                    output.push_str(&format!(":date[{display}]{{{}}}", attr_parts.join(" ")));
                }
            }
        }
        "mention" => {
            if let Some(ref attrs) = node.attrs {
                let id = attrs
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let text = attrs
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let mut attr_parts = vec![format!("id={id}")];
                if let Some(ut) = attrs.get("userType").and_then(serde_json::Value::as_str) {
                    attr_parts.push(format!("userType={ut}"));
                }
                if let Some(al) = attrs.get("accessLevel").and_then(serde_json::Value::as_str) {
                    attr_parts.push(format!("accessLevel={al}"));
                }
                maybe_push_local_id(attrs, &mut attr_parts, opts);
                output.push_str(&format!(":mention[{text}]{{{}}}", attr_parts.join(" ")));
            }
        }
        "placeholder" => {
            if let Some(ref attrs) = node.attrs {
                let text = attrs
                    .get("text")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                output.push_str(&format!(":placeholder[{text}]"));
            }
        }
        "inlineExtension" => {
            if let Some(ref attrs) = node.attrs {
                let ext_type = attrs
                    .get("extensionType")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let ext_key = attrs
                    .get("extensionKey")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("");
                let fallback = node.text.as_deref().unwrap_or("");
                output.push_str(&format!(
                    ":extension[{fallback}]{{type={ext_type} key={ext_key}}}"
                ));
            }
        }
        _ => {
            output.push_str(&format!("<!-- unsupported inline: {} -->", node.node_type));
        }
    }
}

/// Renders text with ADF marks applied as markdown syntax.
///
/// Mark ordering is preserved by checking the position of the `link` mark
/// relative to formatting marks. Formatting marks that appear before `link`
/// in the marks array are rendered as outer wrappers (e.g., `**[text](url)**`),
/// while those after `link` are rendered inside the link (e.g., `[**text**](url)`).
fn render_marked_text(text: &str, marks: &[AdfMark], output: &mut String) {
    let link_pos = marks.iter().position(|m| m.mark_type == "link");
    let has_link = link_pos.map(|lp| &marks[lp]);
    let has_strong = marks.iter().any(|m| m.mark_type == "strong");
    let has_em = marks.iter().any(|m| m.mark_type == "em");
    let has_code = marks.iter().any(|m| m.mark_type == "code");
    let has_strike = marks.iter().any(|m| m.mark_type == "strike");

    if has_code {
        // Code marks override other formatting in markdown
        if let Some(link_mark) = has_link {
            let href = link_mark
                .attrs
                .as_ref()
                .and_then(|a| a.get("href"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            output.push('[');
            output.push('`');
            output.push_str(text);
            output.push('`');
            output.push_str("](");
            output.push_str(href);
            output.push(')');
        } else {
            output.push('`');
            output.push_str(text);
            output.push('`');
        }
        return;
    }

    // Helper: check if a formatting mark appears before the link mark.
    let is_before_link = |mark_type: &str| -> bool {
        if let Some(lp) = link_pos {
            marks[..lp].iter().any(|m| m.mark_type == mark_type)
        } else {
            false
        }
    };

    // Partition formatting marks into outer (before link) and inner (after link / no link).
    let outer_strike = has_strike && is_before_link("strike");
    let outer_strong = has_strong && is_before_link("strong");
    let outer_em = has_em && is_before_link("em");
    let inner_strike = has_strike && !outer_strike;
    let inner_strong = has_strong && !outer_strong;
    let inner_em = has_em && !outer_em;

    // Build the innermost formatted text.
    let mut inner = String::new();
    if inner_strike {
        inner.push_str("~~");
    }
    if inner_strong {
        inner.push_str("**");
    }
    if inner_em {
        inner.push('*');
    }
    inner.push_str(&escape_backticks(&escape_emphasis_markers(text)));
    if inner_em {
        inner.push('*');
    }
    if inner_strong {
        inner.push_str("**");
    }
    if inner_strike {
        inner.push_str("~~");
    }

    // Check for span-style marks (textColor, backgroundColor, subsup)
    let text_color = marks.iter().find(|m| m.mark_type == "textColor");
    let bg_color = marks.iter().find(|m| m.mark_type == "backgroundColor");
    let subsup = marks.iter().find(|m| m.mark_type == "subsup");
    let has_underline = marks.iter().any(|m| m.mark_type == "underline");
    let annotations: Vec<&AdfMark> = marks
        .iter()
        .filter(|m| m.mark_type == "annotation")
        .collect();

    let needs_span = text_color.is_some() || bg_color.is_some() || subsup.is_some();

    // Build the core content (with span/bracketed/link wrapping).
    let mut core = String::new();
    if needs_span {
        // Wrap in :span[text]{attrs} syntax
        let mut attr_parts = Vec::new();
        if let Some(m) = text_color {
            if let Some(c) = m
                .attrs
                .as_ref()
                .and_then(|a| a.get("color"))
                .and_then(serde_json::Value::as_str)
            {
                attr_parts.push(format!("color={c}"));
            }
        }
        if let Some(m) = bg_color {
            if let Some(c) = m
                .attrs
                .as_ref()
                .and_then(|a| a.get("color"))
                .and_then(serde_json::Value::as_str)
            {
                attr_parts.push(format!("bg={c}"));
            }
        }
        if let Some(m) = subsup {
            if let Some(kind) = m
                .attrs
                .as_ref()
                .and_then(|a| a.get("type"))
                .and_then(serde_json::Value::as_str)
            {
                attr_parts.push(kind.to_string());
            }
        }
        let span = format!(":span[{inner}]{{{}}}", attr_parts.join(" "));
        if let Some(link_mark) = has_link {
            let href = link_href(link_mark);
            if is_before_link("textColor")
                || is_before_link("backgroundColor")
                || is_before_link("subsup")
            {
                // Span wraps the link: :span[[text](url)]{attrs}
                let link_part = format!("[{inner}]({href})");
                core = format!(":span[{link_part}]{{{}}}", attr_parts.join(" "));
            } else {
                // Link wraps the span: [:span[text]{attrs}](url)
                core.push('[');
                core.push_str(&span);
                core.push_str("](");
                core.push_str(href);
                core.push(')');
            }
        } else {
            core.push_str(&span);
        }
    } else if has_underline || !annotations.is_empty() {
        let mut attr_parts = Vec::new();
        if has_underline {
            attr_parts.push("underline".to_string());
        }
        for ann in &annotations {
            if let Some(ref attrs) = ann.attrs {
                if let Some(id) = attrs.get("id").and_then(serde_json::Value::as_str) {
                    let escaped = id.replace('\\', "\\\\").replace('"', "\\\"");
                    attr_parts.push(format!("annotation-id=\"{escaped}\""));
                }
                if let Some(at) = attrs
                    .get("annotationType")
                    .and_then(serde_json::Value::as_str)
                {
                    attr_parts.push(format!("annotation-type={at}"));
                }
            }
        }
        let bracketed = format!("[{inner}]{{{}}}", attr_parts.join(" "));
        if let Some(link_mark) = has_link {
            let href = link_href(link_mark);
            if is_before_link("underline")
                || link_pos
                    .is_some_and(|lp| marks[..lp].iter().any(|m| m.mark_type == "annotation"))
            {
                // Bracketed span wraps the link: [[text](url)]{underline}
                let link_part = format!("[{inner}]({href})");
                core = format!("[{link_part}]{{{}}}", attr_parts.join(" "));
            } else {
                // Link wraps the bracketed span: [[text]{underline}](url)
                core.push('[');
                core.push_str(&bracketed);
                core.push_str("](");
                core.push_str(href);
                core.push(')');
            }
        } else {
            core.push_str(&bracketed);
        }
    } else if let Some(link_mark) = has_link {
        let href = link_href(link_mark);
        core.push('[');
        core.push_str(&inner);
        core.push_str("](");
        core.push_str(href);
        core.push(')');
    } else {
        core.push_str(&inner);
    }

    // Apply outer formatting wrappers (marks that appeared before link).
    if outer_strike {
        output.push_str("~~");
    }
    if outer_strong {
        output.push_str("**");
    }
    if outer_em {
        output.push('*');
    }
    output.push_str(&core);
    if outer_em {
        output.push('*');
    }
    if outer_strong {
        output.push_str("**");
    }
    if outer_strike {
        output.push_str("~~");
    }
}

/// Extracts the href from a link mark.
fn link_href(mark: &AdfMark) -> &str {
    mark.attrs
        .as_ref()
        .and_then(|a| a.get("href"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── markdown_to_adf tests ───────────────────────────────────────

    #[test]
    fn paragraph() {
        let doc = markdown_to_adf("Hello world").unwrap();
        assert_eq!(doc.content.len(), 1);
        assert_eq!(doc.content[0].node_type, "paragraph");
    }

    #[test]
    fn heading_levels() {
        for level in 1..=6 {
            let hashes = "#".repeat(level);
            let md = format!("{hashes} Title");
            let doc = markdown_to_adf(&md).unwrap();
            assert_eq!(doc.content[0].node_type, "heading");
            let attrs = doc.content[0].attrs.as_ref().unwrap();
            assert_eq!(attrs["level"], level as u64);
        }
    }

    #[test]
    fn code_block() {
        let md = "```rust\nfn main() {}\n```";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "codeBlock");
        let attrs = doc.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["language"], "rust");
    }

    #[test]
    fn code_block_no_language() {
        let md = "```\nsome code\n```";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "codeBlock");
        assert!(doc.content[0].attrs.is_none());
    }

    #[test]
    fn horizontal_rule() {
        let doc = markdown_to_adf("---").unwrap();
        assert_eq!(doc.content[0].node_type, "rule");
    }

    #[test]
    fn horizontal_rule_stars() {
        let doc = markdown_to_adf("***").unwrap();
        assert_eq!(doc.content[0].node_type, "rule");
    }

    #[test]
    fn blockquote() {
        let md = "> This is a quote\n> Second line";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "blockquote");
    }

    #[test]
    fn bullet_list() {
        let md = "- Item 1\n- Item 2\n- Item 3";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "bulletList");
        let items = doc.content[0].content.as_ref().unwrap();
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn ordered_list() {
        let md = "1. First\n2. Second\n3. Third";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "orderedList");
        let items = doc.content[0].content.as_ref().unwrap();
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn task_list() {
        let md = "- [ ] Todo item\n- [x] Done item";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "taskList");
        let items = doc.content[0].content.as_ref().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].node_type, "taskItem");
        assert_eq!(items[0].attrs.as_ref().unwrap()["state"], "TODO");
        assert_eq!(items[1].attrs.as_ref().unwrap()["state"], "DONE");
    }

    #[test]
    fn task_list_uppercase_x() {
        let md = "- [X] Done item";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "taskList");
        let item = &doc.content[0].content.as_ref().unwrap()[0];
        assert_eq!(item.attrs.as_ref().unwrap()["state"], "DONE");
    }

    #[test]
    fn adf_task_list_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::task_list(vec![
                AdfNode::task_item(
                    "TODO",
                    vec![AdfNode::paragraph(vec![AdfNode::text("Todo")])],
                ),
                AdfNode::task_item(
                    "DONE",
                    vec![AdfNode::paragraph(vec![AdfNode::text("Done")])],
                ),
            ])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("- [ ] Todo"));
        assert!(md.contains("- [x] Done"));
    }

    #[test]
    fn round_trip_task_list() {
        let md = "- [ ] Todo item\n- [x] Done item\n";
        let doc = markdown_to_adf(md).unwrap();
        let result = adf_to_markdown(&doc).unwrap();
        assert!(result.contains("- [ ] Todo item"));
        assert!(result.contains("- [x] Done item"));
    }

    /// Issue #408: taskItem content with inline nodes directly (no paragraph wrapper).
    #[test]
    fn adf_task_item_unwrapped_inline_content() {
        // Real Confluence ADF: taskItem contains text nodes directly, no paragraph.
        let json = r#"{
            "version": 1,
            "type": "doc",
            "content": [{
                "type": "taskList",
                "attrs": {"localId": "list-001"},
                "content": [{
                    "type": "taskItem",
                    "attrs": {"localId": "task-001", "state": "TODO"},
                    "content": [{"type": "text", "text": "Do something"}]
                }]
            }]
        }"#;
        let doc: AdfDocument = serde_json::from_str(json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("- [ ] Do something"), "got: {md}");
        assert!(!md.contains("adf-unsupported"), "got: {md}");
    }

    /// Issue #408: multiple taskItems with unwrapped inline content.
    #[test]
    fn adf_task_list_multiple_unwrapped_items() {
        let json = r#"{
            "version": 1,
            "type": "doc",
            "content": [{
                "type": "taskList",
                "attrs": {"localId": "list-001"},
                "content": [
                    {
                        "type": "taskItem",
                        "attrs": {"localId": "task-001", "state": "TODO"},
                        "content": [{"type": "text", "text": "First task"}]
                    },
                    {
                        "type": "taskItem",
                        "attrs": {"localId": "task-002", "state": "DONE"},
                        "content": [{"type": "text", "text": "Second task"}]
                    }
                ]
            }]
        }"#;
        let doc: AdfDocument = serde_json::from_str(json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("- [ ] First task"), "got: {md}");
        assert!(md.contains("- [x] Second task"), "got: {md}");
        assert!(!md.contains("adf-unsupported"), "got: {md}");
    }

    /// Issue #408: unwrapped inline content with marks (bold text).
    #[test]
    fn adf_task_item_unwrapped_inline_with_marks() {
        let json = r#"{
            "version": 1,
            "type": "doc",
            "content": [{
                "type": "taskList",
                "attrs": {"localId": "list-001"},
                "content": [{
                    "type": "taskItem",
                    "attrs": {"localId": "task-001", "state": "TODO"},
                    "content": [
                        {"type": "text", "text": "Buy "},
                        {"type": "text", "text": "groceries", "marks": [{"type": "strong"}]},
                        {"type": "text", "text": " today"}
                    ]
                }]
            }]
        }"#;
        let doc: AdfDocument = serde_json::from_str(json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("- [ ] Buy **groceries** today"), "got: {md}");
    }

    /// Issue #408: taskItem localId is preserved for unwrapped inline content.
    #[test]
    fn adf_task_item_unwrapped_preserves_local_id() {
        let json = r#"{
            "version": 1,
            "type": "doc",
            "content": [{
                "type": "taskList",
                "attrs": {"localId": "list-001"},
                "content": [{
                    "type": "taskItem",
                    "attrs": {"localId": "task-001", "state": "TODO"},
                    "content": [{"type": "text", "text": "Do something"}]
                }]
            }]
        }"#;
        let doc: AdfDocument = serde_json::from_str(json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("{localId=task-001}"), "got: {md}");
        assert!(md.contains("{localId=list-001}"), "got: {md}");
    }

    /// Issue #408: round-trip from Confluence ADF with unwrapped taskItem content.
    #[test]
    fn round_trip_task_list_unwrapped_inline() {
        let json = r#"{
            "version": 1,
            "type": "doc",
            "content": [{
                "type": "taskList",
                "attrs": {"localId": "list-001"},
                "content": [
                    {
                        "type": "taskItem",
                        "attrs": {"localId": "task-001", "state": "TODO"},
                        "content": [{"type": "text", "text": "Do something"}]
                    },
                    {
                        "type": "taskItem",
                        "attrs": {"localId": "task-002", "state": "DONE"},
                        "content": [{"type": "text", "text": "Already done"}]
                    }
                ]
            }]
        }"#;
        let doc: AdfDocument = serde_json::from_str(json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();

        // Round-trip: markdown back to ADF
        let doc2 = markdown_to_adf(&md).unwrap();
        assert_eq!(doc2.content[0].node_type, "taskList");

        let items = doc2.content[0].content.as_ref().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].attrs.as_ref().unwrap()["state"], "TODO");
        assert_eq!(items[1].attrs.as_ref().unwrap()["state"], "DONE");

        // localIds preserved
        assert_eq!(items[0].attrs.as_ref().unwrap()["localId"], "task-001");
        assert_eq!(items[1].attrs.as_ref().unwrap()["localId"], "task-002");
        assert_eq!(
            doc2.content[0].attrs.as_ref().unwrap()["localId"],
            "list-001"
        );
    }

    /// Issue #408: taskItem with inline content followed by a nested block (sub-list).
    #[test]
    fn adf_task_item_unwrapped_inline_then_block() {
        let json = r#"{
            "version": 1,
            "type": "doc",
            "content": [{
                "type": "taskList",
                "attrs": {"localId": "list-001"},
                "content": [{
                    "type": "taskItem",
                    "attrs": {"localId": "task-001", "state": "TODO"},
                    "content": [
                        {"type": "text", "text": "Parent task"},
                        {
                            "type": "bulletList",
                            "content": [{
                                "type": "listItem",
                                "content": [{
                                    "type": "paragraph",
                                    "content": [{"type": "text", "text": "sub-item"}]
                                }]
                            }]
                        }
                    ]
                }]
            }]
        }"#;
        let doc: AdfDocument = serde_json::from_str(json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("- [ ] Parent task"), "got: {md}");
        assert!(md.contains("  - sub-item"), "got: {md}");
        assert!(!md.contains("adf-unsupported"), "got: {md}");
    }

    /// Issue #408: taskItem with empty content array renders without panic.
    #[test]
    fn adf_task_item_empty_content() {
        let json = r#"{
            "version": 1,
            "type": "doc",
            "content": [{
                "type": "taskList",
                "attrs": {"localId": "list-001"},
                "content": [{
                    "type": "taskItem",
                    "attrs": {"localId": "task-001", "state": "TODO"},
                    "content": []
                }]
            }]
        }"#;
        let doc: AdfDocument = serde_json::from_str(json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("- [ ] "), "got: {md}");
        assert!(!md.contains("adf-unsupported"), "got: {md}");
    }

    /// Covers the else branch in render_list_item_content where the first
    /// child of a list item is a block node (not paragraph, not inline).
    #[test]
    fn adf_list_item_leading_block_node() {
        let json = r#"{
            "version": 1,
            "type": "doc",
            "content": [{
                "type": "bulletList",
                "content": [{
                    "type": "listItem",
                    "content": [{
                        "type": "codeBlock",
                        "attrs": {"language": "rust"},
                        "content": [{"type": "text", "text": "let x = 1;"}]
                    }]
                }]
            }]
        }"#;
        let doc: AdfDocument = serde_json::from_str(json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("```rust"), "got: {md}");
        assert!(md.contains("let x = 1;"), "got: {md}");
    }

    #[test]
    fn inline_bold() {
        let doc = markdown_to_adf("Some **bold** text").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert!(content.len() >= 3);
        let bold_node = &content[1];
        assert_eq!(bold_node.text.as_deref(), Some("bold"));
        let marks = bold_node.marks.as_ref().unwrap();
        assert_eq!(marks[0].mark_type, "strong");
    }

    #[test]
    fn inline_italic() {
        let doc = markdown_to_adf("Some *italic* text").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        let italic_node = &content[1];
        assert_eq!(italic_node.text.as_deref(), Some("italic"));
        let marks = italic_node.marks.as_ref().unwrap();
        assert_eq!(marks[0].mark_type, "em");
    }

    #[test]
    fn inline_code() {
        let doc = markdown_to_adf("Use `code` here").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        let code_node = &content[1];
        assert_eq!(code_node.text.as_deref(), Some("code"));
        let marks = code_node.marks.as_ref().unwrap();
        assert_eq!(marks[0].mark_type, "code");
    }

    #[test]
    fn inline_strikethrough() {
        let doc = markdown_to_adf("Some ~~deleted~~ text").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        let strike_node = &content[1];
        assert_eq!(strike_node.text.as_deref(), Some("deleted"));
        let marks = strike_node.marks.as_ref().unwrap();
        assert_eq!(marks[0].mark_type, "strike");
    }

    #[test]
    fn inline_link() {
        let doc = markdown_to_adf("Click [here](https://example.com) now").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        let link_node = &content[1];
        assert_eq!(link_node.text.as_deref(), Some("here"));
        let marks = link_node.marks.as_ref().unwrap();
        assert_eq!(marks[0].mark_type, "link");
    }

    #[test]
    fn block_image() {
        let md = "![Alt text](https://example.com/image.png)";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "mediaSingle");
    }

    #[test]
    fn table() {
        let md = "| A | B |\n| --- | --- |\n| 1 | 2 |";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "table");
        let rows = doc.content[0].content.as_ref().unwrap();
        assert_eq!(rows.len(), 2); // header + 1 body row
    }

    // ── adf_to_markdown tests ───────────────────────────────────────

    #[test]
    fn adf_paragraph_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::text("Hello world")])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert_eq!(md.trim(), "Hello world");
    }

    #[test]
    fn adf_heading_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::heading(2, vec![AdfNode::text("Title")])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert_eq!(md.trim(), "## Title");
    }

    #[test]
    fn adf_bold_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![
                AdfNode::text("Normal "),
                AdfNode::text_with_marks("bold", vec![AdfMark::strong()]),
                AdfNode::text(" text"),
            ])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert_eq!(md.trim(), "Normal **bold** text");
    }

    #[test]
    fn adf_code_block_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::code_block(Some("rust"), "let x = 1;")],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("```rust"));
        assert!(md.contains("let x = 1;"));
        assert!(md.contains("```"));
    }

    #[test]
    fn adf_rule_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::rule()],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("---"));
    }

    #[test]
    fn adf_bullet_list_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::bullet_list(vec![
                AdfNode::list_item(vec![AdfNode::paragraph(vec![AdfNode::text("A")])]),
                AdfNode::list_item(vec![AdfNode::paragraph(vec![AdfNode::text("B")])]),
            ])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("- A"));
        assert!(md.contains("- B"));
    }

    #[test]
    fn adf_link_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::text_with_marks(
                "click",
                vec![AdfMark::link("https://example.com")],
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert_eq!(md.trim(), "[click](https://example.com)");
    }

    #[test]
    fn unsupported_block_preserved_as_json() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode {
                node_type: "unknownBlock".to_string(),
                attrs: Some(serde_json::json!({"key": "value"})),
                content: None,
                text: None,
                marks: None,
                local_id: None,
                parameters: None,
            }],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("```adf-unsupported"));
        assert!(md.contains("\"unknownBlock\""));
    }

    #[test]
    fn unsupported_block_round_trips() {
        let original = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode {
                node_type: "unknownBlock".to_string(),
                attrs: Some(serde_json::json!({"key": "value"})),
                content: None,
                text: None,
                marks: None,
                local_id: None,
                parameters: None,
            }],
        };
        let md = adf_to_markdown(&original).unwrap();
        let restored = markdown_to_adf(&md).unwrap();
        assert_eq!(restored.content[0].node_type, "unknownBlock");
        assert_eq!(restored.content[0].attrs.as_ref().unwrap()["key"], "value");
    }

    // ── Round-trip tests ────────────────────────────────────────────

    #[test]
    fn round_trip_simple_document() {
        let md = "# Hello\n\nSome text with **bold** and *italic*.\n\n- Item 1\n- Item 2\n";
        let adf = markdown_to_adf(md).unwrap();
        let restored = adf_to_markdown(&adf).unwrap();

        assert!(restored.contains("# Hello"));
        assert!(restored.contains("**bold**"));
        assert!(restored.contains("*italic*"));
        assert!(restored.contains("- Item 1"));
        assert!(restored.contains("- Item 2"));
    }

    #[test]
    fn round_trip_code_block() {
        let md = "```python\nprint('hello')\n```\n";
        let adf = markdown_to_adf(md).unwrap();
        let restored = adf_to_markdown(&adf).unwrap();

        assert!(restored.contains("```python"));
        assert!(restored.contains("print('hello')"));
    }

    #[test]
    fn multiple_paragraphs() {
        let md = "First paragraph.\n\nSecond paragraph.\n";
        let adf = markdown_to_adf(md).unwrap();
        assert_eq!(adf.content.len(), 2);
        assert_eq!(adf.content[0].node_type, "paragraph");
        assert_eq!(adf.content[1].node_type, "paragraph");
    }

    // ── Additional markdown_to_adf tests ───────────────────────────────

    #[test]
    fn horizontal_rule_underscores() {
        let doc = markdown_to_adf("___").unwrap();
        assert_eq!(doc.content[0].node_type, "rule");
    }

    #[test]
    fn not_a_horizontal_rule_too_short() {
        let doc = markdown_to_adf("--").unwrap();
        assert_eq!(doc.content[0].node_type, "paragraph");
    }

    #[test]
    fn bullet_list_star_marker() {
        let md = "* Apple\n* Banana";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "bulletList");
        let items = doc.content[0].content.as_ref().unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn bullet_list_plus_marker() {
        let md = "+ One\n+ Two";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "bulletList");
    }

    #[test]
    fn ordered_list_non_one_start() {
        let md = "5. Fifth\n6. Sixth";
        let doc = markdown_to_adf(md).unwrap();
        let node = &doc.content[0];
        assert_eq!(node.node_type, "orderedList");
        let attrs = node.attrs.as_ref().unwrap();
        assert_eq!(attrs["order"], 5);
    }

    #[test]
    fn ordered_list_start_at_one_has_order_attr() {
        let md = "1. First\n2. Second";
        let doc = markdown_to_adf(md).unwrap();
        let node = &doc.content[0];
        assert_eq!(node.node_type, "orderedList");
        assert_eq!(node.attrs.as_ref().unwrap()["order"], 1);
    }

    #[test]
    fn blockquote_bare_marker() {
        // ">" with no space after
        let md = ">quoted text";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "blockquote");
    }

    #[test]
    fn image_no_alt() {
        let md = "![](https://example.com/img.png)";
        let doc = markdown_to_adf(md).unwrap();
        let node = &doc.content[0];
        assert_eq!(node.node_type, "mediaSingle");
        // media child should have no alt attr
        let media = &node.content.as_ref().unwrap()[0];
        let attrs = media.attrs.as_ref().unwrap();
        assert!(attrs.get("alt").is_none());
    }

    #[test]
    fn image_with_alt() {
        let md = "![A photo](https://example.com/photo.jpg)";
        let doc = markdown_to_adf(md).unwrap();
        let media = &doc.content[0].content.as_ref().unwrap()[0];
        let attrs = media.attrs.as_ref().unwrap();
        assert_eq!(attrs["alt"], "A photo");
    }

    #[test]
    fn table_multi_body_rows() {
        let md = "| H1 | H2 |\n| --- | --- |\n| a | b |\n| c | d |";
        let doc = markdown_to_adf(md).unwrap();
        let rows = doc.content[0].content.as_ref().unwrap();
        assert_eq!(rows.len(), 3); // header + 2 body rows
                                   // First row cells are tableHeader
        let header_cells = rows[0].content.as_ref().unwrap();
        assert_eq!(header_cells[0].node_type, "tableHeader");
        // Body row cells are tableCell
        let body_cells = rows[1].content.as_ref().unwrap();
        assert_eq!(body_cells[0].node_type, "tableCell");
    }

    #[test]
    fn table_no_separator_is_not_table() {
        // Pipe characters without a separator row should not parse as table
        let md = "| not | a table |";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "paragraph");
    }

    #[test]
    fn inline_underscore_bold() {
        let doc = markdown_to_adf("Some __bold__ text").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        let bold_node = &content[1];
        assert_eq!(bold_node.text.as_deref(), Some("bold"));
        let marks = bold_node.marks.as_ref().unwrap();
        assert_eq!(marks[0].mark_type, "strong");
    }

    #[test]
    fn inline_underscore_italic() {
        let doc = markdown_to_adf("Some _italic_ text").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        let italic_node = &content[1];
        assert_eq!(italic_node.text.as_deref(), Some("italic"));
        let marks = italic_node.marks.as_ref().unwrap();
        assert_eq!(marks[0].mark_type, "em");
    }

    #[test]
    fn intraword_underscore_not_emphasis() {
        // Single intraword underscore pair: do_something_useful
        let doc = markdown_to_adf("call do_something_useful now").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 1, "should be a single text node");
        assert_eq!(
            content[0].text.as_deref(),
            Some("call do_something_useful now")
        );
        assert!(content[0].marks.is_none());
    }

    #[test]
    fn intraword_underscore_multiple() {
        // Multiple intraword underscores: a_b_c_d
        let doc = markdown_to_adf("use a_b_c_d here").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].text.as_deref(), Some("use a_b_c_d here"));
        assert!(content[0].marks.is_none());
    }

    #[test]
    fn intraword_double_underscore_not_bold() {
        // Intraword double underscores: foo__bar__baz
        let doc = markdown_to_adf("foo__bar__baz").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].text.as_deref(), Some("foo__bar__baz"));
        assert!(content[0].marks.is_none());
    }

    #[test]
    fn intraword_triple_underscore_not_bold_italic() {
        // Intraword triple underscores: x___y___z
        let doc = markdown_to_adf("x___y___z").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].text.as_deref(), Some("x___y___z"));
        assert!(content[0].marks.is_none());
    }

    #[test]
    fn underscore_emphasis_still_works_with_spaces() {
        // Normal emphasis with spaces around delimiters should still work
        let doc = markdown_to_adf("some _italic_ here").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[1].text.as_deref(), Some("italic"));
        let marks = content[1].marks.as_ref().unwrap();
        assert_eq!(marks[0].mark_type, "em");
    }

    #[test]
    fn underscore_bold_still_works_with_spaces() {
        // Normal bold with spaces around delimiters should still work
        let doc = markdown_to_adf("some __bold__ here").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[1].text.as_deref(), Some("bold"));
        let marks = content[1].marks.as_ref().unwrap();
        assert_eq!(marks[0].mark_type, "strong");
    }

    #[test]
    fn intraword_underscore_closing_only() {
        // Opening _ is valid (preceded by space) but closing _ is intraword: _foo_bar
        let doc = markdown_to_adf("_foo_bar").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].text.as_deref(), Some("_foo_bar"));
    }

    #[test]
    fn intraword_double_underscore_closing_only() {
        // Opening __ is valid (at start) but closing __ is intraword: __foo__bar
        let doc = markdown_to_adf("__foo__bar").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].text.as_deref(), Some("__foo__bar"));
    }

    #[test]
    fn intraword_triple_underscore_closing_only() {
        // Opening ___ is valid (at start) but closing ___ is intraword: ___foo___bar
        let doc = markdown_to_adf("___foo___bar").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].text.as_deref(), Some("___foo___bar"));
    }

    #[test]
    fn asterisk_emphasis_unaffected_by_intraword_fix() {
        // Asterisks should still work for intraword emphasis (CommonMark allows this)
        let doc = markdown_to_adf("foo*bar*baz").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        // Asterisks CAN be intraword emphasis per CommonMark
        assert!(content.len() > 1 || content[0].marks.is_some());
    }

    #[test]
    fn intraword_underscore_at_start_of_text() {
        // Underscore at start of text is not intraword (no preceding alphanumeric)
        let doc = markdown_to_adf("_italic_ word").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content[0].text.as_deref(), Some("italic"));
        let marks = content[0].marks.as_ref().unwrap();
        assert_eq!(marks[0].mark_type, "em");
    }

    #[test]
    fn intraword_underscore_at_end_of_text() {
        // Underscore at end of text is not intraword (no following alphanumeric)
        let doc = markdown_to_adf("word _italic_").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        let last = content.last().unwrap();
        assert_eq!(last.text.as_deref(), Some("italic"));
        let marks = last.marks.as_ref().unwrap();
        assert_eq!(marks[0].mark_type, "em");
    }

    #[test]
    fn intraword_underscore_opening_only() {
        // Opening underscore is intraword but closing is not: a_b c_d
        // The first _ is intraword (a_b), so it shouldn't open emphasis
        let doc = markdown_to_adf("a_b c_d").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].text.as_deref(), Some("a_b c_d"));
    }

    #[test]
    fn intraword_underscore_roundtrip() {
        // The original reproducer from issue #438
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"call the do_something_useful function"}]}]}"#;
        let adf: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let jfm = adf_to_markdown(&adf).unwrap();
        let roundtripped = markdown_to_adf(&jfm).unwrap();
        let content = roundtripped.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 1, "should round-trip as a single text node");
        assert_eq!(
            content[0].text.as_deref(),
            Some("call the do_something_useful function")
        );
        assert!(content[0].marks.is_none());
    }

    #[test]
    fn asterisk_emphasis_roundtrip() {
        // The original reproducer from issue #452
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Status: *confirmed* and active"}]}]}"#;
        let adf: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let jfm = adf_to_markdown(&adf).unwrap();
        let roundtripped = markdown_to_adf(&jfm).unwrap();
        let content = roundtripped.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 1, "should round-trip as a single text node");
        assert_eq!(
            content[0].text.as_deref(),
            Some("Status: *confirmed* and active")
        );
        assert!(content[0].marks.is_none());
    }

    #[test]
    fn double_asterisk_roundtrip() {
        // **bold** delimiters in plain text should not become strong marks
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Use **kwargs in Python"}]}]}"#;
        let adf: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let jfm = adf_to_markdown(&adf).unwrap();
        let roundtripped = markdown_to_adf(&jfm).unwrap();
        let content = roundtripped.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 1, "should round-trip as a single text node");
        assert_eq!(content[0].text.as_deref(), Some("Use **kwargs in Python"));
        assert!(content[0].marks.is_none());
    }

    #[test]
    fn asterisk_with_em_mark_roundtrip() {
        // Text that already has an em mark should preserve both the mark and escaped content
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"a*b","marks":[{"type":"em"}]}]}]}"#;
        let adf: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let jfm = adf_to_markdown(&adf).unwrap();
        let roundtripped = markdown_to_adf(&jfm).unwrap();
        let content = roundtripped.content[0].content.as_ref().unwrap();
        // Find the node with em mark
        let em_node = content.iter().find(|n| {
            n.marks
                .as_ref()
                .is_some_and(|m| m.iter().any(|mk| mk.mark_type == "em"))
        });
        assert!(em_node.is_some(), "should have an em-marked node");
        assert_eq!(em_node.unwrap().text.as_deref(), Some("a*b"));
    }

    #[test]
    fn lone_asterisk_roundtrip() {
        // A single asterisk that cannot form emphasis should round-trip
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"rating: 5 * stars"}]}]}"#;
        let adf: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let jfm = adf_to_markdown(&adf).unwrap();
        let roundtripped = markdown_to_adf(&jfm).unwrap();
        let content = roundtripped.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 1, "should round-trip as a single text node");
        assert_eq!(content[0].text.as_deref(), Some("rating: 5 * stars"));
    }

    #[test]
    fn escape_emphasis_markers_unit() {
        assert_eq!(escape_emphasis_markers("hello"), "hello");
        assert_eq!(escape_emphasis_markers("*bold*"), r"\*bold\*");
        assert_eq!(escape_emphasis_markers("**strong**"), r"\*\*strong\*\*");
        assert_eq!(escape_emphasis_markers("no stars"), "no stars");
        assert_eq!(escape_emphasis_markers("a * b"), r"a \* b");
        assert_eq!(escape_emphasis_markers(""), "");
    }

    #[test]
    fn find_unescaped_skips_backslash_escaped() {
        // Escaped `**` should not be found
        assert_eq!(find_unescaped(r"a\*\*b**", "**"), Some(6));
        // No unescaped match at all
        assert_eq!(find_unescaped(r"a\*\*b", "**"), None);
        // Plain match without any escaping
        assert_eq!(find_unescaped("a**b", "**"), Some(1));
        // Empty haystack
        assert_eq!(find_unescaped("", "**"), None);
    }

    #[test]
    fn find_unescaped_char_skips_backslash_escaped() {
        // Escaped `*` should not be found
        assert_eq!(find_unescaped_char(r"a\*b*", b'*'), Some(4));
        // No unescaped match at all
        assert_eq!(find_unescaped_char(r"\*", b'*'), None);
        // Plain match
        assert_eq!(find_unescaped_char("a*b", b'*'), Some(1));
        // Empty haystack
        assert_eq!(find_unescaped_char("", b'*'), None);
    }

    #[test]
    fn double_asterisk_in_strong_mark_roundtrip() {
        // Text with ** inside a strong mark should preserve the literal **
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"call **kwargs","marks":[{"type":"strong"}]}]}]}"#;
        let adf: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let jfm = adf_to_markdown(&adf).unwrap();
        let roundtripped = markdown_to_adf(&jfm).unwrap();
        let content = roundtripped.content[0].content.as_ref().unwrap();
        let strong_node = content.iter().find(|n| {
            n.marks
                .as_ref()
                .is_some_and(|m| m.iter().any(|mk| mk.mark_type == "strong"))
        });
        assert!(strong_node.is_some(), "should have a strong-marked node");
        assert_eq!(strong_node.unwrap().text.as_deref(), Some("call **kwargs"));
    }

    #[test]
    fn backtick_code_roundtrip() {
        // The original reproducer from issue #453
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Set `max_retries` to 3 in the config"}]}]}"#;
        let adf: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let jfm = adf_to_markdown(&adf).unwrap();
        let roundtripped = markdown_to_adf(&jfm).unwrap();
        let content = roundtripped.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 1, "should round-trip as a single text node");
        assert_eq!(
            content[0].text.as_deref(),
            Some("Set `max_retries` to 3 in the config")
        );
        assert!(content[0].marks.is_none());
    }

    #[test]
    fn multiple_backtick_spans_roundtrip() {
        // Multiple backtick-delimited spans in a single text node
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Use `foo` and `bar` together"}]}]}"#;
        let adf: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let jfm = adf_to_markdown(&adf).unwrap();
        let roundtripped = markdown_to_adf(&jfm).unwrap();
        let content = roundtripped.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 1, "should round-trip as a single text node");
        assert_eq!(
            content[0].text.as_deref(),
            Some("Use `foo` and `bar` together")
        );
        assert!(content[0].marks.is_none());
    }

    #[test]
    fn lone_backtick_roundtrip() {
        // A single backtick that cannot form a code span should round-trip
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Use a ` character"}]}]}"#;
        let adf: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let jfm = adf_to_markdown(&adf).unwrap();
        let roundtripped = markdown_to_adf(&jfm).unwrap();
        let content = roundtripped.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 1, "should round-trip as a single text node");
        assert_eq!(content[0].text.as_deref(), Some("Use a ` character"));
        assert!(content[0].marks.is_none());
    }

    #[test]
    fn backtick_with_code_mark_roundtrip() {
        // Text that already has a code mark should preserve both the mark and content
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"max_retries","marks":[{"type":"code"}]}]}]}"#;
        let adf: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let jfm = adf_to_markdown(&adf).unwrap();
        assert_eq!(jfm.trim(), "`max_retries`");
        let roundtripped = markdown_to_adf(&jfm).unwrap();
        let content = roundtripped.content[0].content.as_ref().unwrap();
        let code_node = content.iter().find(|n| {
            n.marks
                .as_ref()
                .is_some_and(|m| m.iter().any(|mk| mk.mark_type == "code"))
        });
        assert!(code_node.is_some(), "should have a code-marked node");
        assert_eq!(code_node.unwrap().text.as_deref(), Some("max_retries"));
    }

    #[test]
    fn backtick_with_em_mark_roundtrip() {
        // Backtick inside em-marked text should preserve both
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"use `cfg`","marks":[{"type":"em"}]}]}]}"#;
        let adf: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let jfm = adf_to_markdown(&adf).unwrap();
        let roundtripped = markdown_to_adf(&jfm).unwrap();
        let content = roundtripped.content[0].content.as_ref().unwrap();
        let em_node = content.iter().find(|n| {
            n.marks
                .as_ref()
                .is_some_and(|m| m.iter().any(|mk| mk.mark_type == "em"))
        });
        assert!(em_node.is_some(), "should have an em-marked node");
        assert_eq!(em_node.unwrap().text.as_deref(), Some("use `cfg`"));
    }

    #[test]
    fn escape_backticks_unit() {
        assert_eq!(escape_backticks("hello"), "hello");
        assert_eq!(escape_backticks("`code`"), r"\`code\`");
        assert_eq!(escape_backticks("no ticks"), "no ticks");
        assert_eq!(escape_backticks("a ` b"), r"a \` b");
        assert_eq!(escape_backticks(""), "");
        assert_eq!(escape_backticks("`a` and `b`"), r"\`a\` and \`b\`");
    }

    #[test]
    fn heading_not_valid_without_space() {
        // "#Title" without space should be a paragraph, not heading
        let doc = markdown_to_adf("#Title").unwrap();
        assert_eq!(doc.content[0].node_type, "paragraph");
    }

    #[test]
    fn heading_level_too_high() {
        // ####### (7 hashes) is not a valid heading
        let doc = markdown_to_adf("####### Not a heading").unwrap();
        assert_eq!(doc.content[0].node_type, "paragraph");
    }

    #[test]
    fn empty_document() {
        let doc = markdown_to_adf("").unwrap();
        assert!(doc.content.is_empty());
    }

    #[test]
    fn only_blank_lines() {
        let doc = markdown_to_adf("\n\n\n").unwrap();
        assert!(doc.content.is_empty());
    }

    #[test]
    fn code_block_unterminated() {
        // Code block without closing fence
        let md = "```rust\nfn main() {}";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "codeBlock");
    }

    #[test]
    fn mixed_document() {
        let md = "# Title\n\nA paragraph.\n\n- Item\n\n```\ncode\n```\n\n> quote\n\n---\n\n1. numbered\n";
        let doc = markdown_to_adf(md).unwrap();
        let types: Vec<&str> = doc.content.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(
            types,
            vec![
                "heading",
                "paragraph",
                "bulletList",
                "codeBlock",
                "blockquote",
                "rule",
                "orderedList",
            ]
        );
    }

    // ── Additional adf_to_markdown tests ───────────────────────────────

    #[test]
    fn adf_ordered_list_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::ordered_list(
                vec![
                    AdfNode::list_item(vec![AdfNode::paragraph(vec![AdfNode::text("First")])]),
                    AdfNode::list_item(vec![AdfNode::paragraph(vec![AdfNode::text("Second")])]),
                ],
                None,
            )],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("1. First"));
        assert!(md.contains("2. Second"));
    }

    #[test]
    fn adf_ordered_list_custom_start() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::ordered_list(
                vec![AdfNode::list_item(vec![AdfNode::paragraph(vec![
                    AdfNode::text("Third"),
                ])])],
                Some(3),
            )],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("3. Third"));
    }

    #[test]
    fn adf_blockquote_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::blockquote(vec![AdfNode::paragraph(vec![
                AdfNode::text("A quote"),
            ])])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("> A quote"));
    }

    #[test]
    fn adf_table_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![
                AdfNode::table_row(vec![
                    AdfNode::table_header(vec![AdfNode::paragraph(vec![AdfNode::text("Name")])]),
                    AdfNode::table_header(vec![AdfNode::paragraph(vec![AdfNode::text("Value")])]),
                ]),
                AdfNode::table_row(vec![
                    AdfNode::table_cell(vec![AdfNode::paragraph(vec![AdfNode::text("a")])]),
                    AdfNode::table_cell(vec![AdfNode::paragraph(vec![AdfNode::text("1")])]),
                ]),
            ])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("| Name | Value |"));
        assert!(md.contains("| --- | --- |"));
        assert!(md.contains("| a | 1 |"));
    }

    #[test]
    fn adf_media_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::media_single(
                "https://example.com/img.png",
                Some("Alt"),
            )],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("![Alt](https://example.com/img.png)"));
    }

    #[test]
    fn adf_media_no_alt_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::media_single("https://example.com/img.png", None)],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("![](https://example.com/img.png)"));
    }

    #[test]
    fn adf_italic_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::text_with_marks(
                "emphasis",
                vec![AdfMark::em()],
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert_eq!(md.trim(), "*emphasis*");
    }

    #[test]
    fn adf_strikethrough_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::text_with_marks(
                "deleted",
                vec![AdfMark::strike()],
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert_eq!(md.trim(), "~~deleted~~");
    }

    #[test]
    fn adf_inline_code_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::text_with_marks(
                "code",
                vec![AdfMark::code()],
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert_eq!(md.trim(), "`code`");
    }

    #[test]
    fn adf_code_with_link_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::text_with_marks(
                "func",
                vec![AdfMark::code(), AdfMark::link("https://example.com")],
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert_eq!(md.trim(), "[`func`](https://example.com)");
    }

    #[test]
    fn adf_bold_italic_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::text_with_marks(
                "both",
                vec![AdfMark::strong(), AdfMark::em()],
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert_eq!(md.trim(), "***both***");
    }

    #[test]
    fn adf_bold_link_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::text_with_marks(
                "bold link",
                vec![AdfMark::strong(), AdfMark::link("https://example.com")],
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert_eq!(md.trim(), "**[bold link](https://example.com)**");
    }

    #[test]
    fn adf_strikethrough_bold_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::text_with_marks(
                "struck",
                vec![AdfMark::strike(), AdfMark::strong()],
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert_eq!(md.trim(), "~~**struck**~~");
    }

    #[test]
    fn adf_hard_break_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![
                AdfNode::text("Line 1"),
                AdfNode::hard_break(),
                AdfNode::text("Line 2"),
            ])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("Line 1\\\nLine 2"));
    }

    #[test]
    #[test]
    fn adf_unsupported_inline_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode {
                node_type: "unknownInline".to_string(),
                attrs: None,
                content: None,
                text: None,
                marks: None,
                local_id: None,
                parameters: None,
            }])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("<!-- unsupported inline: unknownInline -->"));
    }

    #[test]
    fn emoji_shortcode() {
        let doc = markdown_to_adf("Hello :wave: world").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content[0].text.as_deref(), Some("Hello "));
        assert_eq!(content[1].node_type, "emoji");
        assert_eq!(content[1].attrs.as_ref().unwrap()["shortName"], ":wave:");
        assert_eq!(content[2].text.as_deref(), Some(" world"));
    }

    #[test]
    fn adf_emoji_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::emoji("thumbsup")])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":thumbsup:"));
    }

    #[test]
    fn adf_emoji_with_colon_prefix_to_markdown() {
        // JIRA stores shortName as ":thumbsup:" with colons
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode {
                node_type: "emoji".to_string(),
                attrs: Some(serde_json::json!({"shortName": ":thumbsup:"})),
                content: None,
                text: None,
                marks: None,
                local_id: None,
                parameters: None,
            }])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":thumbsup:"));
        // Should not produce ::thumbsup:: (double colons)
        assert!(!md.contains("::thumbsup::"));
    }

    #[test]
    fn round_trip_emoji() {
        let md = "Hello :wave: world\n";
        let doc = markdown_to_adf(md).unwrap();
        let result = adf_to_markdown(&doc).unwrap();
        assert!(result.contains(":wave:"));
    }

    #[test]
    fn emoji_with_id_and_text_round_trips() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode {
                node_type: "emoji".to_string(),
                attrs: Some(
                    serde_json::json!({"shortName": ":check_mark:", "id": "2705", "text": "✅"}),
                ),
                content: None,
                text: None,
                marks: None,
                local_id: None,
                parameters: None,
            }])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":check_mark:"), "shortcode present: {md}");
        assert!(md.contains("id="), "id attr present: {md}");
        assert!(md.contains("text="), "text attr present: {md}");

        // Round-trip back to ADF
        let round_tripped = markdown_to_adf(&md).unwrap();
        let emoji = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let attrs = emoji.attrs.as_ref().unwrap();
        assert_eq!(attrs["shortName"], ":check_mark:");
        assert_eq!(attrs["id"], "2705");
        assert_eq!(attrs["text"], "✅");
    }

    #[test]
    fn emoji_without_extra_attrs_still_works() {
        let md = "Hello :wave: world\n";
        let doc = markdown_to_adf(md).unwrap();
        let emoji = &doc.content[0].content.as_ref().unwrap()[1];
        assert_eq!(emoji.attrs.as_ref().unwrap()["shortName"], ":wave:");
        // No id or text attrs when not provided
        assert!(emoji.attrs.as_ref().unwrap().get("id").is_none());
    }

    #[test]
    fn emoji_shortname_preserves_colons_round_trip() {
        // Issue #362: emoji shortName colons stripped during round-trip
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"emoji","attrs":{"shortName":":cross_mark:","id":"atlassian-cross_mark","text":"❌"}}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();

        // ADF → markdown → ADF round-trip
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let emoji = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let attrs = emoji.attrs.as_ref().unwrap();
        assert_eq!(
            attrs["shortName"], ":cross_mark:",
            "shortName should preserve colons, got: {}",
            attrs["shortName"]
        );
        assert_eq!(attrs["id"], "atlassian-cross_mark");
        assert_eq!(attrs["text"], "❌");
    }

    #[test]
    fn emoji_shortname_without_colons_preserved() {
        // Issue #379: shortName without colons should not gain colons
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"emoji","attrs":{"shortName":"white_check_mark","id":"2705","text":"✅"}}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let emoji = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let attrs = emoji.attrs.as_ref().unwrap();
        assert_eq!(
            attrs["shortName"], "white_check_mark",
            "shortName without colons should stay without colons, got: {}",
            attrs["shortName"]
        );
    }

    #[test]
    fn colon_in_text_not_emoji() {
        // A lone colon should not trigger emoji parsing
        let doc = markdown_to_adf("Time is 10:30 today").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].node_type, "text");
    }

    #[test]
    fn adf_inline_card_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode {
                node_type: "inlineCard".to_string(),
                attrs: Some(
                    serde_json::json!({"url": "https://org.atlassian.net/browse/ACCS-4382"}),
                ),
                content: None,
                text: None,
                marks: None,
                local_id: None,
                parameters: None,
            }])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":card[https://org.atlassian.net/browse/ACCS-4382]"));
        assert!(!md.contains("<!-- unsupported inline"));
    }

    #[test]
    fn inline_card_directive_round_trips() {
        // inlineCard → :card[url] → inlineCard
        let original = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::inline_card(
                "https://org.atlassian.net/browse/ACCS-4382",
            )])],
        };
        let md = adf_to_markdown(&original).unwrap();
        assert!(md.contains(":card[https://org.atlassian.net/browse/ACCS-4382]"));
        let restored = markdown_to_adf(&md).unwrap();
        let node = &restored.content[0].content.as_ref().unwrap()[0];
        assert_eq!(node.node_type, "inlineCard");
        assert_eq!(
            node.attrs.as_ref().unwrap()["url"],
            "https://org.atlassian.net/browse/ACCS-4382"
        );
    }

    #[test]
    fn inline_card_directive_parsed_from_jfm() {
        // :card[url] in JFM → inlineCard in ADF
        let doc = markdown_to_adf("See :card[https://example.com/issue/123] for details.").unwrap();
        let nodes = doc.content[0].content.as_ref().unwrap();
        assert_eq!(nodes[0].node_type, "text");
        assert_eq!(nodes[0].text.as_deref(), Some("See "));
        assert_eq!(nodes[1].node_type, "inlineCard");
        assert_eq!(
            nodes[1].attrs.as_ref().unwrap()["url"],
            "https://example.com/issue/123"
        );
        assert_eq!(nodes[2].node_type, "text");
        assert_eq!(nodes[2].text.as_deref(), Some(" for details."));
    }

    #[test]
    fn self_link_becomes_link_mark_not_inline_card() {
        // Issue #378: [url](url) should produce a link mark, not inlineCard.
        // inlineCard is only for :card[url] directives and bare URLs.
        let doc = markdown_to_adf("[https://example.com](https://example.com)").unwrap();
        let node = &doc.content[0].content.as_ref().unwrap()[0];
        assert_eq!(node.node_type, "text");
        assert_eq!(node.text.as_deref(), Some("https://example.com"));
        let mark = &node.marks.as_ref().unwrap()[0];
        assert_eq!(mark.mark_type, "link");
        assert_eq!(mark.attrs.as_ref().unwrap()["href"], "https://example.com");
    }

    #[test]
    fn named_link_does_not_become_inline_card() {
        // [#4668](url) — text differs from url, stays as a link mark
        let doc = markdown_to_adf("[#4668](https://github.com/org/repo/pull/4668)").unwrap();
        let node = &doc.content[0].content.as_ref().unwrap()[0];
        assert_eq!(node.node_type, "text");
        assert_eq!(node.text.as_deref(), Some("#4668"));
        let mark = &node.marks.as_ref().unwrap()[0];
        assert_eq!(mark.mark_type, "link");
    }

    #[test]
    fn adf_inline_card_no_url_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode {
                node_type: "inlineCard".to_string(),
                attrs: Some(serde_json::json!({})),
                content: None,
                text: None,
                marks: None,
                local_id: None,
                parameters: None,
            }])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        // No url attr — renders nothing (not a comment)
        assert!(!md.contains("<!-- unsupported inline"));
    }

    #[test]
    fn adf_code_block_no_language_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::code_block(None, "plain code")],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("```\n"));
        assert!(md.contains("plain code"));
    }

    // ── Additional round-trip tests ────────────────────────────────────

    #[test]
    fn round_trip_table() {
        let md = "| A | B |\n| --- | --- |\n| 1 | 2 |\n";
        let adf = markdown_to_adf(md).unwrap();
        let restored = adf_to_markdown(&adf).unwrap();
        assert!(restored.contains("| A | B |"));
        assert!(restored.contains("| 1 | 2 |"));
    }

    #[test]
    fn round_trip_blockquote() {
        let md = "> This is quoted\n";
        let adf = markdown_to_adf(md).unwrap();
        let restored = adf_to_markdown(&adf).unwrap();
        assert!(restored.contains("> This is quoted"));
    }

    #[test]
    fn round_trip_image() {
        let md = "![Logo](https://example.com/logo.png)\n";
        let adf = markdown_to_adf(md).unwrap();
        let restored = adf_to_markdown(&adf).unwrap();
        assert!(restored.contains("![Logo](https://example.com/logo.png)"));
    }

    #[test]
    fn round_trip_ordered_list() {
        let md = "1. A\n2. B\n3. C\n";
        let adf = markdown_to_adf(md).unwrap();
        let restored = adf_to_markdown(&adf).unwrap();
        assert!(restored.contains("1. A"));
        assert!(restored.contains("2. B"));
        assert!(restored.contains("3. C"));
    }

    #[test]
    fn round_trip_inline_marks() {
        let md = "Text with `code` and ~~strike~~ and [link](https://x.com).\n";
        let adf = markdown_to_adf(md).unwrap();
        let restored = adf_to_markdown(&adf).unwrap();
        assert!(restored.contains("`code`"));
        assert!(restored.contains("~~strike~~"));
        assert!(restored.contains("[link](https://x.com)"));
    }

    // ── Container directive tests (Tier 2) ───────────────────────────

    #[test]
    fn panel_info() {
        let md = ":::panel{type=info}\nThis is informational.\n:::";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "panel");
        assert_eq!(doc.content[0].attrs.as_ref().unwrap()["panelType"], "info");
        let inner = doc.content[0].content.as_ref().unwrap();
        assert_eq!(inner[0].node_type, "paragraph");
    }

    #[test]
    fn adf_panel_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::panel(
                "warning",
                vec![AdfNode::paragraph(vec![AdfNode::text("Be careful.")])],
            )],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":::panel{type=warning}"));
        assert!(md.contains("Be careful."));
        assert!(md.contains(":::"));
    }

    #[test]
    fn round_trip_panel() {
        let md = ":::panel{type=info}\nThis is informational.\n:::\n";
        let doc = markdown_to_adf(md).unwrap();
        let result = adf_to_markdown(&doc).unwrap();
        assert!(result.contains(":::panel{type=info}"));
        assert!(result.contains("This is informational."));
    }

    #[test]
    fn expand_with_title() {
        let md = ":::expand{title=\"Click me\"}\nHidden content.\n:::";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "expand");
        assert_eq!(doc.content[0].attrs.as_ref().unwrap()["title"], "Click me");
    }

    #[test]
    fn adf_expand_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::expand(
                Some("Details"),
                vec![AdfNode::paragraph(vec![AdfNode::text("Inner.")])],
            )],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":::expand{title=\"Details\"}"));
        assert!(md.contains("Inner."));
    }

    #[test]
    fn round_trip_expand() {
        let md = ":::expand{title=\"Details\"}\nInner content.\n:::\n";
        let doc = markdown_to_adf(md).unwrap();
        let result = adf_to_markdown(&doc).unwrap();
        assert!(result.contains(":::expand{title=\"Details\"}"));
        assert!(result.contains("Inner content."));
    }

    #[test]
    fn layout_two_columns() {
        let md =
            "::::layout\n:::column{width=50}\nLeft.\n:::\n:::column{width=50}\nRight.\n:::\n::::";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "layoutSection");
        let columns = doc.content[0].content.as_ref().unwrap();
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].node_type, "layoutColumn");
        assert_eq!(columns[1].node_type, "layoutColumn");
    }

    #[test]
    fn adf_layout_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::layout_section(vec![
                AdfNode::layout_column(
                    50.0,
                    vec![AdfNode::paragraph(vec![AdfNode::text("Left.")])],
                ),
                AdfNode::layout_column(
                    50.0,
                    vec![AdfNode::paragraph(vec![AdfNode::text("Right.")])],
                ),
            ])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("::::layout"));
        assert!(md.contains(":::column{width=50}"));
        assert!(md.contains("Left."));
        assert!(md.contains("Right."));
    }

    #[test]
    fn decisions_list() {
        let md = ":::decisions\n- <> Use PostgreSQL\n- <> REST API\n:::";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "decisionList");
        let items = doc.content[0].content.as_ref().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].attrs.as_ref().unwrap()["state"], "DECIDED");
    }

    #[test]
    fn adf_decisions_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::decision_list(vec![AdfNode::decision_item(
                "DECIDED",
                vec![AdfNode::paragraph(vec![AdfNode::text("Use PostgreSQL")])],
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":::decisions"));
        assert!(md.contains("- <> Use PostgreSQL"));
    }

    #[test]
    fn bodied_extension_container() {
        let md = ":::extension{type=com.forge key=my-macro}\nContent.\n:::";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "bodiedExtension");
        assert_eq!(
            doc.content[0].attrs.as_ref().unwrap()["extensionType"],
            "com.forge"
        );
    }

    #[test]
    fn adf_bodied_extension_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::bodied_extension(
                "com.forge",
                "my-macro",
                vec![AdfNode::paragraph(vec![AdfNode::text("Content.")])],
            )],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":::extension{type=com.forge key=my-macro}"));
        assert!(md.contains("Content."));
    }

    // ── Leaf block directive tests (Tier 3) ──────────────────────────

    #[test]
    fn leaf_block_card() {
        let doc = markdown_to_adf("::card[https://example.com/browse/PROJ-123]").unwrap();
        assert_eq!(doc.content[0].node_type, "blockCard");
        assert_eq!(
            doc.content[0].attrs.as_ref().unwrap()["url"],
            "https://example.com/browse/PROJ-123"
        );
    }

    #[test]
    fn adf_block_card_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::block_card("https://example.com/browse/PROJ-123")],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("::card[https://example.com/browse/PROJ-123]"));
    }

    #[test]
    fn round_trip_block_card() {
        let md = "::card[https://example.com/browse/PROJ-123]\n";
        let doc = markdown_to_adf(md).unwrap();
        let result = adf_to_markdown(&doc).unwrap();
        assert!(result.contains("::card[https://example.com/browse/PROJ-123]"));
    }

    #[test]
    fn leaf_embed_card() {
        let doc =
            markdown_to_adf("::embed[https://figma.com/file/abc]{layout=wide width=80}").unwrap();
        assert_eq!(doc.content[0].node_type, "embedCard");
        let attrs = doc.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["url"], "https://figma.com/file/abc");
        assert_eq!(attrs["layout"], "wide");
        assert_eq!(attrs["width"], 80.0);
    }

    #[test]
    fn leaf_embed_card_with_original_height() {
        let doc = markdown_to_adf(
            "::embed[https://example.com]{layout=center originalHeight=732 width=100}",
        )
        .unwrap();
        assert_eq!(doc.content[0].node_type, "embedCard");
        let attrs = doc.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["url"], "https://example.com");
        assert_eq!(attrs["layout"], "center");
        assert_eq!(attrs["originalHeight"], 732.0);
        assert_eq!(attrs["width"], 100.0);
    }

    #[test]
    fn adf_embed_card_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::embed_card(
                "https://figma.com/file/abc",
                Some("wide"),
                None,
                Some(80.0),
            )],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("::embed[https://figma.com/file/abc]{layout=wide width=80}"));
    }

    #[test]
    fn adf_embed_card_original_height_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::embed_card(
                "https://example.com",
                Some("center"),
                Some(732.0),
                Some(100.0),
            )],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("::embed[https://example.com]{layout=center originalHeight=732 width=100}"),
            "expected originalHeight and width in md: {md}"
        );
    }

    #[test]
    fn embed_card_roundtrip_with_all_attrs() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{
            "type":"embedCard",
            "attrs":{"layout":"center","originalHeight":732.0,"url":"https://example.com","width":100.0}
        }]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("originalHeight=732"),
            "originalHeight missing from md: {md}"
        );
        assert!(md.contains("width=100"), "width missing from md: {md}");
        let rt = markdown_to_adf(&md).unwrap();
        let attrs = rt.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["originalHeight"], 732.0);
        assert_eq!(attrs["width"], 100.0);
        assert_eq!(attrs["layout"], "center");
        assert_eq!(attrs["url"], "https://example.com");
    }

    #[test]
    fn embed_card_fractional_dimensions() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::embed_card(
                "https://example.com",
                Some("center"),
                Some(732.5),
                Some(99.9),
            )],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("originalHeight=732.5"),
            "fractional originalHeight missing: {md}"
        );
        assert!(md.contains("width=99.9"), "fractional width missing: {md}");
        let rt = markdown_to_adf(&md).unwrap();
        let attrs = rt.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["originalHeight"], 732.5);
        assert_eq!(attrs["width"], 99.9);
    }

    #[test]
    fn embed_card_integer_width_in_json() {
        // JSON integer (not float) should also be extracted via as_f64()
        let adf_json = r#"{"version":1,"type":"doc","content":[{
            "type":"embedCard",
            "attrs":{"url":"https://example.com","width":100}
        }]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("width=100"),
            "integer width missing from md: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        assert_eq!(rt.content[0].attrs.as_ref().unwrap()["width"], 100.0);
    }

    #[test]
    fn embed_card_only_original_height() {
        // originalHeight without width
        let adf_json = r#"{"version":1,"type":"doc","content":[{
            "type":"embedCard",
            "attrs":{"url":"https://example.com","originalHeight":500.0}
        }]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("originalHeight=500"),
            "originalHeight missing: {md}"
        );
        assert!(!md.contains("width="), "width should not appear: {md}");
        let rt = markdown_to_adf(&md).unwrap();
        let attrs = rt.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["originalHeight"], 500.0);
        assert!(attrs.get("width").is_none());
    }

    #[test]
    fn leaf_void_extension() {
        let doc = markdown_to_adf("::extension{type=com.atlassian.macro key=jira-chart}").unwrap();
        assert_eq!(doc.content[0].node_type, "extension");
        assert_eq!(
            doc.content[0].attrs.as_ref().unwrap()["extensionType"],
            "com.atlassian.macro"
        );
        assert_eq!(
            doc.content[0].attrs.as_ref().unwrap()["extensionKey"],
            "jira-chart"
        );
    }

    #[test]
    fn adf_void_extension_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::extension(
                "com.atlassian.macro",
                "jira-chart",
                None,
            )],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("::extension{type=com.atlassian.macro key=jira-chart}"));
    }

    // ── Bare URL autolink tests ──────────────────────────────────────

    #[test]
    fn bare_url_autolink() {
        let doc = markdown_to_adf("Visit https://example.com today").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content[0].text.as_deref(), Some("Visit "));
        assert_eq!(content[1].node_type, "inlineCard");
        assert_eq!(
            content[1].attrs.as_ref().unwrap()["url"],
            "https://example.com"
        );
        assert_eq!(content[2].text.as_deref(), Some(" today"));
    }

    #[test]
    fn bare_url_strips_trailing_punctuation() {
        let doc = markdown_to_adf("See https://example.com.").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(
            content[1].attrs.as_ref().unwrap()["url"],
            "https://example.com"
        );
    }

    #[test]
    fn bare_url_round_trip() {
        let doc = markdown_to_adf("Visit https://example.com/path today").unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":card[https://example.com/path]"));
    }

    // ── Block-level attribute marks (Tier 5/6) ───────────────────────

    #[test]
    fn paragraph_align_center() {
        let md = "Centered text.\n{align=center}";
        let doc = markdown_to_adf(md).unwrap();
        let marks = doc.content[0].marks.as_ref().unwrap();
        assert_eq!(marks[0].mark_type, "alignment");
        assert_eq!(marks[0].attrs.as_ref().unwrap()["align"], "center");
    }

    #[test]
    fn adf_alignment_to_markdown() {
        let mut node = AdfNode::paragraph(vec![AdfNode::text("Centered.")]);
        node.marks = Some(vec![AdfMark::alignment("center")]);
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![node],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("Centered."));
        assert!(md.contains("{align=center}"));
    }

    #[test]
    fn round_trip_alignment() {
        let md = "Centered.\n{align=center}\n";
        let doc = markdown_to_adf(md).unwrap();
        let result = adf_to_markdown(&doc).unwrap();
        assert!(result.contains("{align=center}"));
    }

    #[test]
    fn paragraph_indent() {
        let md = "Indented.\n{indent=2}";
        let doc = markdown_to_adf(md).unwrap();
        let marks = doc.content[0].marks.as_ref().unwrap();
        assert_eq!(marks[0].mark_type, "indentation");
        assert_eq!(marks[0].attrs.as_ref().unwrap()["level"], 2);
    }

    #[test]
    fn code_block_breakout() {
        let md = "```python\ndef f(): pass\n```\n{breakout=wide}";
        let doc = markdown_to_adf(md).unwrap();
        let marks = doc.content[0].marks.as_ref().unwrap();
        assert_eq!(marks[0].mark_type, "breakout");
        assert_eq!(marks[0].attrs.as_ref().unwrap()["mode"], "wide");
        assert!(marks[0].attrs.as_ref().unwrap().get("width").is_none());
    }

    #[test]
    fn code_block_breakout_with_width() {
        let md = "```python\ndef f(): pass\n```\n{breakout=wide breakoutWidth=1200}";
        let doc = markdown_to_adf(md).unwrap();
        let marks = doc.content[0].marks.as_ref().unwrap();
        assert_eq!(marks[0].mark_type, "breakout");
        assert_eq!(marks[0].attrs.as_ref().unwrap()["mode"], "wide");
        assert_eq!(marks[0].attrs.as_ref().unwrap()["width"], 1200);
    }

    #[test]
    fn adf_breakout_to_markdown() {
        let mut node = AdfNode::code_block(Some("python"), "pass");
        node.marks = Some(vec![AdfMark::breakout("wide", None)]);
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![node],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("{breakout=wide}"));
        assert!(!md.contains("breakoutWidth"));
    }

    #[test]
    fn adf_breakout_with_width_to_markdown() {
        let mut node = AdfNode::code_block(Some("python"), "pass");
        node.marks = Some(vec![AdfMark::breakout("wide", Some(1200))]);
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![node],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("breakout=wide"));
        assert!(md.contains("breakoutWidth=1200"));
    }

    #[test]
    fn breakout_width_round_trip() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{
            "type":"codeBlock",
            "attrs":{"language":"text"},
            "marks":[{"type":"breakout","attrs":{"mode":"wide","width":1200}}],
            "content":[{"type":"text","text":"some code"}]
        }]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("breakout=wide"));
        assert!(md.contains("breakoutWidth=1200"));
        let round_tripped = markdown_to_adf(&md).unwrap();
        let marks = round_tripped.content[0].marks.as_ref().unwrap();
        let breakout = marks.iter().find(|m| m.mark_type == "breakout").unwrap();
        assert_eq!(breakout.attrs.as_ref().unwrap()["mode"], "wide");
        assert_eq!(breakout.attrs.as_ref().unwrap()["width"], 1200);
    }

    // ── Attribute extensions — media & table (Tier 5) ────────────────

    #[test]
    fn image_with_layout_attrs() {
        let doc = markdown_to_adf("![alt](url){layout=wide width=80}").unwrap();
        let node = &doc.content[0];
        assert_eq!(node.node_type, "mediaSingle");
        let attrs = node.attrs.as_ref().unwrap();
        assert_eq!(attrs["layout"], "wide");
        assert_eq!(attrs["width"], 80);
    }

    #[test]
    fn adf_image_with_layout_to_markdown() {
        let mut node = AdfNode::media_single("url", Some("alt"));
        node.attrs.as_mut().unwrap()["layout"] = serde_json::json!("wide");
        node.attrs.as_mut().unwrap()["width"] = serde_json::json!(80);
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![node],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("![alt](url){layout=wide width=80}"));
    }

    #[test]
    fn table_with_layout_attrs() {
        let md = "| H |\n| --- |\n| C |\n{layout=wide numbered}";
        let doc = markdown_to_adf(md).unwrap();
        let table = &doc.content[0];
        assert_eq!(table.node_type, "table");
        let attrs = table.attrs.as_ref().unwrap();
        assert_eq!(attrs["layout"], "wide");
        assert_eq!(attrs["isNumberColumnEnabled"], true);
    }

    #[test]
    fn adf_table_with_attrs_to_markdown() {
        let mut table = AdfNode::table(vec![
            AdfNode::table_row(vec![AdfNode::table_header(vec![AdfNode::paragraph(vec![
                AdfNode::text("H"),
            ])])]),
            AdfNode::table_row(vec![AdfNode::table_cell(vec![AdfNode::paragraph(vec![
                AdfNode::text("C"),
            ])])]),
        ]);
        table.attrs = Some(serde_json::json!({"layout": "wide", "isNumberColumnEnabled": true}));
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![table],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("{layout=wide numbered}"));
    }

    // ── Attribute extensions — inline marks (Tier 5) ─────────────────

    #[test]
    fn underline_bracketed_span() {
        let doc = markdown_to_adf("This is [underlined text]{underline} here.").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content[1].text.as_deref(), Some("underlined text"));
        let marks = content[1].marks.as_ref().unwrap();
        assert_eq!(marks[0].mark_type, "underline");
    }

    #[test]
    fn adf_underline_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::text_with_marks(
                "underlined",
                vec![AdfMark::underline()],
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("[underlined]{underline}"));
    }

    #[test]
    fn round_trip_underline() {
        let md = "This is [underlined text]{underline} here.\n";
        let doc = markdown_to_adf(md).unwrap();
        let result = adf_to_markdown(&doc).unwrap();
        assert!(result.contains("[underlined text]{underline}"));
    }

    #[test]
    fn mark_ordering_underline_strong_preserved() {
        // Issue #383: mark ordering was non-deterministic
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"bold and underlined","marks":[{"type":"underline"},{"type":"strong"}]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let mark_types: Vec<&str> = node
            .marks
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mark_type.as_str())
            .collect();
        assert_eq!(
            mark_types,
            vec!["underline", "strong"],
            "mark order should be preserved, got: {mark_types:?}"
        );
    }

    #[test]
    fn mark_ordering_link_strong_preserved() {
        // Issue #403: link+strong mark order was swapped on round-trip
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"bold link","marks":[
            {"type":"link","attrs":{"href":"https://example.com"}},
            {"type":"strong"}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let mark_types: Vec<&str> = node
            .marks
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mark_type.as_str())
            .collect();
        assert_eq!(
            mark_types,
            vec!["link", "strong"],
            "mark order should be preserved, got: {mark_types:?}"
        );
    }

    #[test]
    fn mark_ordering_link_textcolor_preserved() {
        // Issue #403 comment: link+textColor mark order was swapped on round-trip
        let adf_json = r##"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"red link","marks":[
            {"type":"link","attrs":{"href":"https://example.com"}},
            {"type":"textColor","attrs":{"color":"#ff0000"}}
          ]}
        ]}]}"##;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let mark_types: Vec<&str> = node
            .marks
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mark_type.as_str())
            .collect();
        assert_eq!(
            mark_types,
            vec!["link", "textColor"],
            "mark order should be preserved, got: {mark_types:?}"
        );
    }

    #[test]
    fn mark_ordering_link_em_preserved() {
        // Issue #403: link+em mark order should be preserved
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"italic link","marks":[
            {"type":"link","attrs":{"href":"https://example.com"}},
            {"type":"em"}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let mark_types: Vec<&str> = node
            .marks
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mark_type.as_str())
            .collect();
        assert_eq!(
            mark_types,
            vec!["link", "em"],
            "mark order should be preserved, got: {mark_types:?}"
        );
    }

    #[test]
    fn mark_ordering_link_strike_preserved() {
        // Issue #403: link+strike mark order should be preserved
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"struck link","marks":[
            {"type":"link","attrs":{"href":"https://example.com"}},
            {"type":"strike"}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let mark_types: Vec<&str> = node
            .marks
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mark_type.as_str())
            .collect();
        assert_eq!(
            mark_types,
            vec!["link", "strike"],
            "mark order should be preserved, got: {mark_types:?}"
        );
    }

    #[test]
    fn mark_ordering_strong_link_preserved() {
        // Issue #403: [strong, link] order must also be preserved (reverse direction)
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"bold link","marks":[
            {"type":"strong"},
            {"type":"link","attrs":{"href":"https://example.com"}}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let mark_types: Vec<&str> = node
            .marks
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mark_type.as_str())
            .collect();
        assert_eq!(
            mark_types,
            vec!["strong", "link"],
            "mark order should be preserved, got: {mark_types:?}"
        );
    }

    #[test]
    fn mark_ordering_em_link_preserved() {
        // Issue #403: [em, link] order must also be preserved
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"italic link","marks":[
            {"type":"em"},
            {"type":"link","attrs":{"href":"https://example.com"}}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let mark_types: Vec<&str> = node
            .marks
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mark_type.as_str())
            .collect();
        assert_eq!(
            mark_types,
            vec!["em", "link"],
            "mark order should be preserved, got: {mark_types:?}"
        );
    }

    #[test]
    fn mark_ordering_strike_link_preserved() {
        // Issue #403: [strike, link] order must also be preserved
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"struck link","marks":[
            {"type":"strike"},
            {"type":"link","attrs":{"href":"https://example.com"}}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let mark_types: Vec<&str> = node
            .marks
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mark_type.as_str())
            .collect();
        assert_eq!(
            mark_types,
            vec!["strike", "link"],
            "mark order should be preserved, got: {mark_types:?}"
        );
    }

    #[test]
    fn mark_ordering_underline_link_preserved() {
        // Issue #403: [underline, link] order must be preserved
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"click here","marks":[
            {"type":"underline"},
            {"type":"link","attrs":{"href":"https://example.com"}}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let mark_types: Vec<&str> = node
            .marks
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mark_type.as_str())
            .collect();
        assert_eq!(
            mark_types,
            vec!["underline", "link"],
            "mark order should be preserved, got: {mark_types:?}"
        );
    }

    #[test]
    fn mark_ordering_textcolor_link_preserved() {
        // Issue #403: [textColor, link] order must be preserved
        let adf_json = r##"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"red link","marks":[
            {"type":"textColor","attrs":{"color":"#ff0000"}},
            {"type":"link","attrs":{"href":"https://example.com"}}
          ]}
        ]}]}"##;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let mark_types: Vec<&str> = node
            .marks
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mark_type.as_str())
            .collect();
        assert_eq!(
            mark_types,
            vec!["textColor", "link"],
            "mark order should be preserved, got: {mark_types:?}"
        );
    }

    #[test]
    fn mark_ordering_link_underline_preserved() {
        // Issue #403: [link, underline] order must be preserved (link wraps bracketed span)
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"click here","marks":[
            {"type":"link","attrs":{"href":"https://example.com"}},
            {"type":"underline"}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        // Link should wrap the underline bracketed span: [[click here]{underline}](url)
        assert!(
            md.contains("](https://example.com)"),
            "should have link: {md}"
        );
        assert!(md.contains("underline"), "should have underline: {md}");
        let round_tripped = markdown_to_adf(&md).unwrap();
        let node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let mark_types: Vec<&str> = node
            .marks
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mark_type.as_str())
            .collect();
        assert_eq!(
            mark_types,
            vec!["link", "underline"],
            "mark order should be preserved, got: {mark_types:?}"
        );
    }

    #[test]
    fn em_strong_round_trip() {
        // Issue #401: em mark dropped when combined with strong
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"bold and italic","marks":[{"type":"strong"},{"type":"em"}]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert_eq!(md.trim(), "***bold and italic***");
        let round_tripped = markdown_to_adf(&md).unwrap();
        let node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        assert_eq!(node.text.as_deref(), Some("bold and italic"));
        let mark_types: Vec<&str> = node
            .marks
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mark_type.as_str())
            .collect();
        assert_eq!(
            mark_types,
            vec!["strong", "em"],
            "both strong and em marks should be preserved, got: {mark_types:?}"
        );
    }

    #[test]
    fn em_strong_round_trip_em_first() {
        // Issue #401: em+strong with em listed first
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"italic and bold","marks":[{"type":"em"},{"type":"strong"}]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        assert_eq!(node.text.as_deref(), Some("italic and bold"));
        let mark_types: Vec<&str> = node
            .marks
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mark_type.as_str())
            .collect();
        assert!(
            mark_types.contains(&"strong") && mark_types.contains(&"em"),
            "both strong and em marks should be present, got: {mark_types:?}"
        );
    }

    #[test]
    fn triple_asterisk_parse_to_adf() {
        // Issue #401: ***text*** should parse as text with strong+em marks
        let md = "***bold and italic***\n";
        let doc = markdown_to_adf(md).unwrap();
        let node = &doc.content[0].content.as_ref().unwrap()[0];
        assert_eq!(node.text.as_deref(), Some("bold and italic"));
        let mark_types: Vec<&str> = node
            .marks
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mark_type.as_str())
            .collect();
        assert!(
            mark_types.contains(&"strong") && mark_types.contains(&"em"),
            "***text*** should produce both strong and em marks, got: {mark_types:?}"
        );
    }

    #[test]
    fn triple_asterisk_with_surrounding_text() {
        // Issue #401: surrounding text should not be affected
        let md = "before ***bold italic*** after\n";
        let doc = markdown_to_adf(md).unwrap();
        let nodes = doc.content[0].content.as_ref().unwrap();
        // Should have: "before " (plain), "bold italic" (strong+em), " after" (plain)
        assert!(
            nodes.len() >= 3,
            "expected at least 3 nodes, got {}",
            nodes.len()
        );
        assert_eq!(nodes[0].text.as_deref(), Some("before "));
        assert_eq!(nodes[1].text.as_deref(), Some("bold italic"));
        let mark_types: Vec<&str> = nodes[1]
            .marks
            .as_ref()
            .unwrap()
            .iter()
            .map(|m| m.mark_type.as_str())
            .collect();
        assert!(
            mark_types.contains(&"strong") && mark_types.contains(&"em"),
            "middle node should have strong+em, got: {mark_types:?}"
        );
        assert_eq!(nodes[2].text.as_deref(), Some(" after"));
    }

    #[test]
    fn annotation_mark_round_trip() {
        // Issue #364: annotation marks were silently dropped
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"highlighted text","marks":[
            {"type":"annotation","attrs":{"id":"abc123","annotationType":"inlineComment"}}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();

        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("annotation-id="),
            "JFM should contain annotation-id, got: {md}"
        );

        let round_tripped = markdown_to_adf(&md).unwrap();
        let text_node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        assert_eq!(text_node.text.as_deref(), Some("highlighted text"));
        let marks = text_node.marks.as_ref().expect("should have marks");
        let ann = marks
            .iter()
            .find(|m| m.mark_type == "annotation")
            .expect("should have annotation mark");
        let attrs = ann.attrs.as_ref().unwrap();
        assert_eq!(attrs["id"], "abc123");
        assert_eq!(attrs["annotationType"], "inlineComment");
    }

    #[test]
    fn annotation_mark_with_bold() {
        // Annotation + bold should both survive round-trip
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::text_with_marks(
                "bold comment",
                vec![
                    AdfMark::strong(),
                    AdfMark::annotation("def456", "inlineComment"),
                ],
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let text_node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let marks = text_node.marks.as_ref().expect("should have marks");
        assert!(
            marks.iter().any(|m| m.mark_type == "strong"),
            "should have strong mark"
        );
        assert!(
            marks.iter().any(|m| m.mark_type == "annotation"),
            "should have annotation mark"
        );
    }

    #[test]
    fn annotation_and_link_marks_both_preserved() {
        // Issue #390: text with both annotation and link marks loses link on round-trip
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"HANGUL-8","marks":[
            {"type":"annotation","attrs":{"annotationType":"inlineComment","id":"5ca7425e-34cd-48d3-b4eb-9873ac8b20e0"}},
            {"type":"link","attrs":{"href":"https://zendesk.atlassian.net/browse/HANGUL-8"}}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        // Should contain both annotation attrs and link syntax
        assert!(
            md.contains("annotation-id="),
            "JFM should contain annotation-id, got: {md}"
        );
        assert!(
            md.contains("](https://"),
            "JFM should contain link href, got: {md}"
        );
        let round_tripped = markdown_to_adf(&md).unwrap();
        let text_node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let marks = text_node.marks.as_ref().expect("should have marks");
        assert!(
            marks.iter().any(|m| m.mark_type == "annotation"),
            "should have annotation mark, got: {:?}",
            marks.iter().map(|m| &m.mark_type).collect::<Vec<_>>()
        );
        assert!(
            marks.iter().any(|m| m.mark_type == "link"),
            "should have link mark, got: {:?}",
            marks.iter().map(|m| &m.mark_type).collect::<Vec<_>>()
        );
    }

    #[test]
    fn underline_and_link_marks_both_preserved() {
        // Underline + link should also coexist
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::text_with_marks(
                "click here",
                vec![AdfMark::underline(), AdfMark::link("https://example.com")],
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("underline"), "should have underline attr: {md}");
        assert!(
            md.contains("](https://example.com)"),
            "should have link: {md}"
        );
        let round_tripped = markdown_to_adf(&md).unwrap();
        let text_node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let marks = text_node.marks.as_ref().expect("should have marks");
        assert!(marks.iter().any(|m| m.mark_type == "underline"));
        assert!(marks.iter().any(|m| m.mark_type == "link"));
    }

    #[test]
    fn annotation_link_and_bold_all_preserved() {
        // All three marks should coexist
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"important","marks":[
            {"type":"annotation","attrs":{"annotationType":"inlineComment","id":"abc"}},
            {"type":"link","attrs":{"href":"https://example.com"}},
            {"type":"strong"}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let text_node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let marks = text_node.marks.as_ref().expect("should have marks");
        assert!(
            marks.iter().any(|m| m.mark_type == "annotation"),
            "should have annotation"
        );
        assert!(
            marks.iter().any(|m| m.mark_type == "link"),
            "should have link"
        );
        assert!(
            marks.iter().any(|m| m.mark_type == "strong"),
            "should have strong"
        );
    }

    #[test]
    fn multiple_annotation_marks_round_trip() {
        // Issue #439: multiple annotation marks on same text node — all but last dropped
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"some annotated text","marks":[
            {"type":"annotation","attrs":{"id":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","annotationType":"inlineComment"}},
            {"type":"annotation","attrs":{"id":"ffffffff-1111-2222-3333-444444444444","annotationType":"inlineComment"}}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();

        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
            "JFM should contain first annotation id, got: {md}"
        );
        assert!(
            md.contains("ffffffff-1111-2222-3333-444444444444"),
            "JFM should contain second annotation id, got: {md}"
        );

        let round_tripped = markdown_to_adf(&md).unwrap();
        let text_node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        assert_eq!(text_node.text.as_deref(), Some("some annotated text"));
        let marks = text_node.marks.as_ref().expect("should have marks");
        let annotations: Vec<_> = marks
            .iter()
            .filter(|m| m.mark_type == "annotation")
            .collect();
        assert_eq!(
            annotations.len(),
            2,
            "should have 2 annotation marks, got: {annotations:?}"
        );
        let ids: Vec<_> = annotations
            .iter()
            .map(|a| a.attrs.as_ref().unwrap()["id"].as_str().unwrap())
            .collect();
        assert!(ids.contains(&"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"));
        assert!(ids.contains(&"ffffffff-1111-2222-3333-444444444444"));
    }

    #[test]
    fn three_annotation_marks_round_trip() {
        // Verify three overlapping annotations all survive
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::text_with_marks(
                "triple annotated",
                vec![
                    AdfMark::annotation("id-1", "inlineComment"),
                    AdfMark::annotation("id-2", "inlineComment"),
                    AdfMark::annotation("id-3", "inlineComment"),
                ],
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let text_node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let marks = text_node.marks.as_ref().expect("should have marks");
        let annotations: Vec<_> = marks
            .iter()
            .filter(|m| m.mark_type == "annotation")
            .collect();
        assert_eq!(
            annotations.len(),
            3,
            "should have 3 annotation marks, got: {annotations:?}"
        );
    }

    #[test]
    fn multiple_annotations_with_bold_round_trip() {
        // Multiple annotations + bold should all survive
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::text_with_marks(
                "bold double annotated",
                vec![
                    AdfMark::strong(),
                    AdfMark::annotation("ann-a", "inlineComment"),
                    AdfMark::annotation("ann-b", "inlineComment"),
                ],
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let text_node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let marks = text_node.marks.as_ref().expect("should have marks");
        assert!(
            marks.iter().any(|m| m.mark_type == "strong"),
            "should have strong mark"
        );
        let annotations: Vec<_> = marks
            .iter()
            .filter(|m| m.mark_type == "annotation")
            .collect();
        assert_eq!(
            annotations.len(),
            2,
            "should have 2 annotation marks, got: {annotations:?}"
        );
    }

    #[test]
    fn multiple_annotations_with_link_round_trip() {
        // Multiple annotations + link should all survive
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"linked text","marks":[
            {"type":"annotation","attrs":{"id":"ann-x","annotationType":"inlineComment"}},
            {"type":"annotation","attrs":{"id":"ann-y","annotationType":"inlineComment"}},
            {"type":"link","attrs":{"href":"https://example.com"}}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let text_node = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let marks = text_node.marks.as_ref().expect("should have marks");
        assert!(
            marks.iter().any(|m| m.mark_type == "link"),
            "should have link mark"
        );
        let annotations: Vec<_> = marks
            .iter()
            .filter(|m| m.mark_type == "annotation")
            .collect();
        assert_eq!(
            annotations.len(),
            2,
            "should have 2 annotation marks, got: {annotations:?}"
        );
    }

    // ── Inline directive tests (Tier 4) ───────────────────────────────

    #[test]
    fn status_directive() {
        let doc = markdown_to_adf("The ticket is :status[In Progress]{color=blue}.").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content[1].node_type, "status");
        assert_eq!(content[1].attrs.as_ref().unwrap()["text"], "In Progress");
        assert_eq!(content[1].attrs.as_ref().unwrap()["color"], "blue");
    }

    #[test]
    fn adf_status_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::status("Done", "green")])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":status[Done]{color=green}"));
    }

    #[test]
    fn round_trip_status() {
        let md = "The ticket is :status[In Progress]{color=blue}.\n";
        let doc = markdown_to_adf(md).unwrap();
        let result = adf_to_markdown(&doc).unwrap();
        assert!(result.contains(":status[In Progress]{color=blue}"));
    }

    #[test]
    fn status_with_style_and_localid_roundtrips() {
        let adf = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![{
                let mut node = AdfNode::status("open", "green");
                node.attrs.as_mut().unwrap()["style"] =
                    serde_json::Value::String("bold".to_string());
                node.attrs.as_mut().unwrap()["localId"] =
                    serde_json::Value::String("d2205ca5-84b9-4950-a730-bfe550fc146b".to_string());
                node
            }])],
        };

        let md = adf_to_markdown(&adf).unwrap();
        assert!(
            md.contains("style=bold"),
            "Markdown should contain style attr: {md}"
        );
        assert!(
            md.contains("localId=d2205ca5"),
            "Markdown should contain localId attr: {md}"
        );

        let rt = markdown_to_adf(&md).unwrap();
        let status = &rt.content[0].content.as_ref().unwrap()[0];
        let attrs = status.attrs.as_ref().unwrap();
        assert_eq!(attrs["text"], "open");
        assert_eq!(attrs["color"], "green");
        assert_eq!(attrs["style"], "bold");
        assert_eq!(
            attrs["localId"], "d2205ca5-84b9-4950-a730-bfe550fc146b",
            "localId should be preserved, got: {}",
            attrs["localId"]
        );
    }

    #[test]
    fn status_without_style_still_works() {
        let md = ":status[Done]{color=green}\n";
        let doc = markdown_to_adf(md).unwrap();
        let status = &doc.content[0].content.as_ref().unwrap()[0];
        let attrs = status.attrs.as_ref().unwrap();
        assert_eq!(attrs["text"], "Done");
        assert_eq!(attrs["color"], "green");
        // No style attr — should not be present
        assert!(
            attrs.get("style").is_none() || attrs["style"].is_null(),
            "style should not be set when not provided"
        );
    }

    #[test]
    fn strip_local_ids_removes_localid_from_status() {
        let adf = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![{
                let mut node = AdfNode::status("open", "green");
                node.attrs.as_mut().unwrap()["localId"] =
                    serde_json::Value::String("real-uuid-here".to_string());
                node
            }])],
        };
        let opts = RenderOptions {
            strip_local_ids: true,
        };
        let md = adf_to_markdown_with_options(&adf, &opts).unwrap();
        assert!(
            !md.contains("localId"),
            "localId should be stripped, got: {md}"
        );
        assert!(md.contains("color=green"), "color should be preserved");
    }

    #[test]
    fn strip_local_ids_removes_localid_from_table() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","attrs":{"layout":"default","localId":"table-uuid"},"content":[{"type":"tableRow","content":[{"type":"tableCell","content":[{"type":"paragraph","content":[{"type":"text","text":"cell"}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let opts = RenderOptions {
            strip_local_ids: true,
        };
        let md = adf_to_markdown_with_options(&doc, &opts).unwrap();
        assert!(
            !md.contains("localId"),
            "localId should be stripped from table, got: {md}"
        );
        assert!(md.contains("layout=default"), "layout should be preserved");
    }

    #[test]
    fn default_options_preserve_localid() {
        let adf = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![{
                let mut node = AdfNode::status("open", "green");
                node.attrs.as_mut().unwrap()["localId"] =
                    serde_json::Value::String("real-uuid-here".to_string());
                node
            }])],
        };
        let md = adf_to_markdown(&adf).unwrap();
        assert!(
            md.contains("localId=real-uuid-here"),
            "Default should preserve localId, got: {md}"
        );
    }

    #[test]
    fn mention_localid_roundtrip() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"mention","attrs":{"id":"user123","text":"@Alice","localId":"m-001"}}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("localId=m-001"),
            "mention should have localId in md: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        let mention = &rt.content[0].content.as_ref().unwrap()[0];
        assert_eq!(mention.attrs.as_ref().unwrap()["localId"], "m-001");
    }

    #[test]
    fn date_localid_roundtrip() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"date","attrs":{"timestamp":"1700000000000","localId":"d-001"}}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("localId=d-001"),
            "date should have localId in md: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        let date = &rt.content[0].content.as_ref().unwrap()[0];
        assert_eq!(date.attrs.as_ref().unwrap()["localId"], "d-001");
    }

    #[test]
    fn emoji_localid_roundtrip() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"emoji","attrs":{"shortName":":smile:","localId":"e-001"}}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("localId=e-001"),
            "emoji should have localId in md: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        let emoji = &rt.content[0].content.as_ref().unwrap()[0];
        assert_eq!(emoji.attrs.as_ref().unwrap()["localId"], "e-001");
    }

    #[test]
    fn inline_card_localid_roundtrip() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"inlineCard","attrs":{"url":"https://example.com","localId":"c-001"}}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("localId=c-001"),
            "inlineCard should have localId in md: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        let card = &rt.content[0].content.as_ref().unwrap()[0];
        assert_eq!(card.attrs.as_ref().unwrap()["localId"], "c-001");
    }

    #[test]
    fn strip_local_ids_removes_from_mention() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"mention","attrs":{"id":"user123","text":"@Alice","localId":"m-001"}}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let opts = RenderOptions {
            strip_local_ids: true,
        };
        let md = adf_to_markdown_with_options(&doc, &opts).unwrap();
        assert!(
            !md.contains("localId"),
            "localId should be stripped from mention: {md}"
        );
        assert!(md.contains("id=user123"), "other attrs should be preserved");
    }

    #[test]
    fn strip_local_ids_removes_from_date() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"date","attrs":{"timestamp":"1700000000000","localId":"d-001"}}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let opts = RenderOptions {
            strip_local_ids: true,
        };
        let md = adf_to_markdown_with_options(&doc, &opts).unwrap();
        assert!(
            !md.contains("localId"),
            "localId should be stripped from date: {md}"
        );
    }

    #[test]
    fn strip_local_ids_removes_from_block_attrs() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","attrs":{"localId":"p-001"},"content":[{"type":"text","text":"hello"}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let opts = RenderOptions {
            strip_local_ids: true,
        };
        let md = adf_to_markdown_with_options(&doc, &opts).unwrap();
        assert!(
            !md.contains("localId"),
            "localId should be stripped from block attrs: {md}"
        );
    }

    #[test]
    fn table_cell_localid_roundtrip() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","attrs":{},"content":[{"type":"tableRow","content":[{"type":"tableCell","attrs":{"localId":"tc-001"},"content":[{"type":"paragraph","content":[{"type":"text","text":"cell"}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("localId=tc-001"),
            "tableCell should have localId in md: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        let cell = &rt.content[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap()[0];
        assert_eq!(
            cell.attrs.as_ref().unwrap()["localId"],
            "tc-001",
            "tableCell localId should round-trip"
        );
    }

    #[test]
    fn table_row_localid_roundtrip() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","attrs":{},"content":[{"type":"tableRow","attrs":{"localId":"tr-001"},"content":[{"type":"tableCell","content":[{"type":"paragraph","content":[{"type":"text","text":"cell"}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("localId=tr-001"),
            "tableRow should have localId in md: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        let row = &rt.content[0].content.as_ref().unwrap()[0];
        assert_eq!(
            row.attrs.as_ref().unwrap()["localId"],
            "tr-001",
            "tableRow localId should round-trip"
        );
    }

    #[test]
    fn list_item_localid_roundtrip() {
        // listItem localId is emitted as trailing inline attrs and parsed back
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[{"type":"listItem","attrs":{"localId":"li-001"},"content":[{"type":"paragraph","content":[{"type":"text","text":"item"}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("localId=li-001"),
            "listItem should have localId in md: {md}"
        );
        // Verify localId is on the listItem, NOT promoted to bulletList
        let rt = markdown_to_adf(&md).unwrap();
        let list = &rt.content[0];
        assert!(
            list.attrs.is_none() || list.attrs.as_ref().unwrap().get("localId").is_none(),
            "bulletList should NOT have localId: {:?}",
            list.attrs
        );
        let item = &list.content.as_ref().unwrap()[0];
        assert_eq!(
            item.attrs.as_ref().unwrap()["localId"],
            "li-001",
            "listItem should have localId=li-001"
        );
    }

    #[test]
    fn list_item_localid_not_promoted_to_parent() {
        // Verify localId stays on listItem and doesn't leak to parent list
        let md = "- item {localId=li-002}\n";
        let doc = markdown_to_adf(md).unwrap();
        let list = &doc.content[0];
        assert!(
            list.attrs.is_none(),
            "bulletList should have no attrs: {:?}",
            list.attrs
        );
        let item = &list.content.as_ref().unwrap()[0];
        assert_eq!(item.attrs.as_ref().unwrap()["localId"], "li-002");
    }

    #[test]
    fn ordered_list_item_localid_roundtrip() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"orderedList","attrs":{"order":1},"content":[{"type":"listItem","attrs":{"localId":"oli-001"},"content":[{"type":"paragraph","content":[{"type":"text","text":"first"}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("localId=oli-001"), "md: {md}");
        let rt = markdown_to_adf(&md).unwrap();
        let item = &rt.content[0].content.as_ref().unwrap()[0];
        assert_eq!(item.attrs.as_ref().unwrap()["localId"], "oli-001");
    }

    #[test]
    fn task_item_localid_roundtrip() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"taskList","attrs":{"localId":"tl-001"},"content":[{"type":"taskItem","attrs":{"localId":"ti-001","state":"TODO"},"content":[{"type":"paragraph","content":[{"type":"text","text":"task"}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("localId=ti-001"), "md: {md}");
        let rt = markdown_to_adf(&md).unwrap();
        let item = &rt.content[0].content.as_ref().unwrap()[0];
        assert_eq!(item.attrs.as_ref().unwrap()["localId"], "ti-001");
    }

    /// Issue #447: taskList with empty-string localId and taskItems with
    /// short numeric localIds must survive a full round-trip.
    #[test]
    fn task_list_short_localid_roundtrip() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"taskList","attrs":{"localId":""},"content":[{"type":"taskItem","attrs":{"localId":"42","state":"TODO"}},{"type":"taskItem","attrs":{"localId":"99","state":"DONE"},"content":[{"type":"text","text":"done task"}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        // Both taskItem localIds should appear in the markdown
        assert!(md.contains("localId=42"), "localId=42 missing: {md}");
        assert!(md.contains("localId=99"), "localId=99 missing: {md}");
        // Empty-string localId should NOT appear as {localId=}
        assert!(
            !md.contains("localId=}"),
            "empty localId should not be emitted: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        let task_list = &rt.content[0];
        assert_eq!(task_list.node_type, "taskList");
        // No spurious extra nodes from {localId=}
        assert_eq!(rt.content.len(), 1, "should be exactly one top-level node");
        let items = task_list.content.as_ref().unwrap();
        assert_eq!(items.len(), 2);
        // First taskItem: localId=42, state=TODO, no content
        assert_eq!(items[0].attrs.as_ref().unwrap()["localId"], "42");
        assert_eq!(items[0].attrs.as_ref().unwrap()["state"], "TODO");
        assert!(
            items[0].content.is_none(),
            "empty taskItem should have no content: {:?}",
            items[0].content
        );
        // Second taskItem: localId=99, state=DONE, content with text
        assert_eq!(items[1].attrs.as_ref().unwrap()["localId"], "99");
        assert_eq!(items[1].attrs.as_ref().unwrap()["state"], "DONE");
        let content = items[1].content.as_ref().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].text.as_deref(), Some("done task"));
    }

    /// Issue #447: regression — taskList with empty localId must not inject
    /// a spurious paragraph.
    #[test]
    fn task_list_empty_localid_no_spurious_paragraph() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"taskList","attrs":{"localId":""},"content":[{"type":"taskItem","attrs":{"localId":"tsk-1","state":"DONE"},"content":[{"type":"text","text":"completed item"}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            !md.contains("{localId=}"),
            "empty localId should not be emitted: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        assert_eq!(
            rt.content.len(),
            1,
            "no spurious paragraph: {:#?}",
            rt.content
        );
        assert_eq!(rt.content[0].node_type, "taskList");
    }

    /// Issue #447: taskList localId should be stripped when strip_local_ids is set.
    #[test]
    fn task_list_localid_stripped() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"taskList","attrs":{"localId":"tl-001"},"content":[{"type":"taskItem","attrs":{"localId":"ti-001","state":"TODO"},"content":[{"type":"paragraph","content":[{"type":"text","text":"task"}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let opts = RenderOptions {
            strip_local_ids: true,
        };
        let md = adf_to_markdown_with_options(&doc, &opts).unwrap();
        assert!(!md.contains("localId"), "localId should be stripped: {md}");
    }

    /// Issue #447: taskItem with no content still emits localId.
    #[test]
    fn task_item_no_content_emits_localid() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"taskList","attrs":{"localId":"00000000-0000-0000-0000-000000000000"},"content":[{"type":"taskItem","attrs":{"localId":"abc","state":"TODO"}}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("localId=abc"),
            "localId should be emitted even without content: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        let item = &rt.content[0].content.as_ref().unwrap()[0];
        assert_eq!(item.attrs.as_ref().unwrap()["localId"], "abc");
        assert!(item.content.is_none(), "should have no content");
    }

    /// Issue #447: taskList localId roundtrips through block attrs.
    #[test]
    fn task_list_localid_roundtrip() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"taskList","attrs":{"localId":"tl-xyz"},"content":[{"type":"taskItem","attrs":{"localId":"ti-001","state":"TODO"},"content":[{"type":"paragraph","content":[{"type":"text","text":"task"}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("localId=tl-xyz"),
            "taskList localId missing: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        assert_eq!(
            rt.content[0].attrs.as_ref().unwrap()["localId"],
            "tl-xyz",
            "taskList localId should survive round-trip"
        );
    }

    #[test]
    fn trailing_space_preserved_with_hex_localid() {
        // Issue #449: trailing whitespace stripped from text node
        // when listItem has a hex-format localId (no hyphens)
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[{"type":"listItem","attrs":{"localId":"aabb112233cc"},"content":[{"type":"paragraph","content":[{"type":"text","text":"trailing space "}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();
        let item = &rt.content[0].content.as_ref().unwrap()[0];
        assert_eq!(
            item.attrs.as_ref().unwrap()["localId"],
            "aabb112233cc",
            "localId should round-trip"
        );
        let para = &item.content.as_ref().unwrap()[0];
        let inlines = para.content.as_ref().unwrap();
        let last = inlines.last().unwrap();
        assert!(
            last.text.as_deref().unwrap_or("").ends_with(' '),
            "trailing space should be preserved, got nodes: {:?}",
            inlines
                .iter()
                .map(|n| (&n.node_type, &n.text))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn extract_trailing_local_id_preserves_trailing_space() {
        // Issue #449: only strip the single separator space before {localId=...}
        let (before, lid, _) = extract_trailing_local_id("trailing space  {localId=aabb112233cc}");
        assert_eq!(before, "trailing space ");
        assert_eq!(lid.as_deref(), Some("aabb112233cc"));
    }

    #[test]
    fn extract_trailing_local_id_no_trailing_space() {
        let (before, lid, _) = extract_trailing_local_id("text {localId=abc123}");
        assert_eq!(before, "text");
        assert_eq!(lid.as_deref(), Some("abc123"));
    }

    #[test]
    fn extract_trailing_local_id_no_attrs() {
        let (before, lid, pid) = extract_trailing_local_id("plain text");
        assert_eq!(before, "plain text");
        assert!(lid.is_none());
        assert!(pid.is_none());
    }

    #[test]
    fn list_item_localid_stripped() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[{"type":"listItem","attrs":{"localId":"li-001"},"content":[{"type":"paragraph","content":[{"type":"text","text":"item"}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let opts = RenderOptions {
            strip_local_ids: true,
        };
        let md = adf_to_markdown_with_options(&doc, &opts).unwrap();
        assert!(!md.contains("localId"), "localId should be stripped: {md}");
    }

    #[test]
    fn paragraph_localid_in_list_item_roundtrip() {
        // Issue #417: paragraph.attrs.localId dropped in listItem context
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","attrs":{"localId":"list-001"},"content":[{"type":"listItem","attrs":{"localId":"item-001"},"content":[{"type":"paragraph","attrs":{"localId":"para-001"},"content":[{"type":"text","text":"item text"}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("paraLocalId=para-001"),
            "paragraph localId should be in md: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        let item = &rt.content[0].content.as_ref().unwrap()[0];
        assert_eq!(
            item.attrs.as_ref().unwrap()["localId"],
            "item-001",
            "listItem localId should survive"
        );
        let para = &item.content.as_ref().unwrap()[0];
        assert_eq!(
            para.attrs.as_ref().unwrap()["localId"],
            "para-001",
            "paragraph localId should survive round-trip"
        );
    }

    #[test]
    fn paragraph_localid_in_ordered_list_item_roundtrip() {
        // Issue #417: paragraph localId in ordered list
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"orderedList","attrs":{"order":1},"content":[{"type":"listItem","attrs":{"localId":"oli-001"},"content":[{"type":"paragraph","attrs":{"localId":"op-001"},"content":[{"type":"text","text":"first"}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("paraLocalId=op-001"), "md: {md}");
        let rt = markdown_to_adf(&md).unwrap();
        let item = &rt.content[0].content.as_ref().unwrap()[0];
        assert_eq!(item.attrs.as_ref().unwrap()["localId"], "oli-001");
        let para = &item.content.as_ref().unwrap()[0];
        assert_eq!(para.attrs.as_ref().unwrap()["localId"], "op-001");
    }

    #[test]
    fn paragraph_localid_only_in_list_item() {
        // paragraph has localId but listItem does not
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[{"type":"listItem","content":[{"type":"paragraph","attrs":{"localId":"para-only"},"content":[{"type":"text","text":"text"}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("paraLocalId=para-only"),
            "paragraph localId should be emitted: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        let item = &rt.content[0].content.as_ref().unwrap()[0];
        assert!(item.attrs.is_none(), "listItem should have no attrs");
        let para = &item.content.as_ref().unwrap()[0];
        assert_eq!(para.attrs.as_ref().unwrap()["localId"], "para-only");
    }

    #[test]
    fn paragraph_localid_in_table_header_roundtrip() {
        // Issue #417: paragraph.attrs.localId dropped in tableHeader context
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","attrs":{"isNumberColumnEnabled":false,"layout":"default"},"content":[{"type":"tableRow","content":[{"type":"tableHeader","attrs":{},"content":[{"type":"paragraph","attrs":{"localId":"aaaa-aaaa"},"content":[{"type":"text","text":"hello"}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        // Should use directive form (not pipe table) to preserve paragraph localId
        assert!(
            md.contains("localId=aaaa-aaaa"),
            "paragraph localId should be in md: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        let cell = &rt.content[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap()[0];
        let para = &cell.content.as_ref().unwrap()[0];
        assert_eq!(
            para.attrs.as_ref().unwrap()["localId"],
            "aaaa-aaaa",
            "paragraph localId should survive round-trip in tableHeader"
        );
    }

    #[test]
    fn paragraph_localid_in_table_cell_roundtrip() {
        // Issue #417: paragraph localId in tableCell forces directive table
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","attrs":{"isNumberColumnEnabled":false,"layout":"default"},"content":[{"type":"tableRow","content":[{"type":"tableHeader","attrs":{},"content":[{"type":"paragraph","content":[{"type":"text","text":"header"}]}]}]},{"type":"tableRow","content":[{"type":"tableCell","attrs":{},"content":[{"type":"paragraph","attrs":{"localId":"cell-para"},"content":[{"type":"text","text":"data"}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("localId=cell-para"),
            "paragraph localId should be in md: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        // Data row -> cell -> paragraph
        let cell = &rt.content[0].content.as_ref().unwrap()[1]
            .content
            .as_ref()
            .unwrap()[0];
        let para = &cell.content.as_ref().unwrap()[0];
        assert_eq!(
            para.attrs.as_ref().unwrap()["localId"],
            "cell-para",
            "paragraph localId should survive round-trip in tableCell"
        );
    }

    #[test]
    fn nbsp_paragraph_with_localid_roundtrip() {
        // Issue #417: nbsp paragraph localId emitted as text instead of attrs
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","attrs":{"localId":"nbsp-para"},"content":[{"type":"text","text":"\u00a0"}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("::paragraph["),
            "nbsp should use directive form: {md}"
        );
        assert!(
            md.contains("localId=nbsp-para"),
            "localId should be in directive: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        let para = &rt.content[0];
        assert_eq!(
            para.attrs.as_ref().unwrap()["localId"],
            "nbsp-para",
            "localId should survive round-trip"
        );
        let text = para.content.as_ref().unwrap()[0].text.as_ref().unwrap();
        assert_eq!(text, "\u{00a0}", "nbsp should survive");
    }

    #[test]
    fn empty_paragraph_with_localid_roundtrip() {
        // Empty paragraph directive with localId
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","attrs":{"localId":"empty-para"}}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("::paragraph{localId=empty-para}"),
            "empty paragraph should include localId in directive: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        assert_eq!(
            rt.content[0].attrs.as_ref().unwrap()["localId"],
            "empty-para"
        );
    }

    #[test]
    fn paragraph_localid_stripped_from_list_item() {
        // strip_local_ids should also strip paraLocalId
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[{"type":"listItem","attrs":{"localId":"li-001"},"content":[{"type":"paragraph","attrs":{"localId":"p-001"},"content":[{"type":"text","text":"item"}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let opts = RenderOptions {
            strip_local_ids: true,
        };
        let md = adf_to_markdown_with_options(&doc, &opts).unwrap();
        assert!(!md.contains("localId"), "localId should be stripped: {md}");
        assert!(
            !md.contains("paraLocalId"),
            "paraLocalId should be stripped: {md}"
        );
    }

    #[test]
    fn date_directive() {
        let doc = markdown_to_adf("Due by :date[2026-04-15].").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content[1].node_type, "date");
        // ISO date is converted to epoch milliseconds
        assert_eq!(
            content[1].attrs.as_ref().unwrap()["timestamp"],
            "1776211200000"
        );
    }

    #[test]
    fn adf_date_to_markdown() {
        // ADF dates use epoch ms; renderer converts back to ISO with timestamp attr
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::date("1776211200000")])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":date[2026-04-15]{timestamp=1776211200000}"));
    }

    #[test]
    fn adf_date_iso_passthrough() {
        // If ADF already has ISO date (legacy), pass through
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::date("2026-04-15")])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":date[2026-04-15]{timestamp=2026-04-15}"));
    }

    #[test]
    fn round_trip_date() {
        let md = "Due by :date[2026-04-15].\n";
        let doc = markdown_to_adf(md).unwrap();
        let result = adf_to_markdown(&doc).unwrap();
        assert!(result.contains(":date[2026-04-15]"));
    }

    #[test]
    fn round_trip_date_non_midnight_timestamp() {
        // Issue #409: non-midnight timestamps must survive round-trip
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"date","attrs":{"timestamp":"1700000000000"}}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        // JFM should include the original timestamp
        assert!(
            md.contains("timestamp=1700000000000"),
            "JFM should preserve original timestamp: {md}"
        );
        // Round-trip back to ADF
        let doc2 = markdown_to_adf(&md).unwrap();
        let content = doc2.content[0].content.as_ref().unwrap();
        assert_eq!(
            content[0].attrs.as_ref().unwrap()["timestamp"],
            "1700000000000",
            "Round-trip must preserve original non-midnight timestamp"
        );
    }

    #[test]
    fn date_epoch_ms_passthrough() {
        // If JFM date is already epoch ms, pass through
        let doc = markdown_to_adf("Due by :date[1776211200000].").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(
            content[1].attrs.as_ref().unwrap()["timestamp"],
            "1776211200000"
        );
    }

    #[test]
    fn date_timestamp_attr_preferred_over_content() {
        // When timestamp attr is present, it takes priority over the display date
        let md = ":date[2023-11-14]{timestamp=1700000000000}\n";
        let doc = markdown_to_adf(md).unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(
            content[0].attrs.as_ref().unwrap()["timestamp"],
            "1700000000000",
            "timestamp attr should be used directly"
        );
    }

    #[test]
    fn date_without_timestamp_attr_backward_compat() {
        // Legacy JFM without timestamp attr still works via iso_date_to_epoch_ms
        let md = ":date[2026-04-15]\n";
        let doc = markdown_to_adf(md).unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(
            content[0].attrs.as_ref().unwrap()["timestamp"],
            "1776211200000",
            "Should fall back to computing timestamp from date string"
        );
    }

    #[test]
    fn date_with_local_id_and_timestamp() {
        // Both localId and timestamp should round-trip
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"date","attrs":{"timestamp":"1700000000000","localId":"d-001"}}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("timestamp=1700000000000"),
            "Should contain timestamp: {md}"
        );
        assert!(md.contains("localId=d-001"), "Should contain localId: {md}");
        // Round-trip
        let doc2 = markdown_to_adf(&md).unwrap();
        let content = doc2.content[0].content.as_ref().unwrap();
        let attrs = content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["timestamp"], "1700000000000");
        assert_eq!(attrs["localId"], "d-001");
    }

    #[test]
    fn mention_directive() {
        let doc = markdown_to_adf("Assigned to :mention[Alice]{id=abc123}.").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content[1].node_type, "mention");
        assert_eq!(content[1].attrs.as_ref().unwrap()["id"], "abc123");
        assert_eq!(content[1].attrs.as_ref().unwrap()["text"], "Alice");
    }

    #[test]
    fn adf_mention_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::mention(
                "abc123", "Alice",
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":mention[Alice]{id=abc123}"));
    }

    #[test]
    fn round_trip_mention() {
        let md = "Assigned to :mention[Alice]{id=abc123}.\n";
        let doc = markdown_to_adf(md).unwrap();
        let result = adf_to_markdown(&doc).unwrap();
        assert!(result.contains(":mention[Alice]{id=abc123}"));
    }

    #[test]
    fn mention_with_empty_access_level_round_trips() {
        // Issue #363: accessLevel="" produces accessLevel= which failed to parse
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"mention","attrs":{"id":"61921b41c15977006af2b1d1","text":"@Javier Inchausti","accessLevel":""}}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();

        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let mention = &round_tripped.content[0].content.as_ref().unwrap()[0];
        assert_eq!(
            mention.node_type, "mention",
            "mention with empty accessLevel was not parsed as mention, got: {}",
            mention.node_type
        );
    }

    #[test]
    fn span_with_color() {
        let doc = markdown_to_adf("This is :span[red text]{color=#ff5630}.").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content[1].node_type, "text");
        assert_eq!(content[1].text.as_deref(), Some("red text"));
        let marks = content[1].marks.as_ref().unwrap();
        assert_eq!(marks[0].mark_type, "textColor");
    }

    #[test]
    fn adf_text_color_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::text_with_marks(
                "red text",
                vec![AdfMark::text_color("#ff5630")],
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":span[red text]{color=#ff5630}"));
    }

    #[test]
    fn round_trip_span_color() {
        let md = "This is :span[red text]{color=#ff5630}.\n";
        let doc = markdown_to_adf(md).unwrap();
        let result = adf_to_markdown(&doc).unwrap();
        assert!(result.contains(":span[red text]{color=#ff5630}"));
    }

    #[test]
    fn text_color_and_link_marks_both_preserved() {
        // Issue #405: text with both textColor and link marks loses link on round-trip
        let adf_json = r##"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"red link","marks":[
            {"type":"link","attrs":{"href":"https://example.com"}},
            {"type":"textColor","attrs":{"color":"#ff0000"}}
          ]}
        ]}]}"##;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains(":span[red link]{color=#ff0000}"),
            "JFM should contain span with color, got: {md}"
        );
        assert!(
            md.contains("](https://example.com)"),
            "JFM should contain link href, got: {md}"
        );
        // Full round-trip: both marks survive
        let rt = markdown_to_adf(&md).unwrap();
        let text_node = &rt.content[0].content.as_ref().unwrap()[0];
        let marks = text_node.marks.as_ref().expect("should have marks");
        assert!(
            marks.iter().any(|m| m.mark_type == "textColor"),
            "should have textColor mark, got: {:?}",
            marks.iter().map(|m| &m.mark_type).collect::<Vec<_>>()
        );
        assert!(
            marks.iter().any(|m| m.mark_type == "link"),
            "should have link mark, got: {:?}",
            marks.iter().map(|m| &m.mark_type).collect::<Vec<_>>()
        );
        // Verify attribute values survive
        let link_mark = marks.iter().find(|m| m.mark_type == "link").unwrap();
        assert_eq!(
            link_mark.attrs.as_ref().unwrap()["href"],
            "https://example.com"
        );
        let color_mark = marks.iter().find(|m| m.mark_type == "textColor").unwrap();
        assert_eq!(color_mark.attrs.as_ref().unwrap()["color"], "#ff0000");
    }

    #[test]
    fn bg_color_and_link_marks_both_preserved() {
        let adf_json = r##"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"highlighted link","marks":[
            {"type":"link","attrs":{"href":"https://example.com"}},
            {"type":"backgroundColor","attrs":{"color":"#ffff00"}}
          ]}
        ]}]}"##;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("bg=#ffff00"), "should have bg color: {md}");
        assert!(
            md.contains("](https://example.com)"),
            "should have link: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        let text_node = &rt.content[0].content.as_ref().unwrap()[0];
        let marks = text_node.marks.as_ref().expect("should have marks");
        assert!(marks.iter().any(|m| m.mark_type == "backgroundColor"));
        assert!(marks.iter().any(|m| m.mark_type == "link"));
    }

    #[test]
    fn text_color_link_and_strong_rendering() {
        // Verify textColor + link + strong renders all three formatting elements
        let adf_json = r##"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"bold red link","marks":[
            {"type":"strong"},
            {"type":"link","attrs":{"href":"https://example.com"}},
            {"type":"textColor","attrs":{"color":"#ff0000"}}
          ]}
        ]}]}"##;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.starts_with("**") && md.trim().ends_with("**"),
            "should have bold wrapping: {md}"
        );
        assert!(md.contains("color=#ff0000"), "should have color: {md}");
        assert!(
            md.contains("](https://example.com)"),
            "should have link: {md}"
        );
    }

    #[test]
    fn subsup_and_link_marks_both_preserved() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"note","marks":[
            {"type":"link","attrs":{"href":"https://example.com"}},
            {"type":"subsup","attrs":{"type":"sup"}}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("sup"), "should have sup: {md}");
        assert!(
            md.contains("](https://example.com)"),
            "should have link: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        let text_node = &rt.content[0].content.as_ref().unwrap()[0];
        let marks = text_node.marks.as_ref().expect("should have marks");
        assert!(marks.iter().any(|m| m.mark_type == "subsup"));
        assert!(marks.iter().any(|m| m.mark_type == "link"));
    }

    #[test]
    fn text_color_without_link_unchanged() {
        // Regression guard: textColor without link should still work
        let adf_json = r##"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"just red","marks":[
            {"type":"textColor","attrs":{"color":"#ff0000"}}
          ]}
        ]}]}"##;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":span[just red]{color=#ff0000}"), "md: {md}");
        assert!(!md.contains("](http"), "should NOT have link syntax: {md}");
    }

    #[test]
    fn inline_extension_directive() {
        let doc =
            markdown_to_adf("See :extension[fallback]{type=com.app key=widget} here.").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content[1].node_type, "inlineExtension");
        assert_eq!(
            content[1].attrs.as_ref().unwrap()["extensionType"],
            "com.app"
        );
        assert_eq!(content[1].attrs.as_ref().unwrap()["extensionKey"], "widget");
    }

    #[test]
    fn adf_inline_extension_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::inline_extension(
                "com.app",
                "widget",
                Some("fallback"),
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":extension[fallback]{type=com.app key=widget}"));
    }

    // ── Helper function tests ──────────────────────────────────────────

    #[test]
    fn parse_ordered_list_marker_valid() {
        let result = parse_ordered_list_marker("1. Hello");
        assert_eq!(result, Some((1, "Hello")));
    }

    #[test]
    fn parse_ordered_list_marker_high_number() {
        let result = parse_ordered_list_marker("42. Item");
        assert_eq!(result, Some((42, "Item")));
    }

    #[test]
    fn parse_ordered_list_marker_not_a_list() {
        assert!(parse_ordered_list_marker("not a list").is_none());
        assert!(parse_ordered_list_marker("1.no space").is_none());
    }

    #[test]
    fn is_list_start_various() {
        assert!(is_list_start("- item"));
        assert!(is_list_start("* item"));
        assert!(is_list_start("+ item"));
        assert!(is_list_start("1. item"));
        assert!(!is_list_start("not a list"));
    }

    #[test]
    fn is_horizontal_rule_various() {
        assert!(is_horizontal_rule("---"));
        assert!(is_horizontal_rule("***"));
        assert!(is_horizontal_rule("___"));
        assert!(is_horizontal_rule("------"));
        assert!(!is_horizontal_rule("--"));
        assert!(!is_horizontal_rule("abc"));
    }

    #[test]
    fn is_table_separator_valid() {
        assert!(is_table_separator("| --- | --- |"));
        assert!(is_table_separator("|:---:|:---|"));
        assert!(!is_table_separator("no pipes here"));
    }

    #[test]
    fn parse_table_row_cells() {
        let cells = parse_table_row("| A | B | C |");
        assert_eq!(cells, vec!["A", "B", "C"]);
    }

    #[test]
    fn parse_image_syntax_valid() {
        let result = parse_image_syntax("![alt](url)");
        assert_eq!(result, Some(("alt", "url")));
    }

    #[test]
    fn parse_image_syntax_not_image() {
        assert!(parse_image_syntax("not an image").is_none());
    }

    #[test]
    fn flush_plain_empty_range() {
        let mut nodes = Vec::new();
        flush_plain("hello", 3, 3, &mut nodes);
        assert!(nodes.is_empty());
    }

    #[test]
    fn add_mark_to_unmarked_node() {
        let mut node = AdfNode::text("test");
        add_mark(&mut node, AdfMark::strong());
        assert_eq!(node.marks.as_ref().unwrap().len(), 1);
    }

    #[test]
    fn add_mark_to_marked_node() {
        let mut node = AdfNode::text_with_marks("test", vec![AdfMark::strong()]);
        add_mark(&mut node, AdfMark::em());
        assert_eq!(node.marks.as_ref().unwrap().len(), 2);
    }

    // ── Directive table tests ──────────────────────────────────────

    #[test]
    fn directive_table_basic() {
        let md = "::::table\n:::tr\n:::th\nHeader 1\n:::\n:::th\nHeader 2\n:::\n:::\n:::tr\n:::td\nCell 1\n:::\n:::td\nCell 2\n:::\n:::\n::::\n";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content[0].node_type, "table");
        let rows = doc.content[0].content.as_ref().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0].content.as_ref().unwrap()[0].node_type,
            "tableHeader"
        );
        assert_eq!(rows[1].content.as_ref().unwrap()[0].node_type, "tableCell");
    }

    #[test]
    fn directive_table_with_block_content() {
        let md = "::::table\n:::tr\n:::td\nCell with list:\n\n- Item 1\n- Item 2\n:::\n:::td\nSimple cell\n:::\n:::\n::::\n";
        let doc = markdown_to_adf(md).unwrap();
        let rows = doc.content[0].content.as_ref().unwrap();
        let cell = &rows[0].content.as_ref().unwrap()[0];
        // Cell should have block content (paragraph + bullet list)
        let content = cell.content.as_ref().unwrap();
        assert!(content.len() >= 2);
        assert_eq!(content[1].node_type, "bulletList");
    }

    #[test]
    fn directive_table_with_cell_attrs() {
        let md = "::::table\n:::tr\n:::td{colspan=2 bg=#DEEBFF}\nSpanning cell\n:::\n:::\n::::\n";
        let doc = markdown_to_adf(md).unwrap();
        let cell = &doc.content[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap()[0];
        let attrs = cell.attrs.as_ref().unwrap();
        assert_eq!(attrs["colspan"], 2);
        assert_eq!(attrs["background"], "#DEEBFF");
    }

    #[test]
    fn directive_table_with_css_var_background() {
        let bg = "var(--ds-background-accent-gray-subtlest, var(--ds-background-accent-gray-subtlest, #F1F2F4))";
        let md = format!("::::table\n:::tr\n:::th{{bg=\"{bg}\"}}\nHeader\n:::\n:::\n::::\n");
        let doc = markdown_to_adf(&md).unwrap();
        let row = &doc.content[0].content.as_ref().unwrap()[0];
        let cells = row.content.as_ref().unwrap();
        assert_eq!(cells.len(), 1, "row must have at least one cell");
        let attrs = cells[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["background"], bg);
    }

    #[test]
    fn css_var_background_round_trips() {
        let bg = "var(--ds-background-accent-gray-subtlest, #F1F2F4)";
        let adf = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![AdfNode::table_row(vec![
                AdfNode::table_header_with_attrs(
                    vec![AdfNode::paragraph(vec![AdfNode::text("Header")])],
                    serde_json::json!({"background": bg}),
                ),
            ])])],
        };
        let md = adf_to_markdown(&adf).unwrap();
        assert!(
            md.contains(&format!("bg=\"{bg}\"")),
            "bg value must be quoted in markdown: {md}"
        );

        let round_tripped = markdown_to_adf(&md).unwrap();
        let row = &round_tripped.content[0].content.as_ref().unwrap()[0];
        let cells = row.content.as_ref().unwrap();
        assert_eq!(cells.len(), 1, "round-tripped row must have one cell");
        let rt_attrs = cells[0].attrs.as_ref().unwrap();
        assert_eq!(rt_attrs["background"], bg);
    }

    #[test]
    fn directive_table_with_table_attrs() {
        let md = "::::table{layout=wide numbered}\n:::tr\n:::td\nCell\n:::\n:::\n::::\n";
        let doc = markdown_to_adf(md).unwrap();
        let attrs = doc.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["layout"], "wide");
        assert_eq!(attrs["isNumberColumnEnabled"], true);
    }

    #[test]
    fn adf_table_with_block_content_renders_directive_form() {
        // Table with a bullet list in a cell → should render as ::::table directive
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![AdfNode::table_row(vec![
                AdfNode::table_cell(vec![
                    AdfNode::paragraph(vec![AdfNode::text("Cell with list:")]),
                    AdfNode::bullet_list(vec![AdfNode::list_item(vec![AdfNode::paragraph(vec![
                        AdfNode::text("Item 1"),
                    ])])]),
                ]),
            ])])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("::::table"));
        assert!(md.contains(":::td"));
        assert!(md.contains("- Item 1"));
    }

    #[test]
    fn adf_table_inline_only_renders_pipe_form() {
        // Table with only inline content → pipe table
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![
                AdfNode::table_row(vec![
                    AdfNode::table_header(vec![AdfNode::paragraph(vec![AdfNode::text("H1")])]),
                    AdfNode::table_header(vec![AdfNode::paragraph(vec![AdfNode::text("H2")])]),
                ]),
                AdfNode::table_row(vec![
                    AdfNode::table_cell(vec![AdfNode::paragraph(vec![AdfNode::text("C1")])]),
                    AdfNode::table_cell(vec![AdfNode::paragraph(vec![AdfNode::text("C2")])]),
                ]),
            ])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("| H1 | H2 |"));
        assert!(!md.contains("::::table"));
    }

    #[test]
    fn adf_table_header_outside_first_row_renders_directive() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![
                AdfNode::table_row(vec![
                    AdfNode::table_header(vec![AdfNode::paragraph(vec![AdfNode::text("H")])]),
                    AdfNode::table_cell(vec![AdfNode::paragraph(vec![AdfNode::text("C")])]),
                ]),
                AdfNode::table_row(vec![
                    AdfNode::table_header(vec![AdfNode::paragraph(vec![AdfNode::text("H2")])]),
                    AdfNode::table_cell(vec![AdfNode::paragraph(vec![AdfNode::text("C2")])]),
                ]),
            ])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("::::table"));
        assert!(md.contains(":::th"));
    }

    #[test]
    fn adf_table_cell_attrs_rendered() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![
                AdfNode::table_row(vec![AdfNode::table_header(vec![AdfNode::paragraph(vec![
                    AdfNode::text("H"),
                ])])]),
                AdfNode::table_row(vec![AdfNode::table_cell_with_attrs(
                    vec![AdfNode::paragraph(vec![AdfNode::text("C")])],
                    serde_json::json!({"background": "#DEEBFF", "colspan": 2}),
                )]),
            ])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("{colspan=2 bg=#DEEBFF}"));
    }

    // ── Pipe table cell attrs tests ────────────────────────────────

    #[test]
    fn pipe_table_cell_attrs() {
        let md = "| H1 | H2 |\n|---|---|\n| {bg=#DEEBFF} highlighted | normal |\n";
        let doc = markdown_to_adf(md).unwrap();
        let rows = doc.content[0].content.as_ref().unwrap();
        let cell = &rows[1].content.as_ref().unwrap()[0];
        let attrs = cell.attrs.as_ref().unwrap();
        assert_eq!(attrs["background"], "#DEEBFF");
    }

    #[test]
    fn pipe_table_cell_colspan() {
        let md = "| H1 | H2 |\n|---|---|\n| {colspan=2} spanning |\n";
        let doc = markdown_to_adf(md).unwrap();
        let rows = doc.content[0].content.as_ref().unwrap();
        let cell = &rows[1].content.as_ref().unwrap()[0];
        let attrs = cell.attrs.as_ref().unwrap();
        assert_eq!(attrs["colspan"], 2);
    }

    #[test]
    fn trailing_space_after_mention_in_table_cell_preserved() {
        // Issue #372: trailing space after mention in table cell was dropped
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","attrs":{"isNumberColumnEnabled":false,"layout":"default"},"content":[{"type":"tableRow","content":[{"type":"tableCell","attrs":{},"content":[{"type":"paragraph","content":[
          {"type":"mention","attrs":{"id":"aaa","text":"@Rob"}},
          {"type":"text","text":" "}
        ]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let cell = &round_tripped.content[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap()[0];
        let para = &cell.content.as_ref().unwrap()[0];
        let inlines = para.content.as_ref().unwrap();
        assert!(
            inlines.len() >= 2,
            "expected mention + text(' ') nodes, got {} nodes: {:?}",
            inlines.len(),
            inlines.iter().map(|n| &n.node_type).collect::<Vec<_>>()
        );
        assert_eq!(inlines[0].node_type, "mention");
        assert_eq!(inlines[1].node_type, "text");
        assert_eq!(inlines[1].text.as_deref(), Some(" "));
    }

    // ── Column alignment tests ─────────────────────────────────────

    #[test]
    fn pipe_table_column_alignment() {
        let md = "| Left | Center | Right |\n|:---|:---:|---:|\n| L | C | R |\n";
        let doc = markdown_to_adf(md).unwrap();
        let rows = doc.content[0].content.as_ref().unwrap();
        // Header row
        let h_cells = rows[0].content.as_ref().unwrap();
        // Left → no mark
        assert!(h_cells[0].content.as_ref().unwrap()[0].marks.is_none());
        // Center → alignment center
        let center_marks = h_cells[1].content.as_ref().unwrap()[0]
            .marks
            .as_ref()
            .unwrap();
        assert_eq!(center_marks[0].attrs.as_ref().unwrap()["align"], "center");
        // Right → alignment end
        let right_marks = h_cells[2].content.as_ref().unwrap()[0]
            .marks
            .as_ref()
            .unwrap();
        assert_eq!(right_marks[0].attrs.as_ref().unwrap()["align"], "end");
    }

    #[test]
    fn adf_table_alignment_roundtrip() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![
                AdfNode::table_row(vec![
                    AdfNode::table_header(vec![{
                        let mut p = AdfNode::paragraph(vec![AdfNode::text("Center")]);
                        p.marks = Some(vec![AdfMark::alignment("center")]);
                        p
                    }]),
                    AdfNode::table_header(vec![{
                        let mut p = AdfNode::paragraph(vec![AdfNode::text("Right")]);
                        p.marks = Some(vec![AdfMark::alignment("end")]);
                        p
                    }]),
                ]),
                AdfNode::table_row(vec![
                    AdfNode::table_cell(vec![{
                        let mut p = AdfNode::paragraph(vec![AdfNode::text("C")]);
                        p.marks = Some(vec![AdfMark::alignment("center")]);
                        p
                    }]),
                    AdfNode::table_cell(vec![{
                        let mut p = AdfNode::paragraph(vec![AdfNode::text("R")]);
                        p.marks = Some(vec![AdfMark::alignment("end")]);
                        p
                    }]),
                ]),
            ])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":---:"));
        assert!(md.contains("---:"));
    }

    // ── Panel custom attrs tests ───────────────────────────────────

    #[test]
    fn panel_custom_attrs_round_trip() {
        let md = ":::panel{type=custom icon=\":star:\" color=\"#DEEBFF\"}\nContent\n:::\n";
        let doc = markdown_to_adf(md).unwrap();
        let panel = &doc.content[0];
        let attrs = panel.attrs.as_ref().unwrap();
        assert_eq!(attrs["panelType"], "custom");
        assert_eq!(attrs["panelIcon"], ":star:");
        assert_eq!(attrs["panelColor"], "#DEEBFF");

        let result = adf_to_markdown(&doc).unwrap();
        assert!(result.contains("type=custom"));
        assert!(result.contains("icon="));
        assert!(result.contains("color="));
    }

    // ── Block card with attrs tests ────────────────────────────────

    #[test]
    fn block_card_with_layout() {
        let md = "::card[https://example.com]{layout=wide}\n";
        let doc = markdown_to_adf(md).unwrap();
        let attrs = doc.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["layout"], "wide");

        let result = adf_to_markdown(&doc).unwrap();
        assert!(result.contains("::card[https://example.com]{layout=wide}"));
    }

    // ── Extension params test ──────────────────────────────────────

    #[test]
    fn extension_with_params() {
        let md = r#"::extension{type=com.atlassian.macro key=jira-chart params='{"jql":"project=PROJ"}'}"#;
        let doc = markdown_to_adf(&format!("{md}\n")).unwrap();
        let attrs = doc.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["parameters"]["jql"], "project=PROJ");
    }

    #[test]
    fn leaf_extension_layout_preserved_in_roundtrip() {
        // Issue #381: layout attr on extension nodes was dropped
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"extension","attrs":{"extensionType":"com.atlassian.confluence.macro.core","extensionKey":"toc","layout":"default","parameters":{}}}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("layout=default"),
            "JFM should contain layout=default, got: {md}"
        );
        let round_tripped = markdown_to_adf(&md).unwrap();
        let attrs = round_tripped.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["layout"], "default", "layout should be preserved");
        assert_eq!(attrs["extensionKey"], "toc");
    }

    #[test]
    fn bodied_extension_layout_preserved_in_roundtrip() {
        // Bodied extension with layout
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"bodiedExtension","attrs":{"extensionType":"com.atlassian.macro","extensionKey":"expand","layout":"wide"},
           "content":[{"type":"paragraph","content":[{"type":"text","text":"inner"}]}]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("layout=wide"),
            "JFM should contain layout=wide, got: {md}"
        );
        let round_tripped = markdown_to_adf(&md).unwrap();
        let attrs = round_tripped.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["layout"], "wide", "layout should be preserved");
    }

    #[test]
    fn leaf_extension_localid_preserved_in_roundtrip() {
        // Extension with both layout and localId
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"extension","attrs":{"extensionType":"com.atlassian.macro","extensionKey":"toc","layout":"default","localId":"abc-123"}}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let attrs = round_tripped.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["layout"], "default");
        assert_eq!(attrs["localId"], "abc-123");
    }

    // ── Mention with userType test ─────────────────────────────────

    #[test]
    fn mention_with_user_type() {
        let md = "Hi :mention[Alice]{id=abc123 userType=DEFAULT}.\n";
        let doc = markdown_to_adf(md).unwrap();
        let mention = &doc.content[0].content.as_ref().unwrap()[1];
        assert_eq!(mention.attrs.as_ref().unwrap()["userType"], "DEFAULT");

        let result = adf_to_markdown(&doc).unwrap();
        assert!(result.contains("userType=DEFAULT"));
    }

    // ── Colwidth tests ─────────────────────────────────────────────

    #[test]
    fn directive_table_colwidth() {
        let md = "::::table\n:::tr\n:::td{colwidth=100,200}\nCell\n:::\n:::\n::::\n";
        let doc = markdown_to_adf(md).unwrap();
        let cell = &doc.content[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap()[0];
        let colwidth = cell.attrs.as_ref().unwrap()["colwidth"].as_array().unwrap();
        assert_eq!(
            colwidth,
            &[serde_json::json!(100.0), serde_json::json!(200.0)]
        );
    }

    #[test]
    fn directive_table_colwidth_float_roundtrip() {
        // Confluence returns colwidth as floats (e.g. 157.0, 863.0).
        // adf_to_markdown must preserve them so markdown_to_adf can restore them.
        let adf_doc = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "table",
                "content": [{
                    "type": "tableRow",
                    "content": [
                        {
                            "type": "tableHeader",
                            "attrs": { "colwidth": [157.0] },
                            "content": [{ "type": "paragraph" }]
                        },
                        {
                            "type": "tableHeader",
                            "attrs": { "colwidth": [863.0] },
                            "content": [{ "type": "paragraph" }]
                        }
                    ]
                }]
            }]
        });
        let doc: crate::atlassian::adf::AdfDocument = serde_json::from_value(adf_doc).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("colwidth=157.0"),
            "expected colwidth=157.0 in markdown, got: {md}"
        );
        assert!(
            md.contains("colwidth=863.0"),
            "expected colwidth=863.0 in markdown, got: {md}"
        );
        // Round-trip back to ADF
        let doc2 = markdown_to_adf(&md).unwrap();
        let row = &doc2.content[0].content.as_ref().unwrap()[0];
        let header1 = &row.content.as_ref().unwrap()[0];
        let header2 = &row.content.as_ref().unwrap()[1];
        assert_eq!(
            header1.attrs.as_ref().unwrap()["colwidth"]
                .as_array()
                .unwrap(),
            &[serde_json::json!(157.0)]
        );
        assert_eq!(
            header2.attrs.as_ref().unwrap()["colwidth"]
                .as_array()
                .unwrap(),
            &[serde_json::json!(863.0)]
        );
    }

    #[test]
    fn colwidth_float_preserved_in_roundtrip() {
        // Issue #369: colwidth 254.0 was coerced to integer 254
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","attrs":{"isNumberColumnEnabled":false,"layout":"default"},"content":[{"type":"tableRow","content":[{"type":"tableHeader","attrs":{"colwidth":[254.0,416.0]},"content":[{"type":"paragraph","content":[]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let cell = &round_tripped.content[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap()[0];
        let colwidth = cell.attrs.as_ref().unwrap()["colwidth"].as_array().unwrap();
        assert_eq!(
            colwidth,
            &[serde_json::json!(254.0), serde_json::json!(416.0)],
            "colwidth should preserve float values"
        );
    }

    #[test]
    fn default_rowspan_colspan_preserved_in_roundtrip() {
        // Issue #369: rowspan=1 and colspan=1 were elided
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","attrs":{"isNumberColumnEnabled":false,"layout":"default"},"content":[{"type":"tableRow","content":[{"type":"tableCell","attrs":{"rowspan":1,"colspan":1},"content":[{"type":"paragraph","content":[{"type":"text","text":"cell"}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let cell = &round_tripped.content[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap()[0];
        let attrs = cell.attrs.as_ref().unwrap();
        assert_eq!(attrs["rowspan"], 1, "rowspan=1 should be preserved");
        assert_eq!(attrs["colspan"], 1, "colspan=1 should be preserved");
    }

    // ── Nested list tests ──────────────────────────────────────────────

    #[test]
    fn table_localid_preserved_in_roundtrip() {
        // Issue #374: localId on table nodes was dropped
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","attrs":{"isNumberColumnEnabled":false,"layout":"default","localId":"7afd4550-e66c-4b12-875f-a91c6c7b62c7"},"content":[{"type":"tableRow","content":[{"type":"tableCell","attrs":{},"content":[{"type":"paragraph","content":[{"type":"text","text":"cell"}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("localId="),
            "JFM should contain localId, got: {md}"
        );
        let round_tripped = markdown_to_adf(&md).unwrap();
        let attrs = round_tripped.content[0].attrs.as_ref().unwrap();
        assert_eq!(
            attrs["localId"], "7afd4550-e66c-4b12-875f-a91c6c7b62c7",
            "localId should be preserved"
        );
    }

    #[test]
    fn paragraph_localid_preserved_in_roundtrip() {
        // Issue #399: localId on paragraph nodes was dropped
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"paragraph","attrs":{"localId":"abc-123"},"content":[{"type":"text","text":"hello"}]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("localId=abc-123"),
            "JFM should contain localId, got: {md}"
        );
        let round_tripped = markdown_to_adf(&md).unwrap();
        let attrs = round_tripped.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["localId"], "abc-123", "localId should be preserved");
    }

    #[test]
    fn heading_localid_preserved_in_roundtrip() {
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"heading","attrs":{"level":2,"localId":"h-456"},"content":[{"type":"text","text":"Title"}]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let attrs = round_tripped.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["localId"], "h-456");
    }

    #[test]
    fn localid_with_alignment_preserved() {
        // localId and alignment marks should coexist in the same {attrs} block
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"paragraph","attrs":{"localId":"p-789"},"marks":[{"type":"alignment","attrs":{"align":"center"}}],
           "content":[{"type":"text","text":"centered"}]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("localId=p-789"), "should have localId: {md}");
        assert!(md.contains("align=center"), "should have align: {md}");
        let round_tripped = markdown_to_adf(&md).unwrap();
        let attrs = round_tripped.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["localId"], "p-789");
        let marks = round_tripped.content[0].marks.as_ref().unwrap();
        assert!(marks.iter().any(|m| m.mark_type == "alignment"));
    }

    #[test]
    fn table_layout_default_preserved_in_roundtrip() {
        // Issue #380: layout='default' was elided
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","attrs":{"isNumberColumnEnabled":false,"layout":"default"},"content":[{"type":"tableRow","content":[{"type":"tableCell","attrs":{},"content":[{"type":"paragraph","content":[{"type":"text","text":"cell"}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let attrs = round_tripped.content[0].attrs.as_ref().unwrap();
        assert_eq!(
            attrs["layout"], "default",
            "layout='default' should be preserved"
        );
    }

    #[test]
    fn table_is_number_column_enabled_false_preserved() {
        // Issue #380: isNumberColumnEnabled=false was elided
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","attrs":{"isNumberColumnEnabled":false,"layout":"default"},"content":[{"type":"tableRow","content":[{"type":"tableCell","attrs":{},"content":[{"type":"paragraph","content":[{"type":"text","text":"cell"}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let attrs = round_tripped.content[0].attrs.as_ref().unwrap();
        assert_eq!(
            attrs["isNumberColumnEnabled"], false,
            "isNumberColumnEnabled=false should be preserved"
        );
    }

    #[test]
    fn table_is_number_column_enabled_true_preserved() {
        // Regression check: isNumberColumnEnabled=true should still work
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","attrs":{"isNumberColumnEnabled":true,"layout":"default"},"content":[{"type":"tableRow","content":[{"type":"tableCell","attrs":{},"content":[{"type":"paragraph","content":[{"type":"text","text":"cell"}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let attrs = round_tripped.content[0].attrs.as_ref().unwrap();
        assert_eq!(
            attrs["isNumberColumnEnabled"], true,
            "isNumberColumnEnabled=true should be preserved"
        );
    }

    #[test]
    fn directive_table_is_number_column_enabled_false_preserved() {
        // Covers render_directive_table + directive table parsing for numbered=false.
        // Multi-paragraph cell forces directive table form.
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","attrs":{"isNumberColumnEnabled":false,"layout":"default"},"content":[{"type":"tableRow","content":[{"type":"tableCell","attrs":{},"content":[
          {"type":"paragraph","content":[{"type":"text","text":"line one"}]},
          {"type":"paragraph","content":[{"type":"text","text":"line two"}]}
        ]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("::::table"), "should use directive table form");
        assert!(
            md.contains("numbered=false"),
            "should contain numbered=false, got: {md}"
        );
        let round_tripped = markdown_to_adf(&md).unwrap();
        let attrs = round_tripped.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["isNumberColumnEnabled"], false);
        assert_eq!(attrs["layout"], "default");
    }

    #[test]
    fn directive_table_is_number_column_enabled_true_preserved() {
        // Covers render_directive_table + directive table parsing for numbered (true).
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","attrs":{"isNumberColumnEnabled":true,"layout":"default"},"content":[{"type":"tableRow","content":[{"type":"tableCell","attrs":{},"content":[
          {"type":"paragraph","content":[{"type":"text","text":"line one"}]},
          {"type":"paragraph","content":[{"type":"text","text":"line two"}]}
        ]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("::::table"), "should use directive table form");
        assert!(
            md.contains("numbered}") || md.contains("numbered "),
            "should contain numbered flag, got: {md}"
        );
        let round_tripped = markdown_to_adf(&md).unwrap();
        let attrs = round_tripped.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["isNumberColumnEnabled"], true);
    }

    #[test]
    fn trailing_space_in_bullet_list_item_preserved() {
        // Issue #394: trailing space text node in list item dropped
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[
          {"type":"listItem","content":[{"type":"paragraph","content":[
            {"type":"text","text":"Before link "},
            {"type":"text","text":"link text","marks":[{"type":"link","attrs":{"href":"https://example.com"}}]},
            {"type":"text","text":" "}
          ]}]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let list = &round_tripped.content[0];
        let item = &list.content.as_ref().unwrap()[0];
        let para = &item.content.as_ref().unwrap()[0];
        let inlines = para.content.as_ref().unwrap();
        let last = inlines.last().unwrap();
        assert_eq!(
            last.text.as_deref(),
            Some(" "),
            "trailing space text node should be preserved, got nodes: {:?}",
            inlines
                .iter()
                .map(|n| (&n.node_type, &n.text))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn trailing_space_after_mention_in_bullet_list_preserved() {
        // Mention + trailing space in list item
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[
          {"type":"listItem","content":[{"type":"paragraph","content":[
            {"type":"mention","attrs":{"id":"abc","text":"@Alice"}},
            {"type":"text","text":" "}
          ]}]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let para = &round_tripped.content[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap()[0];
        let inlines = para.content.as_ref().unwrap();
        assert!(
            inlines.len() >= 2,
            "should have mention + trailing space, got {} nodes",
            inlines.len()
        );
        assert_eq!(inlines.last().unwrap().text.as_deref(), Some(" "));
    }

    #[test]
    fn trailing_space_in_ordered_list_item_preserved() {
        // Same issue in ordered list context
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"orderedList","attrs":{"order":1},"content":[
          {"type":"listItem","content":[{"type":"paragraph","content":[
            {"type":"text","text":"item "},
            {"type":"text","text":"link","marks":[{"type":"link","attrs":{"href":"https://example.com"}}]},
            {"type":"text","text":" "}
          ]}]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let para = &round_tripped.content[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap()[0];
        let inlines = para.content.as_ref().unwrap();
        let last = inlines.last().unwrap();
        assert_eq!(
            last.text.as_deref(),
            Some(" "),
            "trailing space should be preserved in ordered list item"
        );
    }

    #[test]
    fn trailing_space_in_heading_text_preserved() {
        // Issue #400: trailing space in heading text node trimmed on round-trip
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"heading","attrs":{"level":1},"content":[
          {"type":"text","text":"Firefighting Engineers "}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let inlines = round_tripped.content[0].content.as_ref().unwrap();
        assert_eq!(
            inlines[0].text.as_deref(),
            Some("Firefighting Engineers "),
            "trailing space in heading should be preserved"
        );
    }

    #[test]
    fn trailing_space_in_heading_before_bold_preserved() {
        // Issue #400: trailing space before bold sibling in heading
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"heading","attrs":{"level":2},"content":[
          {"type":"text","text":"Classic "},
          {"type":"text","text":"bold","marks":[{"type":"strong"}]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let inlines = round_tripped.content[0].content.as_ref().unwrap();
        assert_eq!(
            inlines[0].text.as_deref(),
            Some("Classic "),
            "trailing space in heading text before bold should be preserved"
        );
    }

    #[test]
    fn trailing_space_in_paragraph_text_preserved() {
        // Issue #400: trailing space in paragraph text node preserved on round-trip
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"word followed by space "},
          {"type":"text","text":"next node","marks":[{"type":"strong"}]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let inlines = round_tripped.content[0].content.as_ref().unwrap();
        assert_eq!(
            inlines[0].text.as_deref(),
            Some("word followed by space "),
            "trailing space in paragraph text should be preserved"
        );
    }

    #[test]
    fn nested_bullet_list_roundtrip() {
        // ADF with a listItem containing a paragraph + nested bulletList
        let adf_doc = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "bulletList",
                "content": [{
                    "type": "listItem",
                    "content": [
                        {
                            "type": "paragraph",
                            "content": [{"type": "text", "text": "parent item"}]
                        },
                        {
                            "type": "bulletList",
                            "content": [
                                {
                                    "type": "listItem",
                                    "content": [{
                                        "type": "paragraph",
                                        "content": [{"type": "text", "text": "sub item 1"}]
                                    }]
                                },
                                {
                                    "type": "listItem",
                                    "content": [{
                                        "type": "paragraph",
                                        "content": [{"type": "text", "text": "sub item 2"}]
                                    }]
                                }
                            ]
                        }
                    ]
                }]
            }]
        });
        let doc: AdfDocument = serde_json::from_value(adf_doc).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("- parent item\n"),
            "expected top-level item in markdown, got: {md}"
        );
        assert!(
            md.contains("  - sub item 1\n"),
            "expected indented sub item 1 in markdown, got: {md}"
        );
        assert!(
            md.contains("  - sub item 2\n"),
            "expected indented sub item 2 in markdown, got: {md}"
        );

        // Round-trip back
        let doc2 = markdown_to_adf(&md).unwrap();
        let list = &doc2.content[0];
        assert_eq!(list.node_type, "bulletList");
        let item = &list.content.as_ref().unwrap()[0];
        assert_eq!(item.node_type, "listItem");
        let item_content = item.content.as_ref().unwrap();
        assert_eq!(
            item_content.len(),
            2,
            "listItem should have paragraph + nested list"
        );
        assert_eq!(item_content[0].node_type, "paragraph");
        assert_eq!(item_content[1].node_type, "bulletList");
        let sub_items = item_content[1].content.as_ref().unwrap();
        assert_eq!(sub_items.len(), 2);
    }

    #[test]
    fn nested_bullet_in_table_cell_roundtrip() {
        let md = "::::table\n:::tr\n:::td\n- parent\n  - child\n:::\n:::\n::::\n";
        let doc = markdown_to_adf(md).unwrap();
        let table = &doc.content[0];
        let row = &table.content.as_ref().unwrap()[0];
        let cell = &row.content.as_ref().unwrap()[0];
        let list = &cell.content.as_ref().unwrap()[0];
        assert_eq!(list.node_type, "bulletList");
        let item = &list.content.as_ref().unwrap()[0];
        let item_content = item.content.as_ref().unwrap();
        assert_eq!(
            item_content.len(),
            2,
            "listItem should have paragraph + nested list"
        );
        assert_eq!(item_content[1].node_type, "bulletList");

        // Round-trip: adf→md→adf should preserve the nested list
        let md2 = adf_to_markdown(&doc).unwrap();
        assert!(
            md2.contains("  - child"),
            "expected indented child in round-tripped markdown, got: {md2}"
        );
    }

    #[test]
    fn nested_ordered_list_roundtrip() {
        // Issue #389: nested orderedList inside listItem flattened
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"orderedList","attrs":{"order":1},"content":[
          {"type":"listItem","content":[
            {"type":"paragraph","content":[{"type":"text","text":"Top level"}]},
            {"type":"orderedList","attrs":{"order":1},"content":[
              {"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"Nested 1"}]}]},
              {"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"Nested 2"}]}]}
            ]}
          ]},
          {"type":"listItem","content":[
            {"type":"paragraph","content":[{"type":"text","text":"Second top"}]}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();

        // Outer list should have 2 items
        let outer = &round_tripped.content[0];
        assert_eq!(outer.node_type, "orderedList");
        assert_eq!(outer.attrs.as_ref().unwrap()["order"], 1);
        let outer_items = outer.content.as_ref().unwrap();
        assert_eq!(
            outer_items.len(),
            2,
            "outer list should have 2 items, got {}",
            outer_items.len()
        );

        // First item should have paragraph + nested orderedList
        let first_item = &outer_items[0];
        let first_content = first_item.content.as_ref().unwrap();
        assert_eq!(
            first_content.len(),
            2,
            "first listItem should have paragraph + nested list, got {}",
            first_content.len()
        );
        assert_eq!(first_content[0].node_type, "paragraph");
        assert_eq!(first_content[1].node_type, "orderedList");
        let nested_items = first_content[1].content.as_ref().unwrap();
        assert_eq!(nested_items.len(), 2, "nested list should have 2 items");
    }

    #[test]
    fn nested_ordered_list_markdown_parsing() {
        // Direct markdown parsing of nested ordered list
        let md = "1. Top level\n  1. Nested 1\n  2. Nested 2\n2. Second top\n";
        let doc = markdown_to_adf(md).unwrap();
        let outer = &doc.content[0];
        assert_eq!(outer.node_type, "orderedList");
        let outer_items = outer.content.as_ref().unwrap();
        assert_eq!(outer_items.len(), 2, "should have 2 top-level items");

        let first_content = outer_items[0].content.as_ref().unwrap();
        assert_eq!(
            first_content.len(),
            2,
            "first item should have paragraph + nested list"
        );
        assert_eq!(first_content[1].node_type, "orderedList");
    }

    #[test]
    fn bullet_list_nested_inside_ordered_list() {
        // Mixed nesting: bullet list nested inside ordered list
        let md = "1. Ordered item\n  - Bullet child 1\n  - Bullet child 2\n2. Second ordered\n";
        let doc = markdown_to_adf(md).unwrap();
        let outer = &doc.content[0];
        assert_eq!(outer.node_type, "orderedList");
        let outer_items = outer.content.as_ref().unwrap();
        assert_eq!(outer_items.len(), 2);

        let first_content = outer_items[0].content.as_ref().unwrap();
        assert_eq!(
            first_content.len(),
            2,
            "first item should have paragraph + nested list"
        );
        assert_eq!(first_content[1].node_type, "bulletList");
        let sub_items = first_content[1].content.as_ref().unwrap();
        assert_eq!(sub_items.len(), 2, "nested bullet list should have 2 items");
    }

    #[test]
    fn ordered_list_order_attr_always_preserved() {
        // order=1 should be preserved, not elided
        let md = "1. A\n2. B\n";
        let doc = markdown_to_adf(md).unwrap();
        let attrs = doc.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["order"], 1, "order=1 should be explicitly present");

        // Round-trip should preserve it
        let md2 = adf_to_markdown(&doc).unwrap();
        let doc2 = markdown_to_adf(&md2).unwrap();
        let attrs2 = doc2.content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs2["order"], 1);
    }

    // ── File media round-trip tests ─────────────────────────────────────

    #[test]
    fn file_media_roundtrip() {
        // ADF with a Confluence file attachment (type:file media)
        let adf_doc = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "mediaSingle",
                "attrs": {"layout": "center"},
                "content": [{
                    "type": "media",
                    "attrs": {
                        "type": "file",
                        "id": "6e8ebc85-81a3-4b4c-865a-ec4dd8978c2d",
                        "collection": "contentId-8220672100",
                        "height": 56,
                        "width": 312,
                        "alt": "Screenshot.png"
                    }
                }]
            }]
        });
        let doc: crate::atlassian::adf::AdfDocument = serde_json::from_value(adf_doc).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("type=file"),
            "expected type=file in markdown, got: {md}"
        );
        assert!(
            md.contains("id=6e8ebc85-81a3-4b4c-865a-ec4dd8978c2d"),
            "expected id in markdown, got: {md}"
        );
        assert!(
            md.contains("collection=contentId-8220672100"),
            "expected collection in markdown, got: {md}"
        );
        // Round-trip back to ADF
        let doc2 = markdown_to_adf(&md).unwrap();
        let ms = &doc2.content[0];
        assert_eq!(ms.node_type, "mediaSingle");
        let media = &ms.content.as_ref().unwrap()[0];
        assert_eq!(media.node_type, "media");
        let attrs = media.attrs.as_ref().unwrap();
        assert_eq!(attrs["type"], "file");
        assert_eq!(attrs["id"], "6e8ebc85-81a3-4b4c-865a-ec4dd8978c2d");
        assert_eq!(attrs["collection"], "contentId-8220672100");
        assert_eq!(attrs["height"], 56);
        assert_eq!(attrs["width"], 312);
        assert_eq!(attrs["alt"], "Screenshot.png");
    }

    #[test]
    fn table_width_roundtrip() {
        // ADF table with width attribute
        let adf_doc = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "table",
                "attrs": {"layout": "default", "width": 760.0},
                "content": [{
                    "type": "tableRow",
                    "content": [{
                        "type": "tableHeader",
                        "content": [{"type": "paragraph", "content": [{"type": "text", "text": "H"}]}]
                    }]
                }]
            }]
        });
        let doc: crate::atlassian::adf::AdfDocument = serde_json::from_value(adf_doc).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("width=760"),
            "expected width=760 in markdown, got: {md}"
        );
        // Round-trip back to ADF
        let doc2 = markdown_to_adf(&md).unwrap();
        let table = &doc2.content[0];
        assert_eq!(table.node_type, "table");
        let table_attrs = table.attrs.as_ref().unwrap();
        assert_eq!(table_attrs["width"], 760.0);
    }

    #[test]
    fn file_media_width_type_roundtrip() {
        // mediaSingle with widthType:pixel should survive round-trip
        let adf_doc = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "mediaSingle",
                "attrs": {"layout": "center", "width": 312, "widthType": "pixel"},
                "content": [{
                    "type": "media",
                    "attrs": {
                        "type": "file",
                        "id": "abc123",
                        "collection": "contentId-999",
                        "height": 56,
                        "width": 312
                    }
                }]
            }]
        });
        let doc: crate::atlassian::adf::AdfDocument = serde_json::from_value(adf_doc).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("widthType=pixel"),
            "expected widthType=pixel in markdown, got: {md}"
        );
        let doc2 = markdown_to_adf(&md).unwrap();
        let ms = &doc2.content[0];
        let ms_attrs = ms.attrs.as_ref().unwrap();
        assert_eq!(ms_attrs["widthType"], "pixel");
        assert_eq!(ms_attrs["width"], 312);
    }

    #[test]
    fn file_media_mode_roundtrip() {
        // mediaSingle with mode attr should survive round-trip (issue #431)
        let adf_doc = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "mediaSingle",
                "attrs": {"layout": "wide", "mode": "wide", "width": 1200},
                "content": [{
                    "type": "media",
                    "attrs": {
                        "type": "file",
                        "id": "abc123",
                        "collection": "test",
                        "width": 1200,
                        "height": 600
                    }
                }]
            }]
        });
        let doc: crate::atlassian::adf::AdfDocument = serde_json::from_value(adf_doc).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("mode=wide"),
            "expected mode=wide in markdown, got: {md}"
        );
        let doc2 = markdown_to_adf(&md).unwrap();
        let ms = &doc2.content[0];
        let ms_attrs = ms.attrs.as_ref().unwrap();
        assert_eq!(ms_attrs["mode"], "wide");
        assert_eq!(ms_attrs["layout"], "wide");
        assert_eq!(ms_attrs["width"], 1200);
    }

    #[test]
    fn external_media_mode_roundtrip() {
        // External mediaSingle with mode attr should survive round-trip (issue #431)
        let adf_doc = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "mediaSingle",
                "attrs": {"layout": "wide", "mode": "wide"},
                "content": [{
                    "type": "media",
                    "attrs": {
                        "type": "external",
                        "url": "https://example.com/image.png"
                    }
                }]
            }]
        });
        let doc: crate::atlassian::adf::AdfDocument = serde_json::from_value(adf_doc).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("mode=wide"),
            "expected mode=wide in markdown, got: {md}"
        );
        let doc2 = markdown_to_adf(&md).unwrap();
        let ms = &doc2.content[0];
        let ms_attrs = ms.attrs.as_ref().unwrap();
        assert_eq!(ms_attrs["mode"], "wide");
        assert_eq!(ms_attrs["layout"], "wide");
    }

    #[test]
    fn media_mode_only_roundtrip() {
        // mediaSingle with mode but default layout should still preserve mode (issue #431)
        let adf_doc = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "mediaSingle",
                "attrs": {"layout": "center", "mode": "default"},
                "content": [{
                    "type": "media",
                    "attrs": {
                        "type": "external",
                        "url": "https://example.com/image.png"
                    }
                }]
            }]
        });
        let doc: crate::atlassian::adf::AdfDocument = serde_json::from_value(adf_doc).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("mode=default"),
            "expected mode=default in markdown, got: {md}"
        );
        let doc2 = markdown_to_adf(&md).unwrap();
        let ms = &doc2.content[0];
        let ms_attrs = ms.attrs.as_ref().unwrap();
        assert_eq!(ms_attrs["mode"], "default");
    }

    #[test]
    fn file_media_hex_localid_roundtrip() {
        // Issue #432: short hex localId (non-UUID) must survive round-trip
        let adf_doc = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "mediaSingle",
                "attrs": {"layout": "wide", "width": 1200, "widthType": "pixel"},
                "content": [{
                    "type": "media",
                    "attrs": {
                        "type": "file",
                        "id": "eb7a9c3b-314e-4458-8200-4b22b67b122e",
                        "collection": "contentId-123",
                        "height": 484,
                        "width": 915,
                        "alt": "image.png",
                        "localId": "0e79f58ac382"
                    }
                }]
            }]
        });
        let doc: crate::atlassian::adf::AdfDocument = serde_json::from_value(adf_doc).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("localId=0e79f58ac382"),
            "expected localId=0e79f58ac382 in markdown, got: {md}"
        );
        let doc2 = markdown_to_adf(&md).unwrap();
        let ms = &doc2.content[0];
        let media = &ms.content.as_ref().unwrap()[0];
        let attrs = media.attrs.as_ref().unwrap();
        assert_eq!(attrs["localId"], "0e79f58ac382");
    }

    #[test]
    fn file_media_uuid_localid_roundtrip() {
        // UUID-format localId must also survive round-trip
        let adf_doc = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "mediaSingle",
                "attrs": {"layout": "center"},
                "content": [{
                    "type": "media",
                    "attrs": {
                        "type": "file",
                        "id": "abc-123",
                        "collection": "contentId-456",
                        "height": 100,
                        "width": 200,
                        "localId": "a1b2c3d4-e5f6-7890-abcd-ef1234567890"
                    }
                }]
            }]
        });
        let doc: crate::atlassian::adf::AdfDocument = serde_json::from_value(adf_doc).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("localId=a1b2c3d4-e5f6-7890-abcd-ef1234567890"),
            "expected UUID localId in markdown, got: {md}"
        );
        let doc2 = markdown_to_adf(&md).unwrap();
        let media = &doc2.content[0].content.as_ref().unwrap()[0];
        let attrs = media.attrs.as_ref().unwrap();
        assert_eq!(attrs["localId"], "a1b2c3d4-e5f6-7890-abcd-ef1234567890");
    }

    #[test]
    fn file_media_null_uuid_localid_stripped() {
        // Null UUID localId should be stripped (consistent with other node types)
        let adf_doc = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "mediaSingle",
                "attrs": {"layout": "center"},
                "content": [{
                    "type": "media",
                    "attrs": {
                        "type": "file",
                        "id": "abc-123",
                        "collection": "contentId-456",
                        "height": 100,
                        "width": 200,
                        "localId": "00000000-0000-0000-0000-000000000000"
                    }
                }]
            }]
        });
        let doc: crate::atlassian::adf::AdfDocument = serde_json::from_value(adf_doc).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            !md.contains("localId="),
            "null UUID localId should be stripped, got: {md}"
        );
    }

    #[test]
    fn file_media_localid_stripped_when_option_set() {
        // localId should be stripped when strip_local_ids option is enabled
        let adf_doc = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "mediaSingle",
                "attrs": {"layout": "center"},
                "content": [{
                    "type": "media",
                    "attrs": {
                        "type": "file",
                        "id": "abc-123",
                        "collection": "contentId-456",
                        "height": 100,
                        "width": 200,
                        "localId": "0e79f58ac382"
                    }
                }]
            }]
        });
        let doc: crate::atlassian::adf::AdfDocument = serde_json::from_value(adf_doc).unwrap();
        let opts = RenderOptions {
            strip_local_ids: true,
            ..Default::default()
        };
        let md = adf_to_markdown_with_options(&doc, &opts).unwrap();
        assert!(
            !md.contains("localId="),
            "localId should be stripped with strip_local_ids, got: {md}"
        );
    }

    #[test]
    fn external_media_localid_roundtrip() {
        // localId on external media nodes must also survive round-trip
        let adf_doc = serde_json::json!({
            "type": "doc",
            "version": 1,
            "content": [{
                "type": "mediaSingle",
                "attrs": {"layout": "center"},
                "content": [{
                    "type": "media",
                    "attrs": {
                        "type": "external",
                        "url": "https://example.com/image.png",
                        "alt": "test",
                        "localId": "deadbeef1234"
                    }
                }]
            }]
        });
        let doc: crate::atlassian::adf::AdfDocument = serde_json::from_value(adf_doc).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("localId=deadbeef1234"),
            "expected localId in markdown for external media, got: {md}"
        );
        let doc2 = markdown_to_adf(&md).unwrap();
        let media = &doc2.content[0].content.as_ref().unwrap()[0];
        let attrs = media.attrs.as_ref().unwrap();
        assert_eq!(attrs["localId"], "deadbeef1234");
    }

    #[test]
    fn bracket_in_text_not_parsed_as_link() {
        // "[Task] some text (Link)" — the [Task] must NOT be treated as a link anchor
        let md = ":check_mark: [Task] Unable to start trial ([Link](https://example.com/link))";
        let doc = markdown_to_adf(md).unwrap();
        let para = &doc.content[0];
        assert_eq!(para.node_type, "paragraph");
        let content = para.content.as_ref().unwrap();
        // Find the text node containing "[Task]"
        let text_nodes: Vec<_> = content.iter().filter(|n| n.node_type == "text").collect();
        let has_task_bracket = text_nodes
            .iter()
            .any(|n| n.text.as_deref().unwrap_or("").contains("[Task]"));
        assert!(
            has_task_bracket,
            "expected [Task] in plain text, nodes: {content:?}"
        );
        // Also verify the (Link) is a proper link
        let link_nodes: Vec<_> = content
            .iter()
            .filter(|n| {
                n.marks
                    .as_ref()
                    .is_some_and(|m| m.iter().any(|mk| mk.mark_type == "link"))
            })
            .collect();
        assert!(!link_nodes.is_empty(), "expected a link node");
        assert_eq!(
            link_nodes[0].text.as_deref(),
            Some("Link"),
            "link text should be 'Link'"
        );
    }

    #[test]
    fn empty_paragraph_roundtrip() {
        // An empty ADF paragraph node should survive a round-trip through markdown
        let mut adf_in = AdfDocument::new();
        adf_in.content = vec![
            AdfNode::paragraph(vec![AdfNode::text("before")]),
            AdfNode::paragraph(vec![]),
            AdfNode::paragraph(vec![AdfNode::text("after")]),
        ];
        let md = adf_to_markdown(&adf_in).unwrap();
        let adf_out = markdown_to_adf(&md).unwrap();
        assert_eq!(
            adf_out.content.len(),
            3,
            "should have 3 blocks, markdown:\n{md}"
        );
        assert_eq!(adf_out.content[0].node_type, "paragraph");
        assert_eq!(adf_out.content[1].node_type, "paragraph");
        assert!(
            adf_out.content[1].content.is_none(),
            "middle paragraph should be empty"
        );
        assert_eq!(adf_out.content[2].node_type, "paragraph");
    }

    #[test]
    fn nbsp_paragraph_roundtrip() {
        // Issue #411: paragraph with only NBSP should survive round-trip
        let adf_json = "{\"version\":1,\"type\":\"doc\",\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"\\u00a0\"}]}]}";
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("::paragraph["),
            "NBSP paragraph should use directive form: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        assert_eq!(rt.content.len(), 1, "should have 1 block");
        assert_eq!(rt.content[0].node_type, "paragraph");
        let text = rt.content[0].content.as_ref().unwrap()[0]
            .text
            .as_deref()
            .unwrap_or("");
        assert_eq!(text, "\u{00a0}", "NBSP should survive round-trip");
    }

    #[test]
    fn nbsp_in_nested_expand_roundtrip() {
        // Issue #411 real-world case: NBSP paragraph inside nestedExpand
        let adf_json = "{\"version\":1,\"type\":\"doc\",\"content\":[{\"type\":\"nestedExpand\",\"attrs\":{\"title\":\"Section\"},\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"\\u00a0\"}]}]}]}";
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();
        let ne = &rt.content[0];
        assert_eq!(ne.node_type, "nestedExpand");
        let inner = ne.content.as_ref().unwrap();
        assert_eq!(inner.len(), 1, "should have 1 inner block");
        assert_eq!(inner[0].node_type, "paragraph");
        let content = inner[0].content.as_ref().unwrap();
        assert!(!content.is_empty(), "paragraph should not be empty");
        let text = content[0].text.as_deref().unwrap_or("");
        assert_eq!(text, "\u{00a0}", "NBSP should survive in nestedExpand");
    }

    #[test]
    fn nbsp_followed_by_content() {
        // NBSP paragraph followed by regular content should not interfere
        let adf_json = "{\"version\":1,\"type\":\"doc\",\"content\":[{\"type\":\"nestedExpand\",\"attrs\":{\"title\":\"S\"},\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"\\u00a0\"}]}]},{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"after\"}]}]}";
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();
        assert!(rt.content.len() >= 2, "should have at least 2 blocks");
        // The second block should be a paragraph with "after"
        let after_para = rt.content.iter().find(|n| {
            n.node_type == "paragraph"
                && n.content
                    .as_ref()
                    .and_then(|c| c.first())
                    .and_then(|n| n.text.as_deref())
                    .map_or(false, |t| t.contains("after"))
        });
        assert!(after_para.is_some(), "should have paragraph with 'after'");
    }

    #[test]
    fn nbsp_paragraph_with_marks_survives() {
        // NBSP with bold marks renders as `** **` which contains non-whitespace
        // chars and thus doesn't need the directive form — it round-trips naturally
        let adf_json = "{\"version\":1,\"type\":\"doc\",\"content\":[{\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"\\u00a0\",\"marks\":[{\"type\":\"strong\"}]}]}]}";
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("**"), "should have bold markers: {md}");
        let rt = markdown_to_adf(&md).unwrap();
        let content = rt.content[0].content.as_ref().unwrap();
        assert!(!content.is_empty(), "should preserve content");
    }

    #[test]
    fn regular_paragraph_unchanged() {
        // Regression guard: normal paragraphs should NOT use directive form
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"hello"}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            !md.contains("::paragraph"),
            "regular paragraphs should not use directive form: {md}"
        );
        assert!(md.contains("hello"));
    }

    #[test]
    fn paragraph_directive_with_content_parsed() {
        // ::paragraph[content] should parse to a paragraph with inline nodes
        let md = "::paragraph[\u{00a0}]\n";
        let doc = markdown_to_adf(md).unwrap();
        assert_eq!(doc.content.len(), 1);
        assert_eq!(doc.content[0].node_type, "paragraph");
        let content = doc.content[0].content.as_ref().unwrap();
        assert!(!content.is_empty(), "should have inline content");
        assert_eq!(content[0].text.as_deref().unwrap(), "\u{00a0}");
    }

    #[test]
    fn nbsp_paragraph_in_list_item_with_nested_list() {
        // Issue #448: NBSP paragraph content lost inside listItem with nested bulletList
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[{"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"\u00a0"}]},{"type":"bulletList","content":[{"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"sub item one"}]}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();
        let list = &rt.content[0];
        assert_eq!(list.node_type, "bulletList");
        let item = &list.content.as_ref().unwrap()[0];
        let item_content = item.content.as_ref().unwrap();
        assert_eq!(
            item_content.len(),
            2,
            "listItem should have paragraph + nested list, got: {item_content:?}"
        );
        let para = &item_content[0];
        assert_eq!(para.node_type, "paragraph");
        let para_content = para
            .content
            .as_ref()
            .expect("paragraph should have content");
        assert!(
            !para_content.is_empty(),
            "NBSP paragraph content should not be empty"
        );
        assert_eq!(
            para_content[0].text.as_deref().unwrap(),
            "\u{00a0}",
            "NBSP should survive round-trip inside listItem"
        );
    }

    #[test]
    fn nbsp_paragraph_in_list_item_with_local_ids() {
        // Issue #448: NBSP paragraph with localIds inside listItem with nested list
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[{"type":"listItem","attrs":{"localId":"li-001"},"content":[{"type":"paragraph","attrs":{"localId":"p-001"},"content":[{"type":"text","text":"\u00a0"}]},{"type":"bulletList","content":[{"type":"listItem","attrs":{"localId":"li-002"},"content":[{"type":"paragraph","attrs":{"localId":"p-002"},"content":[{"type":"text","text":"sub item"}]}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();
        let list = &rt.content[0];
        let item = &list.content.as_ref().unwrap()[0];
        // Check listItem localId
        assert_eq!(
            item.attrs.as_ref().unwrap()["localId"],
            "li-001",
            "listItem localId should survive"
        );
        let item_content = item.content.as_ref().unwrap();
        assert_eq!(item_content.len(), 2);
        // Check paragraph localId and NBSP content
        let para = &item_content[0];
        assert_eq!(
            para.attrs.as_ref().unwrap()["localId"],
            "p-001",
            "paragraph localId should survive"
        );
        let text = para.content.as_ref().unwrap()[0].text.as_deref().unwrap();
        assert_eq!(text, "\u{00a0}", "NBSP should survive with localIds");
    }

    #[test]
    fn nbsp_paragraph_in_list_item_without_nested_list() {
        // NBSP paragraph in a simple listItem (no nested list)
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[{"type":"listItem","attrs":{"localId":"li-001"},"content":[{"type":"paragraph","attrs":{"localId":"p-001"},"content":[{"type":"text","text":"\u00a0"}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();
        let list = &rt.content[0];
        let item = &list.content.as_ref().unwrap()[0];
        let para = &item.content.as_ref().unwrap()[0];
        let text = para.content.as_ref().unwrap()[0].text.as_deref().unwrap();
        assert_eq!(text, "\u{00a0}", "NBSP should survive in simple list item");
    }

    #[test]
    fn nbsp_paragraph_in_ordered_list_item_with_nested_list() {
        // NBSP paragraph in ordered listItem with nested bulletList
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"orderedList","content":[{"type":"listItem","attrs":{"localId":"li-001"},"content":[{"type":"paragraph","attrs":{"localId":"p-001"},"content":[{"type":"text","text":"\u00a0"}]},{"type":"bulletList","content":[{"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"sub item"}]}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();
        let list = &rt.content[0];
        let item = &list.content.as_ref().unwrap()[0];
        let item_content = item.content.as_ref().unwrap();
        assert_eq!(item_content.len(), 2);
        let para = &item_content[0];
        let text = para.content.as_ref().unwrap()[0].text.as_deref().unwrap();
        assert_eq!(text, "\u{00a0}", "NBSP should survive in ordered list item");
    }

    #[test]
    fn list_item_leading_space_preserved() {
        // Leading space in list item text must not be stripped
        let md = "- hello world\n- - text";
        let doc = markdown_to_adf(md).unwrap();
        let list = &doc.content[0];
        assert_eq!(list.node_type, "bulletList");
        let items = list.content.as_ref().unwrap();
        // First item: "hello world" (no leading space, unchanged)
        let first_para = &items[0].content.as_ref().unwrap()[0];
        let first_text = &first_para.content.as_ref().unwrap()[0];
        assert_eq!(first_text.text.as_deref(), Some("hello world"));
    }

    #[test]
    fn list_item_leading_space_not_stripped() {
        // When the markdown list item content has a leading space (e.g. " :emoji:"),
        // that space must reach parse_inline as-is.
        let md = "-  leading space text";
        let doc = markdown_to_adf(md).unwrap();
        let list = &doc.content[0];
        let items = list.content.as_ref().unwrap();
        let para = &items[0].content.as_ref().unwrap()[0];
        let text_node = &para.content.as_ref().unwrap()[0];
        // After "- " (2 chars), trim_end keeps the leading space: " leading space text"
        assert_eq!(
            text_node.text.as_deref(),
            Some(" leading space text"),
            "leading space should be preserved"
        );
    }

    // ── Nested container directive tests ───────────────────────────

    // ── hardBreak in table cell tests ────────────────────────────

    #[test]
    fn hardbreak_in_cell_uses_directive_table() {
        // A table cell with a hardBreak should NOT use pipe syntax
        // because the newline would break the row
        let adf = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![AdfNode::table_row(vec![
                AdfNode::table_cell(vec![AdfNode::paragraph(vec![
                    AdfNode::text("before"),
                    AdfNode::hard_break(),
                    AdfNode::text("after"),
                ])]),
            ])])],
        };
        let md = adf_to_markdown(&adf).unwrap();
        // Should render as directive table, not pipe table
        assert!(
            md.contains(":::td") || md.contains("::::table"),
            "Table with hardBreak should use directive form, got:\n{md}"
        );
        assert!(
            !md.contains("| before"),
            "Should NOT use pipe syntax with hardBreak"
        );
    }

    #[test]
    fn hardbreak_in_cell_roundtrips() {
        // Verify the directive table form preserves the hardBreak on round-trip
        let adf = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![AdfNode::table_row(vec![
                AdfNode::table_cell(vec![AdfNode::paragraph(vec![
                    AdfNode::text("line one"),
                    AdfNode::hard_break(),
                    AdfNode::text("line two"),
                ])]),
            ])])],
        };
        let md = adf_to_markdown(&adf).unwrap();
        let roundtripped = markdown_to_adf(&md).unwrap();

        // Should still have one table with one row with one cell
        assert_eq!(roundtripped.content.len(), 1);
        assert_eq!(roundtripped.content[0].node_type, "table");
        let rows = roundtripped.content[0].content.as_ref().unwrap();
        assert_eq!(
            rows.len(),
            1,
            "Should have exactly 1 row, got {}",
            rows.len()
        );
    }

    #[test]
    fn hardbreak_in_paragraph_roundtrips() {
        // Issue #373: hardBreak absorbed into preceding text node
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"line one"},
          {"type":"hardBreak"},
          {"type":"text","text":"line two"}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let inlines = round_tripped.content[0].content.as_ref().unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(
            types,
            vec!["text", "hardBreak", "text"],
            "hardBreak should be preserved, got: {types:?}"
        );
        assert_eq!(inlines[0].text.as_deref(), Some("line one"));
        assert_eq!(inlines[2].text.as_deref(), Some("line two"));
    }

    #[test]
    fn consecutive_hardbreaks_in_paragraph_roundtrip() {
        // Issue #410: consecutive hardBreak nodes collapsed on round-trip
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"before"},
          {"type":"hardBreak"},
          {"type":"hardBreak"},
          {"type":"text","text":"after"}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        assert_eq!(
            round_tripped.content.len(),
            1,
            "Should remain a single paragraph, got {} blocks",
            round_tripped.content.len()
        );
        let inlines = round_tripped.content[0].content.as_ref().unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(
            types,
            vec!["text", "hardBreak", "hardBreak", "text"],
            "Both hardBreaks should be preserved, got: {types:?}"
        );
        assert_eq!(inlines[0].text.as_deref(), Some("before"));
        assert_eq!(inlines[3].text.as_deref(), Some("after"));
    }

    #[test]
    fn hardbreak_only_paragraph_roundtrips() {
        // Issue #410: paragraph whose only content is a hardBreak is dropped
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"paragraph","content":[{"type":"hardBreak"}]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        assert_eq!(
            round_tripped.content.len(),
            1,
            "Paragraph should not be dropped, got {} blocks",
            round_tripped.content.len()
        );
        let inlines = round_tripped.content[0].content.as_ref().unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(
            types,
            vec!["hardBreak"],
            "hardBreak-only paragraph should preserve its content, got: {types:?}"
        );
    }

    #[test]
    fn issue_410_full_reproducer_roundtrips() {
        // Full reproducer from issue #410: consecutive hardBreaks + hardBreak-only paragraph
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"paragraph","content":[
            {"type":"text","text":"before"},
            {"type":"hardBreak"},
            {"type":"hardBreak"},
            {"type":"text","text":"after"}
          ]},
          {"type":"paragraph","content":[
            {"type":"hardBreak"}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        assert_eq!(
            round_tripped.content.len(),
            2,
            "Should have exactly 2 paragraphs, got {}",
            round_tripped.content.len()
        );
        // First paragraph: text, hardBreak, hardBreak, text
        let p1 = round_tripped.content[0].content.as_ref().unwrap();
        let types1: Vec<&str> = p1.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types1, vec!["text", "hardBreak", "hardBreak", "text"]);
        // Second paragraph: hardBreak only
        let p2 = round_tripped.content[1].content.as_ref().unwrap();
        let types2: Vec<&str> = p2.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types2, vec!["hardBreak"]);
    }

    #[test]
    fn trailing_space_hardbreak_still_parsed() {
        // Backward compatibility: trailing-space hardBreak (old JFM format) still parses
        let md = "line one  \nline two\n";
        let doc = markdown_to_adf(md).unwrap();
        let inlines = doc.content[0].content.as_ref().unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(
            types,
            vec!["text", "hardBreak", "text"],
            "Trailing-space hardBreak should still parse, got: {types:?}"
        );
    }

    #[test]
    fn trailing_hardbreak_at_end_of_paragraph_roundtrips() {
        // A paragraph ending with a hardBreak (no text after it)
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[
          {"type":"text","text":"text"},
          {"type":"hardBreak"}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let inlines = round_tripped.content[0].content.as_ref().unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(
            types,
            vec!["text", "hardBreak"],
            "Trailing hardBreak should be preserved, got: {types:?}"
        );
    }

    #[test]
    #[test]
    fn table_with_header_row_uses_pipe_syntax() {
        // A table with tableHeader in the first row should use pipe syntax
        let adf = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![AdfNode::table_row(vec![
                AdfNode::table_header(vec![AdfNode::paragraph(vec![AdfNode::text("header cell")])]),
            ])])],
        };
        let md = adf_to_markdown(&adf).unwrap();
        assert!(
            md.contains("| header cell |"),
            "Table with header row should use pipe syntax, got:\n{md}"
        );
    }

    #[test]
    fn table_without_header_row_uses_directive_syntax() {
        // Issue #392: tableCell-only first row must use directive syntax
        // to avoid converting tableCell → tableHeader on round-trip
        let adf = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![AdfNode::table_row(vec![
                AdfNode::table_cell(vec![AdfNode::paragraph(vec![AdfNode::text("simple cell")])]),
            ])])],
        };
        let md = adf_to_markdown(&adf).unwrap();
        assert!(
            md.contains("::::table"),
            "Table without header row should use directive syntax, got:\n{md}"
        );
    }

    #[test]
    fn tablecell_first_row_preserved_on_roundtrip() {
        // Issue #392: tableCell in first row round-trips as tableHeader
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","attrs":{},"content":[
          {"type":"tableRow","content":[
            {"type":"tableCell","attrs":{},"content":[{"type":"paragraph","content":[{"type":"text","text":"row1 cell"}]}]}
          ]},
          {"type":"tableRow","content":[
            {"type":"tableCell","attrs":{},"content":[{"type":"paragraph","content":[{"type":"text","text":"row2 cell"}]}]}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let rows = round_tripped.content[0].content.as_ref().unwrap();
        let row0_cell = &rows[0].content.as_ref().unwrap()[0];
        assert_eq!(
            row0_cell.node_type, "tableCell",
            "first row cell should remain tableCell, got: {}",
            row0_cell.node_type
        );
        let row1_cell = &rows[1].content.as_ref().unwrap()[0];
        assert_eq!(row1_cell.node_type, "tableCell");
    }

    #[test]
    fn mixed_header_and_cell_first_row_uses_pipe() {
        // A first row with at least one tableHeader qualifies for pipe syntax
        let adf = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![
                AdfNode::table_row(vec![
                    AdfNode::table_header(vec![AdfNode::paragraph(vec![AdfNode::text("H1")])]),
                    AdfNode::table_header(vec![AdfNode::paragraph(vec![AdfNode::text("H2")])]),
                ]),
                AdfNode::table_row(vec![
                    AdfNode::table_cell(vec![AdfNode::paragraph(vec![AdfNode::text("C1")])]),
                    AdfNode::table_cell(vec![AdfNode::paragraph(vec![AdfNode::text("C2")])]),
                ]),
            ])],
        };
        let md = adf_to_markdown(&adf).unwrap();
        assert!(
            md.contains("| H1 |"),
            "Table with header first row should use pipe syntax, got:\n{md}"
        );
        assert!(!md.contains("::::table"), "should not use directive syntax");
    }

    #[test]
    fn cell_contains_hard_break_true() {
        let para = AdfNode::paragraph(vec![
            AdfNode::text("a"),
            AdfNode::hard_break(),
            AdfNode::text("b"),
        ]);
        assert!(cell_contains_hard_break(&para));
    }

    #[test]
    fn cell_contains_hard_break_false() {
        let para = AdfNode::paragraph(vec![AdfNode::text("no break here")]);
        assert!(!cell_contains_hard_break(&para));
    }

    #[test]
    fn cell_contains_hard_break_empty() {
        let para = AdfNode::paragraph(vec![]);
        assert!(!cell_contains_hard_break(&para));
    }

    // ── Multi-paragraph container tests ──────────────────────────

    #[test]
    fn multi_paragraph_panel_roundtrips() {
        let adf = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode {
                node_type: "panel".to_string(),
                attrs: Some(serde_json::json!({"panelType": "info"})),
                content: Some(vec![
                    AdfNode::paragraph(vec![AdfNode::text("First paragraph.")]),
                    AdfNode::paragraph(vec![AdfNode::text("Second paragraph.")]),
                ]),
                text: None,
                marks: None,
                local_id: None,
                parameters: None,
            }],
        };

        let md = adf_to_markdown(&adf).unwrap();
        // Should have blank line between paragraphs inside the panel
        assert!(
            md.contains("First paragraph.\n\nSecond paragraph."),
            "Panel should have blank line between paragraphs, got:\n{md}"
        );

        // Round-trip should preserve two separate paragraphs
        let roundtripped = markdown_to_adf(&md).unwrap();
        assert_eq!(roundtripped.content.len(), 1);
        assert_eq!(roundtripped.content[0].node_type, "panel");
        let panel_content = roundtripped.content[0].content.as_ref().unwrap();
        assert_eq!(
            panel_content.len(),
            2,
            "Panel should have 2 paragraphs after round-trip, got {}",
            panel_content.len()
        );
    }

    #[test]
    fn multi_paragraph_expand_roundtrips() {
        let adf = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode {
                node_type: "expand".to_string(),
                attrs: Some(serde_json::json!({"title": "Details"})),
                content: Some(vec![
                    AdfNode::paragraph(vec![AdfNode::text("Para one.")]),
                    AdfNode::paragraph(vec![AdfNode::text("Para two.")]),
                ]),
                text: None,
                marks: None,
                local_id: None,
                parameters: None,
            }],
        };

        let md = adf_to_markdown(&adf).unwrap();
        let roundtripped = markdown_to_adf(&md).unwrap();
        let expand_content = roundtripped.content[0].content.as_ref().unwrap();
        assert_eq!(
            expand_content.len(),
            2,
            "Expand should have 2 paragraphs after round-trip, got {}",
            expand_content.len()
        );
    }

    #[test]
    fn consecutive_nested_expands_in_table_cell_roundtrip() {
        let cell_content = vec![
            AdfNode {
                node_type: "nestedExpand".to_string(),
                attrs: Some(serde_json::json!({"title": "First"})),
                content: Some(vec![AdfNode::paragraph(vec![AdfNode::text("item 1")])]),
                text: None,
                marks: None,
                local_id: None,
                parameters: None,
            },
            AdfNode {
                node_type: "nestedExpand".to_string(),
                attrs: Some(serde_json::json!({"title": "Second"})),
                content: Some(vec![AdfNode::paragraph(vec![AdfNode::text("item 2")])]),
                text: None,
                marks: None,
                local_id: None,
                parameters: None,
            },
        ];
        let adf = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![AdfNode::table_row(vec![
                AdfNode::table_cell(cell_content),
            ])])],
        };

        let md = adf_to_markdown(&adf).unwrap();
        assert!(
            md.contains(":::\n\n:::nested-expand"),
            "Should have blank line between consecutive nested-expands in cell, got:\n{md}"
        );

        let rt = markdown_to_adf(&md).unwrap();
        let cell = &rt.content[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap()[0];
        let cell_nodes = cell.content.as_ref().unwrap();
        let expand_count = cell_nodes
            .iter()
            .filter(|n| n.node_type == "nestedExpand")
            .count();
        assert_eq!(
            expand_count, 2,
            "Both nested-expands should survive round-trip, got {expand_count}"
        );
    }

    #[test]
    fn multi_paragraph_in_table_cell_roundtrip() {
        // Two paragraphs inside a directive table cell should survive round-trip
        let adf = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![AdfNode::table_row(vec![
                AdfNode::table_cell(vec![
                    AdfNode::paragraph(vec![AdfNode::text("Para one.")]),
                    AdfNode::paragraph(vec![AdfNode::text("Para two.")]),
                ]),
            ])])],
        };

        let md = adf_to_markdown(&adf).unwrap();
        assert!(
            md.contains("Para one.\n\nPara two."),
            "Should have blank line between paragraphs in cell, got:\n{md}"
        );

        let rt = markdown_to_adf(&md).unwrap();
        let cell = &rt.content[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap()[0];
        let para_count = cell
            .content
            .as_ref()
            .unwrap()
            .iter()
            .filter(|n| n.node_type == "paragraph")
            .count();
        assert_eq!(para_count, 2, "Both paragraphs should survive round-trip");
    }

    #[test]
    fn panel_inside_table_cell_roundtrip() {
        // A panel inside a directive table cell
        let adf = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![AdfNode::table_row(vec![
                AdfNode::table_cell(vec![
                    AdfNode::paragraph(vec![AdfNode::text("Before panel.")]),
                    AdfNode {
                        node_type: "panel".to_string(),
                        attrs: Some(serde_json::json!({"panelType": "info"})),
                        content: Some(vec![AdfNode::paragraph(vec![AdfNode::text(
                            "Panel content",
                        )])]),
                        text: None,
                        marks: None,
                        local_id: None,
                        parameters: None,
                    },
                ]),
            ])])],
        };

        let md = adf_to_markdown(&adf).unwrap();
        assert!(
            md.contains(":::panel"),
            "Should contain panel directive, got:\n{md}"
        );

        let rt = markdown_to_adf(&md).unwrap();
        let cell = &rt.content[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap()[0];
        let has_panel = cell
            .content
            .as_ref()
            .unwrap()
            .iter()
            .any(|n| n.node_type == "panel");
        assert!(has_panel, "Panel should survive round-trip in table cell");
    }

    #[test]
    fn three_consecutive_expands_in_table_cell() {
        let make_expand = |title: &str| AdfNode {
            node_type: "nestedExpand".to_string(),
            attrs: Some(serde_json::json!({"title": title})),
            content: Some(vec![AdfNode::paragraph(vec![AdfNode::text("content")])]),
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        };
        let adf = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![AdfNode::table_row(vec![
                AdfNode::table_cell(vec![
                    make_expand("First"),
                    make_expand("Second"),
                    make_expand("Third"),
                ]),
            ])])],
        };

        let md = adf_to_markdown(&adf).unwrap();
        let rt = markdown_to_adf(&md).unwrap();
        let cell = &rt.content[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap()[0];
        let expand_count = cell
            .content
            .as_ref()
            .unwrap()
            .iter()
            .filter(|n| n.node_type == "nestedExpand")
            .count();
        assert_eq!(expand_count, 3, "All 3 expands should survive round-trip");
    }

    // ── Nested container directive tests ───────────────────────────

    #[test]
    fn nested_expand_inside_panel() {
        let md = ":::panel{type=info}\n:::expand{title=\"Details\"}\nHidden content\n:::\nMore panel content\n:::";
        let adf = markdown_to_adf(md).unwrap();

        // Should produce a panel node
        assert_eq!(adf.content.len(), 1);
        assert_eq!(adf.content[0].node_type, "panel");

        // Panel should contain the expand AND "More panel content"
        let panel_content = adf.content[0].content.as_ref().unwrap();
        assert!(
            panel_content.len() >= 2,
            "Panel should contain expand + paragraph, got {} nodes",
            panel_content.len()
        );
    }

    #[test]
    fn nested_expand_inside_table_cell() {
        let md = "::::table\n:::tr\n:::td\n:::expand{title=\"Details\"}\nExpand content\n:::\n:::\n:::\n::::";
        let adf = markdown_to_adf(md).unwrap();

        // Should produce a table
        assert_eq!(adf.content.len(), 1);
        assert_eq!(adf.content[0].node_type, "table");

        // Table -> row -> cell -> should contain an expand node
        let rows = adf.content[0].content.as_ref().unwrap();
        assert_eq!(rows.len(), 1);
        let cells = rows[0].content.as_ref().unwrap();
        assert_eq!(cells.len(), 1);
        let cell_content = cells[0].content.as_ref().unwrap();
        assert!(
            cell_content.iter().any(|n| n.node_type == "expand"),
            "Cell should contain an expand node, got: {:?}",
            cell_content
                .iter()
                .map(|n| &n.node_type)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn nested_expand_inside_layout_column() {
        let md = ":::layout\n:::column{width=100}\n:::expand{title=\"Col Expand\"}\nExpanded\n:::\n:::\n:::";
        let adf = markdown_to_adf(md).unwrap();

        assert_eq!(adf.content.len(), 1);
        assert_eq!(adf.content[0].node_type, "layoutSection");

        let columns = adf.content[0].content.as_ref().unwrap();
        assert_eq!(columns.len(), 1);
        let col_content = columns[0].content.as_ref().unwrap();
        assert!(
            col_content.iter().any(|n| n.node_type == "expand"),
            "Column should contain an expand node, got: {:?}",
            col_content.iter().map(|n| &n.node_type).collect::<Vec<_>>()
        );
    }

    #[test]
    fn expand_localid_in_directive_attrs() {
        // Issue #412: localId should be in directive attrs, not trailing text
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"expand","attrs":{"localId":"exp-001","title":"Details"},"content":[
            {"type":"paragraph","content":[{"type":"text","text":"body"}]}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("localId=exp-001"),
            "should contain localId: {md}"
        );
        assert!(
            md.contains(":::expand{"),
            "should have expand directive with attrs: {md}"
        );
        assert!(
            !md.contains(":::\n{localId="),
            "localId should NOT be trailing: {md}"
        );
    }

    #[test]
    fn expand_localid_roundtrip() {
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"expand","attrs":{"localId":"exp-001","title":"Details"},"content":[
            {"type":"paragraph","content":[{"type":"text","text":"body"}]}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();
        let expand = &rt.content[0];
        assert_eq!(expand.node_type, "expand");
        assert_eq!(
            expand.local_id.as_deref(),
            Some("exp-001"),
            "expand localId should survive round-trip"
        );
        assert_eq!(
            expand.attrs.as_ref().unwrap()["title"],
            "Details",
            "expand title should survive round-trip"
        );
    }

    #[test]
    fn nested_expand_localid_roundtrip() {
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"nestedExpand","attrs":{"localId":"ne-001","title":"S"},"content":[
            {"type":"paragraph","content":[{"type":"text","text":"content"}]}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains(":::nested-expand{"),
            "should have directive: {md}"
        );
        assert!(md.contains("localId=ne-001"), "should have localId: {md}");
        let rt = markdown_to_adf(&md).unwrap();
        let ne = &rt.content[0];
        assert_eq!(ne.node_type, "nestedExpand");
        assert_eq!(ne.local_id.as_deref(), Some("ne-001"));
    }

    #[test]
    fn nested_expand_localid_followed_by_content() {
        // Issue #412 reproducer: localId must not leak into following paragraph
        let adf_json = "{\
            \"version\":1,\"type\":\"doc\",\"content\":[\
              {\"type\":\"nestedExpand\",\"attrs\":{\"localId\":\"exp-001\",\"title\":\"S\"},\"content\":[\
                {\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"\\u00a0\"}]}\
              ]},\
              {\"type\":\"paragraph\",\"content\":[{\"type\":\"text\",\"text\":\"after\"}]}\
            ]}";
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();
        // nestedExpand should have localId
        let ne = &rt.content[0];
        assert_eq!(ne.node_type, "nestedExpand");
        assert_eq!(
            ne.local_id.as_deref(),
            Some("exp-001"),
            "nestedExpand should preserve localId"
        );
        // Following paragraph should contain "after", not "{localId=...}"
        let para = &rt.content[1];
        assert_eq!(para.node_type, "paragraph");
        let text = para.content.as_ref().unwrap()[0]
            .text
            .as_deref()
            .unwrap_or("");
        assert!(
            !text.contains("localId"),
            "following paragraph should not contain localId: {text}"
        );
        assert!(
            text.contains("after"),
            "following paragraph should contain 'after': {text}"
        );
    }

    #[test]
    fn expand_localid_without_title() {
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"expand","attrs":{"localId":"exp-002"},"content":[
            {"type":"paragraph","content":[{"type":"text","text":"no title"}]}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains(":::expand{localId=exp-002}"),
            "should have localId without title: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        assert_eq!(rt.content[0].local_id.as_deref(), Some("exp-002"));
    }

    #[test]
    fn expand_localid_stripped() {
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"expand","attrs":{"localId":"exp-001","title":"X"},"content":[
            {"type":"paragraph","content":[{"type":"text","text":"body"}]}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let opts = RenderOptions {
            strip_local_ids: true,
        };
        let md = adf_to_markdown_with_options(&doc, &opts).unwrap();
        assert!(!md.contains("localId"), "localId should be stripped: {md}");
        assert!(
            md.contains(":::expand{title=\"X\"}"),
            "title should remain: {md}"
        );
    }

    // ── Issue #444: top-level localId and parameters on expand ──

    #[test]
    fn expand_top_level_localid_roundtrip() {
        // localId as a top-level field (not inside attrs) should survive round-trip
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"expand","attrs":{"title":"My Section"},"localId":"abc-123","content":[
            {"type":"paragraph","content":[{"type":"text","text":"hello"}]}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        assert_eq!(doc.content[0].local_id.as_deref(), Some("abc-123"));
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("localId=abc-123"),
            "JFM should contain localId: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        let expand = &rt.content[0];
        assert_eq!(expand.node_type, "expand");
        assert_eq!(expand.local_id.as_deref(), Some("abc-123"));
        assert_eq!(
            expand.attrs.as_ref().unwrap()["title"],
            "My Section",
            "title should survive round-trip"
        );
    }

    #[test]
    fn expand_parameters_roundtrip() {
        // parameters (macroMetadata) should survive round-trip
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"expand","attrs":{"title":"Props"},"parameters":{"macroMetadata":{"macroId":{"value":"m-001"},"schemaVersion":{"value":"1"}}},"content":[
            {"type":"paragraph","content":[{"type":"text","text":"body"}]}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        assert!(doc.content[0].parameters.is_some());
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("params="), "JFM should contain params: {md}");
        let rt = markdown_to_adf(&md).unwrap();
        let expand = &rt.content[0];
        let params = expand
            .parameters
            .as_ref()
            .expect("parameters should survive round-trip");
        assert_eq!(params["macroMetadata"]["macroId"]["value"], "m-001");
        assert_eq!(params["macroMetadata"]["schemaVersion"]["value"], "1");
    }

    #[test]
    fn expand_localid_and_parameters_roundtrip() {
        // Issue #444: both localId and parameters on expand should survive round-trip
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"expand","attrs":{"title":"My Section"},"localId":"abc-123","parameters":{"macroMetadata":{"macroId":{"value":"macro-001"},"schemaVersion":{"value":"1"},"title":"Page Properties"}},"content":[{"type":"paragraph","content":[{"type":"text","text":"hello"}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();
        let expand = &rt.content[0];
        assert_eq!(expand.node_type, "expand");
        assert_eq!(expand.local_id.as_deref(), Some("abc-123"));
        assert_eq!(expand.attrs.as_ref().unwrap()["title"], "My Section");
        let params = expand
            .parameters
            .as_ref()
            .expect("parameters should survive");
        assert_eq!(params["macroMetadata"]["macroId"]["value"], "macro-001");
        assert_eq!(params["macroMetadata"]["title"], "Page Properties");
    }

    #[test]
    fn nested_expand_top_level_localid_and_parameters_roundtrip() {
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"nestedExpand","attrs":{"title":"Nested"},"localId":"ne-100","parameters":{"macroMetadata":{"macroId":{"value":"nm-001"}}},"content":[
            {"type":"paragraph","content":[{"type":"text","text":"inner"}]}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains(":::nested-expand{"),
            "should use nested-expand: {md}"
        );
        assert!(md.contains("localId=ne-100"), "should have localId: {md}");
        assert!(md.contains("params="), "should have params: {md}");
        let rt = markdown_to_adf(&md).unwrap();
        let ne = &rt.content[0];
        assert_eq!(ne.node_type, "nestedExpand");
        assert_eq!(ne.local_id.as_deref(), Some("ne-100"));
        assert_eq!(
            ne.parameters.as_ref().unwrap()["macroMetadata"]["macroId"]["value"],
            "nm-001"
        );
    }

    #[test]
    fn expand_top_level_localid_stripped() {
        // strip_local_ids should strip top-level localId too
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"expand","attrs":{"title":"X"},"localId":"exp-strip","content":[
            {"type":"paragraph","content":[{"type":"text","text":"body"}]}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let opts = RenderOptions {
            strip_local_ids: true,
        };
        let md = adf_to_markdown_with_options(&doc, &opts).unwrap();
        assert!(!md.contains("localId"), "localId should be stripped: {md}");
        assert!(
            md.contains(":::expand{title=\"X\"}"),
            "title should remain: {md}"
        );
    }

    #[test]
    fn expand_parameters_without_localid() {
        // parameters without localId should work
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"expand","attrs":{"title":"P"},"parameters":{"macroMetadata":{"macroId":{"value":"solo"}}},"content":[
            {"type":"paragraph","content":[{"type":"text","text":"data"}]}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(!md.contains("localId"), "no localId: {md}");
        assert!(md.contains("params="), "has params: {md}");
        let rt = markdown_to_adf(&md).unwrap();
        assert!(rt.content[0].local_id.is_none());
        assert_eq!(
            rt.content[0].parameters.as_ref().unwrap()["macroMetadata"]["macroId"]["value"],
            "solo"
        );
    }

    #[test]
    fn expand_localid_without_parameters() {
        // top-level localId without parameters should work
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"expand","attrs":{"title":"L"},"localId":"lid-only","content":[
            {"type":"paragraph","content":[{"type":"text","text":"txt"}]}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("localId=lid-only"), "has localId: {md}");
        assert!(!md.contains("params="), "no params: {md}");
        let rt = markdown_to_adf(&md).unwrap();
        assert_eq!(rt.content[0].local_id.as_deref(), Some("lid-only"));
        assert!(rt.content[0].parameters.is_none());
    }

    #[test]
    fn nested_panel_inside_panel() {
        let md = ":::panel{type=info}\n:::panel{type=warning}\nInner warning\n:::\n:::";
        let adf = markdown_to_adf(md).unwrap();

        // Outer panel should exist
        assert_eq!(adf.content.len(), 1);
        assert_eq!(adf.content[0].node_type, "panel");

        // Outer panel should contain an inner panel (not have it truncated)
        let panel_content = adf.content[0].content.as_ref().unwrap();
        assert!(
            panel_content.iter().any(|n| n.node_type == "panel"),
            "Outer panel should contain an inner panel, got: {:?}",
            panel_content
                .iter()
                .map(|n| &n.node_type)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn content_after_directive_table_is_preserved() {
        // Issue #361: content after a ::::table block was silently dropped
        let md = "\
## Before table

::::table{layout=default}
:::tr
:::th{}
Cell
:::
:::
::::

## After table

Paragraph after.";
        let adf = markdown_to_adf(md).unwrap();
        let types: Vec<&str> = adf.content.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(
            types,
            vec!["heading", "table", "heading", "paragraph"],
            "Content after table was dropped: got {types:?}"
        );
    }

    #[test]
    fn paragraph_after_directive_table_is_preserved() {
        // Issue #361: minimal reproducer — paragraph after table
        let md = "\
::::table{layout=default}
:::tr
:::th{}
Header
:::
:::
::::

Just a paragraph.";
        let adf = markdown_to_adf(md).unwrap();
        let types: Vec<&str> = adf.content.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(
            types,
            vec!["table", "paragraph"],
            "Paragraph after table was dropped: got {types:?}"
        );
    }

    #[test]
    fn extension_after_directive_table_is_preserved() {
        // Issue #361: extension after table
        let md = "\
::::table{layout=default}
:::tr
:::th{}
Header
:::
:::
::::

::extension{type=com.atlassian.confluence.macro.core key=toc}";
        let adf = markdown_to_adf(md).unwrap();
        let types: Vec<&str> = adf.content.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(
            types,
            vec!["table", "extension"],
            "Extension after table was dropped: got {types:?}"
        );
    }

    #[test]
    fn multiple_blocks_after_directive_table() {
        // Issue #361: multiple blocks after table, including another table
        let md = "\
## Heading 1

::::table{layout=default}
:::tr
:::td{}
A
:::
:::td{}
B
:::
:::
::::

## Heading 2

Some text.

---

::::table{layout=default}
:::tr
:::th{}
C
:::
:::
::::

## Heading 3";
        let adf = markdown_to_adf(md).unwrap();
        let types: Vec<&str> = adf.content.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(
            types,
            vec![
                "heading",
                "table",
                "heading",
                "paragraph",
                "rule",
                "table",
                "heading"
            ],
            "Content after tables was dropped: got {types:?}"
        );
    }

    // ── Table caption tests (issue #382) ────────────────────────────

    #[test]
    fn adf_table_caption_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![
                AdfNode::table_row(vec![AdfNode::table_cell(vec![AdfNode::paragraph(vec![
                    AdfNode::text("cell"),
                ])])]),
                AdfNode::caption(vec![AdfNode::text("Table caption")]),
            ])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("::::table"),
            "table with caption must use directive form"
        );
        assert!(
            md.contains(":::caption"),
            "caption directive missing, got: {md}"
        );
        assert!(
            md.contains("Table caption"),
            "caption text missing, got: {md}"
        );
    }

    #[test]
    fn directive_table_caption_parses() {
        let md = "::::table\n:::tr\n:::td\ncell\n:::\n:::\n:::caption\nTable caption\n:::\n::::\n";
        let doc = markdown_to_adf(md).unwrap();
        let table = &doc.content[0];
        assert_eq!(table.node_type, "table");
        let children = table.content.as_ref().unwrap();
        assert_eq!(children.len(), 2, "expected row + caption");
        assert_eq!(children[0].node_type, "tableRow");
        assert_eq!(children[1].node_type, "caption");
        let caption_content = children[1].content.as_ref().unwrap();
        assert_eq!(caption_content[0].text.as_deref(), Some("Table caption"));
    }

    #[test]
    fn table_caption_round_trip_from_adf_json() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","attrs":{"isNumberColumnEnabled":false,"layout":"default"},"content":[
          {"type":"tableRow","content":[{"type":"tableCell","attrs":{},"content":[{"type":"paragraph","content":[{"type":"text","text":"cell"}]}]}]},
          {"type":"caption","content":[{"type":"text","text":"Table caption"}]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("Table caption"), "caption text lost in ADF→JFM");
        let round_tripped = markdown_to_adf(&md).unwrap();
        let children = round_tripped.content[0].content.as_ref().unwrap();
        let caption = children.iter().find(|n| n.node_type == "caption");
        assert!(caption.is_some(), "caption lost on round-trip");
        let caption_text = caption.unwrap().content.as_ref().unwrap();
        assert_eq!(caption_text[0].text.as_deref(), Some("Table caption"));
    }

    #[test]
    fn table_caption_with_inline_marks_round_trips() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![
                AdfNode::table_row(vec![AdfNode::table_cell(vec![AdfNode::paragraph(vec![
                    AdfNode::text("data"),
                ])])]),
                AdfNode::caption(vec![
                    AdfNode::text("Caption with "),
                    AdfNode::text_with_marks("bold", vec![AdfMark::strong()]),
                ]),
            ])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("**bold**"), "bold mark missing in caption");
        let round_tripped = markdown_to_adf(&md).unwrap();
        let caption = round_tripped.content[0]
            .content
            .as_ref()
            .unwrap()
            .iter()
            .find(|n| n.node_type == "caption")
            .expect("caption node missing after round-trip");
        let inlines = caption.content.as_ref().unwrap();
        let bold_node = inlines.iter().find(|n| {
            n.marks
                .as_ref()
                .is_some_and(|m| m.iter().any(|mk| mk.mark_type == "strong"))
        });
        assert!(bold_node.is_some(), "bold mark lost in caption round-trip");
    }

    #[test]
    #[test]
    fn tablecell_empty_attrs_preserved_on_roundtrip() {
        // Issue #385: tableCell with empty attrs:{} dropped on round-trip
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","content":[{"type":"tableRow","content":[{"type":"tableCell","attrs":{},"content":[{"type":"paragraph","content":[{"type":"text","text":"hello"}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let rows = round_tripped.content[0].content.as_ref().unwrap();
        let cell = &rows[0].content.as_ref().unwrap()[0];
        assert!(
            cell.attrs.is_some(),
            "tableCell attrs should be preserved, got None"
        );
        assert_eq!(
            cell.attrs.as_ref().unwrap(),
            &serde_json::json!({}),
            "tableCell attrs should be an empty object"
        );
    }

    #[test]
    fn tablecell_empty_attrs_serialized_in_json() {
        // Issue #385: ensure the serialized JSON includes "attrs":{}
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","content":[{"type":"tableRow","content":[{"type":"tableCell","attrs":{},"content":[{"type":"paragraph","content":[{"type":"text","text":"hello"}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let json = serde_json::to_string(&round_tripped).unwrap();
        assert!(
            json.contains(r#""attrs":{}"#),
            "serialized JSON should contain \"attrs\":{{}}, got: {json}"
        );
    }

    #[test]
    fn tablecell_empty_attrs_renders_braces_in_markdown() {
        // Issue #385: tableCell with empty attrs should render {} prefix in pipe tables
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","content":[{"type":"tableRow","content":[{"type":"tableHeader","content":[{"type":"paragraph","content":[{"type":"text","text":"H"}]}]},{"type":"tableHeader","content":[{"type":"paragraph","content":[{"type":"text","text":"H2"}]}]}]},{"type":"tableRow","content":[{"type":"tableCell","attrs":{},"content":[{"type":"paragraph","content":[{"type":"text","text":"hello"}]}]},{"type":"tableCell","content":[{"type":"paragraph","content":[{"type":"text","text":"world"}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        // Cell with attrs:{} should have {} prefix, cell without attrs should not
        assert!(
            md.contains("{} hello"),
            "cell with empty attrs should render '{{}} hello', got: {md}"
        );
        assert!(
            !md.contains("{} world"),
            "cell without attrs should not render '{{}}', got: {md}"
        );
    }

    #[test]
    fn tablecell_no_attrs_unchanged_on_roundtrip() {
        // Ensure tableCell without attrs stays without attrs
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","content":[{"type":"tableRow","content":[{"type":"tableCell","content":[{"type":"paragraph","content":[{"type":"text","text":"hello"}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let rows = round_tripped.content[0].content.as_ref().unwrap();
        let cell = &rows[0].content.as_ref().unwrap()[0];
        assert!(
            cell.attrs.is_none(),
            "tableCell without attrs should stay None, got: {:?}",
            cell.attrs
        );
    }

    #[test]
    fn tablecell_nonempty_attrs_preserved_on_roundtrip() {
        // Ensure tableCell with non-empty attrs still works
        let adf_json = r##"{"version":1,"type":"doc","content":[{"type":"table","content":[{"type":"tableRow","content":[{"type":"tableHeader","content":[{"type":"paragraph","content":[{"type":"text","text":"H"}]}]}]},{"type":"tableRow","content":[{"type":"tableCell","attrs":{"background":"#DEEBFF","colspan":2},"content":[{"type":"paragraph","content":[{"type":"text","text":"highlighted"}]}]}]}]}]}"##;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let round_tripped = markdown_to_adf(&md).unwrap();
        let rows = round_tripped.content[0].content.as_ref().unwrap();
        let cell = &rows[1].content.as_ref().unwrap()[0];
        let attrs = cell.attrs.as_ref().unwrap();
        assert_eq!(attrs["background"], "#DEEBFF");
        assert_eq!(attrs["colspan"], 2);
    }

    #[test]
    fn pipe_table_not_used_when_caption_present() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![
                AdfNode::table_row(vec![AdfNode::table_header(vec![AdfNode::paragraph(vec![
                    AdfNode::text("H"),
                ])])]),
                AdfNode::table_row(vec![AdfNode::table_cell(vec![AdfNode::paragraph(vec![
                    AdfNode::text("D"),
                ])])]),
                AdfNode::caption(vec![AdfNode::text("cap")]),
            ])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("::::table"),
            "pipe syntax should not be used when caption is present"
        );
    }

    // ── Issue #402: ordered-list-like text in list item hardBreak ──

    #[test]
    fn hardbreak_with_ordered_marker_in_bullet_item_roundtrips() {
        // Issue #402: text starting with "2. " after a hardBreak inside a
        // bullet list item must not be re-parsed as a new ordered list.
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[
          {"type":"listItem","content":[{"type":"paragraph","content":[
            {"type":"text","text":"1. First item"},
            {"type":"hardBreak"},
            {"type":"text","text":"2. Honouring existing commitments"}
          ]}]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();

        // The continuation line must be indented so it stays within the list item.
        assert!(
            md.contains("  2. Honouring"),
            "Continuation line should be indented, got:\n{md}"
        );

        // Round-trip back to ADF
        let rt = markdown_to_adf(&md).unwrap();
        let list = &rt.content[0];
        assert_eq!(list.node_type, "bulletList");
        let items = list.content.as_ref().unwrap();
        assert_eq!(
            items.len(),
            1,
            "Should be one list item, got {}",
            items.len()
        );

        let para = &items[0].content.as_ref().unwrap()[0];
        let inlines = para.content.as_ref().unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(
            types,
            vec!["text", "hardBreak", "text"],
            "Expected text+hardBreak+text, got {types:?}"
        );
        assert_eq!(
            inlines[2].text.as_deref().unwrap(),
            "2. Honouring existing commitments"
        );
    }

    #[test]
    fn hardbreak_with_ordered_marker_in_ordered_item_roundtrips() {
        // Same as above but inside an ordered list.
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"orderedList","attrs":{"order":1},"content":[
          {"type":"listItem","content":[{"type":"paragraph","content":[
            {"type":"text","text":"Introduction  "},
            {"type":"hardBreak"},
            {"type":"text","text":"3. Third point"}
          ]}]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        let list = &rt.content[0];
        assert_eq!(list.node_type, "orderedList");
        let items = list.content.as_ref().unwrap();
        assert_eq!(items.len(), 1);

        let para = &items[0].content.as_ref().unwrap()[0];
        let inlines = para.content.as_ref().unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types, vec!["text", "hardBreak", "text"]);
        assert_eq!(inlines[2].text.as_deref().unwrap(), "3. Third point");
    }

    #[test]
    fn hardbreak_with_bullet_marker_in_bullet_item_roundtrips() {
        // Text starting with "- " after a hardBreak must not become a nested bullet list.
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[
          {"type":"listItem","content":[{"type":"paragraph","content":[
            {"type":"text","text":"Header  "},
            {"type":"hardBreak"},
            {"type":"text","text":"- not a sub-item"}
          ]}]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        let list = &rt.content[0];
        assert_eq!(list.node_type, "bulletList");
        let items = list.content.as_ref().unwrap();
        assert_eq!(
            items.len(),
            1,
            "Should be one list item, not {}",
            items.len()
        );

        let para = &items[0].content.as_ref().unwrap()[0];
        let inlines = para.content.as_ref().unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types, vec!["text", "hardBreak", "text"]);
        assert_eq!(inlines[2].text.as_deref().unwrap(), "- not a sub-item");
    }

    #[test]
    fn hardbreak_continuation_followed_by_sub_list() {
        // A hardBreak continuation line followed by a real sub-list.
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[
          {"type":"listItem","content":[
            {"type":"paragraph","content":[
              {"type":"text","text":"Main item  "},
              {"type":"hardBreak"},
              {"type":"text","text":"continued here"}
            ]},
            {"type":"bulletList","content":[
              {"type":"listItem","content":[{"type":"paragraph","content":[
                {"type":"text","text":"sub-item"}
              ]}]}
            ]}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        let list = &rt.content[0];
        let items = list.content.as_ref().unwrap();
        assert_eq!(items.len(), 1);

        let item_content = items[0].content.as_ref().unwrap();
        assert_eq!(item_content.len(), 2, "Expected paragraph + nested list");
        assert_eq!(item_content[0].node_type, "paragraph");
        assert_eq!(item_content[1].node_type, "bulletList");

        // Check the paragraph has hardBreak
        let inlines = item_content[0].content.as_ref().unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types, vec!["text", "hardBreak", "text"]);
    }

    #[test]
    fn multiple_hardbreaks_with_numbered_text_roundtrip() {
        // Multiple hardBreaks where each continuation resembles an ordered list.
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[
          {"type":"listItem","content":[{"type":"paragraph","content":[
            {"type":"text","text":"Preamble  "},
            {"type":"hardBreak"},
            {"type":"text","text":"1. Alpha  "},
            {"type":"hardBreak"},
            {"type":"text","text":"2. Bravo"}
          ]}]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        let items = rt.content[0].content.as_ref().unwrap();
        assert_eq!(items.len(), 1);

        let inlines = items[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(
            types,
            vec!["text", "hardBreak", "text", "hardBreak", "text"]
        );
    }

    #[test]
    fn trailing_hardbreak_in_bullet_item_roundtrips() {
        // A hardBreak as the last inline node with no text after it.
        // Exercises the `break` path in the continuation loop and the
        // empty-line rendering branch.
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[
          {"type":"listItem","content":[{"type":"paragraph","content":[
            {"type":"text","text":"ends with break"},
            {"type":"hardBreak"}
          ]}]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        let list = &rt.content[0];
        assert_eq!(list.node_type, "bulletList");
        let inlines = list.content.as_ref().unwrap()[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types, vec!["text", "hardBreak"]);
    }

    #[test]
    fn trailing_hardbreak_in_ordered_item_roundtrips() {
        // Same as above but in an ordered list, covering the ordered-list
        // continuation `break` path.
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"orderedList","attrs":{"order":1},"content":[
          {"type":"listItem","content":[{"type":"paragraph","content":[
            {"type":"text","text":"ends with break"},
            {"type":"hardBreak"}
          ]}]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        let list = &rt.content[0];
        assert_eq!(list.node_type, "orderedList");
        let inlines = list.content.as_ref().unwrap()[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types, vec!["text", "hardBreak"]);
    }

    #[test]
    fn trailing_space_hardbreak_continuation_in_bullet_item() {
        // Exercises the `ends_with("  ")` path in `has_trailing_hard_break`
        // by parsing hand-written markdown that uses trailing-space style
        // hardBreaks instead of backslash style.
        let md = "- first line  \n  2. continued\n";
        let doc = markdown_to_adf(md).unwrap();

        let list = &doc.content[0];
        assert_eq!(list.node_type, "bulletList");
        let items = list.content.as_ref().unwrap();
        assert_eq!(
            items.len(),
            1,
            "Should be one list item, got {}",
            items.len()
        );

        let para = &items[0].content.as_ref().unwrap()[0];
        let inlines = para.content.as_ref().unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types, vec!["text", "hardBreak", "text"]);
        assert_eq!(inlines[2].text.as_deref().unwrap(), "2. continued");
    }

    #[test]
    fn trailing_space_hardbreak_continuation_in_ordered_item() {
        // Same as above but for ordered list, exercising the trailing-space
        // path in the ordered-list continuation loop.
        let md = "1. first line  \n  - continued\n";
        let doc = markdown_to_adf(md).unwrap();

        let list = &doc.content[0];
        assert_eq!(list.node_type, "orderedList");
        let items = list.content.as_ref().unwrap();
        assert_eq!(
            items.len(),
            1,
            "Should be one list item, got {}",
            items.len()
        );

        let para = &items[0].content.as_ref().unwrap()[0];
        let inlines = para.content.as_ref().unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types, vec!["text", "hardBreak", "text"]);
        assert_eq!(inlines[2].text.as_deref().unwrap(), "- continued");
    }

    #[test]
    fn multi_paragraph_list_item_with_ordered_marker_roundtrips() {
        // Issue #402 comment: a listItem with a second paragraph starting
        // with "2. " must not become a separate orderedList.
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[
          {"type":"listItem","content":[
            {"type":"paragraph","content":[{"type":"text","text":"some preamble"}]},
            {"type":"paragraph","content":[{"type":"text","text":"2. Honouring existing commitments"}]}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        assert_eq!(rt.content.len(), 1, "Should be one top-level block");
        let list = &rt.content[0];
        assert_eq!(list.node_type, "bulletList");
        let items = list.content.as_ref().unwrap();
        assert_eq!(items.len(), 1);
        let item_content = items[0].content.as_ref().unwrap();
        assert_eq!(
            item_content.len(),
            2,
            "Expected 2 paragraphs inside the list item, got {}",
            item_content.len()
        );
        assert_eq!(item_content[0].node_type, "paragraph");
        assert_eq!(item_content[1].node_type, "paragraph");
        let text = item_content[1].content.as_ref().unwrap()[0]
            .text
            .as_deref()
            .unwrap();
        assert_eq!(text, "2. Honouring existing commitments");
    }

    #[test]
    fn multi_paragraph_list_item_with_bullet_marker_roundtrips() {
        // Paragraph starting with "- " inside a list item.
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[
          {"type":"listItem","content":[
            {"type":"paragraph","content":[{"type":"text","text":"preamble"}]},
            {"type":"paragraph","content":[{"type":"text","text":"- not a sub-item"}]}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        let items = rt.content[0].content.as_ref().unwrap();
        assert_eq!(items.len(), 1);
        let item_content = items[0].content.as_ref().unwrap();
        assert_eq!(item_content.len(), 2);
        assert_eq!(item_content[1].node_type, "paragraph");
        let text = item_content[1].content.as_ref().unwrap()[0]
            .text
            .as_deref()
            .unwrap();
        assert_eq!(text, "- not a sub-item");
    }

    #[test]
    fn backslash_escape_in_inline_text() {
        // Verify that `\. ` is unescaped to `. ` in inline parsing.
        let nodes = parse_inline(r"2\. text");
        assert_eq!(nodes.len(), 1, "Should be one text node");
        assert_eq!(nodes[0].text.as_deref().unwrap(), "2. text");
    }

    #[test]
    fn escape_list_marker_ordered() {
        assert_eq!(escape_list_marker("2. text"), r"2\. text");
        assert_eq!(escape_list_marker("10. tenth"), r"10\. tenth");
    }

    #[test]
    fn escape_list_marker_bullet() {
        assert_eq!(escape_list_marker("- text"), r"\- text");
        assert_eq!(escape_list_marker("* text"), r"\* text");
        assert_eq!(escape_list_marker("+ text"), r"\+ text");
    }

    #[test]
    fn escape_list_marker_plain() {
        assert_eq!(escape_list_marker("plain text"), "plain text");
        assert_eq!(escape_list_marker("no. marker"), "no. marker");
    }

    #[test]
    fn merge_adjacent_text_nodes() {
        let mut nodes = vec![AdfNode::text("a"), AdfNode::text("b"), AdfNode::text("c")];
        merge_adjacent_text(&mut nodes);
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].text.as_deref().unwrap(), "abc");
    }

    #[test]
    fn ordered_marker_paragraph_in_table_cell_roundtrips() {
        // Issue #402: paragraph with "2. " text inside a tableCell must
        // not be re-parsed as an ordered list.
        let adf_json = r#"{"version":1,"type":"doc","content":[{
          "type":"table","attrs":{"isNumberColumnEnabled":false,"layout":"default"},
          "content":[{"type":"tableRow","content":[{
            "type":"tableCell","attrs":{"colspan":1,"rowspan":1},
            "content":[{"type":"paragraph","content":[
              {"type":"text","text":"2. Honouring existing commitments"}
            ]}]
          }]}]
        }]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        let table = &rt.content[0];
        let cell = &table.content.as_ref().unwrap()[0].content.as_ref().unwrap()[0];
        let para = &cell.content.as_ref().unwrap()[0];
        assert_eq!(para.node_type, "paragraph");
        let text = para.content.as_ref().unwrap()[0].text.as_deref().unwrap();
        assert_eq!(text, "2. Honouring existing commitments");
    }

    #[test]
    fn bullet_marker_paragraph_standalone_roundtrips() {
        // A top-level paragraph starting with "- " must round-trip as
        // a paragraph, not a bullet list.
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"paragraph","content":[
            {"type":"text","text":"- not a list item"}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains(r"\- not a list item"),
            "Should escape the leading dash, got:\n{md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        assert_eq!(rt.content[0].node_type, "paragraph");
        let text = rt.content[0].content.as_ref().unwrap()[0]
            .text
            .as_deref()
            .unwrap();
        assert_eq!(text, "- not a list item");
    }

    #[test]
    fn merge_adjacent_text_skips_non_text_nodes() {
        // Exercises the `else { i += 1 }` branch when adjacent nodes
        // are not both plain text.
        let mut nodes = vec![
            AdfNode::text("a"),
            AdfNode::hard_break(),
            AdfNode::text("b"),
        ];
        merge_adjacent_text(&mut nodes);
        assert_eq!(nodes.len(), 3);
    }

    #[test]
    fn star_bullet_paragraph_roundtrips() {
        // Paragraph starting with "* " must round-trip without becoming
        // a bullet list.
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"paragraph","content":[
            {"type":"text","text":"* starred"}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();
        assert_eq!(rt.content[0].node_type, "paragraph");
        assert_eq!(
            rt.content[0].content.as_ref().unwrap()[0]
                .text
                .as_deref()
                .unwrap(),
            "* starred"
        );
    }

    // ---- Issue #388 tests ----

    #[test]
    fn issue_388_ordered_list_with_strong_hardbreak_roundtrips() {
        // Issue #388: orderedList with 2 listItems, each containing
        // strong-marked text + hardBreak + plain text.
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"orderedList","attrs":{"order":1},"content":[
            {"type":"listItem","content":[
              {"type":"paragraph","content":[
                {"type":"text","text":"Bold heading","marks":[{"type":"strong"}]},
                {"type":"hardBreak"},
                {"type":"text","text":"Content after break"}
              ]}
            ]},
            {"type":"listItem","content":[
              {"type":"paragraph","content":[
                {"type":"text","text":"Second item","marks":[{"type":"strong"}]},
                {"type":"hardBreak"},
                {"type":"text","text":"More content"}
              ]}
            ]}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        // Must remain a single orderedList
        assert_eq!(
            rt.content.len(),
            1,
            "Should be 1 block (orderedList), got {}",
            rt.content.len()
        );
        assert_eq!(rt.content[0].node_type, "orderedList");
        let items = rt.content[0].content.as_ref().unwrap();
        assert_eq!(
            items.len(),
            2,
            "Should have 2 listItems, got {}",
            items.len()
        );

        // First item: text(strong) + hardBreak + text
        let p1 = items[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap();
        let types1: Vec<&str> = p1.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types1, vec!["text", "hardBreak", "text"]);
        assert_eq!(p1[0].text.as_deref(), Some("Bold heading"));
        assert_eq!(p1[2].text.as_deref(), Some("Content after break"));

        // Second item: text(strong) + hardBreak + text
        let p2 = items[1].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap();
        let types2: Vec<&str> = p2.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types2, vec!["text", "hardBreak", "text"]);
        assert_eq!(p2[0].text.as_deref(), Some("Second item"));
        assert_eq!(p2[2].text.as_deref(), Some("More content"));
    }

    #[test]
    fn issue_388_bullet_list_with_strong_hardbreak_roundtrips() {
        // Bullet list variant of issue #388.
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"bulletList","content":[
            {"type":"listItem","content":[
              {"type":"paragraph","content":[
                {"type":"text","text":"First","marks":[{"type":"strong"}]},
                {"type":"hardBreak"},
                {"type":"text","text":"details"}
              ]}
            ]},
            {"type":"listItem","content":[
              {"type":"paragraph","content":[
                {"type":"text","text":"Second","marks":[{"type":"em"}]},
                {"type":"hardBreak"},
                {"type":"text","text":"more details"}
              ]}
            ]}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        assert_eq!(rt.content.len(), 1);
        assert_eq!(rt.content[0].node_type, "bulletList");
        let items = rt.content[0].content.as_ref().unwrap();
        assert_eq!(items.len(), 2);

        let p1 = items[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap();
        assert_eq!(p1[0].text.as_deref(), Some("First"));
        assert_eq!(p1[2].text.as_deref(), Some("details"));

        let p2 = items[1].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap();
        assert_eq!(p2[0].text.as_deref(), Some("Second"));
        assert_eq!(p2[2].text.as_deref(), Some("more details"));
    }

    #[test]
    fn issue_388_ordered_list_hardbreak_jfm_indentation() {
        // Verify the JFM output has properly indented continuation lines.
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"orderedList","attrs":{"order":1},"content":[
            {"type":"listItem","content":[
              {"type":"paragraph","content":[
                {"type":"text","text":"heading","marks":[{"type":"strong"}]},
                {"type":"hardBreak"},
                {"type":"text","text":"body"}
              ]}
            ]}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("1. **heading**\\\n  body"),
            "Continuation should be indented, got:\n{md}"
        );
    }

    #[test]
    fn issue_388_ordered_list_hardbreak_from_jfm() {
        // Direct JFM → ADF: ordered list with hardBreak continuation.
        let md = "1. **bold**\\\n  continued\n2. **also bold**\\\n  also continued\n";
        let doc = markdown_to_adf(md).unwrap();

        assert_eq!(doc.content.len(), 1);
        assert_eq!(doc.content[0].node_type, "orderedList");
        let items = doc.content[0].content.as_ref().unwrap();
        assert_eq!(items.len(), 2);

        let p1 = items[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap();
        let types1: Vec<&str> = p1.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types1, vec!["text", "hardBreak", "text"]);
        assert_eq!(p1[0].text.as_deref(), Some("bold"));
        assert_eq!(p1[2].text.as_deref(), Some("continued"));

        let p2 = items[1].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap();
        let types2: Vec<&str> = p2.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types2, vec!["text", "hardBreak", "text"]);
    }

    #[test]
    fn issue_388_bullet_list_hardbreak_from_jfm() {
        // Direct JFM → ADF: bullet list with hardBreak continuation.
        let md = "- first\\\n  second\n- third\\\n  fourth\n";
        let doc = markdown_to_adf(md).unwrap();

        assert_eq!(doc.content.len(), 1);
        assert_eq!(doc.content[0].node_type, "bulletList");
        let items = doc.content[0].content.as_ref().unwrap();
        assert_eq!(items.len(), 2);

        for (i, expected) in [("first", "second"), ("third", "fourth")]
            .iter()
            .enumerate()
        {
            let p = items[i].content.as_ref().unwrap()[0]
                .content
                .as_ref()
                .unwrap();
            let types: Vec<&str> = p.iter().map(|n| n.node_type.as_str()).collect();
            assert_eq!(types, vec!["text", "hardBreak", "text"]);
            assert_eq!(p[0].text.as_deref(), Some(expected.0));
            assert_eq!(p[2].text.as_deref(), Some(expected.1));
        }
    }

    #[test]
    fn issue_433_heading_hardbreak_roundtrips() {
        // Issue #433: hardBreak inside heading splits into heading + paragraph.
        let adf_json = r#"{"version":1,"type":"doc","content":[{
          "type":"heading",
          "attrs":{"level":1},
          "content":[
            {"type":"text","text":"Line one"},
            {"type":"hardBreak"},
            {"type":"text","text":"Line two"}
          ]
        }]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        assert_eq!(
            rt.content.len(),
            1,
            "Should remain a single heading, got {} blocks",
            rt.content.len()
        );
        assert_eq!(rt.content[0].node_type, "heading");
        let inlines = rt.content[0].content.as_ref().unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(
            types,
            vec!["text", "hardBreak", "text"],
            "hardBreak should be preserved, got: {types:?}"
        );
        assert_eq!(inlines[0].text.as_deref(), Some("Line one"));
        assert_eq!(inlines[2].text.as_deref(), Some("Line two"));
    }

    #[test]
    fn issue_433_heading_hardbreak_jfm_indentation() {
        // Verify the JFM output has properly indented continuation lines.
        let adf_json = r#"{"version":1,"type":"doc","content":[{
          "type":"heading",
          "attrs":{"level":2},
          "content":[
            {"type":"text","text":"Title"},
            {"type":"hardBreak"},
            {"type":"text","text":"Subtitle"}
          ]
        }]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("## Title\\\n  Subtitle"),
            "Continuation should be indented, got:\n{md}"
        );
    }

    #[test]
    fn issue_433_heading_hardbreak_from_jfm() {
        // Direct JFM → ADF: heading with hardBreak continuation.
        let md = "# First\\\n  Second\n";
        let doc = markdown_to_adf(md).unwrap();

        assert_eq!(doc.content.len(), 1);
        assert_eq!(doc.content[0].node_type, "heading");
        let inlines = doc.content[0].content.as_ref().unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types, vec!["text", "hardBreak", "text"]);
        assert_eq!(inlines[0].text.as_deref(), Some("First"));
        assert_eq!(inlines[2].text.as_deref(), Some("Second"));
    }

    #[test]
    fn issue_433_heading_consecutive_hardbreaks_roundtrip() {
        // Consecutive hardBreaks in a heading.
        let adf_json = r#"{"version":1,"type":"doc","content":[{
          "type":"heading",
          "attrs":{"level":3},
          "content":[
            {"type":"text","text":"A"},
            {"type":"hardBreak"},
            {"type":"hardBreak"},
            {"type":"text","text":"B"}
          ]
        }]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        assert_eq!(rt.content.len(), 1, "Should remain a single heading");
        assert_eq!(rt.content[0].node_type, "heading");
        let inlines = rt.content[0].content.as_ref().unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types, vec!["text", "hardBreak", "hardBreak", "text"]);
    }

    #[test]
    fn issue_433_heading_with_strong_and_hardbreak_roundtrips() {
        // Heading with strong-marked text + hardBreak + plain text.
        let adf_json = r#"{"version":1,"type":"doc","content":[{
          "type":"heading",
          "attrs":{"level":1},
          "content":[
            {"type":"text","text":"Bold title","marks":[{"type":"strong"}]},
            {"type":"hardBreak"},
            {"type":"text","text":"plain continuation"}
          ]
        }]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        assert_eq!(rt.content.len(), 1);
        assert_eq!(rt.content[0].node_type, "heading");
        let inlines = rt.content[0].content.as_ref().unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types, vec!["text", "hardBreak", "text"]);
        assert_eq!(inlines[0].text.as_deref(), Some("Bold title"));
        assert_eq!(inlines[2].text.as_deref(), Some("plain continuation"));
    }

    #[test]
    fn issue_433_heading_with_link_and_hardbreak_roundtrips() {
        // Real-world pattern: heading with link + hardBreak + text.
        let adf_json = r#"{"version":1,"type":"doc","content":[{
          "type":"heading",
          "attrs":{"level":1},
          "content":[
            {"type":"text","text":"Click here","marks":[{"type":"link","attrs":{"href":"https://example.com"}}]},
            {"type":"hardBreak"},
            {"type":"text","text":"Subtitle text"}
          ]
        }]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        assert_eq!(rt.content.len(), 1);
        assert_eq!(rt.content[0].node_type, "heading");
        let inlines = rt.content[0].content.as_ref().unwrap();
        let types: Vec<&str> = inlines.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types, vec!["text", "hardBreak", "text"]);
        assert_eq!(inlines[2].text.as_deref(), Some("Subtitle text"));
    }

    #[test]
    fn has_trailing_hard_break_backslash() {
        assert!(has_trailing_hard_break("text\\"));
        assert!(has_trailing_hard_break("**bold**\\"));
    }

    #[test]
    fn has_trailing_hard_break_trailing_spaces() {
        assert!(has_trailing_hard_break("text  "));
        assert!(has_trailing_hard_break("word   "));
    }

    #[test]
    fn has_trailing_hard_break_false() {
        assert!(!has_trailing_hard_break("plain text"));
        assert!(!has_trailing_hard_break("text "));
        assert!(!has_trailing_hard_break(""));
    }

    #[test]
    fn collect_hardbreak_continuations_collects_indented() {
        // A line ending with `\` followed by 2-space-indented continuation.
        // Only one line is collected because the result no longer ends with `\`.
        let input = "first\\\n  second\n  third\n";
        let mut parser = MarkdownParser::new(input);
        parser.advance(); // skip first line
        let mut text = "first\\".to_string();
        parser.collect_hardbreak_continuations(&mut text);
        assert_eq!(text, "first\\\nsecond");
    }

    #[test]
    fn collect_hardbreak_continuations_stops_at_non_indented() {
        let input = "first\\\nnot indented\n";
        let mut parser = MarkdownParser::new(input);
        parser.advance();
        let mut text = "first\\".to_string();
        parser.collect_hardbreak_continuations(&mut text);
        // Should NOT collect the non-indented line
        assert_eq!(text, "first\\");
    }

    #[test]
    fn collect_hardbreak_continuations_no_trailing_break() {
        // If the text doesn't end with a hardBreak marker, nothing is collected.
        let input = "plain\n  indented\n";
        let mut parser = MarkdownParser::new(input);
        parser.advance();
        let mut text = "plain".to_string();
        parser.collect_hardbreak_continuations(&mut text);
        assert_eq!(text, "plain");
    }

    #[test]
    fn collect_hardbreak_continuations_chained() {
        // Multiple continuation lines chained via repeated hardBreaks.
        let input = "a\\\n  b\\\n  c\\\n  d\n";
        let mut parser = MarkdownParser::new(input);
        parser.advance();
        let mut text = "a\\".to_string();
        parser.collect_hardbreak_continuations(&mut text);
        assert_eq!(text, "a\\\nb\\\nc\\\nd");
    }

    #[test]
    fn ordered_list_with_sub_content_after_hardbreak() {
        // Exercises the sub-content collection loop in parse_ordered_list
        // (lines 339-347) with a hardBreak item that also has a nested list.
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"orderedList","attrs":{"order":1},"content":[
            {"type":"listItem","content":[
              {"type":"paragraph","content":[
                {"type":"text","text":"parent"},
                {"type":"hardBreak"},
                {"type":"text","text":"continued"}
              ]},
              {"type":"bulletList","content":[
                {"type":"listItem","content":[
                  {"type":"paragraph","content":[
                    {"type":"text","text":"child"}
                  ]}
                ]}
              ]}
            ]}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        assert_eq!(rt.content.len(), 1);
        assert_eq!(rt.content[0].node_type, "orderedList");
        let item_content = rt.content[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap();
        // Paragraph with hardBreak
        let p = item_content[0].content.as_ref().unwrap();
        let types: Vec<&str> = p.iter().map(|n| n.node_type.as_str()).collect();
        assert_eq!(types, vec!["text", "hardBreak", "text"]);
        assert_eq!(p[0].text.as_deref(), Some("parent"));
        assert_eq!(p[2].text.as_deref(), Some("continued"));
        // Nested bullet list
        assert_eq!(item_content[1].node_type, "bulletList");
    }

    #[test]
    fn render_list_item_content_no_content() {
        // A listItem with content: None should produce just a newline.
        let item = AdfNode {
            node_type: "listItem".to_string(),
            attrs: None,
            content: None,
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        };
        let mut output = String::new();
        let opts = RenderOptions::default();
        render_list_item_content(&item, &mut output, &opts);
        assert_eq!(output, "\n");
    }

    #[test]
    fn render_list_item_content_empty_content() {
        // A listItem with content: Some(vec![]) should produce just a newline.
        let item = AdfNode::list_item(vec![]);
        let mut output = String::new();
        let opts = RenderOptions::default();
        render_list_item_content(&item, &mut output, &opts);
        assert_eq!(output, "\n");
    }

    #[test]
    fn plus_bullet_paragraph_roundtrips() {
        // Paragraph starting with "+ " must round-trip without becoming
        // a bullet list.
        let adf_json = r#"{"version":1,"type":"doc","content":[
          {"type":"paragraph","content":[
            {"type":"text","text":"+ plus"}
          ]}
        ]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();
        assert_eq!(rt.content[0].node_type, "paragraph");
        assert_eq!(
            rt.content[0].content.as_ref().unwrap()[0]
                .text
                .as_deref()
                .unwrap(),
            "+ plus"
        );
    }

    // ---- Issue #430 tests: mediaSingle inside listItem ----

    #[test]
    fn issue_430_file_media_in_bullet_list_roundtrip() {
        // Issue #430: mediaSingle (type:file) as direct child of listItem
        // in a bulletList must survive round-trip.
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[
          {"type":"listItem","content":[{
            "type":"mediaSingle",
            "attrs":{"layout":"center","width":1009,"widthType":"pixel"},
            "content":[{
              "type":"media",
              "attrs":{"collection":"contentId-123","height":576,"id":"00066e8e-554e-4d7e-af59-a0ef2888bdb6","type":"file","width":1009}
            }]
          }]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        let list = &rt.content[0];
        assert_eq!(list.node_type, "bulletList");
        let item = &list.content.as_ref().unwrap()[0];
        assert_eq!(item.node_type, "listItem");
        let ms = &item.content.as_ref().unwrap()[0];
        assert_eq!(ms.node_type, "mediaSingle");
        let ms_attrs = ms.attrs.as_ref().unwrap();
        assert_eq!(ms_attrs["layout"], "center");
        assert_eq!(ms_attrs["width"], 1009);
        assert_eq!(ms_attrs["widthType"], "pixel");
        let media = &ms.content.as_ref().unwrap()[0];
        assert_eq!(media.node_type, "media");
        let m_attrs = media.attrs.as_ref().unwrap();
        assert_eq!(m_attrs["type"], "file");
        assert_eq!(m_attrs["id"], "00066e8e-554e-4d7e-af59-a0ef2888bdb6");
        assert_eq!(m_attrs["collection"], "contentId-123");
        assert_eq!(m_attrs["height"], 576);
        assert_eq!(m_attrs["width"], 1009);
    }

    #[test]
    fn issue_430_file_media_in_ordered_list_roundtrip() {
        // Same as above but inside an orderedList.
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"orderedList","attrs":{"order":1},"content":[
          {"type":"listItem","content":[{
            "type":"mediaSingle",
            "attrs":{"layout":"center"},
            "content":[{
              "type":"media",
              "attrs":{"type":"file","id":"abc-123","collection":"contentId-456","height":100,"width":200}
            }]
          }]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        let list = &rt.content[0];
        assert_eq!(list.node_type, "orderedList");
        let item = &list.content.as_ref().unwrap()[0];
        assert_eq!(item.node_type, "listItem");
        let ms = &item.content.as_ref().unwrap()[0];
        assert_eq!(ms.node_type, "mediaSingle");
        let media = &ms.content.as_ref().unwrap()[0];
        assert_eq!(media.node_type, "media");
        let m_attrs = media.attrs.as_ref().unwrap();
        assert_eq!(m_attrs["type"], "file");
        assert_eq!(m_attrs["id"], "abc-123");
        assert_eq!(m_attrs["collection"], "contentId-456");
    }

    #[test]
    fn issue_430_external_media_in_bullet_list_roundtrip() {
        // External image (type:external) inside a bullet list item.
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[
          {"type":"listItem","content":[{
            "type":"mediaSingle",
            "attrs":{"layout":"center"},
            "content":[{
              "type":"media",
              "attrs":{"type":"external","url":"https://example.com/img.png","alt":"Photo"}
            }]
          }]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        let list = &rt.content[0];
        assert_eq!(list.node_type, "bulletList");
        let item = &list.content.as_ref().unwrap()[0];
        let ms = &item.content.as_ref().unwrap()[0];
        assert_eq!(ms.node_type, "mediaSingle");
        let media = &ms.content.as_ref().unwrap()[0];
        assert_eq!(media.node_type, "media");
        let m_attrs = media.attrs.as_ref().unwrap();
        assert_eq!(m_attrs["type"], "external");
        assert_eq!(m_attrs["url"], "https://example.com/img.png");
    }

    #[test]
    fn issue_430_media_with_paragraph_siblings_in_list_item() {
        // listItem containing a paragraph followed by a mediaSingle.
        // Both children must survive round-trip.
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[
          {"type":"listItem","content":[
            {"type":"paragraph","content":[{"type":"text","text":"Caption:"}]},
            {"type":"mediaSingle","attrs":{"layout":"center"},
             "content":[{"type":"media","attrs":{"type":"file","id":"img-001","collection":"col-1","height":50,"width":100}}]}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        let item = &rt.content[0].content.as_ref().unwrap()[0];
        let children = item.content.as_ref().unwrap();
        assert_eq!(children.len(), 2, "expected 2 children in listItem");
        assert_eq!(children[0].node_type, "paragraph");
        assert_eq!(children[1].node_type, "mediaSingle");
        let media = &children[1].content.as_ref().unwrap()[0];
        assert_eq!(media.attrs.as_ref().unwrap()["id"], "img-001");
    }

    #[test]
    fn issue_430_multiple_media_in_list_items() {
        // Multiple list items each containing mediaSingle.
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[
          {"type":"listItem","content":[{
            "type":"mediaSingle","attrs":{"layout":"center"},
            "content":[{"type":"media","attrs":{"type":"file","id":"img-a","collection":"c1","height":10,"width":20}}]
          }]},
          {"type":"listItem","content":[{
            "type":"mediaSingle","attrs":{"layout":"center"},
            "content":[{"type":"media","attrs":{"type":"file","id":"img-b","collection":"c2","height":30,"width":40}}]
          }]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        let items = rt.content[0].content.as_ref().unwrap();
        assert_eq!(items.len(), 2);
        for (i, expected_id) in [("img-a", "c1"), ("img-b", "c2")].iter().enumerate() {
            let ms = &items[i].content.as_ref().unwrap()[0];
            assert_eq!(ms.node_type, "mediaSingle");
            let m_attrs = ms.content.as_ref().unwrap()[0].attrs.as_ref().unwrap();
            assert_eq!(m_attrs["id"], expected_id.0);
            assert_eq!(m_attrs["collection"], expected_id.1);
        }
    }

    #[test]
    fn issue_430_jfm_to_adf_media_in_bullet_item() {
        // Parse JFM directly: image syntax on the first line of a bullet item
        // must produce mediaSingle, not a paragraph with corrupted text.
        let md = "- ![](){type=file id=test-id collection=col-1 height=100 width=200}\n";
        let doc = markdown_to_adf(md).unwrap();

        let list = &doc.content[0];
        assert_eq!(list.node_type, "bulletList");
        let item = &list.content.as_ref().unwrap()[0];
        let ms = &item.content.as_ref().unwrap()[0];
        assert_eq!(
            ms.node_type, "mediaSingle",
            "expected mediaSingle, got {}",
            ms.node_type
        );
        let media = &ms.content.as_ref().unwrap()[0];
        assert_eq!(media.node_type, "media");
        let m_attrs = media.attrs.as_ref().unwrap();
        assert_eq!(m_attrs["type"], "file");
        assert_eq!(m_attrs["id"], "test-id");
    }

    #[test]
    fn issue_430_jfm_to_adf_media_in_ordered_item() {
        // Parse JFM directly: image syntax on the first line of an ordered list item.
        let md = "1. ![alt text](https://example.com/photo.jpg)\n";
        let doc = markdown_to_adf(md).unwrap();

        let list = &doc.content[0];
        assert_eq!(list.node_type, "orderedList");
        let item = &list.content.as_ref().unwrap()[0];
        let ms = &item.content.as_ref().unwrap()[0];
        assert_eq!(
            ms.node_type, "mediaSingle",
            "expected mediaSingle, got {}",
            ms.node_type
        );
    }

    #[test]
    fn issue_430_media_then_paragraph_in_bullet_list_roundtrip() {
        // listItem with mediaSingle as first child followed by a paragraph.
        // Exercises the sub_lines non-empty path when first_node is mediaSingle.
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[
          {"type":"listItem","content":[
            {"type":"mediaSingle","attrs":{"layout":"center"},
             "content":[{"type":"media","attrs":{"type":"file","id":"img-first","collection":"col-1","height":50,"width":100}}]},
            {"type":"paragraph","content":[{"type":"text","text":"Caption below"}]}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        let item = &rt.content[0].content.as_ref().unwrap()[0];
        let children = item.content.as_ref().unwrap();
        assert_eq!(children.len(), 2, "expected 2 children in listItem");
        assert_eq!(children[0].node_type, "mediaSingle");
        let media = &children[0].content.as_ref().unwrap()[0];
        assert_eq!(media.attrs.as_ref().unwrap()["id"], "img-first");
        assert_eq!(children[1].node_type, "paragraph");
    }

    #[test]
    fn issue_430_media_then_paragraph_in_ordered_list_roundtrip() {
        // Same as above but for ordered lists.
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"orderedList","attrs":{"order":1},"content":[
          {"type":"listItem","content":[
            {"type":"mediaSingle","attrs":{"layout":"center"},
             "content":[{"type":"media","attrs":{"type":"file","id":"img-ord","collection":"col-2","height":60,"width":120}}]},
            {"type":"paragraph","content":[{"type":"text","text":"Description"}]}
          ]}
        ]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        let item = &rt.content[0].content.as_ref().unwrap()[0];
        let children = item.content.as_ref().unwrap();
        assert_eq!(children.len(), 2, "expected 2 children in listItem");
        assert_eq!(children[0].node_type, "mediaSingle");
        assert_eq!(children[1].node_type, "paragraph");
    }

    #[test]
    fn issue_430_external_media_with_width_type_roundtrip() {
        // External image with widthType attr must survive round-trip.
        let adf_json = r#"{"version":1,"type":"doc","content":[{
          "type":"mediaSingle",
          "attrs":{"layout":"wide","width":800,"widthType":"pixel"},
          "content":[{
            "type":"media",
            "attrs":{"type":"external","url":"https://example.com/photo.png","alt":"wide photo"}
          }]
        }]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains("widthType=pixel"),
            "expected widthType=pixel in markdown, got: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        let ms = &rt.content[0];
        assert_eq!(ms.node_type, "mediaSingle");
        let ms_attrs = ms.attrs.as_ref().unwrap();
        assert_eq!(ms_attrs["widthType"], "pixel");
        assert_eq!(ms_attrs["width"], 800);
        assert_eq!(ms_attrs["layout"], "wide");
    }

    // ── Placeholder node tests ────────────────────────────────────

    #[test]
    fn adf_placeholder_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::placeholder(
                "Type something here",
            )])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains(":placeholder[Type something here]"),
            "expected :placeholder directive, got: {md}"
        );
    }

    #[test]
    fn markdown_placeholder_to_adf() {
        let doc = markdown_to_adf("Before :placeholder[Enter name] after").unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content[1].node_type, "placeholder");
        let attrs = content[1].attrs.as_ref().unwrap();
        assert_eq!(attrs["text"], "Enter name");
    }

    #[test]
    fn placeholder_round_trip() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"placeholder","attrs":{"text":"Type something here"}}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();
        let content = rt.content[0].content.as_ref().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0].node_type, "placeholder");
        let attrs = content[0].attrs.as_ref().unwrap();
        assert_eq!(attrs["text"], "Type something here");
    }

    #[test]
    fn placeholder_empty_text() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::placeholder("")])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(
            md.contains(":placeholder[]"),
            "expected empty placeholder directive, got: {md}"
        );
        let rt = markdown_to_adf(&md).unwrap();
        let content = rt.content[0].content.as_ref().unwrap();
        assert_eq!(content[0].node_type, "placeholder");
        assert_eq!(content[0].attrs.as_ref().unwrap()["text"], "");
    }

    #[test]
    fn placeholder_with_surrounding_text() {
        let md = "Click :placeholder[here] to continue\n";
        let doc = markdown_to_adf(md).unwrap();
        let content = doc.content[0].content.as_ref().unwrap();
        assert_eq!(content[0].text.as_deref(), Some("Click "));
        assert_eq!(content[1].node_type, "placeholder");
        assert_eq!(content[1].attrs.as_ref().unwrap()["text"], "here");
        assert_eq!(content[2].text.as_deref(), Some(" to continue"));
    }

    #[test]
    fn placeholder_missing_attrs() {
        // Placeholder node with no attrs should not panic
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode {
                node_type: "placeholder".to_string(),
                attrs: None,
                content: None,
                text: None,
                marks: None,
                local_id: None,
                parameters: None,
            }])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        // With no attrs, nothing is emitted for the placeholder
        assert!(!md.contains("placeholder"));
    }

    // Issue #446: mention in table+list loses id and misplaces localId
    #[test]
    fn mention_in_table_bullet_list_preserves_id_and_local_id() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"table","content":[{"type":"tableRow","content":[{"type":"tableCell","attrs":{"colspan":1,"colwidth":[200],"rowspan":1},"content":[{"type":"bulletList","content":[{"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"prefix text "},{"type":"mention","attrs":{"id":"aabbccdd11223344aabbccdd","localId":"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee","text":"@Alice Example"}},{"type":"text","text":" "}]}]}]}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        // Navigate: doc → table → tableRow → tableCell → bulletList → listItem → paragraph
        let cell = &rt.content[0].content.as_ref().unwrap()[0]
            .content
            .as_ref()
            .unwrap()[0];
        let list = &cell.content.as_ref().unwrap()[0];
        let list_item = &list.content.as_ref().unwrap()[0];

        // listItem must NOT have a localId attribute
        assert!(
            list_item
                .attrs
                .as_ref()
                .and_then(|a| a.get("localId"))
                .is_none(),
            "localId should stay on the mention, not the listItem"
        );

        let para = &list_item.content.as_ref().unwrap()[0];
        let inlines = para.content.as_ref().unwrap();

        // Should have: text("prefix text "), mention, text(" ")
        assert_eq!(inlines.len(), 3, "expected 3 inline nodes, got {inlines:?}");

        assert_eq!(inlines[0].node_type, "text");
        assert_eq!(inlines[0].text.as_deref(), Some("prefix text "));

        assert_eq!(inlines[1].node_type, "mention");
        let mention_attrs = inlines[1].attrs.as_ref().unwrap();
        assert_eq!(
            mention_attrs["id"], "aabbccdd11223344aabbccdd",
            "mention id must be preserved"
        );
        assert_eq!(
            mention_attrs["localId"], "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            "mention localId must be preserved"
        );
        assert_eq!(mention_attrs["text"], "@Alice Example");

        assert_eq!(inlines[2].node_type, "text");
        assert_eq!(inlines[2].text.as_deref(), Some(" "));
    }

    #[test]
    fn mention_in_bullet_list_preserves_id_and_local_id() {
        // Same bug outside of a table context
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"bulletList","content":[{"type":"listItem","content":[{"type":"paragraph","content":[{"type":"mention","attrs":{"id":"user123","localId":"11111111-2222-3333-4444-555555555555","text":"@Bob"}},{"type":"text","text":" "}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        let list_item = &rt.content[0].content.as_ref().unwrap()[0];
        assert!(
            list_item
                .attrs
                .as_ref()
                .and_then(|a| a.get("localId"))
                .is_none(),
            "localId should stay on the mention, not the listItem"
        );

        let para = &list_item.content.as_ref().unwrap()[0];
        let inlines = para.content.as_ref().unwrap();
        assert_eq!(inlines[0].node_type, "mention");
        let mention_attrs = inlines[0].attrs.as_ref().unwrap();
        assert_eq!(mention_attrs["id"], "user123");
        assert_eq!(
            mention_attrs["localId"],
            "11111111-2222-3333-4444-555555555555"
        );
    }

    #[test]
    fn mention_in_ordered_list_preserves_id_and_local_id() {
        let adf_json = r#"{"version":1,"type":"doc","content":[{"type":"orderedList","content":[{"type":"listItem","content":[{"type":"paragraph","content":[{"type":"text","text":"see "},{"type":"mention","attrs":{"id":"xyz","localId":"aaaa-bbbb","text":"@Carol"}}]}]}]}]}"#;
        let doc: AdfDocument = serde_json::from_str(adf_json).unwrap();
        let md = adf_to_markdown(&doc).unwrap();
        let rt = markdown_to_adf(&md).unwrap();

        let list_item = &rt.content[0].content.as_ref().unwrap()[0];
        assert!(
            list_item
                .attrs
                .as_ref()
                .and_then(|a| a.get("localId"))
                .is_none(),
            "localId should stay on the mention, not the listItem"
        );

        let para = &list_item.content.as_ref().unwrap()[0];
        let inlines = para.content.as_ref().unwrap();
        assert_eq!(inlines[1].node_type, "mention");
        let mention_attrs = inlines[1].attrs.as_ref().unwrap();
        assert_eq!(mention_attrs["id"], "xyz");
        assert_eq!(mention_attrs["localId"], "aaaa-bbbb");
    }

    #[test]
    fn list_item_own_local_id_with_mention_both_preserved() {
        // When a listItem has its own localId AND contains a mention with localId,
        // both should be preserved independently.
        let md = "- hello :mention[@Eve]{id=e1 localId=mention-lid} {localId=item-lid}\n";
        let doc = markdown_to_adf(md).unwrap();
        let list_item = &doc.content[0].content.as_ref().unwrap()[0];

        // listItem should have its own localId
        let item_attrs = list_item.attrs.as_ref().unwrap();
        assert_eq!(item_attrs["localId"], "item-lid");

        // mention should have its own localId
        let para = &list_item.content.as_ref().unwrap()[0];
        let inlines = para.content.as_ref().unwrap();
        let mention = inlines.iter().find(|n| n.node_type == "mention").unwrap();
        let mention_attrs = mention.attrs.as_ref().unwrap();
        assert_eq!(mention_attrs["id"], "e1");
        assert_eq!(mention_attrs["localId"], "mention-lid");
    }

    #[test]
    fn extract_trailing_local_id_ignores_directive_attrs() {
        // Directly test the helper: a line ending with a directive's {…}
        // should NOT be treated as a trailing localId.
        let line = "text :mention[@X]{id=abc localId=uuid}";
        let (text, lid, plid) = extract_trailing_local_id(line);
        assert_eq!(text, line, "text should be unchanged");
        assert!(
            lid.is_none(),
            "should not extract localId from directive attrs"
        );
        assert!(plid.is_none());
    }

    #[test]
    fn extract_trailing_local_id_matches_standalone_block() {
        // A standalone trailing {localId=…} separated by whitespace should still work.
        let line = "some text {localId=abc-123}";
        let (text, lid, plid) = extract_trailing_local_id(line);
        assert_eq!(text, "some text");
        assert_eq!(lid.as_deref(), Some("abc-123"));
        assert!(plid.is_none());
    }
}
