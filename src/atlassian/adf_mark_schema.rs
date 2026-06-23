//! ADF per-context mark allow-lists and per-mark attribute schemas.
//!
//! Validates the `marks` array on each ADF node against:
//!
//! 1. **Allow-list by context.** Marks on an *inline* node (text, hardBreak,
//!    mention, …) are checked against the *parent* container's inline-mark
//!    allow-list — e.g. `code` on text inside `paragraph` is fine, on text
//!    inside `heading` it is not. Marks on a *block* node (paragraph,
//!    heading, tableCell, …) are checked against that node type's
//!    block-mark allow-list.
//!
//! 2. **Per-mark attribute schema.** Re-uses the
//!    [`crate::atlassian::adf_attr_schema::AttrSchema`] /
//!    [`crate::atlassian::adf_attr_schema::AttrType`] machinery from the
//!    second sub-PR. `link.href` must parse as a URL, `subsup.type` must
//!    be `sub` or `sup`, etc.
//!
//! # Source of truth
//!
//! Lists are transcribed from
//! `packages/adf-schema/src/schema/marks/<mark>.ts` and the per-node
//! `inlineContent` / `marks` declarations in the upstream tarball pinned by
//! [`crate::atlassian::adf_schema::SCHEMA_VERSION`]. Mark groups (e.g.
//! `formatting`) are flattened into per-context allow-lists for direct
//! lookup; the trade-off (a slightly larger table vs. a runtime group
//! resolver) is the same one ADR-0023 made for content models.
//!
//! # Forward compatibility
//!
//! - Unknown parent / node types: no mark validation runs (permissive).
//! - Unknown mark types under known parents: flagged as
//!   [`crate::atlassian::adf_schema::AdfSchemaViolation::DisallowedMark`].
//!   `unsupportedMark` (the round-trip preservation wrapper for marks) is
//!   accepted everywhere via the same escape-hatch convention as
//!   `unsupportedBlock` / `unsupportedInline`.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde_json::Value;

use crate::atlassian::adf_attr_schema::{AttrPresence, AttrSchema, AttrType};
use crate::atlassian::adf_schema::AdfSchemaViolation;

// -----------------------------------------------------------------------------
// Mark allow-lists by context
// -----------------------------------------------------------------------------

/// Inline marks shared by most inline-content containers (paragraph,
/// taskItem, decisionItem, caption).
const STD_INLINE_MARKS: &[&str] = &[
    "annotation",
    "backgroundColor",
    "code",
    "em",
    "link",
    "strike",
    "strong",
    "subsup",
    "textColor",
    "underline",
];

/// Heading inline marks — same as STD_INLINE_MARKS minus `code` (upstream
/// `heading` content model excludes code marks since the heading text is
/// styled by the heading itself).
const HEADING_INLINE_MARKS: &[&str] = &[
    "annotation",
    "backgroundColor",
    "em",
    "link",
    "strike",
    "strong",
    "subsup",
    "textColor",
    "underline",
];

/// `codeBlock` text accepts no marks — code blocks are literal text.
const CODE_BLOCK_INLINE_MARKS: &[&str] = &[];

/// `caption` — narrower than std (no `code`, no `annotation`).
const CAPTION_INLINE_MARKS: &[&str] = &[
    "backgroundColor",
    "em",
    "link",
    "strike",
    "strong",
    "subsup",
    "textColor",
    "underline",
];

/// Block-level marks per block node type.
const PARAGRAPH_BLOCK_MARKS: &[&str] = &["alignment", "indentation"];
const HEADING_BLOCK_MARKS: &[&str] = &["alignment", "indentation"];
const TABLE_CELL_BLOCK_MARKS: &[&str] = &["backgroundColor", "border"];
const TABLE_HEADER_BLOCK_MARKS: &[&str] = &["backgroundColor", "border"];

// -----------------------------------------------------------------------------
// Inline-mark combination groups (issue #1047)
// -----------------------------------------------------------------------------

/// Marks permitted together on a *monospace* text node, transcribed from the
/// `code_inline_node` variant in the pinned upstream schema
/// (`assets/adf-schema/full.json`).
const CODE_INLINE_MARK_GROUP: &[&str] = &["annotation", "code", "link"];

/// Marks permitted together on a *styled* (non-monospace) text node,
/// transcribed from the `formatted_text_inline_node` variant.
const FORMATTED_INLINE_MARK_GROUP: &[&str] = &[
    "annotation",
    "backgroundColor",
    "em",
    "link",
    "strike",
    "strong",
    "subsup",
    "textColor",
    "underline",
];

