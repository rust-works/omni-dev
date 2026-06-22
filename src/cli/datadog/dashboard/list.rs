//! CLI command for `omni-dev datadog dashboard list`.

use anyhow::Result;
use clap::Parser;

use crate::cli::datadog::dashboard::{render_dashboard_table, DashboardRow};
use crate::cli::datadog::format::{output_as, OutputFormat};
use crate::datadog::client::DatadogClient;
use crate::datadog::dashboards_api::{DashboardListFilter, DashboardsApi};
use crate::datadog::types::DashboardSummary;

/// Lists Datadog dashboards.
///
/// Note: `GET /api/v1/dashboard` returns every dashboard in a single
/// response, so there is no `--limit` / pagination flag here. If a
/// caller needs truncation it should pipe through a downstream tool.
#[derive(Parser)]
pub struct ListCommand {
    /// Restricts results to dashboards shared with the wider organisation.
    #[arg(long = "filter-shared")]
    pub filter_shared: bool,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `DatadogCommand::execute`. Taking the client as a parameter keeps this
    /// entry point free of process env and fully testable (issue #1030).
    pub async fn execute(self, client: &DatadogClient) -> Result<()> {
        let filter = DashboardListFilter {
            filter_shared: if self.filter_shared { Some(true) } else { None },
        };
        run_list(client, &filter, &self.output).await
    }
}

/// Fetches the list and emits it in the requested format.
///
/// Split from [`ListCommand::execute`] so tests can inject a wiremock
/// client without going through the credential-loading path.
async fn run_list(
    client: &DatadogClient,
    filter: &DashboardListFilter,
    output: &OutputFormat,
) -> Result<()> {
    let dashboards = DashboardsApi::new(client).list(filter).await?;
    if output_as(&dashboards, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let rows: Vec<DashboardRow<'_>> = dashboards.iter().map(dashboard_row).collect();
    render_dashboard_table(&rows, &mut handle)
}

fn dashboard_row(d: &DashboardSummary) -> DashboardRow<'_> {
    DashboardRow {
        id: d.id.as_str(),
        title: d.title.as_str(),
        author: d.author_label(),
        url: d.url_label(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn dashboard_json(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "title": format!("Dashboard {id}"),
            "author_handle": "alice@example.com",
            "url": format!("/dashboard/{id}"),
            "is_shared": true
        })
    }

    #[test]
    fn dashboard_row_falls_back_to_dash_when_optional_fields_missing() {
        let s: DashboardSummary = serde_json::from_value(serde_json::json!({
            "id": "x",
            "title": "y"
        }))
        .unwrap();
        let row = dashboard_row(&s);
        assert_eq!(row.author, "-");
        assert_eq!(row.url, "-");
    }

    // ── run_list ───────────────────────────────────────────────────

    #[tokio::test]
    async fn run_list_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/dashboard"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "dashboards": [dashboard_json("abc")]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(
            &client,
            &DashboardListFilter::default(),
            &OutputFormat::Table,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_list_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/dashboard"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "dashboards": [dashboard_json("abc"), dashboard_json("def")]
                })),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(
            &client,
            &DashboardListFilter::default(),
            &OutputFormat::Json,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_list_passes_filter_shared_flag_through() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/dashboard"))
            .and(wiremock::matchers::query_param("filter_shared", "true"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "dashboards": [dashboard_json("abc")]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(
            &client,
            &DashboardListFilter {
                filter_shared: Some(true),
            },
            &OutputFormat::Json,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/dashboard"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = run_list(
            &client,
            &DashboardListFilter::default(),
            &OutputFormat::Table,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("500"));
    }

    // ── ListCommand::execute glue ──────────────────────────────────
    //
    // Tests inject a wiremock-backed client into `execute`, covering the
    // execute-level filter-construction glue without touching credentials or
    // the environment. (Credential resolution itself is covered by the
    // `load_credentials_with` / `create_client_from` tests.)

    #[tokio::test]
    async fn execute_omits_filter_shared_when_flag_unset() {
        // Covers the `else { None }` branch of the filter-construction ternary.
        let server = wiremock::MockServer::start().await;
        // Match only when `filter_shared` is *absent* from the query string.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/dashboard"))
            .and(wiremock::matchers::query_param_is_missing("filter_shared"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"dashboards": []})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let cmd = ListCommand {
            filter_shared: false,
            output: OutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();
    }

    #[tokio::test]
    async fn execute_sets_filter_shared_when_flag_set() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/dashboard"))
            .and(wiremock::matchers::query_param("filter_shared", "true"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "dashboards": [dashboard_json("abc"), dashboard_json("def")]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let cmd = ListCommand {
            filter_shared: true,
            output: OutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();
    }
}
