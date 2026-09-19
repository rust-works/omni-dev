//! `ai jev noul` — estimates the probability of a yes/no condition.

use std::collections::BTreeMap;

use anyhow::Result;
use clap::Parser;

use crate::jev::client::JevClient;
use crate::jev::config::JevConfig;
use crate::jev::protocol::{NoulCriteria, Question, SystemOneRequest};

use super::common::{
    build_state_value, format_single, resolve_state, JevFormat, SINGLE_QUESTION_KEY,
};

/// Estimates the probability of a yes/no condition for the given state.
#[derive(Parser)]
pub struct NoulCommand {
    /// The state to judge. Read from stdin if omitted.
    pub state: Option<String>,

    /// Parses `state` as JSON/YAML instead of sending it as a literal string.
    #[arg(long)]
    pub state_json: bool,

    /// The yes/no condition to estimate.
    #[arg(long)]
    pub instructions: String,

    /// What a `true`-leaning answer means.
    #[arg(long = "true-means", value_name = "TEXT")]
    pub true_means: Option<String>,

    /// What a `false`-leaning answer means.
    #[arg(long = "false-means", value_name = "TEXT")]
    pub false_means: Option<String>,

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

impl NoulCommand {
    /// Executes the noul command.
    pub async fn execute(self) -> Result<()> {
        let mut config = JevConfig::from_env()?;
        if let Some(model) = self.jev_model {
            config.model = model;
        }
        let client = JevClient::from_config(&config)?;
        let output = run_noul(
            &client,
            &config.model,
            self.state,
            self.state_json,
            &self.instructions,
            self.true_means,
            self.false_means,
            self.output,
        )
        .await?;
        println!("{output}");
        Ok(())
    }
}

/// Builds and sends the noul request, returning the formatted output.
///
/// `criteria` is only emitted when at least one of `true_means`/`false_means`
/// is set, matching [`Question::Noul`]'s `skip_serializing_if` on the field.
#[allow(clippy::too_many_arguments)]
async fn run_noul(
    client: &JevClient,
    model: &str,
    state: Option<String>,
    state_json: bool,
    instructions: &str,
    true_means: Option<String>,
    false_means: Option<String>,
    format: JevFormat,
) -> Result<String> {
    let raw_state = resolve_state(state)?;
    let state_value = build_state_value(&raw_state, state_json)?;

    let criteria = if true_means.is_some() || false_means.is_some() {
        Some(NoulCriteria {
            true_means,
            false_means,
        })
    } else {
        None
    };

    let request = SystemOneRequest {
        state: state_value,
        model: model.to_string(),
        questions: BTreeMap::from([(
            SINGLE_QUESTION_KEY.to_string(),
            Question::Noul {
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
    use super::*;

    fn success_response() -> serde_json::Value {
        serde_json::json!({
            "model": "jev-1.13.0",
            "answers": {"answer": {"type": "noul", "noul": 0.5}},
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
    }

    // ── wiremock: criteria omitted vs present ───────────────────────

    #[tokio::test]
    async fn run_noul_omits_criteria_when_unset() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "state": "payouts failing 3 days",
                "model": "jev-latest",
                "questions": {
                    "answer": {"type": "noul", "instructions": "Urgent?"}
                }
            })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(success_response()))
            .expect(1)
            .mount(&server)
            .await;

        let client = JevClient::new(&server.uri(), "my-key").unwrap();
        let out = run_noul(
            &client,
            "jev-latest",
            Some("payouts failing 3 days".to_string()),
            false,
            "Urgent?",
            None,
            None,
            JevFormat::Json,
        )
        .await
        .unwrap();
        assert!(out.contains("\"noul\": 0.5"));
    }

    #[tokio::test]
    async fn run_noul_sends_criteria_when_either_side_set() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "state": "payouts failing 3 days",
                "model": "jev-latest",
                "questions": {
                    "answer": {
                        "type": "noul",
                        "instructions": "Urgent?",
                        "criteria": {"true": "Customer is frustrated"}
                    }
                }
            })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(success_response()))
            .expect(1)
            .mount(&server)
            .await;

        let client = JevClient::new(&server.uri(), "my-key").unwrap();
        let out = run_noul(
            &client,
            "jev-latest",
            Some("payouts failing 3 days".to_string()),
            false,
            "Urgent?",
            Some("Customer is frustrated".to_string()),
            None,
            JevFormat::Json,
        )
        .await
        .unwrap();
        assert!(out.contains("\"noul\": 0.5"));
    }

    #[tokio::test]
    async fn run_noul_sends_both_criteria_sides() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "state": "state",
                "model": "jev-latest",
                "questions": {
                    "answer": {
                        "type": "noul",
                        "instructions": "Urgent?",
                        "criteria": {
                            "true": "Customer is frustrated",
                            "false": "Customer is calm"
                        }
                    }
                }
            })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(success_response()))
            .expect(1)
            .mount(&server)
            .await;

        let client = JevClient::new(&server.uri(), "my-key").unwrap();
        run_noul(
            &client,
            "jev-latest",
            Some("state".to_string()),
            false,
            "Urgent?",
            Some("Customer is frustrated".to_string()),
            Some("Customer is calm".to_string()),
            JevFormat::Json,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_noul_yaml_output() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(success_response()))
            .mount(&server)
            .await;

        let client = JevClient::new(&server.uri(), "my-key").unwrap();
        let out = run_noul(
            &client,
            "jev-latest",
            Some("state".to_string()),
            false,
            "Urgent?",
            None,
            None,
            JevFormat::Yaml,
        )
        .await
        .unwrap();
        assert!(out.contains("noul: 0.5"));
    }

    // ── clap parsing ────────────────────────────────────────────────

    #[test]
    fn clap_parses_true_false_means_and_short_output_flag() {
        let cmd = NoulCommand::try_parse_from([
            "noul",
            "state text",
            "--instructions",
            "Urgent?",
            "--true-means",
            "frustrated",
            "--false-means",
            "calm",
            "-o",
            "yaml",
        ])
        .unwrap();
        assert_eq!(cmd.true_means.as_deref(), Some("frustrated"));
        assert_eq!(cmd.false_means.as_deref(), Some("calm"));
        assert_eq!(cmd.output, JevFormat::Yaml);
    }

    #[test]
    fn clap_true_false_means_default_to_none() {
        let cmd = NoulCommand::try_parse_from(["noul", "state text", "--instructions", "Urgent?"])
            .unwrap();
        assert!(cmd.true_means.is_none());
        assert!(cmd.false_means.is_none());
    }
}