/// The mutually-exclusive inline-mark groups. A text node's marks must all
/// fit within a *single* group — the upstream schema offers a text node as an
/// `anyOf` over these variants, so mixing marks from different groups (most
/// commonly `code` with any styling mark) is what the API rejects as opaque
/// `INVALID_INPUT`.
///
/// Note: `link` and `annotation` appear in both groups, so they never
/// conflict with anything. `code` appears only in the code group, which is
/// why it combines with nothing but `link`/`annotation`.
const INLINE_MARK_GROUPS: &[&[&str]] = &[CODE_INLINE_MARK_GROUP, FORMATTED_INLINE_MARK_GROUP];

/// True when some single group contains both marks (i.e. they may coexist on
/// one text node). Marks that belong to no group are treated as
/// non-conflicting here — their legality is decided by the allow-list check,
/// not the combination check.
fn marks_may_coexist(a: &str, b: &str) -> bool {
    let in_a_group = |m: &str| INLINE_MARK_GROUPS.iter().any(|g| g.contains(&m));
    if !in_a_group(a) || !in_a_group(b) {
        return true;
    }
    INLINE_MARK_GROUPS
        .iter()
        .any(|g| g.contains(&a) && g.contains(&b))
}

const INLINE_MARKS_ENTRIES: &[(&str, &[&str])] = &[
    ("caption", CAPTION_INLINE_MARKS),
    ("codeBlock", CODE_BLOCK_INLINE_MARKS),
    ("decisionItem", STD_INLINE_MARKS),
    ("heading", HEADING_INLINE_MARKS),
    ("paragraph", STD_INLINE_MARKS),
    ("taskItem", STD_INLINE_MARKS),
];

const BLOCK_MARKS_ENTRIES: &[(&str, &[&str])] = &[
    ("heading", HEADING_BLOCK_MARKS),
    ("paragraph", PARAGRAPH_BLOCK_MARKS),
    ("tableCell", TABLE_CELL_BLOCK_MARKS),
    ("tableHeader", TABLE_HEADER_BLOCK_MARKS),
];

static INLINE_MARKS: LazyLock<HashMap<&'static str, &'static [&'static str]>> =
    LazyLock::new(|| INLINE_MARKS_ENTRIES.iter().copied().collect());

static BLOCK_MARKS: LazyLock<HashMap<&'static str, &'static [&'static str]>> =
    LazyLock::new(|| BLOCK_MARKS_ENTRIES.iter().copied().collect());

/// Inline node types whose marks are validated against the *parent*'s
/// inline-mark allow-list (rather than the node's own block-mark
/// allow-list). Sorted alphabetically.
const INLINE_NODE_TYPES: &[&str] = &[
    "date",
    "emoji",
    "hardBreak",
    "inlineCard",
    "inlineExtension",
    "mediaInline",
    "mention",
    "placeholder",
    "status",
    "text",
];

/// Returns the allowed inline marks under an inline-content container, or
/// `None` if the container is not registered (permissive on unknown
/// parents).
#[must_use]
pub fn allowed_inline_marks(parent: &str) -> Option<&'static [&'static str]> {
    INLINE_MARKS.get(parent).copied()
}

/// Returns the allowed block-level marks for a block node type, or `None`
/// if the node has no registered block-mark allow-list.
#[must_use]
pub fn allowed_block_marks(node_type: &str) -> Option<&'static [&'static str]> {
    BLOCK_MARKS.get(node_type).copied()
}

/// True when `node_type` is an inline node (whose marks should be checked
/// against the parent's inline-mark allow-list, not its own block-mark
/// allow-list).
#[must_use]
pub fn is_inline_node(node_type: &str) -> bool {
    INLINE_NODE_TYPES.contains(&node_type)
}

/// True for the round-trip preservation wrapper. Accepted under any
/// context.
fn is_unsupported_mark(mark_type: &str) -> bool {
    mark_type == "unsupportedMark" || mark_type == "unsupportedNodeAttribute"
}

// -----------------------------------------------------------------------------
// Per-mark attribute schemas
// -----------------------------------------------------------------------------

const ENUM_SUBSUP_TYPE: &[&str] = &["sub", "sup"];
const ENUM_ALIGNMENT_ALIGN: &[&str] = &["start", "end", "center", "right", "left"];
const ENUM_BREAKOUT_MODE: &[&str] = &["wide", "full-width"];

