//! `ai jev score` — rates the state against an ordered scale.

use std::collections::BTreeMap;

use anyhow::Result;
use clap::Parser;

use crate::jev::client::JevClient;
use crate::jev::config::JevConfig;
use crate::jev::protocol::{Question, SystemOneRequest};

use super::common::{
    build_state_value, format_single, resolve_state, JevFormat, SINGLE_QUESTION_KEY,
};

/// Rates the given state against an ordered scale.
#[derive(Parser)]
pub struct ScoreCommand {
    /// The state to judge. Read from stdin if omitted.
    pub state: Option<String>,

    /// Parses `state` as JSON/YAML instead of sending it as a literal string.
    #[arg(long)]
    pub state_json: bool,

    /// What to rate and how.
    #[arg(long)]
    pub instructions: String,

    /// A scale level, low to high. Repeatable; at least two are required.
    /// Order is significant and preserved.
    #[arg(long = "level", value_name = "DESC")]
    pub level: Vec<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = JevFormat::Json)]
    pub(super) output: JevFormat,

    /// Overrides the configured Jev model for this call.
    ///
    /// Jev subcommands accept no `--model` flag (passing one is a clap error) and no
    /// other AI backend flags; an exported `OMNI_DEV_MODEL` is ignored too.
    #[arg(long, value_name = "MODEL")]
    pub jev_model: Option<String>,
}

impl ScoreCommand {
    /// Executes the score command.
    pub async fn execute(self) -> Result<()> {
        let mut config = JevConfig::from_env()?;
        if let Some(model) = self.jev_model {
            config.model = model;
        }
        let client = JevClient::from_config(&config)?;
        let output = run_score(
            &client,
            &config.model,
            self.state,
            self.state_json,
            &self.instructions,
            self.level,
            self.output,
        )
        .await?;
        print!("{output}");
        Ok(())
    }
}

/// Builds and sends the score request, returning the formatted output.
///
/// The `--level` minimum-count validation ([`Question::validate`]) happens
/// here, before stdin is read, rather than in `execute`, so it is testable
/// without an env or a network call (STYLE-0025).
#[allow(clippy::too_many_arguments)]
async fn run_score(
    client: &JevClient,
    model: &str,
    state: Option<String>,
    state_json: bool,
    instructions: &str,
    levels: Vec<String>,
    format: JevFormat,
) -> Result<String> {
    let question = Question::Score {
        instructions: instructions.to_string(),
        criteria: levels,
    };
    question.validate()?;

    let raw_state = resolve_state(state)?;
    let state_value = build_state_value(&raw_state, state_json)?;

    let request = SystemOneRequest {
        state: state_value,
        model: model.to_string(),
        questions: BTreeMap::from([(SINGLE_QUESTION_KEY.to_string(), question)]),
    };

    let response = client.system_one(&request).await?;
    format_single(&response, format)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::super::common::test_support::dead_client;
    use super::*;

    // ── run_score validation ────────────────────────────────────────

    #[tokio::test]
    async fn run_score_rejects_zero_levels() {
        let err = run_score(
            &dead_client(),
            "jev-latest",
            Some("state".to_string()),
            false,
            "How urgent",
            Vec::new(),
            JevFormat::Json,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("at least 2 levels"));
    }

    #[tokio::test]
    async fn run_score_rejects_a_single_level() {
        let err = run_score(
            &dead_client(),
            "jev-latest",
            Some("state".to_string()),
            false,
            "How urgent",
            vec!["Low".to_string()],
            JevFormat::Json,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("got 1"));
    }

    // ── wiremock ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_score_preserves_level_order_in_request() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "state": "payouts failing 3 days",
                "model": "jev-latest",
                "questions": {
                    "answer": {
                        "type": "score",
                        "instructions": "How urgent",
                        "criteria": ["Low", "Medium", "High"]
                    }
                }
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {
                        "answer": {
                            "type": "score",
                            "score": 1.0,
                            "confidence": 0.9,
                            "legend": {"0": "Low", "1": "Medium", "2": "High"},
                            "probabilities": {"0": 0.1, "1": 0.8, "2": 0.1}
                        }
                    },
                    "usage": {"input_tokens": 10, "output_tokens": 5}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = JevClient::new(&server.uri(), "my-key").unwrap();
        let out = run_score(
            &client,
            "jev-latest",
            Some("payouts failing 3 days".to_string()),
            false,
            "How urgent",
            vec!["Low".to_string(), "Medium".to_string(), "High".to_string()],
            JevFormat::Json,
        )
        .await
        .unwrap();
        assert!(out.contains("\"score\": 1.0"));
    }

    #[tokio::test]
    async fn run_score_yaml_output() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {
                        "answer": {
                            "type": "score",
                            "score": 1.0,
                            "confidence": 0.9,
                            "legend": {"0": "Low"},
                            "probabilities": {"0": 1.0}
                        }
                    },
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })),
            )
            .mount(&server)
            .await;

        let client = JevClient::new(&server.uri(), "my-key").unwrap();
        let out = run_score(
            &client,
            "jev-latest",
            Some("state".to_string()),
            false,
            "How urgent",
            vec!["Low".to_string(), "High".to_string()],
            JevFormat::Yaml,
        )
        .await
        .unwrap();
        assert!(out.contains("score: 1.0"));
    }

    // ── clap parsing ────────────────────────────────────────────────

    #[test]
    fn clap_parses_repeated_levels_in_order() {
        let cmd = ScoreCommand::try_parse_from([
            "score",
            "state text",
            "--instructions",
            "How urgent",
            "--level",
            "Low",
            "--level",
            "Medium",
            "--level",
            "High",
            "-o",
            "yaml",
        ])
        .unwrap();
        assert_eq!(
            cmd.level,
            vec!["Low".to_string(), "Medium".to_string(), "High".to_string()]
        );
        assert_eq!(cmd.output, JevFormat::Yaml);
    }
}
