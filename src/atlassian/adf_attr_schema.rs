//! ADF per-node attribute schemas.
//!
//! Validates the `attrs` map on each ADF node against an upstream-derived
//! schema describing required/optional fields and their accepted shapes
//! (enum, integer range, URL, etc.).
//!
//! # Source of truth
//!
//! Attribute schemas are transcribed from
//! `packages/adf-schema/src/schema/nodes/<node>.ts` and
//! `packages/adf-schema/src/schema/marks/<mark>.ts` in the upstream
//! `@atlaskit/adf-schema` tarball pinned by
//! [`crate::atlassian::adf_schema::SCHEMA_VERSION`] /
//! [`crate::atlassian::adf_schema::UPSTREAM_TARBALL_SHA256`]. Each schema
//! entry cites the upstream file so refresh reviews are line-by-line
//! tractable.
//!
//! # Forward compatibility
//!
//! - Unknown node types are permissive (no validation runs). A future
//!   Atlassian schema addition does not start producing violations.
//! - Unknown attribute names are permissive (only declared fields are
//!   checked). This keeps round-trip safe — Atlassian sometimes adds
//!   optional fields that omni-dev's snapshot doesn't yet describe.
//! - `serde_json::Value::Null` for an optional field is treated as
//!   "absent" (matches Atlassian's payload conventions).
//!
//! # Coverage in this slice (PR #733-attrs)
//!
//! Schemas are encoded for the node types whose attribute mistakes are
//! user-visible and easy to produce by hand:
//!
//! - `panel.panelType`, `heading.level`, `media.type`, `mediaSingle.layout`,
//!   `taskItem.state`, `decisionItem.state`, `taskList.localId`,
//!   `decisionList.localId`, `status.color`, `extension.extensionType`,
//!   `extension.extensionKey`, `mention.id`, `date.timestamp`,
//!   `emoji.shortName`, `embedCard.url`, `expand.title`,
//!   `nestedExpand.title`, `orderedList.order`, `layoutColumn.width`,
//!   `codeBlock.language`, `bodiedExtension.extensionType`/`.extensionKey`.
//!
//! Mark-attribute schemas (`link.href`, `textColor.color`, …) live in the
//! mark-validation slice (PR #733-marks) and reuse the same `AttrType` /
//! `AttrProblem` machinery defined here.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde_json::Value;

use crate::atlassian::adf_schema::AdfSchemaViolation;

// -----------------------------------------------------------------------------
// Attribute-type primitives
// -----------------------------------------------------------------------------

/// Whether an attribute must be present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttrPresence {
    /// The attribute must be present (and non-null).
    Required,
    /// The attribute may be present or absent. Null is treated as absent.
    Optional,
}

/// The accepted shape of an attribute value.
#[derive(Debug, Clone, PartialEq)]
pub enum AttrType {
    /// One of a finite list of string values, case-sensitive.
    Enum(&'static [&'static str]),
    /// An integer (no fractional part) in `[lo, hi]` inclusive.
    IntRange(i64, i64),
    /// A number in `[lo, hi]` inclusive (accepts integers).
    NumRange(f64, f64),
    /// A number whose accepted `[lo, hi]` range depends on a sibling
    /// attribute's value.
    ///
    /// When the sibling attribute named `sibling` equals the string `equals`,
    /// the value is checked against `when_true`; otherwise (sibling absent,
    /// non-string, or a different string) against `when_false`.
    ///
    /// Models the upstream `mediaSingle.width` `anyOf`: pixel widths
    /// (`widthType = "pixel"`) are unbounded, percentage widths (the default)
    /// cap at 100. Resolution requires the full attr map, so it happens in
    /// [`validate_attrs`] before [`check_value`] runs.
    CondNumRange {
        /// Name of the sibling attribute that selects the range.
        sibling: &'static str,
        /// Sibling string value that selects [`Self::CondNumRange::when_true`].
        equals: &'static str,
        /// Inclusive `(lo, hi)` used when the sibling equals `equals`.
        when_true: (f64, f64),
        /// Inclusive `(lo, hi)` used otherwise.
        when_false: (f64, f64),
    },
    /// A boolean.
    Bool,
    /// Any JSON string (no further validation).
    String,
    /// A string that parses as an absolute URL.
    Url,
    /// A JSON object (any shape).
    Object,
    /// Any JSON value. Used for fields whose shape we have not audited.
    Free,
}

