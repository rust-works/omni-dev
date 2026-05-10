//! CLI command for deleting Confluence pages.

use std::io::{self, BufRead, Write};

use anyhow::Result;
use clap::Parser;

use crate::atlassian::api::AtlassianApi;
use crate::atlassian::confluence_api::ConfluenceApi;
use crate::cli::atlassian::confirm::{guard_destructive_with_io, GuardOptions, GuardOutcome};
use crate::cli::atlassian::helpers::create_client;

/// Deletes a Confluence page.
#[derive(Parser)]
pub struct DeleteCommand {
    /// Confluence page ID (e.g., 12345678).
    pub id: String,

    /// Skips the confirmation prompt.
    #[arg(long)]
    pub force: bool,

    /// Prints what would be deleted without making any API calls.
    #[arg(long)]
    pub dry_run: bool,

    /// Permanently purges the page instead of moving to trash (requires space admin).
    #[arg(long)]
    pub purge: bool,
}

impl DeleteCommand {
    /// Executes the delete command.
    pub async fn execute(self) -> Result<()> {
        let (client, instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);
        let mut reader = io::BufReader::new(io::stdin());
        let mut writer = io::stdout();
        self.execute_with_io(&api, &instance_url, &mut reader, &mut writer)
            .await
    }

    /// Inner form taking explicit API, instance URL, and IO handles, for unit tests.
    async fn execute_with_io(
        self,
        api: &ConfluenceApi,
        instance_url: &str,
        reader: &mut (dyn BufRead + Send),
        writer: &mut (dyn Write + Send),
    ) -> Result<()> {
        if !self.force || self.dry_run {
            let item = api.get_content(&self.id).await?;
            let suffix = if self.purge { " (purge)" } else { "" };
            let prompt = format!("Delete page {} ({}){}? [y/N] ", self.id, item.title, suffix);
            let dry_run_message =
                format!("Would delete page {} ({}){}.", self.id, item.title, suffix);

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

        api.delete_page(&self.id, self.purge).await?;
        writeln!(writer, "Deleted page {} from {}.", self.id, instance_url)?;

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::atlassian::client::AtlassianClient;
    use std::io::Cursor;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn setup_mock() -> (MockServer, ConfluenceApi) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/wiki/api/v2/pages/12345"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "12345",
                "title": "Architecture Overview",
                "status": "current",
                "spaceId": "98765",
                "version": {"number": 1},
                "body": {"atlas_doc_format": {"value": "{\"version\":1,\"type\":\"doc\",\"content\":[]}"}},
                "parentId": null
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/wiki/api/v2/spaces/98765"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(&server)
            .await;
        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);
        (server, api)
    }

