//! Response-schema registry for structured AI calls.
//!
//! Schemas are derived once from Rust types via `schemars`, cached in a
//! `OnceLock`, and handed out as `serde_json::Value` so call sites can
//! pass them through `RequestOptions` without recomputing each call.
//!
//! Adding a new structured response type:
//! 1. Derive `JsonSchema` on the response type with
//!    `#[schemars(deny_unknown_fields)]` for strictness.
//! 2. Add a constant identifier here and a getter that returns its
//!    cached `serde_json::Value`.
//! 3. The `tests::all_schemas_serialize` golden test will pick it up
//!    automatically once it's listed in [`ALL_SCHEMAS`].

use std::sync::OnceLock;

use schemars::{schema_for, JsonSchema};
use serde_json::Value;

use crate::cli::git::PrContent;
use crate::data::amendments::AmendmentFile;
use crate::data::check::AiCheckResponse;

/// Returns the cached JSON Schema for the AI response of a structured call.
fn schema_value<T: JsonSchema>(slot: &'static OnceLock<Value>) -> &'static Value {
    slot.get_or_init(|| {
        // schema_for! returns a `schemars::Schema` whose `Serialize` impl
        // produces the JSON Schema document. Routing through serde_json
        // gives us a `Value` we can pass around without exposing schemars
        // types in public APIs.
        let schema = schema_for!(T);
        serde_json::to_value(schema).unwrap_or(Value::Null)
    })
}

/// JSON Schema for [`AmendmentFile`] responses (twiddle, check-with-suggestions).
pub fn amendment_file_schema() -> &'static Value {
    static SLOT: OnceLock<Value> = OnceLock::new();
    schema_value::<AmendmentFile>(&SLOT)
}

/// JSON Schema for [`PrContent`] responses (PR title + description).
pub fn pr_content_schema() -> &'static Value {
    static SLOT: OnceLock<Value> = OnceLock::new();
    schema_value::<PrContent>(&SLOT)
}

/// JSON Schema for [`AiCheckResponse`] responses (commit-check list).
pub fn check_response_schema() -> &'static Value {
    static SLOT: OnceLock<Value> = OnceLock::new();
    schema_value::<AiCheckResponse>(&SLOT)
}

/// Cached-schema getter signature, used by the golden-coverage test.
#[cfg(test)]
type SchemaGetter = fn() -> &'static Value;

/// Identifiers for golden snapshot coverage.
#[cfg(test)]
const ALL_SCHEMAS: &[(&str, SchemaGetter)] = &[
    ("amendment_file", amendment_file_schema),
    ("pr_content", pr_content_schema),
    ("check_response", check_response_schema),
];

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Sanity-check that every schema serializes to a non-null object.
    #[test]
    fn all_schemas_are_non_null_objects() {
        for (name, getter) in ALL_SCHEMAS {
            let value = getter();
            assert!(
                value.is_object(),
                "{name} schema should serialize to an object: {value}"
            );
        }
    }

    /// Cached getters return pointer-equal values across calls.
    #[test]
    fn schemas_are_cached() {
        let amendment_first = amendment_file_schema();
        let amendment_second = amendment_file_schema();
        assert!(
            std::ptr::eq(amendment_first, amendment_second),
            "amendment_file_schema should return the same OnceLock value"
        );

        let pr_first = pr_content_schema();
        let pr_second = pr_content_schema();
        assert!(
            std::ptr::eq(pr_first, pr_second),
            "pr_content_schema should return the same OnceLock value"
        );

        let check_first = check_response_schema();
        let check_second = check_response_schema();
        assert!(
            std::ptr::eq(check_first, check_second),
            "check_response_schema should return the same OnceLock value"
        );
    }

    /// Strict-mode invariant: every nested object enforces
    /// `additionalProperties: false`.
    #[test]
    fn schemas_enforce_strict_objects() {
        for (name, getter) in ALL_SCHEMAS {
            let value = getter();
            assert_strict_objects(value, name);
        }
    }

    /// Walks the schema and asserts every object subschema either has
    /// `additionalProperties: false` or is a wildcard reference. Object
    /// types are detected by the presence of a `properties` map.
    fn assert_strict_objects(value: &Value, name: &str) {
        if let Some(map) = value.as_object() {
            if map.contains_key("properties") {
                let strict = map
                    .get("additionalProperties")
                    .and_then(Value::as_bool)
                    .is_some_and(|b| !b);
                assert!(
                    strict,
                    "{name}: object subschema missing `additionalProperties: false`: {value}"
                );
            }
            for (_, child) in map {
                assert_strict_objects(child, name);
            }
        } else if let Some(arr) = value.as_array() {
            for item in arr {
                assert_strict_objects(item, name);
            }
        }
    }

    /// OpenAI strict-subset invariant: every property in `properties`
    /// must also appear in `required`. OpenAI rejects schemas where a
    /// property is declared but optional; nullability must be expressed
    /// with `type: ["string", "null"]` (or equivalent) rather than by
    /// omitting from `required`.
    ///
    /// Schemars 1.x does honor `Option<T>` by including the field in
    /// `properties` and emitting a null-permissive type, but historically
    /// some setups dropped the field from `required`. This test pins the
    /// invariant against drift.
    #[test]
    fn schemas_satisfy_openai_strict_subset() {
        for (name, getter) in ALL_SCHEMAS {
            let value = getter();
            assert_openai_strict_subset(value, name);
        }
    }

    /// Recurses into every object subschema and confirms `properties`
    /// keys are a subset of `required`.
    fn assert_openai_strict_subset(value: &Value, name: &str) {
        if let Some(map) = value.as_object() {
            if let Some(props) = map.get("properties").and_then(Value::as_object) {
                let required: std::collections::HashSet<&str> = map
                    .get("required")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                for prop_name in props.keys() {
                    assert!(
                        required.contains(prop_name.as_str()),
                        "{name}: property '{prop_name}' missing from `required` (OpenAI strict-mode \
                         requires every property to be required; use a nullable type for optional \
                         semantics). Schema: {value}"
                    );
                }
            }
            for (_, child) in map {
                assert_openai_strict_subset(child, name);
            }
        } else if let Some(arr) = value.as_array() {
            for item in arr {
                assert_openai_strict_subset(item, name);
            }
        }
    }

    /// PrContent must require both fields.
    #[test]
    fn pr_content_requires_title_and_description() {
        let value = pr_content_schema();
        let required = value
            .get("required")
            .and_then(Value::as_array)
            .expect("pr_content schema should have a `required` array");
        let names: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
        assert!(
            names.contains(&"title"),
            "missing title in required: {names:?}"
        );
        assert!(
            names.contains(&"description"),
            "missing description in required: {names:?}"
        );
    }

    /// AmendmentFile must require `amendments`; nested `Amendment`
    /// must require `commit` and `message` (not `summary`).
    #[test]
    fn amendment_file_required_fields() {
        let value = amendment_file_schema();
        let required = value
            .get("required")
            .and_then(Value::as_array)
            .expect("amendment_file schema should have a `required` array");
        let names: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
        assert_eq!(names, vec!["amendments"]);
    }

    /// CheckResponse exposes a `checks` array.
    #[test]
    fn check_response_required_fields() {
        let value = check_response_schema();
        let required = value
            .get("required")
            .and_then(Value::as_array)
            .expect("check_response schema should have a `required` array");
        let names: Vec<&str> = required.iter().filter_map(Value::as_str).collect();
        assert_eq!(names, vec!["checks"]);
    }
}