/// What is wrong with an attribute value, surfaced inside
/// [`AdfSchemaViolation::InvalidAttr`].
#[derive(Debug, Clone, PartialEq)]
pub enum AttrProblem {
    /// The value is a string but not in the allowed enum.
    NotInEnum {
        /// The accepted values, in declaration order.
        allowed: Vec<&'static str>,
        /// The actual value supplied (rendered as a string for display).
        actual: String,
    },
    /// The value is an integer outside the accepted range.
    OutOfRange {
        /// Inclusive lower bound.
        lo: i64,
        /// Inclusive upper bound.
        hi: i64,
        /// The actual value supplied.
        actual: i64,
    },
    /// The value is a number outside the accepted range.
    OutOfRangeF {
        /// Inclusive lower bound.
        lo: f64,
        /// Inclusive upper bound.
        hi: f64,
        /// The actual value supplied.
        actual: f64,
    },
    /// The value's JSON kind is wrong.
    WrongType {
        /// What was expected (e.g. `"string"`, `"integer"`, `"object"`).
        expected: &'static str,
    },
    /// The value is a string but doesn't satisfy a structured constraint.
    BadFormat {
        /// Short reason (e.g. `"not a valid URL"`).
        reason: &'static str,
    },
}

impl std::fmt::Display for AttrProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInEnum { allowed, actual } => {
                let allowed_str = allowed
                    .iter()
                    .map(|a| format!("'{a}'"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(
                    f,
                    "value '{actual}' is not in the allowed set ({allowed_str})"
                )
            }
            Self::OutOfRange { lo, hi, actual } => {
                write!(
                    f,
                    "value {actual} is outside the allowed range [{lo}, {hi}]"
                )
            }
            Self::OutOfRangeF { lo, hi, actual } => {
                write!(
                    f,
                    "value {actual} is outside the allowed range [{lo}, {hi}]"
                )
            }
            Self::WrongType { expected } => {
                write!(f, "value has wrong type (expected {expected})")
            }
            Self::BadFormat { reason } => write!(f, "{reason}"),
        }
    }
}

/// Schema describing the legal `attrs` for one node (or mark) type.
#[derive(Debug, Clone)]
pub struct AttrSchema {
    /// Field name → (type, presence). Order is preserved for diff stability
    /// against the upstream source.
    pub fields: &'static [(&'static str, AttrType, AttrPresence)],
}

// -----------------------------------------------------------------------------
// Per-node attribute schemas
// -----------------------------------------------------------------------------

const ENUM_PANEL_TYPE: &[&str] = &["info", "note", "warning", "success", "error", "custom"];

const ENUM_TASK_STATE: &[&str] = &["TODO", "DONE"];

const ENUM_DECISION_STATE: &[&str] = &["DECIDED", "UNDECIDED"];

const ENUM_MEDIA_TYPE: &[&str] = &["file", "link", "external"];

const ENUM_MEDIA_SINGLE_LAYOUT: &[&str] = &[
    "align-end",
    "align-start",
    "center",
    "full-width",
    "wide",
    "wrap-left",
    "wrap-right",
];

const ENUM_STATUS_COLOR: &[&str] = &["neutral", "purple", "blue", "red", "yellow", "green"];

const ENUM_MENTION_USER_TYPE: &[&str] = &["DEFAULT", "SPECIAL", "APP", "TEAM"];

const ENUM_EXTENSION_LAYOUT: &[&str] = &["default", "wide", "full-width"];

