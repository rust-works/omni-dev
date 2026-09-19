//! `ai jev ask` — answers several questions about one state in a single pass.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::Parser;

use crate::jev::client::JevClient;
use crate::jev::config::JevConfig;
use crate::jev::protocol::{Question, SystemOneRequest};

use super::common::{build_state_value, format_multi, resolve_state, JevFormat};

/// Answers several questions about one state in a single pass, from a file.
#[derive(Parser)]
pub struct AskCommand {
    /// The state to judge. Read from stdin if omitted.
    pub state: Option<String>,

    /// Parses `state` as JSON/YAML instead of sending it as a literal string.
    #[arg(long)]
    pub state_json: bool,

    /// YAML or JSON file mapping question name to a Jev question spec.
    /// Questions are file-only — state stays positional/stdin, so a pipe like
    /// `git diff | omni-dev ai jev ask --questions q.yaml` isn't ambiguous
    /// about which input stdin feeds.
    #[arg(long, value_name = "FILE")]
    pub questions: PathBuf,

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

impl AskCommand {
    /// Executes the ask command.
    pub async fn execute(self) -> Result<()> {
        let mut config = JevConfig::from_env()?;
        if let Some(model) = self.jev_model {
            config.model = model;
        }
        let client = JevClient::from_config(&config)?;
        let output = run_ask(
            &client,
            &config.model,
            self.state,
            self.state_json,
            &self.questions,
            self.output,
        )
        .await?;
        print!("{output}");
        Ok(())
    }
}

/// Reads a `--questions` file through a **single** `serde_yaml` parse path.
///
/// `serde_yaml` accepts JSON (YAML is a superset), so this covers both JSON
/// and YAML question files without branching on file extension. Each spec
/// is then checked with [`Question::validate`] — the same minimums the
/// single-question subcommands enforce — so a degenerate spec fails locally,
/// naming the offending question, before any paid request is sent.
fn read_questions(path: &Path) -> Result<BTreeMap<String, Question>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read questions file {}", path.display()))?;
    let questions: BTreeMap<String, Question> = serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse questions file {}", path.display()))?;
    if questions.is_empty() {
        bail!("questions file {} defines no questions", path.display());
    }
    for (name, question) in &questions {
        question
            .validate()
            .with_context(|| format!("question {name:?} in {}", path.display()))?;
    }
    Ok(questions)
}

