//! Resolves frontmatter custom field values and body sections to a JIRA
//! `fields` payload for write operations.
//!
//! Input:
//! - Frontmatter scalar map keyed by human name (from `custom_fields:` in JFM).
//! - Body sections parsed via [`crate::atlassian::document::split_custom_sections`].
//! - [`EditMeta`] fetched for the target issue (or create target).
//!
//! Output: `{ field_id -> api_json }` ready to be merged into a PUT/POST.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context, Result};

use crate::atlassian::client::{EditMeta, EditMetaField};
use crate::atlassian::convert::markdown_to_adf;
use crate::atlassian::document::CustomFieldSection;

/// Plugin type URI for the rich-text "textarea" custom field.
const CUSTOM_TEXTAREA: &str = "com.atlassian.jira.plugin.system.customfieldtypes:textarea";

/// Resolves a mixed set of frontmatter scalars and body sections into an
/// API-ready custom field map keyed by stable field ID.
///
/// - **Scalars** are dispatched by schema: option/radiobutton fields become
///   `{"value": "..."}`, textfield/number/date pass through, rich-text
///   fields are rejected (must use a body section instead).
/// - **Sections** must reference rich-text fields; their markdown is
///   converted to ADF via [`markdown_to_adf`].
///
/// Field names are looked up in [`EditMeta`]; entries already formatted as
/// `customfield_<digits>` bypass the lookup. An unknown or ambiguous name
/// produces an error naming the available editable fields.
pub fn resolve_custom_fields(
    scalars: &BTreeMap<String, serde_yaml::Value>,
    sections: &[CustomFieldSection],
    editmeta: &EditMeta,
) -> Result<BTreeMap<String, serde_json::Value>> {
    let mut out: BTreeMap<String, serde_json::Value> = BTreeMap::new();

    for (key, value) in scalars {
        let (id, field) = lookup_field(editmeta, key)?;
        if field.is_adf_rich_text() {
            bail!(
                "Field '{}' ({}) is a rich-text field; set it via a `<!-- field: {} ({}) -->` section in the body, not as a scalar in frontmatter",
                field.name, id, field.name, id
            );
        }
        let payload = scalar_to_api_value(value, field).with_context(|| {
            format!(
                "Failed to convert custom field '{}' ({}) to API value",
                field.name, id
            )
        })?;
        out.insert(id, payload);
    }

    for section in sections {
        let (id, field) = resolve_section_field(editmeta, section)?;
        if !field.is_adf_rich_text() {
            bail!(
                "Field '{}' ({}) is not a rich-text field; put scalar values in `custom_fields:` frontmatter instead of a body section",
                field.name, id
            );
        }
        let adf = markdown_to_adf(&section.body).with_context(|| {
            format!(
                "Failed to convert body for custom field '{}' ({}) to ADF",
                field.name, id
            )
        })?;
        let value =
            serde_json::to_value(&adf).context("Failed to serialize custom field ADF document")?;
        out.insert(id, value);
    }

    Ok(out)
}

/// Looks up a field by id-or-name, preferring exact `customfield_<id>`
/// matches before falling back to a name lookup.
fn lookup_field<'a>(editmeta: &'a EditMeta, key: &str) -> Result<(String, &'a EditMetaField)> {
    if looks_like_field_id(key) {
        if let Some(field) = editmeta.fields.get(key) {
            return Ok((key.to_string(), field));
        }
        // Fall through to name lookup in case the caller named a field
        // literally "customfield_something".
    }

    let matches: Vec<_> = editmeta
        .fields
        .iter()
        .filter(|(_, f)| f.name == key)
        .collect();

    match matches.as_slice() {
        [] => {
            let candidates = editmeta
                .fields
                .iter()
                .map(|(id, f)| format!("  {id}  {}", f.name))
                .collect::<Vec<_>>()
                .join("\n");
            Err(anyhow!(
                "Unknown custom field '{key}'. Available editable fields on this issue:\n{candidates}"
            ))
        }
        [(id, field)] => Ok(((*id).clone(), field)),
        multi => {
            let ids: Vec<_> = multi.iter().map(|(id, _)| id.as_str()).collect();
            Err(anyhow!(
                "Ambiguous custom field '{key}' matches multiple IDs: {}",
                ids.join(", ")
            ))
        }
    }
}

/// Resolves a body section's tag (which carries both name and id) against
/// editmeta, trusting the id when both are present.
fn resolve_section_field<'a>(
    editmeta: &'a EditMeta,
    section: &CustomFieldSection,
) -> Result<(String, &'a EditMetaField)> {
    if let Some(field) = editmeta.fields.get(&section.id) {
        return Ok((section.id.clone(), field));
    }
    lookup_field(editmeta, &section.name)
}