// Per-node entries. Each entry cites the upstream source file. Sorted
// alphabetically by node type for diffability.
type AttrEntry = (&'static str, AttrSchema);

const ATTR_ENTRIES: &[AttrEntry] = &[
    // blockCard — definitions/blockCard_node
    // upstream: { url?: string, data?: object }
    // We accept either; both optional in our snapshot to keep round-trip
    // tolerant of API responses that vary the shape.
    (
        "blockCard",
        AttrSchema {
            fields: &[
                ("url", AttrType::Url, AttrPresence::Optional),
                ("data", AttrType::Object, AttrPresence::Optional),
            ],
        },
    ),
    // bodiedExtension — definitions/bodiedExtension_node
    (
        "bodiedExtension",
        AttrSchema {
            fields: &[
                ("extensionType", AttrType::String, AttrPresence::Required),
                ("extensionKey", AttrType::String, AttrPresence::Required),
                (
                    "layout",
                    AttrType::Enum(ENUM_EXTENSION_LAYOUT),
                    AttrPresence::Optional,
                ),
                ("parameters", AttrType::Object, AttrPresence::Optional),
                ("text", AttrType::String, AttrPresence::Optional),
            ],
        },
    ),
    // codeBlock — definitions/codeBlock_node
    // upstream: { language?: string }
    (
        "codeBlock",
        AttrSchema {
            fields: &[("language", AttrType::String, AttrPresence::Optional)],
        },
    ),
    // date — definitions/date_node
    // upstream: { timestamp: string } (epoch-ms as a string, e.g. "1690000000000")
    (
        "date",
        AttrSchema {
            fields: &[("timestamp", AttrType::String, AttrPresence::Required)],
        },
    ),
    // decisionItem — definitions/decisionItem_node
    (
        "decisionItem",
        AttrSchema {
            fields: &[
                ("localId", AttrType::String, AttrPresence::Required),
                (
                    "state",
                    AttrType::Enum(ENUM_DECISION_STATE),
                    AttrPresence::Required,
                ),
            ],
        },
    ),
    // decisionList — definitions/decisionList_node
    (
        "decisionList",
        AttrSchema {
            fields: &[("localId", AttrType::String, AttrPresence::Required)],
        },
    ),
    // embedCard — definitions/embedCard_node
    (
        "embedCard",
        AttrSchema {
            fields: &[
                ("url", AttrType::Url, AttrPresence::Required),
                (
                    "layout",
                    AttrType::Enum(ENUM_EXTENSION_LAYOUT),
                    AttrPresence::Optional,
                ),
                (
                    "width",
                    AttrType::NumRange(0.0, 100.0),
                    AttrPresence::Optional,
                ),
                (
                    "originalHeight",
                    AttrType::NumRange(0.0, f64::MAX),
                    AttrPresence::Optional,
                ),
                (
                    "originalWidth",
                    AttrType::NumRange(0.0, f64::MAX),
                    AttrPresence::Optional,
                ),
            ],
        },
    ),
    // emoji — definitions/emoji_node
    (
        "emoji",
        AttrSchema {
            fields: &[
                ("shortName", AttrType::String, AttrPresence::Required),
                ("id", AttrType::String, AttrPresence::Optional),
                ("text", AttrType::String, AttrPresence::Optional),
            ],
        },
    ),
    // expand — definitions/expand_node
    (
        "expand",
        AttrSchema {
            fields: &[("title", AttrType::String, AttrPresence::Optional)],
        },
    ),
    // extension — definitions/extension_node
    (
        "extension",
        AttrSchema {
            fields: &[
                ("extensionType", AttrType::String, AttrPresence::Required),
                ("extensionKey", AttrType::String, AttrPresence::Required),
                (
                    "layout",
                    AttrType::Enum(ENUM_EXTENSION_LAYOUT),
                    AttrPresence::Optional,
                ),
                ("parameters", AttrType::Object, AttrPresence::Optional),
                ("text", AttrType::String, AttrPresence::Optional),
            ],
        },
    ),
    // heading — definitions/heading_node
    // upstream: { level: 1..=6 }
    (
        "heading",
        AttrSchema {
            fields: &[("level", AttrType::IntRange(1, 6), AttrPresence::Required)],
        },
    ),
    // inlineCard — definitions/inlineCard_node
    (
        "inlineCard",
        AttrSchema {
            fields: &[
                ("url", AttrType::Url, AttrPresence::Optional),
                ("data", AttrType::Object, AttrPresence::Optional),
            ],
        },
    ),
    // layoutColumn — definitions/layoutColumn_node
    // upstream: { width: number 0..=100 }
    (
        "layoutColumn",
        AttrSchema {
            fields: &[(
                "width",
                AttrType::NumRange(0.0, 100.0),
                AttrPresence::Required,
            )],
        },
    ),
    // media — definitions/media_node
    (
        "media",
        AttrSchema {
            fields: &[
                (
                    "type",
                    AttrType::Enum(ENUM_MEDIA_TYPE),
                    AttrPresence::Required,
                ),
                ("id", AttrType::String, AttrPresence::Optional),
                ("collection", AttrType::String, AttrPresence::Optional),
                ("url", AttrType::String, AttrPresence::Optional),
                ("alt", AttrType::String, AttrPresence::Optional),
                (
                    "width",
                    AttrType::NumRange(0.0, f64::MAX),
                    AttrPresence::Optional,
                ),
                (
                    "height",
                    AttrType::NumRange(0.0, f64::MAX),
                    AttrPresence::Optional,
                ),
                ("occurrenceKey", AttrType::String, AttrPresence::Optional),
            ],
        },
    ),
    // mediaSingle — definitions/mediaSingle_node
    // upstream `attrs` is an `anyOf`: width ∈ [0, 100] for the percentage
    // branch (the default when widthType is absent), width ∈ [0, ∞) for the
    // pixel branch (widthType = "pixel"). Real Confluence emits pixel widths
    // well above 100 for editor-sized images, so the range must branch on
    // widthType rather than cap unconditionally (issue #1037).
    (
        "mediaSingle",
        AttrSchema {
            fields: &[
                (
                    "layout",
                    AttrType::Enum(ENUM_MEDIA_SINGLE_LAYOUT),
                    AttrPresence::Optional,
                ),
                (
                    "width",
                    AttrType::CondNumRange {
                        sibling: "widthType",
                        equals: "pixel",
                        when_true: (0.0, f64::MAX),
                        when_false: (0.0, 100.0),
                    },
                    AttrPresence::Optional,
                ),
                ("widthType", AttrType::String, AttrPresence::Optional),
            ],
        },
    ),
    // mention — definitions/mention_node
    (
        "mention",
        AttrSchema {
            fields: &[
                ("id", AttrType::String, AttrPresence::Required),
                ("text", AttrType::String, AttrPresence::Optional),
                (
                    "userType",
                    AttrType::Enum(ENUM_MENTION_USER_TYPE),
                    AttrPresence::Optional,
                ),
                ("accessLevel", AttrType::String, AttrPresence::Optional),
            ],
        },
    ),
    // nestedExpand — definitions/nestedExpand_node
    (
        "nestedExpand",
        AttrSchema {
            fields: &[("title", AttrType::String, AttrPresence::Optional)],
        },
    ),
    // orderedList — definitions/orderedList_node
    // upstream: { order?: positive integer }
    (
        "orderedList",
        AttrSchema {
            fields: &[(
                "order",
                AttrType::IntRange(0, i64::MAX),
                AttrPresence::Optional,
            )],
        },
    ),
    // panel — definitions/panel_node
    // upstream: { panelType: enum }
    (
        "panel",
        AttrSchema {
            fields: &[(
                "panelType",
                AttrType::Enum(ENUM_PANEL_TYPE),
                AttrPresence::Required,
            )],
        },
    ),
    // status — definitions/status_node
    (
        "status",
        AttrSchema {
            fields: &[
                ("text", AttrType::String, AttrPresence::Required),
                (
                    "color",
                    AttrType::Enum(ENUM_STATUS_COLOR),
                    AttrPresence::Required,
                ),
                ("localId", AttrType::String, AttrPresence::Optional),
                ("style", AttrType::String, AttrPresence::Optional),
            ],
        },
    ),
    // taskItem — definitions/taskItem_node
    (
        "taskItem",
        AttrSchema {
            fields: &[
                ("localId", AttrType::String, AttrPresence::Required),
                (
                    "state",
                    AttrType::Enum(ENUM_TASK_STATE),
                    AttrPresence::Required,
                ),
            ],
        },
    ),
    // taskList — definitions/taskList_node
    (
        "taskList",
        AttrSchema {
            fields: &[("localId", AttrType::String, AttrPresence::Required)],
        },
    ),
];

static ATTR_SCHEMAS: LazyLock<HashMap<&'static str, &'static AttrSchema>> = LazyLock::new(|| {
    ATTR_ENTRIES
        .iter()
        .map(|(node_type, schema)| (*node_type, schema))
        .collect()
});

/// Returns the attribute schema for a node type, or `None` if not registered.
#[must_use]
pub fn attr_schema(node_type: &str) -> Option<&'static AttrSchema> {
    ATTR_SCHEMAS.get(node_type).copied()
}

