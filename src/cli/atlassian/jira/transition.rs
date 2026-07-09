//! CLI commands for listing and executing JIRA issue transitions.

use std::collections::BTreeMap;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::adf_validated::markdown_to_validated_adf;
use crate::atlassian::client::AtlassianClient;
use crate::atlassian::custom_fields::parse_set_field;
use crate::atlassian::jira_types::{EditMeta, JiraTransition};
use crate::atlassian::transition_fields::resolve_transition_fields;
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
    /// Lists workflow transitions available from the issue's current status (mirrors the `jira_transition_list` MCP tool).
    List(ListCommand),
    /// Executes a workflow transition on a JIRA issue (mirrors the `jira_transition` MCP tool).
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

    /// Set a transition-screen field inline: `--set-field "NAME=VALUE"`. Can
    /// be used multiple times. Values are parsed as YAML scalars (numbers,
    /// bools) when possible, falling back to strings. Array fields accept
    /// comma-separated values (`Labels=a,b,c`) or a YAML list (`Labels=[a, b]`).
    /// Names resolve against the transition's screen fields.
    #[arg(long = "set-field", value_name = "NAME=VALUE")]
    pub set_fields: Vec<String>,

    /// Set the resolution on the transition, e.g. `--resolution Fixed`. Sent as
    /// `{"name": "..."}`; the transition screen must accept a resolution.
    #[arg(long, value_name = "NAME")]
    pub resolution: Option<String>,

    /// Add a comment (JFM markdown) with the transition. Delivered in the
    /// transition itself when the screen accepts a comment (atomic, satisfies a
    /// mandatory-comment screen); otherwise posted as a separate comment.
    #[arg(long, value_name = "JFM")]
    pub comment: Option<String>,
}

