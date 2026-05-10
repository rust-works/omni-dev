//! Integration tests for the ADF schema validator.
//!
//! Library smoke test exercising the public API from outside the crate, using
//! ADF JSON deserialised through the public `AdfDocument::from_json_str`
//! entry point — the same path real callers (Confluence/Jira API responses)
//! take.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use omni_dev::atlassian::adf::AdfDocument;
use omni_dev::atlassian::adf_schema::{permits_child, validate_document, SCHEMA_VERSION};

#[test]
fn schema_version_constant_is_populated() {
    assert!(!SCHEMA_VERSION.is_empty());
}

#[test]
fn well_formed_document_has_no_violations() {
    let json = r#"{
        "version": 1,
        "type": "doc",
        "content": [
            {"type": "paragraph", "content": [{"type": "text", "text": "hello"}]},
            {"type": "heading", "attrs": {"level": 1}, "content": [{"type": "text", "text": "world"}]}
        ]
    }"#;
    let doc = AdfDocument::from_json_str(json).unwrap();
    assert_eq!(validate_document(&doc), vec![]);
}

#[test]
fn expand_inside_panel_is_flagged_via_public_api() {
    // A panel containing an expand — the canonical example from the issue.
    let json = r#"{
        "version": 1,
        "type": "doc",
        "content": [
            {
                "type": "panel",
                "attrs": {"panelType": "info"},
                "content": [
                    {
                        "type": "expand",
                        "attrs": {"title": "details"},
                        "content": [
                            {"type": "paragraph", "content": [{"type": "text", "text": "x"}]}
                        ]
                    }
                ]
            }
        ]
    }"#;
    let doc = AdfDocument::from_json_str(json).unwrap();
    let violations = validate_document(&doc);
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0].child_type, "expand");
    assert_eq!(violations[0].parent_type, "panel");
    assert_eq!(violations[0].path, vec![0, 0]);
}

#[test]
fn permits_child_is_callable_externally() {
    // Round-trips through the public re-export.
    assert!(permits_child("tableCell", "nestedExpand"));
    assert!(!permits_child("tableCell", "expand"));
}