/// Builds and sends the ask request, returning the formatted output.
async fn run_ask(
    client: &JevClient,
    model: &str,
    state: Option<String>,
    state_json: bool,
    questions_path: &Path,
    format: JevFormat,
) -> Result<String> {
    let questions = read_questions(questions_path)?;
    let raw_state = resolve_state(state)?;
    let state_value = build_state_value(&raw_state, state_json)?;

    let request = SystemOneRequest {
        state: state_value,
        model: model.to_string(),
        questions,
    };

    let response = client.system_one(&request).await?;
    format_multi(&response, format)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Write as _;
    use tempfile::NamedTempFile;

    fn write_temp(contents: &str) -> NamedTempFile {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file
    }

    // ── read_questions ──────────────────────────────────────────────

    #[test]
    fn read_questions_parses_yaml_file() {
        let file = write_temp(
            "department:\n  type: choice\n  instructions: Route this\n  criteria:\n    billing: Payments\n    technical: Bugs\n",
        );
        let questions = read_questions(file.path()).unwrap();
        assert_eq!(questions.len(), 1);
        assert!(matches!(questions["department"], Question::Choice { .. }));
    }

    #[test]
    fn read_questions_parses_json_file() {
        let file = write_temp(r#"{"refund": {"type": "noul", "instructions": "Urgent?"}}"#);
        let questions = read_questions(file.path()).unwrap();
        assert_eq!(questions.len(), 1);
        assert!(matches!(questions["refund"], Question::Noul { .. }));
    }

    #[test]
    fn read_questions_rejects_empty_map() {
        let file = write_temp("{}\n");
        let err = read_questions(file.path()).unwrap_err();
        assert!(err.to_string().contains("defines no questions"));
    }

    #[test]
    fn read_questions_rejects_a_choice_with_one_option() {
        let file = write_temp(
            "department:\n  type: choice\n  instructions: Route this\n  criteria:\n    billing: Payments\n",
        );
        let err = read_questions(file.path()).unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("question \"department\""), "{chain}");
        assert!(chain.contains("at least 2 options, got 1"), "{chain}");
    }

    #[test]
    fn read_questions_rejects_a_score_with_one_level() {
        let file =
            write_temp("urgency:\n  type: score\n  instructions: How urgent\n  criteria: [Low]\n");
        let err = read_questions(file.path()).unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("question \"urgency\""), "{chain}");
        assert!(chain.contains("at least 2 levels, got 1"), "{chain}");
    }

    #[test]
    fn read_questions_missing_file_errors() {
        let err = read_questions(Path::new("/nonexistent/questions.yaml")).unwrap_err();
        assert!(err.to_string().contains("Failed to read questions file"));
    }

    #[test]
    fn read_questions_invalid_yaml_errors() {
        let file = write_temp("not: [a, valid, question, map");
        let err = read_questions(file.path()).unwrap_err();
        assert!(err.to_string().contains("Failed to parse questions file"));
    }

    // ── wiremock ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_ask_sends_multi_question_request_and_parses_answers() {
        let file = write_temp(
            "department:\n  type: choice\n  instructions: Route this\n  criteria:\n    billing: Payments\n    technical: Bugs\nrefund:\n  type: noul\n  instructions: Urgent?\n",
        );
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "state": "payouts failing 3 days",
                "model": "jev-latest",
                "questions": {
                    "department": {
                        "type": "choice",
                        "instructions": "Route this",
                        "criteria": {"billing": "Payments", "technical": "Bugs"}
                    },
                    "refund": {"type": "noul", "instructions": "Urgent?"}
                }
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {
                        "department": {
                            "type": "choice",
                            "choice": "billing",
                            "confidence": 0.97,
                            "probabilities": {"billing": 0.97, "technical": 0.03}
                        },
                        "refund": {"type": "noul", "noul": 0.98}
                    },
                    "usage": {"input_tokens": 20, "output_tokens": 10}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = JevClient::new(&server.uri(), "my-key").unwrap();
        let out = run_ask(
            &client,
            "jev-latest",
            Some("payouts failing 3 days".to_string()),
            false,
            file.path(),
            JevFormat::Json,
        )
        .await
        .unwrap();
        assert!(out.contains("\"department\""));
        assert!(out.contains("\"refund\""));
        assert!(out.contains("\"noul\": 0.98"));
    }

    #[tokio::test]
    async fn run_ask_yaml_output() {
        let file = write_temp("refund:\n  type: noul\n  instructions: Urgent?\n");
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {"refund": {"type": "noul", "noul": 0.98}},
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })),
            )
            .mount(&server)
            .await;

        let client = JevClient::new(&server.uri(), "my-key").unwrap();
        let out = run_ask(
            &client,
            "jev-latest",
            Some("state".to_string()),
            false,
            file.path(),
            JevFormat::Yaml,
        )
        .await
        .unwrap();
        assert!(out.contains("answers:"));
        assert!(out.contains("noul: 0.98"));
    }

    #[tokio::test]
    async fn run_ask_state_json_flag_parses_state() {
        let file = write_temp("refund:\n  type: noul\n  instructions: Urgent?\n");
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/systemone"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "state": {"a": 1},
                "model": "jev-latest",
                "questions": {"refund": {"type": "noul", "instructions": "Urgent?"}}
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "model": "jev-1.13.0",
                    "answers": {"refund": {"type": "noul", "noul": 0.5}},
                    "usage": {"input_tokens": 1, "output_tokens": 1}
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = JevClient::new(&server.uri(), "my-key").unwrap();
        run_ask(
            &client,
            "jev-latest",
            Some(r#"{"a": 1}"#.to_string()),
            true,
            file.path(),
            JevFormat::Json,
        )
        .await
        .unwrap();
    }

    // ── clap parsing ────────────────────────────────────────────────

    #[test]
    fn clap_parses_questions_path_and_short_output_flag() {
        let cmd = AskCommand::try_parse_from([
            "ask",
            "state text",
            "--questions",
            "q.yaml",
            "-o",
            "yaml",
        ])
        .unwrap();
        assert_eq!(cmd.questions, PathBuf::from("q.yaml"));
        assert_eq!(cmd.output, JevFormat::Yaml);
    }

    #[test]
    fn clap_requires_questions_flag() {
        let err = AskCommand::try_parse_from(["ask", "state text"])
            .err()
            .expect("expected a parse error");
        assert!(err.to_string().contains("--questions"));
    }
}