impl ExecuteCommand {
    /// Executes the transition.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        self.execute_with_client(&client).await
    }

    /// Runs the transition against an explicit client. Split out so tests can
    /// inject a wiremock-backed client without env-configured credentials.
    async fn execute_with_client(self, client: &AtlassianClient) -> Result<()> {
        run_execute_transition(
            client,
            &self.key,
            &self.transition,
            &self.set_fields,
            self.resolution.as_deref(),
            self.comment.as_deref(),
        )
        .await
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

/// Resolves and executes a transition on an issue, optionally setting
/// transition-screen fields, a resolution, and a comment.
async fn run_execute_transition(
    client: &AtlassianClient,
    key: &str,
    transition: &str,
    set_fields: &[String],
    resolution: Option<&str>,
    comment: Option<&str>,
) -> Result<()> {
    let (transitions, metas) = client.get_transitions_with_fields(key).await?;
    let matched = resolve_transition(transition, &transitions)?;
    let matched_id = matched.id.clone();
    let matched_name = matched.name.clone();

    // Resolve `--set-field` / `--resolution` against this transition's screen.
    let scalars = set_fields
        .iter()
        .map(|s| parse_set_field(s))
        .collect::<Result<BTreeMap<_, _>>>()?;
    let default_meta = EditMeta::default();
    let editmeta = metas.get(&matched_id).unwrap_or(&default_meta);
    let resolved = resolve_transition_fields(&scalars, resolution, editmeta)?;

    // Convert the comment (if any) to ADF once; route it into the transition
    // body when the screen accepts a comment, else post it separately after.
    let comment_adf = comment
        .filter(|s| !s.is_empty())
        .map(markdown_to_validated_adf)
        .transpose()?;
    let in_body_comment = comment_adf.as_ref().filter(|_| resolved.comment_on_screen);

    client
        .do_transition_with_fields(key, &matched_id, &resolved.fields, in_body_comment)
        .await?;

    if !resolved.comment_on_screen {
        if let Some(adf) = comment_adf.as_ref() {
            client.add_comment(key, adf).await?;
        }
    }

    println!("Transitioned {key} to \"{matched_name}\".");
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
                to_status: Some(crate::atlassian::jira_types::JiraTransitionToStatus {
                    id: "3".to_string(),
                    name: "In Progress".to_string(),
                    category: Some("indeterminate".to_string()),
                }),
                has_screen: Some(false),
            },
            JiraTransition {
                id: "31".to_string(),
                name: "Done".to_string(),
                to_status: Some(crate::atlassian::jira_types::JiraTransitionToStatus {
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
        assert!(
            run_execute_transition(&client, "PROJ-1", "Done", &[], None, None)
                .await
                .is_ok()
        );
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
        let err = run_execute_transition(&client, "PROJ-1", "Nonexistent", &[], None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("No transition matching"));
    }

    /// Mounts an expanded-transitions GET whose "Resolve" transition (id 21)
    /// carries the given screen fields.
    async fn mount_expanded_transitions(server: &wiremock::MockServer, fields: serde_json::Value) {
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .and(wiremock::matchers::query_param(
                "expand",
                "transitions.fields",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "transitions": [
                        {"id": "11", "name": "In Progress"},
                        {"id": "21", "name": "Resolve", "hasScreen": true, "fields": fields}
                    ]
                })),
            )
            .expect(1)
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn run_execute_transition_sends_resolution_and_set_field() {
        let server = wiremock::MockServer::start().await;
        mount_expanded_transitions(
            &server,
            serde_json::json!({
                "resolution": {"name": "Resolution", "schema": {"type": "resolution"}},
                "customfield_100": {
                    "name": "Severity",
                    "schema": {"type": "option"},
                    "allowedValues": [{"value": "High"}]
                }
            }),
        )
        .await;
        // The transition POST must carry both resolved fields.
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "transition": {"id": "21"},
                "fields": {
                    "resolution": {"name": "Fixed"},
                    "customfield_100": {"value": "High"}
                }
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let set_fields = vec!["Severity=High".to_string()];
        assert!(run_execute_transition(
            &client,
            "PROJ-1",
            "Resolve",
            &set_fields,
            Some("Fixed"),
            None
        )
        .await
        .is_ok());
    }

    #[tokio::test]
    async fn run_execute_transition_comment_in_body_when_screen_accepts_it() {
        let server = wiremock::MockServer::start().await;
        mount_expanded_transitions(
            &server,
            serde_json::json!({
                "comment": {"name": "Comment", "schema": {"type": "comment"}}
            }),
        )
        .await;
        // Comment rides in the transition body; there must be NO separate
        // comment POST (no /comment mock is mounted, so one would 404/panic).
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "transition": {"id": "21"},
                "update": {"comment": [{"add": {"body": {"type": "doc"}}}]}
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(
            run_execute_transition(&client, "PROJ-1", "Resolve", &[], None, Some("all done"))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_execute_transition_comment_posted_separately_when_screenless() {
        let server = wiremock::MockServer::start().await;
        // No screen fields → comment must fall back to a separate POST.
        mount_expanded_transitions(&server, serde_json::json!({})).await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "transition": {"id": "21"}
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/issue/PROJ-1/comment"))
            .respond_with(
                wiremock::ResponseTemplate::new(201).set_body_json(serde_json::json!({"id": "c1"})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(
            run_execute_transition(&client, "PROJ-1", "Resolve", &[], None, Some("note"))
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn execute_with_client_forwards_all_flags() {
        // Drives the `ExecuteCommand::execute` seam (the delegation covered by
        // no other test) with an injected client, exercising the full
        // set-field + resolution + comment path.
        let server = wiremock::MockServer::start().await;
        mount_expanded_transitions(
            &server,
            serde_json::json!({
                "resolution": {"name": "Resolution", "schema": {"type": "resolution"}},
                "comment": {"name": "Comment", "schema": {"type": "comment"}}
            }),
        )
        .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path(
                "/rest/api/3/issue/PROJ-1/transitions",
            ))
            .and(wiremock::matchers::body_partial_json(serde_json::json!({
                "transition": {"id": "21"},
                "fields": {"resolution": {"name": "Fixed"}},
                "update": {"comment": [{"add": {"body": {"type": "doc"}}}]}
            })))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let cmd = ExecuteCommand {
            key: "PROJ-1".to_string(),
            transition: "Resolve".to_string(),
            set_fields: vec![],
            resolution: Some("Fixed".to_string()),
            comment: Some("all done".to_string()),
        };
        let client = mock_client(&server.uri());
        assert!(cmd.execute_with_client(&client).await.is_ok());
    }

    #[tokio::test]
    async fn run_execute_transition_resolution_collision_errors() {
        let server = wiremock::MockServer::start().await;
        mount_expanded_transitions(
            &server,
            serde_json::json!({
                "resolution": {"name": "Resolution", "schema": {"type": "option"}}
            }),
        )
        .await;
        // No POST mock: resolution must fail before any request is sent.
        let client = mock_client(&server.uri());
        let set_fields = vec!["resolution=Done".to_string()];
        let err = run_execute_transition(
            &client,
            "PROJ-1",
            "Resolve",
            &set_fields,
            Some("Fixed"),
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("collides"));
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
                set_fields: vec![],
                resolution: None,
                comment: None,
            }),
        };
        assert!(matches!(cmd.command, TransitionSubcommands::Execute(_)));
    }
}
