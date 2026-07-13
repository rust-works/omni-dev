//! CLI commands for JIRA project versions (release versions).

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::atlassian::client::AtlassianClient;
use crate::atlassian::jira_types::JiraProjectVersionList;
use crate::cli::atlassian::confirm::{guard_destructive_with_io, GuardOptions, GuardOutcome};
use crate::cli::atlassian::format::{output_as, OutputFormat};
use crate::cli::atlassian::helpers::create_client;

/// Manages JIRA project versions (release versions).
#[derive(Parser)]
pub struct VersionCommand {
    /// The version subcommand to execute.
    #[command(subcommand)]
    pub command: VersionSubcommands,
}

/// Version subcommands.
#[derive(Subcommand)]
pub enum VersionSubcommands {
    /// Lists versions for a project (mirrors the `jira_version_list` MCP tool).
    List(ListCommand),
    /// Creates a new project version (mirrors the `jira_version_create` MCP tool).
    Create(CreateCommand),
    /// Marks a version as released (mirrors the `jira_version_release` MCP tool).
    Release(ReleaseCommand),
    /// Archives a version (mirrors the `jira_version_archive` MCP tool).
    Archive(ArchiveCommand),
    /// Renames a version (mirrors the `jira_version_rename` MCP tool).
    Rename(RenameCommand),
    /// Deletes a version (mirrors the `jira_version_delete` MCP tool).
    Delete(DeleteCommand),
}

impl VersionCommand {
    /// Executes the version command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            VersionSubcommands::List(cmd) => cmd.execute().await,
            VersionSubcommands::Create(cmd) => cmd.execute().await,
            VersionSubcommands::Release(cmd) => cmd.execute().await,
            VersionSubcommands::Archive(cmd) => cmd.execute().await,
            VersionSubcommands::Rename(cmd) => cmd.execute().await,
            VersionSubcommands::Delete(cmd) => cmd.execute().await,
        }
    }

    /// Dispatches against an injected client result (issue #950 DI seam). Lets
    /// tests drive the full `VersionCommand` -> subcommand dispatch path against
    /// a mock without mutating process-global env.
    #[cfg(test)]
    async fn execute_with(self, client: Result<(AtlassianClient, String)>) -> Result<()> {
        match self.command {
            VersionSubcommands::List(cmd) => cmd.execute_with(client).await,
            VersionSubcommands::Create(cmd) => cmd.execute_with(client).await,
            VersionSubcommands::Release(cmd) => cmd.execute_with(client).await,
            VersionSubcommands::Archive(cmd) => cmd.execute_with(client).await,
            VersionSubcommands::Rename(cmd) => cmd.execute_with(client).await,
            VersionSubcommands::Delete(cmd) => cmd.execute_with(client).await,
        }
    }
}

/// Lists versions for a JIRA project.
#[derive(Parser)]
pub struct ListCommand {
    /// Project key (e.g., "PROJ").
    #[arg(long)]
    pub project: String,

    /// Show only released versions.
    #[arg(long, conflicts_with = "unreleased")]
    pub released: bool,

    /// Show only unreleased versions.
    #[arg(long, conflicts_with = "released")]
    pub unreleased: bool,

    /// Show only archived versions.
    #[arg(long, conflicts_with = "unarchived")]
    pub archived: bool,

    /// Show only non-archived versions.
    #[arg(long, conflicts_with = "archived")]
    pub unarchived: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Fetches and displays versions.
    pub async fn execute(self) -> Result<()> {
        self.execute_with(create_client()).await
    }

    /// Runs against an injected client result (issue #950 DI seam). `execute`
    /// supplies the env-resolved client; tests supply one built from explicit
    /// credentials (or an `Err` to exercise the propagation path) without
    /// touching process-global env.
    async fn execute_with(self, client: Result<(AtlassianClient, String)>) -> Result<()> {
        let (client, _instance_url) = client?;
        run_list_versions(
            &client,
            &self.project,
            tri_state(self.released, self.unreleased),
            tri_state(self.archived, self.unarchived),
            &self.output,
        )
        .await
    }
}

/// Creates a new version on a JIRA project.
#[derive(Parser)]
pub struct CreateCommand {
    /// Project key (e.g., "PROJ").
    #[arg(long)]
    pub project: String,

    /// Version name (e.g., "1.0.0").
    #[arg(long)]
    pub name: String,

    /// Version description.
    #[arg(long)]
    pub description: Option<String>,

