//! Shared arguments, parsing, and output shaping for the `ai jev` leaves.

use std::collections::BTreeMap;
use std::io::Read as _;

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Serialize;

use crate::jev::protocol::{Answer, SystemOneResponse, Usage};

/// Output format selector for `ai jev` subcommands.
///
/// Named `JevFormat`, not `OutputFormat`: `crate::cli::ai` already re-exports
/// an `OutputFormat` (from `claude::skills::common`, variants `Text | Yaml`)
/// for a sibling command group, and a second one under the same `ai` group
/// with different variants would be confusing — do not re-export this type
/// from `crate::cli::ai`.
#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum JevFormat {
    /// Pretty-printed JSON (default).
    #[default]
    Json,
    /// YAML.
    Yaml,
}

/// The fixed internal key used for the one-entry `questions`/`answers` map on
/// the three single-question subcommands (`choice`/`score`/`noul`). `ask`
/// uses the caller-supplied names from its questions file instead.
pub(super) const SINGLE_QUESTION_KEY: &str = "answer";

/// Resolves the request state from an explicit `[STATE]` argument or stdin,
/// erroring if the result is empty or whitespace-only.
pub(super) fn resolve_state(state: Option<String>) -> Result<String> {
    let raw = match state {
        Some(s) => s,
        None => read_stdin()?,
    };
    if raw.trim().is_empty() {
        anyhow::bail!("state must not be empty (pass it as an argument or on stdin)");
    }
    Ok(raw)
}

/// Reads the whole of stdin as a UTF-8 string.
///
/// A local copy rather than a shared helper: the three existing
/// `read_stdin` helpers in the CLI tree are private to their own modules and
/// unsuitable here (`snowflake.rs` hardcodes a SQL-specific error message,
/// `drive/edit.rs` returns `Vec<u8>`, `worktrees.rs` reads a single line).
fn read_stdin() -> Result<String> {
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("Failed to read state from stdin")?;
    Ok(buf)
}

/// Converts the raw state string into the JSON value sent to Jev.
///
/// Default is a **literal string** — blind-parsing as JSON would silently
/// reinterpret plain text that happens to start with `{`. `--state-json`
/// opts into parsing `raw` as YAML (a JSON superset, so this also accepts
/// plain JSON) instead.
///
/// The API accepts a string, object or array, so a `--state-json` parse
/// that yields any other scalar (`42`, `true`, `null`) is rejected locally
/// rather than sent for an opaque `422`. A parsed string is allowed — it is
/// exactly what the default would have sent.
pub(super) fn build_state_value(raw: &str, state_json: bool) -> Result<serde_json::Value> {
    if !state_json {
        return Ok(serde_json::Value::String(raw.to_string()));
    }
    let value: serde_json::Value =
        serde_yaml::from_str(raw).context("Failed to parse --state-json state as JSON/YAML")?;
    match value {
        serde_json::Value::String(_)
        | serde_json::Value::Object(_)
        | serde_json::Value::Array(_) => Ok(value),
        other => anyhow::bail!(
            "--state-json state must be an object, array or string, got `{other}`; \
             drop --state-json to send it as a literal string"
        ),
    }
}

/// Output payload for the three single-question subcommands: `answer` is the
/// one entry of the response's `answers` map, unwrapped.
#[derive(Serialize)]
struct SingleAnswerOutput<'a> {
    model: &'a str,
    answer: &'a Answer,
    usage: Usage,
}

/// Output payload for `ask`.
#[derive(Serialize)]
struct MultiAnswerOutput<'a> {
    model: &'a str,
    answers: &'a BTreeMap<String, Answer>,
    usage: Usage,
}

/// Extracts the sole answer from a single-question response, keyed by
/// [`SINGLE_QUESTION_KEY`].
fn single_answer(response: &SystemOneResponse) -> Result<&Answer> {
    response.answers.get(SINGLE_QUESTION_KEY).with_context(|| {
        format!(
            "Jev response did not include an answer for the expected key {SINGLE_QUESTION_KEY:?}"
        )
    })
}

/// Formats a single-question response (`choice`/`score`/`noul`) as JSON or YAML.
pub(super) fn format_single(response: &SystemOneResponse, format: JevFormat) -> Result<String> {
    let answer = single_answer(response)?;
    let output = SingleAnswerOutput {
        model: &response.model,
        answer,
        usage: response.usage,
    };
    format_output(&output, format)
}

/// Formats a multi-question (`ask`) response as JSON or YAML.
pub(super) fn format_multi(response: &SystemOneResponse, format: JevFormat) -> Result<String> {
    let output = MultiAnswerOutput {
        model: &response.model,
        answers: &response.answers,
        usage: response.usage,
    };
    format_output(&output, format)
}

