//! Generic directive parsers for JFM.
//!
//! Supports three levels per the [Generic Directives proposal]:
//! - Inline: `:name[content]{attrs}` (e.g., `:status[In Progress]{color=blue}`)
//! - Leaf block: `::name[content]{attrs}` (e.g., `::card[https://example.com]`)
//! - Container: `:::name{attrs}` open/close fences (e.g., `:::panel{type=info}`)
//!
//! [Generic Directives proposal]: https://talk.commonmark.org/t/generic-directives-plugins-syntax/444

use crate::atlassian::attrs::{parse_attrs, Attrs};

/// A parsed directive at any level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedDirective {
    /// Directive name (e.g., "panel", "status", "card").
    pub name: String,
    /// Content inside `[...]` brackets, if present.
    pub content: Option<String>,
    /// Parsed `{key=value}` attributes, if present.
    pub attrs: Option<Attrs>,
    /// Byte position after the directive (for inline directives only).
    pub end_pos: usize,
}

/// Parses an inline directive `:name[content]{attrs}` starting at `pos`.
///
/// The name must be alphabetic (plus hyphens). Content in `[...]` is required.
/// Attributes in `{...}` are optional.
///
/// Returns the parsed directive or `None` if the text doesn't match.
pub fn try_parse_inline_directive(text: &str, pos: usize) -> Option<ParsedDirective> {
    let rest = &text[pos..];
    if !rest.starts_with(':') {
        return None;
    }

    // Parse name after ':'
    let name_start = 1;
    let name_end = rest[name_start..]
        .find(|c: char| !c.is_alphanumeric() && c != '-')
        .map_or(rest.len(), |i| i + name_start);

    if name_end == name_start {
        return None; // no name
    }
    let name = &rest[name_start..name_end];

    // Content in [...] is required for inline directives
    let after_name = &rest[name_end..];
    if !after_name.starts_with('[') {
        return None;
    }
    let bracket_close = after_name.find(']')?;
    let content = &after_name[1..bracket_close];
    let mut cursor = pos + name_end + bracket_close + 1;

    // Optional {attrs}
    let attrs = if cursor < text.len() && text[cursor..].starts_with('{') {
        let (end, a) = parse_attrs(text, cursor)?;
        cursor = end;
        Some(a)
    } else {
        None
    };

    Some(ParsedDirective {
        name: name.to_string(),
        content: Some(content.to_string()),
        attrs,
        end_pos: cursor,
    })
}

/// Parses a leaf block directive `::name[content]{attrs}` from a full line.
///
/// The line must start with `::` (exactly two colons, not three).
/// Content in `[...]` is optional. Attributes in `{...}` are optional.
pub fn try_parse_leaf_directive(line: &str) -> Option<ParsedDirective> {
    let trimmed = line.trim();
    if !trimmed.starts_with("::") || trimmed.starts_with(":::") {
        return None;
    }

    // Parse name after '::'
    let name_start = 2;
    let name_end = trimmed[name_start..]
        .find(|c: char| !c.is_alphanumeric() && c != '-')
        .map_or(trimmed.len(), |i| i + name_start);

    if name_end == name_start {
        return None;
    }
    let name = &trimmed[name_start..name_end];

    let mut cursor = name_end;

    // Optional content in [...]
    let content = if cursor < trimmed.len() && trimmed[cursor..].starts_with('[') {
        let bracket_close = trimmed[cursor..].find(']')? + cursor;
        let c = &trimmed[cursor + 1..bracket_close];
        cursor = bracket_close + 1;
        Some(c.to_string())
    } else {
        None
    };

    // Optional {attrs}
    let attrs = if cursor < trimmed.len() && trimmed[cursor..].starts_with('{') {
        let (end, a) = parse_attrs(trimmed, cursor)?;
        cursor = end;
        Some(a)
    } else {
        None
    };

    // Remaining text on the line should be empty (or whitespace)
    if !trimmed[cursor..].trim().is_empty() {
        return None;
    }

    Some(ParsedDirective {
        name: name.to_string(),
        content,
        attrs,
        end_pos: cursor,
    })
}

