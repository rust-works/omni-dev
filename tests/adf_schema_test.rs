//! Integration tests for the ADF schema validator.
//!
//! Library smoke test exercising the public API from outside the crate, using
//! ADF JSON deserialised through the public `AdfDocument::from_json_str`
//! entry point — the same path real callers (Confluence/Jira API responses)
//! take.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::needless_collect)]

use omni_dev::atlassian::adf::AdfDocument;
use omni_dev::atlassian::adf_schema::{
    permits_child, validate_document, AdfSchemaViolation, Quantifier, SCHEMA_VERSION,
};

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
    // Emits both DisallowedChild (for the expand) and Arity (panel needs 1+
    // valid children — disallowed children do not count).
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

    let disallowed: Vec<&AdfSchemaViolation> = violations
        .iter()
        .filter(|v| matches!(v, AdfSchemaViolation::DisallowedChild { .. }))
        .collect();
    assert_eq!(disallowed.len(), 1);
    match disallowed[0] {
        AdfSchemaViolation::DisallowedChild {
            child_type,
            parent_type,
            path,
        } => {
            assert_eq!(child_type, "expand");
            assert_eq!(parent_type, "panel");
            assert_eq!(path, &vec![0_usize, 0]);
        }
        other => unreachable!("filtered to DisallowedChild, got {other:?}"),
    }
}

#[test]
fn empty_bullet_list_flagged_arity_via_public_api() {
    let json = r#"{
        "version": 1,
        "type": "doc",
        "content": [
            {"type": "bulletList", "content": []}
        ]
    }"#;
    let doc = AdfDocument::from_json_str(json).unwrap();
    let violations = validate_document(&doc);
    assert_eq!(violations.len(), 1);
    match &violations[0] {
        AdfSchemaViolation::Arity {
            parent_type,
            atoms,
            expected,
            actual,
            path,
        } => {
            assert_eq!(parent_type, "bulletList");
            assert_eq!(atoms, &vec!["listItem"]);
            assert_eq!(expected, &Quantifier::OneOrMore);
            assert_eq!(*actual, 0);
            assert_eq!(path, &vec![0_usize]);
        }
        other => panic!("expected Arity, got {other:?}"),
    }
}

#[test]
fn media_single_with_two_media_flagged_via_public_api() {
    let json = r#"{
        "version": 1,
        "type": "doc",
        "content": [
            {
                "type": "mediaSingle",
                "content": [
                    {"type": "media", "attrs": {"id": "a", "type": "file", "collection": "x"}},
                    {"type": "media", "attrs": {"id": "b", "type": "file", "collection": "x"}}
                ]
            }
        ]
    }"#;
    let doc = AdfDocument::from_json_str(json).unwrap();
    let violations = validate_document(&doc);
    let arity: Vec<&AdfSchemaViolation> = violations
        .iter()
        .filter(|v| matches!(v, AdfSchemaViolation::Arity { .. }))
        .collect();
    assert_eq!(arity.len(), 1, "got: {violations:?}");
    match arity[0] {
        AdfSchemaViolation::Arity {
            parent_type,
            expected,
            actual,
            ..
        } => {
            assert_eq!(parent_type, "mediaSingle");
            assert_eq!(expected, &Quantifier::Exactly(1));
            assert_eq!(*actual, 2);
        }
        other => unreachable!("filtered to Arity, got {other:?}"),
    }
}

#[test]
fn permits_child_is_callable_externally() {
    // Round-trips through the public re-export.
    assert!(permits_child("tableCell", "nestedExpand"));
    assert!(!permits_child("tableCell", "expand"));
}

#[test]
fn invalid_panel_type_flagged_via_public_api() {
    let json = r#"{
        "version": 1,
        "type": "doc",
        "content": [
            {
                "type": "panel",
                "attrs": {"panelType": "purple"},
                "content": [{"type": "paragraph", "content": [{"type": "text", "text": "x"}]}]
            }
        ]
    }"#;
    let doc = AdfDocument::from_json_str(json).unwrap();
    let violations = validate_document(&doc);
    let invalid = violations
        .iter()
        .filter(|v| matches!(v, AdfSchemaViolation::InvalidAttr { .. }))
        .collect::<Vec<_>>();
    assert_eq!(invalid.len(), 1, "got: {violations:?}");
    let _ = &invalid;
    match invalid[0] {
        AdfSchemaViolation::InvalidAttr {
            node_type,
            attr_name,
            ..
        } => {
            assert_eq!(node_type, "panel");
            assert_eq!(attr_name, "panelType");
        }
        other => unreachable!("filtered to InvalidAttr, got {other:?}"),
    }
}

#[test]
fn missing_panel_type_flagged_via_public_api() {
    let json = r#"{
        "version": 1,
        "type": "doc",
        "content": [
            {
                "type": "panel",
                "content": [{"type": "paragraph", "content": [{"type": "text", "text": "x"}]}]
            }
        ]
    }"#;
    let doc = AdfDocument::from_json_str(json).unwrap();
    let violations = validate_document(&doc);
    let missing: Vec<&AdfSchemaViolation> = violations
        .iter()
        .filter(|v| matches!(v, AdfSchemaViolation::MissingAttr { .. }))
        .collect();
    assert_eq!(missing.len(), 1, "got: {violations:?}");
    match missing[0] {
        AdfSchemaViolation::MissingAttr {
            node_type,
            attr_name,
            ..
        } => {
            assert_eq!(node_type, "panel");
            assert_eq!(attr_name, "panelType");
        }
        other => unreachable!("filtered to MissingAttr, got {other:?}"),
    }
}

#[test]
fn heading_level_out_of_range_flagged_via_public_api() {
    let json = r#"{
        "version": 1,
        "type": "doc",
        "content": [
            {"type": "heading", "attrs": {"level": 7}, "content": [{"type": "text", "text": "x"}]}
        ]
    }"#;
    let doc = AdfDocument::from_json_str(json).unwrap();
    let violations = validate_document(&doc);
    let invalid: Vec<&AdfSchemaViolation> = violations
        .iter()
        .filter(|v| matches!(v, AdfSchemaViolation::InvalidAttr { .. }))
        .collect();
    assert_eq!(invalid.len(), 1, "got: {violations:?}");
}
