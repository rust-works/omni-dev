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

        let text = trimmed[level + 1..].trim();
        let inline_nodes = parse_inline(text);

        self.advance();
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
                let inline_nodes = parse_inline(text);
                items.push(AdfNode::task_item(
                    state,
                    vec![AdfNode::paragraph(inline_nodes)],
                ));
                self.advance();
            } else {
                let item_text = trimmed[2..].trim_end();
                let inline_nodes = parse_inline(item_text);
                self.advance();
                // Collect indented sub-list lines (2-space prefix + list marker)
                let mut sub_lines: Vec<String> = Vec::new();
                while !self.at_end() {
                    let next = self.current_line();
                    if let Some(stripped) = next.strip_prefix("  ") {
                        let st = stripped.trim_start();
                        if st.starts_with("- ") || st.starts_with("* ") || st.starts_with("+ ") {
                            sub_lines.push(stripped.to_string());
                            self.advance();
                            continue;
                        }
                    }
                    break;
                }
                if sub_lines.is_empty() {
                    items.push(AdfNode::list_item(vec![AdfNode::paragraph(inline_nodes)]));
                } else {
                    let sub_text = sub_lines.join("\n");
                    let mut nested = MarkdownParser::new(&sub_text).parse_blocks()?;
                    let mut item_content = vec![AdfNode::paragraph(inline_nodes)];
                    item_content.append(&mut nested);
                    items.push(AdfNode::list_item(item_content));
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
                let inline_nodes = parse_inline(rest.trim());
                items.push(AdfNode::list_item(vec![AdfNode::paragraph(inline_nodes)]));
                self.advance();
            } else {
                break;
            }
        }

        if items.is_empty() {
            Ok(None)
        } else {
            let start_attr = if start == 1 { None } else { Some(start) };
            Ok(Some(AdfNode::ordered_list(items, start_attr)))
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
            marks.push(AdfMark::breakout(mode));
        }

        if !marks.is_empty() {
            let existing = node.marks.get_or_insert_with(Vec::new);
            existing.extend(marks);
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
                AdfNode::expand(title, inner_blocks)
            }
            "nested-expand" => {
                let title = d.attrs.as_ref().and_then(|a| a.get("title"));
                let inner_blocks = MarkdownParser::new(&inner_text).parse_blocks()?;
                AdfNode::nested_expand(title, inner_blocks)
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
                AdfNode::bodied_extension(ext_type, ext_key, inner_blocks)
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
                    i += 1;
                    let (row, next_i) = self.parse_directive_table_row(&lines, i)?;
                    rows.push(row);
                    i = next_i;
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
                let width = d
                    .attrs
                    .as_ref()
                    .and_then(|a| a.get("width"))
                    .and_then(|w| w.parse::<u32>().ok());
                AdfNode::embed_card(url, layout, width)
            }
            "extension" => {
                let ext_type = d.attrs.as_ref().and_then(|a| a.get("type")).unwrap_or("");
                let ext_key = d.attrs.as_ref().and_then(|a| a.get("key")).unwrap_or("");
                let params = d
                    .attrs
                    .as_ref()
                    .and_then(|a| a.get("params"))
                    .and_then(|p| serde_json::from_str(p).ok());
                AdfNode::extension(ext_type, ext_key, params)
            }
            "paragraph" => AdfNode::paragraph(vec![]),
            _ => return None,
        };

        self.advance();
        Some(node)
    }

    fn try_image(&mut self) -> Option<AdfNode> {
        let line = self.current_line().trim();
        if !line.starts_with("![") {
            return None;
        }

        if let Some((alt, url)) = parse_image_syntax(line) {
            self.advance();
            let alt_opt = if alt.is_empty() { None } else { Some(alt) };

            // Check for trailing {attrs} after the image syntax
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
                            media_attrs["collection"] =
                                serde_json::Value::String(collection.to_string());
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
                        return Some(AdfNode {
                            node_type: "mediaSingle".to_string(),
                            attrs: Some(ms_attrs),
                            content: Some(vec![AdfNode {
                                node_type: "media".to_string(),
                                attrs: Some(media_attrs),
                                content: None,
                                text: None,
                                marks: None,
                            }]),
                            text: None,
                            marks: None,
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
                    }
                    return Some(node);
                }
            }

            Some(AdfNode::media_single(url, alt_opt))
        } else {
            None
        }
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

