//! CLI commands for listing and executing JIRA issue transitions.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::client::{AtlassianClient, JiraTransition};
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Lists or executes workflow transitions on a JIRA issue.
#[derive(Parser)]
pub struct TransitionCommand {
    /// The transition subcommand to execute.
    #[command(subcommand)]
    pub command: TransitionSubcommands,
}

/// Transition subcommands.
#[derive(Subcommand)]
pub enum TransitionSubcommands {
    /// Lists workflow transitions available from the issue's current status.
    List(ListCommand),
    /// Executes a workflow transition on a JIRA issue.
    Execute(ExecuteCommand),
}

impl TransitionCommand {
    /// Executes the transition command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            TransitionSubcommands::List(cmd) => cmd.execute().await,
            TransitionSubcommands::Execute(cmd) => cmd.execute().await,
        }
    }
}

/// Lists available workflow transitions for a JIRA issue.
#[derive(Parser)]
pub struct ListCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Fetches and displays available transitions.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_list_transitions(&client, &self.key, &self.output).await
    }
}

/// Executes a transition on a JIRA issue.
#[derive(Parser)]
pub struct ExecuteCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Transition name or ID to execute.
    pub transition: String,
}

impl ExecuteCommand {
    /// Executes the transition.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_execute_transition(&client, &self.key, &self.transition).await
    }
}

/// Lists transitions for an issue and prints them.
async fn run_list_transitions(
    client: &AtlassianClient,
    key: &str,
    output: &OutputFormat,
) -> Result<()> {
    let transitions = client.get_transitions(key).await?;
    if output_as(&transitions, output)? {
        return Ok(());
    }
    print_transitions(&transitions);
    Ok(())
}

/// Resolves and executes a transition on an issue.
async fn run_execute_transition(
    client: &AtlassianClient,
    key: &str,
    transition: &str,
) -> Result<()> {
    let transitions = client.get_transitions(key).await?;
    let matched = resolve_transition(transition, &transitions)?;
    client.do_transition(key, &matched.id).await?;
    println!("Transitioned {key} to \"{}\".", matched.name);
    Ok(())
}

/// Resolves a transition by exact ID or case-insensitive name match.
fn resolve_transition<'a>(
    target: &str,
    transitions: &'a [JiraTransition],
) -> Result<&'a JiraTransition> {
    // Try exact ID match first
    if let Some(t) = transitions.iter().find(|t| t.id == target) {
        return Ok(t);
    }

    // Try case-insensitive name match
    let target_lower = target.to_lowercase();
    let matches: Vec<_> = transitions
        .iter()
        .filter(|t| t.name.to_lowercase() == target_lower)
        .collect();

    match matches.len() {
        0 => {
            let available: Vec<_> = transitions
                .iter()
                .map(|t| format!("\"{}\" (id: {})", t.name, t.id))
                .collect();
            anyhow::bail!(
                "No transition matching \"{target}\" found.\nAvailable transitions: {}",
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                }
            )
        }
        1 => Ok(matches[0]),
        _ => {
            let dupes: Vec<_> = matches
                .iter()
                .map(|t| format!("\"{}\" (id: {})", t.name, t.id))
                .collect();
            anyhow::bail!(
                "Ambiguous transition \"{target}\". Matches: {}. Use the transition ID instead.",
                dupes.join(", ")
            )
        }
    }
}