// -----------------------------------------------------------------------------
// Attribute validation
// -----------------------------------------------------------------------------

/// Validates `attrs` against the schema for `node_type`, appending any
/// violations to `out`.
///
/// `path` should be the index path from the document root to the node whose
/// attrs are being validated. Each emitted violation will carry that path.
///
/// If `node_type` has no registered schema, no violations are emitted (the
/// validator is permissive on unknown node types). If the schema declares
/// fields but `attrs` is `None` and there are no required fields, no
/// violations are emitted either.
pub fn validate_attrs(
    node_type: &str,
    attrs: Option<&Value>,
    path: &[usize],
    out: &mut Vec<AdfSchemaViolation>,
) {
    let Some(schema) = attr_schema(node_type) else {
        return;
    };

    // Treat `Some(Null)` and missing object both as "absent".
    let attr_obj = match attrs {
        Some(Value::Object(map)) => Some(map),
        Some(Value::Null) | None => None,
        Some(_other) => {
            // attrs is present but not an object — every required field is
            // effectively missing; flag the most common failure (any field of
            // wrong shape) by reporting one MissingAttr per required field.
            // This mirrors how Atlassian's renderer treats malformed attrs.
            for (field, _ty, presence) in schema.fields {
                if *presence == AttrPresence::Required {
                    out.push(AdfSchemaViolation::MissingAttr {
                        node_type: node_type.to_string(),
                        attr_name: (*field).to_string(),
                        path: path.to_vec(),
                    });
                }
            }
            return;
        }
    };

    for (field, ty, presence) in schema.fields {
        let value = attr_obj.and_then(|m| m.get(*field));

        // Treat explicit Null as absent.
        let value = match value {
            Some(Value::Null) | None => None,
            Some(v) => Some(v),
        };

        match (value, *presence) {
            (None, AttrPresence::Required) => {
                out.push(AdfSchemaViolation::MissingAttr {
                    node_type: node_type.to_string(),
                    attr_name: (*field).to_string(),
                    path: path.to_vec(),
                });
            }
            (None, AttrPresence::Optional) => {
                // Absent and optional — fine.
            }
            (Some(v), _) => {
                // Resolve sibling-conditional types (e.g. mediaSingle.width
                // depends on widthType) against the full attr map before
                // shape-checking; non-conditional types pass through unchanged.
                let effective = resolve_attr_type(ty, attr_obj);
                if let Some(problem) = check_value(&effective, v) {
                    out.push(AdfSchemaViolation::InvalidAttr {
                        node_type: node_type.to_string(),
                        attr_name: (*field).to_string(),
                        problem,
                        path: path.to_vec(),
                    });
                }
            }
        }
    }
}