type MarkAttrEntry = (&'static str, AttrSchema);

const MARK_ATTR_ENTRIES: &[MarkAttrEntry] = &[
    // alignment — marks/alignment.ts
    (
        "alignment",
        AttrSchema {
            fields: &[(
                "align",
                AttrType::Enum(ENUM_ALIGNMENT_ALIGN),
                AttrPresence::Required,
            )],
        },
    ),
    // annotation — marks/annotation.ts
    (
        "annotation",
        AttrSchema {
            fields: &[
                ("id", AttrType::String, AttrPresence::Required),
                ("annotationType", AttrType::String, AttrPresence::Required),
            ],
        },
    ),
    // backgroundColor — marks/backgroundColor.ts
    // upstream: { color: hex string } (must look like #RRGGBB or #RRGGBBAA)
    (
        "backgroundColor",
        AttrSchema {
            fields: &[("color", AttrType::String, AttrPresence::Required)],
        },
    ),
    // border — marks/border.ts
    (
        "border",
        AttrSchema {
            fields: &[
                ("color", AttrType::String, AttrPresence::Required),
                ("size", AttrType::IntRange(1, 3), AttrPresence::Required),
            ],
        },
    ),
    // breakout — marks/breakout.ts
    (
        "breakout",
        AttrSchema {
            fields: &[(
                "mode",
                AttrType::Enum(ENUM_BREAKOUT_MODE),
                AttrPresence::Required,
            )],
        },
    ),
    // code — marks/code.ts (no attrs upstream)
    ("code", AttrSchema { fields: &[] }),
    // em — marks/em.ts (no attrs)
    ("em", AttrSchema { fields: &[] }),
    // indentation — marks/indentation.ts
    (
        "indentation",
        AttrSchema {
            fields: &[("level", AttrType::IntRange(1, 6), AttrPresence::Required)],
        },
    ),
    // link — marks/link.ts
    // upstream: { href: string (URI), title?: string, id?: string,
    //             collection?: string, occurrenceKey?: string }
    (
        "link",
        AttrSchema {
            fields: &[
                ("href", AttrType::Url, AttrPresence::Required),
                ("title", AttrType::String, AttrPresence::Optional),
                ("id", AttrType::String, AttrPresence::Optional),
                ("collection", AttrType::String, AttrPresence::Optional),
                ("occurrenceKey", AttrType::String, AttrPresence::Optional),
            ],
        },
    ),
    // strike — marks/strike.ts (no attrs)
    ("strike", AttrSchema { fields: &[] }),
    // strong — marks/strong.ts (no attrs)
    ("strong", AttrSchema { fields: &[] }),
    // subsup — marks/subsup.ts
    (
        "subsup",
        AttrSchema {
            fields: &[(
                "type",
                AttrType::Enum(ENUM_SUBSUP_TYPE),
                AttrPresence::Required,
            )],
        },
    ),
    // textColor — marks/textColor.ts
    // upstream: { color: hex string }
    (
        "textColor",
        AttrSchema {
            fields: &[("color", AttrType::String, AttrPresence::Required)],
        },
    ),
    // underline — marks/underline.ts (no attrs)
    ("underline", AttrSchema { fields: &[] }),
];

static MARK_ATTR_SCHEMAS: LazyLock<HashMap<&'static str, &'static AttrSchema>> =
    LazyLock::new(|| {
        MARK_ATTR_ENTRIES
            .iter()
            .map(|(mark_type, schema)| (*mark_type, schema))
            .collect()
    });

/// Returns the attribute schema for a mark type, or `None` if not
/// registered.
///
/// Permissive on unknown marks — they will still be flagged by the
/// allow-list check if they appear in a context that doesn't permit
/// them.
#[must_use]
pub fn mark_attr_schema(mark_type: &str) -> Option<&'static AttrSchema> {
    MARK_ATTR_SCHEMAS.get(mark_type).copied()
}

// -----------------------------------------------------------------------------
// Validation entry point
// -----------------------------------------------------------------------------

