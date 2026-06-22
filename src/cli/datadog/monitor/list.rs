//! CLI command for `omni-dev datadog monitor list`.

use anyhow::Result;
use clap::Parser;

use crate::cli::datadog::format::{output_as, OutputFormat};
use crate::cli::datadog::helpers::create_client;
use crate::cli::datadog::monitor::{render_monitor_table, MonitorRow};
use crate::datadog::client::DatadogClient;
use crate::datadog::monitors_api::{MonitorListFilter, MonitorsApi};
use crate::datadog::types::Monitor;

/// Lists Datadog monitors.
#[derive(Parser)]
pub struct ListCommand {
    /// Filter by substring match on monitor name.
    #[arg(long)]
    pub name: Option<String>,

    /// Filter by monitor `tags` (comma-separated `key:value` pairs).
    #[arg(long)]
    pub tags: Option<String>,

    /// Filter by `monitor_tags` (comma-separated `key:value` pairs).
    #[arg(long = "monitor-tags")]
    pub monitor_tags: Option<String>,

    /// Maximum number of monitors to return.
    ///
    /// `0` = fetch all pages, capped at 10,000 monitors per invocation.
    #[arg(long, default_value_t = 100)]
    pub limit: usize,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Executes the list against a freshly-created Datadog client.
    pub async fn execute(self) -> Result<()> {
        let (client, _site) = create_client()?;
        self.execute_with(&client).await
    }

    /// Runs the command against an injected client — the value-in seam used by
    /// tests (wiremock) so the execute-level glue is covered without touching
    /// credentials or the environment (issue #1030).
    async fn execute_with(self, client: &DatadogClient) -> Result<()> {
        let filter = MonitorListFilter {
            name: self.name,
            tags: self.tags,
            monitor_tags: self.monitor_tags,
        };
        run_list(client, &filter, self.limit, &self.output).await
    }
}

/// Fetches the list and emits it in the requested format.
///
/// Split from [`ListCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path.
async fn run_list(
    client: &DatadogClient,
    filter: &MonitorListFilter,
    limit: usize,
    output: &OutputFormat,
) -> Result<()> {
    let monitors = MonitorsApi::new(client).list(filter, limit).await?;
    if output_as(&monitors, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let rows: Vec<MonitorRow<'_>> = monitors.iter().map(monitor_row).collect();
    render_monitor_table(&rows, &mut handle)
}

fn monitor_row(m: &Monitor) -> MonitorRow<'_> {
    MonitorRow {
        id: m.id,
        name: m.name.as_str(),
        status: m.status(),
        tags: &m.tags,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn monitor_json(id: i64) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "name": format!("Monitor {id}"),
            "type": "metric alert",
            "query": "avg(last_5m):avg:system.cpu.user{*} > 90",
            "tags": ["team:sre"],
            "overall_state": "OK"
        })
    }

    #[test]
    fn monitor_row_uses_dash_when_state_missing() {
        let m: Monitor = serde_json::from_value(serde_json::json!({
            "id": 1_i64,
            "name": "n",
            "type": "metric alert",
            "query": "q"
        }))
        .unwrap();
        let row = monitor_row(&m);
        assert_eq!(row.id, 1);
        assert_eq!(row.status, "-");
    }

    // ── run_list ───────────────────────────────────────────────────

    #[tokio::test]
    async fn run_list_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor"))
            .and(wiremock::matchers::query_param("page", "0"))
            .and(wiremock::matchers::query_param("page_size", "5"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([monitor_json(1)])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(
            &client,
            &MonitorListFilter::default(),
            5,
            &OutputFormat::Table,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_list_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([monitor_json(1), monitor_json(2)])),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(
            &client,
            &MonitorListFilter::default(),
            5,
            &OutputFormat::Json,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = run_list(
            &client,
            &MonitorListFilter::default(),
            5,
            &OutputFormat::Table,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    // ── ListCommand::execute_with glue ─────────────────────────────
    //
    // Tests inject a wiremock-backed client into `execute_with`, covering the
    // execute-level filter-construction glue without touching credentials or
    // the environment. (Credential resolution itself is covered by the
    // `load_credentials_with` / `create_client_from` tests.)

    #[tokio::test]
    async fn execute_with_passes_name_filter_through() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor"))
            .and(wiremock::matchers::query_param("name", "cpu"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([monitor_json(1), monitor_json(2)])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let cmd = ListCommand {
            name: Some("cpu".into()),
            tags: None,
            monitor_tags: None,
            limit: 5,
            output: OutputFormat::Json,
        };
        cmd.execute_with(&client).await.unwrap();
    }
}