    #[test]
    fn delete_command_struct_fields() {
        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: false,
            dry_run: false,
            purge: false,
        };
        assert_eq!(cmd.id, "12345");
        assert!(!cmd.force);
        assert!(!cmd.dry_run);
        assert!(!cmd.purge);
    }

    #[test]
    fn delete_command_force_mode() {
        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: true,
            dry_run: false,
            purge: false,
        };
        assert!(cmd.force);
    }

    #[test]
    fn delete_command_dry_run_mode() {
        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: false,
            dry_run: true,
            purge: false,
        };
        assert!(cmd.dry_run);
    }

    #[test]
    fn delete_command_purge_mode() {
        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: true,
            dry_run: false,
            purge: true,
        };
        assert!(cmd.purge);
    }

    #[tokio::test]
    async fn execute_with_force_calls_delete() {
        let (server, api) = setup_mock().await;
        Mock::given(method("DELETE"))
            .and(path("/wiki/api/v2/pages/12345"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: true,
            dry_run: false,
            purge: false,
        };
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(
            &api,
            "https://example.atlassian.net",
            &mut input,
            &mut output,
        )
        .await
        .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Deleted page 12345"));
    }

    #[tokio::test]
    async fn execute_with_dry_run_does_not_call_delete() {
        let (_server, api) = setup_mock().await;

        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: false,
            dry_run: true,
            purge: false,
        };
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(
            &api,
            "https://example.atlassian.net",
            &mut input,
            &mut output,
        )
        .await
        .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Would delete page 12345 (Architecture Overview)."));
        assert!(!out.contains("Deleted page 12345"));
    }

    #[tokio::test]
    async fn execute_with_dry_run_and_purge_includes_suffix() {
        let (_server, api) = setup_mock().await;

        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: false,
            dry_run: true,
            purge: true,
        };
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(
            &api,
            "https://example.atlassian.net",
            &mut input,
            &mut output,
        )
        .await
        .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("(purge)"));
    }

    #[tokio::test]
    async fn execute_with_prompt_yes_calls_delete() {
        let (server, api) = setup_mock().await;
        Mock::given(method("DELETE"))
            .and(path("/wiki/api/v2/pages/12345"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: false,
            dry_run: false,
            purge: false,
        };
        let mut input = Cursor::new(b"y\n".to_vec());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(
            &api,
            "https://example.atlassian.net",
            &mut input,
            &mut output,
        )
        .await
        .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Delete page 12345 (Architecture Overview)?"));
        assert!(out.contains("Deleted page 12345"));
    }

    #[tokio::test]
    async fn execute_with_prompt_no_does_not_call_delete() {
        let (_server, api) = setup_mock().await;

        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: false,
            dry_run: false,
            purge: false,
        };
        let mut input = Cursor::new(b"n\n".to_vec());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(
            &api,
            "https://example.atlassian.net",
            &mut input,
            &mut output,
        )
        .await
        .unwrap();
        let out = String::from_utf8(output).unwrap();
        assert!(out.contains("Cancelled."));
        assert!(!out.contains("Deleted page 12345"));
    }

    #[tokio::test]
    async fn execute_with_force_and_purge_appends_query_param() {
        let (server, api) = setup_mock().await;
        Mock::given(method("DELETE"))
            .and(path("/wiki/api/v2/pages/12345"))
            .and(query_param("purge", "true"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: true,
            dry_run: false,
            purge: true,
        };
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        cmd.execute_with_io(
            &api,
            "https://example.atlassian.net",
            &mut input,
            &mut output,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn execute_with_force_propagates_delete_api_error() {
        let (server, api) = setup_mock().await;
        Mock::given(method("DELETE"))
            .and(path("/wiki/api/v2/pages/12345"))
            .respond_with(ResponseTemplate::new(403).set_body_string("Forbidden"))
            .mount(&server)
            .await;

        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: true,
            dry_run: false,
            purge: false,
        };
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        let err = cmd
            .execute_with_io(
                &api,
                "https://example.atlassian.net",
                &mut input,
                &mut output,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("403"));
    }

    #[tokio::test]
    async fn execute_lookup_error_aborts_before_prompt() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/wiki/api/v2/pages/12345"))
            .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
            .mount(&server)
            .await;
        let client = AtlassianClient::new(&server.uri(), "u@t.com", "tok").unwrap();
        let api = ConfluenceApi::new(client);

        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: false,
            dry_run: false,
            purge: false,
        };
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut output = Vec::<u8>::new();
        let err = cmd
            .execute_with_io(
                &api,
                "https://example.atlassian.net",
                &mut input,
                &mut output,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    /// Force-mode + failing writer covers `?` on the post-API writeln.
    #[tokio::test]
    async fn execute_with_force_propagates_writeln_error() {
        use crate::test_support::failing_io::FailingWriter;
        let (server, api) = setup_mock().await;
        Mock::given(method("DELETE"))
            .and(path("/wiki/api/v2/pages/12345"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: true,
            dry_run: false,
            purge: false,
        };
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut writer = FailingWriter;
        let err = cmd
            .execute_with_io(
                &api,
                "https://example.atlassian.net",
                &mut input,
                &mut writer,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("simulated write failure"));
    }

    /// Dry-run with a failing writer covers `?` on guard_destructive_with_io.
    #[tokio::test]
    async fn execute_dry_run_propagates_guard_error() {
        use crate::test_support::failing_io::FailingWriter;
        let (_server, api) = setup_mock().await;
        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: false,
            dry_run: true,
            purge: false,
        };
        let mut input = Cursor::new(Vec::<u8>::new());
        let mut writer = FailingWriter;
        let err = cmd
            .execute_with_io(
                &api,
                "https://example.atlassian.net",
                &mut input,
                &mut writer,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("simulated write failure"));
    }

    /// End-to-end exercise of the public `execute()` wrapper.
    #[tokio::test]
    async fn execute_with_force_drives_create_client_and_calls_delete() {
        use crate::test_support::atlassian_env::AtlassianEnvGuard;
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/wiki/api/v2/pages/12345"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let _env = AtlassianEnvGuard::new(&server.uri(), "u@t.com", "tok");
        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: true,
            dry_run: false,
            purge: false,
        };
        cmd.execute().await.unwrap();
    }
}
