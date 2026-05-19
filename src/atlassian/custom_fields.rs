//! Resolves frontmatter custom field values and body sections to a JIRA
//! `fields` payload for write operations.
//!
//! Input:
//! - Frontmatter scalar map keyed by human name (from `custom_fields:` in JFM).
//! - Body sections parsed via `crate::atlassian::document::split_custom_sections`.
//! - [`EditMeta`] fetched for the target issue (or create target).
//!
//! Output: `{ field_id -> api_json }` ready to be merged into a PUT/POST.

use std::collections::BTreeMap;

use anyhow::{anyhow, bail, Context, Result};

use crate::atlassian::adf_validated::ValidatedAdfDocument;
use crate::atlassian::client::{EditMeta, EditMetaField};
use crate::atlassian::convert::markdown_to_adf;
use crate::atlassian::document::CustomFieldSection;

#[cfg(test)]
use crate::atlassian::client::TEXTAREA_CUSTOM_TYPE as CUSTOM_TEXTAREA;

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
            let payload = rich_text_scalar_to_api_value(value, field, &id)?;
            out.insert(id, payload);
            continue;
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
        let validated = ValidatedAdfDocument::try_new(adf).with_context(|| {
            format!(
                "Custom field '{}' ({}) failed ADF nesting validation",
                field.name, id
            )
        })?;
        let value = serde_json::to_value(&validated)
            .context("Failed to serialize custom field ADF document")?;
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

/// Converts a frontmatter / `--set-field` scalar targeting a rich-text custom
/// field into the API JSON shape.
///
/// String values are treated as JFM markdown and converted to ADF (matching
/// the contract for `content`/description and for body sections). An empty
/// string or YAML null clears the field by emitting `null`. Non-string
/// scalars (numbers, bools, sequences, mappings) are rejected — rich-text
/// fields require either a JFM string or a body section.
///
/// Null handling is load-bearing for the CLI: `--set-field "Name="` parses
/// the empty RHS as YAML null (not a string), so a "clear the field"
/// invocation arrives here as `Value::Null`.
fn rich_text_scalar_to_api_value(
    value: &serde_yaml::Value,
    field: &EditMetaField,
    id: &str,
) -> Result<serde_json::Value> {
    let s = match value {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Null => String::new(),
        _ => bail!(
            "Field '{}' ({}) is a rich-text field; supply JFM markdown as a string or use a `<!-- field: {} ({}) -->` body section",
            field.name,
            id,
            field.name,
            id
        ),
    };
    string_to_rich_text_api_value(&s, &field.name, id)
}

/// Shared conversion: empty → `null`; otherwise JFM → validated ADF JSON.
fn string_to_rich_text_api_value(s: &str, field_name: &str, id: &str) -> Result<serde_json::Value> {
    if s.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    let adf = markdown_to_adf(s).with_context(|| {
        format!("Failed to convert rich-text field '{field_name}' ({id}) JFM to ADF")
    })?;
    let validated = ValidatedAdfDocument::try_new(adf).with_context(|| {
        format!("Custom field '{field_name}' ({id}) failed ADF nesting validation")
    })?;
    serde_json::to_value(&validated).context("Failed to serialize custom field ADF document")
}

