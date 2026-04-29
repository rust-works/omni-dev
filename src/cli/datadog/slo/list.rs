//! CLI command for `omni-dev datadog slo list`.

use anyhow::Result;
use clap::Parser;

use crate::cli::datadog::format::{output_as, OutputFormat};
use crate::cli::datadog::helpers::create_client;
use crate::cli::datadog::slo::{render_slo_table, SloRow};
use crate::datadog::client::DatadogClient;
use crate::datadog::slo_api::{SloApi, SloListFilter};
use crate::datadog::types::Slo;

/// Lists Datadog Service Level Objectives.
#[derive(Parser)]
pub struct ListCommand {
    /// Comma-separated `key:value` tags applied to the SLO.
    #[arg(long)]
    pub tags: Option<String>,

    /// Free-text query; matches SLO names / metrics.
    #[arg(long)]
    pub query: Option<String>,

    /// Comma-separated list of SLO ids.
    #[arg(long)]
    pub ids: Option<String>,

    /// Comma-separated list of metric names referenced by the SLO.
    #[arg(long = "metrics-query")]
    pub metrics_query: Option<String>,

    /// Maximum number of SLOs to return; `0` = fetch all (capped at 10000).
    #[arg(long, default_value_t = 50)]
    pub limit: usize,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Executes the list against a freshly-created Datadog client.
    pub async fn execute(self) -> Result<()> {
        let (client, _site) = create_client()?;
        let filter = SloListFilter {
            tags: self.tags,
            query: self.query,
            ids: self.ids,
            metrics: self.metrics_query,
        };
        run_list(&client, &filter, self.limit, &self.output).await
    }
}

async fn run_list(
    client: &DatadogClient,
    filter: &SloListFilter,
    limit: usize,
    output: &OutputFormat,
) -> Result<()> {
    let slos = SloApi::new(client).list(filter, limit).await?;
    if output_as(&slos, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let rows: Vec<SloRow<'_>> = slos.iter().map(slo_row).collect();
    render_slo_table(&rows, &mut handle)
}

fn slo_row(s: &Slo) -> SloRow<'_> {
    SloRow {
        id: s.id.as_str(),
        name: s.name.as_str(),
        slo_type: s.slo_type.as_str(),
        tags: &s.tags,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn slo_json(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "name": format!("SLO {id}"),
            "type": "metric",
            "tags": ["team:sre"],
            "monitor_ids": []
        })
    }

    #[test]
    fn slo_row_uses_borrowed_fields() {
        let s: Slo = serde_json::from_value(slo_json("abc")).unwrap();
        let row = slo_row(&s);
        assert_eq!(row.id, "abc");
        assert_eq!(row.slo_type, "metric");
    }

    // ── run_list ───────────────────────────────────────────────────

    #[tokio::test]
    async fn run_list_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo"))
            .and(wiremock::matchers::query_param("limit", "5"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": [slo_json("abc")]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(&client, &SloListFilter::default(), 5, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_list_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": [slo_json("a"), slo_json("b")]
                })),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(&client, &SloListFilter::default(), 5, &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = run_list(&client, &SloListFilter::default(), 5, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    // ── ListCommand::execute error paths ───────────────────────────

    #[tokio::test]
    async fn list_command_execute_errors_when_credentials_missing() {
        use crate::datadog::test_support::{with_empty_home, EnvGuard};
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);

        let cmd = ListCommand {
            tags: None,
            query: None,
            ids: None,
            metrics_query: None,
            limit: 5,
            output: OutputFormat::Table,
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[tokio::test]
    async fn list_command_execute_end_to_end_via_api_url_override() {
        use std::fs;

        use crate::datadog::auth::{DATADOG_API_KEY, DATADOG_API_URL, DATADOG_APP_KEY};
        use crate::datadog::test_support::{with_empty_home, EnvGuard};

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/slo"))
            .and(wiremock::matchers::query_param("tags_query", "team:sre"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": [slo_json("abc"), slo_json("def")]
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

        let cmd = ListCommand {
            tags: Some("team:sre".into()),
            query: None,
            ids: None,
            metrics_query: None,
            limit: 5,
            output: OutputFormat::Json,
        };
        cmd.execute().await.unwrap();
    }
}
