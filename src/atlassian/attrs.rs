//! Pandoc-style `{key=value}` attribute parser for JFM directives.
//!
//! Parses attribute blocks like `{type=info}`, `{color="bright red"}`,
//! `{underline}`, and `{bg=#DEEBFF colspan=2}`.

use std::collections::BTreeMap;

/// Parsed attributes from a `{...}` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Attrs {
    /// Key-value pairs (e.g., `type=info`, `color="#ff5630"`).
    pub map: BTreeMap<String, String>,
    /// Boolean flags without values (e.g., `underline`, `numbered`).
    pub flags: Vec<String>,
    /// Keys that appear more than once (e.g., multiple `annotation-id` values).
    pub(crate) multi: BTreeMap<String, Vec<String>>,
}

impl Attrs {
    /// Returns true if there are no attributes or flags.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty() && self.multi.is_empty() && self.flags.is_empty()
    }

    /// Gets a single value by key (first occurrence for multi-valued keys).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).map(String::as_str).or_else(|| {
            self.multi
                .get(key)
                .and_then(|v| v.first())
                .map(String::as_str)
        })
    }

    /// Returns all values for a key that may appear more than once.
    pub fn get_all(&self, key: &str) -> Vec<&str> {
        if let Some(values) = self.multi.get(key) {
            values.iter().map(String::as_str).collect()
        } else if let Some(value) = self.map.get(key) {
            vec![value.as_str()]
        } else {
            vec![]
        }
    }

    /// Returns true if the given flag is present.
    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }

    /// Renders the attributes back to `{key=value flag}` syntax.
    pub fn render(&self) -> String {
        let mut parts = Vec::new();
        for (k, v) in &self.map {
            parts.push(format_kv(k, v));
        }
        for (k, values) in &self.multi {
            for v in values {
                parts.push(format_kv(k, v));
            }
        }
        for f in &self.flags {
            parts.push(f.clone());
        }
        format!("{{{}}}", parts.join(" "))
    }
}

/// Formats a `key=value` pair for a JFM `{...}` attribute block, quoting the
/// value if it contains whitespace, a closing brace, quote, or backslash.
///
/// The parser treats whitespace and `}` as value terminators for unquoted
/// values, so values containing these characters must be quoted and escaped
/// to round-trip correctly.
pub fn format_kv(k: &str, v: &str) -> String {
    let needs_quoting = v
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '}' | '"' | '\\'));
    if needs_quoting {
        let escaped = v.replace('\\', "\\\\").replace('"', "\\\"");
        format!("{k}=\"{escaped}\"")
    } else {
        format!("{k}={v}")
    }
}

/// Parses a `{...}` attribute block starting at `start` in `text`.
///
/// Returns `(end_pos_exclusive, Attrs)` on success, or `None` if the text
/// at `start` does not begin with `{` or is malformed.
pub fn parse_attrs(text: &str, start: usize) -> Option<(usize, Attrs)> {
    let rest = &text[start..];
    if !rest.starts_with('{') {
        return None;
    }

    let close = find_matching_brace(rest)?;
    let inner = &rest[1..close];

    let attrs = parse_inner(inner)?;
    Some((start + close + 1, attrs))
}

