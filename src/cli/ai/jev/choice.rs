//! `ai jev choice` — picks one of a labelled set of options.

use std::collections::BTreeMap;

use anyhow::{bail, Result};
use clap::Parser;

use crate::jev::client::JevClient;
use crate::jev::config::JevConfig;
use crate::jev::protocol::{Question, SystemOneRequest};

use super::common::{
    build_state_value, format_single, resolve_state, JevFormat, SINGLE_QUESTION_KEY,
};

/// Picks one of a labelled set of options for the given state.
#[derive(Parser)]
pub struct ChoiceCommand {
    /// The state to judge. Read from stdin if omitted.
    pub state: Option<String>,

    /// Parses `state` as JSON/YAML instead of sending it as a literal string.
    #[arg(long)]
    pub state_json: bool,

    /// What to choose and why.
    #[arg(long)]
    pub instructions: String,

    /// A `NAME=DESCRIPTION` option. Repeatable; at least one is required.
    #[arg(long = "option", value_name = "NAME=DESC", value_parser = parse_option_kv)]
    pub option: Vec<(String, String)>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = JevFormat::Json)]
    pub(super) output: JevFormat,

    /// Overrides the configured Jev model for this call.
    ///
    /// Distinct from the global `--model` flag, which does not apply to jev
    /// (see `omni-dev ai jev --help`).
    #[arg(long, value_name = "MODEL")]
    pub jev_model: Option<String>,
}

impl ChoiceCommand {
    /// Executes the choice command.
    pub async fn execute(self) -> Result<()> {
        let mut config = JevConfig::from_env()?;
        if let Some(model) = self.jev_model {
            config.model = model;
        }
        let client = JevClient::from_config(&config)?;
        let output = run_choice(
            &client,
            &config.model,
            self.state,
            self.state_json,
            &self.instructions,
            self.option,
            self.output,
        )
        .await?;
        println!("{output}");
        Ok(())
    }
}

/// Parses a `NAME=DESC` `--option` value.
///
/// Splits on the **first** `=` only, so a later `=` stays inside `DESC`; an
/// empty `NAME` is rejected. Wired as a clap `value_parser`.
fn parse_option_kv(s: &str) -> Result<(String, String), String> {
    let (name, desc) = s
        .split_once('=')
        .ok_or_else(|| format!("`{s}` is not in NAME=DESC form"))?;
    if name.is_empty() {
        return Err(format!("`{s}` has an empty option name"));
    }
    Ok((name.to_string(), desc.to_string()))
}

