//! CLI commands for listing JIRA projects.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::client::AtlassianClient;
use crate::atlassian::jira_types::{CreateMeta, JiraProjectList};
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Manages JIRA projects.
#[derive(Parser)]
pub struct ProjectCommand {
    /// The project subcommand to execute.
    #[command(subcommand)]
    pub command: ProjectSubcommands,
}

/// Project subcommands.
#[derive(Subcommand)]
pub enum ProjectSubcommands {
    /// Lists all accessible JIRA projects (mirrors the `jira_project_list` MCP tool).
    List(ListCommand),
    /// Shows the create-screen fields for a project + issue type (mirrors the `jira_project_create_meta` MCP tool).
    CreateMeta(CreateMetaCommand),
}

impl ProjectCommand {
    /// Executes the project command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            ProjectSubcommands::List(cmd) => cmd.execute().await,
            ProjectSubcommands::CreateMeta(cmd) => cmd.execute().await,
        }
    }
}

/// Lists all accessible JIRA projects.
#[derive(Parser)]
pub struct ListCommand {
    /// Maximum number of results, 0 for unlimited (default: 50).
    #[arg(long, default_value_t = 50)]
    pub limit: u32,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Fetches and displays projects.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        run_list_projects(&client, self.limit, &self.output).await
    }
}

/// Fetches and displays projects.
async fn run_list_projects(
    client: &AtlassianClient,
    limit: u32,
    output: &OutputFormat,
) -> Result<()> {
    let result = client.get_projects(limit).await?;
    if output_as(&result, output)? {
        return Ok(());
    }
    print_projects(&result);
    Ok(())
}

/// Prints projects as a formatted table.
fn print_projects(result: &JiraProjectList) {
    if result.projects.is_empty() {
        println!("No projects found.");
        return;
    }

    let key_width = result
        .projects
        .iter()
        .map(|p| p.key.len())
        .max()
        .unwrap_or(3)
        .max(3);
    let type_width = result
        .projects
        .iter()
        .filter_map(|p| p.project_type.as_ref().map(String::len))
        .max()
        .unwrap_or(4)
        .max(4);
    let lead_width = result
        .projects
        .iter()
        .filter_map(|p| p.lead.as_ref().map(String::len))
        .max()
        .unwrap_or(4)
        .max(4);

    println!(
        "{:<key_width$}  {:<type_width$}  {:<lead_width$}  NAME",
        "KEY", "TYPE", "LEAD"
    );
    let name_sep = "-".repeat(4);
    println!(
        "{:<key_width$}  {:<type_width$}  {:<lead_width$}  {name_sep}",
        "-".repeat(key_width),
        "-".repeat(type_width),
        "-".repeat(lead_width),
    );

    for project in &result.projects {
        let ptype = project.project_type.as_deref().unwrap_or("-");
        let lead = project.lead.as_deref().unwrap_or("-");
        println!(
            "{:<key_width$}  {:<type_width$}  {:<lead_width$}  {}",
            project.key, ptype, lead, project.name
        );
    }

    if result.total > result.projects.len() as u32 {
        println!(
            "\nShowing {} of {} projects.",
            result.projects.len(),
            result.total
        );
    }
}

/// Shows the create-screen fields for a project + issue type.
///
/// Introspects which fields are required, their types, and allowed values —
/// the pre-flight alternative to attempting a create and recovering from the
/// HTTP 400.
#[derive(Parser)]
pub struct CreateMetaCommand {
    /// Project key (e.g., "PROJ").
    #[arg(long)]
    pub project: String,

