//! CLI command for deleting JIRA issues.

use std::io::{self, BufRead, Write};

use anyhow::Result;
use clap::Parser;

use crate::atlassian::client::AtlassianClient;
use crate::cli::atlassian::confirm::{guard_destructive_with_io, GuardOptions, GuardOutcome};
use crate::cli::atlassian::helpers::create_client;

/// Deletes a JIRA issue.
#[derive(Parser)]
pub struct DeleteCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Skips the confirmation prompt.
    #[arg(long)]
    pub force: bool,

    /// Prints what would be deleted without making any API calls.
    #[arg(long)]
    pub dry_run: bool,
}

impl DeleteCommand {
    /// Executes the delete command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;
        let mut reader = io::BufReader::new(io::stdin());
        let mut writer = io::stdout();
        self.execute_with_io(&client, &mut reader, &mut writer)
            .await
    }

    /// Inner form taking explicit client and IO handles, for unit tests.
    async fn execute_with_io(
        self,
        client: &AtlassianClient,
        reader: &mut (dyn BufRead + Send),
        writer: &mut (dyn Write + Send),
    ) -> Result<()> {
        if !self.force || self.dry_run {
            let issue = client.get_issue(&self.key).await?;
            let prompt = format!("Delete {} ({})? [y/N] ", self.key, issue.summary);
            let dry_run_message = format!("Would delete {} ({}).", self.key, issue.summary);

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

        client.delete_issue(&self.key).await?;
        writeln!(writer, "Deleted {}.", self.key)?;

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn mock_client(base_url: &str) -> AtlassianClient {
        AtlassianClient::new(base_url, "u@t.com", "tok").unwrap()
    }

    fn issue_body() -> serde_json::Value {
        serde_json::json!({
            "key": "PROJ-1",
            "fields": {"summary": "Fix the bug"},
        })
    }

    #[test]
    fn delete_command_struct_fields() {
        let cmd = DeleteCommand {
            key: "PROJ-42".to_string(),
            force: false,
            dry_run: false,
        };
        assert_eq!(cmd.key, "PROJ-42");
        assert!(!cmd.force);
        assert!(!cmd.dry_run);
    }

    #[test]
    fn delete_command_force_mode() {
        let cmd = DeleteCommand {
            key: "PROJ-1".to_string(),
            force: true,
            dry_run: false,
        };
        assert!(cmd.force);
    }

    #[test]
    fn delete_command_dry_run_mode() {
        let cmd = DeleteCommand {
            key: "PROJ-1".to_string(),
            force: false,
            dry_run: true,
        };
        assert!(cmd.dry_run);
    }

    #[tokio::test]
    async fn execute_with_force_skips_lookup_and_calls_delete() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let cmd = DeleteCommand {
            key: "PROJ-1".to_string(),
            force: true,
            dry_run: false,
        };
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Deleted PROJ-1."));
    }

    #[tokio::test]
    async fn execute_with_dry_run_does_not_call_delete() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_body()))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let cmd = DeleteCommand {
            key: "PROJ-1".to_string(),
            force: false,
            dry_run: true,
        };
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Would delete PROJ-1 (Fix the bug)."));
        assert!(!out.contains("Deleted PROJ-1."));
    }

    #[tokio::test]
    async fn execute_with_prompt_yes_calls_delete() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_body()))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let cmd = DeleteCommand {
            key: "PROJ-1".to_string(),
            force: false,
            dry_run: false,
        };
        let mut input = Cursor::new(b"y\n".to_vec());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Delete PROJ-1 (Fix the bug)?"));
        assert!(out.contains("Deleted PROJ-1."));
    }

    #[tokio::test]
    async fn execute_with_prompt_no_does_not_call_delete() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(issue_body()))
            .expect(1)
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let cmd = DeleteCommand {
            key: "PROJ-1".to_string(),
            force: false,
            dry_run: false,
        };
        let mut input = Cursor::new(b"n\n".to_vec());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Cancelled."));
        assert!(!out.contains("Deleted PROJ-1."));
    }

    #[tokio::test]
    async fn execute_with_force_propagates_api_error() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let cmd = DeleteCommand {
            key: "PROJ-1".to_string(),
            force: true,
            dry_run: false,
        };
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        let err = cmd
            .execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn execute_lookup_error_aborts_before_prompt() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/rest/api/3/issue/PROJ-1"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;

        let client = mock_client(&server.uri());
        let cmd = DeleteCommand {
            key: "PROJ-1".to_string(),
            force: false,
            dry_run: false,
        };
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        let err = cmd
            .execute_with_io(&client, &mut input, &mut output)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }
}