/// Parses a container directive opening fence `:::name{attrs}`.
///
/// Returns the parsed directive and the colon count (for matching the close fence).
/// The line must start with 3+ colons followed by a name.
pub fn try_parse_container_open(line: &str) -> Option<(ParsedDirective, usize)> {
    let trimmed = line.trim();
    if !trimmed.starts_with(":::") {
        return None;
    }

    // Count colons
    let colon_count = trimmed.chars().take_while(|&c| c == ':').count();

    // Parse name after colons
    let name_start = colon_count;
    let name_end = trimmed[name_start..]
        .find(|c: char| !c.is_alphanumeric() && c != '-')
        .map_or(trimmed.len(), |i| i + name_start);

    if name_end == name_start {
        return None; // bare `:::` is a close fence, not an open
    }
    let name = &trimmed[name_start..name_end];

    let mut cursor = name_end;

    // Optional {attrs}
    let attrs = if cursor < trimmed.len() && trimmed[cursor..].starts_with('{') {
        let (end, a) = parse_attrs(trimmed, cursor)?;
        cursor = end;
        Some(a)
    } else {
        None
    };

    // Remaining text on the line should be empty
    if !trimmed[cursor..].trim().is_empty() {
        return None;
    }

    let directive = ParsedDirective {
        name: name.to_string(),
        content: None,
        attrs,
        end_pos: cursor,
    };

    Some((directive, colon_count))
}

