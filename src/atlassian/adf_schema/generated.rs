//! Auto-generated from `assets/adf-schema/full.json` by
//! `src/bin/adf_schema_codegen.rs`.
//!
//! **Do not edit by hand.** To refresh the snapshot, follow
//! `assets/adf-schema/README.md`:
//!
//! 1. Replace `assets/adf-schema/full.json` with a newly-extracted upstream
//!    `dist/json-schema/v1/full.json`.
//! 2. Update `assets/adf-schema/provenance.json` with the new version and
//!    tarball/JSON SHA-256s.
//! 3. Run `cargo run --bin adf-schema-codegen`.
//! 4. Commit `full.json`, `provenance.json`, and this file together.
//!
//! See issue #732 (ADR-0023 follow-up) for the rationale.

/// Upstream npm package name.
pub const UPSTREAM_PACKAGE: &str = "@atlaskit/adf-schema";

/// Upstream npm package version this snapshot was generated from.
pub const UPSTREAM_VERSION: &str = "52.9.5";

/// SHA-256 of the upstream tarball that produced this snapshot.
pub const UPSTREAM_TARBALL_SHA256: &str =
    "90b9b26f5cdf6f0850cebe5cf2df7662601b249322d6bcbeead712ca018e0b56";

/// SHA-256 of the vendored `assets/adf-schema/full.json` bytes.
pub const UPSTREAM_FULL_JSON_SHA256: &str =
    "9c4c8237e8ecdb5088438421be2f9fc7becf1ffb6096951af4eb4d4c85a06190";

/// Per-parent allowed-children atoms, derived faithfully from the upstream
/// `@atlaskit/adf-schema` JSON schema in `assets/adf-schema/full.json`.
///
/// Sorted alphabetically by parent; children within each parent are also
/// sorted alphabetically and deduplicated. Quantifier and order information
/// (`+`, `*`, `?`, `{n}`, `{m,n}`, sequence order) is *not* preserved here —
/// the upstream JSON schema's `anyOf`-of-`$ref` shape does not encode it in
/// a parseable way. See `super::CONTENT_ENTRIES` in
/// `src/atlassian/adf_schema/mod.rs` for the runtime model that layers
/// quantifier arity on top of these atoms.
///
/// The unit test `generated_upstream_atoms_match_local_snapshot` in
/// `src/atlassian/adf_schema/mod.rs` asserts that the flattened atoms from
/// `super::CONTENT_ENTRIES` agree with `UPSTREAM_ENTRIES` modulo a small
/// allowlist of documented leniency deviations.
pub const UPSTREAM_ENTRIES: &[(&str, &[&str])] = &[
    ("blockTaskItem", &["extension", "paragraph"]),
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
    ("bulletList", &["listItem"]),
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
    ("codeBlock", &["text"]),
    (
        "decisionItem",
        &[
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
        ],
    ),
    ("decisionList", &["decisionItem"]),
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
    (
        "heading",
        &[
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
        ],
    ),
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
    ("layoutSection", &["layoutColumn"]),
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
    ("mediaGroup", &["media"]),
    ("mediaSingle", &["caption", "media"]),
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
    ("orderedList", &["listItem"]),
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
    (
        "paragraph",
        &[
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
        ],
    ),
    ("table", &["tableRow"]),
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
    ("tableRow", &["tableCell", "tableHeader"]),
    (
        "taskItem",
        &[
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
        ],
    ),
    ("taskList", &["blockTaskItem", "taskItem", "taskList"]),
];
