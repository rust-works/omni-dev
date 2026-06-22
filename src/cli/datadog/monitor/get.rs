//! CLI command for `omni-dev datadog monitor get`.

use std::io::Write;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::datadog::format::{output_as, OutputFormat};
use crate::cli::datadog::helpers::create_client;
use crate::cli::datadog::monitor::{render_monitor_table, MonitorRow};
use crate::datadog::client::DatadogClient;
use crate::datadog::monitors_api::MonitorsApi;
use crate::datadog::types::Monitor;

/// Fetches a single Datadog monitor by id.
#[derive(Parser)]
pub struct GetCommand {
    /// Numeric monitor identifier.
    pub id: i64,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl GetCommand {
    /// Executes the fetch against a freshly-created Datadog client.
    pub async fn execute(self) -> Result<()> {
        let (client, _site) = create_client()?;
        self.execute_with(&client).await
    }

    /// Runs the command against an injected client — the value-in seam used by
    /// tests (wiremock) so the execute-level glue is covered without touching
    /// credentials or the environment (issue #1030).
    async fn execute_with(self, client: &DatadogClient) -> Result<()> {
        run_get(client, self.id, &self.output).await
    }
}

/// Fetches the monitor and emits it in the requested format.
///
/// Split from [`GetCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path.
async fn run_get(client: &DatadogClient, id: i64, output: &OutputFormat) -> Result<()> {
    let monitor = MonitorsApi::new(client).get(id).await?;
    if output_as(&monitor, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_get_table(&monitor, &mut handle)
}

/// Renders a single monitor as the bespoke `ID | NAME | STATUS | TAGS`
/// table followed by the full query expression on a final line, so the
/// table view of `get` carries one extra piece of detail beyond what
/// `list` shows.
pub(crate) fn render_get_table(monitor: &Monitor, out: &mut dyn Write) -> Result<()> {
    let row = MonitorRow {
        id: monitor.id,
        name: monitor.name.as_str(),
        status: monitor.status(),
        tags: &monitor.tags,
    };
    render_monitor_table(std::slice::from_ref(&row), out)?;
    writeln!(out, "QUERY: {}", monitor.query).context("Failed to write query line")?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// `Write` impl that succeeds for the first `n` line-terminated rows
    /// and then fails. Used to exercise each `?`-propagation site in
    /// [`render_get_table`] independently.
    struct FailAfter {
        successes_remaining: usize,
        sink: Vec<u8>,
    }

    impl FailAfter {
        fn new(successes_remaining: usize) -> Self {
            Self {
                successes_remaining,
                sink: Vec::new(),
            }
        }
    }

    impl Write for FailAfter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.successes_remaining == 0 {
                return Err(std::io::Error::other("test forced write failure"));
            }
            self.sink.extend_from_slice(buf);
            if buf.contains(&b'\n') {
                self.successes_remaining -= 1;
            }
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn monitor_json() -> serde_json::Value {
        serde_json::json!({
            "id": 12345_i64,
            "name": "Disk full",
            "type": "metric alert",
            "query": "avg(last_5m):avg:system.disk.in_use{*} > 0.9",
            "tags": ["team:sre"],
            "overall_state": "Alert"
        })
    }

    // ── render_get_table ───────────────────────────────────────────

    #[test]
    fn render_get_table_includes_row_and_query_line() {
        let m: Monitor = serde_json::from_value(monitor_json()).unwrap();
        let mut buf = Vec::new();
        render_get_table(&m, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("ID"));
        assert!(out.contains("12345"));
        assert!(out.contains("Disk full"));
        assert!(out.contains("Alert"));
        assert!(out.contains("team:sre"));
        assert!(out.contains("QUERY: avg(last_5m):avg:system.disk.in_use{*} > 0.9"));
    }

    #[test]
    fn render_get_table_propagates_table_write_errors() {
        // Fails on the *first* write — the table header — so the renderer
        // surfaces the failure before we get to the QUERY line.
        let m: Monitor = serde_json::from_value(monitor_json()).unwrap();
        let err = render_get_table(&m, &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_get_table_propagates_query_line_write_errors() {
        // Header + separator + data row succeed; the QUERY line fails.
        let m: Monitor = serde_json::from_value(monitor_json()).unwrap();
        let err = render_get_table(&m, &mut FailAfter::new(3)).unwrap_err();
        assert!(err.to_string().contains("Failed to write query line"));
    }

    #[test]
    fn fail_after_flush_is_a_noop() {
        let mut w = FailAfter::new(0);
        w.flush().unwrap();
    }

    // ── run_get ────────────────────────────────────────────────────

    #[tokio::test]
    async fn run_get_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor/12345"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(monitor_json()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_get(&client, 12345, &OutputFormat::Table).await.unwrap();
    }

    #[tokio::test]
    async fn run_get_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor/1"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(monitor_json()))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_get(&client, 1, &OutputFormat::Json).await.unwrap();
    }

    #[tokio::test]
    async fn run_get_propagates_404() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor/9"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = run_get(&client, 9, &OutputFormat::Table).await.unwrap_err();
        assert!(err.to_string().contains("404"));
    }

    // ── GetCommand::execute_with glue ──────────────────────────────
    //
    // Tests inject a wiremock-backed client into `execute_with`, covering the
    // execute-level glue without touching credentials or the environment.
    // (Credential resolution itself is covered by the `load_credentials_with`
    // / `create_client_from` tests.)

    #[tokio::test]
    async fn execute_with_threads_id_and_output_through() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/monitor/123"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(monitor_json()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let cmd = GetCommand {
            id: 123,
            output: OutputFormat::Json,
        };
        cmd.execute_with(&client).await.unwrap();
    }
}