/// Validates the marks on a single node, appending any violations to `out`.
///
/// `parent_type` is the parent of `node`. `path` is the index path from
/// the document root to `node` (the same path used by the
/// `DisallowedChild` / `Arity` checks).
///
/// Mark validation is structured as:
///
/// 1. Determine the active allow-list:
///     - inline node → inline-marks for `parent_type`
///     - block node  → block-marks for `node.node_type`
///       Unknown contexts produce no allow-list and skip the check.
///
/// 2. For each mark on the node:
///     - If `mark_type` is `unsupported{Mark,NodeAttribute}`, accept it
///       (round-trip preservation wrapper).
///     - If the allow-list doesn't include the mark, emit
///       `DisallowedMark`.
///    - Validate the mark's `attrs` against `mark_attr_schema(mark_type)`,
///      emitting `InvalidMarkAttr` per problem.
pub fn validate_marks(
    parent_type: &str,
    node: &crate::atlassian::adf::AdfNode,
    path: &[usize],
    out: &mut Vec<AdfSchemaViolation>,
) {
    let Some(marks) = node.marks.as_ref() else {
        return;
    };
    if marks.is_empty() {
        return;
    }

    let node_type = node.node_type.as_str();
    let allowed = if is_inline_node(node_type) {
        allowed_inline_marks(parent_type)
    } else {
        allowed_block_marks(node_type)
    };

    for (mark_idx, mark) in marks.iter().enumerate() {
        let mark_type = mark.mark_type.as_str();

        if is_unsupported_mark(mark_type) {
            continue;
        }

        if let Some(allowed) = allowed {
            if !allowed.contains(&mark_type) {
                out.push(AdfSchemaViolation::DisallowedMark {
                    mark_type: mark_type.to_string(),
                    parent_type: if is_inline_node(node_type) {
                        parent_type.to_string()
                    } else {
                        node_type.to_string()
                    },
                    inline_index: if is_inline_node(node_type) {
                        Some(*path.last().unwrap_or(&0))
                    } else {
                        None
                    },
                    path: path.to_vec(),
                });
                // Don't validate attrs for a mark that isn't even allowed
                // here — the schema lookup might still succeed but the
                // mark is structurally rejected.
                continue;
            }
        }

        if let Some(schema) = mark_attr_schema(mark_type) {
            validate_mark_attrs_against(
                schema,
                mark_type,
                mark.attrs.as_ref(),
                mark_idx,
                path,
                out,
            );
        }
    }

    // Cross-mark combination check (issue #1047). Only inline text marks are
    // partitioned into mutually-exclusive groups; block marks have no such
    // constraint, so restrict the check to inline nodes.
    if is_inline_node(node_type) {
        check_inline_mark_combination(parent_type, marks, path, out);
    }
}

/// Emits a single [`AdfSchemaViolation::ForbiddenMarkCombination`] for the
/// first pair of marks on `node` that cannot coexist on one text node per
/// [`INLINE_MARK_GROUPS`]. Reporting only the first pair keeps the diagnosis
/// focused (the same "surface the first violation" convention the rest of the
/// validator follows).
fn check_inline_mark_combination(
    parent_type: &str,
    marks: &[crate::atlassian::adf::AdfMark],
    path: &[usize],
    out: &mut Vec<AdfSchemaViolation>,
) {
    // Distinct mark types in first-seen order; skip the round-trip wrappers.
    let mut seen: Vec<&str> = Vec::new();
    for mark in marks {
        let m = mark.mark_type.as_str();
        if is_unsupported_mark(m) || seen.contains(&m) {
            continue;
        }
        seen.push(m);
    }

    for i in 0..seen.len() {
        for j in (i + 1)..seen.len() {
            if !marks_may_coexist(seen[i], seen[j]) {
                out.push(AdfSchemaViolation::ForbiddenMarkCombination {
                    mark_type: seen[i].to_string(),
                    conflicts_with: seen[j].to_string(),
                    parent_type: parent_type.to_string(),
                    inline_index: Some(*path.last().unwrap_or(&0)),
                    path: path.to_vec(),
                });
                return;
            }
        }
    }
}

