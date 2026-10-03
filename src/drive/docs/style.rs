//! Bounded style patches: field masks are derived, never supplied by callers.
use serde::Serialize;

/// Explicit character formatting. Omitted properties are unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct TextStyle {
    /// Explicit bold value, including false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bold: Option<bool>,
    /// Explicit italic value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub italic: Option<bool>,
    /// Explicit underline value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub underline: Option<bool>,
    /// Explicit strikethrough value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strikethrough: Option<bool>,
}

/// Supported paragraph alignment values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Alignment {
    /// Leading edge in the paragraph's writing direction.
    Start,
    /// Centered.
    Center,
    /// Trailing edge in the paragraph's writing direction.
    End,
    /// Justified.
    Justified,
}

/// Supported named paragraph styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, clap::ValueEnum)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum NamedStyle {
    /// Ordinary prose.
    NormalText,
    /// Document title.
    Title,
    /// Document subtitle.
    Subtitle,
    /// Heading level one.
    #[serde(rename = "HEADING_1")]
    Heading1,
    /// Heading level two.
    #[serde(rename = "HEADING_2")]
    Heading2,
    /// Heading level three.
    #[serde(rename = "HEADING_3")]
    Heading3,
    /// Heading level four.
    #[serde(rename = "HEADING_4")]
    Heading4,
    /// Heading level five.
    #[serde(rename = "HEADING_5")]
    Heading5,
    /// Heading level six.
    #[serde(rename = "HEADING_6")]
    Heading6,
}

/// Explicit paragraph formatting. Omitted properties are unchanged.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ParagraphStyle {
    /// Paragraph alignment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alignment: Option<Alignment>,
    /// Named paragraph style; Google may change inherited formatting.
    #[serde(rename = "namedStyleType", skip_serializing_if = "Option::is_none")]
    pub named_style_type: Option<NamedStyle>,
}

/// Exactly one formatting family per invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum StylePatch {
    /// Character formatting.
    #[serde(rename = "textStyle")]
    Text(TextStyle),
    /// Whole-paragraph formatting.
    #[serde(rename = "paragraphStyle")]
    Paragraph(ParagraphStyle),
}

impl StylePatch {
    /// Deterministic field mask containing only explicitly supplied properties.
    #[must_use]
    pub fn fields(&self) -> String {
        let fields: Vec<&str> = match self {
            Self::Text(style) => [
                (style.bold.is_some(), "bold"),
                (style.italic.is_some(), "italic"),
                (style.underline.is_some(), "underline"),
                (style.strikethrough.is_some(), "strikethrough"),
            ]
            .into_iter()
            .filter_map(|(set, name)| set.then_some(name))
            .collect(),
            Self::Paragraph(style) => [
                (style.alignment.is_some(), "alignment"),
                (style.named_style_type.is_some(), "namedStyleType"),
            ]
            .into_iter()
            .filter_map(|(set, name)| set.then_some(name))
            .collect(),
        };
        fields.join(",")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn masks_include_explicit_false_and_only_supplied_fields() {
        let text = StylePatch::Text(TextStyle {
            bold: Some(false),
            underline: Some(true),
            ..TextStyle::default()
        });
        assert_eq!(text.fields(), "bold,underline");
        assert_eq!(
            serde_json::to_value(text).unwrap(),
            serde_json::json!({"textStyle": {"bold": false, "underline": true}})
        );
        assert!(StylePatch::Text(TextStyle::default()).fields().is_empty());
        assert!(StylePatch::Paragraph(ParagraphStyle::default())
            .fields()
            .is_empty());
        assert_eq!(
            StylePatch::Paragraph(ParagraphStyle {
                alignment: None,
                named_style_type: Some(NamedStyle::Title)
            })
            .fields(),
            "namedStyleType"
        );
    }
    #[test]
    fn named_heading_styles_use_google_enum_spelling() {
        for (i, style) in [
            NamedStyle::Heading1,
            NamedStyle::Heading2,
            NamedStyle::Heading3,
            NamedStyle::Heading4,
            NamedStyle::Heading5,
            NamedStyle::Heading6,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(
                serde_json::to_value(style).unwrap(),
                format!("HEADING_{}", i + 1)
            );
        }
    }
}