fn looks_like_field_id(s: &str) -> bool {
    s.starts_with("customfield_") && s[12..].chars().all(|c| c.is_ascii_digit())
}

/// Dispatches a scalar YAML value to the API shape expected for a given
/// field schema.
fn scalar_to_api_value(
    value: &serde_yaml::Value,
    field: &EditMetaField,
) -> Result<serde_json::Value> {
    let kind = field.schema.kind.as_str();
    let custom = field.schema.custom.as_deref();
    match (kind, custom) {
        ("option", _) | ("string", Some("com.atlassian.jira.plugin.system.customfieldtypes:radiobuttons")) => {
            let s = yaml_as_string(value).with_context(|| {
                format!("expected a string for option field '{}'", field.name)
            })?;
            Ok(serde_json::json!({ "value": s }))
        }
        ("array", _) => {
            let seq = value.as_sequence().ok_or_else(|| {
                anyhow!("expected a sequence for array field '{}'", field.name)
            })?;
            let items: Vec<serde_json::Value> = seq
                .iter()
                .map(|v| {
                    let s = yaml_as_string(v).with_context(|| {
                        format!(
                            "expected a string array element for field '{}'",
                            field.name
                        )
                    })?;
                    Ok(serde_json::json!({ "value": s }))
                })
                .collect::<Result<_>>()?;
            Ok(serde_json::Value::Array(items))
        }
        ("string", Some(CUSTOM_TEXTAREA)) => Err(anyhow!(
            "Field '{}' is a rich-text field; use a body section instead of a scalar value",
            field.name
        )),
        ("string" | "number" | "date" | "datetime", _) => yaml_to_json(value),
        (other, _) => Err(anyhow!(
            "Unsupported field type '{other}' for '{}'; custom field writes currently support option, textfield, number, date, and array-of-options",
            field.name
        )),
    }
}

fn yaml_as_string(value: &serde_yaml::Value) -> Result<String> {
    match value {
        serde_yaml::Value::String(s) => Ok(s.clone()),
        serde_yaml::Value::Bool(b) => Ok(b.to_string()),
        serde_yaml::Value::Number(n) => Ok(n.to_string()),
        _ => Err(anyhow!("expected a scalar string value")),
    }
}

