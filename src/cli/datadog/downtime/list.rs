//! CLI command for `omni-dev datadog downtime list`.

use anyhow::Result;
use chrono::{DateTime, SecondsFormat, Utc};
use clap::Parser;

use crate::cli::datadog::downtime::{render_downtime_table, DowntimeRow};
use crate::cli::datadog::format::{output_as, OutputFormat};
use crate::cli::datadog::helpers::create_client;
use crate::datadog::client::DatadogClient;
use crate::datadog::downtimes_api::DowntimesApi;
use crate::datadog::types::Downtime;

/// Lists Datadog scheduled downtimes.
#[derive(Parser)]
pub struct ListCommand {
    /// Show only currently-active downtimes.
    #[arg(long = "active-only")]
    pub active_only: bool,

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
        run_list(client, self.active_only, &self.output).await
    }
}

async fn run_list(client: &DatadogClient, active_only: bool, output: &OutputFormat) -> Result<()> {
    let downtimes = DowntimesApi::new(client).list(active_only).await?;
    if output_as(&downtimes, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    // Pre-render variable-width strings owned by the loop scope.
    let scope_strings: Vec<String> = downtimes.iter().map(Downtime::scope_label).collect();
    let start_strings: Vec<String> = downtimes
        .iter()
        .map(|d| d.start.map_or_else(|| "-".to_string(), epoch_to_rfc3339))
        .collect();
    let end_strings: Vec<String> = downtimes
        .iter()
        .map(|d| d.end.map_or_else(|| "-".to_string(), epoch_to_rfc3339))
        .collect();
    let monitor_strings: Vec<String> = downtimes.iter().map(Downtime::monitor_label).collect();
    let rows: Vec<DowntimeRow<'_>> = downtimes
        .iter()
        .enumerate()
        .map(|(i, d)| DowntimeRow {
            id: d.id,
            scope: &scope_strings[i],
            start: &start_strings[i],
            end: &end_strings[i],
            monitor: &monitor_strings[i],
            message: d.message_label(),
        })
        .collect();
    render_downtime_table(&rows, &mut handle)
}

fn epoch_to_rfc3339(seconds: i64) -> String {
    DateTime::<Utc>::from_timestamp(seconds, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn downtime_json(id: i64) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "scope": ["env:prod"],
            "start": 1_700_000_000_i64,
            "end": 1_700_000_300_i64,
            "monitor_id": 99_i64,
            "message": "Maintenance",
            "active": true
        })
    }

    #[test]
    fn epoch_to_rfc3339_uses_z_suffix() {
        assert_eq!(epoch_to_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn epoch_to_rfc3339_falls_back_on_out_of_range() {
        assert_eq!(epoch_to_rfc3339(i64::MAX), "1970-01-01T00:00:00Z");
    }

    // ── run_list ───────────────────────────────────────────────────

    #[tokio::test]
    async fn run_list_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/downtime"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([downtime_json(1)])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(&client, false, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_list_table_path_handles_missing_optional_fields() {
        // Covers the `unwrap_or "-"` branches for start / end / monitor.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/downtime"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!([
                    {"id": 1_i64}
                ])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(&client, false, &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_list_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/downtime"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([downtime_json(1), downtime_json(2)])),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(&client, false, &OutputFormat::Json).await.unwrap();
    }

    #[tokio::test]
    async fn run_list_passes_active_only() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/downtime"))
            .and(wiremock::matchers::query_param("current_only", "true"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([downtime_json(1)])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(&client, true, &OutputFormat::Table).await.unwrap();
    }

    #[tokio::test]
    async fn run_list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/downtime"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = run_list(&client, false, &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    // ── ListCommand::execute_with glue ─────────────────────────────
    //
    // Tests inject a wiremock-backed client into `execute_with`, covering the
    // execute-level glue without touching credentials or the environment.
    // (Credential resolution itself is covered by the `load_credentials_with`
    // / `create_client_from` tests.)

    #[tokio::test]
    async fn execute_with_runs_against_injected_client() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/downtime"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!([downtime_json(1)])),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let cmd = ListCommand {
            active_only: false,
            output: OutputFormat::Json,
        };
        cmd.execute_with(&client).await.unwrap();
    }
}