    /// Issue type name (e.g., "Task", "Bug").
    #[arg(long)]
    pub issue_type: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl CreateMetaCommand {
    /// Fetches and displays create-screen field metadata.
    pub async fn execute(self) -> Result<()> {
        self.execute_with(create_client()).await
    }

    /// Runs against an injected client result (issue #950 DI seam). `execute`
    /// supplies the env-resolved client; tests supply one built from explicit
    /// credentials (or an `Err` to exercise the propagation path) without
    /// touching process-global env.
    async fn execute_with(self, client: Result<(AtlassianClient, String)>) -> Result<()> {
        let (client, _instance_url) = client?;
        run_create_meta(&client, &self.project, &self.issue_type, &self.output).await
    }
}

/// Fetches and displays create-screen field metadata.
async fn run_create_meta(
    client: &AtlassianClient,
    project: &str,
    issue_type: &str,
    output: &OutputFormat,
) -> Result<()> {
    let meta = client.get_project_create_meta(project, issue_type).await?;
    if output_as(&meta, output)? {
        return Ok(());
    }
    print_create_meta(&meta);
    Ok(())
}

/// Prints create-screen fields as a formatted table.
fn print_create_meta(meta: &CreateMeta) {
    if meta.fields.is_empty() {
        println!(
            "No create-screen fields found for {} / {}.",
            meta.project, meta.issue_type
        );
        return;
    }

    let id_width = meta
        .fields
        .iter()
        .map(|f| f.field_id.len())
        .max()
        .unwrap_or(8)
        .max(8);
    let type_width = meta
        .fields
        .iter()
        .map(|f| f.schema_type.len())
        .max()
        .unwrap_or(4)
        .max(4);
    let name_width = meta
        .fields
        .iter()
        .map(|f| f.name.len())
        .max()
        .unwrap_or(4)
        .max(4);

    println!(
        "{:<8}  {:<id_width$}  {:<type_width$}  {:<name_width$}  ALLOWED",
        "REQUIRED", "FIELD ID", "TYPE", "NAME"
    );
    println!(
        "{:<8}  {:<id_width$}  {:<type_width$}  {:<name_width$}  {}",
        "-".repeat(8),
        "-".repeat(id_width),
        "-".repeat(type_width),
        "-".repeat(name_width),
        "-".repeat(7),
    );

    for field in &meta.fields {
        let required = if field.required { "yes" } else { "-" };
        println!(
            "{:<8}  {:<id_width$}  {:<type_width$}  {:<name_width$}  {}",
            required,
            field.field_id,
            field.schema_type,
            field.name,
            summarize_allowed(field),
        );
    }
}

/// Summarizes a field's allowed values for the table's `ALLOWED` column:
/// the first few display values, with a trailing count when truncated.
fn summarize_allowed(field: &crate::atlassian::jira_types::CreateMetaField) -> String {
    const MAX_SHOWN: usize = 3;
    if field.allowed_values.is_empty() {
        return "-".to_string();
    }
    let shown: Vec<&str> = field
        .allowed_values
        .iter()
        .take(MAX_SHOWN)
        .map(|v| v.value.as_deref().unwrap_or("?"))
        .collect();
    let mut summary = shown.join(", ");
    if field.allowed_values.len() > MAX_SHOWN {
        summary.push_str(&format!(
            " (+{} more)",
            field.allowed_values.len() - MAX_SHOWN
        ));
    }
    summary
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::jira_types::JiraProject;

    fn sample_project(
        key: &str,
        name: &str,
        ptype: Option<&str>,
        lead: Option<&str>,
    ) -> JiraProject {
        JiraProject {
            id: "1".to_string(),
            key: key.to_string(),
            name: name.to_string(),
            project_type: ptype.map(String::from),
            lead: lead.map(String::from),
        }
    }

    // ── print_projects ─────────────────────────────────────────────

    #[test]
    fn print_projects_empty() {
        let result = JiraProjectList {
            projects: vec![],
            total: 0,
        };
        print_projects(&result);
    }

    #[test]
    fn print_projects_with_data() {
        let result = JiraProjectList {
            projects: vec![
                sample_project("PROJ", "My Project", Some("software"), Some("Alice")),
                sample_project("OPS", "Operations", Some("business"), None),
            ],
            total: 2,
        };
        print_projects(&result);
    }

    #[test]
    fn print_projects_with_pagination() {
        let result = JiraProjectList {
            projects: vec![sample_project(
                "PROJ",
                "My Project",
                Some("software"),
                Some("Alice"),
            )],
            total: 100,
        };
        print_projects(&result);
    }

    #[test]
    fn print_projects_all_fields_none() {
        let result = JiraProjectList {
            projects: vec![sample_project("X", "Minimal", None, None)],
            total: 1,
        };
        print_projects(&result);
    }

    // ── run_list_projects ──────────────────────────────────────────

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "user@test.com", "token").unwrap()
    }

    #[tokio::test]
    async fn run_list_projects_table_output() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/project/search"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "values": [{"id": "1", "key": "PROJ", "name": "Project", "projectTypeKey": "software"}],
                    "total": 1
                })),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_list_projects(&client, 50, &OutputFormat::Table)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn run_list_projects_json_output() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/project/search"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"values": [], "total": 0})),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(run_list_projects(&client, 50, &OutputFormat::Json)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn run_list_projects_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/project/search"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_list_projects(&client, 50, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── dispatch ───────────────────────────────────────────────────

    #[test]
    fn project_command_list_variant() {
        let cmd = ProjectCommand {
            command: ProjectSubcommands::List(ListCommand {
                limit: 50,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, ProjectSubcommands::List(_)));
    }

    #[test]
    fn list_command_defaults() {
        let cmd = ListCommand {
            limit: 50,
            output: OutputFormat::Table,
        };
        assert_eq!(cmd.limit, 50);
    }

    #[test]
    fn project_command_create_meta_variant() {
        let cmd = ProjectCommand {
            command: ProjectSubcommands::CreateMeta(CreateMetaCommand {
                project: "PROJ".to_string(),
                issue_type: "Task".to_string(),
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, ProjectSubcommands::CreateMeta(_)));
    }

    // ── print_create_meta / summarize_allowed ──────────────────────

    use crate::atlassian::jira_types::{CreateMeta, CreateMetaAllowedValue, CreateMetaField};

    fn allowed(value: &str) -> CreateMetaAllowedValue {
        CreateMetaAllowedValue {
            id: None,
            value: Some(value.to_string()),
            children: vec![],
        }
    }

    fn sample_field(id: &str, name: &str, required: bool, allowed: Vec<&str>) -> CreateMetaField {
        CreateMetaField {
            field_id: id.to_string(),
            name: name.to_string(),
            required,
            schema_type: "option".to_string(),
            items: None,
            custom: None,
            allowed_values: allowed.into_iter().map(self::allowed).collect(),
            default_value: None,
        }
    }

    #[test]
    fn print_create_meta_empty() {
        let meta = CreateMeta {
            project: "PROJ".to_string(),
            issue_type: "Task".to_string(),
            fields: vec![],
        };
        print_create_meta(&meta);
    }

    #[test]
    fn print_create_meta_with_data() {
        let meta = CreateMeta {
            project: "PROJ".to_string(),
            issue_type: "Task".to_string(),
            fields: vec![
                sample_field("summary", "Summary", true, vec![]),
                sample_field(
                    "customfield_1",
                    "Work Type",
                    true,
                    vec!["Planned", "Unplanned"],
                ),
            ],
        };
        print_create_meta(&meta);
    }

    #[test]
    fn summarize_allowed_truncates_after_three() {
        let field = sample_field("cf", "Sprint", false, vec!["A", "B", "C", "D", "E"]);
        let summary = summarize_allowed(&field);
        assert!(summary.contains("A, B, C"));
        assert!(summary.contains("(+2 more)"));
    }

    #[test]
    fn summarize_allowed_dash_when_empty() {
        let field = sample_field("summary", "Summary", true, vec![]);
        assert_eq!(summarize_allowed(&field), "-");
    }

    // ── run_create_meta ────────────────────────────────────────────

    #[tokio::test]
    async fn run_create_meta_table_output() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/createmeta"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "projects": [{
                        "issuetypes": [{
                            "fields": {
                                "summary": {
                                    "name": "Summary",
                                    "required": true,
                                    "schema": {"type": "string"}
                                }
                            }
                        }]
                    }]
                })),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(
            run_create_meta(&client, "PROJ", "Task", &OutputFormat::Table)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_create_meta_json_output() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/createmeta"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"projects": []})),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(
            run_create_meta(&client, "PROJ", "Task", &OutputFormat::Json)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_create_meta_jsonl_output() {
        // Exercises the `JsonlSerialize for CreateMeta` path (one line per field).
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/createmeta"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "projects": [{
                        "issuetypes": [{
                            "fields": {
                                "summary": {
                                    "name": "Summary",
                                    "required": true,
                                    "schema": {"type": "string"}
                                }
                            }
                        }]
                    }]
                })),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(
            run_create_meta(&client, "PROJ", "Task", &OutputFormat::Jsonl)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_create_meta_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/createmeta"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_create_meta(&client, "PROJ", "Task", &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    // ── execute() / execute_with() DI seam (issue #950) ────────────
    //
    // `execute_with` takes an injected client, so the happy path runs against
    // a mock without mutating process-global env. The error-path test drives
    // the production `ProjectCommand::execute` -> `CreateMetaCommand::execute`
    // -> `create_client()` wrappers with credentials cleared behind the one
    // canonical `EnvGuard`, so the env-reading dispatch + `?` propagation run.

    use crate::atlassian::auth::test_util::EnvGuard;
    use crate::atlassian::auth::AtlassianCredentials;
    use crate::cli::atlassian::helpers::create_client_from;

    /// Credentials pointed at a mock server. The mock matches method + path
    /// only; the dummy email/token are never authenticated.
    fn mock_credentials(instance_url: &str) -> AtlassianCredentials {
        AtlassianCredentials {
            instance_url: instance_url.to_string(),
            email: "test@example.com".to_string(),
            api_token: "test-token".into(),
        }
    }

    #[tokio::test]
    async fn create_meta_command_execute_with_dispatches_through_client() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rest/api/3/issue/createmeta"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"projects": []})),
            )
            .mount(&server)
            .await;

        let cmd = CreateMetaCommand {
            project: "PROJ".to_string(),
            issue_type: "Task".to_string(),
            output: OutputFormat::Yaml,
        };
        let client = create_client_from(mock_credentials(&server.uri()));
        assert!(cmd.execute_with(client).await.is_ok());
    }

    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn project_command_execute_create_meta_propagates_create_client_error() {
        let guard = EnvGuard::take();
        let _home = guard.clear_credentials();

        let cmd = ProjectCommand {
            command: ProjectSubcommands::CreateMeta(CreateMetaCommand {
                project: "PROJ".to_string(),
                issue_type: "Task".to_string(),
                output: OutputFormat::Yaml,
            }),
        };
        assert!(cmd.execute().await.is_err());
    }
}
