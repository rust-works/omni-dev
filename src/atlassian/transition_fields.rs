//! Resolves `execute --set-field`/`--resolution` inputs against a transition's
//! screen metadata into a JIRA transition `fields` payload, and reports whether
//! the screen accepts an in-body comment.
//!
//! Transition-screen fields come back in the same [`EditMeta`] shape as the
//! editmeta used by `jira write`, so scalar resolution reuses
//! [`resolve_custom_fields`](crate::atlassian::custom_fields::resolve_custom_fields)
//! verbatim.

use std::collections::BTreeMap;

use anyhow::{bail, Result};

use crate::atlassian::custom_fields::resolve_custom_fields;
use crate::atlassian::jira_types::EditMeta;

/// The outcome of resolving transition-screen inputs.
#[derive(Debug)]
pub struct ResolvedTransitionFields {
    /// API-shaped `fields` payload keyed by stable JIRA field id, ready to be
    /// sent in the transition body. Empty when no field inputs were supplied.
    pub fields: BTreeMap<String, serde_json::Value>,

    /// Whether the transition screen exposes a `comment` field. Callers use
    /// this to decide whether a supplied comment rides in the transition body
    /// (`update.comment`, atomic) or falls back to a separate comment post.
    pub comment_on_screen: bool,
}

/// Resolves `--set-field` scalars and an optional `--resolution` against a
/// transition's screen [`EditMeta`].
///
/// `scalars` are name→YAML-value pairs (as parsed by
/// [`parse_set_field`](crate::atlassian::custom_fields::parse_set_field)); they
/// are dispatched by schema exactly as `jira write` does. `resolution`, when
/// supplied, is added as the standard system shape `{"name": <value>}`, and
/// collides (hard error) with a `--set-field resolution=…` targeting the same
/// field id.
pub fn resolve_transition_fields(
    scalars: &BTreeMap<String, serde_yaml::Value>,
    resolution: Option<&str>,
    editmeta: &EditMeta,
) -> Result<ResolvedTransitionFields> {
    let mut fields = resolve_custom_fields(scalars, &[], editmeta)?;

    if let Some(value) = resolution {
        if fields.contains_key("resolution") {
            bail!("`--resolution` collides with `--set-field resolution=…`; supply only one");
        }
        fields.insert(
            "resolution".to_string(),
            serde_json::json!({ "name": value }),
        );
    }

    let comment_on_screen = editmeta.fields.contains_key("comment");

    Ok(ResolvedTransitionFields {
        fields,
        comment_on_screen,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::jira_types::{EditMetaField, EditMetaSchema};

    /// Builds an [`EditMeta`] from `(id, name, kind, custom)` tuples.
    fn meta(entries: &[(&str, &str, &str, Option<&str>)]) -> EditMeta {
        let mut fields = BTreeMap::new();
        for (id, name, kind, custom) in entries {
            fields.insert(
                (*id).to_string(),
                EditMetaField {
                    name: (*name).to_string(),
                    schema: EditMetaSchema {
                        kind: (*kind).to_string(),
                        custom: custom.map(str::to_string),
                        ..EditMetaSchema::default()
                    },
                    allowed_values: Vec::new(),
                },
            );
        }
        EditMeta { fields }
    }

    fn scalar(name: &str, value: &str) -> BTreeMap<String, serde_yaml::Value> {
        let mut m = BTreeMap::new();
        m.insert(
            name.to_string(),
            serde_yaml::Value::String(value.to_string()),
        );
        m
    }

    #[test]
    fn resolution_becomes_name_shape() {
        let editmeta = meta(&[("resolution", "Resolution", "resolution", None)]);
        let resolved =
            resolve_transition_fields(&BTreeMap::new(), Some("Fixed"), &editmeta).unwrap();
        assert_eq!(
            resolved.fields.get("resolution"),
            Some(&serde_json::json!({ "name": "Fixed" }))
        );
    }

    #[test]
    fn set_field_resolves_against_screen_metadata() {
        // A free-text custom field on the transition screen passes through.
        let editmeta = meta(&[("customfield_100", "Reason", "string", None)]);
        let resolved =
            resolve_transition_fields(&scalar("Reason", "because"), None, &editmeta).unwrap();
        assert_eq!(
            resolved.fields.get("customfield_100"),
            Some(&serde_json::json!("because"))
        );
    }

    #[test]
    fn resolution_collides_with_set_field_resolution() {
        // Use an `option`-kind field at id `resolution` so the `--set-field`
        // path resolves successfully and the collision guard is what fires
        // (JIRA's real `resolution` type isn't scalar-resolvable, but a
        // caller could still target the `resolution` id via `--set-field`).
        let editmeta = meta(&[("resolution", "Resolution", "option", None)]);
        let err =
            resolve_transition_fields(&scalar("resolution", "Done"), Some("Fixed"), &editmeta)
                .unwrap_err();
        assert!(err.to_string().contains("collides"));
    }

    #[test]
    fn comment_on_screen_detected() {
        let with = meta(&[("comment", "Comment", "comment", None)]);
        let without = meta(&[("resolution", "Resolution", "resolution", None)]);
        assert!(
            resolve_transition_fields(&BTreeMap::new(), None, &with)
                .unwrap()
                .comment_on_screen
        );
        assert!(
            !resolve_transition_fields(&BTreeMap::new(), None, &without)
                .unwrap()
                .comment_on_screen
        );
    }

    #[test]
    fn empty_inputs_yield_empty_fields() {
        let editmeta = meta(&[("resolution", "Resolution", "resolution", None)]);
        let resolved = resolve_transition_fields(&BTreeMap::new(), None, &editmeta).unwrap();
        assert!(resolved.fields.is_empty());
    }

    #[test]
    fn unknown_set_field_errors_with_candidates() {
        let editmeta = meta(&[("customfield_100", "Reason", "string", None)]);
        let err =
            resolve_transition_fields(&scalar("Nonexistent", "x"), None, &editmeta).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Nonexistent"));
        assert!(msg.contains("Reason"));
    }
}
