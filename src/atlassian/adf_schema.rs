//! ADF content-model schema and structural validator.
//!
//! Encodes the per-parent content expressions from the upstream
//! `@atlaskit/adf-schema` npm package as a static lookup table, and exposes a
//! [`validate_document`] walker that reports nesting **and** arity violations
//! against that model.
//!
//! # Source of truth
//!
//! The lookup table is a manual transcription of the per-node `content:`
//! expressions defined in the upstream schema. The pinned version is recorded
//! in [`SCHEMA_VERSION`] and the upstream tarball SHA-256 in
//! [`UPSTREAM_TARBALL_SHA256`]. When refreshing the snapshot, bump both
//! constants and re-verify each entry against the upstream source files
//! (`packages/adf-schema/src/schema/nodes/<node>.ts`).
//!
//! # Forward compatibility
//!
//! The walker is **permissive on unknown parents**: a node whose `node_type` is
//! not in the table is treated as opaque and its children are not validated.
//! This preserves the round-trip guarantee of ADR-0020's `adf-unsupported`
//! escape hatch — the validator never rejects a document just because it
//! contains a node type the snapshot doesn't know about.
//!
//! `unsupportedBlock` and `unsupportedInline` are accepted under any known
//! parent and **count toward arity** for the parent's current content term, so
//! a round-tripped document carrying a preservation wrapper still satisfies
//! the parent's `+` / `Exactly(n)` requirements.
//!
//! # Coverage in this slice
//!
//! - Allowed-children sets for every container node type (PR #717 / ADR-0023).
//! - Per-term quantifiers (`?`, `*`, `+`, exact, range) and per-parent term
//!   sequences (PR #733). Empty `bulletList`, two-`media` `mediaSingle`,
//!   `layoutSection` with one column, etc. are all flagged via
//!   [`AdfSchemaViolation::Arity`].
//!
//! Mark whitelists and attribute schemas are still out of scope; they are
//! addressed in the follow-up sub-PRs of #733.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::LazyLock;

use crate::atlassian::adf::{AdfDocument, AdfNode};

pub mod drift;
pub mod generated;

/// Crate-internal view of the schema as a `BTreeMap`, used by the drift
/// detector to diff against an upstream-derived map of the same shape.
///
/// Built by flattening every parent's [`CONTENT_ENTRIES`] terms into the
/// union of their atoms. Quantifier and order information is intentionally
/// stripped because the drift detector compares against the upstream JSON
/// schema's `anyOf` of `$ref` items, which has the same flat-set shape.
#[must_use]
pub(crate) fn local_schema_map() -> BTreeMap<&'static str, BTreeSet<&'static str>> {
    let mut m = BTreeMap::new();
    for (parent, terms) in CONTENT_ENTRIES {
        let children: BTreeSet<&'static str> =
            terms.iter().flat_map(|t| t.atoms.iter().copied()).collect();
        m.insert(*parent, children);
    }
    m
}

/// Pinned upstream schema version.
///
/// Format: `<npm-package-version>-<transcription-date>`. Bumped manually when
/// the lookup table is refreshed against a new upstream release.
pub const SCHEMA_VERSION: &str = "56.1.3-2026-07-16";

/// SHA-256 of the upstream `@atlaskit/adf-schema` tarball used as the source
/// for the current transcription.
///
/// Recorded for reproducibility. Kept here (not in the ADR) so the binary
/// itself carries the provenance and so refreshing the snapshot is a single
/// file change.
///
/// To verify locally:
/// ```text
/// curl -sL https://registry.npmjs.org/@atlaskit/adf-schema/-/adf-schema-56.1.3.tgz \
///   | shasum -a 256
/// ```
pub const UPSTREAM_TARBALL_SHA256: &str =
    "a40d1c999f0b08328fc40b4439cdb9013d170f97004a3016c5a3396e501b2855";

// -----------------------------------------------------------------------------
// Quantifier and content-term types
// -----------------------------------------------------------------------------

/// A quantifier applied to a single term in a parent's content expression.
///
/// Mirrors the ProseMirror content-expression grammar used by
/// `@atlaskit/adf-schema`. `Range` is used for the only known range case
/// (`layoutSection` permits 2–3 `layoutColumn` children); `Exactly` is used
/// for `mediaSingle`'s required `media` child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Quantifier {
    /// `?` — zero or one (optional).
    ZeroOrOne,
    /// `*` — zero or more.
    ZeroOrMore,
    /// `+` — one or more.
    OneOrMore,
    /// `{n}` — exactly `n`.
    Exactly(usize),
    /// `{min,max}` — between `min` and `max` inclusive.
    Range(usize, usize),
}

impl Quantifier {
    /// True when a count of `n` is acceptable for this quantifier.
    #[must_use]
    pub fn satisfied_by(&self, n: usize) -> bool {
        match *self {
            Self::ZeroOrOne => n <= 1,
            Self::ZeroOrMore => true,
            Self::OneOrMore => n >= 1,
            Self::Exactly(k) => n == k,
            Self::Range(lo, hi) => n >= lo && n <= hi,
        }
    }

    /// Human-readable phrasing used in [`AdfSchemaViolation`] messages.
    fn phrasing(&self) -> String {
        match *self {
            Self::ZeroOrOne => "at most one".to_string(),
            Self::ZeroOrMore => "any number of".to_string(),
            Self::OneOrMore => "at least one".to_string(),
            Self::Exactly(1) => "exactly one".to_string(),
            Self::Exactly(n) => format!("exactly {n}"),
            Self::Range(lo, hi) => format!("between {lo} and {hi}"),
        }
    }
}

/// One term in a parent's content expression: an atom (or alternation of
/// atoms), with a quantifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentTerm {
    /// One or more allowed node types. A list of length 1 is a single atom; a
    /// list of length >1 is an alternation.
    pub atoms: &'static [&'static str],
    /// Quantifier applied to this term.
    pub quant: Quantifier,
}

// -----------------------------------------------------------------------------
// Violation enum
// -----------------------------------------------------------------------------

/// A structural violation reported by the validator.
///
/// Each variant corresponds to a distinct class of issue so callers can opt in
/// to strictness (e.g. surface only [`Self::DisallowedChild`] today, then layer
/// in arity checks once their pipeline is ready). New variants are added in
/// later sub-PRs of #733 (marks, attributes); pattern matches should remain
/// non-exhaustive-aware.
#[derive(Debug, Clone, PartialEq)]
pub enum AdfSchemaViolation {
    /// A child node type appears under a parent that does not permit it.
    DisallowedChild {
        /// The `node_type` of the offending child.
        child_type: String,
        /// The `node_type` of the parent that does not permit the child.
        parent_type: String,
        /// Index path from the document root to the offending child.
        ///
        /// Each element is the position of the node in its parent's `content`
        /// array. The last element identifies the child within its parent.
        path: Vec<usize>,
    },

    /// A parent has the wrong number of children matching one of its content
    /// terms.
    ///
    /// Examples:
    /// - `mediaSingle` with two `media` children: `expected = Exactly(1)`,
    ///   `actual = 2`, `atoms = ["media"]`.
    /// - Empty `bulletList`: `expected = OneOrMore`, `actual = 0`,
    ///   `atoms = ["listItem"]`.
    /// - `layoutSection` with one column: `expected = Range(2, 3)`,
    ///   `actual = 1`, `atoms = ["layoutColumn"]`.
    Arity {
        /// The `node_type` of the parent whose content count is wrong.
        parent_type: String,
        /// The term's atoms (alternation list). Length 1 for a single atom,
        /// >1 for an alternation like `["tableCell", "tableHeader"]`.
        atoms: Vec<&'static str>,
        /// The quantifier the term expects.
        expected: Quantifier,
        /// The actual number of children matching the term's atoms.
        actual: usize,
        /// Index path from the document root to the **parent** node.
        path: Vec<usize>,
    },

    /// A node's `attrs` value is missing a required field.
    ///
    /// Example: `panel` without `panelType`, `heading` without `level`.
    MissingAttr {
        /// The `node_type` whose attrs are incomplete.
        node_type: String,
        /// The name of the missing attribute.
        attr_name: String,
        /// Index path from the document root to the offending node.
        path: Vec<usize>,
    },

    /// A node's `attrs` value has the wrong shape for a declared field.
    ///
    /// Examples: `panel.panelType: "purple"` (not in the enum),
    /// `heading.level: 7` (out of range), `heading.level: "two"` (wrong
    /// type), `embedCard.url: "not a url"` (bad format).
    InvalidAttr {
        /// The `node_type` whose attrs are malformed.
        node_type: String,
        /// The name of the offending attribute.
        attr_name: String,
        /// What is wrong with the value (enum / range / type / format).
        problem: crate::atlassian::adf_attr_schema::AttrProblem,
        /// Index path from the document root to the offending node.
        path: Vec<usize>,
    },

    /// A mark appears in a context that does not permit it.
    ///
    /// Examples: `code` mark on text inside a `heading`, `border` mark on a
    /// `paragraph` (block marks like `border` are tableCell-only).
    DisallowedMark {
        /// The `mark_type` of the offending mark.
        mark_type: String,
        /// The context that rejects this mark — for inline marks, the
        /// inline-content parent (e.g. `"heading"`); for block marks, the
        /// node whose own `marks` array contains the mark (e.g.
        /// `"paragraph"`).
        parent_type: String,
        /// For inline-mark violations, the position of the inline node
        /// within its parent. `None` for block-mark violations.
        inline_index: Option<usize>,
        /// Index path from the document root to the node whose marks were
        /// being validated.
        path: Vec<usize>,
    },