/// Applies JFM → ADF conversion in-place to string values targeting rich-text
/// custom fields, per issue #866.
///
/// For each entry in `fields`:
/// - If the key is not present in `editmeta.fields`, leave the value
///   untouched (pass-through — the API will surface its own error).
/// - If the resolved field is not a rich-text textarea, leave the value
///   untouched.
/// - If the value is a JSON object, leave it untouched (assumed to be a raw
///   ADF document — backwards-compatible).
/// - If the value is a JSON string, treat it as JFM markdown and convert.
///   An empty string becomes `null`, which clears the field.
/// - Any other value type (number/bool/array/null) is left untouched.
///
/// Designed for the MCP `jira_write` `fields` escape hatch: lets callers pass
/// `"customfield_19300": "- bullet\n- bullet"` and get the right ADF on the
/// wire without hand-crafting the document.
pub fn convert_textarea_string_values(
    fields: &mut BTreeMap<String, serde_json::Value>,
    editmeta: &EditMeta,
) -> Result<()> {
    for (id, value) in fields.iter_mut() {
        let Some(field) = editmeta.fields.get(id) else {
            continue;
        };
        if !field.is_adf_rich_text() {
            continue;
        }
        let serde_json::Value::String(s) = value else {
            continue;
        };
        *value = string_to_rich_text_api_value(s, &field.name, id)?;
    }
    Ok(())
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

/// Parses a `--set-field NAME=VALUE` argument into a `(name, value)` pair.
///
/// The value is parsed as YAML when possible so `--set-field "Points=8"`
/// becomes a number and `--set-field "Enabled=true"` becomes a bool.
/// Values that fail to parse as YAML fall back to plain strings.
pub fn parse_set_field(input: &str) -> Result<(String, serde_yaml::Value)> {
    let (name, value) = input
        .split_once('=')
        .ok_or_else(|| anyhow!("expected --set-field \"NAME=VALUE\", got '{input}'"))?;
    let name = name.trim().to_string();
    if name.is_empty() {
        bail!("--set-field requires a non-empty name before '='");
    }
    let yaml_value = serde_yaml::from_str::<serde_yaml::Value>(value)
        .unwrap_or_else(|_| serde_yaml::Value::String(value.to_string()));
    Ok((name, yaml_value))
}

/// Translates an `accountId`-style assignee/reporter input to the JSON
/// shape JIRA expects.
///
/// The empty string clears the user (Atlassian's supported `null` payload);
/// any other value is wrapped as `{"accountId": "<value>"}`. The literal
/// `-1` is preserved as `{"accountId": "-1"}`, which JIRA interprets as
/// automatic assignment.
pub fn user_field_value(raw: &str) -> serde_json::Value {
    if raw.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::json!({ "accountId": raw })
    }
}

/// Merges typed `assignee`/`reporter` parameters into a resolved JIRA fields
/// map.
///
/// Rejects collisions where the same field id has already been set
/// (typically via the `fields` escape hatch on the MCP side or `--set-field`
/// on the CLI side). `other_source_label` is interpolated into the error
/// message to identify the colliding source — for example
/// `the same key inside fields` or ``--set-field`` of the same name``.
pub fn apply_user_field_overrides(
    fields: &mut BTreeMap<String, serde_json::Value>,
    assignee: Option<&str>,
    reporter: Option<&str>,
    other_source_label: &str,
) -> Result<()> {
    if let Some(value) = assignee {
        if fields.contains_key("assignee") {
            bail!("`assignee` collides with {other_source_label}; supply only one");
        }
        fields.insert("assignee".to_string(), user_field_value(value));
    }
    if let Some(value) = reporter {
        if fields.contains_key("reporter") {
            bail!("`reporter` collides with {other_source_label}; supply only one");
        }
        fields.insert("reporter".to_string(), user_field_value(value));
    }
    Ok(())
}

