//! CLI command for `omni-dev datadog events list`.

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::Parser;

use crate::cli::datadog::events::{render_event_table, EventRow};
use crate::cli::datadog::format::{output_as, OutputFormat};
use crate::cli::datadog::helpers::create_client;
use crate::datadog::client::DatadogClient;
use crate::datadog::events_api::{EventsApi, EventsListFilter};
use crate::datadog::time::parse_time_range;
use crate::datadog::types::{Event, EventsResponse};

/// Lists Datadog events.
#[derive(Parser)]
pub struct ListCommand {
    /// Datadog events search filter (e.g. `service:api`).
    #[arg(long)]
    pub filter: Option<String>,

    /// Start of the time range (relative shorthand like `1h`, RFC 3339,
    /// `now`, or Unix epoch seconds).
    #[arg(long, default_value = "1h")]
    pub from: String,

    /// End of the time range; defaults to `now`.
    #[arg(long, default_value = "now")]
    pub to: String,

    /// Maximum events to return. Pass `0` to fetch every match across
    /// pages (capped at 10000); any non-zero value caps the total at
    /// that count, paginating underneath as needed.
    #[arg(long, default_value_t = 100)]
    pub limit: usize,

    /// Comma-separated list of source names (e.g. `aws,kubernetes`).
    #[arg(long)]
    pub sources: Option<String>,

    /// Comma-separated list of `key:value` tags applied to the event.
    #[arg(long)]
    pub tags: Option<String>,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Executes the list against a freshly-created Datadog client.
    pub async fn execute(self) -> Result<()> {
        let (client, _site) = create_client()?;
        let (from_str, to_str) = resolve_time_range(&self.from, &self.to)?;
        let filter = EventsListFilter {
            query: self.filter.clone(),
            sources: self.sources.clone(),
            tags: self.tags.clone(),
        };
        run_list(
            &client,
            &filter,
            &from_str,
            &to_str,
            self.limit,
            &self.output,
        )
        .await
    }
}

/// Parses CLI `--from` / `--to` strings into RFC 3339 timestamps suitable
/// for the Datadog v2 events query parameters.
fn resolve_time_range(from: &str, to: &str) -> Result<(String, String)> {
    let (from_secs, to_secs) =
        parse_time_range(from, Some(to)).context("Failed to parse --from / --to")?;
    Ok((epoch_to_rfc3339(from_secs), epoch_to_rfc3339(to_secs)))
}

fn epoch_to_rfc3339(seconds: i64) -> String {
    DateTime::<Utc>::from_timestamp(seconds, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Fetches the events list and emits it in the requested format.
async fn run_list(
    client: &DatadogClient,
    filter: &EventsListFilter,
    from: &str,
    to: &str,
    limit: usize,
    output: &OutputFormat,
) -> Result<()> {
    let result: EventsResponse = EventsApi::new(client)
        .list_all(filter, from, to, limit)
        .await?;
    if output_as(&result, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let rows: Vec<EventRow<'_>> = result.data.iter().map(event_row).collect();
    render_event_table(&rows, &mut handle)
}

fn event_row(e: &Event) -> EventRow<'_> {
    EventRow {
        timestamp: e.timestamp_label(),
        title: e.title_label(),
        source: e.source_label(),
        host: e.host_label(),
        tags: &e.attributes.tags,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::datadog::types::EventAttributes;

    fn events_body() -> serde_json::Value {
        serde_json::json!({
            "data": [
                {
                    "id": "EV1",
                    "type": "event",
                    "attributes": {
                        "timestamp": "2026-04-22T10:00:00.000Z",
                        "title": "Deploy",
                        "source": "github",
                        "host": "web-01",
                        "tags": ["env:prod"]
                    }
                }
            ]
        })
    }

    #[test]
    fn event_row_falls_back_to_dashes_when_attributes_missing() {
        let e = Event {
            id: "x".into(),
            event_type: None,
            attributes: EventAttributes::default(),
        };
        let row = event_row(&e);
        assert_eq!(row.timestamp, "-");
        assert_eq!(row.title, "-");
        assert_eq!(row.source, "-");
        assert_eq!(row.host, "-");
        assert!(row.tags.is_empty());
    }

    #[test]
    fn epoch_to_rfc3339_uses_z_suffix() {
        assert_eq!(epoch_to_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn epoch_to_rfc3339_falls_back_to_unix_epoch_on_out_of_range() {
        assert_eq!(epoch_to_rfc3339(i64::MAX), "1970-01-01T00:00:00Z");
        assert_eq!(epoch_to_rfc3339(i64::MIN), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn resolve_time_range_emits_rfc3339_strings() {
        let (from, to) =
            resolve_time_range("2026-04-22T09:00:00Z", "2026-04-22T10:00:00Z").unwrap();
        assert_eq!(from, "2026-04-22T09:00:00Z");
        assert_eq!(to, "2026-04-22T10:00:00Z");
    }

    #[test]
    fn resolve_time_range_propagates_parse_errors() {
        let err = resolve_time_range("garbage", "now").unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    // ── run_list ───────────────────────────────────────────────────

    #[tokio::test]
    async fn run_list_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v2/events"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(events_body()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(
            &client,
            &EventsListFilter::default(),
            "2026-04-22T09:00:00Z",
            "2026-04-22T10:00:00Z",
            10,
            &OutputFormat::Table,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_list_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v2/events"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(events_body()))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(
            &client,
            &EventsListFilter::default(),
            "2026-04-22T09:00:00Z",
            "2026-04-22T10:00:00Z",
            10,
            &OutputFormat::Json,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_list_with_zero_limit_auto_paginates_until_no_cursor() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v2/events"))
            .and(wiremock::matchers::query_param("page[limit]", "1000"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": [],
                    "meta": { "page": {} }
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_list(
            &client,
            &EventsListFilter::default(),
            "2026-04-22T09:00:00Z",
            "2026-04-22T10:00:00Z",
            0,
            &OutputFormat::Json,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_list_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v2/events"))
            .respond_with(wiremock::ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = run_list(
            &client,
            &EventsListFilter::default(),
            "2026-04-22T09:00:00Z",
            "2026-04-22T10:00:00Z",
            10,
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
            from: "1h".into(),
            to: "now".into(),
            limit: 10,
            sources: None,
            tags: None,
            output: OutputFormat::Table,
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(
            err.to_string().contains("Failed to parse")
                || err.to_string().contains("not configured")
        );
    }

    #[tokio::test]
    async fn list_command_execute_propagates_time_range_parse_errors() {
        use std::fs;

        use crate::datadog::auth::{DATADOG_API_KEY, DATADOG_APP_KEY};
        use crate::datadog::test_support::{with_empty_home, EnvGuard};

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

        let cmd = ListCommand {
            filter: None,
            from: "garbage-time".into(),
            to: "now".into(),
            limit: 10,
            sources: None,
            tags: None,
            output: OutputFormat::Table,
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }

    #[tokio::test]
    async fn list_command_execute_end_to_end_via_api_url_override() {
        use std::fs;

        use crate::datadog::auth::{DATADOG_API_KEY, DATADOG_API_URL, DATADOG_APP_KEY};
        use crate::datadog::test_support::{with_empty_home, EnvGuard};

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/api/v2/events"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(events_body()))
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
            filter: Some("service:api".into()),
            from: "2026-04-22T09:00:00Z".into(),
            to: "2026-04-22T10:00:00Z".into(),
            limit: 10,
            sources: None,
            tags: None,
            output: OutputFormat::Json,
        };
        cmd.execute().await.unwrap();
    }
}