/// Resolves a possibly sibling-conditional [`AttrType`] into a concrete one,
/// using the node's full attribute map to pick the branch.
///
/// [`AttrType::CondNumRange`] collapses to a plain [`AttrType::NumRange`]
/// chosen by the sibling attribute; every other type is returned unchanged
/// (a cheap clone — all variants hold only `Copy` data or `&'static` slices).
fn resolve_attr_type(ty: &AttrType, attrs: Option<&serde_json::Map<String, Value>>) -> AttrType {
    match ty {
        AttrType::CondNumRange {
            sibling,
            equals,
            when_true,
            when_false,
        } => {
            let selected =
                attrs.and_then(|m| m.get(*sibling)).and_then(Value::as_str) == Some(*equals);
            let (lo, hi) = if selected { *when_true } else { *when_false };
            AttrType::NumRange(lo, hi)
        }
        other => other.clone(),
    }
}

/// Validates a single value against an [`AttrType`].
///
/// Returns `Some(problem)` describing what's wrong, or `None` if the value
/// is acceptable. Public so that mark-attribute validation
/// ([`crate::atlassian::adf_mark_schema`]) can reuse the same shape rules.
///
/// [`AttrType::CondNumRange`] should be resolved via [`resolve_attr_type`]
/// before reaching here (it needs sibling context this function lacks); if one
/// arrives unresolved, the stricter `when_false` range is applied so the check
/// is never silently permissive.
#[must_use]
pub fn check_value(ty: &AttrType, value: &Value) -> Option<AttrProblem> {
    match ty {
        AttrType::Enum(allowed) => match value.as_str() {
            Some(s) if allowed.contains(&s) => None,
            Some(s) => Some(AttrProblem::NotInEnum {
                allowed: allowed.to_vec(),
                actual: s.to_string(),
            }),
            None => Some(AttrProblem::WrongType { expected: "string" }),
        },
        AttrType::IntRange(lo, hi) => match value.as_i64() {
            Some(n) if n >= *lo && n <= *hi => None,
            Some(n) => Some(AttrProblem::OutOfRange {
                lo: *lo,
                hi: *hi,
                actual: n,
            }),
            None => Some(AttrProblem::WrongType {
                expected: "integer",
            }),
        },
        AttrType::NumRange(lo, hi) => match value.as_f64() {
            Some(n) if n >= *lo && n <= *hi => None,
            Some(n) => Some(AttrProblem::OutOfRangeF {
                lo: *lo,
                hi: *hi,
                actual: n,
            }),
            None => Some(AttrProblem::WrongType { expected: "number" }),
        },
        AttrType::CondNumRange { when_false, .. } => {
            // Reached only if a conditional type bypassed `resolve_attr_type`
            // (no sibling context). Apply the stricter branch defensively.
            check_value(&AttrType::NumRange(when_false.0, when_false.1), value)
        }
        AttrType::Bool => match value.as_bool() {
            Some(_) => None,
            None => Some(AttrProblem::WrongType { expected: "bool" }),
        },
        AttrType::String => match value.as_str() {
            Some(_) => None,
            None => Some(AttrProblem::WrongType { expected: "string" }),
        },
        AttrType::Url => match value.as_str() {
            Some(s) => match url::Url::parse(s) {
                Ok(_) => None,
                Err(_) => Some(AttrProblem::BadFormat {
                    reason: "not a valid URL",
                }),
            },
            None => Some(AttrProblem::WrongType { expected: "string" }),
        },
        AttrType::Object => match value {
            Value::Object(_) => None,
            _ => Some(AttrProblem::WrongType { expected: "object" }),
        },
        AttrType::Free => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(node_type: &str, attrs: Value) -> Vec<AdfSchemaViolation> {
        let mut out = Vec::new();
        validate_attrs(node_type, Some(&attrs), &[], &mut out);
        out
    }

    fn run_no_attrs(node_type: &str) -> Vec<AdfSchemaViolation> {
        let mut out = Vec::new();
        validate_attrs(node_type, None, &[], &mut out);
        out
    }

    #[test]
    fn panel_panel_type_known_value_validates() {
        for value in ENUM_PANEL_TYPE {
            assert!(
                run("panel", json!({ "panelType": value })).is_empty(),
                "panelType '{value}' should validate"
            );
        }
    }

    #[test]
    fn panel_panel_type_unknown_value_flagged() {
        let v = run("panel", json!({ "panelType": "purple" }));
        assert_eq!(v.len(), 1);
        match &v[0] {
            AdfSchemaViolation::InvalidAttr {
                node_type,
                attr_name,
                problem,
                ..
            } => {
                assert_eq!(node_type, "panel");
                assert_eq!(attr_name, "panelType");
                assert!(matches!(problem, AttrProblem::NotInEnum { .. }));
            }
            other => panic!("expected InvalidAttr, got {other:?}"),
        }
    }

    #[test]
    fn panel_missing_panel_type_flagged() {
        let v = run("panel", json!({}));
        assert_eq!(v.len(), 1);
        match &v[0] {
            AdfSchemaViolation::MissingAttr {
                node_type,
                attr_name,
                ..
            } => {
                assert_eq!(node_type, "panel");
                assert_eq!(attr_name, "panelType");
            }
            other => panic!("expected MissingAttr, got {other:?}"),
        }
    }

    #[test]
    fn panel_missing_attrs_object_flagged() {
        let v = run_no_attrs("panel");
        assert_eq!(v.len(), 1);
        assert!(matches!(v[0], AdfSchemaViolation::MissingAttr { .. }));
    }

    #[test]
    fn heading_level_in_range_validates() {
        for level in 1_i64..=6 {
            assert!(run("heading", json!({ "level": level })).is_empty());
        }
    }

    #[test]
    fn heading_level_out_of_range_flagged() {
        let v = run("heading", json!({ "level": 7 }));
        assert_eq!(v.len(), 1);
        match &v[0] {
            AdfSchemaViolation::InvalidAttr {
                attr_name, problem, ..
            } => {
                assert_eq!(attr_name, "level");
                assert!(matches!(
                    problem,
                    AttrProblem::OutOfRange {
                        lo: 1,
                        hi: 6,
                        actual: 7
                    }
                ));
            }
            other => panic!("expected InvalidAttr, got {other:?}"),
        }
    }

    #[test]
    fn heading_level_wrong_type_flagged() {
        let v = run("heading", json!({ "level": "two" }));
        assert_eq!(v.len(), 1);
        match &v[0] {
            AdfSchemaViolation::InvalidAttr { problem, .. } => {
                assert!(matches!(
                    problem,
                    AttrProblem::WrongType {
                        expected: "integer"
                    }
                ));
            }
            other => panic!("expected InvalidAttr, got {other:?}"),
        }
    }

    #[test]
    fn heading_missing_level_flagged_as_missing() {
        let v = run("heading", json!({}));
        assert_eq!(v.len(), 1);
        assert!(
            matches!(&v[0], AdfSchemaViolation::MissingAttr { attr_name, .. } if attr_name == "level")
        );
    }

    #[test]
    fn task_item_known_state_validates() {
        for state in ENUM_TASK_STATE {
            assert!(run("taskItem", json!({ "localId": "abc", "state": state })).is_empty());
        }
    }

    #[test]
    fn task_item_unknown_state_flagged() {
        let v = run(
            "taskItem",
            json!({ "localId": "abc", "state": "INPROGRESS" }),
        );
        assert_eq!(v.len(), 1);
        match &v[0] {
            AdfSchemaViolation::InvalidAttr { attr_name, .. } => {
                assert_eq!(attr_name, "state");
            }
            other => panic!("expected InvalidAttr, got {other:?}"),
        }
    }

    #[test]
    fn media_single_layout_known_validates() {
        assert!(run("mediaSingle", json!({ "layout": "center" })).is_empty());
        assert!(run("mediaSingle", json!({ "layout": "wide" })).is_empty());
    }

    #[test]
    fn media_single_layout_misspelled_flagged() {
        let v = run("mediaSingle", json!({ "layout": "centre" }));
        assert_eq!(v.len(), 1);
        assert!(matches!(
            &v[0],
            AdfSchemaViolation::InvalidAttr { attr_name, .. } if attr_name == "layout"
        ));
    }

    #[test]
    fn media_single_pixel_width_above_100_validates() {
        // The issue #1037 reproducer: real Confluence emits pixel widths well
        // above 100 for editor-sized images. With widthType:pixel the value is
        // unbounded, so width=900 must validate.
        assert!(run(
            "mediaSingle",
            json!({ "layout": "center", "width": 900, "widthType": "pixel" })
        )
        .is_empty());
    }

    #[test]
    fn media_single_percentage_width_above_100_flagged() {
        // The percentage branch (default when widthType is absent, or explicit)
        // still caps at 100 — width=900 must be flagged in both forms.
        for attrs in [
            json!({ "layout": "center", "width": 900 }),
            json!({ "layout": "center", "width": 900, "widthType": "percentage" }),
        ] {
            let v = run("mediaSingle", attrs.clone());
            assert_eq!(v.len(), 1, "expected one violation for {attrs}");
            match &v[0] {
                AdfSchemaViolation::InvalidAttr {
                    attr_name, problem, ..
                } => {
                    assert_eq!(attr_name, "width");
                    assert!(matches!(problem, AttrProblem::OutOfRangeF { .. }));
                }
                other => panic!("expected InvalidAttr, got {other:?}"),
            }
        }
    }

    #[test]
    fn media_single_percentage_width_in_range_validates() {
        assert!(run(
            "mediaSingle",
            json!({ "layout": "center", "width": 75, "widthType": "percentage" })
        )
        .is_empty());
        // widthType absent defaults to the percentage branch.
        assert!(run("mediaSingle", json!({ "layout": "center", "width": 75 })).is_empty());
    }

    #[test]
    fn media_type_required() {
        let v = run("media", json!({}));
        assert_eq!(v.len(), 1);
        assert!(matches!(
            &v[0],
            AdfSchemaViolation::MissingAttr { attr_name, .. } if attr_name == "type"
        ));
    }

    #[test]
    fn embed_card_url_format() {
        assert!(run("embedCard", json!({ "url": "https://example.com" })).is_empty());
        let v = run("embedCard", json!({ "url": "not a url" }));
        assert_eq!(v.len(), 1);
        match &v[0] {
            AdfSchemaViolation::InvalidAttr { problem, .. } => {
                assert!(matches!(problem, AttrProblem::BadFormat { .. }));
            }
            other => panic!("expected InvalidAttr, got {other:?}"),
        }
    }

    #[test]
    fn layout_column_width_in_range() {
        assert!(run("layoutColumn", json!({ "width": 33.3 })).is_empty());
        let v = run("layoutColumn", json!({ "width": 150 }));
        assert_eq!(v.len(), 1);
        assert!(matches!(
            &v[0],
            AdfSchemaViolation::InvalidAttr {
                problem: AttrProblem::OutOfRangeF { .. },
                ..
            }
        ));
    }

    #[test]
    fn ordered_list_order_optional() {
        assert!(run("orderedList", json!({})).is_empty());
        assert!(run("orderedList", json!({ "order": 5 })).is_empty());
        // Negative not in our IntRange(0, MAX) — flagged.
        let v = run("orderedList", json!({ "order": -1 }));
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn unknown_node_type_is_permissive() {
        assert!(run("madeUpNode", json!({ "anyField": "anyValue" })).is_empty());
    }

    #[test]
    fn unknown_field_under_known_node_is_permissive() {
        // panel only declares panelType; an extra unknown field is ignored.
        assert!(run("panel", json!({ "panelType": "info", "futureField": "ok" })).is_empty());
    }

    #[test]
    fn null_attribute_treated_as_absent() {
        // status.localId is optional; null is fine.
        assert!(run(
            "status",
            json!({ "text": "hi", "color": "blue", "localId": null })
        )
        .is_empty());
        // status.color is required; null is treated as absent → MissingAttr.
        let v = run("status", json!({ "text": "hi", "color": null }));
        assert!(matches!(
            &v[0],
            AdfSchemaViolation::MissingAttr { attr_name, .. } if attr_name == "color"
        ));
    }

    #[test]
    fn attrs_array_treated_as_invalid_object() {
        // Wrong-shape attrs (not an object): every required field flagged
        // missing.
        let mut out = Vec::new();
        validate_attrs("panel", Some(&json!([1, 2, 3])), &[], &mut out);
        assert_eq!(out.len(), 1);
        assert!(matches!(
            &out[0],
            AdfSchemaViolation::MissingAttr { attr_name, .. } if attr_name == "panelType"
        ));
    }

    #[test]
    fn attr_problem_display_messages() {
        let p = AttrProblem::NotInEnum {
            allowed: vec!["info", "note"],
            actual: "purple".to_string(),
        };
        let s = p.to_string();
        assert!(s.contains("'purple'"), "got: {s}");
        assert!(s.contains("'info'"), "got: {s}");

        let p = AttrProblem::OutOfRange {
            lo: 1,
            hi: 6,
            actual: 7,
        };
        assert!(p.to_string().contains("[1, 6]"));

        let p = AttrProblem::BadFormat {
            reason: "not a valid URL",
        };
        assert_eq!(p.to_string(), "not a valid URL");

        let p = AttrProblem::WrongType {
            expected: "integer",
        };
        assert!(p.to_string().contains("integer"));
    }

    #[test]
    fn attr_problem_out_of_range_f_display() {
        // Float-range Display arm — never exercised by the per-node fixture
        // tests above because their NumRange tests trigger the message
        // through the field validator, not directly. Cover it here.
        let p = AttrProblem::OutOfRangeF {
            lo: 0.0,
            hi: 100.0,
            actual: 200.0,
        };
        let s = p.to_string();
        assert!(s.contains("200"), "got: {s}");
        assert!(s.contains("[0, 100]"), "got: {s}");
    }

    // ── check_value: WrongType arms for every AttrType ──────────────
    //
    // Covers the `None =>` branch of each match in `check_value` that
    // converts a wrongly-typed JSON value into `AttrProblem::WrongType`.
    // Most of these aren't reachable via the per-node fixture tests
    // because each declared field is exercised with the *correct* shape;
    // these tests drive `check_value` directly with a deliberately
    // wrong value.

    #[test]
    fn check_value_enum_wrong_type_for_non_string() {
        let ty = AttrType::Enum(&["a", "b"]);
        let p = check_value(&ty, &json!(123)).expect("should reject");
        assert!(matches!(p, AttrProblem::WrongType { expected: "string" }));
    }

    #[test]
    fn check_value_int_range_wrong_type_for_non_integer() {
        let ty = AttrType::IntRange(0, 10);
        let p = check_value(&ty, &json!("abc")).expect("should reject");
        assert!(matches!(
            p,
            AttrProblem::WrongType {
                expected: "integer"
            }
        ));
    }

    #[test]
    fn check_value_num_range_wrong_type_for_non_number() {
        let ty = AttrType::NumRange(0.0, 100.0);
        let p = check_value(&ty, &json!("abc")).expect("should reject");
        assert!(matches!(p, AttrProblem::WrongType { expected: "number" }));
    }

    #[test]
    fn check_value_bool_arms() {
        let ty = AttrType::Bool;
        assert!(check_value(&ty, &json!(true)).is_none());
        let p = check_value(&ty, &json!("yes")).expect("should reject");
        assert!(matches!(p, AttrProblem::WrongType { expected: "bool" }));
    }

    #[test]
    fn check_value_string_arms() {
        let ty = AttrType::String;
        assert!(check_value(&ty, &json!("hi")).is_none());
        let p = check_value(&ty, &json!(42)).expect("should reject");
        assert!(matches!(p, AttrProblem::WrongType { expected: "string" }));
    }

    #[test]
    fn check_value_url_wrong_type_for_non_string() {
        let ty = AttrType::Url;
        let p = check_value(&ty, &json!(42)).expect("should reject");
        assert!(matches!(p, AttrProblem::WrongType { expected: "string" }));
    }

    #[test]
    fn check_value_object_arms() {
        let ty = AttrType::Object;
        assert!(check_value(&ty, &json!({"k": "v"})).is_none());
        let p = check_value(&ty, &json!([1, 2])).expect("should reject");
        assert!(matches!(p, AttrProblem::WrongType { expected: "object" }));
    }

    #[test]
    fn check_value_free_accepts_anything() {
        let ty = AttrType::Free;
        assert!(check_value(&ty, &json!(null)).is_none());
        assert!(check_value(&ty, &json!(42)).is_none());
        assert!(check_value(&ty, &json!("x")).is_none());
        assert!(check_value(&ty, &json!({"k": "v"})).is_none());
        assert!(check_value(&ty, &json!([1, 2, 3])).is_none());
    }
}