/// Checks if a line starts a list item.
fn is_list_start(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("- ")
        || trimmed.starts_with("* ")
        || trimmed.starts_with("+ ")
        || parse_ordered_list_marker(trimmed).is_some()
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
        if adf_attrs == serde_json::json!({}) {
            (cell_text.to_string(), None)
        } else {
            (remaining, Some(adf_attrs))
        }
    } else {
        (cell_text.to_string(), None)
    }
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
                        add_mark(&mut node, mark.clone());
                        nodes.push(node);
                    }
                    // Advance past the consumed characters
                    while chars.peek().is_some_and(|&(idx, _)| idx < end) {
                        chars.next();
                    }
                    plain_start = end;
                    continue;
                }
                chars.next();
            }
            '~' => {
                if let Some((end, content)) = try_parse_strikethrough(text, i) {
                    flush_plain(text, plain_start, i, &mut nodes);
                    let inner = parse_inline(content);
                    for mut node in inner {
                        add_mark(&mut node, AdfMark::strike());
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
                    // When text == href, emit an inlineCard (smart link) so that
                    // JIRA renders it as a resolved card rather than a plain link.
                    if link_text == href {
                        nodes.push(AdfNode::inline_card(href));
                    } else {
                        let inner = parse_inline(link_text);
                        for mut node in inner {
                            add_mark(&mut node, AdfMark::link(href));
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

    nodes
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
fn add_mark(node: &mut AdfNode, mark: AdfMark) {
    if let Some(ref mut marks) = node.marks {
        marks.push(mark);
    } else {
        node.marks = Some(vec![mark]);
    }
}

/// Tries to parse **bold** or *italic* or __bold__ or _italic_ starting at position `i`.
/// Returns (end_position, inner_content, is_bold).
fn try_parse_emphasis(text: &str, i: usize) -> Option<(usize, &str, bool)> {
    let rest = &text[i..];

    // Bold: ** or __
    if rest.starts_with("**") || rest.starts_with("__") {
        let delimiter = &rest[..2];
        let after = &rest[2..];
        let close = after.find(delimiter)?;
        if close == 0 {
            return None;
        }
        let content = &after[..close];
        let end = i + 2 + close + 2;
        return Some((end, content, true));
    }

    // Italic: * or _
    if rest.starts_with('*') || rest.starts_with('_') {
        let delim_char = rest.as_bytes()[0];
        let after = &rest[1..];
        let close = after.find(delim_char as char)?;
        if close == 0 {
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

    let bracket_close = rest.find(']')?;
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
    if let Some(ann_id) = attrs.get("annotation-id") {
        let ann_type = attrs.get("annotation-type").unwrap_or("inlineComment");
        marks.push(AdfMark::annotation(ann_id, ann_type));
    }

    if marks.is_empty() {
        return None; // no recognized marks
    }

    let inner = parse_inline(span_text);
    let result: Vec<AdfNode> = inner
        .into_iter()
        .map(|mut node| {
            for mark in &marks {
                add_mark(&mut node, mark.clone());
            }
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
        "card" => AdfNode::inline_card(content),
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
            // Convert ISO 8601 date to epoch milliseconds for ADF
            let timestamp = iso_date_to_epoch_ms(content);
            AdfNode::date(&timestamp)
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
                AdfNode::text_with_marks(content, marks)
            }
        }
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
        let colon_name = format!(":{short_name}:");
        let mut emoji_attrs = serde_json::json!({"shortName": colon_name});
        if let Some(id) = attrs.get("id") {
            emoji_attrs["id"] = serde_json::Value::String(id.to_string());
        }
        if let Some(t) = attrs.get("text") {
            emoji_attrs["text"] = serde_json::Value::String(t.to_string());
        }
        (
            attr_end,
            AdfNode {
                node_type: "emoji".to_string(),
                attrs: Some(emoji_attrs),
                content: None,
                text: None,
                marks: None,
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

/// Converts an ADF document to a markdown string.
pub fn adf_to_markdown(doc: &AdfDocument) -> Result<String> {
    let mut output = String::new();

    for (i, node) in doc.content.iter().enumerate() {
        if i > 0 {
            output.push('\n');
        }
        render_block_node(node, &mut output);
    }

    Ok(output)
}

/// Renders a sequence of block nodes with blank-line separators between them.
fn render_block_children(children: &[AdfNode], output: &mut String) {
    for (i, child) in children.iter().enumerate() {
        if i > 0 {
            output.push('\n');
        }
        render_block_node(child, output);
    }
}

/// Renders a block-level ADF node to markdown.
fn render_block_node(node: &AdfNode, output: &mut String) {
    match node.node_type.as_str() {
        "paragraph" => {
            let is_empty = node.content.as_ref().map_or(true, Vec::is_empty);
            if is_empty {
                output.push_str("::paragraph\n");
            } else {
                render_inline_content(node, output);
                output.push('\n');
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
            render_inline_content(node, output);
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
                    render_block_node(child, &mut inner);
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
                    render_list_item_content(item, output);
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
                    render_list_item_content(item, output);
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
                    render_list_item_content(item, output);
                }
            }
        }
        "rule" => {
            output.push_str("---\n");
        }
        "table" => {
            render_table(node, output);
        }
        "mediaSingle" => {
            if let Some(ref content) = node.content {
                for child in content {
                    if child.node_type == "media" {
                        render_media(child, node.attrs.as_ref(), output);
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
                if let Some(width) = attrs.get("width").and_then(serde_json::Value::as_u64) {
                    attr_parts.push(format!("width={width}"));
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
                if let Some(params) = attrs.get("parameters") {
                    if let Ok(json_str) = serde_json::to_string(params) {
                        attr_parts.push(format!("params='{json_str}'"));
                    }
                }
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
                render_block_children(content, output);
            }
            output.push_str(":::\n");
        }
        "expand" | "nestedExpand" => {
            let directive_name = if node.node_type == "nestedExpand" {
                "nested-expand"
            } else {
                "expand"
            };
            let title = node
                .attrs
                .as_ref()
                .and_then(|a| a.get("title"))
                .and_then(serde_json::Value::as_str);
            if let Some(t) = title {
                output.push_str(&format!(":::{directive_name}{{title=\"{t}\"}}\n"));
            } else {
                output.push_str(&format!(":::{directive_name}\n"));
            }
            if let Some(ref content) = node.content {
                render_block_children(content, output);
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
                            render_block_children(col_content, output);
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
                    render_list_item_content(item, output);
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
                output.push_str(&format!(":::extension{{type={ext_type} key={ext_key}}}\n"));
                if let Some(ref content) = node.content {
                    render_block_children(content, output);
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

    // Emit block-level attribute marks (align, indent, breakout)
    if let Some(ref marks) = node.marks {
        let mut parts = Vec::new();
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
                }
                _ => {}
            }
        }
        if !parts.is_empty() {
            output.push_str(&format!("{{{}}}\n", parts.join(" ")));
        }
    }
}

/// Renders the content of a list item (unwraps the paragraph layer).
/// Nested block children (e.g. sub-lists) are indented with two spaces.
fn render_list_item_content(item: &AdfNode, output: &mut String) {
    let Some(ref content) = item.content else {
        return;
    };
    let mut iter = content.iter();
    let Some(first) = iter.next() else {
        return;
    };
    if first.node_type == "paragraph" {
        render_inline_content(first, output);
        output.push('\n');
    } else {
        render_block_node(first, output);
    }
    for child in iter {
        let mut nested = String::new();
        render_block_node(child, &mut nested);
        for line in nested.lines() {
            output.push_str("  ");
            output.push_str(line);
            output.push('\n');
        }
    }
}

/// Renders a table node, choosing between pipe table and directive table form.
fn render_table(node: &AdfNode, output: &mut String) {
    let Some(ref rows) = node.content else {
        return;
    };

    if table_qualifies_for_pipe_syntax(rows) {
        render_pipe_table(node, rows, output);
    } else {
        render_directive_table(node, rows, output);
    }
}

/// Checks whether all cells qualify for GFM pipe table syntax:
/// - Every cell has exactly one paragraph child with only inline nodes
/// - All `tableHeader` nodes appear exclusively in the first row
fn table_qualifies_for_pipe_syntax(rows: &[AdfNode]) -> bool {
    for (row_idx, row) in rows.iter().enumerate() {
        let Some(ref cells) = row.content else {
            continue;
        };
        for cell in cells {
            // Header cells outside first row → must use directive form
            if row_idx > 0 && cell.node_type == "tableHeader" {
                return false;
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
        }
    }
    true
}

/// Returns true if a paragraph node contains any `hardBreak` inline nodes.
fn cell_contains_hard_break(paragraph: &AdfNode) -> bool {
    paragraph
        .content
        .as_ref()
        .is_some_and(|nodes| nodes.iter().any(|n| n.node_type == "hardBreak"))
}

/// Renders a table as GFM pipe syntax.
fn render_pipe_table(node: &AdfNode, rows: &[AdfNode], output: &mut String) {
    for (row_idx, row) in rows.iter().enumerate() {
        let Some(ref cells) = row.content else {
            continue;
        };

        output.push('|');
        for cell in cells {
            output.push(' ');
            render_cell_attrs_prefix(cell, output);
            render_inline_content_from_first_paragraph(cell, output);
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
    render_table_level_attrs(node, output);
}

/// Renders a table as `::::table` directive syntax (block-content cells).
fn render_directive_table(node: &AdfNode, rows: &[AdfNode], output: &mut String) {
    // Opening fence with attrs
    let mut attr_parts = Vec::new();
    if let Some(ref attrs) = node.attrs {
        if let Some(layout) = attrs.get("layout").and_then(serde_json::Value::as_str) {
            attr_parts.push(format!("layout={layout}"));
        }
        if attrs
            .get("isNumberColumnEnabled")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            attr_parts.push("numbered".to_string());
        }
        if let Some(tw) = attrs.get("width").and_then(serde_json::Value::as_f64) {
            let tw_str = if tw.fract() == 0.0 {
                (tw as u64).to_string()
            } else {
                tw.to_string()
            };
            attr_parts.push(format!("width={tw_str}"));
        }
        if let Some(local_id) = attrs.get("localId").and_then(serde_json::Value::as_str) {
            attr_parts.push(format!("localId={local_id}"));
        }
    }
    if attr_parts.is_empty() {
        output.push_str("::::table\n");
    } else {
        output.push_str(&format!("::::table{{{}}}\n", attr_parts.join(" ")));
    }

    for row in rows {
        let Some(ref cells) = row.content else {
            continue;
        };
        output.push_str(":::tr\n");
        for cell in cells {
            let directive_name = if cell.node_type == "tableHeader" {
                "th"
            } else {
                "td"
            };
            let cell_attr_str = build_cell_attrs_string(cell);
            if cell_attr_str.is_empty() {
                output.push_str(&format!(":::{directive_name}\n"));
            } else {
                output.push_str(&format!(":::{directive_name}{{{cell_attr_str}}}\n"));
            }
            if let Some(ref content) = cell.content {
                for block in content {
                    render_block_node(block, output);
                }
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
    let attr_str = build_cell_attrs_string(cell);
    if !attr_str.is_empty() {
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
fn render_table_level_attrs(node: &AdfNode, output: &mut String) {
    if let Some(ref attrs) = node.attrs {
        let mut parts = Vec::new();
        if let Some(layout) = attrs.get("layout").and_then(serde_json::Value::as_str) {
            parts.push(format!("layout={layout}"));
        }
        if attrs
            .get("isNumberColumnEnabled")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            parts.push("numbered".to_string());
        }
        if let Some(tw) = attrs.get("width").and_then(serde_json::Value::as_f64) {
            let tw_str = if tw.fract() == 0.0 {
                (tw as u64).to_string()
            } else {
                tw.to_string()
            };
            parts.push(format!("width={tw_str}"));
        }
        if let Some(local_id) = attrs.get("localId").and_then(serde_json::Value::as_str) {
            parts.push(format!("localId={local_id}"));
        }
        if !parts.is_empty() {
            output.push_str(&format!("{{{}}}\n", parts.join(" ")));
        }
    }
}

/// Renders inline content from the first paragraph child of a table cell.
fn render_inline_content_from_first_paragraph(cell: &AdfNode, output: &mut String) {
    if let Some(ref content) = cell.content {
        if let Some(first) = content.first() {
            if first.node_type == "paragraph" {
                render_inline_content(first, output);
            }
        }
    }
}

/// Renders a media node as a markdown image, with optional parent (mediaSingle) attrs.
fn render_media(node: &AdfNode, parent_attrs: Option<&serde_json::Value>, output: &mut String) {
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
            }
            output.push_str(&format!("{{{}}}", parts.join(" ")));
        } else {
            // External image
            let url = attrs
                .get("url")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            output.push_str(&format!("![{alt}]({url})"));

            // Emit {layout=... width=... widthType=...} if parent has non-default attrs
            if let Some(p_attrs) = parent_attrs {
                let layout = p_attrs.get("layout").and_then(serde_json::Value::as_str);
                let width = p_attrs.get("width").and_then(serde_json::Value::as_u64);
                let width_type = p_attrs.get("widthType").and_then(serde_json::Value::as_str);
                let has_non_default = layout.is_some_and(|l| l != "center")
                    || width.is_some()
                    || width_type.is_some();
                if has_non_default {
                    let mut parts = Vec::new();
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
                    if !parts.is_empty() {
                        output.push_str(&format!("{{{}}}", parts.join(" ")));
                    }
                }
            }
        }

        output.push('\n');
    }
}

/// Renders inline content (text nodes with marks) from a block node's children.
fn render_inline_content(node: &AdfNode, output: &mut String) {
    if let Some(ref content) = node.content {
        for child in content {
            render_inline_node(child, output);
        }
    }
}

/// Renders a single inline ADF node to markdown.
fn render_inline_node(node: &AdfNode, output: &mut String) {
    match node.node_type.as_str() {
        "text" => {
            let text = node.text.as_deref().unwrap_or("");
            let marks = node.marks.as_deref().unwrap_or(&[]);
            render_marked_text(text, marks, output);
        }
        "hardBreak" => {
            output.push_str("  \n");
        }
        "inlineCard" => {
            if let Some(url) = node
                .attrs
                .as_ref()
                .and_then(|a| a.get("url"))
                .and_then(serde_json::Value::as_str)
            {
                output.push_str(":card[");
                output.push_str(url);
                output.push(']');
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

                    // Preserve id and text attrs for round-trip fidelity.
                    let id = attrs.get("id").and_then(serde_json::Value::as_str);
                    let text = attrs.get("text").and_then(serde_json::Value::as_str);
                    if id.is_some() || text.is_some() {
                        let mut parts = Vec::new();
                        if let Some(id) = id {
                            let escaped = id.replace('\\', "\\\\").replace('"', "\\\"");
                            parts.push(format!("id=\"{escaped}\""));
                        }
                        if let Some(text) = text {
                            let escaped = text.replace('\\', "\\\\").replace('"', "\\\"");
                            parts.push(format!("text=\"{escaped}\""));
                        }
                        output.push('{');
                        output.push_str(&parts.join(" "));
                        output.push('}');
                    }
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
                if let Some(local_id) = attrs.get("localId").and_then(serde_json::Value::as_str) {
                    if local_id != "00000000-0000-0000-0000-000000000000" {
                        attr_parts.push(format!("localId={local_id}"));
                    }
                }
                output.push_str(&format!(":status[{text}]{{{}}}", attr_parts.join(" ")));
            }
        }
        "date" => {
            if let Some(timestamp) = node
                .attrs
                .as_ref()
                .and_then(|a| a.get("timestamp"))
                .and_then(serde_json::Value::as_str)
            {
                // Convert epoch ms to ISO 8601 date for display
                let display = epoch_ms_to_iso_date(timestamp);
                output.push_str(&format!(":date[{display}]"));
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
                output.push_str(&format!(":mention[{text}]{{{}}}", attr_parts.join(" ")));
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
fn render_marked_text(text: &str, marks: &[AdfMark], output: &mut String) {
    // Determine wrapping order: link is outermost, then bold/italic, then code
    let has_link = marks.iter().find(|m| m.mark_type == "link");
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

    let mut inner = String::new();
    if has_strike {
        inner.push_str("~~");
    }
    if has_strong {
        inner.push_str("**");
    }
    if has_em {
        inner.push('*');
    }
    inner.push_str(text);
    if has_em {
        inner.push('*');
    }
    if has_strong {
        inner.push_str("**");
    }
    if has_strike {
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
        output.push_str(&format!(":span[{inner}]{{{}}}", attr_parts.join(" ")));
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
        output.push_str(&format!("[{inner}]{{{}}}", attr_parts.join(" ")));
    } else if let Some(link_mark) = has_link {
        let href = link_mark
            .attrs
            .as_ref()
            .and_then(|a| a.get("href"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        output.push('[');
        output.push_str(&inner);
        output.push_str("](");
        output.push_str(href);
        output.push(')');
    } else {
        output.push_str(&inner);
    }
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
    fn ordered_list_start_at_one_no_attrs() {
        let md = "1. First\n2. Second";
        let doc = markdown_to_adf(md).unwrap();
        let node = &doc.content[0];
        assert_eq!(node.node_type, "orderedList");
        assert!(node.attrs.is_none());
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
        assert_eq!(md.trim(), "[**bold link**](https://example.com)");
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
        assert!(md.contains("Line 1  \nLine 2"));
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
    fn self_link_still_becomes_inline_card() {
        // [url](url) — text equals url, still produces inlineCard (Tier 1 bare URL)
        let doc = markdown_to_adf("[https://example.com](https://example.com)").unwrap();
        let node = &doc.content[0].content.as_ref().unwrap()[0];
        assert_eq!(node.node_type, "inlineCard");
        assert_eq!(node.attrs.as_ref().unwrap()["url"], "https://example.com");
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
        assert_eq!(
            doc.content[0].attrs.as_ref().unwrap()["url"],
            "https://figma.com/file/abc"
        );
        assert_eq!(doc.content[0].attrs.as_ref().unwrap()["layout"], "wide");
    }

    #[test]
    fn adf_embed_card_to_markdown() {
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::embed_card(
                "https://figma.com/file/abc",
                Some("wide"),
                Some(80),
            )],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("::embed[https://figma.com/file/abc]{layout=wide width=80}"));
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
    }

    #[test]
    fn adf_breakout_to_markdown() {
        let mut node = AdfNode::code_block(Some("python"), "pass");
        node.marks = Some(vec![AdfMark::breakout("wide")]);
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![node],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains("{breakout=wide}"));
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
        // ADF dates use epoch ms; renderer converts back to ISO
        let doc = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::paragraph(vec![AdfNode::date("1776211200000")])],
        };
        let md = adf_to_markdown(&doc).unwrap();
        assert!(md.contains(":date[2026-04-15]"));
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
        assert!(md.contains(":date[2026-04-15]"));
    }

    #[test]
    fn round_trip_date() {
        let md = "Due by :date[2026-04-15].\n";
        let doc = markdown_to_adf(md).unwrap();
        let result = adf_to_markdown(&doc).unwrap();
        assert!(result.contains(":date[2026-04-15]"));
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
    fn table_without_hardbreak_uses_pipe_syntax() {
        // A simple table without hardBreak should still use pipe syntax
        let adf = AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content: vec![AdfNode::table(vec![AdfNode::table_row(vec![
                AdfNode::table_cell(vec![AdfNode::paragraph(vec![AdfNode::text("simple cell")])]),
            ])])],
        };
        let md = adf_to_markdown(&adf).unwrap();
        assert!(
            md.contains("| simple cell |"),
            "Simple table should use pipe syntax, got:\n{md}"
        );
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
}
