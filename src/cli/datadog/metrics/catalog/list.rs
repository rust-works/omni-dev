//! CLI command for `omni-dev datadog metrics catalog list`.

use anyhow::Result;
use clap::Parser;

use crate::cli::datadog::format::{output_as, OutputFormat};
use crate::cli::datadog::metrics::catalog::render_metrics_table;
use crate::datadog::client::DatadogClient;
use crate::datadog::metrics_catalog_api::MetricsCatalogApi;

/// Lists metrics in the Datadog catalog.
#[derive(Parser)]
pub struct ListCommand {
    /// Filter by host (e.g. `web-01`).
    #[arg(long)]
    pub host: Option<String>,

    /// `from` cutoff in Unix epoch seconds; only metrics ingested since
    /// this timestamp are returned.
    #[arg(long)]
    pub from: Option<i64>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `DatadogCommand::execute`. Taking the client as a parameter keeps this
    /// entry point free of process env and fully testable (issue #1030).
    pub async fn execute(self, client: &DatadogClient) -> Result<()> {
        run_list(client, self.host.as_deref(), self.from, &self.output).await
    }
}

async fn run_list(
    client: &DatadogClient,
    host: Option<&str>,
    from: Option<i64>,
    output: &OutputFormat,
) -> Result<()> {
    let result = MetricsCatalogApi::new(client).list(host, from).await?;
    if output_as(&result, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_metrics_table(&result.metrics, &mut handle)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn body() -> serde_json::Value {
        serde_json::json!({
            "from": 1_700_000_000_i64,
            "metrics": ["system.cpu.user", "system.cpu.idle"]
        })
    }

    // ── run_list ───────────────────────────────────────────────────

    #[tokio::test]
    async fn run_list_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/metrics"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(&client, None, None, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_list_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/metrics"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body()))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(&client, None, None, &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_list_passes_host_and_from() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/metrics"))
            .and(wiremock::matchers::query_param("host", "web-01"))
            .and(wiremock::matchers::query_param("from", "1700000000"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(
            &client,
            Some("web-01"),
            Some(1_700_000_000),
            &OutputFormat::Table,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/metrics"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = run_list(&client, None, None, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    // ── ListCommand::execute glue ──────────────────────────────────
    //
    // Tests inject a wiremock-backed client into `execute`, covering the
    // execute-level argument-forwarding glue without touching credentials or
    // the environment. (Credential resolution itself is covered by the
    // `load_credentials_with` / `create_client_from` tests.)

    #[tokio::test]
    async fn execute_forwards_host_from_and_output() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/metrics"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(body()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let cmd = ListCommand {
            host: None,
            from: None,
            output: OutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();
    }
}
