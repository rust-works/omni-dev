//! ADF content-model schema and structural validator.
//!
//! Encodes the allowed-children content model from the upstream
//! `@atlaskit/adf-schema` npm package as a static lookup table, and exposes a
//! [`validate_document`] walker that reports nesting violations against that
//! model.
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
//! # Scope of v1
//!
//! Only the **allowed-children set** for each parent is encoded. Quantifiers
//! (`+`, `*`, `?`), mark whitelists, attribute schemas, and arity (e.g.
//! "exactly one media child") are out of scope; they will be addressed in
//! follow-up issues.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::LazyLock;

use crate::atlassian::adf::{AdfDocument, AdfNode};

pub mod drift;

/// Crate-internal view of the schema as a `BTreeMap`, used by the drift
/// detector to diff against an upstream-derived map of the same shape.
#[must_use]
pub(crate) fn local_schema_map() -> BTreeMap<&'static str, BTreeSet<&'static str>> {
    let mut m = BTreeMap::new();
    for (parent, children) in ENTRIES {
        m.insert(*parent, children.iter().copied().collect());
    }
    m
}

/// Pinned upstream schema version.
///
/// Format: `<npm-package-version>-<transcription-date>`. Bumped manually when
/// the lookup table is refreshed against a new upstream release.
pub const SCHEMA_VERSION: &str = "52.9.5-2026-05-10";

/// SHA-256 of the upstream `@atlaskit/adf-schema` tarball used as the source
/// for the current transcription.
///
/// Recorded for reproducibility. Kept here (not in the ADR) so the binary
/// itself carries the provenance and so refreshing the snapshot is a single
/// file change.
///
/// To verify locally:
/// ```text
/// curl -sL https://registry.npmjs.org/@atlaskit/adf-schema/-/adf-schema-52.9.5.tgz \
///   | shasum -a 256
/// ```
pub const UPSTREAM_TARBALL_SHA256: &str =
    "90b9b26f5cdf6f0850cebe5cf2df7662601b249322d6bcbeead712ca018e0b56";

/// A single nesting violation reported by the validator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdfSchemaViolation {
    /// The `node_type` of the offending child.
    pub child_type: String,
    /// The `node_type` of the parent that does not permit the child.
    pub parent_type: String,
    /// Index path from the document root to the offending node.
    ///
    /// Each element is the position of the node in its parent's `content`
    /// array. The path identifies the child; the parent is one level up.
    pub path: Vec<usize>,
}

impl std::fmt::Display for AdfSchemaViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ADF schema violation at /{}: '{}' is not permitted inside '{}'",
            self.path
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join("/"),
            self.child_type,
            self.parent_type,
        )
    }
}

// -----------------------------------------------------------------------------
// Common content-model groups
// -----------------------------------------------------------------------------
//
// Most parents have bespoke content models in upstream's JSON schema; only the
// inline content list is shared between enough parents to be worth factoring.

