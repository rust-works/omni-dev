//! CLI command for `omni-dev datadog hosts list`.

use anyhow::Result;
use chrono::{DateTime, SecondsFormat, Utc};
use clap::Parser;

use crate::cli::datadog::format::{output_as, OutputFormat};
use crate::cli::datadog::helpers::create_client;
use crate::cli::datadog::hosts::{render_host_table, HostRow};
use crate::datadog::client::DatadogClient;
use crate::datadog::hosts_api::{HostsApi, HostsListFilter};
use crate::datadog::types::{Host, HostsResponse};

/// Lists Datadog reporting hosts.
#[derive(Parser)]
pub struct ListCommand {
    /// Datadog hosts filter (e.g. `env:prod`).
    #[arg(long)]
    pub filter: Option<String>,

    /// Cutoff (Unix epoch seconds); hosts last reporting before this are
    /// excluded.
    #[arg(long)]
    pub from: Option<i64>,

    /// Maximum number of hosts to return; `0` = fetch all (capped at 10000).
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
        let filter = HostsListFilter {
            filter: self.filter,
            from: self.from,
            ..HostsListFilter::default()
        };
        run_list(&client, &filter, self.limit, &self.output).await
    }
}

async fn run_list(
    client: &DatadogClient,
    filter: &HostsListFilter,
    limit: usize,
    output: &OutputFormat,
) -> Result<()> {
    let result: HostsResponse = HostsApi::new(client).list(filter, limit).await?;
    if output_as(&result, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let last_reported_strings: Vec<String> = result
        .host_list
        .iter()
        .map(|h| {
            h.last_reported_time
                .map_or_else(|| "-".to_string(), epoch_to_rfc3339)
        })
        .collect();
    let rows: Vec<HostRow<'_>> = result
        .host_list
        .iter()
        .enumerate()
        .map(|(i, h)| host_row(h, &last_reported_strings[i]))
        .collect();
    render_host_table(&rows, &mut handle)
}

fn host_row<'a>(h: &'a Host, last_reported: &'a str) -> HostRow<'a> {
    HostRow {
        name: h.name.as_str(),
        up: h.up_label(),
        last_reported,
        apps: &h.apps,
    }
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

    fn host_json(name: &str, up: bool) -> serde_json::Value {
        serde_json::json!({
            "name": name,
            "up": up,
            "last_reported_time": 1_700_000_000_i64,
            "apps": ["nginx"]
        })
    }

    // ── helpers ────────────────────────────────────────────────────

    #[test]
    fn epoch_to_rfc3339_uses_z_suffix() {
        assert_eq!(epoch_to_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn epoch_to_rfc3339_falls_back_on_out_of_range() {
        assert_eq!(epoch_to_rfc3339(i64::MAX), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn host_row_uses_borrowed_fields_and_up_label() {
        let h: Host = serde_json::from_value(serde_json::json!({"name": "web-01"})).unwrap();
        let row = host_row(&h, "-");
        assert_eq!(row.name, "web-01");
        assert_eq!(row.up, "-");
        assert_eq!(row.last_reported, "-");
    }

    // ── run_list ───────────────────────────────────────────────────

    #[tokio::test]
    async fn run_list_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/hosts"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "host_list": [host_json("web-01", true)]
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(
            &client,
            &HostsListFilter::default(),
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
            .and(wiremock::matchers::path("/api/v1/hosts"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "host_list": [host_json("web-01", true), host_json("web-02", false)]
                })),
            )
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(&client, &HostsListFilter::default(), 5, &OutputFormat::Json)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn run_list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v1/hosts"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = run_list(
            &client,
            &HostsListFilter::default(),
            5,
            &OutputFormat::Table,
        )
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
            filter: None,
            from: None,
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
            .and(wiremock::matchers::path("/api/v1/hosts"))
            .and(wiremock::matchers::query_param("filter", "env:prod"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "host_list": [host_json("web-01", true)]
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
            filter: Some("env:prod".into()),
            from: None,
            limit: 5,
            output: OutputFormat::Json,
        };
        cmd.execute().await.unwrap();
    }
}
