//! CLI command for `omni-dev datadog logs search`.

use anyhow::{Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::{Parser, ValueEnum};

use crate::cli::datadog::format::{output_as, OutputFormat};
use crate::cli::datadog::logs::{render_log_table, LogRow};
use crate::datadog::client::DatadogClient;
use crate::datadog::logs_api::LogsApi;
use crate::datadog::time::parse_time_range;
use crate::datadog::types::{LogEvent, LogSearchResult, SortOrder};

/// Sort order argument for `omni-dev datadog logs search`.
///
/// Wraps [`SortOrder`] so clap can derive a `ValueEnum` impl with the
/// kebab-case strings the CLI exposes (`timestamp-asc`,
/// `timestamp-desc`); the on-the-wire representation lives on
/// [`SortOrder::as_api_str`].
#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum SortArg {
    /// Oldest first.
    TimestampAsc,
    /// Newest first.
    TimestampDesc,
}

impl SortArg {
    /// Maps the CLI value to the API enum.
    #[must_use]
    pub fn to_sort_order(self) -> SortOrder {
        match self {
            Self::TimestampAsc => SortOrder::TimestampAsc,
            Self::TimestampDesc => SortOrder::TimestampDesc,
        }
    }
}

/// Searches Datadog log events.
#[derive(Parser)]
pub struct SearchCommand {
    /// Search filter (Datadog logs query language; see Datadog docs).
    #[arg(long)]
    pub filter: String,

    /// Start of the time range (relative shorthand like `15m`/`1h`,
    /// `now`, RFC 3339, or Unix epoch seconds).
    #[arg(long, default_value = "15m")]
    pub from: String,

    /// End of the time range; defaults to `now`.
    #[arg(long, default_value = "now")]
    pub to: String,

    /// Maximum events to return. Pass `0` to fetch every match across
    /// pages (capped at 10000); any non-zero value caps the total at
    /// that count, paginating underneath as needed.
    #[arg(long, default_value_t = 100)]
    pub limit: usize,

