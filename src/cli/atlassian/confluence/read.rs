//! CLI command for reading Confluence pages.

use anyhow::Result;
use clap::Parser;

use crate::atlassian::confluence_api::ConfluenceApi;
use crate::cli::atlassian::format::ContentFormat;
use crate::cli::atlassian::helpers::{create_client, render_content_item, run_read};

/// Fetches a Confluence page and outputs it as JFM markdown or ADF JSON.
///
/// Any author/version metadata is returned as Atlassian account IDs — resolve
/// them to display names with `omni-dev atlassian confluence user get`.
#[derive(Parser)]
pub struct ReadCommand {
    /// Confluence page ID (e.g., 12345678).
    pub id: String,

    /// Output file (writes to stdout if omitted).
    #[arg(short, long)]
    pub output: Option<String>,

    /// Output format.
    #[arg(long, value_enum, default_value_t = ContentFormat::Jfm)]
    pub format: ContentFormat,

    /// Read a specific historical version instead of the current head
    /// (e.g. `--version 3`). Confluence stores each version as an immutable
    /// snapshot; omit for the latest.
    #[arg(long)]
    pub version: Option<u32>,
}

impl ReadCommand {
    /// Fetches the page and outputs it.
    pub async fn execute(self) -> Result<()> {
        let (client, instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);

        match self.version {
            Some(version) => {
                // `get_page_at_version` is Confluence-specific (not on the shared
                // `AtlassianApi` trait), so fetch here and reuse the shared renderer.
                let item = api.get_page_at_version(&self.id, version).await?;
                render_content_item(&item, self.output.as_deref(), &self.format, &instance_url)
            }
            None => {
                run_read(
                    &self.id,
                    self.output.as_deref(),
                    &self.format,
                    &api,
                    &instance_url,
                )
                .await
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::await_holding_lock)]
mod tests {
    use super::*;

    #[test]
    fn read_command_struct_fields() {
        let cmd = ReadCommand {
            id: "12345".to_string(),
            output: Some("page.md".to_string()),
            format: ContentFormat::Jfm,
            version: None,
        };
        assert_eq!(cmd.id, "12345");
        assert_eq!(cmd.output.as_deref(), Some("page.md"));
        assert!(matches!(cmd.format, ContentFormat::Jfm));
        assert!(cmd.version.is_none());
    }

    #[test]
    fn read_command_adf_format() {
        let cmd = ReadCommand {
            id: "99999".to_string(),
            output: None,
            format: ContentFormat::Adf,
            version: Some(3),
        };
        assert_eq!(cmd.id, "99999");
        assert!(cmd.output.is_none());
        assert!(matches!(cmd.format, ContentFormat::Adf));
        assert_eq!(cmd.version, Some(3));
    }

    // ── ReadCommand::execute ────────────────────────────────────────

    fn set_atlassian_env(uri: &str) {
        std::env::set_var(crate::atlassian::auth::ATLASSIAN_INSTANCE_URL, uri);
        std::env::set_var(crate::atlassian::auth::ATLASSIAN_EMAIL, "user@test.com");
        std::env::set_var(crate::atlassian::auth::ATLASSIAN_API_TOKEN, "t");
    }

    fn clear_atlassian_env() {
        std::env::remove_var(crate::atlassian::auth::ATLASSIAN_INSTANCE_URL);
        std::env::remove_var(crate::atlassian::auth::ATLASSIAN_EMAIL);
        std::env::remove_var(crate::atlassian::auth::ATLASSIAN_API_TOKEN);
    }

    /// Mounts a page response (optionally version-pinned via the `version`
    /// query parameter) plus its space lookup.
    async fn mount_page(server: &wiremock::MockServer, version: Option<u32>, text: &str) {
        let adf_value = format!(
            "{{\"version\":1,\"type\":\"doc\",\"content\":[{{\"type\":\"paragraph\",\"content\":[{{\"type\":\"text\",\"text\":{}}}]}}]}}",
            serde_json::Value::String(text.to_string())
        );
        let mut mock = wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/pages/12345"));
        if let Some(v) = version {
            mock = mock.and(wiremock::matchers::query_param("version", v.to_string()));
        }
        mock.respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "12345",
                "title": "Mock Page",
                "status": "current",
                "spaceId": "98",
                "version": {"number": version.unwrap_or(1)},
                "body": {"atlas_doc_format": {"value": adf_value}}
            })),
        )
        .mount(server)
        .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/wiki/api/v2/spaces/98"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"key": "ENG"})),
            )
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn read_command_execute_propagates_create_client_error() {
        let guard = crate::atlassian::auth::test_util::EnvGuard::take();
        let _home = guard.clear_credentials();

        let cmd = ReadCommand {
            id: "12345".to_string(),
            output: None,
            format: ContentFormat::Jfm,
            version: None,
        };
        assert!(cmd.execute().await.is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_command_execute_latest_version_writes_output_file() {
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let server = wiremock::MockServer::start().await;
        mount_page(&server, None, "Latest body").await;

        let temp_dir = tempfile::tempdir().unwrap();
        let out_path = temp_dir.path().join("page.md");

        set_atlassian_env(&server.uri());
        let cmd = ReadCommand {
            id: "12345".to_string(),
            output: Some(out_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            version: None,
        };
        let result = cmd.execute().await;
        clear_atlassian_env();
        assert!(result.is_ok(), "{result:?}");

        let written = std::fs::read_to_string(&out_path).unwrap();
        assert!(written.contains("Latest body"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_command_execute_at_version_writes_output_file() {
        let _lock = crate::atlassian::auth::test_util::AUTH_ENV_MUTEX
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let server = wiremock::MockServer::start().await;
        mount_page(&server, Some(3), "Version three body").await;

        let temp_dir = tempfile::tempdir().unwrap();
        let out_path = temp_dir.path().join("page-v3.md");

        set_atlassian_env(&server.uri());
        let cmd = ReadCommand {
            id: "12345".to_string(),
            output: Some(out_path.to_str().unwrap().to_string()),
            format: ContentFormat::Jfm,
            version: Some(3),
        };
        let result = cmd.execute().await;
        clear_atlassian_env();
        assert!(result.is_ok(), "{result:?}");

        let written = std::fs::read_to_string(&out_path).unwrap();
        assert!(written.contains("Version three body"));
    }
}