    /// Release date (ISO 8601, "YYYY-MM-DD").
    #[arg(long)]
    pub release_date: Option<String>,

    /// Start date (ISO 8601, "YYYY-MM-DD").
    #[arg(long)]
    pub start_date: Option<String>,

    /// Mark the version as released.
    #[arg(long)]
    pub released: bool,

    /// Mark the version as archived.
    #[arg(long)]
    pub archived: bool,
}

impl CreateCommand {
    /// Creates the version.
    pub async fn execute(self) -> Result<()> {
        self.execute_with(create_client()).await
    }

    /// Runs against an injected client result (issue #950 DI seam). See
    /// [`ListCommand::execute_with`].
    async fn execute_with(self, client: Result<(AtlassianClient, String)>) -> Result<()> {
        let (client, _instance_url) = client?;
        run_create_version(
            &client,
            &self.project,
            &self.name,
            self.description.as_deref(),
            self.release_date.as_deref(),
            self.start_date.as_deref(),
            self.released,
            self.archived,
        )
        .await
    }
}

/// Marks a project version as released.
#[derive(Parser)]
pub struct ReleaseCommand {
    /// Version ID (from `version list`).
    pub version_id: String,

    /// Release date (ISO 8601, "YYYY-MM-DD"). Defaults to leaving it unset.
    #[arg(long)]
    pub release_date: Option<String>,
}

impl ReleaseCommand {
    /// Releases the version.
    pub async fn execute(self) -> Result<()> {
        self.execute_with(create_client()).await
    }

    /// Runs against an injected client result (issue #950 DI seam).
    async fn execute_with(self, client: Result<(AtlassianClient, String)>) -> Result<()> {
        let (client, _instance_url) = client?;
        client
            .update_project_version(
                &self.version_id,
                None,
                None,
                Some(true),
                self.release_date.as_deref(),
                None,
                None,
            )
            .await?;
        println!("Released version {}.", self.version_id);
        Ok(())
    }
}

/// Archives a project version.
#[derive(Parser)]
pub struct ArchiveCommand {
    /// Version ID (from `version list`).
    pub version_id: String,
}

impl ArchiveCommand {
    /// Archives the version.
    pub async fn execute(self) -> Result<()> {
        self.execute_with(create_client()).await
    }

    /// Runs against an injected client result (issue #950 DI seam).
    async fn execute_with(self, client: Result<(AtlassianClient, String)>) -> Result<()> {
        let (client, _instance_url) = client?;
        client
            .update_project_version(&self.version_id, None, None, None, None, Some(true), None)
            .await?;
        println!("Archived version {}.", self.version_id);
        Ok(())
    }
}

/// Renames (and optionally re-describes) a project version.
#[derive(Parser)]
pub struct RenameCommand {
    /// Version ID (from `version list`).
    pub version_id: String,

    /// New version name.
    pub name: String,

    /// New description (optional).
    #[arg(long)]
    pub description: Option<String>,
}

impl RenameCommand {
    /// Renames the version.
    pub async fn execute(self) -> Result<()> {
        self.execute_with(create_client()).await
    }

    /// Runs against an injected client result (issue #950 DI seam).
    async fn execute_with(self, client: Result<(AtlassianClient, String)>) -> Result<()> {
        let (client, _instance_url) = client?;
        client
            .update_project_version(
                &self.version_id,
                Some(&self.name),
                self.description.as_deref(),
                None,
                None,
                None,
                None,
            )
            .await?;
        println!("Renamed version {} to {}.", self.version_id, self.name);
        Ok(())
    }
}

/// Deletes a project version.
#[derive(Parser)]
pub struct DeleteCommand {
    /// Version ID (from `version list`).
    pub version_id: String,

    /// Reassign the `fixVersion` of affected issues to this version id before
    /// deleting (otherwise the references are simply dropped).
    #[arg(long)]
    pub move_fix_issues_to: Option<String>,

    /// Reassign the `affectedVersion` of affected issues to this version id
    /// before deleting.
    #[arg(long)]
    pub move_affected_issues_to: Option<String>,

    /// Skips the confirmation prompt.
    #[arg(long)]
    pub force: bool,

    /// Prints what would be deleted without making any API calls.
    #[arg(long)]
    pub dry_run: bool,
}

impl DeleteCommand {
    /// Deletes the version.
    pub async fn execute(self) -> Result<()> {
        self.execute_with(create_client()).await
    }