/// Builds and sends the choice request, returning the formatted output.
///
/// Pure validation (at-least-one option, no duplicate names) happens here
/// rather than in `execute`, so it is testable without an env or a network
/// call (STYLE-0025).
#[allow(clippy::too_many_arguments)]
async fn run_choice(
    client: &JevClient,
    model: &str,
    state: Option<String>,
    state_json: bool,
    instructions: &str,
    options: Vec<(String, String)>,
    format: JevFormat,
) -> Result<String> {
    if options.is_empty() {
        bail!("at least one --option NAME=DESC is required");
    }
    let mut criteria = BTreeMap::new();
    for (name, desc) in options {
        if criteria.insert(name.clone(), desc).is_some() {
            bail!("duplicate --option name: {name}");
        }
    }

    let raw_state = resolve_state(state)?;
    let state_value = build_state_value(&raw_state, state_json)?;

    let request = SystemOneRequest {
        state: state_value,
        model: model.to_string(),
        questions: BTreeMap::from([(
            SINGLE_QUESTION_KEY.to_string(),
            Question::Choice {
                instructions: instructions.to_string(),
                criteria,
            },
        )]),
    };

    let response = client.system_one(&request).await?;
    format_single(&response, format)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::common::test_support::dead_client;
    use super::*;

    // ── parse_option_kv ─────────────────────────────────────────────

    #[test]
    fn parse_option_kv_splits_name_and_description() {
        assert_eq!(
            parse_option_kv("billing=Payments").unwrap(),
            ("billing".to_string(), "Payments".to_string())
        );
    }

    #[test]
    fn parse_option_kv_keeps_later_equals_in_description() {
        assert_eq!(
            parse_option_kv("a=b=c").unwrap(),
            ("a".to_string(), "b=c".to_string())
        );
    }

    #[test]
    fn parse_option_kv_rejects_missing_equals() {
        let err = parse_option_kv("noequals").unwrap_err();
        assert!(err.contains("NAME=DESC"));
    }

    #[test]
    fn parse_option_kv_rejects_empty_name() {
        let err = parse_option_kv("=desc").unwrap_err();
        assert!(err.contains("empty option name"));
    }

    #[test]
    fn parse_option_kv_allows_empty_description() {
        assert_eq!(
            parse_option_kv("name=").unwrap(),
            ("name".to_string(), String::new())
        );
    }

    // ── run_choice validation ───────────────────────────────────────

    #[tokio::test]
    async fn run_choice_requires_at_least_one_option() {
        let err = run_choice(
            &dead_client(),
            "jev-latest",
            Some("state".to_string()),
            false,
            "route this",
            Vec::new(),
            JevFormat::Json,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("at least one --option"));
    }

    #[tokio::test]
    async fn run_choice_rejects_duplicate_option_names() {
        let err = run_choice(
            &dead_client(),
            "jev-latest",
            Some("state".to_string()),
            false,
            "route this",
            vec![
                ("billing".to_string(), "Payments".to_string()),
                ("billing".to_string(), "Other".to_string()),
            ],
            JevFormat::Json,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("duplicate --option name: billing"));
    }

    #[tokio::test]
    async fn run_choice_rejects_empty_state() {
        let err = run_choice(
            &dead_client(),
            "jev-latest",
            Some("   ".to_string()),
            false,
            "route this",
            vec![("billing".to_string(), "Payments".to_string())],
            JevFormat::Json,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    // ── wiremock ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_choice_sends_expected_request_and_parses_response() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "state": "payouts failing 3 days",
                "model": "jev-latest",
                "questions": {
                    "answer": {
                        "type": "choice",
                        "instructions": "Route this",
                        "criteria": {"billing": "Payments", "technical": "Bugs"}
                    }
                }
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {
                        "answer": {
                            "type": "choice",
                            "choice": "billing",
                            "confidence": 0.97,
                            "probabilities": {"billing": 0.97, "technical": 0.03}
                        }
                    },
                    "usage": {"input_tokens": 10, "output_tokens": 5}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = JevClient::new(&server.uri(), "my-key").unwrap();
        let out = run_choice(
            &client,
            "jev-latest",
            Some("payouts failing 3 days".to_string()),
            false,
            "Route this",
            vec![
                ("billing".to_string(), "Payments".to_string()),
                ("technical".to_string(), "Bugs".to_string()),
            ],
            JevFormat::Json,
        )
        .await
        .unwrap();
        assert!(out.contains("\"choice\": \"billing\""));
        assert!(out.contains("\"model\": \"jev-1.13.0\""));
    }

    #[tokio::test]
    async fn run_choice_yaml_output() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {
                        "answer": {
                            "type": "choice",
                            "choice": "billing",
                            "confidence": 0.97,
                            "probabilities": {"billing": 0.97}
                        }
                    },
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })),
            )
            .mount(&server)
            .await;

        let client = JevClient::new(&server.uri(), "my-key").unwrap();
        let out = run_choice(
            &client,
            "jev-latest",
            Some("state".to_string()),
            false,
            "Route this",
            vec![("billing".to_string(), "Payments".to_string())],
            JevFormat::Yaml,
        )
        .await
        .unwrap();
        assert!(out.contains("choice: billing"));
        assert!(out.contains("model: jev-1.13.0"));
    }

    // ── clap parsing ────────────────────────────────────────────────

    #[test]
    fn clap_parses_repeated_options_and_short_output_flag() {
        let cmd = ChoiceCommand::try_parse_from([
            "choice",
            "state text",
            "--instructions",
            "Route this",
            "--option",
            "billing=Payments",
            "--option",
            "technical=Bugs",
            "-o",
            "yaml",
        ])
        .unwrap();
        assert_eq!(cmd.state.as_deref(), Some("state text"));
        assert_eq!(
            cmd.option,
            vec![
                ("billing".to_string(), "Payments".to_string()),
                ("technical".to_string(), "Bugs".to_string()),
            ]
        );
        assert_eq!(cmd.output, JevFormat::Yaml);
    }

    #[test]
    fn clap_rejects_malformed_option() {
        let err = ChoiceCommand::try_parse_from([
            "choice",
            "state text",
            "--instructions",
            "x",
            "--option",
            "noequals",
        ])
        .err()
        .expect("expected a parse error");
        assert!(err.to_string().contains("NAME=DESC"));
    }
}