/// Inline content shared by `paragraph`, `heading`, `taskItem`, `decisionItem`.
///
/// Note: `caption`'s inline list is a strict subset of this and is inlined
/// below rather than referencing this constant, so the per-parent diff against
/// upstream remains exact.
const FULL_INLINE_CONTENT: &[&str] = &[
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
// allowed-children set in the upstream JSON schema — they are runtime-only
// preservation wrappers, not first-class content. They are accepted under any
// known parent by a walker-level short-circuit; see [`is_unsupported`].
//
// Entries are sorted alphabetically by parent name; each child slice is
// sorted alphabetically too. The whole table can be diffed line-by-line
// against the output of the upstream JSON schema's `content.items.anyOf`
// expansion.

/// Allowed-children entries, sorted alphabetically by parent.
pub(crate) type Entry = (&'static str, &'static [&'static str]);

pub(crate) const ENTRIES: &[Entry] = &[
    // blockTaskItem — definitions/blockTaskItem_node
    ("blockTaskItem", &["extension", "paragraph"]),
    // blockquote — definitions/blockquote_node
    (
        "blockquote",
        &[
            "bulletList",
            "codeBlock",
            "extension",
            "mediaGroup",
            "mediaSingle",
            "orderedList",
            "paragraph",
        ],
    ),
    // bodiedExtension — definitions/bodiedExtension_node (does NOT permit
    // bodiedExtension recursively, expand, nestedExpand, or layoutSection)
    (
        "bodiedExtension",
        &[
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
        ],
    ),
    // bodiedSyncBlock — definitions/bodiedSyncBlock_node
    (
        "bodiedSyncBlock",
        &[
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
        ],
    ),
    // bulletList — definitions/bulletList_node
    ("bulletList", &["listItem"]),
    // caption — definitions/caption_node (NB: tighter than the full inline
    // group — no inlineExtension, no mediaInline)
    (
        "caption",
        &[
            "date",
            "emoji",
            "hardBreak",
            "inlineCard",
            "mention",
            "placeholder",
            "status",
            "text",
        ],
    ),
    // codeBlock — definitions/codeBlock_node (text only — hardBreak is NOT
    // permitted by the upstream JSON schema even though some renderers handle
    // it)
    ("codeBlock", &["text"]),
    // decisionItem — definitions/decisionItem_node
    ("decisionItem", FULL_INLINE_CONTENT),
    // decisionList — definitions/decisionList_node
    ("decisionList", &["decisionItem"]),
    // doc — definitions/doc_node
    (
        "doc",
        &[
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
        ],
    ),
    // expand — definitions/expand_node (DOES permit nestedExpand per the
    // upstream JSON schema, contrary to some legacy documentation)
    (
        "expand",
        &[
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
        ],
    ),
    // heading — definitions/heading_node
    ("heading", FULL_INLINE_CONTENT),
    // layoutColumn — definitions/layoutColumn_node (permits expand and
    // bodiedExtension; does NOT permit nestedExpand)
    (
        "layoutColumn",
        &[
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
        ],
    ),
    // layoutSection — definitions/layoutSection_node
    ("layoutSection", &["layoutColumn"]),
    // listItem — definitions/listItem_node
    (
        "listItem",
        &[
            "bulletList",
            "codeBlock",
            "extension",
            "mediaSingle",
            "orderedList",
            "paragraph",
            "taskList",
        ],
    ),
    // mediaGroup — definitions/mediaGroup_node
    ("mediaGroup", &["media"]),
    // mediaSingle — definitions/mediaSingle_caption_node and
    // mediaSingle_full_node merged: one media child, optionally a caption
    ("mediaSingle", &["caption", "media"]),
    // nestedExpand — definitions/nestedExpand_node (permits panel and
    // blockquote; does NOT permit table, blockCard, embedCard, expand, or
    // nestedExpand itself)
    (
        "nestedExpand",
        &[
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
        ],
    ),
    // orderedList — definitions/orderedList_node
    ("orderedList", &["listItem"]),
    // panel — definitions/panel_node
    (
        "panel",
        &[
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
        ],
    ),
    // paragraph — definitions/paragraph_node
    ("paragraph", FULL_INLINE_CONTENT),
    // table — definitions/table_node
    ("table", &["tableRow"]),
    // tableCell — definitions/table_cell_content (shared with tableHeader)
    (
        "tableCell",
        &[
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
        ],
    ),
    // tableHeader — definitions/table_header_node (uses table_cell_content)
    (
        "tableHeader",
        &[
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
        ],
    ),
    // tableRow — definitions/table_row_node
    ("tableRow", &["tableCell", "tableHeader"]),
    // taskItem — definitions/taskItem_node
    ("taskItem", FULL_INLINE_CONTENT),
    // taskList — definitions/taskList_node
    ("taskList", &["blockTaskItem", "taskItem", "taskList"]),
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

static SCHEMA: LazyLock<HashMap<&'static str, &'static [&'static str]>> =
    LazyLock::new(|| ENTRIES.iter().copied().collect());

/// Returns the allowed direct children for a parent node type.
///
/// `None` means the node has no entry in the schema (either a leaf type or a
/// type unknown to this snapshot). Unknown parents are treated permissively
/// by [`permits_child`] and the walker.
#[must_use]
pub fn allowed_children(parent: &str) -> Option<&'static [&'static str]> {
    SCHEMA.get(parent).copied()
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
    match SCHEMA.get(parent) {
        Some(children) => children.contains(&child),
        None => true,
    }
}

/// Validates an entire ADF document and returns all violations found.
///
/// An empty `Vec` means the document is structurally valid against the
/// snapshot. The walker is depth-first and reports violations in document
/// order.
#[must_use]
pub fn validate_document(doc: &AdfDocument) -> Vec<AdfSchemaViolation> {
    let mut violations = Vec::new();
    let mut path = Vec::new();
    if let Some(children) = SCHEMA.get(doc.doc_type.as_str()).copied() {
        walk_children(
            &doc.content,
            &doc.doc_type,
            children,
            &mut path,
            &mut violations,
        );
    }
    violations
}

fn walk_children(
    children: &[AdfNode],
    parent_type: &str,
    allowed: &'static [&'static str],
    path: &mut Vec<usize>,
    out: &mut Vec<AdfSchemaViolation>,
) {
    for (idx, child) in children.iter().enumerate() {
        path.push(idx);
        if !is_unsupported(&child.node_type) && !allowed.contains(&child.node_type.as_str()) {
            out.push(AdfSchemaViolation {
                child_type: child.node_type.clone(),
                parent_type: parent_type.to_string(),
                path: path.clone(),
            });
        }
        if let (Some(grand), Some(grand_allowed)) = (
            child.content.as_deref(),
            SCHEMA.get(child.node_type.as_str()).copied(),
        ) {
            walk_children(grand, &child.node_type, grand_allowed, path, out);
        }
        path.pop();
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

    fn doc(content: Vec<AdfNode>) -> AdfDocument {
        AdfDocument {
            version: 1,
            doc_type: "doc".to_string(),
            content,
        }
    }

    #[test]
    fn schema_has_entry_for_every_advertised_container() {
        // Spot-check: every parent referenced from another entry's child list
        // is itself either a known leaf or has its own entry. Catches typos
        // that would silently make the validator permissive on a real type.
        // unsupportedBlock / unsupportedInline are listed because the walker
        // accepts them via the escape hatch — they should not appear in any
        // ENTRY child list, but if a future refactor mistakenly adds them,
        // this test still passes (they're known leaves).
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
        for (_parent, children) in ENTRIES {
            for child in *children {
                let known = SCHEMA.contains_key(child) || known_leaves.contains(child);
                assert!(
                    known,
                    "child '{child}' has no schema entry and is not in the leaf list"
                );
            }
        }
    }

    #[test]
    fn child_lists_are_sorted_for_diffability() {
        for (parent, children) in ENTRIES {
            let mut sorted = children.to_vec();
            sorted.sort_unstable();
            assert_eq!(
                children.to_vec(),
                sorted,
                "child list for '{parent}' is not sorted"
            );
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
        // Per upstream JSON schema 52.9.5, `expand` permits `nestedExpand` as
        // a child but NOT another `expand`.
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
        // Per upstream JSON schema 52.9.5: `extension` IS in the list,
        // and `unsupportedBlock` is NOT (it is accepted via the walker
        // escape hatch instead).
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
        // The flip side of the permissive-parent rule: when the parent IS
        // known, an unknown child is not silently let through.
        assert!(!permits_child("paragraph", "madeUpInline"));
    }

    #[test]
    fn nested_expand_distinguished_from_expand() {
        // Per upstream JSON schema 52.9.5: `nestedExpand` permits `panel` and
        // `blockquote` (it has a tighter content model than `expand` only in
        // that it forbids `table`, `blockCard`, `embedCard`, and any
        // `expand` / `nestedExpand` child to bound nesting depth).
        assert!(permits_child("nestedExpand", "panel"));
        assert!(permits_child("nestedExpand", "blockquote"));
        assert!(!permits_child("nestedExpand", "table"));
        assert!(!permits_child("nestedExpand", "blockCard"));
        assert!(!permits_child("nestedExpand", "embedCard"));
        assert!(!permits_child("nestedExpand", "nestedExpand"));
        assert!(!permits_child("nestedExpand", "expand"));
    }

    // ---- Walker behaviour ------------------------------------------------

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
        let bad_panel = node("panel", vec![node("expand", vec![])]);
        let document = doc(vec![bad_panel]);

        let violations = validate_document(&document);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].child_type, "expand");
        assert_eq!(violations[0].parent_type, "panel");
        assert_eq!(violations[0].path, vec![0, 0]);
    }

    #[test]
    fn validate_finds_expand_inside_table_cell() {
        // The issue specifically calls out tableCell allowing nestedExpand but
        // not expand.
        let bad_cell = node(
            "tableCell",
            vec![node("expand", vec![AdfNode::paragraph(vec![])])],
        );
        let row = node("tableRow", vec![bad_cell]);
        let table = node("table", vec![row]);
        let document = doc(vec![table]);

        let violations = validate_document(&document);
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].child_type, "expand");
        assert_eq!(violations[0].parent_type, "tableCell");
        assert_eq!(violations[0].path, vec![0, 0, 0, 0]);
    }

    #[test]
    fn validate_walks_into_nested_violations_in_document_order() {
        let document = doc(vec![
            // Two violations: rule inside paragraph, then expand inside panel.
            AdfNode::paragraph(vec![leaf("rule")]),
            node("panel", vec![node("expand", vec![])]),
        ]);

        let violations = validate_document(&document);
        assert_eq!(violations.len(), 2);
        assert_eq!(violations[0].child_type, "rule");
        assert_eq!(violations[0].parent_type, "paragraph");
        assert_eq!(violations[0].path, vec![0, 0]);
        assert_eq!(violations[1].child_type, "expand");
        assert_eq!(violations[1].parent_type, "panel");
        assert_eq!(violations[1].path, vec![1, 0]);
    }

    #[test]
    fn validate_is_permissive_under_unknown_parents() {
        // A document whose root contains an unknown container should not
        // trigger validation of that container's subtree, but the unknown
        // container itself is still flagged as not allowed at the doc root.
        let document = doc(vec![node("futureBlock", vec![node("expand", vec![])])]);
        let violations = validate_document(&document);
        // Only the doc-level rejection of `futureBlock`; the inner subtree is
        // left alone because we don't know `futureBlock`'s content model.
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].child_type, "futureBlock");
        assert_eq!(violations[0].parent_type, "doc");
    }

    #[test]
    fn unsupported_block_is_universally_accepted_via_walker_escape_hatch() {
        // Per upstream JSON schema 52.9.5, `unsupportedBlock` does NOT appear
        // in any parent's allowed-children set. The walker accepts it under
        // any known parent regardless, so that documents round-tripped
        // through ADR-0020's `adf-unsupported` fence still validate.
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
            // And it really is NOT in the allowed-children set itself.
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
        // Defensive branch: if the document's root `doc_type` is not in the
        // schema (e.g. a future-renamed root, or a partially-deserialised
        // payload), validate_document returns no violations rather than
        // panicking. This keeps the validator's "permissive on unknown
        // parents" invariant honest at the very top of the tree.
        let document = AdfDocument {
            version: 1,
            doc_type: "futureRoot".to_string(),
            content: vec![node("expand", vec![])],
        };
        assert_eq!(validate_document(&document), vec![]);
    }

    #[test]
    fn walker_does_not_flag_unsupported_block_inside_panel() {
        // End-to-end: a panel containing an unsupportedBlock is not flagged.
        let document = doc(vec![node("panel", vec![leaf("unsupportedBlock")])]);
        assert_eq!(validate_document(&document), vec![]);
    }

    #[test]
    fn display_format_is_actionable() {
        let v = AdfSchemaViolation {
            child_type: "expand".into(),
            parent_type: "panel".into(),
            path: vec![0, 1, 0],
        };
        assert_eq!(
            v.to_string(),
            "ADF schema violation at /0/1/0: 'expand' is not permitted inside 'panel'"
        );
    }
}
