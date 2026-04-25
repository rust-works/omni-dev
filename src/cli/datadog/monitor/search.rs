//! CLI command for `omni-dev datadog monitor search`.

use anyhow::Result;
use clap::Parser;

use crate::cli::datadog::format::{output_as, OutputFormat};
use crate::cli::datadog::helpers::create_client;
use crate::cli::datadog::monitor::{render_monitor_table, MonitorRow};
use crate::datadog::client::DatadogClient;
use crate::datadog::monitors_api::MonitorsApi;
use crate::datadog::types::MonitorSearchItem;

/// Searches Datadog monitors by free-text / faceted query.
#[derive(Parser)]
pub struct SearchCommand {
    /// Search query (e.g. `status:alert AND env:prod`).
    #[arg(long)]
    pub query: String,

    /// Maximum number of monitors to return.
    ///
    /// `0` = fetch all pages, capped at 10,000 monitors per invocation.
    #[arg(long, default_value_t = 100)]
    pub limit: usize,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl SearchCommand {
    /// Executes the search against a freshly-created Datadog client.
    pub async fn execute(self) -> Result<()> {
        let (client, _site) = create_client()?;
        run_search(&client, &self.query, self.limit, &self.output).await
    }
}

/// Fetches the search response and emits it in the requested format.
///
/// Split from [`SearchCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path.
async fn run_search(
    client: &DatadogClient,
    query: &str,
    limit: usize,
    output: &OutputFormat,
) -> Result<()> {
    let result = MonitorsApi::new(client).search(query, limit).await?;
    if output_as(&result, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let rows: Vec<MonitorRow<'_>> = result.monitors.iter().map(search_row).collect();
    render_monitor_table(&rows, &mut handle)
}

fn search_row(item: &MonitorSearchItem) -> MonitorRow<'_> {
    MonitorRow {
        id: item.id,
        name: item.name.as_str(),
        status: item.status_label(),
        tags: &item.tags,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn search_body() -> serde_json::Value {
        serde_json::json!({
            "monitors": [
                {
                    "id": 1_i64,
                    "name": "Disk full",
                    "status": "ALERT",
                    "tags": ["team:sre"]
                }
            ],
            "metadata": {
                "page": 0,
                "per_page": 30,
                "page_count": 1,
                "total_count": 1
            }
        })
    }

    #[test]
    fn search_row_uses_dash_when_status_missing() {
        let item = MonitorSearchItem {
            id: 1,
            name: "n".into(),
            status: None,
            tags: vec![],
            monitor_type: None,
            query: None,
            last_triggered_ts: None,
            creator: None,
        };
        let row = search_row(&item);
        assert_eq!(row.id, 1);
        assert_eq!(row.status, "-");
    }

    // ── run_search ─────────────────────────────────────────────────

    #[tokio::test]
    async fn run_search_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor/search"))
            .and(wiremock::matchers::query_param("query", "status:alert"))
            .and(wiremock::matchers::query_param("page", "0"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(search_body()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_search(&client, "status:alert", 30, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_search_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor/search"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(search_body()))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_search(&client, "q", 30, &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_search_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor/search"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("bad"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = run_search(&client, "??", 30, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    // ── SearchCommand::execute error paths ─────────────────────────

    #[tokio::test]
    async fn search_command_execute_errors_when_credentials_missing() {
        use crate::datadog::test_support::{with_empty_home, EnvGuard};
        let guard = EnvGuard::take();
        let _dir = with_empty_home(&guard);

        let cmd = SearchCommand {
            query: "q".into(),
            limit: 10,
            output: OutputFormat::Table,
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("not configured"));
    }

    #[tokio::test]
    async fn search_command_execute_end_to_end_via_api_url_override() {
        use std::fs;

        use crate::datadog::auth::{DATADOG_API_KEY, DATADOG_API_URL, DATADOG_APP_KEY};
        use crate::datadog::test_support::{with_empty_home, EnvGuard};

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor/search"))
            .and(wiremock::matchers::query_param("query", "status:alert"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(search_body()))
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

        let cmd = SearchCommand {
            query: "status:alert".into(),
            limit: 30,
            output: OutputFormat::Json,
        };
        cmd.execute().await.unwrap();
    }
}