    /// Sort order for returned events.
    #[arg(long, value_enum, default_value_t = SortArg::TimestampDesc)]
    pub sort: SortArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl SearchCommand {
    /// Runs the command against the shared client resolved by the parent
    /// `DatadogCommand::execute`. Taking the client as a parameter keeps this
    /// entry point free of process env and fully testable (issue #1030).
    pub async fn execute(self, client: &DatadogClient) -> Result<()> {
        let (from_str, to_str) = resolve_time_range(&self.from, &self.to)?;
        run_search(
            client,
            &self.filter,
            &from_str,
            &to_str,
            self.limit,
            self.sort.to_sort_order(),
            &self.output,
        )
        .await
    }
}

/// Parses the CLI `--from` / `--to` strings and converts them into
/// RFC 3339 timestamps suitable for the Datadog v2 logs search body.
fn resolve_time_range(from: &str, to: &str) -> Result<(String, String)> {
    let (from_secs, to_secs) =
        parse_time_range(from, Some(to)).context("Failed to parse --from / --to")?;
    Ok((epoch_to_rfc3339(from_secs), epoch_to_rfc3339(to_secs)))
}

/// Renders an epoch-seconds timestamp as `YYYY-MM-DDTHH:MM:SSZ`.
///
/// Falls back to the Unix epoch when `seconds` is outside the
/// chrono-representable range. `parse_time_range` already validates
/// production inputs, so the fallback only fires for direct callers
/// that pass extreme values (e.g. `i64::MAX`).
fn epoch_to_rfc3339(seconds: i64) -> String {
    DateTime::<Utc>::from_timestamp(seconds, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Fetches the search response and emits it in the requested format.
///
/// Split from [`SearchCommand::execute`] so tests can inject a wiremock
/// client and pre-resolved time strings without going through the
/// credential-loading path.
async fn run_search(
    client: &DatadogClient,
    filter: &str,
    from: &str,
    to: &str,
    limit: usize,
    sort: SortOrder,
    output: &OutputFormat,
) -> Result<()> {
    let result: LogSearchResult = LogsApi::new(client)
        .search_all(filter, from, to, limit, sort)
        .await?;
    if output_as(&result, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    let rows: Vec<LogRow<'_>> = result.data.iter().map(log_row).collect();
    render_log_table(&rows, &mut handle)
}

fn log_row(event: &LogEvent) -> LogRow<'_> {
    LogRow {
        timestamp: event.timestamp_label(),
        service: event.service_label(),
        status: event.status_label(),
        message: event.message_label(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::datadog::types::LogEventAttributes;

    fn search_body() -> serde_json::Value {
        // No `meta.page.after` so the auto-paginating wrapper terminates
        // after a single request; tests that exercise cursor follow-up
        // live in `LogsApi::search_all` unit tests.
        serde_json::json!({
            "data": [
                {
                    "id": "AAAA",
                    "type": "log",
                    "attributes": {
                        "timestamp": "2026-04-22T10:00:00.000Z",
                        "service": "api",
                        "status": "info",
                        "message": "ok",
                        "tags": ["env:prod"]
                    }
                }
            ],
            "meta": { "page": {} }
        })
    }

    // ── log_row ────────────────────────────────────────────────────

    #[test]
    fn log_row_falls_back_to_dashes_when_attributes_missing() {
        let event = LogEvent {
            id: "x".into(),
            event_type: None,
            attributes: LogEventAttributes::default(),
        };
        let row = log_row(&event);
        assert_eq!(row.timestamp, "-");
        assert_eq!(row.service, "-");
        assert_eq!(row.status, "-");
        assert_eq!(row.message, "");
    }

    #[test]
    fn log_row_uses_attribute_values_when_present() {
        let event = LogEvent {
            id: "x".into(),
            event_type: Some("log".into()),
            attributes: LogEventAttributes {
                timestamp: Some("t".into()),
                service: Some("s".into()),
                status: Some("warn".into()),
                host: None,
                message: Some("m".into()),
                tags: vec![],
            },
        };
        let row = log_row(&event);
        assert_eq!(row.timestamp, "t");
        assert_eq!(row.service, "s");
        assert_eq!(row.status, "warn");
        assert_eq!(row.message, "m");
    }

    // ── time helpers ───────────────────────────────────────────────

    #[test]
    fn epoch_to_rfc3339_uses_z_suffix() {
        assert_eq!(epoch_to_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
    }

    #[test]
    fn epoch_to_rfc3339_falls_back_to_unix_epoch_on_out_of_range() {
        // chrono can't represent i64::MAX seconds (year ~292 billion);
        // the fallback produces 1970-01-01T00:00:00Z so the function
        // never panics on extreme inputs.
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

    // ── run_search ─────────────────────────────────────────────────

    #[tokio::test]
    async fn run_search_table_path_writes_to_stdout() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "filter": {
                    "query": "service:api status:error",
                    "from": "2026-04-22T09:00:00Z",
                    "to": "2026-04-22T10:00:00Z"
                },
                "page": { "limit": 100 },
                "sort": "-timestamp"
            })))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(search_body()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_search(
            &client,
            "service:api status:error",
            "2026-04-22T09:00:00Z",
            "2026-04-22T10:00:00Z",
            100,
            SortOrder::TimestampDesc,
            &OutputFormat::Table,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_search_json_path_returns_ok() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(search_body()))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        run_search(
            &client,
            "*",
            "2026-04-22T09:00:00Z",
            "2026-04-22T10:00:00Z",
            10,
            SortOrder::TimestampAsc,
            &OutputFormat::Json,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn run_search_propagates_api_errors() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .respond_with(wiremock::ResponseTemplate::new(400).set_body_string("bad"))
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let err = run_search(
            &client,
            "*",
            "2026-04-22T09:00:00Z",
            "2026-04-22T10:00:00Z",
            10,
            SortOrder::TimestampDesc,
            &OutputFormat::Table,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("400"));
    }

    #[tokio::test]
    async fn run_search_with_zero_limit_auto_paginates_until_no_cursor() {
        // limit == 0 means "fetch every match"; the wrapper requests
        // pages of MAX_PAGE_LIMIT until the API stops returning a
        // cursor.
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "filter": {
                    "query": "*",
                    "from": "2026-04-22T09:00:00Z",
                    "to": "2026-04-22T10:00:00Z"
                },
                "page": { "limit": 1000 },
                "sort": "-timestamp"
            })))
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
        run_search(
            &client,
            "*",
            "2026-04-22T09:00:00Z",
            "2026-04-22T10:00:00Z",
            0,
            SortOrder::TimestampDesc,
            &OutputFormat::Json,
        )
        .await
        .unwrap();
    }

    // ── SearchCommand::execute glue ────────────────────────────────
    //
    // Tests inject a wiremock-backed client into `execute`, covering the
    // execute-level time-range resolution glue without touching credentials or
    // the environment. (Credential resolution itself is covered by the
    // `load_credentials_with` / `create_client_from` tests.)

    #[tokio::test]
    async fn execute_resolves_time_range_and_searches() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/api/v2/logs/events/search"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(search_body()))
            .expect(1)
            .mount(&server)
            .await;

        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let cmd = SearchCommand {
            filter: "*".into(),
            from: "2026-04-22T09:00:00Z".into(),
            to: "2026-04-22T10:00:00Z".into(),
            limit: 10,
            sort: SortArg::TimestampDesc,
            output: OutputFormat::Json,
        };
        cmd.execute(&client).await.unwrap();
    }

    #[tokio::test]
    async fn execute_propagates_time_range_parse_errors() {
        // --from is intentionally garbage; the time-range parse step runs
        // before any request reaches the injected client.
        let server = wiremock::MockServer::start().await;
        let client = DatadogClient::new(&server.uri(), "api", "app").unwrap();
        let cmd = SearchCommand {
            filter: "*".into(),
            from: "garbage-time".into(),
            to: "now".into(),
            limit: 10,
            sort: SortArg::TimestampDesc,
            output: OutputFormat::Table,
        };
        let err = cmd.execute(&client).await.unwrap_err();
        assert!(err.to_string().contains("Failed to parse"));
    }
}