fn validate_mark_attrs_against(
    schema: &AttrSchema,
    mark_type: &str,
    attrs: Option<&Value>,
    mark_idx: usize,
    path: &[usize],
    out: &mut Vec<AdfSchemaViolation>,
) {
    // Reuse the per-node validate_attrs by calling its underlying logic
    // through a small adapter that translates Missing/Invalid attr
    // violations into the mark-specific variants.
    let mut tmp: Vec<AdfSchemaViolation> = Vec::new();
    crate::atlassian::adf_attr_schema::validate_attrs(
        // The shared validate_attrs uses node_type as a *lookup key*. To
        // reuse its body without lookup, pass a sentinel that has no
        // schema and validate inline below. Easier: replicate the small
        // loop here so we control variant emission.
        "<__adf_mark_inline__>",
        attrs,
        path,
        &mut tmp,
    );
    debug_assert!(
        tmp.is_empty(),
        "sentinel must not match a registered schema"
    );

    // Inline replication of the schema walk so we emit the mark variants.
    let attr_obj = match attrs {
        Some(Value::Object(map)) => Some(map),
        Some(Value::Null) | None => None,
        Some(_other) => {
            for (field, _ty, presence) in schema.fields {
                if *presence == AttrPresence::Required {
                    out.push(AdfSchemaViolation::DisallowedMark {
                        mark_type: mark_type.to_string(),
                        parent_type: format!("<malformed attrs for mark '{mark_type}'>"),
                        inline_index: Some(mark_idx),
                        path: path.to_vec(),
                    });
                    let _ = field;
                    return;
                }
            }
            return;
        }
    };

    for (field, ty, presence) in schema.fields {
        let value = attr_obj.and_then(|m| m.get(*field));
        let value = match value {
            Some(Value::Null) | None => None,
            Some(v) => Some(v),
        };

        match (value, *presence) {
            (None, AttrPresence::Required) => {
                out.push(AdfSchemaViolation::InvalidMarkAttr {
                    mark_type: mark_type.to_string(),
                    attr_name: (*field).to_string(),
                    problem: crate::atlassian::adf_attr_schema::AttrProblem::WrongType {
                        expected: "present",
                    },
                    inline_index: Some(mark_idx),
                    path: path.to_vec(),
                });
            }
            (None, AttrPresence::Optional) => {}
            (Some(v), _) => {
                if let Some(problem) = crate::atlassian::adf_attr_schema::check_value(ty, v) {
                    out.push(AdfSchemaViolation::InvalidMarkAttr {
                        mark_type: mark_type.to_string(),
                        attr_name: (*field).to_string(),
                        problem,
                        inline_index: Some(mark_idx),
                        path: path.to_vec(),
                    });
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::needless_collect)]
mod tests {
    use super::*;
    use crate::atlassian::adf::{AdfMark, AdfNode};

    fn text_with_marks(text: &str, marks: Vec<AdfMark>) -> AdfNode {
        AdfNode {
            node_type: "text".to_string(),
            attrs: None,
            content: None,
            text: Some(text.to_string()),
            marks: Some(marks),
            local_id: None,
            parameters: None,
        }
    }

    fn paragraph_with_marks(marks: Vec<AdfMark>, content: Vec<AdfNode>) -> AdfNode {
        AdfNode {
            node_type: "paragraph".to_string(),
            attrs: None,
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
            text: None,
            marks: Some(marks),
            local_id: None,
            parameters: None,
        }
    }

    fn mark(mark_type: &str, attrs: Option<serde_json::Value>) -> AdfMark {
        AdfMark {
            mark_type: mark_type.to_string(),
            attrs,
        }
    }

    fn run_inline(parent: &str, child: AdfNode) -> Vec<AdfSchemaViolation> {
        let mut out = Vec::new();
        validate_marks(parent, &child, &[0_usize], &mut out);
        out
    }

    fn run_block(node: AdfNode) -> Vec<AdfSchemaViolation> {
        let mut out = Vec::new();
        validate_marks("doc", &node, &[0_usize], &mut out);
        out
    }

    // ---- Inline marks: allow-list ---------------------------------------

    #[test]
    fn paragraph_allows_code_mark_on_text() {
        let node = text_with_marks("hi", vec![mark("code", None)]);
        assert!(run_inline("paragraph", node).is_empty());
    }

    #[test]
    fn heading_rejects_code_mark_on_text() {
        let node = text_with_marks("hi", vec![mark("code", None)]);
        let v = run_inline("heading", node);
        assert_eq!(v.len(), 1, "got: {v:?}");
        match &v[0] {
            AdfSchemaViolation::DisallowedMark {
                mark_type,
                parent_type,
                ..
            } => {
                assert_eq!(mark_type, "code");
                assert_eq!(parent_type, "heading");
            }
            other => panic!("expected DisallowedMark, got {other:?}"),
        }
    }

    #[test]
    fn code_block_rejects_any_mark_on_text() {
        let node = text_with_marks("hi", vec![mark("strong", None)]);
        let v = run_inline("codeBlock", node);
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn unknown_parent_skips_mark_validation() {
        let node = text_with_marks("hi", vec![mark("madeUp", None)]);
        assert!(run_inline("madeUpParent", node).is_empty());
    }

    #[test]
    fn unsupported_mark_accepted_anywhere() {
        let node = text_with_marks(
            "hi",
            vec![
                mark("unsupportedMark", None),
                mark("unsupportedNodeAttribute", None),
            ],
        );
        assert!(run_inline("heading", node).is_empty());
    }

    // ---- Block marks ----------------------------------------------------

    #[test]
    fn paragraph_block_allows_alignment() {
        let node = paragraph_with_marks(
            vec![mark(
                "alignment",
                Some(serde_json::json!({"align": "center"})),
            )],
            vec![AdfNode::text("x")],
        );
        assert!(run_block(node).is_empty());
    }

    #[test]
    fn paragraph_block_rejects_border() {
        // border is a tableCell-only block mark.
        let node = paragraph_with_marks(
            vec![mark(
                "border",
                Some(serde_json::json!({"color": "#ff0000", "size": 1})),
            )],
            vec![AdfNode::text("x")],
        );
        let v = run_block(node);
        let disallowed: Vec<_> = v
            .iter()
            .filter(|v| matches!(v, AdfSchemaViolation::DisallowedMark { .. }))
            .collect();
        assert_eq!(disallowed.len(), 1, "got: {v:?}");
    }

    #[test]
    fn table_cell_allows_border() {
        let cell = AdfNode {
            node_type: "tableCell".to_string(),
            attrs: None,
            content: None,
            text: None,
            marks: Some(vec![mark(
                "border",
                Some(serde_json::json!({"color": "#ff0000", "size": 2})),
            )]),
            local_id: None,
            parameters: None,
        };
        assert!(run_block(cell).is_empty());
    }

    // ---- Mark attr validation -------------------------------------------

    #[test]
    fn link_mark_with_valid_href_validates() {
        let node = text_with_marks(
            "hi",
            vec![mark(
                "link",
                Some(serde_json::json!({"href": "https://x.com"})),
            )],
        );
        assert!(run_inline("paragraph", node).is_empty());
    }

    #[test]
    fn link_mark_with_invalid_href_flagged() {
        let node = text_with_marks(
            "hi",
            vec![mark("link", Some(serde_json::json!({"href": "not a url"})))],
        );
        let v = run_inline("paragraph", node);
        let invalid: Vec<_> = v
            .iter()
            .filter(|v| matches!(v, AdfSchemaViolation::InvalidMarkAttr { .. }))
            .collect();
        assert_eq!(invalid.len(), 1, "got: {v:?}");
    }

    #[test]
    fn link_mark_missing_href_flagged() {
        let node = text_with_marks("hi", vec![mark("link", Some(serde_json::json!({})))]);
        let v = run_inline("paragraph", node);
        let invalid: Vec<_> = v
            .iter()
            .filter(|v| matches!(v, AdfSchemaViolation::InvalidMarkAttr { .. }))
            .collect();
        assert_eq!(invalid.len(), 1);
    }

    #[test]
    fn subsup_known_type_validates() {
        for t in ["sub", "sup"] {
            let node = text_with_marks(
                "hi",
                vec![mark("subsup", Some(serde_json::json!({"type": t})))],
            );
            assert!(run_inline("paragraph", node).is_empty());
        }
    }

    #[test]
    fn subsup_unknown_type_flagged() {
        let node = text_with_marks(
            "hi",
            vec![mark("subsup", Some(serde_json::json!({"type": "side"})))],
        );
        let v = run_inline("paragraph", node);
        let invalid: Vec<_> = v
            .iter()
            .filter(|v| matches!(v, AdfSchemaViolation::InvalidMarkAttr { .. }))
            .collect();
        assert_eq!(invalid.len(), 1);
    }

    #[test]
    fn indentation_level_in_range() {
        let node = paragraph_with_marks(
            vec![mark("indentation", Some(serde_json::json!({"level": 3})))],
            vec![AdfNode::text("x")],
        );
        assert!(run_block(node).is_empty());
    }

    #[test]
    fn indentation_level_out_of_range_flagged() {
        let node = paragraph_with_marks(
            vec![mark("indentation", Some(serde_json::json!({"level": 10})))],
            vec![AdfNode::text("x")],
        );
        let v = run_block(node);
        let invalid: Vec<_> = v
            .iter()
            .filter(|v| matches!(v, AdfSchemaViolation::InvalidMarkAttr { .. }))
            .collect();
        assert_eq!(invalid.len(), 1);
    }

    #[test]
    fn border_with_size_too_large_flagged() {
        let cell = AdfNode {
            node_type: "tableCell".to_string(),
            attrs: None,
            content: None,
            text: None,
            marks: Some(vec![mark(
                "border",
                Some(serde_json::json!({"color": "#ff0000", "size": 5})),
            )]),
            local_id: None,
            parameters: None,
        };
        let v = run_block(cell);
        let invalid: Vec<_> = v
            .iter()
            .filter(|v| matches!(v, AdfSchemaViolation::InvalidMarkAttr { .. }))
            .collect();
        assert_eq!(invalid.len(), 1);
    }

    #[test]
    fn empty_marks_array_no_violations() {
        let node = text_with_marks("hi", vec![]);
        assert!(run_inline("paragraph", node).is_empty());
    }

    #[test]
    fn no_marks_field_no_violations() {
        let node = AdfNode::text("hi");
        assert!(run_inline("paragraph", node).is_empty());
    }

    // ── malformed attrs path on a mark ─────────────────────────────────

    #[test]
    fn link_mark_with_array_attrs_flagged_as_disallowed_mark() {
        // attrs is present but not an object (array). The link schema has
        // a required `href`, so the malformed-attrs branch fires and emits
        // a DisallowedMark with a sentinel parent_type marker.
        let node = text_with_marks(
            "click",
            vec![mark("link", Some(serde_json::json!([1, 2, 3])))],
        );
        let v = run_inline("paragraph", node);
        let disallowed: Vec<_> = v
            .iter()
            .filter(|v| matches!(v, AdfSchemaViolation::DisallowedMark { .. }))
            .collect();
        assert_eq!(disallowed.len(), 1, "got: {v:?}");
        match disallowed[0] {
            AdfSchemaViolation::DisallowedMark {
                mark_type,
                parent_type,
                ..
            } => {
                assert_eq!(mark_type, "link");
                assert!(
                    parent_type.contains("malformed attrs"),
                    "expected malformed-attrs sentinel, got: {parent_type}"
                );
            }
            other => panic!("expected DisallowedMark, got {other:?}"),
        }
    }

    #[test]
    fn code_mark_with_array_attrs_no_violation() {
        // `code` mark schema has no required fields, so the malformed-attrs
        // branch should fall through without emitting anything (covers the
        // bare `return;` arm at the bottom of the malformed-attrs match).
        let node = text_with_marks("x", vec![mark("code", Some(serde_json::json!([1, 2, 3])))]);
        assert!(run_inline("paragraph", node).is_empty());
    }

    // ── inline mark combinations (issue #1047) ────────────────────────

    fn combos(v: &[AdfSchemaViolation]) -> Vec<(&str, &str)> {
        v.iter()
            .filter_map(|x| match x {
                AdfSchemaViolation::ForbiddenMarkCombination {
                    mark_type,
                    conflicts_with,
                    ..
                } => Some((mark_type.as_str(), conflicts_with.as_str())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn strong_plus_code_is_forbidden() {
        // The issue's repeatable trigger: `**`text`**` → strong + code.
        let node = text_with_marks("hi", vec![mark("strong", None), mark("code", None)]);
        let v = run_inline("paragraph", node);
        assert_eq!(combos(&v), vec![("strong", "code")], "got: {v:?}");
    }

    #[test]
    fn code_plus_background_color_is_forbidden() {
        let node = text_with_marks(
            "hi",
            vec![
                mark("code", None),
                mark(
                    "backgroundColor",
                    Some(serde_json::json!({"color": "#ff0000"})),
                ),
            ],
        );
        let v = run_inline("paragraph", node);
        assert_eq!(combos(&v), vec![("code", "backgroundColor")], "got: {v:?}");
    }

    #[test]
    fn code_plus_link_is_allowed() {
        // `link` and `code` share the code-inline group, so a monospace link
        // is valid.
        let node = text_with_marks(
            "hi",
            vec![
                mark("code", None),
                mark("link", Some(serde_json::json!({"href": "https://x.com"}))),
            ],
        );
        let v = run_inline("paragraph", node);
        assert!(combos(&v).is_empty(), "got: {v:?}");
    }

    #[test]
    fn code_plus_annotation_is_allowed() {
        let node = text_with_marks(
            "hi",
            vec![
                mark("code", None),
                mark(
                    "annotation",
                    Some(serde_json::json!({"id": "a1", "annotationType": "inlineComment"})),
                ),
            ],
        );
        let v = run_inline("paragraph", node);
        assert!(combos(&v).is_empty(), "got: {v:?}");
    }

    #[test]
    fn styling_marks_combine_freely() {
        // strong + em + strike + underline all live in the formatted group.
        let node = text_with_marks(
            "hi",
            vec![
                mark("strong", None),
                mark("em", None),
                mark("strike", None),
                mark("underline", None),
            ],
        );
        let v = run_inline("paragraph", node);
        assert!(combos(&v).is_empty(), "got: {v:?}");
    }

    #[test]
    fn link_plus_text_color_is_allowed() {
        // Both live in the formatted group per the pinned upstream schema, so
        // this is NOT flagged (despite issue #1047's example listing it).
        let node = text_with_marks(
            "hi",
            vec![
                mark("link", Some(serde_json::json!({"href": "https://x.com"}))),
                mark("textColor", Some(serde_json::json!({"color": "#0000ff"}))),
            ],
        );
        let v = run_inline("paragraph", node);
        assert!(combos(&v).is_empty(), "got: {v:?}");
    }

    #[test]
    fn heading_bold_code_flags_both_disallowed_and_combination() {
        // In a heading, `code` is disallowed outright (DisallowedMark) AND the
        // bold+code pairing is a forbidden combination — so the violation list
        // mixes variants. `combos` must surface only the combination, ignoring
        // the DisallowedMark.
        let node = text_with_marks("hi", vec![mark("strong", None), mark("code", None)]);
        let v = run_inline("heading", node);
        assert!(
            v.iter()
                .any(|x| matches!(x, AdfSchemaViolation::DisallowedMark { .. })),
            "expected a DisallowedMark for code-on-heading too, got: {v:?}"
        );
        assert_eq!(combos(&v), vec![("strong", "code")], "got: {v:?}");
    }

    #[test]
    fn only_first_conflicting_pair_is_reported() {
        // code conflicts with both strong and em; only the first pair fires.
        let node = text_with_marks(
            "hi",
            vec![mark("code", None), mark("strong", None), mark("em", None)],
        );
        let v = run_inline("paragraph", node);
        assert_eq!(combos(&v), vec![("code", "strong")], "got: {v:?}");
    }

    #[test]
    fn combination_check_records_parent_and_inline_index() {
        let node = text_with_marks("hi", vec![mark("code", None), mark("strong", None)]);
        let mut out = Vec::new();
        validate_marks("paragraph", &node, &[3_usize], &mut out);
        assert_eq!(out.len(), 1, "got: {out:?}");
        assert!(
            matches!(
                &out[0],
                AdfSchemaViolation::ForbiddenMarkCombination { parent_type, inline_index, path, .. }
                    if parent_type == "paragraph" && *inline_index == Some(3) && path.as_slice() == [3]
            ),
            "got: {out:?}"
        );
    }

    #[test]
    fn marks_may_coexist_treats_unknown_marks_as_non_conflicting() {
        // A mark outside every group is not flagged by the combination check
        // (its legality is the allow-list's job).
        assert!(marks_may_coexist("code", "madeUpMark"));
        assert!(marks_may_coexist("madeUpMark", "strong"));
    }

    // ── inline node under a parent without an inline-mark allow-list ──

    #[test]
    fn inline_node_under_unknown_parent_skips_mark_check() {
        // Parent has no inline-mark allow-list, so the mark is neither
        // accepted-by-allow-list nor flagged. Validates that the
        // `if let Some(allowed) = allowed { ... }` guard short-circuits
        // cleanly when `allowed` is `None`, but the per-mark attr schema
        // still runs (link.href validation here).
        let node = text_with_marks(
            "x",
            vec![mark("link", Some(serde_json::json!({"href": "not a url"})))],
        );
        let v = run_inline("madeUpParent", node);
        // No DisallowedMark (no allow-list to violate).
        assert!(v
            .iter()
            .all(|v| !matches!(v, AdfSchemaViolation::DisallowedMark { .. })));
        // But the mark-attr validation still fires.
        assert!(v
            .iter()
            .any(|v| matches!(v, AdfSchemaViolation::InvalidMarkAttr { .. })));
    }
}