    /// Runs against an injected client result (issue #950 DI seam), with the
    /// default stdin/stdout for the confirmation prompt.
    async fn execute_with(self, client: Result<(AtlassianClient, String)>) -> Result<()> {
        let (client, _instance_url) = client?;
        let mut reader = std::io::BufReader::new(std::io::stdin());
        let mut writer = std::io::stdout();
        self.execute_with_io(&client, &mut reader, &mut writer)
            .await
    }

    /// Inner form taking explicit client and IO handles, for unit tests.
    async fn execute_with_io(
        self,
        client: &AtlassianClient,
        reader: &mut (dyn std::io::BufRead + Send),
        writer: &mut (dyn std::io::Write + Send),
    ) -> Result<()> {
        if !self.force || self.dry_run {
            let prompt = format!("Delete version {}? [y/N] ", self.version_id);
            let dry_run_message = format!("Would delete version {}.", self.version_id);

            let outcome = guard_destructive_with_io(
                &GuardOptions {
                    prompt: &prompt,
                    dry_run_message: &dry_run_message,
                    force: self.force,
                    dry_run: self.dry_run,
                },
                reader,
                writer,
            )?;

            match outcome {
                GuardOutcome::Cancelled | GuardOutcome::DryRun => return Ok(()),
                GuardOutcome::Proceed => {}
            }
        }

        client
            .delete_project_version(
                &self.version_id,
                self.move_fix_issues_to.as_deref(),
                self.move_affected_issues_to.as_deref(),
            )
            .await?;
        writeln!(writer, "Deleted version {}.", self.version_id)?;

        Ok(())
    }
}

/// Folds a pair of mutually-exclusive boolean flags into a tri-state filter:
/// `Some(true)` for the positive flag, `Some(false)` for the negative,
/// `None` when neither was passed.
fn tri_state(yes: bool, no: bool) -> Option<bool> {
    match (yes, no) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    }
}