/// Merges CLI `--set-field` overrides into a frontmatter scalar map,
/// with CLI overriding frontmatter on name conflicts.
pub fn merge_set_field_overrides(
    frontmatter: BTreeMap<String, serde_yaml::Value>,
    overrides: Vec<(String, serde_yaml::Value)>,
) -> BTreeMap<String, serde_yaml::Value> {
    let mut merged = frontmatter;
    for (name, value) in overrides {
        merged.insert(name, value);
    }
    merged
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

    // ── user_field_value ──────────────────────────────────────

    #[test]
    fn user_field_value_empty_string_is_null() {
        assert_eq!(user_field_value(""), serde_json::Value::Null);
    }

    #[test]
    fn user_field_value_account_id_wrapped() {
        assert_eq!(
            user_field_value("abc123"),
            serde_json::json!({"accountId": "abc123"})
        );
    }

    #[test]
    fn user_field_value_dash_one_preserves_auto_assign() {
        assert_eq!(
            user_field_value("-1"),
            serde_json::json!({"accountId": "-1"})
        );
    }

    // ── apply_user_field_overrides ────────────────────────────

    #[test]
    fn apply_user_field_overrides_inserts_assignee_and_reporter() {
        let mut fields = BTreeMap::new();
        apply_user_field_overrides(&mut fields, Some("a1"), Some("r1"), "ignored").unwrap();
        assert_eq!(
            fields.get("assignee"),
            Some(&serde_json::json!({"accountId": "a1"}))
        );
        assert_eq!(
            fields.get("reporter"),
            Some(&serde_json::json!({"accountId": "r1"}))
        );
    }

    #[test]
    fn apply_user_field_overrides_skips_none() {
        let mut fields = BTreeMap::new();
        apply_user_field_overrides(&mut fields, None, None, "ignored").unwrap();
        assert!(fields.is_empty());
    }

    #[test]
    fn apply_user_field_overrides_empty_string_clears() {
        let mut fields = BTreeMap::new();
        apply_user_field_overrides(&mut fields, Some(""), None, "ignored").unwrap();
        assert_eq!(fields.get("assignee"), Some(&serde_json::Value::Null));
    }

    #[test]
    fn apply_user_field_overrides_assignee_collision_errors() {
        let mut fields = BTreeMap::new();
        fields.insert(
            "assignee".to_string(),
            serde_json::json!({"accountId": "existing"}),
        );
        let err = apply_user_field_overrides(&mut fields, Some("new"), None, "the test source")
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("assignee"));
        assert!(msg.contains("the test source"));
    }

    #[test]
    fn apply_user_field_overrides_reporter_collision_errors() {
        let mut fields = BTreeMap::new();
        fields.insert(
            "reporter".to_string(),
            serde_json::json!({"accountId": "existing"}),
        );
        let err = apply_user_field_overrides(&mut fields, None, Some("new"), "the test source")
            .unwrap_err();
        assert!(err.to_string().contains("reporter"));
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
    fn scalar_string_to_rich_text_field_converts_jfm_to_adf() {
        // Issue #866: a string scalar targeting a textarea custom field is
        // treated as JFM markdown and converted to ADF.
        let editmeta = meta(&[(
            "customfield_19300",
            "Acceptance Criteria",
            "string",
            Some(CUSTOM_TEXTAREA),
        )]);
        let mut scalars = BTreeMap::new();
        scalars.insert(
            "Acceptance Criteria".to_string(),
            serde_yaml::Value::String("- one\n- two".to_string()),
        );
        let out = resolve_custom_fields(&scalars, &[], &editmeta).unwrap();
        let value = out.get("customfield_19300").unwrap();
        assert_eq!(value["type"], "doc");
        assert_eq!(value["version"], 1);
        assert!(value["content"].is_array());
    }

    #[test]
    fn scalar_empty_string_to_rich_text_field_clears() {
        let editmeta = meta(&[(
            "customfield_19300",
            "Acceptance Criteria",
            "string",
            Some(CUSTOM_TEXTAREA),
        )]);
        let mut scalars = BTreeMap::new();
        scalars.insert(
            "Acceptance Criteria".to_string(),
            serde_yaml::Value::String(String::new()),
        );
        let out = resolve_custom_fields(&scalars, &[], &editmeta).unwrap();
        assert_eq!(
            out.get("customfield_19300").unwrap(),
            &serde_json::Value::Null
        );
    }

    #[test]
    fn scalar_yaml_null_to_rich_text_field_clears() {
        // Distinct from the empty-string case: the CLI's `--set-field Name=`
        // parses the empty RHS as YAML null (not a string), so this arm
        // covers the production code path callers actually traverse to
        // clear a rich-text field from the command line.
        let editmeta = meta(&[(
            "customfield_19300",
            "Acceptance Criteria",
            "string",
            Some(CUSTOM_TEXTAREA),
        )]);
        let mut scalars = BTreeMap::new();
        scalars.insert("Acceptance Criteria".to_string(), serde_yaml::Value::Null);
        let out = resolve_custom_fields(&scalars, &[], &editmeta).unwrap();
        assert_eq!(
            out.get("customfield_19300").unwrap(),
            &serde_json::Value::Null
        );
    }

    #[test]
    fn scalar_non_string_to_rich_text_field_errors() {
        // Non-string scalars (numbers, bools, mappings, sequences) targeting
        // a rich-text field still need a body section / JFM string.
        let editmeta = meta(&[(
            "customfield_19300",
            "Acceptance Criteria",
            "string",
            Some(CUSTOM_TEXTAREA),
        )]);
        let mut scalars = BTreeMap::new();
        scalars.insert(
            "Acceptance Criteria".to_string(),
            serde_yaml::Value::Number(42.into()),
        );
        let err = resolve_custom_fields(&scalars, &[], &editmeta).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("rich-text field"), "got: {msg}");
        assert!(msg.contains("JFM markdown"), "got: {msg}");
    }

    #[test]
    fn scalar_string_with_invalid_adf_nesting_to_rich_text_field_errors() {
        let editmeta = meta(&[(
            "customfield_19300",
            "Acceptance Criteria",
            "string",
            Some(CUSTOM_TEXTAREA),
        )]);
        let mut scalars = BTreeMap::new();
        scalars.insert(
            "Acceptance Criteria".to_string(),
            serde_yaml::Value::String(
                ":::panel{type=info}\n:::expand{title=\"x\"}\nbody\n:::\n:::".to_string(),
            ),
        );
        let err = resolve_custom_fields(&scalars, &[], &editmeta).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Acceptance Criteria"));
        assert!(msg.contains("ADF nesting validation"));
        assert!(msg.contains("`expand` cannot be a child of `panel`"));
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
    fn rich_text_section_with_invalid_adf_nesting_errors() {
        // Issue #714: a section whose body produces ADF that violates
        // Confluence's nesting constraints (here panel→expand) must be
        // rejected with the validation context, not silently included in the
        // payload.
        let editmeta = meta(&[(
            "customfield_19300",
            "Acceptance Criteria",
            "string",
            Some(CUSTOM_TEXTAREA),
        )]);
        let sections = [CustomFieldSection {
            name: "Acceptance Criteria".to_string(),
            id: "customfield_19300".to_string(),
            body: ":::panel{type=info}\n:::expand{title=\"x\"}\nbody\n:::\n:::".to_string(),
        }];
        let err = resolve_custom_fields(&BTreeMap::new(), &sections, &editmeta).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Acceptance Criteria"));
        assert!(msg.contains("ADF nesting validation"));
        assert!(msg.contains("`expand` cannot be a child of `panel`"));
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
    fn ambiguous_field_name_errors_listing_ids() {
        let editmeta = meta(&[
            ("customfield_1", "Duplicate", "string", None),
            ("customfield_2", "Duplicate", "string", None),
        ]);
        let mut scalars = BTreeMap::new();
        scalars.insert("Duplicate".to_string(), serde_yaml::Value::from("x"));
        let err = resolve_custom_fields(&scalars, &[], &editmeta).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Ambiguous"));
        assert!(msg.contains("customfield_1"));
        assert!(msg.contains("customfield_2"));
    }

    #[test]
    fn array_field_requires_sequence_value() {
        let editmeta = meta(&[("customfield_10004", "Components", "array", None)]);
        let mut scalars = BTreeMap::new();
        scalars.insert(
            "Components".to_string(),
            serde_yaml::Value::String("not-a-sequence".to_string()),
        );
        let err = resolve_custom_fields(&scalars, &[], &editmeta).unwrap_err();
        assert!(format!("{err:#}").contains("expected a sequence"));
    }

    #[test]
    fn array_element_must_be_scalar_string() {
        let editmeta = meta(&[("customfield_10004", "Components", "array", None)]);
        let mut scalars = BTreeMap::new();
        scalars.insert(
            "Components".to_string(),
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::Sequence(vec![
                serde_yaml::Value::String("nested".to_string()),
            ])]),
        );
        let err = resolve_custom_fields(&scalars, &[], &editmeta).unwrap_err();
        assert!(format!("{err:#}").contains("expected a scalar string value"));
    }

    #[test]
    fn unsupported_schema_type_errors_with_field_name() {
        let editmeta = meta(&[("customfield_20000", "Reporter", "user", None)]);
        let mut scalars = BTreeMap::new();
        scalars.insert("Reporter".to_string(), serde_yaml::Value::from("alice"));
        let err = resolve_custom_fields(&scalars, &[], &editmeta).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Unsupported field type 'user'"));
        assert!(msg.contains("Reporter"));
    }

    #[test]
    fn option_field_accepts_bool_and_number_scalars() {
        let editmeta = meta(&[
            (
                "customfield_bool",
                "Toggle",
                "option",
                Some("com.atlassian.jira.plugin.system.customfieldtypes:select"),
            ),
            (
                "customfield_num",
                "Number choice",
                "option",
                Some("com.atlassian.jira.plugin.system.customfieldtypes:select"),
            ),
        ]);
        let mut scalars = BTreeMap::new();
        scalars.insert("Toggle".to_string(), serde_yaml::Value::Bool(true));
        scalars.insert(
            "Number choice".to_string(),
            serde_yaml::Value::Number(3.into()),
        );
        let out = resolve_custom_fields(&scalars, &[], &editmeta).unwrap();
        assert_eq!(
            out.get("customfield_bool").unwrap(),
            &serde_json::json!({"value": "true"})
        );
        assert_eq!(
            out.get("customfield_num").unwrap(),
            &serde_json::json!({"value": "3"})
        );
    }

    #[test]
    fn option_field_rejects_non_scalar_value() {
        let editmeta = meta(&[("customfield_opt", "Opt", "option", None)]);
        let mut mapping = serde_yaml::Mapping::new();
        mapping.insert(serde_yaml::Value::from("k"), serde_yaml::Value::from("v"));
        let mut scalars = BTreeMap::new();
        scalars.insert("Opt".to_string(), serde_yaml::Value::Mapping(mapping));
        let err = resolve_custom_fields(&scalars, &[], &editmeta).unwrap_err();
        assert!(format!("{err:#}").contains("expected a scalar string value"));
    }

    #[test]
    fn section_with_stale_id_falls_back_to_name_lookup() {
        // editmeta has the field under a new id; the section tag carries an
        // older id. Resolver should fall back to name lookup and find it.
        let editmeta = meta(&[(
            "customfield_NEW",
            "Acceptance Criteria",
            "string",
            Some(CUSTOM_TEXTAREA),
        )]);
        let sections = [CustomFieldSection {
            name: "Acceptance Criteria".to_string(),
            id: "customfield_OLD".to_string(),
            body: "body".to_string(),
        }];
        let out = resolve_custom_fields(&BTreeMap::new(), &sections, &editmeta).unwrap();
        assert!(out.contains_key("customfield_NEW"));
        assert!(!out.contains_key("customfield_OLD"));
    }

    #[test]
    fn field_id_that_does_not_exist_falls_through_to_name_lookup() {
        // A `customfield_<digits>` key that isn't in editmeta should still
        // try a name lookup before erroring.
        let editmeta = meta(&[("customfield_ACTUAL", "My Field", "string", None)]);
        let mut scalars = BTreeMap::new();
        scalars.insert("customfield_999".to_string(), serde_yaml::Value::from("x"));
        let err = resolve_custom_fields(&scalars, &[], &editmeta).unwrap_err();
        assert!(err.to_string().contains("Unknown custom field"));
    }

    // ── parse_set_field / merge_set_field_overrides ─────────────────

    #[test]
    fn parse_set_field_bare_string_value() {
        let (name, value) = parse_set_field("Status=Open").unwrap();
        assert_eq!(name, "Status");
        assert_eq!(value, serde_yaml::Value::String("Open".to_string()));
    }

    #[test]
    fn parse_set_field_numeric_value_becomes_number() {
        let (_name, value) = parse_set_field("Points=8").unwrap();
        assert_eq!(value, serde_yaml::Value::Number(8.into()));
    }

    #[test]
    fn parse_set_field_bool_value_becomes_bool() {
        let (_name, value) = parse_set_field("Enabled=true").unwrap();
        assert_eq!(value, serde_yaml::Value::Bool(true));
    }

    #[test]
    fn parse_set_field_preserves_spaces_in_name() {
        let (name, value) = parse_set_field("Planned / Unplanned Work=Unplanned").unwrap();
        assert_eq!(name, "Planned / Unplanned Work");
        assert_eq!(value, serde_yaml::Value::String("Unplanned".to_string()));
    }

    #[test]
    fn parse_set_field_equals_in_value_preserved() {
        // Only the FIRST `=` splits name from value.
        let (name, value) = parse_set_field("Formula=a=b+c").unwrap();
        assert_eq!(name, "Formula");
        assert_eq!(value, serde_yaml::Value::String("a=b+c".to_string()));
    }

    #[test]
    fn parse_set_field_requires_equals() {
        let err = parse_set_field("just-a-name").unwrap_err();
        assert!(err.to_string().contains("expected --set-field"));
    }

    #[test]
    fn parse_set_field_empty_name_errors() {
        let err = parse_set_field("=value").unwrap_err();
        assert!(err.to_string().contains("non-empty name"));
    }

    #[test]
    fn merge_set_field_overrides_cli_wins() {
        let mut frontmatter = BTreeMap::new();
        frontmatter.insert(
            "Priority".to_string(),
            serde_yaml::Value::String("Low".to_string()),
        );
        frontmatter.insert(
            "Keep".to_string(),
            serde_yaml::Value::String("from-fm".to_string()),
        );
        let overrides = vec![(
            "Priority".to_string(),
            serde_yaml::Value::String("High".to_string()),
        )];
        let merged = merge_set_field_overrides(frontmatter, overrides);
        assert_eq!(
            merged.get("Priority"),
            Some(&serde_yaml::Value::String("High".to_string()))
        );
        assert_eq!(
            merged.get("Keep"),
            Some(&serde_yaml::Value::String("from-fm".to_string()))
        );
    }

    #[test]
    fn merge_set_field_overrides_with_empty_overrides_preserves_frontmatter() {
        let mut frontmatter = BTreeMap::new();
        frontmatter.insert("K".to_string(), serde_yaml::Value::from("v"));
        let merged = merge_set_field_overrides(frontmatter, vec![]);
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged.get("K"),
            Some(&serde_yaml::Value::String("v".to_string()))
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

    // ── convert_textarea_string_values ────────────────────────────────

    #[test]
    fn convert_textarea_string_value_converts_to_adf() {
        let editmeta = meta(&[(
            "customfield_19300",
            "Acceptance Criteria",
            "string",
            Some(CUSTOM_TEXTAREA),
        )]);
        let mut fields = BTreeMap::new();
        fields.insert(
            "customfield_19300".to_string(),
            serde_json::Value::String("- one\n- two".to_string()),
        );
        convert_textarea_string_values(&mut fields, &editmeta).unwrap();
        let value = fields.get("customfield_19300").unwrap();
        assert_eq!(value["type"], "doc");
        assert_eq!(value["version"], 1);
        assert!(value["content"].is_array());
    }

    #[test]
    fn convert_textarea_object_value_passes_through() {
        let editmeta = meta(&[(
            "customfield_19300",
            "Acceptance Criteria",
            "string",
            Some(CUSTOM_TEXTAREA),
        )]);
        let raw_adf = serde_json::json!({
            "version": 1,
            "type": "doc",
            "content": [{"type": "paragraph", "content": [{"type": "text", "text": "x"}]}]
        });
        let mut fields = BTreeMap::new();
        fields.insert("customfield_19300".to_string(), raw_adf.clone());
        convert_textarea_string_values(&mut fields, &editmeta).unwrap();
        assert_eq!(fields.get("customfield_19300").unwrap(), &raw_adf);
    }

    #[test]
    fn convert_textarea_empty_string_clears_field() {
        let editmeta = meta(&[(
            "customfield_19300",
            "Acceptance Criteria",
            "string",
            Some(CUSTOM_TEXTAREA),
        )]);
        let mut fields = BTreeMap::new();
        fields.insert(
            "customfield_19300".to_string(),
            serde_json::Value::String(String::new()),
        );
        convert_textarea_string_values(&mut fields, &editmeta).unwrap();
        assert_eq!(
            fields.get("customfield_19300").unwrap(),
            &serde_json::Value::Null
        );
    }

    #[test]
    fn convert_non_textarea_string_passes_through() {
        let editmeta = meta(&[("customfield_10010", "Some Text", "string", None)]);
        let mut fields = BTreeMap::new();
        fields.insert(
            "customfield_10010".to_string(),
            serde_json::Value::String("plain".to_string()),
        );
        convert_textarea_string_values(&mut fields, &editmeta).unwrap();
        assert_eq!(
            fields.get("customfield_10010").unwrap(),
            &serde_json::Value::String("plain".to_string())
        );
    }

    #[test]
    fn convert_unknown_field_passes_through() {
        // Field id not present in editmeta — leave the value alone and let the
        // API surface its own error.
        let editmeta = meta(&[("customfield_OTHER", "Other", "string", None)]);
        let mut fields = BTreeMap::new();
        fields.insert(
            "customfield_99999".to_string(),
            serde_json::Value::String("- a".to_string()),
        );
        convert_textarea_string_values(&mut fields, &editmeta).unwrap();
        assert_eq!(
            fields.get("customfield_99999").unwrap(),
            &serde_json::Value::String("- a".to_string())
        );
    }

    #[test]
    fn convert_textarea_non_string_non_object_passes_through() {
        // Numbers, bools, arrays, nulls are not coerced — those are not
        // legitimate textarea payloads and the API will tell the caller.
        let editmeta = meta(&[(
            "customfield_19300",
            "Acceptance Criteria",
            "string",
            Some(CUSTOM_TEXTAREA),
        )]);
        let mut fields = BTreeMap::new();
        fields.insert(
            "customfield_19300".to_string(),
            serde_json::Value::Number(42.into()),
        );
        convert_textarea_string_values(&mut fields, &editmeta).unwrap();
        assert_eq!(
            fields.get("customfield_19300").unwrap(),
            &serde_json::Value::Number(42.into())
        );
    }

    #[test]
    fn convert_textarea_invalid_adf_nesting_errors() {
        let editmeta = meta(&[(
            "customfield_19300",
            "Acceptance Criteria",
            "string",
            Some(CUSTOM_TEXTAREA),
        )]);
        let mut fields = BTreeMap::new();
        fields.insert(
            "customfield_19300".to_string(),
            serde_json::Value::String(
                ":::panel{type=info}\n:::expand{title=\"x\"}\nbody\n:::\n:::".to_string(),
            ),
        );
        let err = convert_textarea_string_values(&mut fields, &editmeta).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Acceptance Criteria"));
        assert!(msg.contains("ADF nesting validation"));
        assert!(msg.contains("`expand` cannot be a child of `panel`"));
    }
}