/// Prints transitions as a formatted table.
fn print_transitions(transitions: &[JiraTransition]) {
    if transitions.is_empty() {
        println!("No transitions available.");
        return;
    }

    let id_width = transitions
        .iter()
        .map(|t| t.id.len())
        .max()
        .unwrap_or(2)
        .max(2);

    let name_width = transitions
        .iter()
        .map(|t| t.name.len())
        .max()
        .unwrap_or(4)
        .max(4);

    let to_status_width = transitions
        .iter()
        .map(|t| t.to_status.as_ref().map_or(0, |s| s.name.len()))
        .max()
        .unwrap_or(9)
        .max(9);

    println!(
        "{:<id_width$}  {:<name_width$}  {:<to_status_width$}  CATEGORY",
        "ID", "NAME", "TO STATUS"
    );
    println!(
        "{}  {}  {}  {}",
        "-".repeat(id_width),
        "-".repeat(name_width),
        "-".repeat(to_status_width),
        "-".repeat(8),
    );

    for t in transitions {
        let to_name = t.to_status.as_ref().map_or("", |s| s.name.as_str());
        let category = t
            .to_status
            .as_ref()
            .and_then(|s| s.category.as_deref())
            .unwrap_or("");
        println!(
            "{:<id_width$}  {:<name_width$}  {:<to_status_width$}  {}",
            t.id, t.name, to_name, category,
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample_transitions() -> Vec<JiraTransition> {
        vec![
            JiraTransition {
                id: "11".to_string(),
                name: "In Progress".to_string(),
                to_status: None,
                has_screen: None,
            },
            JiraTransition {
                id: "21".to_string(),
                name: "Done".to_string(),
                to_status: None,
                has_screen: None,
            },
            JiraTransition {
                id: "31".to_string(),
                name: "Won't Do".to_string(),
                to_status: None,
                has_screen: None,
            },
        ]
    }

    // ── resolve_transition ─────────────────────────────────────────

    #[test]
    fn resolve_by_exact_id() {
        let transitions = sample_transitions();
        let result = resolve_transition("21", &transitions).unwrap();
        assert_eq!(result.name, "Done");
    }

    #[test]
    fn resolve_by_name_case_insensitive() {
        let transitions = sample_transitions();
        let result = resolve_transition("in progress", &transitions).unwrap();
        assert_eq!(result.id, "11");
    }

    #[test]
    fn resolve_by_name_exact_case() {
        let transitions = sample_transitions();
        let result = resolve_transition("Done", &transitions).unwrap();
        assert_eq!(result.id, "21");
    }

    #[test]
    fn resolve_not_found() {
        let transitions = sample_transitions();
        let err = resolve_transition("Cancelled", &transitions).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("No transition matching"));
        assert!(msg.contains("In Progress"));
        assert!(msg.contains("Done"));
    }

    #[test]
    fn resolve_not_found_empty_list() {
        let err = resolve_transition("Done", &[]).unwrap_err();
        assert!(err.to_string().contains("none"));
    }

    #[test]
    fn resolve_ambiguous() {
        let transitions = vec![
            JiraTransition {
                id: "11".to_string(),
                name: "Done".to_string(),
                to_status: None,
                has_screen: None,
            },
            JiraTransition {
                id: "21".to_string(),
                name: "Done".to_string(),
                to_status: None,
                has_screen: None,
            },
        ];
        let err = resolve_transition("Done", &transitions).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Ambiguous"));
        assert!(msg.contains("id: 11"));
        assert!(msg.contains("id: 21"));
    }

    #[test]
    fn resolve_id_takes_priority_over_name() {
        // If a transition ID matches exactly, use it even if name also matches
        let transitions = vec![
            JiraTransition {
                id: "Done".to_string(),
                name: "Something Else".to_string(),
                to_status: None,
                has_screen: None,
            },
            JiraTransition {
                id: "99".to_string(),
                name: "Done".to_string(),
                to_status: None,
                has_screen: None,
            },
        ];
        let result = resolve_transition("Done", &transitions).unwrap();
        assert_eq!(result.name, "Something Else"); // matched by ID
    }

    // ── print_transitions ──────────────────────────────────────────

    #[test]
    fn print_transitions_with_items() {
        let transitions = sample_transitions();
        // Should not panic
        print_transitions(&transitions);
    }

    #[test]
    fn print_transitions_empty() {
        print_transitions(&[]);
    }

    #[test]
    fn print_transitions_with_rich_to_status() {
        let transitions = vec![
            JiraTransition {
                id: "21".to_string(),
                name: "In Progress".to_string(),
                to_status: Some(crate::atlassian::client::JiraTransitionToStatus {
                    id: "3".to_string(),
                    name: "In Progress".to_string(),
                    category: Some("indeterminate".to_string()),
                }),
                has_screen: Some(false),
            },
            JiraTransition {
                id: "31".to_string(),
                name: "Done".to_string(),
                to_status: Some(crate::atlassian::client::JiraTransitionToStatus {
                    id: "10000".to_string(),
                    name: "Done".to_string(),
                    category: Some("done".to_string()),
                }),
                has_screen: Some(true),
            },
        ];
        // Should not panic and exercise the Some(to_status) branches
        print_transitions(&transitions);
    }

    // ── run_list_transitions / run_execute_transition (wiremock) ───

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "user@test.com", "token").unwrap()
    }

    #[tokio::test]
    async fn run_list_transitions_table() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "transitions": [
                        {"id": "11", "name": "In Progress"},
                        {"id": "21", "name": "Done"}
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(
            run_list_transitions(&client, "PROJ-1", &OutputFormat::Table)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_list_transitions_yaml() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "transitions": [{"id": "11", "name": "In Progress"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_list_transitions(&client, "PROJ-1", &OutputFormat::Yaml)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn run_list_transitions_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/NOPE-1/transitions",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_list_transitions(&client, "NOPE-1", &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn run_execute_transition_by_name() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "transitions": [
                        {"id": "11", "name": "In Progress"},
                        {"id": "21", "name": "Done"}
                    ]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_execute_transition(&client, "PROJ-1", "Done")
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn run_execute_transition_resolve_not_found() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "transitions": [{"id": "11", "name": "In Progress"}]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_execute_transition(&client, "PROJ-1", "Nonexistent")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("No transition matching"));
    }

    // ── TransitionCommand variants ─────────────────────────────────

    #[test]
    fn transition_command_list_variant() {
        let cmd = TransitionCommand {
            command: TransitionSubcommands::List(ListCommand {
                key: "PROJ-1".to_string(),
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, TransitionSubcommands::List(_)));
    }

    #[test]
    fn transition_command_execute_variant() {
        let cmd = TransitionCommand {
            command: TransitionSubcommands::Execute(ExecuteCommand {
                key: "PROJ-1".to_string(),
                transition: "Done".to_string(),
            }),
        };
        assert!(matches!(cmd.command, TransitionSubcommands::Execute(_)));
    }
}