async fn run_list_versions(
    client: &AtlassianClient,
    project: &str,
    released: Option<bool>,
    archived: Option<bool>,
    output: &OutputFormat,
) -> Result<()> {
    let result = client
        .get_project_versions(project, released, archived)
        .await?;
    if output_as(&result, output)? {
        return Ok(());
    }
    print_versions(&result);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_create_version(
    client: &AtlassianClient,
    project: &str,
    name: &str,
    description: Option<&str>,
    release_date: Option<&str>,
    start_date: Option<&str>,
    released: bool,
    archived: bool,
) -> Result<()> {
    let version = client
        .create_project_version(
            project,
            name,
            description,
            release_date,
            start_date,
            released,
            archived,
        )
        .await?;
    println!("Created version {} (id: {}).", version.name, version.id);
    Ok(())
}

/// Prints versions as a formatted table.
fn print_versions(result: &JiraProjectVersionList) {
    if result.versions.is_empty() {
        println!("No versions found.");
        return;
    }

    let id_width = result
        .versions
        .iter()
        .map(|v| v.id.len())
        .max()
        .unwrap_or(2)
        .max(2);
    let name_width = result
        .versions
        .iter()
        .map(|v| v.name.len())
        .max()
        .unwrap_or(4)
        .max(4);

    println!(
        "{:<id_width$}  {:<name_width$}  RELEASED  ARCHIVED  RELEASE     DESCRIPTION",
        "ID", "NAME"
    );
    let desc_sep = "-".repeat(11);
    println!(
        "{:<id_width$}  {:<name_width$}  --------  --------  ----------  {desc_sep}",
        "-".repeat(id_width),
        "-".repeat(name_width),
    );

    for v in &result.versions {
        let release = format_date(v.release_date.as_deref());
        let description = v.description.as_deref().unwrap_or("-");
        println!(
            "{:<id_width$}  {:<name_width$}  {:<8}  {:<8}  {:<10}  {}",
            v.id,
            v.name,
            yes_no(v.released),
            yes_no(v.archived),
            release,
            description,
        );
    }
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

fn format_date(date: Option<&str>) -> &str {
    match date {
        Some(d) if d.len() >= 10 => &d[..10],
        Some(d) => d,
        None => "-",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::jira_types::JiraProjectVersion;

    fn sample_version(
        id: &str,
        name: &str,
        released: bool,
        archived: bool,
        release_date: Option<&str>,
    ) -> JiraProjectVersion {
        JiraProjectVersion {
            id: id.to_string(),
            name: name.to_string(),
            description: None,
            project_key: "PROJ".to_string(),
            released,
            archived,
            release_date: release_date.map(String::from),
            start_date: None,
        }
    }

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "user@test.com", "token").unwrap()
    }

    // ── tri_state ──────────────────────────────────────────────────

    #[test]
    fn tri_state_neither() {
        assert_eq!(tri_state(false, false), None);
    }

    #[test]
    fn tri_state_yes() {
        assert_eq!(tri_state(true, false), Some(true));
    }

    #[test]
    fn tri_state_no() {
        assert_eq!(tri_state(false, true), Some(false));
    }

    #[test]
    fn tri_state_yes_wins_when_both_set() {
        // Clap normally rejects this via conflicts_with; defensive default.
        assert_eq!(tri_state(true, true), Some(true));
    }

    // ── format helpers ─────────────────────────────────────────────

    #[test]
    fn yes_no_true() {
        assert_eq!(yes_no(true), "yes");
    }

    #[test]
    fn yes_no_false() {
        assert_eq!(yes_no(false), "no");
    }

    #[test]
    fn format_date_full_iso() {
        assert_eq!(format_date(Some("2026-04-01T00:00:00.000Z")), "2026-04-01");
    }

    #[test]
    fn format_date_just_date() {
        assert_eq!(format_date(Some("2026-04-01")), "2026-04-01");
    }

    #[test]
    fn format_date_short() {
        assert_eq!(format_date(Some("2026")), "2026");
    }

    #[test]
    fn format_date_none() {
        assert_eq!(format_date(None), "-");
    }

    // ── print_versions ─────────────────────────────────────────────

    #[test]
    fn print_versions_empty() {
        let result = JiraProjectVersionList {
            versions: vec![],
            total: 0,
        };
        print_versions(&result);
    }

    #[test]
    fn print_versions_with_data() {
        let result = JiraProjectVersionList {
            versions: vec![
                sample_version("10000", "1.0.0", true, false, Some("2026-04-01")),
                sample_version("10001", "1.1.0", false, false, None),
            ],
            total: 2,
        };
        print_versions(&result);
    }

    // ── dispatch ───────────────────────────────────────────────────

    #[test]
    fn version_command_list_variant() {
        let cmd = VersionCommand {
            command: VersionSubcommands::List(ListCommand {
                project: "PROJ".to_string(),
                released: false,
                unreleased: false,
                archived: false,
                unarchived: false,
                output: OutputFormat::Table,
            }),
        };
        assert!(matches!(cmd.command, VersionSubcommands::List(_)));
    }

    #[test]
    fn version_command_create_variant() {
        let cmd = VersionCommand {
            command: VersionSubcommands::Create(CreateCommand {
                project: "PROJ".to_string(),
                name: "1.0.0".to_string(),
                description: None,
                release_date: None,
                start_date: None,
                released: false,
                archived: false,
            }),
        };
        assert!(matches!(cmd.command, VersionSubcommands::Create(_)));
    }

    #[test]
    fn version_command_release_variant() {
        let cmd = VersionCommand {
            command: VersionSubcommands::Release(ReleaseCommand {
                version_id: "100".to_string(),
                release_date: None,
            }),
        };
        assert!(matches!(cmd.command, VersionSubcommands::Release(_)));
    }

    #[test]
    fn version_command_archive_variant() {
        let cmd = VersionCommand {
            command: VersionSubcommands::Archive(ArchiveCommand {
                version_id: "100".to_string(),
            }),
        };
        assert!(matches!(cmd.command, VersionSubcommands::Archive(_)));
    }

    #[test]
    fn version_command_rename_variant() {
        let cmd = VersionCommand {
            command: VersionSubcommands::Rename(RenameCommand {
                version_id: "100".to_string(),
                name: "2.0".to_string(),
                description: None,
            }),
        };
        assert!(matches!(cmd.command, VersionSubcommands::Rename(_)));
    }

    #[test]
    fn version_command_delete_variant() {
        let cmd = VersionCommand {
            command: VersionSubcommands::Delete(DeleteCommand {
                version_id: "100".to_string(),
                move_fix_issues_to: None,
                move_affected_issues_to: None,
                force: false,
                dry_run: false,
            }),
        };
        assert!(matches!(cmd.command, VersionSubcommands::Delete(_)));
    }

    // ── run_* version functions ────────────────────────────────────

    #[tokio::test]
    async fn run_list_versions_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/project/PROJ/versions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {"id": "1", "name": "1.0", "released": true, "archived": false}
                ])),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(
            run_list_versions(&client, "PROJ", None, None, &OutputFormat::Table)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_list_versions_yaml_output_returns_early() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/project/PROJ/versions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {"id": "1", "name": "1.0", "released": true, "archived": false}
                ])),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        // Yaml output path branches before print_versions, exercising the
        // `if output_as { return Ok(()); }` early return.
        assert!(
            run_list_versions(&client, "PROJ", None, None, &OutputFormat::Yaml)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_list_versions_with_filter() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/project/PROJ/versions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {"id": "1", "name": "1.0", "released": true, "archived": false},
                    {"id": "2", "name": "2.0", "released": false, "archived": false},
                ])),
            )
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(
            run_list_versions(&client, "PROJ", Some(true), None, &OutputFormat::Table)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_list_versions_api_error() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/project/NONE/versions",
            ))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_list_versions(&client, "NONE", None, None, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn run_create_version_success() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/version"))
            .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(
                serde_json::json!({"id": "100", "name": "1.0.0", "released": false, "archived": false}),
            ))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        assert!(
            run_create_version(&client, "PROJ", "1.0.0", None, None, None, false, false)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn run_create_version_forbidden() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/version"))
            .respond_with(wiremock::ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let err = run_create_version(&client, "PROJ", "1.0", None, None, None, false, false)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn run_create_version_invalid_date() {
        let server = wiremock::MockServer::start().await;
        // No mock — request must short-circuit before HTTP.
        let client = mock_client(&server.uri());
        let err = run_create_version(
            &client,
            "PROJ",
            "1.0",
            None,
            Some("not-a-date"),
            None,
            false,
            false,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("YYYY-MM-DD"));
    }

    // ── execute() integration via injected client (issue #950) ─────
    //
    // The happy-path tests drive each `*Command::execute_with()` end-to-end
    // against a wiremock server by injecting a client built from explicit
    // [`AtlassianCredentials`] pointed at the mock. They do NOT touch the
    // process-global `ATLASSIAN_*` env vars and therefore need no mutex and
    // run fully in parallel — the dependency-injection fix for the flaky
    // env-race documented in issue #950.
    //
    // The error-path tests below still exercise the production `execute()` ->
    // `create_client()` wrappers (which read the environment), so they clear
    // credentials behind the one canonical [`EnvGuard`]/`AUTH_ENV_MUTEX`. That
    // mutation is fully serialised and deterministic (it only ever produces
    // `CredentialsNotFound`), so it is not subject to the original race.

    use crate::atlassian::auth::test_util::EnvGuard;
    use crate::atlassian::auth::AtlassianCredentials;
    use crate::cli::atlassian::helpers::create_client_from;

    /// Credentials pointed at a mock server. Uses dummy email/token; the mock
    /// does not authenticate, it only matches method + path.
    fn mock_credentials(instance_url: &str) -> AtlassianCredentials {
        AtlassianCredentials {
            instance_url: instance_url.to_string(),
            email: "test@example.com".to_string(),
            api_token: "test-token".into(),
        }
    }

    #[tokio::test]
    async fn version_command_execute_list_dispatches_through_create_client() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/rest/api/3/project/PROJ/versions",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {"id": "1", "name": "1.0", "released": false, "archived": false}
                ])),
            )
            .mount(&server)
            .await;

        let cmd = VersionCommand {
            command: VersionSubcommands::List(ListCommand {
                project: "PROJ".to_string(),
                released: false,
                unreleased: false,
                archived: false,
                unarchived: false,
                output: OutputFormat::Yaml,
            }),
        };
        let client = create_client_from(mock_credentials(&server.uri()));
        assert!(cmd.execute_with(client).await.is_ok());
    }

    /// Drives the production `VersionCommand::execute` -> `ListCommand::execute`
    /// -> `create_client()` -> `execute_with(Err)` chain with credentials
    /// cleared, so the `?` propagation path runs end-to-end (covers the
    /// env-reading wrappers the injection tests bypass).
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn version_command_execute_list_propagates_create_client_error() {
        let guard = EnvGuard::take();
        let _home = guard.clear_credentials();

        let cmd = VersionCommand {
            command: VersionSubcommands::List(ListCommand {
                project: "PROJ".to_string(),
                released: false,
                unreleased: false,
                archived: false,
                unarchived: false,
                output: OutputFormat::Yaml,
            }),
        };
        assert!(cmd.execute().await.is_err());
    }

    /// Same as above for the `Create` arm: covers `VersionCommand::execute`
    /// (Create), `CreateCommand::execute`, and `create_client()`.
    #[tokio::test(flavor = "current_thread")]
    #[allow(clippy::await_holding_lock)]
    async fn version_command_execute_create_propagates_create_client_error() {
        let guard = EnvGuard::take();
        let _home = guard.clear_credentials();

        let cmd = VersionCommand {
            command: VersionSubcommands::Create(CreateCommand {
                project: "PROJ".to_string(),
                name: "1.0.0".to_string(),
                description: None,
                release_date: None,
                start_date: None,
                released: false,
                archived: false,
            }),
        };
        assert!(cmd.execute().await.is_err());
    }

    #[tokio::test]
    async fn version_command_execute_create_dispatches_through_create_client() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/rest/api/3/version"))
            .respond_with(wiremock::ResponseTemplate::new(201).set_body_json(
                serde_json::json!({"id": "100", "name": "1.0.0", "released": false, "archived": false}),
            ))
            .mount(&server)
            .await;

        let cmd = VersionCommand {
            command: VersionSubcommands::Create(CreateCommand {
                project: "PROJ".to_string(),
                name: "1.0.0".to_string(),
                description: None,
                release_date: None,
                start_date: None,
                released: false,
                archived: false,
            }),
        };
        let client = create_client_from(mock_credentials(&server.uri()));
        assert!(cmd.execute_with(client).await.is_ok());
    }

    #[tokio::test]
    async fn version_command_execute_release_dispatches_through_client() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/version/100"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "100", "name": "1.0"})),
            )
            .mount(&server)
            .await;

        let cmd = VersionCommand {
            command: VersionSubcommands::Release(ReleaseCommand {
                version_id: "100".to_string(),
                release_date: Some("2026-04-16".to_string()),
            }),
        };
        let client = create_client_from(mock_credentials(&server.uri()));
        assert!(cmd.execute_with(client).await.is_ok());
    }

    #[tokio::test]
    async fn version_command_execute_rename_dispatches_through_client() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path("/rest/api/3/version/100"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"id": "100", "name": "2.0"})),
            )
            .mount(&server)
            .await;

        let cmd = VersionCommand {
            command: VersionSubcommands::Rename(RenameCommand {
                version_id: "100".to_string(),
                name: "2.0".to_string(),
                description: Some("Second release".to_string()),
            }),
        };
        let client = create_client_from(mock_credentials(&server.uri()));
        assert!(cmd.execute_with(client).await.is_ok());
    }

    // ── DeleteCommand (confirm-guarded, tested via execute_with_io) ──

    #[tokio::test]
    async fn delete_version_force_calls_delete() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path("/rest/api/3/version/100"))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let (client, _) = create_client_from(mock_credentials(&server.uri())).unwrap();
        let cmd = DeleteCommand {
            version_id: "100".to_string(),
            move_fix_issues_to: None,
            move_affected_issues_to: None,
            force: true,
            dry_run: false,
        };
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap();
        assert!(String::from_utf8(output)
            .unwrap()
            .contains("Deleted version 100."));
    }

    #[tokio::test]
    async fn delete_version_dry_run_makes_no_api_call() {
        let (client, _) = create_client_from(mock_credentials("http://127.0.0.1:1")).unwrap();
        let cmd = DeleteCommand {
            version_id: "100".to_string(),
            move_fix_issues_to: None,
            move_affected_issues_to: None,
            force: false,
            dry_run: true,
        };
        let mut input = std::io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Would delete version 100."));
        assert!(!out.contains("Deleted version"));
    }

    #[tokio::test]
    async fn delete_version_prompt_no_makes_no_delete() {
        let (client, _) = create_client_from(mock_credentials("http://127.0.0.1:1")).unwrap();
        let cmd = DeleteCommand {
            version_id: "100".to_string(),
            move_fix_issues_to: None,
            move_affected_issues_to: None,
            force: false,
            dry_run: false,
        };
        let mut input = std::io::Cursor::new(b"n\n".to_vec());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap();
        assert!(!String::from_utf8(output)
            .unwrap()
            .contains("Deleted version"));
    }
}