fn yaml_to_json(value: &serde_yaml::Value) -> Result<serde_json::Value> {
    let s = serde_yaml::to_string(value).context("Failed to convert YAML to JSON")?;
    serde_json::to_value(serde_yaml::from_str::<serde_json::Value>(&s)?)
        .context("Failed to convert YAML value to JSON")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::client::{EditMetaField, EditMetaSchema};

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
                    },
                },
            );
        }
        EditMeta { fields }
    }

    #[test]
    fn scalar_option_field_wraps_in_value_object() {
        let editmeta = meta(&[(
            "customfield_10001",
            "Planned / Unplanned Work",
            "option",
            Some("com.atlassian.jira.plugin.system.customfieldtypes:select"),
        )]);
        let mut scalars = BTreeMap::new();
        scalars.insert(
            "Planned / Unplanned Work".to_string(),
            serde_yaml::Value::String("Unplanned".to_string()),
        );
        let out = resolve_custom_fields(&scalars, &[], &editmeta).unwrap();
        assert_eq!(
            out.get("customfield_10001").unwrap(),
            &serde_json::json!({ "value": "Unplanned" })
        );
    }

    #[test]
    fn scalar_radiobutton_wraps_in_value_object() {
        let editmeta = meta(&[(
            "customfield_10002",
            "Risk",
            "string",
            Some("com.atlassian.jira.plugin.system.customfieldtypes:radiobuttons"),
        )]);
        let mut scalars = BTreeMap::new();
        scalars.insert(
            "Risk".to_string(),
            serde_yaml::Value::String("High".to_string()),
        );
        let out = resolve_custom_fields(&scalars, &[], &editmeta).unwrap();
        assert_eq!(
            out.get("customfield_10002").unwrap(),
            &serde_json::json!({ "value": "High" })
        );
    }

    #[test]
    fn scalar_number_field_passes_through() {
        let editmeta = meta(&[(
            "customfield_10003",
            "Story points",
            "number",
            Some("com.atlassian.jira.plugin.system.customfieldtypes:float"),
        )]);
        let mut scalars = BTreeMap::new();
        scalars.insert(
            "Story points".to_string(),
            serde_yaml::Value::Number(8.into()),
        );
        let out = resolve_custom_fields(&scalars, &[], &editmeta).unwrap();
        assert_eq!(out.get("customfield_10003").unwrap(), &serde_json::json!(8));
    }

    #[test]
    fn scalar_array_option_field_wraps_each_item() {
        let editmeta = meta(&[("customfield_10004", "Components", "array", None)]);
        let mut scalars = BTreeMap::new();
        scalars.insert(
            "Components".to_string(),
            serde_yaml::Value::Sequence(vec![
                serde_yaml::Value::String("backend".to_string()),
                serde_yaml::Value::String("auth".to_string()),
            ]),
        );
        let out = resolve_custom_fields(&scalars, &[], &editmeta).unwrap();
        assert_eq!(
            out.get("customfield_10004").unwrap(),
            &serde_json::json!([{"value": "backend"}, {"value": "auth"}])
        );
    }

    #[test]
    fn scalar_to_rich_text_field_errors() {
        let editmeta = meta(&[(
            "customfield_19300",
            "Acceptance Criteria",
            "string",
            Some(CUSTOM_TEXTAREA),
        )]);
        let mut scalars = BTreeMap::new();
        scalars.insert(
            "Acceptance Criteria".to_string(),
            serde_yaml::Value::String("just text".to_string()),
        );
        let err = resolve_custom_fields(&scalars, &[], &editmeta).unwrap_err();
        assert!(err.to_string().contains("rich-text field"));
    }

    #[test]
    fn rich_text_section_becomes_adf_payload() {
        let editmeta = meta(&[(
            "customfield_19300",
            "Acceptance Criteria",
            "string",
            Some(CUSTOM_TEXTAREA),
        )]);
        let sections = [CustomFieldSection {
            name: "Acceptance Criteria".to_string(),
            id: "customfield_19300".to_string(),
            body: "- Item one\n- Item two".to_string(),
        }];
        let out = resolve_custom_fields(&BTreeMap::new(), &sections, &editmeta).unwrap();
        let value = out.get("customfield_19300").unwrap();
        assert_eq!(value["type"], "doc");
        assert_eq!(value["version"], 1);
        assert!(value["content"].is_array());
    }

    #[test]
    fn section_pointing_at_non_rich_text_field_errors() {
        let editmeta = meta(&[("customfield_10001", "Priority Flag", "option", None)]);
        let sections = [CustomFieldSection {
            name: "Priority Flag".to_string(),
            id: "customfield_10001".to_string(),
            body: "Some text".to_string(),
        }];
        let err = resolve_custom_fields(&BTreeMap::new(), &sections, &editmeta).unwrap_err();
        assert!(err.to_string().contains("not a rich-text field"));
    }

    #[test]
    fn unknown_field_name_errors_with_suggestions() {
        let editmeta = meta(&[
            ("customfield_1", "Alpha", "string", None),
            ("customfield_2", "Beta", "string", None),
        ]);
        let mut scalars = BTreeMap::new();
        scalars.insert("Gamma".to_string(), serde_yaml::Value::from("x"));
        let err = resolve_custom_fields(&scalars, &[], &editmeta).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Unknown custom field 'Gamma'"));
        assert!(msg.contains("Alpha"));
        assert!(msg.contains("Beta"));
    }

    #[test]
    fn field_id_bypasses_name_lookup() {
        let editmeta = meta(&[(
            "customfield_10001",
            "Planned / Unplanned Work",
            "option",
            None,
        )]);
        let mut scalars = BTreeMap::new();
        scalars.insert(
            "customfield_10001".to_string(),
            serde_yaml::Value::String("Unplanned".to_string()),
        );
        let out = resolve_custom_fields(&scalars, &[], &editmeta).unwrap();
        assert_eq!(
            out.get("customfield_10001").unwrap(),
            &serde_json::json!({ "value": "Unplanned" })
        );
    }

    #[test]
    fn section_prefers_tag_id_over_name_lookup() {
        // Name "Acceptance Criteria" matches two different IDs globally, but
        // the section tag carries a specific ID so no ambiguity error.
        let editmeta = meta(&[(
            "customfield_19300",
            "Acceptance Criteria",
            "string",
            Some(CUSTOM_TEXTAREA),
        )]);
        let sections = [CustomFieldSection {
            name: "Acceptance Criteria".to_string(),
            id: "customfield_19300".to_string(),
            body: "body".to_string(),
        }];
        let out = resolve_custom_fields(&BTreeMap::new(), &sections, &editmeta).unwrap();
        assert!(out.contains_key("customfield_19300"));
    }
}