/// Finds the closing `}` that matches the opening `{` at position 0.
/// Skips over quoted strings.
fn find_matching_brace(text: &str) -> Option<usize> {
    let mut chars = text[1..].char_indices();
    while let Some((i, ch)) = chars.next() {
        match ch {
            '}' => return Some(i + 1),
            '"' => {
                // Skip quoted string
                loop {
                    match chars.next() {
                        Some((_, '\\')) => {
                            chars.next();
                        }
                        Some((_, '"')) | None => break,
                        _ => {}
                    }
                }
            }
            '\'' => {
                // Skip single-quoted string
                loop {
                    match chars.next() {
                        Some((_, '\\')) => {
                            chars.next();
                        }
                        Some((_, '\'')) | None => break,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Parses the content between `{` and `}` into an `Attrs` struct.
fn parse_inner(inner: &str) -> Option<Attrs> {
    let mut attrs = Attrs::default();
    let mut rest = inner.trim();

    while !rest.is_empty() {
        // Parse key (identifier: alphanumeric, hyphens, underscores)
        let key_end = rest
            .find(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
            .unwrap_or(rest.len());

        if key_end == 0 {
            return None; // unexpected character
        }

        let key = &rest[..key_end];
        rest = rest[key_end..].trim_start();

        if rest.starts_with('=') {
            // Key-value pair (no trim after '=' so empty values like key= are detected)
            rest = &rest[1..];
            let (value, remaining) = parse_value(rest)?;
            if let Some(existing) = attrs.map.remove(key) {
                // Key already seen once — promote to multi-valued.
                attrs
                    .multi
                    .entry(key.to_string())
                    .or_default()
                    .extend([existing, value]);
            } else if let Some(values) = attrs.multi.get_mut(key) {
                // Key already multi-valued — append.
                values.push(value);
            } else {
                attrs.map.insert(key.to_string(), value);
            }
            rest = remaining.trim_start();
        } else {
            // Boolean flag
            attrs.flags.push(key.to_string());
        }
    }

    Some(attrs)
}

/// Parses a value (quoted or unquoted) and returns `(value, remaining_text)`.
fn parse_value(text: &str) -> Option<(String, &str)> {
    if text.starts_with('"') {
        parse_quoted_value(text, '"')
    } else if text.starts_with('\'') {
        parse_quoted_value(text, '\'')
    } else {
        // Unquoted value: runs until whitespace or '}'
        let end = text
            .find(|c: char| c.is_whitespace() || c == '}')
            .unwrap_or(text.len());
        Some((text[..end].to_string(), &text[end..]))
    }
}

/// Parses a quoted value (double or single quotes) with backslash escaping.
fn parse_quoted_value(text: &str, quote: char) -> Option<(String, &str)> {
    let mut chars = text[1..].char_indices();
    let mut value = String::new();

    while let Some((i, ch)) = chars.next() {
        if ch == '\\' {
            if let Some((_, escaped)) = chars.next() {
                value.push(escaped);
            }
        } else if ch == quote {
            return Some((value, &text[i + 2..]));
        } else {
            value.push(ch);
        }
    }
    None // unterminated quote
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_key_value() {
        let (end, attrs) = parse_attrs("{type=info}", 0).unwrap();
        assert_eq!(end, 11);
        assert_eq!(attrs.get("type"), Some("info"));
        assert!(attrs.flags.is_empty());
    }

    #[test]
    fn multiple_key_values() {
        let (_, attrs) = parse_attrs("{type=info color=blue}", 0).unwrap();
        assert_eq!(attrs.get("type"), Some("info"));
        assert_eq!(attrs.get("color"), Some("blue"));
    }

    #[test]
    fn quoted_value() {
        let (_, attrs) = parse_attrs("{title=\"Click to expand\"}", 0).unwrap();
        assert_eq!(attrs.get("title"), Some("Click to expand"));
    }

    #[test]
    fn single_quoted_value() {
        let (_, attrs) = parse_attrs("{params='{\"jql\":\"project=PROJ\"}'}", 0).unwrap();
        assert_eq!(attrs.get("params"), Some("{\"jql\":\"project=PROJ\"}"));
    }

    #[test]
    fn boolean_flag() {
        let (_, attrs) = parse_attrs("{underline}", 0).unwrap();
        assert!(attrs.has_flag("underline"));
        assert!(attrs.map.is_empty());
    }

    #[test]
    fn mixed_flags_and_values() {
        let (_, attrs) = parse_attrs("{layout=wide numbered}", 0).unwrap();
        assert_eq!(attrs.get("layout"), Some("wide"));
        assert!(attrs.has_flag("numbered"));
    }

    #[test]
    fn hex_color_value() {
        let (_, attrs) = parse_attrs("{bg=#DEEBFF colspan=2}", 0).unwrap();
        assert_eq!(attrs.get("bg"), Some("#DEEBFF"));
        assert_eq!(attrs.get("colspan"), Some("2"));
    }

    #[test]
    fn offset_start() {
        let text = "some text {type=info}";
        let (end, attrs) = parse_attrs(text, 10).unwrap();
        assert_eq!(end, 21);
        assert_eq!(attrs.get("type"), Some("info"));
    }

    #[test]
    fn no_opening_brace() {
        assert!(parse_attrs("type=info}", 0).is_none());
    }

    #[test]
    fn unclosed_brace() {
        assert!(parse_attrs("{type=info", 0).is_none());
    }

    #[test]
    fn unterminated_quote() {
        assert!(parse_attrs("{title=\"no close}", 0).is_none());
    }

    #[test]
    fn empty_attrs() {
        let (end, attrs) = parse_attrs("{}", 0).unwrap();
        assert_eq!(end, 2);
        assert!(attrs.is_empty());
    }

    #[test]
    fn escaped_quote_in_value() {
        let (_, attrs) = parse_attrs("{title=\"say \\\"hello\\\"\"}", 0).unwrap();
        assert_eq!(attrs.get("title"), Some("say \"hello\""));
    }

    #[test]
    fn render_round_trip() {
        let (_, original) = parse_attrs("{type=info color=blue numbered}", 0).unwrap();
        let rendered = original.render();
        let (_, reparsed) = parse_attrs(&rendered, 0).unwrap();
        assert_eq!(original, reparsed);
    }

    #[test]
    fn render_quoted_value_with_spaces() {
        let (_, attrs) = parse_attrs("{title=\"Click to expand\"}", 0).unwrap();
        let rendered = attrs.render();
        assert_eq!(rendered, "{title=\"Click to expand\"}");
    }

    #[test]
    fn empty_value() {
        // Issue #363: accessLevel= (empty value) should parse as empty string
        let (end, attrs) = parse_attrs("{id=abc accessLevel=}", 0).unwrap();
        assert_eq!(end, 21);
        assert_eq!(attrs.get("id"), Some("abc"));
        assert_eq!(attrs.get("accessLevel"), Some(""));
    }

    #[test]
    fn empty_value_mid_attrs() {
        let (_, attrs) = parse_attrs("{a= b=value}", 0).unwrap();
        assert_eq!(attrs.get("a"), Some(""));
        assert_eq!(attrs.get("b"), Some("value"));
    }

    #[test]
    fn empty_value_render_round_trip() {
        let (_, original) = parse_attrs("{id=abc accessLevel=}", 0).unwrap();
        let rendered = original.render();
        let (_, reparsed) = parse_attrs(&rendered, 0).unwrap();
        assert_eq!(original, reparsed);
    }

    #[test]
    fn trailing_text_after_attrs() {
        let text = "{type=info} and more text";
        let (end, attrs) = parse_attrs(text, 0).unwrap();
        assert_eq!(end, 11);
        assert_eq!(attrs.get("type"), Some("info"));
        assert_eq!(&text[end..], " and more text");
    }

    #[test]
    fn duplicate_keys_parsed_as_multi() {
        // Issue #439: duplicate keys should all be preserved
        let input = r#"{annotation-id="id1" annotation-type=inlineComment annotation-id="id2" annotation-type=inlineComment}"#;
        let (_, attrs) = parse_attrs(input, 0).unwrap();
        let ids = attrs.get_all("annotation-id");
        assert_eq!(ids, vec!["id1", "id2"]);
        let types = attrs.get_all("annotation-type");
        assert_eq!(types, vec!["inlineComment", "inlineComment"]);
    }

    #[test]
    fn duplicate_keys_get_returns_first() {
        let input = "{k=\"first\" k=\"second\"}";
        let (_, attrs) = parse_attrs(input, 0).unwrap();
        assert_eq!(attrs.get("k"), Some("first"));
        assert_eq!(attrs.get_all("k"), vec!["first", "second"]);
    }

    #[test]
    fn three_duplicate_keys() {
        let input = "{x=a x=b x=c}";
        let (_, attrs) = parse_attrs(input, 0).unwrap();
        assert_eq!(attrs.get_all("x"), vec!["a", "b", "c"]);
        assert_eq!(attrs.get("x"), Some("a"));
    }

    #[test]
    fn duplicate_keys_render_round_trip() {
        let input = r#"{annotation-id="id1" annotation-type=inlineComment annotation-id="id2" annotation-type=inlineComment}"#;
        let (_, original) = parse_attrs(input, 0).unwrap();
        let rendered = original.render();
        let (_, reparsed) = parse_attrs(&rendered, 0).unwrap();
        assert_eq!(original, reparsed);
    }

    #[test]
    fn get_all_single_value() {
        let (_, attrs) = parse_attrs("{type=info}", 0).unwrap();
        assert_eq!(attrs.get_all("type"), vec!["info"]);
        assert!(attrs.get_all("missing").is_empty());
    }

    #[test]
    fn format_kv_plain() {
        assert_eq!(format_kv("k", "v"), "k=v");
        assert_eq!(format_kv("id", "abc-123"), "id=abc-123");
    }

    #[test]
    fn format_kv_with_space_quoted() {
        assert_eq!(format_kv("id", "a b c"), "id=\"a b c\"");
    }

    #[test]
    fn format_kv_with_tab_quoted() {
        assert_eq!(format_kv("id", "a\tb"), "id=\"a\tb\"");
    }

    #[test]
    fn format_kv_with_closing_brace_quoted() {
        assert_eq!(format_kv("id", "a}b"), "id=\"a}b\"");
    }

    #[test]
    fn format_kv_with_quote_escaped() {
        assert_eq!(format_kv("k", "a\"b"), r#"k="a\"b""#);
    }

    #[test]
    fn format_kv_with_backslash_escaped() {
        assert_eq!(format_kv("k", "a\\b"), r#"k="a\\b""#);
    }

    #[test]
    fn format_kv_empty_value_unquoted() {
        assert_eq!(format_kv("k", ""), "k=");
    }

    /// Issue #550: values containing spaces must round-trip through
    /// render → parse_attrs without corruption.
    #[test]
    fn format_kv_round_trip_with_spaces() {
        let rendered = format!("{{{}}}", format_kv("id", "abc 123 def 456"));
        let (_, attrs) = parse_attrs(&rendered, 0).unwrap();
        assert_eq!(attrs.get("id"), Some("abc 123 def 456"));
    }

    /// A value that contains an escaped-quote-and-backslash combination must
    /// also round-trip correctly.
    #[test]
    fn format_kv_round_trip_with_quote_and_backslash() {
        let original = "he said \\\"hi\\\"";
        let rendered = format!("{{{}}}", format_kv("msg", original));
        let (_, attrs) = parse_attrs(&rendered, 0).unwrap();
        assert_eq!(attrs.get("msg"), Some(original));
    }

    #[test]
    fn mixed_single_and_duplicate_keys() {
        let input = "{underline a=1 a=2 b=3}";
        let (_, attrs) = parse_attrs(input, 0).unwrap();
        assert!(attrs.has_flag("underline"));
        assert_eq!(attrs.get_all("a"), vec!["1", "2"]);
        assert_eq!(attrs.get("b"), Some("3"));
    }
}