    /// A mark's `attrs` value has the wrong shape for a declared field, or
    /// is missing a required field.
    ///
    /// Examples: `link.href: "not a url"` (bad format),
    /// `subsup.type: "side"` (not in enum), `border.size: 5` (out of range
    /// 1..=3), `link` without `href` (required field absent).
    InvalidMarkAttr {
        /// The `mark_type` whose attrs are malformed.
        mark_type: String,
        /// The name of the offending attribute.
        attr_name: String,
        /// What is wrong with the value.
        problem: crate::atlassian::adf_attr_schema::AttrProblem,
        /// The position of the mark within the node's `marks` array (for
        /// disambiguation when a node carries multiple marks).
        inline_index: Option<usize>,
        /// Index path from the document root to the node whose mark is
        /// malformed.
        path: Vec<usize>,
    },

    /// Two marks on one inline text node that ADF does not allow together.
    ///
    /// The upstream JSON schema models text marks as two mutually-exclusive
    /// node variants (`code_inline_node` vs `formatted_text_inline_node`):
    /// a text node's marks must all fit within a single variant's mark group.
    /// The `code` mark lives only in the code group (`{annotation, code,
    /// link}`), so pairing it with a styling mark such as `strong` or
    /// `textColor` is rejected by the API as an opaque `INVALID_INPUT`
    /// (issue #1047).
    ForbiddenMarkCombination {
        /// One of the two conflicting marks (the earlier one in `marks`).
        mark_type: String,
        /// The other conflicting mark (the later one in `marks`).
        conflicts_with: String,
        /// The inline-content parent of the text node (e.g. `"paragraph"`).
        parent_type: String,
        /// The position of the offending inline node within its parent.
        inline_index: Option<usize>,
        /// Index path from the document root to the node whose marks conflict.
        path: Vec<usize>,
    },
}

impl AdfSchemaViolation {
    /// Path from the document root to the violation site.
    ///
    /// For [`Self::DisallowedChild`] this is the child; for [`Self::Arity`]
    /// this is the parent whose count is wrong; for [`Self::MissingAttr`]
    /// and [`Self::InvalidAttr`] this is the node whose attrs are wrong.
    #[must_use]
    pub fn path(&self) -> &[usize] {
        match self {
            Self::DisallowedChild { path, .. }
            | Self::Arity { path, .. }
            | Self::MissingAttr { path, .. }
            | Self::InvalidAttr { path, .. }
            | Self::DisallowedMark { path, .. }
            | Self::InvalidMarkAttr { path, .. }
            | Self::ForbiddenMarkCombination { path, .. } => path,
        }
    }
}

impl std::fmt::Display for AdfSchemaViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let path_str = self
            .path()
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join("/");
        match self {
            Self::DisallowedChild {
                child_type,
                parent_type,
                ..
            } => write!(
                f,
                "ADF schema violation at /{path_str}: '{child_type}' is not permitted inside '{parent_type}'",
            ),
            Self::Arity {
                parent_type,
                atoms,
                expected,
                actual,
                ..
            } => write!(
                f,
                "ADF schema violation at /{path_str}: '{parent_type}' must contain {phrasing} {atoms_str} (found {actual})",
                phrasing = expected.phrasing(),
                atoms_str = format_atoms(atoms),
            ),
            Self::MissingAttr {
                node_type,
                attr_name,
                ..
            } => write!(
                f,
                "ADF schema violation at /{path_str}: '{node_type}' is missing required attribute '{attr_name}'",
            ),
            Self::InvalidAttr {
                node_type,
                attr_name,
                problem,
                ..
            } => write!(
                f,
                "ADF schema violation at /{path_str}: '{node_type}.{attr_name}' is invalid — {problem}",
            ),
            Self::DisallowedMark {
                mark_type,
                parent_type,
                ..
            } => write!(
                f,
                "ADF schema violation at /{path_str}: '{mark_type}' mark is not permitted on '{parent_type}'",
            ),
            Self::InvalidMarkAttr {
                mark_type,
                attr_name,
                problem,
                ..
            } => write!(
                f,
                "ADF schema violation at /{path_str}: '{mark_type}' mark's '{attr_name}' is invalid — {problem}",
            ),
            Self::ForbiddenMarkCombination {
                mark_type,
                conflicts_with,
                ..
            } => write!(
                f,
                "ADF schema violation at /{path_str}: '{mark_type}' mark cannot be combined with '{conflicts_with}' mark on the same text",
            ),
        }
    }
}