pub(super) fn format_output<T: Serialize>(value: &T, format: JevFormat) -> Result<String> {
    match format {
        JevFormat::Json => {
            let json = serde_json::to_string_pretty(value)
                .context("Failed to serialize Jev response as JSON")?;
            Ok(format!("{json}\n"))
        }
        JevFormat::Yaml => {
            serde_yaml::to_string(value).context("Failed to serialize Jev response as YAML")
        }
    }
}

/// Test-only helpers shared by every leaf's test module.
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(super) mod test_support {
    use crate::jev::client::JevClient;

    /// A client pointed at an unreachable local address. Used to exercise
    /// validation-before-send paths (e.g. duplicate `--option` names, too
    /// few `--level`s) without touching the network.
    pub(crate) fn dead_client() -> JevClient {
        JevClient::new("http://127.0.0.1:1", "test-key").unwrap()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn sample_response() -> SystemOneResponse {
        SystemOneResponse {
            model: "jev-1.13.0".to_string(),
            answers: BTreeMap::from([(
                SINGLE_QUESTION_KEY.to_string(),
                Answer::Noul { noul: 0.5 },
            )]),
            usage: Usage {
                input_tokens: 1,
                output_tokens: 2,
            },
        }
    }

    #[test]
    fn resolve_state_uses_explicit_argument() {
        let state = resolve_state(Some("hello".to_string())).unwrap();
        assert_eq!(state, "hello");
    }

    #[test]
    fn resolve_state_rejects_empty_argument() {
        let err = resolve_state(Some("   \n".to_string())).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn build_state_value_default_is_literal_string() {
        let value = build_state_value("{not valid json}", false).unwrap();
        assert_eq!(
            value,
            serde_json::Value::String("{not valid json}".to_string())
        );
    }

    #[test]
    fn build_state_value_json_flag_parses_json() {
        let value = build_state_value(r#"{"a": 1}"#, true).unwrap();
        assert_eq!(value, serde_json::json!({"a": 1}));
    }

    #[test]
    fn build_state_value_json_flag_parses_yaml() {
        let value = build_state_value("a: 1\nb: 2\n", true).unwrap();
        assert_eq!(value, serde_json::json!({"a": 1, "b": 2}));
    }

    #[test]
    fn build_state_value_json_flag_parses_array() {
        let value = build_state_value("[1, 2]", true).unwrap();
        assert_eq!(value, serde_json::json!([1, 2]));
    }

    #[test]
    fn build_state_value_json_flag_allows_a_bare_string() {
        let value = build_state_value("hello world", true).unwrap();
        assert_eq!(value, serde_json::json!("hello world"));
    }

    #[test]
    fn build_state_value_json_flag_rejects_non_string_scalars() {
        for raw in ["42", "true", "null", "1.5"] {
            let err = build_state_value(raw, true).unwrap_err();
            assert!(
                err.to_string()
                    .contains("must be an object, array or string"),
                "{raw}: {err}"
            );
        }
    }

    #[test]
    fn build_state_value_json_flag_rejects_invalid_yaml() {
        let err = build_state_value("a: [unterminated", true).unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    #[test]
    fn single_answer_extracts_the_one_entry() {
        let response = sample_response();
        let answer = single_answer(&response).unwrap();
        assert_eq!(*answer, Answer::Noul { noul: 0.5 });
    }

    #[test]
    fn single_answer_missing_key_errors() {
        let response = SystemOneResponse {
            model: "jev-1.13.0".to_string(),
            answers: BTreeMap::new(),
            usage: Usage::default(),
        };
        let err = single_answer(&response).unwrap_err();
        assert!(err.to_string().contains(SINGLE_QUESTION_KEY));
    }

    #[test]
    fn format_single_json_unwraps_answer() {
        let out = format_single(&sample_response(), JevFormat::Json).unwrap();
        assert!(out.contains("\"model\": \"jev-1.13.0\""));
        assert!(out.contains("\"noul\": 0.5"));
        assert!(!out.contains("\"answer\": {\n    \"answer\""));
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn format_single_yaml_unwraps_answer() {
        let out = format_single(&sample_response(), JevFormat::Yaml).unwrap();
        assert!(out.contains("model: jev-1.13.0"));
        assert!(out.contains("noul: 0.5"));
    }

    #[test]
    fn format_multi_json_keeps_answers_map() {
        let response = sample_response();
        let out = format_multi(&response, JevFormat::Json).unwrap();
        assert!(out.contains("\"answers\""));
        assert!(out.contains(SINGLE_QUESTION_KEY));
    }

    #[test]
    fn format_multi_yaml_keeps_answers_map() {
        let response = sample_response();
        let out = format_multi(&response, JevFormat::Yaml).unwrap();
        assert!(out.contains("answers:"));
        assert!(out.contains(SINGLE_QUESTION_KEY));
    }

    #[test]
    fn jev_format_value_enum_names_are_lowercase() {
        assert_eq!(
            JevFormat::Json.to_possible_value().unwrap().get_name(),
            "json"
        );
        assert_eq!(
            JevFormat::Yaml.to_possible_value().unwrap().get_name(),
            "yaml"
        );
    }
}