/// Checks whether a line is a container directive close fence with at least
/// `min_colons` colons and no name after them.
pub fn is_container_close(line: &str, min_colons: usize) -> bool {
    let trimmed = line.trim();
    let colon_count = trimmed.chars().take_while(|&c| c == ':').count();
    colon_count >= min_colons && trimmed[colon_count..].trim().is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Inline directives ──────────────────────────────────────────

    #[test]
    fn inline_card_directive() {
        let d = try_parse_inline_directive(":card[https://example.com]", 0).unwrap();
        assert_eq!(d.name, "card");
        assert_eq!(d.content.as_deref(), Some("https://example.com"));
        assert!(d.attrs.is_none());
        assert_eq!(d.end_pos, 26);
    }

    #[test]
    fn inline_status_with_attrs() {
        let d = try_parse_inline_directive(":status[In Progress]{color=blue}", 0).unwrap();
        assert_eq!(d.name, "status");
        assert_eq!(d.content.as_deref(), Some("In Progress"));
        assert_eq!(d.attrs.as_ref().unwrap().get("color"), Some("blue"));
        assert_eq!(d.end_pos, 32);
    }

    #[test]
    fn inline_date() {
        let d = try_parse_inline_directive(":date[2026-04-15]", 0).unwrap();
        assert_eq!(d.name, "date");
        assert_eq!(d.content.as_deref(), Some("2026-04-15"));
    }

    #[test]
    fn inline_mention_with_attrs() {
        let d = try_parse_inline_directive(":mention[Alice Smith]{id=5b10ac8d82e05b22cc7d4ef5}", 0)
            .unwrap();
        assert_eq!(d.name, "mention");
        assert_eq!(d.content.as_deref(), Some("Alice Smith"));
        assert_eq!(
            d.attrs.as_ref().unwrap().get("id"),
            Some("5b10ac8d82e05b22cc7d4ef5")
        );
    }

    #[test]
    fn inline_span_with_color() {
        let d = try_parse_inline_directive(":span[red text]{color=#ff5630}", 0).unwrap();
        assert_eq!(d.name, "span");
        assert_eq!(d.content.as_deref(), Some("red text"));
        assert_eq!(d.attrs.as_ref().unwrap().get("color"), Some("#ff5630"));
    }

    #[test]
    fn inline_at_offset() {
        let text = "See :card[url] here";
        let d = try_parse_inline_directive(text, 4).unwrap();
        assert_eq!(d.name, "card");
        assert_eq!(d.content.as_deref(), Some("url"));
        assert_eq!(d.end_pos, 14);
    }

    #[test]
    fn inline_no_brackets_fails() {
        assert!(try_parse_inline_directive(":card", 0).is_none());
    }

    #[test]
    fn inline_no_name_fails() {
        assert!(try_parse_inline_directive(":[content]", 0).is_none());
    }

    #[test]
    fn inline_not_starting_with_colon() {
        assert!(try_parse_inline_directive("card[url]", 0).is_none());
    }

    // ── Leaf block directives ───���──────────────────────────────────

    #[test]
    fn leaf_card() {
        let d = try_parse_leaf_directive("::card[https://example.com/browse/PROJ-123]").unwrap();
        assert_eq!(d.name, "card");
        assert_eq!(
            d.content.as_deref(),
            Some("https://example.com/browse/PROJ-123")
        );
    }

    #[test]
    fn leaf_embed_with_attrs() {
        let d =
            try_parse_leaf_directive("::embed[https://figma.com/file/abc]{layout=wide width=80}")
                .unwrap();
        assert_eq!(d.name, "embed");
        assert_eq!(d.content.as_deref(), Some("https://figma.com/file/abc"));
        assert_eq!(d.attrs.as_ref().unwrap().get("layout"), Some("wide"));
        assert_eq!(d.attrs.as_ref().unwrap().get("width"), Some("80"));
    }

    #[test]
    fn leaf_extension_no_content() {
        let d =
            try_parse_leaf_directive("::extension{type=\"com.atlassian.macro\" key=jira-chart}")
                .unwrap();
        assert_eq!(d.name, "extension");
        assert!(d.content.is_none());
        assert_eq!(
            d.attrs.as_ref().unwrap().get("type"),
            Some("com.atlassian.macro")
        );
        assert_eq!(d.attrs.as_ref().unwrap().get("key"), Some("jira-chart"));
    }

    #[test]
    fn leaf_rejects_triple_colon() {
        assert!(try_parse_leaf_directive(":::panel{type=info}").is_none());
    }

    #[test]
    fn leaf_rejects_trailing_text() {
        assert!(try_parse_leaf_directive("::card[url] extra").is_none());
    }

    // ── Container directives ───────────────────────────────────────

    #[test]
    fn container_panel() {
        let (d, colons) = try_parse_container_open(":::panel{type=info}").unwrap();
        assert_eq!(d.name, "panel");
        assert_eq!(d.attrs.as_ref().unwrap().get("type"), Some("info"));
        assert_eq!(colons, 3);
    }

    #[test]
    fn container_expand_with_title() {
        let (d, colons) = try_parse_container_open(":::expand{title=\"Click to expand\"}").unwrap();
        assert_eq!(d.name, "expand");
        assert_eq!(
            d.attrs.as_ref().unwrap().get("title"),
            Some("Click to expand")
        );
        assert_eq!(colons, 3);
    }

    #[test]
    fn container_four_colons_layout() {
        let (d, colons) = try_parse_container_open("::::layout").unwrap();
        assert_eq!(d.name, "layout");
        assert!(d.attrs.is_none());
        assert_eq!(colons, 4);
    }

    #[test]
    fn container_column_with_width() {
        let (d, colons) = try_parse_container_open(":::column{width=50}").unwrap();
        assert_eq!(d.name, "column");
        assert_eq!(d.attrs.as_ref().unwrap().get("width"), Some("50"));
        assert_eq!(colons, 3);
    }

    #[test]
    fn container_bare_close_is_not_open() {
        assert!(try_parse_container_open(":::").is_none());
    }

    #[test]
    fn container_close_matches_min_colons() {
        assert!(is_container_close(":::", 3));
        assert!(is_container_close("::::", 3));
        assert!(is_container_close("::::", 4));
        assert!(!is_container_close("::", 3));
        assert!(!is_container_close(":::panel", 3));
    }

    #[test]
    fn container_close_with_whitespace() {
        assert!(is_container_close(":::  ", 3));
        assert!(is_container_close("  :::  ", 3));
    }
}