fn format_atoms(atoms: &[&str]) -> String {
    if atoms.len() == 1 {
        format!("'{}'", atoms[0])
    } else {
        let inner = atoms
            .iter()
            .map(|a| format!("'{a}'"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{{{inner}}}")
    }
}

// -----------------------------------------------------------------------------
// Common atom slices
// -----------------------------------------------------------------------------

/// Inline content shared by `paragraph`, `heading`, `taskItem`, `decisionItem`.
///
/// `caption`'s inline list is a strict subset (no `inlineExtension`, no
/// `mediaInline`) and is inlined below to keep the per-parent diff against
/// upstream exact.
const FULL_INLINE_ATOMS: &[&str] = &[
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

const CAPTION_INLINE_ATOMS: &[&str] = &[
    "date",
    "emoji",
    "hardBreak",
    "inlineCard",
    "mention",
    "placeholder",
    "status",
    "text",
];

const LISTITEM_BLOCK_ATOMS: &[&str] = &[
    "bulletList",
    "codeBlock",
    "extension",
    "mediaSingle",
    "orderedList",
    "paragraph",
    "taskList",
];

const PANEL_BLOCK_ATOMS: &[&str] = &[
    "blockCard",
    "bulletList",
    "codeBlock",
    "decisionList",
    "extension",
    "heading",
    "mediaGroup",
    "mediaSingle",
    "orderedList",
    "paragraph",
    "rule",
    "taskList",
];

const NESTED_EXPAND_BLOCK_ATOMS: &[&str] = &[
    "blockquote",
    "bulletList",
    "codeBlock",
    "decisionList",
    "extension",
    "heading",
    "mediaGroup",
    "mediaSingle",
    "orderedList",
    "panel",
    "paragraph",
    "rule",
    "taskList",
];

const EXPAND_BLOCK_ATOMS: &[&str] = &[
    "blockCard",
    "blockquote",
    "bulletList",
    "codeBlock",
    "decisionList",
    "embedCard",
    "extension",
    "heading",
    "mediaGroup",
    "mediaSingle",
    "nestedExpand",
    "orderedList",
    "panel",
    "paragraph",
    "rule",
    "table",
    "taskList",
];

const BODIED_EXTENSION_BLOCK_ATOMS: &[&str] = &[
    "blockCard",
    "blockquote",
    "bulletList",
    "codeBlock",
    "decisionList",
    "embedCard",
    "extension",
    "heading",
    "mediaGroup",
    "mediaSingle",
    "orderedList",
    "panel",
    "paragraph",
    "rule",
    "table",
    "taskList",
];

const BODIED_SYNC_BLOCK_ATOMS: &[&str] = &[
    "blockCard",
    "blockquote",
    "bulletList",
    "codeBlock",
    "decisionList",
    "embedCard",
    "expand",
    "heading",
    "layoutSection",
    "mediaGroup",
    "mediaSingle",
    "orderedList",
    "panel",
    "paragraph",
    "rule",
    "table",
    "taskList",
];

const LAYOUT_COLUMN_BLOCK_ATOMS: &[&str] = &[
    "blockCard",
    "blockquote",
    "bodiedExtension",
    "bulletList",
    "codeBlock",
    "decisionList",
    "embedCard",
    "expand",
    "extension",
    "heading",
    "mediaGroup",
    "mediaSingle",
    "orderedList",
    "panel",
    "paragraph",
    "rule",
    "table",
    "taskList",
];

const TABLE_CELL_BLOCK_ATOMS: &[&str] = &[
    "blockCard",
    "blockquote",
    "bulletList",
    "codeBlock",
    "decisionList",
    "embedCard",
    "extension",
    "heading",
    "mediaGroup",
    "mediaSingle",
    "nestedExpand",
    "orderedList",
    "panel",
    "paragraph",
    "rule",
    "taskList",
];

const DOC_BLOCK_ATOMS: &[&str] = &[
    "blockCard",
    "blockquote",
    "bodiedExtension",
    "bodiedSyncBlock",
    "bulletList",
    "codeBlock",
    "decisionList",
    "embedCard",
    "expand",
    "extension",
    "heading",
    "layoutSection",
    "mediaGroup",
    "mediaSingle",
    "orderedList",
    "panel",
    "paragraph",
    "rule",
    "syncBlock",
    "table",
    "taskList",
];

// -----------------------------------------------------------------------------
// Schema entries
// -----------------------------------------------------------------------------
//
// One entry per container node, transcribed faithfully from the upstream JSON
// schema (`json-schema/v1/full.json` in the package tarball pinned by
// `SCHEMA_VERSION` / `UPSTREAM_TARBALL_SHA256`). Leaf nodes (text, hardBreak,
// mention, emoji, date, status, inlineCard, mediaInline, placeholder,
// inlineExtension, media, rule, blockCard, embedCard, extension, syncBlock,
// unsupportedBlock, unsupportedInline) have no `content` upstream and are
// intentionally absent.
//
// `unsupportedBlock` and `unsupportedInline` are NOT listed in any parent's
// allowed-children atoms — they are runtime-only preservation wrappers. The
// walker accepts them under any known parent and counts them toward the
// parent's current term arity; see [`is_unsupported`] and [`walk_children`].
//
// Lenient deviations from upstream (where strict would break common
// real-world inputs) are commented inline:
//
// - `doc`: upstream is `block+`; we use `block*` so `AdfDocument::new()` (the
//   canonical empty document, returned for missing JIRA descriptions) does
//   not produce an arity violation.
// - `tableCell` / `tableHeader`: upstream is `block+`; we use `block*` so
//   visibly-empty cells in real Confluence tables do not produce arity
//   violations.

/// Allowed-children entries, sorted alphabetically by parent. Crate-visible
/// so the drift detector ([`drift`]) can flatten them into a `BTreeMap`
/// without re-deriving from the runtime `ALLOWED_CHILDREN` cache.
pub(crate) type ModelEntry = (&'static str, &'static [ContentTerm]);

pub(crate) const CONTENT_ENTRIES: &[ModelEntry] = &[
    // blockTaskItem — definitions/blockTaskItem_node
    // upstream: (extension | paragraph)+
    (
        "blockTaskItem",
        &[ContentTerm {
            atoms: &["extension", "paragraph"],
            quant: Quantifier::OneOrMore,
        }],
    ),
    // blockquote — definitions/blockquote_node
    // upstream: (paragraph | bulletList | orderedList | mediaGroup |
    //            mediaSingle | codeBlock | extension)+
    (
        "blockquote",
        &[ContentTerm {
            atoms: &[
                "bulletList",
                "codeBlock",
                "extension",
                "mediaGroup",
                "mediaSingle",
                "orderedList",
                "paragraph",
            ],
            quant: Quantifier::OneOrMore,
        }],
    ),
    // bodiedExtension — definitions/bodiedExtension_node
    (
        "bodiedExtension",
        &[ContentTerm {
            atoms: BODIED_EXTENSION_BLOCK_ATOMS,
            quant: Quantifier::OneOrMore,
        }],
    ),
    // bodiedSyncBlock — definitions/bodiedSyncBlock_node
    (
        "bodiedSyncBlock",
        &[ContentTerm {
            atoms: BODIED_SYNC_BLOCK_ATOMS,
            quant: Quantifier::OneOrMore,
        }],
    ),
    // bulletList — definitions/bulletList_node
    // upstream: listItem+
    (
        "bulletList",
        &[ContentTerm {
            atoms: &["listItem"],
            quant: Quantifier::OneOrMore,
        }],
    ),
    // caption — definitions/caption_node
    // upstream: inline* (subset of FULL_INLINE: no inlineExtension, no
    // mediaInline)
    (
        "caption",
        &[ContentTerm {
            atoms: CAPTION_INLINE_ATOMS,
            quant: Quantifier::ZeroOrMore,
        }],
    ),
    // codeBlock — definitions/codeBlock_node
    // upstream: text* (hardBreak NOT permitted by the JSON schema even
    // though some renderers handle it)
    (
        "codeBlock",
        &[ContentTerm {
            atoms: &["text"],
            quant: Quantifier::ZeroOrMore,
        }],
    ),
    // decisionItem — definitions/decisionItem_node
    // upstream: inline*
    (
        "decisionItem",
        &[ContentTerm {
            atoms: FULL_INLINE_ATOMS,
            quant: Quantifier::ZeroOrMore,
        }],
    ),
    // decisionList — definitions/decisionList_node
    // upstream: decisionItem+
    (
        "decisionList",
        &[ContentTerm {
            atoms: &["decisionItem"],
            quant: Quantifier::OneOrMore,
        }],
    ),
    // doc — definitions/doc_node
    // upstream: block+; LENIENT: block* — empty docs are the canonical
    // value for missing JIRA descriptions (`AdfDocument::new()`).
    (
        "doc",
        &[ContentTerm {
            atoms: DOC_BLOCK_ATOMS,
            quant: Quantifier::ZeroOrMore,
        }],
    ),
    // expand — definitions/expand_node
    // upstream: block+ (DOES permit nestedExpand, NOT another expand)
    (
        "expand",
        &[ContentTerm {
            atoms: EXPAND_BLOCK_ATOMS,
            quant: Quantifier::OneOrMore,
        }],
    ),
    // heading — definitions/heading_node
    // upstream: inline*
    (
        "heading",
        &[ContentTerm {
            atoms: FULL_INLINE_ATOMS,
            quant: Quantifier::ZeroOrMore,
        }],
    ),
    // layoutColumn — definitions/layoutColumn_node
    // upstream: block+ (permits expand and bodiedExtension, NOT
    // nestedExpand)
    (
        "layoutColumn",
        &[ContentTerm {
            atoms: LAYOUT_COLUMN_BLOCK_ATOMS,
            quant: Quantifier::OneOrMore,
        }],
    ),
    // layoutSection — definitions/layoutSection_node
    // upstream: layoutColumn{2,3}
    (
        "layoutSection",
        &[ContentTerm {
            atoms: &["layoutColumn"],
            quant: Quantifier::Range(2, 3),
        }],
    ),
    // listItem — definitions/listItem_node
    // upstream: paragraph (paragraph | bulletList | orderedList |
    //                       mediaSingle | codeBlock | taskList)*
    // LENIENT: simplified to (one-or-more of the union) — most listItems
    // start with a paragraph in practice; flagging pure-list-of-list items
    // would be noisy.
    (
        "listItem",
        &[ContentTerm {
            atoms: LISTITEM_BLOCK_ATOMS,
            quant: Quantifier::OneOrMore,
        }],
    ),
    // mediaGroup — definitions/mediaGroup_node
    // upstream: media+
    (
        "mediaGroup",
        &[ContentTerm {
            atoms: &["media"],
            quant: Quantifier::OneOrMore,
        }],
    ),
    // mediaSingle — definitions/mediaSingle_caption_node /
    //               mediaSingle_full_node
    // upstream: media (caption)?  (in this order)
    (
        "mediaSingle",
        &[
            ContentTerm {
                atoms: &["media"],
                quant: Quantifier::Exactly(1),
            },
            ContentTerm {
                atoms: &["caption"],
                quant: Quantifier::ZeroOrOne,
            },
        ],
    ),
    // nestedExpand — definitions/nestedExpand_node
    // upstream: block+ (permits panel and blockquote; NOT table, blockCard,
    // embedCard, expand, or nestedExpand itself)
    (
        "nestedExpand",
        &[ContentTerm {
            atoms: NESTED_EXPAND_BLOCK_ATOMS,
            quant: Quantifier::OneOrMore,
        }],
    ),
    // orderedList — definitions/orderedList_node
    // upstream: listItem+
    (
        "orderedList",
        &[ContentTerm {
            atoms: &["listItem"],
            quant: Quantifier::OneOrMore,
        }],
    ),
    // panel — definitions/panel_node
    // upstream: block+ (subset)
    (
        "panel",
        &[ContentTerm {
            atoms: PANEL_BLOCK_ATOMS,
            quant: Quantifier::OneOrMore,
        }],
    ),
    // paragraph — definitions/paragraph_node
    // upstream: inline*
    (
        "paragraph",
        &[ContentTerm {
            atoms: FULL_INLINE_ATOMS,
            quant: Quantifier::ZeroOrMore,
        }],
    ),
    // table — definitions/table_node
    // upstream: tableRow+
    (
        "table",
        &[ContentTerm {
            atoms: &["tableRow"],
            quant: Quantifier::OneOrMore,
        }],
    ),
    // tableCell — definitions/table_cell_content
    // upstream: block+; LENIENT: block* — visibly-empty cells in real
    // Confluence tables are common and accepted by the renderer.
    (
        "tableCell",
        &[ContentTerm {
            atoms: TABLE_CELL_BLOCK_ATOMS,
            quant: Quantifier::ZeroOrMore,
        }],
    ),
    // tableHeader — definitions/table_header_node (uses table_cell_content)
    // upstream: block+; LENIENT: block* — same reason as tableCell.
    (
        "tableHeader",
        &[ContentTerm {
            atoms: TABLE_CELL_BLOCK_ATOMS,
            quant: Quantifier::ZeroOrMore,
        }],
    ),
    // tableRow — definitions/table_row_node
    // upstream: (tableCell | tableHeader)+
    (
        "tableRow",
        &[ContentTerm {
            atoms: &["tableCell", "tableHeader"],
            quant: Quantifier::OneOrMore,
        }],
    ),
    // taskItem — definitions/taskItem_node
    // upstream: inline*
    (
        "taskItem",
        &[ContentTerm {
            atoms: FULL_INLINE_ATOMS,
            quant: Quantifier::ZeroOrMore,
        }],
    ),
    // taskList — definitions/taskList_node
    // upstream: (taskItem | taskList | blockTaskItem)+
    (
        "taskList",
        &[ContentTerm {
            atoms: &["blockTaskItem", "taskItem", "taskList"],
            quant: Quantifier::OneOrMore,
        }],
    ),
];

/// Forward-compat preservation wrappers. Accepted under any known parent by
/// the walker, regardless of whether the parent's allowed-children set lists
/// them. Atlassian's renderer uses these to wrap content the schema doesn't
/// know how to validate; flagging them as violations would be noisy and would
/// break the round-trip guarantee of [ADR-0020]'s `adf-unsupported` fenced
/// block.
const UNSUPPORTED_NODES: &[&str] = &["unsupportedBlock", "unsupportedInline"];

fn is_unsupported(node_type: &str) -> bool {
    UNSUPPORTED_NODES.contains(&node_type)
}

static CONTENT_MODELS: LazyLock<HashMap<&'static str, &'static [ContentTerm]>> =
    LazyLock::new(|| CONTENT_ENTRIES.iter().copied().collect());

/// Per-parent flattened allowed-children atoms, computed once from
/// [`CONTENT_ENTRIES`] and used by the back-compat [`allowed_children`] /
/// [`permits_child`] helpers. Sorted and deduplicated within each entry.
static ALLOWED_CHILDREN: LazyLock<HashMap<&'static str, Vec<&'static str>>> = LazyLock::new(|| {
    CONTENT_ENTRIES
        .iter()
        .map(|(parent, terms)| {
            let mut atoms: Vec<&'static str> =
                terms.iter().flat_map(|t| t.atoms.iter().copied()).collect();
            atoms.sort_unstable();
            atoms.dedup();
            (*parent, atoms)
        })
        .collect()
});

/// Returns the allowed direct children for a parent node type.
///
/// `None` means the node has no entry in the schema (either a leaf type or a
/// type unknown to this snapshot). Unknown parents are treated permissively
/// by [`permits_child`] and the walker.
///
/// The returned slice is the union of all atoms across the parent's content
/// terms (sorted, deduplicated). Quantifier and order information is not
/// surfaced through this helper — use [`content_model`] for that.
#[must_use]
pub fn allowed_children(parent: &str) -> Option<&'static [&'static str]> {
    ALLOWED_CHILDREN.get(parent).map(Vec::as_slice)
}

/// Returns the full content model (sequence of quantified terms) for a parent
/// node type, or `None` if the parent has no entry.
#[must_use]
pub fn content_model(parent: &str) -> Option<&'static [ContentTerm]> {
    CONTENT_MODELS.get(parent).copied()
}

/// Returns `true` if `child` is permitted as a direct child of `parent`.
///
/// Returns `true` (permissive) when `parent` has no schema entry — see the
/// module-level docs for rationale. Also returns `true` when `child` is
/// `unsupportedBlock` or `unsupportedInline`, regardless of `parent`, because
/// those are forward-compat preservation wrappers, not first-class content.
#[must_use]
pub fn permits_child(parent: &str, child: &str) -> bool {
    if is_unsupported(child) {
        return true;
    }
    match allowed_children(parent) {
        Some(children) => children.contains(&child),
        None => true,
    }
}

/// Validates an entire ADF document and returns all violations found.
///
/// An empty `Vec` means the document is structurally valid against the
/// snapshot. The walker is depth-first: violations under a child are reported
/// after the child's own violations, so the overall ordering is "each parent's
/// own checks, then descend into each child in turn." Arity violations on a
/// parent appear at the position the parent is visited, before any of its
/// descendants' violations.
#[must_use]
pub fn validate_document(doc: &AdfDocument) -> Vec<AdfSchemaViolation> {
    let mut violations = Vec::new();
    let mut path = Vec::new();
    if let Some(model) = content_model(&doc.doc_type) {
        walk_children(
            &doc.content,
            &doc.doc_type,
            model,
            &mut path,
            &mut violations,
        );
    }
    violations
}

/// Walks `children` against `model`, reporting `DisallowedChild` and `Arity`
/// violations into `out`. Recurses into each child's subtree if its
/// `node_type` has a schema entry.
///
/// `path` is the index path from the document root to the **parent** of
/// `children` (i.e. the index of the current child is pushed/popped inside the
/// loop). On entry, `path` identifies the parent; on exit it is unchanged.
fn walk_children(
    children: &[AdfNode],
    parent_type: &str,
    model: &[ContentTerm],
    path: &mut Vec<usize>,
    out: &mut Vec<AdfSchemaViolation>,
) {
    // Per-term match counts. Index aligned with `model`.
    let mut term_counts: Vec<usize> = vec![0; model.len()];
    // Index of the term we are currently consuming children into. Advances
    // monotonically — children that don't match the current term try later
    // terms, but we never go backwards (this matches ProseMirror's greedy
    // sequence-matching semantics).
    let mut current_term: usize = 0;

    for (idx, child) in children.iter().enumerate() {
        path.push(idx);

        let child_type = child.node_type.as_str();

        // Validate this child's attrs (per PR #733-attrs slice). Permissive
        // on unknown node types; emits `MissingAttr` / `InvalidAttr` for
        // declared fields. Always runs — independent of disallowed-child
        // / arity bookkeeping.
        crate::atlassian::adf_attr_schema::validate_attrs(
            child_type,
            child.attrs.as_ref(),
            path,
            out,
        );

        // Validate this child's marks (per PR #733-marks slice). The
        // `parent_type` is the node enclosing `child` — it determines the
        // inline-mark allow-list when `child` is an inline node like text.
        // Permissive on unknown contexts.
        crate::atlassian::adf_mark_schema::validate_marks(parent_type, child, path, out);

        if is_unsupported(child_type) {
            // Round-trip escape hatch: count toward the current term's arity
            // (so a panel containing only an `unsupportedBlock` still
            // satisfies panel's `+`). Never emits a DisallowedChild.
            if current_term < model.len() {
                term_counts[current_term] += 1;
            }
        } else {
            // Find a term (at or after current_term) whose atoms accept this
            // child. Greedy: first match wins; subsequent children continue
            // from the matched term.
            let mut matched: Option<usize> = None;
            let mut try_idx = current_term;
            while try_idx < model.len() {
                if model[try_idx].atoms.contains(&child_type) {
                    matched = Some(try_idx);
                    break;
                }
                try_idx += 1;
            }

            match matched {
                Some(t) => {
                    term_counts[t] += 1;
                    current_term = t;
                }
                None => {
                    out.push(AdfSchemaViolation::DisallowedChild {
                        child_type: child_type.to_string(),
                        parent_type: parent_type.to_string(),
                        path: path.clone(),
                    });
                    // Don't count toward any term — see the doc on
                    // `Arity` for why disallowed children should not satisfy
                    // arity for the parent (the user clearly tried to put a
                    // child here, but it's the wrong type — the right thing
                    // is to flag both DisallowedChild and any missing Arity).
                }
            }
        }

        // Recurse into the child's content if it has a known schema. Treat
        // a missing `content` field as an empty content array so that arity
        // checks still fire for empty containers (`AdfNode::content: None`
        // is how the converter encodes "no children").
        if let Some(grand_model) = content_model(child_type) {
            let grand = child.content.as_deref().unwrap_or(&[]);
            walk_children(grand, child_type, grand_model, path, out);
        }

        path.pop();
    }

    // After consuming children, emit one arity violation per term whose count
    // doesn't satisfy its quantifier. Path here points at the parent (one
    // level up from the children we just walked).
    for (i, term) in model.iter().enumerate() {
        let count = term_counts[i];
        if !term.quant.satisfied_by(count) {
            out.push(AdfSchemaViolation::Arity {
                parent_type: parent_type.to_string(),
                atoms: term.atoms.to_vec(),
                expected: term.quant.clone(),
                actual: count,
                path: path.clone(),
            });
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::adf::{AdfDocument, AdfNode};

    fn node(node_type: &str, content: Vec<AdfNode>) -> AdfNode {
        AdfNode {
            node_type: node_type.to_string(),
            attrs: None,
            content: if content.is_empty() {
                None
            } else {
                Some(content)
            },
            text: None,
            marks: None,
            local_id: None,
            parameters: None,
        }
    }

    fn leaf(node_type: &str) -> AdfNode {
        node(node_type, vec![])
    }

    fn with_attrs(mut n: AdfNode, attrs: serde_json::Value) -> AdfNode {
        n.attrs = Some(attrs);
        n
    }

    /// `panel` with a valid `panelType` so attribute validation does not
    /// add noise to tests focused on content-model behaviour.
    fn panel(content: Vec<AdfNode>) -> AdfNode {
        with_attrs(
            node("panel", content),
            serde_json::json!({"panelType": "info"}),
        )
    }

    /// `media` with a valid `type`.
    fn media() -> AdfNode {
        with_attrs(
            leaf("media"),
            serde_json::json!({"type": "file", "id": "x"}),
        )
    }

    /// `layoutColumn` with a valid `width`.
    fn layout_column(content: Vec<AdfNode>) -> AdfNode {
        with_attrs(
            node("layoutColumn", content),
            serde_json::json!({"width": 33.3}),
        )
    }

    fn doc(content: Vec<AdfNode>) -> AdfDocument {
        AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content,
        }
    }

    fn unwrap_disallowed(v: &AdfSchemaViolation) -> (&str, &str, &[usize]) {
        match v {
            AdfSchemaViolation::DisallowedChild {
                child_type,
                parent_type,
                path,
            } => (child_type.as_str(), parent_type.as_str(), path.as_slice()),
            other => panic!("expected DisallowedChild, got {other:?}"),
        }
    }

    fn unwrap_arity(
        v: &AdfSchemaViolation,
    ) -> (&str, &[&'static str], &Quantifier, usize, &[usize]) {
        match v {
            AdfSchemaViolation::Arity {
                parent_type,
                atoms,
                expected,
                actual,
                path,
            } => (
                parent_type.as_str(),
                atoms.as_slice(),
                expected,
                *actual,
                path.as_slice(),
            ),
            other => panic!("expected Arity, got {other:?}"),
        }
    }

    #[test]
    fn schema_has_entry_for_every_advertised_container() {
        let known_leaves = [
            "blockCard",
            "date",
            "embedCard",
            "emoji",
            "extension",
            "hardBreak",
            "inlineCard",
            "inlineExtension",
            "media",
            "mediaInline",
            "mention",
            "placeholder",
            "rule",
            "status",
            "syncBlock",
            "text",
            "unsupportedBlock",
            "unsupportedInline",
        ];
        for (_parent, terms) in CONTENT_ENTRIES {
            for term in *terms {
                for child in term.atoms {
                    let known = CONTENT_MODELS.contains_key(child) || known_leaves.contains(child);
                    assert!(
                        known,
                        "child '{child}' has no schema entry and is not in the leaf list"
                    );
                }
            }
        }
    }

    #[test]
    fn child_lists_are_sorted_for_diffability() {
        for (parent, terms) in CONTENT_ENTRIES {
            for term in *terms {
                let mut sorted = term.atoms.to_vec();
                sorted.sort_unstable();
                assert_eq!(
                    term.atoms.to_vec(),
                    sorted,
                    "atom list for '{parent}' is not sorted"
                );
            }
        }
    }

    // ---- Issue #717 examples ---------------------------------------------

    #[test]
    fn panel_allows_examples_from_issue_717() {
        for child in [
            "paragraph",
            "heading",
            "bulletList",
            "orderedList",
            "blockCard",
            "mediaGroup",
            "mediaSingle",
            "codeBlock",
            "taskList",
            "rule",
            "decisionList",
            "unsupportedBlock",
            "extension",
        ] {
            assert!(
                permits_child("panel", child),
                "panel should permit '{child}'"
            );
        }
    }

    #[test]
    fn panel_rejects_expand_and_nested_expand() {
        assert!(!permits_child("panel", "expand"));
        assert!(!permits_child("panel", "nestedExpand"));
    }

    #[test]
    fn expand_allows_nested_block_types_and_nested_expand_but_not_self() {
        assert!(permits_child("expand", "panel"));
        assert!(permits_child("expand", "table"));
        assert!(permits_child("expand", "nestedExpand"));
        assert!(!permits_child("expand", "expand"));
    }

    #[test]
    fn table_cell_allows_nested_expand_but_not_expand() {
        assert!(permits_child("tableCell", "nestedExpand"));
        assert!(!permits_child("tableCell", "expand"));
    }

    #[test]
    fn blockquote_allowed_children_match_upstream_json_schema() {
        let expected = [
            "bulletList",
            "codeBlock",
            "extension",
            "mediaGroup",
            "mediaSingle",
            "orderedList",
            "paragraph",
        ];
        let got: Vec<&str> = allowed_children("blockquote")
            .expect("blockquote has an entry")
            .to_vec();
        assert_eq!(got, expected);
    }

    // ---- Permissiveness invariants ---------------------------------------

    #[test]
    fn unknown_parent_is_permissive() {
        assert!(permits_child("madeUpNode", "anything"));
        assert!(permits_child("madeUpNode", "alsoFake"));
    }

    #[test]
    fn unknown_child_inside_known_parent_is_a_violation() {
        assert!(!permits_child("paragraph", "madeUpInline"));
    }

    #[test]
    fn nested_expand_distinguished_from_expand() {
        assert!(permits_child("nestedExpand", "panel"));
        assert!(permits_child("nestedExpand", "blockquote"));
        assert!(!permits_child("nestedExpand", "table"));
        assert!(!permits_child("nestedExpand", "blockCard"));
        assert!(!permits_child("nestedExpand", "embedCard"));
        assert!(!permits_child("nestedExpand", "nestedExpand"));
        assert!(!permits_child("nestedExpand", "expand"));
    }

    // ---- Walker behaviour: existing v1 cases -----------------------------

    #[test]
    fn validate_succeeds_on_known_good_doc() {
        let document = doc(vec![
            AdfNode::paragraph(vec![AdfNode::text("hello")]),
            AdfNode::heading(2, vec![AdfNode::text("world")]),
        ]);
        assert_eq!(validate_document(&document), vec![]);
    }

    #[test]
    fn validate_finds_expand_inside_panel() {
        // panel with [expand]: emits DisallowedChild for the expand AND an
        // Arity violation for the panel (panel needs 1+ valid children;
        // disallowed children do not satisfy arity).
        let bad_panel = panel(vec![with_attrs(
            node("expand", vec![AdfNode::paragraph(vec![])]),
            serde_json::json!({"title": "x"}),
        )]);
        let document = doc(vec![bad_panel]);

        let violations = validate_document(&document);
        let disallowed: Vec<_> = violations
            .iter()
            .filter(|v| matches!(v, AdfSchemaViolation::DisallowedChild { .. }))
            .collect();
        let arity: Vec<_> = violations
            .iter()
            .filter(|v| matches!(v, AdfSchemaViolation::Arity { .. }))
            .collect();

        assert_eq!(disallowed.len(), 1, "got: {violations:?}");
        let (child, parent, path) = unwrap_disallowed(disallowed[0]);
        assert_eq!(child, "expand");
        assert_eq!(parent, "panel");
        assert_eq!(path, [0, 0]);

        assert_eq!(arity.len(), 1, "got: {violations:?}");
        let (parent, _, _, actual, path) = unwrap_arity(arity[0]);
        assert_eq!(parent, "panel");
        assert_eq!(actual, 0);
        assert_eq!(path, [0]);
    }

    #[test]
    fn validate_finds_expand_inside_table_cell() {
        let bad_cell = node(
            "tableCell",
            vec![with_attrs(
                node("expand", vec![AdfNode::paragraph(vec![])]),
                serde_json::json!({"title": "x"}),
            )],
        );
        let row = node("tableRow", vec![bad_cell]);
        let table = node("table", vec![row]);
        let document = doc(vec![table]);

        let violations = validate_document(&document);
        let disallowed: Vec<_> = violations
            .iter()
            .filter(|v| matches!(v, AdfSchemaViolation::DisallowedChild { .. }))
            .collect();
        assert_eq!(disallowed.len(), 1, "got: {violations:?}");
        let (child, parent, path) = unwrap_disallowed(disallowed[0]);
        assert_eq!(child, "expand");
        assert_eq!(parent, "tableCell");
        assert_eq!(path, [0, 0, 0, 0]);
    }

    #[test]
    fn validate_walks_into_nested_violations_in_document_order() {
        let document = doc(vec![
            AdfNode::paragraph(vec![leaf("rule")]),
            panel(vec![with_attrs(
                node("expand", vec![AdfNode::paragraph(vec![])]),
                serde_json::json!({"title": "x"}),
            )]),
        ]);

        let violations = validate_document(&document);
        // First violation: rule inside paragraph (DisallowedChild).
        // Then: panel's DisallowedChild for expand, panel's Arity (0 valid).
        // (Inline-content of paragraph #0 has no further descent because rule
        // is a leaf.)
        let first = violations.first().expect("at least one");
        let (child, parent, _) = unwrap_disallowed(first);
        assert_eq!(child, "rule");
        assert_eq!(parent, "paragraph");
    }

    #[test]
    fn validate_is_permissive_under_unknown_parents() {
        let document = doc(vec![node("futureBlock", vec![node("expand", vec![])])]);
        let violations = validate_document(&document);
        // futureBlock is not in `doc`'s allowed atoms → DisallowedChild.
        // Since `doc` is `*` (lenient), no Arity violation.
        // futureBlock's subtree is not walked (unknown parent).
        assert_eq!(violations.len(), 1);
        let (child, parent, _) = unwrap_disallowed(&violations[0]);
        assert_eq!(child, "futureBlock");
        assert_eq!(parent, "doc");
    }

    #[test]
    fn unsupported_block_is_universally_accepted_via_walker_escape_hatch() {
        for parent in [
            "doc",
            "panel",
            "expand",
            "tableCell",
            "blockquote",
            "listItem",
        ] {
            assert!(
                permits_child(parent, "unsupportedBlock"),
                "{parent} should permit unsupportedBlock via the escape hatch"
            );
            assert!(
                !allowed_children(parent).is_some_and(|c| c.contains(&"unsupportedBlock")),
                "{parent}'s allowed-children list must not list unsupportedBlock — \
                 acceptance comes from the walker escape hatch only"
            );
        }
    }

    #[test]
    fn unsupported_inline_is_universally_accepted_via_walker_escape_hatch() {
        for parent in [
            "paragraph",
            "heading",
            "taskItem",
            "decisionItem",
            "caption",
        ] {
            assert!(
                permits_child(parent, "unsupportedInline"),
                "{parent} should permit unsupportedInline via the escape hatch"
            );
            assert!(
                !allowed_children(parent).is_some_and(|c| c.contains(&"unsupportedInline")),
                "{parent}'s allowed-children list must not list unsupportedInline"
            );
        }
    }

    #[test]
    fn validate_returns_empty_when_doc_type_is_unknown() {
        let document = AdfDocument {
            version: 1,
            doc_type: "futureRoot".to_string(),
            content: vec![node("expand", vec![])],
        };
        assert_eq!(validate_document(&document), vec![]);
    }

    #[test]
    fn walker_does_not_flag_unsupported_block_inside_panel() {
        // Panel contains only an unsupportedBlock: counts toward panel's
        // arity (so no Arity violation), and the wrapper is universally
        // accepted. Should validate cleanly.
        let document = doc(vec![panel(vec![leaf("unsupportedBlock")])]);
        assert_eq!(validate_document(&document), vec![]);
    }

    // ---- Walker behaviour: arity (PR #733) -------------------------------

    #[test]
    fn empty_bullet_list_flagged_as_arity_violation() {
        let document = doc(vec![node("bulletList", vec![])]);
        let violations = validate_document(&document);
        assert_eq!(violations.len(), 1, "got: {violations:?}");
        let (parent, atoms, expected, actual, path) = unwrap_arity(&violations[0]);
        assert_eq!(parent, "bulletList");
        assert_eq!(atoms, &["listItem"]);
        assert_eq!(expected, &Quantifier::OneOrMore);
        assert_eq!(actual, 0);
        assert_eq!(path, [0]);
    }

    #[test]
    fn media_single_with_two_media_flagged_as_arity_violation() {
        // mediaSingle requires exactly one media; two media → Arity (too many).
        let media_single = node("mediaSingle", vec![media(), media()]);
        let document = doc(vec![media_single]);
        let violations = validate_document(&document);

        let arity: Vec<_> = violations
            .iter()
            .filter(|v| matches!(v, AdfSchemaViolation::Arity { .. }))
            .collect();
        assert_eq!(arity.len(), 1, "got: {violations:?}");
        let (parent, atoms, expected, actual, _) = unwrap_arity(arity[0]);
        assert_eq!(parent, "mediaSingle");
        assert_eq!(atoms, &["media"]);
        assert_eq!(expected, &Quantifier::Exactly(1));
        assert_eq!(actual, 2);
    }

    #[test]
    fn media_single_with_only_caption_flagged_missing_media() {
        // mediaSingle: media (caption)? — with [caption] alone, media is
        // missing AND caption is out-of-position. We currently emit only the
        // missing-media Arity (caption matches term 1 successfully).
        let document = doc(vec![node("mediaSingle", vec![leaf("caption")])]);
        let violations = validate_document(&document);
        let arity: Vec<_> = violations
            .iter()
            .filter(|v| matches!(v, AdfSchemaViolation::Arity { .. }))
            .collect();
        assert_eq!(arity.len(), 1, "got: {violations:?}");
        let (parent, atoms, expected, actual, _) = unwrap_arity(arity[0]);
        assert_eq!(parent, "mediaSingle");
        assert_eq!(atoms, &["media"]);
        assert_eq!(expected, &Quantifier::Exactly(1));
        assert_eq!(actual, 0);
    }

    #[test]
    fn media_single_with_media_then_caption_validates() {
        let document = doc(vec![node(
            "mediaSingle",
            vec![media(), node("caption", vec![AdfNode::text("c")])],
        )]);
        assert_eq!(validate_document(&document), vec![]);
    }

    #[test]
    fn media_single_with_just_one_media_validates() {
        let document = doc(vec![node("mediaSingle", vec![media()])]);
        assert_eq!(validate_document(&document), vec![]);
    }

    #[test]
    fn empty_table_row_flagged_arity() {
        let document = doc(vec![node("table", vec![node("tableRow", vec![])])]);
        let violations = validate_document(&document);
        let arity: Vec<_> = violations
            .iter()
            .filter(|v| matches!(v, AdfSchemaViolation::Arity { .. }))
            .collect();
        assert_eq!(arity.len(), 1, "got: {violations:?}");
        let (parent, atoms, expected, actual, _) = unwrap_arity(arity[0]);
        assert_eq!(parent, "tableRow");
        assert_eq!(atoms, &["tableCell", "tableHeader"]);
        assert_eq!(expected, &Quantifier::OneOrMore);
        assert_eq!(actual, 0);
    }

    #[test]
    fn empty_media_group_flagged_arity() {
        let document = doc(vec![node("mediaGroup", vec![])]);
        let violations = validate_document(&document);
        assert_eq!(violations.len(), 1);
        let (parent, atoms, expected, actual, _) = unwrap_arity(&violations[0]);
        assert_eq!(parent, "mediaGroup");
        assert_eq!(atoms, &["media"]);
        assert_eq!(expected, &Quantifier::OneOrMore);
        assert_eq!(actual, 0);
    }

    #[test]
    fn layout_section_with_one_column_flagged_arity_range() {
        let document = doc(vec![node(
            "layoutSection",
            vec![node(
                "layoutColumn",
                vec![AdfNode::paragraph(vec![AdfNode::text("a")])],
            )],
        )]);
        let violations = validate_document(&document);
        let arity: Vec<_> = violations
            .iter()
            .filter(|v| matches!(v, AdfSchemaViolation::Arity { .. }))
            .collect();
        assert_eq!(arity.len(), 1, "got: {violations:?}");
        let (parent, atoms, expected, actual, _) = unwrap_arity(arity[0]);
        assert_eq!(parent, "layoutSection");
        assert_eq!(atoms, &["layoutColumn"]);
        assert_eq!(expected, &Quantifier::Range(2, 3));
        assert_eq!(actual, 1);
    }

    #[test]
    fn layout_section_with_three_columns_validates() {
        let column = || layout_column(vec![AdfNode::paragraph(vec![AdfNode::text("x")])]);
        let document = doc(vec![node(
            "layoutSection",
            vec![column(), column(), column()],
        )]);
        assert_eq!(validate_document(&document), vec![]);
    }

    #[test]
    fn layout_section_with_four_columns_flagged_too_many() {
        let column = || layout_column(vec![AdfNode::paragraph(vec![AdfNode::text("x")])]);
        let document = doc(vec![node(
            "layoutSection",
            vec![column(), column(), column(), column()],
        )]);
        let violations = validate_document(&document);
        let arity: Vec<_> = violations
            .iter()
            .filter(|v| matches!(v, AdfSchemaViolation::Arity { .. }))
            .collect();
        assert_eq!(arity.len(), 1, "got: {violations:?}");
        let (_, _, expected, actual, _) = unwrap_arity(arity[0]);
        assert_eq!(expected, &Quantifier::Range(2, 3));
        assert_eq!(actual, 4);
    }

    #[test]
    fn empty_paragraph_validates_under_lenient_inline_star() {
        let document = doc(vec![AdfNode::paragraph(vec![])]);
        assert_eq!(validate_document(&document), vec![]);
    }

    #[test]
    fn empty_doc_validates_under_lenient_block_star() {
        let document = doc(vec![]);
        assert_eq!(validate_document(&document), vec![]);
    }

    #[test]
    fn empty_table_cell_validates_under_lenient_block_star() {
        let document = doc(vec![node(
            "table",
            vec![node("tableRow", vec![node("tableCell", vec![])])],
        )]);
        assert_eq!(validate_document(&document), vec![]);
    }

    #[test]
    fn empty_panel_flagged_arity() {
        let document = doc(vec![panel(vec![])]);
        let violations = validate_document(&document);
        assert_eq!(violations.len(), 1, "got: {violations:?}");
        let (parent, _, expected, actual, _) = unwrap_arity(&violations[0]);
        assert_eq!(parent, "panel");
        assert_eq!(expected, &Quantifier::OneOrMore);
        assert_eq!(actual, 0);
    }

    #[test]
    fn unsupported_block_satisfies_parent_arity() {
        // panel + with [unsupportedBlock] → no violation (round-trip
        // preservation: the wrapper counts toward panel's arity).
        let document = doc(vec![panel(vec![leaf("unsupportedBlock")])]);
        assert_eq!(validate_document(&document), vec![]);
    }

    #[test]
    fn unsupported_inline_satisfies_inline_parent_arity() {
        // taskItem is `inline*` (lenient), so this is trivially OK; the
        // assertion is that we don't reject the unsupportedInline. Both
        // taskList and taskItem need a localId; taskItem also needs a
        // state.
        let task_item = with_attrs(
            node("taskItem", vec![leaf("unsupportedInline")]),
            serde_json::json!({"localId": "ti1", "state": "TODO"}),
        );
        let task_list = with_attrs(
            node("taskList", vec![task_item]),
            serde_json::json!({"localId": "tl1"}),
        );
        let document = doc(vec![task_list]);
        assert_eq!(validate_document(&document), vec![]);
    }

    // ---- Display formatting ----------------------------------------------

    #[test]
    fn display_format_for_disallowed_child_is_back_compat() {
        let v = AdfSchemaViolation::DisallowedChild {
            child_type: "expand".into(),
            parent_type: "panel".into(),
            path: vec![0, 1, 0],
        };
        assert_eq!(
            v.to_string(),
            "ADF schema violation at /0/1/0: 'expand' is not permitted inside 'panel'"
        );
    }

    #[test]
    fn display_format_for_arity_one_or_more() {
        let v = AdfSchemaViolation::Arity {
            parent_type: "bulletList".into(),
            atoms: vec!["listItem"],
            expected: Quantifier::OneOrMore,
            actual: 0,
            path: vec![1],
        };
        assert_eq!(
            v.to_string(),
            "ADF schema violation at /1: 'bulletList' must contain at least one 'listItem' (found 0)"
        );
    }

    #[test]
    fn display_format_for_arity_exactly_one() {
        let v = AdfSchemaViolation::Arity {
            parent_type: "mediaSingle".into(),
            atoms: vec!["media"],
            expected: Quantifier::Exactly(1),
            actual: 2,
            path: vec![0],
        };
        assert_eq!(
            v.to_string(),
            "ADF schema violation at /0: 'mediaSingle' must contain exactly one 'media' (found 2)"
        );
    }

    #[test]
    fn display_format_for_arity_range() {
        let v = AdfSchemaViolation::Arity {
            parent_type: "layoutSection".into(),
            atoms: vec!["layoutColumn"],
            expected: Quantifier::Range(2, 3),
            actual: 1,
            path: vec![0],
        };
        assert_eq!(
            v.to_string(),
            "ADF schema violation at /0: 'layoutSection' must contain between 2 and 3 'layoutColumn' (found 1)"
        );
    }

    #[test]
    fn display_format_for_arity_alternation() {
        let v = AdfSchemaViolation::Arity {
            parent_type: "tableRow".into(),
            atoms: vec!["tableCell", "tableHeader"],
            expected: Quantifier::OneOrMore,
            actual: 0,
            path: vec![0, 0],
        };
        assert_eq!(
            v.to_string(),
            "ADF schema violation at /0/0: 'tableRow' must contain at least one {'tableCell', 'tableHeader'} (found 0)"
        );
    }

    #[test]
    fn display_format_for_missing_attr() {
        let v = AdfSchemaViolation::MissingAttr {
            node_type: "panel".into(),
            attr_name: "panelType".into(),
            path: vec![0],
        };
        assert_eq!(
            v.to_string(),
            "ADF schema violation at /0: 'panel' is missing required attribute 'panelType'"
        );
    }

    #[test]
    fn display_format_for_invalid_attr() {
        let v = AdfSchemaViolation::InvalidAttr {
            node_type: "heading".into(),
            attr_name: "level".into(),
            problem: crate::atlassian::adf_attr_schema::AttrProblem::OutOfRange {
                lo: 1,
                hi: 6,
                actual: 7,
            },
            path: vec![0],
        };
        let s = v.to_string();
        assert!(s.contains("'heading.level'"), "got: {s}");
        assert!(s.contains("invalid"), "got: {s}");
        assert!(s.contains("[1, 6]"), "got: {s}");
    }

    #[test]
    fn display_format_for_disallowed_mark() {
        let v = AdfSchemaViolation::DisallowedMark {
            mark_type: "code".into(),
            parent_type: "heading".into(),
            inline_index: Some(0),
            path: vec![0, 1],
        };
        assert_eq!(
            v.to_string(),
            "ADF schema violation at /0/1: 'code' mark is not permitted on 'heading'"
        );
    }

    #[test]
    fn display_format_for_invalid_mark_attr() {
        let v = AdfSchemaViolation::InvalidMarkAttr {
            mark_type: "link".into(),
            attr_name: "href".into(),
            problem: crate::atlassian::adf_attr_schema::AttrProblem::BadFormat {
                reason: "not a valid URL",
            },
            inline_index: Some(0),
            path: vec![0, 1],
        };
        let s = v.to_string();
        assert!(s.contains("'link' mark"), "got: {s}");
        assert!(s.contains("'href'"), "got: {s}");
        assert!(s.contains("not a valid URL"), "got: {s}");
    }

    // ---- Quantifier behaviour --------------------------------------------

    #[test]
    fn quantifier_satisfied_by() {
        assert!(Quantifier::ZeroOrOne.satisfied_by(0));
        assert!(Quantifier::ZeroOrOne.satisfied_by(1));
        assert!(!Quantifier::ZeroOrOne.satisfied_by(2));

        assert!(Quantifier::ZeroOrMore.satisfied_by(0));
        assert!(Quantifier::ZeroOrMore.satisfied_by(99));

        assert!(!Quantifier::OneOrMore.satisfied_by(0));
        assert!(Quantifier::OneOrMore.satisfied_by(1));

        assert!(!Quantifier::Exactly(2).satisfied_by(1));
        assert!(Quantifier::Exactly(2).satisfied_by(2));
        assert!(!Quantifier::Exactly(2).satisfied_by(3));

        assert!(!Quantifier::Range(2, 3).satisfied_by(1));
        assert!(Quantifier::Range(2, 3).satisfied_by(2));
        assert!(Quantifier::Range(2, 3).satisfied_by(3));
        assert!(!Quantifier::Range(2, 3).satisfied_by(4));
    }

    // ── Quantifier::phrasing arm coverage ─────────────────────────────
    //
    // Each variant has its own phrasing fragment used in
    // `AdfSchemaViolation::Arity`'s Display. The fixture-driven Display
    // tests above only exercise OneOrMore, Exactly(1), and Range; cover
    // the remaining arms (ZeroOrOne, ZeroOrMore, Exactly(n>1)) here so
    // future renumbering of the Display wording is caught.

    #[test]
    fn display_format_for_arity_zero_or_one() {
        let v = AdfSchemaViolation::Arity {
            parent_type: "mediaSingle".into(),
            atoms: vec!["caption"],
            expected: Quantifier::ZeroOrOne,
            actual: 2,
            path: vec![0],
        };
        assert_eq!(
            v.to_string(),
            "ADF schema violation at /0: 'mediaSingle' must contain at most one 'caption' (found 2)"
        );
    }

    #[test]
    fn display_format_for_arity_zero_or_more() {
        // ZeroOrMore is never violated (any count is OK), so the Arity
        // variant with ZeroOrMore is not produced by the walker. Construct
        // directly to exercise the Display arm.
        let v = AdfSchemaViolation::Arity {
            parent_type: "paragraph".into(),
            atoms: vec!["text"],
            expected: Quantifier::ZeroOrMore,
            actual: 0,
            path: vec![0],
        };
        assert_eq!(
            v.to_string(),
            "ADF schema violation at /0: 'paragraph' must contain any number of 'text' (found 0)"
        );
    }

    #[test]
    fn display_format_for_arity_exactly_n_greater_than_one() {
        let v = AdfSchemaViolation::Arity {
            parent_type: "futureNode".into(),
            atoms: vec!["child"],
            expected: Quantifier::Exactly(3),
            actual: 2,
            path: vec![0],
        };
        assert_eq!(
            v.to_string(),
            "ADF schema violation at /0: 'futureNode' must contain exactly 3 'child' (found 2)"
        );
    }

    // ── path() accessor: every variant returns its path ─────────────────
    //
    // The match in `path()` uses an or-pattern `A | B | C => path`, so
    // each arm needs to be exercised separately to count as covered.

    #[test]
    fn path_accessor_returns_path_for_each_variant() {
        let v1 = AdfSchemaViolation::DisallowedChild {
            child_type: "x".into(),
            parent_type: "y".into(),
            path: vec![1],
        };
        assert_eq!(v1.path(), &[1]);

        let v2 = AdfSchemaViolation::Arity {
            parent_type: "y".into(),
            atoms: vec!["x"],
            expected: Quantifier::OneOrMore,
            actual: 0,
            path: vec![2],
        };
        assert_eq!(v2.path(), &[2]);

        let v3 = AdfSchemaViolation::MissingAttr {
            node_type: "y".into(),
            attr_name: "a".into(),
            path: vec![3],
        };
        assert_eq!(v3.path(), &[3]);

        let v4 = AdfSchemaViolation::InvalidAttr {
            node_type: "y".into(),
            attr_name: "a".into(),
            problem: crate::atlassian::adf_attr_schema::AttrProblem::WrongType {
                expected: "string",
            },
            path: vec![4],
        };
        assert_eq!(v4.path(), &[4]);

        let v5 = AdfSchemaViolation::DisallowedMark {
            mark_type: "code".into(),
            parent_type: "heading".into(),
            inline_index: Some(0),
            path: vec![5],
        };
        assert_eq!(v5.path(), &[5]);

        let v6 = AdfSchemaViolation::InvalidMarkAttr {
            mark_type: "link".into(),
            attr_name: "href".into(),
            problem: crate::atlassian::adf_attr_schema::AttrProblem::BadFormat {
                reason: "not a valid URL",
            },
            inline_index: Some(0),
            path: vec![6],
        };
        assert_eq!(v6.path(), &[6]);
    }

    /// Allowlist entry: `(parent, upstream_extra_atoms, local_extra_atoms,
    /// justification)`. See [`LENIENCY_ALLOWLIST`] below.
    type LenientEntry = (
        &'static str,
        &'static [&'static str],
        &'static [&'static str],
        &'static str,
    );

    /// Result of comparing the local and upstream atom maps.
    #[derive(Debug, Default)]
    struct SchemaAtomDiff {
        /// Parents in `CONTENT_ENTRIES` but not in `UPSTREAM_ENTRIES`.
        local_only_parents: Vec<&'static str>,
        /// Parents in `UPSTREAM_ENTRIES` but not in `CONTENT_ENTRIES`.
        upstream_only_parents: Vec<&'static str>,
        /// Human-readable per-parent atom-set mismatches that are not
        /// covered by the leniency allowlist.
        per_parent_unexpected: Vec<String>,
    }

    impl SchemaAtomDiff {
        fn is_clean(&self) -> bool {
            self.local_only_parents.is_empty()
                && self.upstream_only_parents.is_empty()
                && self.per_parent_unexpected.is_empty()
        }
    }

    /// Pure helper: diff two `BTreeMap<&str, BTreeSet<&str>>` views of the
    /// schema, accounting for an allowlist of intentional leniencies.
    ///
    /// Extracted from `generated_upstream_atoms_match_local_snapshot` so the
    /// failure-detection branches can be exercised by tests with synthetic
    /// inputs (the production maps are intentionally in sync).
    fn diff_atom_sets(
        local: &std::collections::BTreeMap<&'static str, std::collections::BTreeSet<&'static str>>,
        upstream: &std::collections::BTreeMap<
            &'static str,
            std::collections::BTreeSet<&'static str>,
        >,
        leniency: &[LenientEntry],
    ) -> SchemaAtomDiff {
        let local_parents: std::collections::BTreeSet<&'static str> =
            local.keys().copied().collect();
        let upstream_parents: std::collections::BTreeSet<&'static str> =
            upstream.keys().copied().collect();

        let mut diff = SchemaAtomDiff {
            local_only_parents: local_parents
                .difference(&upstream_parents)
                .copied()
                .collect(),
            upstream_only_parents: upstream_parents
                .difference(&local_parents)
                .copied()
                .collect(),
            per_parent_unexpected: Vec::new(),
        };

        for parent in local_parents.intersection(&upstream_parents) {
            let l = &local[parent];
            let u = &upstream[parent];

            let allowed_upstream_extra: std::collections::BTreeSet<&str> = leniency
                .iter()
                .filter(|(p, _, _, _)| p == parent)
                .flat_map(|(_, ue, _, _)| ue.iter().copied())
                .collect();
            let allowed_local_extra: std::collections::BTreeSet<&str> = leniency
                .iter()
                .filter(|(p, _, _, _)| p == parent)
                .flat_map(|(_, _, le, _)| le.iter().copied())
                .collect();

            let upstream_extra: Vec<&str> = u
                .iter()
                .filter(|c| !l.contains(**c) && !allowed_upstream_extra.contains(**c))
                .copied()
                .collect();
            let local_extra: Vec<&str> = l
                .iter()
                .filter(|c| !u.contains(**c) && !allowed_local_extra.contains(**c))
                .copied()
                .collect();

            if !upstream_extra.is_empty() || !local_extra.is_empty() {
                diff.per_parent_unexpected.push(format!(
                    "{parent}: upstream_only={upstream_extra:?}, local_only={local_extra:?}"
                ));
            }
        }

        diff
    }

    /// Intentional atom-set leniencies. Each entry is `(parent,
    /// upstream_extra_atoms, local_extra_atoms, justification)`. Keep
    /// synchronised with the "LENIENT" comments in [`CONTENT_ENTRIES`].
    ///
    /// All currently-documented leniencies are *quantifier-only* (e.g.
    /// `block+` → `block*`), so the atom sets remain identical and this
    /// table is empty. If a future leniency adds or drops atoms (e.g.
    /// narrowing `listItem` to forbid `taskList`), record it here.
    const LENIENCY_ALLOWLIST: &[LenientEntry] = &[];

    /// Build the upstream `BTreeMap` view from `generated::UPSTREAM_ENTRIES`.
    fn upstream_atom_map(
    ) -> std::collections::BTreeMap<&'static str, std::collections::BTreeSet<&'static str>> {
        generated::UPSTREAM_ENTRIES
            .iter()
            .map(|(p, children)| (*p, children.iter().copied().collect()))
            .collect()
    }

    /// Issue #732 — the code-generated upstream-atom snapshot must agree with
    /// the hand-maintained [`CONTENT_ENTRIES`] table (modulo a small allowlist
    /// of intentional leniency deviations that are quantifier-only and
    /// therefore preserve atom-set equality).
    ///
    /// If this test fails, either:
    ///
    /// - Upstream `@atlaskit/adf-schema` shipped a content-model change.
    ///   Refresh `assets/adf-schema/full.json`, re-run
    ///   `cargo run --bin adf-schema-codegen`, update [`CONTENT_ENTRIES`] to
    ///   match, and bump [`SCHEMA_VERSION`] / [`UPSTREAM_TARBALL_SHA256`].
    /// - You edited [`CONTENT_ENTRIES`] in a way that desynchronises it from
    ///   the upstream atoms. Fix the entry, or document a new entry in
    ///   `LENIENCY_ALLOWLIST` if the deviation is intentional.
    #[test]
    fn generated_upstream_atoms_match_local_snapshot() {
        let local = local_schema_map();
        let upstream = upstream_atom_map();
        let diff = diff_atom_sets(&local, &upstream, LENIENCY_ALLOWLIST);
        assert!(
            diff.is_clean(),
            "atom-set drift between CONTENT_ENTRIES and generated::UPSTREAM_ENTRIES:\n\
             local_only_parents={:?}\n\
             upstream_only_parents={:?}\n\
             per_parent_unexpected={:?}",
            diff.local_only_parents,
            diff.upstream_only_parents,
            diff.per_parent_unexpected,
        );
    }

    #[test]
    fn diff_atom_sets_reports_clean_when_maps_agree() {
        let mut m: std::collections::BTreeMap<
            &'static str,
            std::collections::BTreeSet<&'static str>,
        > = std::collections::BTreeMap::new();
        m.insert("panel", ["paragraph", "heading"].into_iter().collect());
        let diff = diff_atom_sets(&m, &m.clone(), &[]);
        assert!(diff.is_clean());
        assert!(diff.local_only_parents.is_empty());
        assert!(diff.upstream_only_parents.is_empty());
        assert!(diff.per_parent_unexpected.is_empty());
    }

    #[test]
    fn diff_atom_sets_reports_local_only_parents() {
        let mut local: std::collections::BTreeMap<
            &'static str,
            std::collections::BTreeSet<&'static str>,
        > = std::collections::BTreeMap::new();
        local.insert("legacyNode", std::iter::once("paragraph").collect());
        let upstream: std::collections::BTreeMap<
            &'static str,
            std::collections::BTreeSet<&'static str>,
        > = std::collections::BTreeMap::new();
        let diff = diff_atom_sets(&local, &upstream, &[]);
        assert!(!diff.is_clean());
        assert_eq!(diff.local_only_parents, vec!["legacyNode"]);
        assert!(diff.upstream_only_parents.is_empty());
    }

    #[test]
    fn diff_atom_sets_reports_upstream_only_parents() {
        let local: std::collections::BTreeMap<
            &'static str,
            std::collections::BTreeSet<&'static str>,
        > = std::collections::BTreeMap::new();
        let mut upstream: std::collections::BTreeMap<
            &'static str,
            std::collections::BTreeSet<&'static str>,
        > = std::collections::BTreeMap::new();
        upstream.insert("newNode", std::iter::once("paragraph").collect());
        let diff = diff_atom_sets(&local, &upstream, &[]);
        assert!(!diff.is_clean());
        assert_eq!(diff.upstream_only_parents, vec!["newNode"]);
        assert!(diff.local_only_parents.is_empty());
    }

    #[test]
    fn diff_atom_sets_reports_unexpected_per_parent_diffs() {
        let mut local: std::collections::BTreeMap<
            &'static str,
            std::collections::BTreeSet<&'static str>,
        > = std::collections::BTreeMap::new();
        local.insert(
            "panel",
            ["paragraph", "heading"]
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>(),
        );
        let mut upstream = local.clone();
        upstream.insert("panel", ["paragraph", "blockCard"].into_iter().collect());
        let diff = diff_atom_sets(&local, &upstream, &[]);
        assert!(!diff.is_clean());
        let msg = diff.per_parent_unexpected.join("\n");
        assert!(msg.contains("panel"));
        assert!(
            msg.contains("blockCard"),
            "upstream_only should mention blockCard: {msg}"
        );
        assert!(
            msg.contains("heading"),
            "local_only should mention heading: {msg}"
        );
    }

    #[test]
    fn diff_atom_sets_honours_leniency_allowlist() {
        let mut local: std::collections::BTreeMap<
            &'static str,
            std::collections::BTreeSet<&'static str>,
        > = std::collections::BTreeMap::new();
        local.insert("panel", ["paragraph", "heading"].into_iter().collect());
        let mut upstream: std::collections::BTreeMap<
            &'static str,
            std::collections::BTreeSet<&'static str>,
        > = std::collections::BTreeMap::new();
        upstream.insert("panel", ["paragraph", "blockCard"].into_iter().collect());
        // Allowlist the exact deviation we just constructed.
        let lenient: &[LenientEntry] = &[(
            "panel",
            &["blockCard"], // upstream-only
            &["heading"],   // local-only
            "synthetic test deviation",
        )];
        let diff = diff_atom_sets(&local, &upstream, lenient);
        assert!(diff.is_clean(), "allowlist should mask the diff: {diff:?}");
    }

    #[test]
    fn generated_provenance_matches_local_constants() {
        assert_eq!(
            generated::UPSTREAM_TARBALL_SHA256,
            UPSTREAM_TARBALL_SHA256,
            "the vendored JSON's provenance SHA must match the runtime constant; \
             both are bumped together when the snapshot is refreshed",
        );
        // SCHEMA_VERSION is `<npm-version>-YYYY-MM-DD`. Strip the trailing
        // 11-char date suffix to recover the npm version, which must match
        // the version baked into the generated file.
        let date_len = "-YYYY-MM-DD".len();
        let local_npm_prefix = SCHEMA_VERSION
            .get(..SCHEMA_VERSION.len().saturating_sub(date_len))
            .unwrap_or(SCHEMA_VERSION);
        assert_eq!(
            generated::UPSTREAM_VERSION,
            local_npm_prefix,
            "generated UPSTREAM_VERSION must match the npm-version prefix of SCHEMA_VERSION",
        );
    }
}
