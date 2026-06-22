//! CLI command for `omni-dev datadog dashboard get`.

use std::io::Write;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::datadog::dashboard::{render_dashboard_table, DashboardRow};
use crate::cli::datadog::format::{output_as, OutputFormat};
use crate::datadog::client::DatadogClient;
use crate::datadog::dashboards_api::DashboardsApi;
use crate::datadog::types::Dashboard;

/// Fetches a single Datadog dashboard by id.
#[derive(Parser)]
pub struct GetCommand {
    /// Datadog dashboard identifier.
    pub id: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl GetCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `DatadogCommand::execute`. Taking the client as a parameter (rather than
    /// calling `create_client` here) keeps this entry point free of process
    /// env and fully testable with a wiremock client (issue #1030).
    pub async fn execute(self, client: &DatadogClient) -> Result<()> {
        run_get(client, &self.id, &self.output).await
    }
}

/// Fetches the dashboard and emits it in the requested format.
///
/// Split from [`GetCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path.
async fn run_get(client: &DatadogClient, id: &str, output: &OutputFormat) -> Result<()> {
    let dashboard = DashboardsApi::new(client).get(id).await?;
    if output_as(&dashboard, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_get_table(&dashboard, &mut handle)
}

/// Renders a single dashboard as the bespoke `ID | TITLE | AUTHOR | URL`
/// table. When the dashboard carries a description, it's appended on a
/// final line so the table view of `get` carries one extra piece of
/// detail beyond what `list` shows.
pub(crate) fn render_get_table(dashboard: &Dashboard, out: &mut dyn Write) -> Result<()> {
    let row = DashboardRow {
        id: dashboard.id.as_str(),
        title: dashboard.title.as_str(),
        author: dashboard.author_label(),
        url: dashboard.url_label(),
    };
    render_dashboard_table(std::slice::from_ref(&row), out)?;
    if let Some(desc) = dashboard.description.as_deref() {
        if !desc.is_empty() {
            writeln!(out, "DESCRIPTION: {desc}").context("Failed to write description line")?;
        }
    }
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

    fn dashboard_json() -> serde_json::Value {
        serde_json::json!({
            "id": "abc-def-ghi",
            "title": "Service Overview",
            "description": "Top-level service health.",
            "url": "/dashboard/abc-def-ghi",
            "author_handle": "alice@example.com",
            "layout_type": "ordered",
            "widgets": []
        })
    }

    // ── render_get_table ───────────────────────────────────────────

    #[test]
    fn render_get_table_includes_row_and_description() {
        let d: Dashboard = serde_json::from_value(dashboard_json()).unwrap();
        let mut buf = Vec::new();
        render_get_table(&d, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("ID"));
        assert!(out.contains("abc-def-ghi"));
        assert!(out.contains("Service Overview"));
        assert!(out.contains("alice@example.com"));
        assert!(out.contains("/dashboard/abc-def-ghi"));
        assert!(out.contains("DESCRIPTION: Top-level service health."));
    }

    #[test]
    fn render_get_table_skips_description_when_missing() {
        let d: Dashboard = serde_json::from_value(serde_json::json!({
            "id": "x",
            "title": "y"
        }))
        .unwrap();
        let mut buf = Vec::new();
        render_get_table(&d, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(!out.contains("DESCRIPTION"));
    }

    #[test]
    fn render_get_table_skips_description_when_empty_string() {
        let d: Dashboard = serde_json::from_value(serde_json::json!({
            "id": "x",
            "title": "y",
            "description": ""
        }))
        .unwrap();
        let mut buf = Vec::new();
        render_get_table(&d, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(!out.contains("DESCRIPTION"));
    }

    #[test]
    fn render_get_table_propagates_table_write_errors() {
        let d: Dashboard = serde_json::from_value(dashboard_json()).unwrap();
        let err = render_get_table(&d, &mut FailAfter::new(0)).unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn render_get_table_propagates_description_line_write_errors() {
        // Header + separator + data row succeed; the DESCRIPTION line fails.
        let d: Dashboard = serde_json::from_value(dashboard_json()).unwrap();
        let err = render_get_table(&d, &mut FailAfter::new(3)).unwrap_err();
        assert!(err.to_string().contains("Failed to write description line"));
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
            .and(wiremock::matchers::path("/api/v1/dashboard/abc-def-ghi"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(dashboard_json()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_get(&client, "abc-def-ghi", &OutputFormat::Table)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_get_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/dashboard/x"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(dashboard_json()))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_get(&client, "x", &OutputFormat::Json).await.unwrap();
    }

    #[tokio::test]
    async fn execute_runs_against_injected_client() {
        // Covers GetCommand::execute (the execute-level glue) without going
        // through credential loading.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/dashboard/x"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(dashboard_json()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let cmd = GetCommand {
            id: "x".to_string(),
            output: OutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();
    }

    #[tokio::test]
    async fn run_get_propagates_404() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/dashboard/missing"))
            .respond_with(wiremock::ResponseTemplate::new(404).set_body_string("not found"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = run_get(&client, "missing", &OutputFormat::Table)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("404"));
    }
}
