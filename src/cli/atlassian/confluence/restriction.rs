//! CLI commands for managing Confluence page content restrictions.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};

use crate::atlassian::confluence_api::ConfluenceApi;
use crate::cli::atlassian::helpers::create_client;

/// Manages read/update restrictions on a Confluence page.
#[derive(Parser)]
pub struct RestrictionCommand {
    /// The restriction subcommand to execute.
    #[command(subcommand)]
    pub command: RestrictionSubcommands,
}

/// Restriction subcommands.
#[derive(Subcommand)]
pub enum RestrictionSubcommands {
    /// Shows the current restrictions on a page (mirrors the `confluence_restriction_get` MCP tool).
    Get(GetCommand),
    /// Grants a user or group a restriction for an operation (mirrors the `confluence_restriction_grant` MCP tool).
    Grant(SubjectCommand),
    /// Revokes a user's or group's restriction for an operation (mirrors the `confluence_restriction_revoke` MCP tool).
    Revoke(SubjectCommand),
}

impl RestrictionCommand {
    /// Executes the restriction command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            RestrictionSubcommands::Get(cmd) => cmd.execute().await,
            RestrictionSubcommands::Grant(cmd) => cmd.execute(true).await,
            RestrictionSubcommands::Revoke(cmd) => cmd.execute(false).await,
        }
    }
}

/// The operation a restriction applies to.
#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum RestrictionOperation {
    /// Restrict who can view the page.
    Read,
    /// Restrict who can edit the page.
    Update,
}

impl RestrictionOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Update => "update",
        }
    }
}

/// Shows current restrictions on a page.
#[derive(Parser)]
pub struct GetCommand {
    /// Confluence page (content) ID.
    pub id: String,
}

impl GetCommand {
    async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        let value = api.get_content_restrictions(&self.id).await?;
        let yaml =
            serde_yaml::to_string(&value).context("Failed to serialize restrictions as YAML")?;
        print!("{yaml}");
        Ok(())
    }
}

/// Grant/revoke a user or group a restriction for an operation.
#[derive(Parser)]
pub struct SubjectCommand {
    /// Confluence page (content) ID.
    pub id: String,

    /// Operation the restriction applies to.
    #[arg(long, value_enum)]
    pub operation: RestrictionOperation,

    /// Atlassian `accountId` of the user. Mutually exclusive with `--group`.
    #[arg(long, conflicts_with = "group")]
    pub account_id: Option<String>,

    /// Group name. Mutually exclusive with `--account-id`.
    #[arg(long, conflicts_with = "account_id")]
    pub group: Option<String>,
}

impl SubjectCommand {
    async fn execute(self, grant: bool) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        let op = self.operation.as_str();
        if grant {
            api.grant_content_restriction(
                &self.id,
                op,
                self.account_id.as_deref(),
                self.group.as_deref(),
            )
            .await?;
            println!(
                "Granted {op} restriction on page {} to {}.",
                self.id,
                self.subject()
            );
        } else {
            api.revoke_content_restriction(
                &self.id,
                op,
                self.account_id.as_deref(),
                self.group.as_deref(),
            )
            .await?;
            println!(
                "Revoked {op} restriction on page {} from {}.",
                self.id,
                self.subject()
            );
        }
        Ok(())
    }

    fn subject(&self) -> String {
        match (&self.account_id, &self.group) {
            (Some(a), _) => format!("user {a}"),
            (_, Some(g)) => format!("group {g}"),
            _ => "(none)".to_string(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn restriction_command_get_variant() {
        let cmd = RestrictionCommand {
            command: RestrictionSubcommands::Get(GetCommand {
                id: "12345".to_string(),
            }),
        };
        assert!(matches!(cmd.command, RestrictionSubcommands::Get(_)));
    }

    #[test]
    fn restriction_command_grant_variant() {
        let cmd = RestrictionCommand {
            command: RestrictionSubcommands::Grant(SubjectCommand {
                id: "12345".to_string(),
                operation: RestrictionOperation::Read,
                account_id: Some("acc-1".to_string()),
                group: None,
            }),
        };
        assert!(matches!(cmd.command, RestrictionSubcommands::Grant(_)));
    }

    #[test]
    fn operation_as_str_maps() {
        assert_eq!(RestrictionOperation::Read.as_str(), "read");
        assert_eq!(RestrictionOperation::Update.as_str(), "update");
    }

    #[test]
    fn subject_labels() {
        let user = SubjectCommand {
            id: "1".to_string(),
            operation: RestrictionOperation::Read,
            account_id: Some("acc".to_string()),
            group: None,
        };
        assert_eq!(user.subject(), "user acc");
        let group = SubjectCommand {
            id: "1".to_string(),
            operation: RestrictionOperation::Read,
            account_id: None,
            group: Some("devs".to_string()),
        };
        assert_eq!(group.subject(), "group devs");
    }

    /// Neither `--account-id` nor `--group` — clap's `conflicts_with` pairing
    /// makes this unreachable from the CLI, but `subject()` still has to render
    /// something for the message.
    #[test]
    fn subject_label_without_account_or_group() {
        let neither = SubjectCommand {
            id: "1".to_string(),
            operation: RestrictionOperation::Update,
            account_id: None,
            group: None,
        };
        assert_eq!(neither.subject(), "(none)");
    }

    // ── execute() end-to-end (drives create_client + the API call) ──

    #[tokio::test]
    async fn get_execute_prints_restrictions() {
        use crate::test_support::atlassian_env::AtlassianEnvGuard;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/restriction",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"results": [{"operation": "read"}]})),
            )
            .mount(&server)
            .await;
        let _env = AtlassianEnvGuard::new(&server.uri(), "u@t.com", "tok");
        GetCommand {
            id: "12345".to_string(),
        }
        .execute()
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn grant_execute_puts_user_restriction() {
        use crate::test_support::atlassian_env::AtlassianEnvGuard;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("PUT"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/restriction/byOperation/update/user",
            ))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .mount(&server)
            .await;
        let _env = AtlassianEnvGuard::new(&server.uri(), "u@t.com", "tok");
        // Routed through the parent so the `Grant` dispatch arm is covered too.
        RestrictionCommand {
            command: RestrictionSubcommands::Grant(SubjectCommand {
                id: "12345".to_string(),
                operation: RestrictionOperation::Update,
                account_id: Some("acc-1".to_string()),
                group: None,
            }),
        }
        .execute()
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn revoke_execute_deletes_group_restriction() {
        use crate::test_support::atlassian_env::AtlassianEnvGuard;
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("DELETE"))
            .and(wiremock::matchers::path(
                "/wiki/rest/api/content/12345/restriction/byOperation/read/group/devs",
            ))
            .respond_with(wiremock::ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let _env = AtlassianEnvGuard::new(&server.uri(), "u@t.com", "tok");
        // Routed through the parent so the `Revoke` dispatch arm is covered too.
        RestrictionCommand {
            command: RestrictionSubcommands::Revoke(SubjectCommand {
                id: "12345".to_string(),
                operation: RestrictionOperation::Read,
                account_id: None,
                group: Some("devs".to_string()),
            }),
        }
        .execute()
        .await
        .unwrap();
    }
}
