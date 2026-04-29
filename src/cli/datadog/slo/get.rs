//! CLI command for `omni-dev datadog slo get`.

use anyhow::Result;
use clap::Parser;

use crate::cli::datadog::format::{output_as, OutputFormat};
use crate::cli::datadog::helpers::create_client;
use crate::cli::datadog::slo::{render_slo_table, SloRow};
use crate::datadog::client::DatadogClient;
use crate::datadog::slo_api::SloApi;

/// Fetches a single Datadog SLO by id.
#[derive(Parser)]
pub struct GetCommand {
    /// Datadog SLO identifier.
    pub id: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl GetCommand {
    /// Executes the fetch against a freshly-created Datadog client.
    pub async fn execute(self) -> Result<()> {
        let (client, _site) = create_client()?;
        run_get(&client, &self.id, &self.output).await
    }
}

async fn run_get(client: &DatadogClient, id: &str, output: &OutputFormat) -> Result<()> {
    let slo = SloApi::new(client).get(id).await?;
    if output_as(&slo, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let row = SloRow {
        id: slo.id.as_str(),
        name: slo.name.as_str(),
        slo_type: slo.slo_type.as_str(),
        tags: &slo.tags,
    };
    render_slo_table(std::slice::from_ref(&row), &mut handle)?;
    if let Some(desc) = slo.description.as_deref() {
        if !desc.is_empty() {
            use std::io::Write;
            writeln!(handle, "DESCRIPTION: {desc}").ok();
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn slo_json() -> serde_json::Value {
        serde_json::json!({
            "id": "abc-def",
            "name": "Latency",
            "type": "metric",
            "tags": ["team:sre"],
            "monitor_ids": [],
            "description": "Latency under 200ms"
        })
    }

    // ── run_get ────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_get_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo/abc-def"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": slo_json()
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_get(&client, "abc-def", &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_get_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo/x"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": slo_json()
                })),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_get(&client, "x", &OutputFormat::Json).await.unwrap();
    }

    #[tokio::test]
    async fn run_get_propagates_404() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo/missing"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = run_get(&client, "missing", &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    #[tokio::test]
    async fn run_get_table_path_succeeds_when_description_missing() {
        // Covers the outer `if let Some(desc)` arm where the SLO has no
        // description field at all — `slo.description` is `None`.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo/x"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": {
                        "id": "x", "name": "y", "type": "metric",
                        "tags": [], "monitor_ids": []
                    }
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_get(&client, "x", &OutputFormat::Table).await.unwrap();
    }

    #[tokio::test]
    async fn run_get_table_path_succeeds_when_description_is_empty_string() {
        // Covers the inner `if !desc.is_empty()` false arm — the SLO has
        // `description: ""` so the `writeln!` is skipped. Without this
        // case, the closing `}` of the inner if has zero coverage.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo/x"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": {
                        "id": "x", "name": "y", "type": "metric",
                        "tags": [], "monitor_ids": [],
                        "description": ""
                    }
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_get(&client, "x", &OutputFormat::Table).await.unwrap();
    }

    // ── GetCommand::execute error paths ────────────────────────────

    #[tokio::test]
    async fn get_command_execute_errors_when_credentials_missing() {
        use crate::datadog::test_support::{with_empty_home, EnvGuard};
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);

        let cmd = GetCommand {
            id: "abc".into(),
            output: OutputFormat::Table,
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[tokio::test]
    async fn get_command_execute_end_to_end_via_api_url_override() {
        use std::fs;

        use crate::datadog::auth::{DATADOG_API_KEY, DATADOG_API_URL, DATADOG_APP_KEY};
        use crate::datadog::test_support::{with_empty_home, EnvGuard};

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo/abc"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": slo_json()
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let guard = EnvGuard::take();
        let dir = with_empty_home(&guard);
        let omni_dir = dir.path().join(".omni-dev");
        fs::create_dir_all(&omni_dir).unwrap();
        fs::write(
            omni_dir.join("settings.json"),
            r#"{"env":{"DATADOG_API_KEY":"api","DATADOG_APP_KEY":"app","DATADOG_SITE":"datadoghq.com"}}"#,
        )
        .unwrap();
        std::env::set_var(DATADOG_API_KEY, "api");
        std::env::set_var(DATADOG_APP_KEY, "app");
        std::env::set_var(DATADOG_API_URL, server.uri());

        let cmd = GetCommand {
            id: "abc".into(),
            output: OutputFormat::Json,
        };
        cmd.execute().await.unwrap();
    }
}
