//! `drive sheets read` engine — cell values for one range, or for every
//! sheet in a workbook.
//!
//! The multi-sheet path is the reason this module exists: Drive's
//! `files.export` can only ever render a Sheet's **first** tab as CSV
//! (`crate::cli::drive::read`), because Drive has no multi-sheet CSV format.
//! Reading every tab needs one `spreadsheets.get` for the titles and then
//! `values.batchGet` for the data.
//!
//! Read-only, so — unlike the mutating verbs — it consults no write gate.
//! That matches `drive search`/`read`/`dedupe`: [ADR-0071](../../../docs/adrs/adr-0071.md)
//! §11 records read-path gate enforcement as a deliberate, still-open
//! fast-follow across the whole Drive surface, not something this command
//! opts out of on its own.

use anyhow::Result;
use serde::Serialize;

use crate::cli::drive::format::{write_scalar_jsonl, JsonlSerialize};
use crate::drive::sheets::a1;
use crate::drive::sheets::api::{SheetsApi, ValueRenderOption, MAX_RANGES_PER_BATCH};

/// Refuse to fan out across more tabs than this in one command.
///
/// Errors rather than truncating, the same posture as
/// `folder_ancestry::MAX_CHAIN_DEPTH` and [ADR-0070](../../../docs/adrs/adr-0070.md)
/// §3: silently returning some of a workbook would look identical to a
/// workbook that genuinely had that little in it.
const MAX_SHEETS_PER_READ: usize = 200;

/// Per-call read options.
#[derive(Debug, Clone)]
pub struct ReadOptions {
    /// Spreadsheet id.
    pub spreadsheet_id: String,
    /// An explicit A1 range, which may carry its own `Sheet!` prefix.
    pub range: Option<String>,
    /// A sheet title, supplying a prefix for a bare `range` or selecting a
    /// whole tab on its own.
    pub sheet: Option<String>,
    /// How the API should render cell values.
    pub render: ValueRenderOption,
}

impl ReadOptions {
    /// Whether this asks for the whole workbook rather than one range.
    fn is_whole_workbook(&self) -> bool {
        self.range.is_none() && self.sheet.is_none()
    }
}

/// One sheet's worth of cells.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SheetValues {
    /// The sheet title, when it could be determined.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The server-normalised range these values came from.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub range: Option<String>,
    /// Row-major values, **ragged** exactly as the API returned them:
    /// trailing empty cells are truncated per row. Rendering to CSV pads;
    /// JSON/YAML preserve the raggedness as the truthful shape.
    pub values: Vec<Vec<serde_json::Value>>,
}

/// The result of a read.
///
/// Serialised as an ordered **list** of sheets rather than a
/// `{title: rows}` map: a map's key order is not guaranteed through
/// `serde_json`, and workbook order is meaningful to the reader.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReadOutcome {
    /// The spreadsheet that was read.
    pub spreadsheet_id: String,
    /// Its workbook title, when known (absent on a single-range read, which
    /// never fetches workbook metadata).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spreadsheet_title: Option<String>,
    /// One entry per sheet read, in workbook order.
    pub sheets: Vec<SheetValues>,
}

impl JsonlSerialize for ReadOutcome {
    fn write_jsonl(&self, out: &mut dyn std::io::Write) -> Result<()> {
        write_scalar_jsonl(self, out)
    }
}

/// Reads cell values, either for one range or for the whole workbook.
pub async fn read(api: &SheetsApi<'_>, opts: &ReadOptions) -> Result<ReadOutcome> {
    if opts.is_whole_workbook() {
        read_whole_workbook(api, opts).await
    } else {
        read_single_range(api, opts).await
    }
}

/// One `values.get` for a caller-specified range.
async fn read_single_range(api: &SheetsApi<'_>, opts: &ReadOptions) -> Result<ReadOutcome> {
    let range = a1::compose(opts.sheet.as_deref(), opts.range.as_deref())?;
    let value_range = api
        .values_get(&opts.spreadsheet_id, &range, opts.render)
        .await?;
    // Prefer the title the server echoes back; fall back to what the caller
    // asked for, which is all we know if the response omits the range.
    let title = value_range
        .range
        .as_deref()
        .and_then(a1::sheet_title_of)
        .or_else(|| opts.sheet.clone());
    Ok(ReadOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        spreadsheet_title: None,
        sheets: vec![SheetValues {
            title,
            range: value_range.range,
            values: value_range.values,
        }],
    })
}

/// `spreadsheets.get` for the tab list, then chunked `values.batchGet`.
async fn read_whole_workbook(api: &SheetsApi<'_>, opts: &ReadOptions) -> Result<ReadOutcome> {
    let spreadsheet = api.get_spreadsheet(&opts.spreadsheet_id).await?;
    let titles = spreadsheet.sheet_titles();

    anyhow::ensure!(
        titles.len() <= MAX_SHEETS_PER_READ,
        "spreadsheet has {} sheets, over the {MAX_SHEETS_PER_READ}-sheet cap for a whole-workbook \
         read; refusing to return a partial workbook — narrow it with --sheet or --range",
        titles.len()
    );

    // A workbook with no grid sheets at all is legal (every tab a chart).
    if titles.is_empty() {
        return Ok(ReadOutcome {
            spreadsheet_id: opts.spreadsheet_id.clone(),
            spreadsheet_title: Some(spreadsheet.title().to_string()),
            sheets: Vec::new(),
        });
    }

    // Each range is a quoted, percent-encoded title in the query string, so
    // a wide workbook has to be split across requests.
    let mut by_title: Vec<SheetValues> = Vec::with_capacity(titles.len());
    for chunk in titles.chunks(MAX_RANGES_PER_BATCH) {
        let ranges: Vec<String> = chunk.iter().map(|t| a1::quote_sheet_title(t)).collect();
        let response = api
            .values_batch_get(&opts.spreadsheet_id, &ranges, opts.render)
            .await?;
        for value_range in response.value_ranges {
            by_title.push(SheetValues {
                // Match on the range the server echoes, never on request
                // order — see `ValueRange::range`.
                title: value_range.range.as_deref().and_then(a1::sheet_title_of),
                range: value_range.range,
                values: value_range.values,
            });
        }
    }

    Ok(ReadOutcome {
        spreadsheet_id: opts.spreadsheet_id.clone(),
        spreadsheet_title: Some(spreadsheet.title().to_string()),
        sheets: order_by_workbook(by_title, &titles),
    })
}

/// Restores workbook order, and appends anything whose title didn't match a
/// known tab rather than dropping it.
///
/// Nothing is discarded: a range the server normalised into a shape
/// `sheet_title_of` can't read still reaches the caller, just at the end.
fn order_by_workbook(mut fetched: Vec<SheetValues>, titles: &[String]) -> Vec<SheetValues> {
    let mut ordered = Vec::with_capacity(fetched.len());
    for title in titles {
        if let Some(pos) = fetched
            .iter()
            .position(|s| s.title.as_deref() == Some(title.as_str()))
        {
            ordered.push(fetched.remove(pos));
        }
    }
    ordered.extend(fetched);
    ordered
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
    use crate::drive::client::DriveClient;
    use crate::drive::sheets::client::{SheetsClient, SHEETS_API_URL};
    use crate::test_support::env::MapEnv;
    use crate::utils::secret::Secret;

    fn test_credentials() -> DriveCredentials {
        DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        }
    }

    /// Builds a Sheets client pointed at wiremock, via the real derivation
    /// path.
    ///
    /// Note the ordering: `replace_session` swaps the Drive client's whole
    /// transport, so it must run **before** the derive. Deriving first would
    /// leave the Sheets client holding the original session, pointed at the
    /// real `oauth2.googleapis.com` — a live network call from a unit test.
    async fn sheets_client(server: &wiremock::MockServer) -> SheetsClient {
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "test-token",
                    "expires_in": 3600,
                })),
            )
            .mount(server)
            .await;

        let mut drive = DriveClient::new(&server.uri(), &test_credentials()).unwrap();
        crate::drive::client::test_support::replace_session(
            &mut drive,
            &test_credentials(),
            &format!("{}/token", server.uri()),
        );
        let env = MapEnv::new().with(SHEETS_API_URL, &server.uri());
        SheetsClient::from_drive_client_with(&env, &drive).unwrap()
    }

    fn opts(range: Option<&str>, sheet: Option<&str>) -> ReadOptions {
        ReadOptions {
            spreadsheet_id: "sheet-1".to_string(),
            range: range.map(str::to_string),
            sheet: sheet.map(str::to_string),
            render: ValueRenderOption::Formatted,
        }
    }

    fn mount_spreadsheet_get(titles: &[&str]) -> wiremock::Mock {
        let sheets: Vec<serde_json::Value> = titles
            .iter()
            .enumerate()
            .map(|(i, t)| serde_json::json!({"properties": {"sheetId": i, "title": t, "index": i}}))
            .collect();
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/v4/spreadsheets/sheet-1"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "spreadsheetId": "sheet-1",
                    "properties": {"title": "Book"},
                    "sheets": sheets,
                })),
            )
    }

    // ── single-range reads ─────────────────────────────────────────────

    #[tokio::test]
    async fn single_range_read_calls_values_get_once_and_never_fetches_metadata() {
        let server = wiremock::MockServer::start().await;
        let client = sheets_client(&server).await;
        // No `spreadsheets.get` mock: a single-range read must not pay for
        // workbook metadata it does not use.
        // The path is matched in its **encoded** form: this is the
        // end-to-end proof that a sheet title containing a space reaches the
        // wire as `%20` rather than breaking the request.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/'My%20Sheet'!A1:B2",
            ))
            .and(wiremock::matchers::query_param(
                "valueRenderOption",
                "FORMATTED_VALUE",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "range": "'My Sheet'!A1:B2",
                    "values": [["a", "b"], ["c"]],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let api = SheetsApi::new(&client);
        let outcome = read(&api, &opts(Some("A1:B2"), Some("My Sheet")))
            .await
            .unwrap();
        assert_eq!(outcome.sheets.len(), 1);
        assert_eq!(outcome.sheets[0].title.as_deref(), Some("My Sheet"));
        assert_eq!(outcome.sheets[0].values.len(), 2);
        assert_eq!(outcome.spreadsheet_title, None);
    }

    #[tokio::test]
    async fn single_range_read_honours_the_render_option() {
        let server = wiremock::MockServer::start().await;
        let client = sheets_client(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/A1",
            ))
            .and(wiremock::matchers::query_param(
                "valueRenderOption",
                "FORMULA",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"range": "S!A1", "values": [["=SUM(B:B)"]]})),
            )
            .expect(1)
            .mount(&server)
            .await;

        let api = SheetsApi::new(&client);
        let mut o = opts(Some("A1"), None);
        o.render = ValueRenderOption::Formula;
        let outcome = read(&api, &o).await.unwrap();
        assert_eq!(outcome.sheets[0].values[0][0], "=SUM(B:B)");
    }

    #[tokio::test]
    async fn single_range_read_surfaces_an_api_error() {
        let server = wiremock::MockServer::start().await;
        let client = sheets_client(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values/A1",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {"code": 400, "message": "Unable to parse range: A1",
                              "status": "INVALID_ARGUMENT"},
                })),
            )
            .mount(&server)
            .await;

        let api = SheetsApi::new(&client);
        let err = read(&api, &opts(Some("A1"), None)).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Unable to parse range"), "{msg}");
        assert!(msg.contains("INVALID_ARGUMENT"), "{msg}");
    }

    #[tokio::test]
    async fn conflicting_sheet_and_prefixed_range_fails_before_any_request() {
        let server = wiremock::MockServer::start().await;
        let client = sheets_client(&server).await;
        // No API mocks at all: the conflict must be caught client-side.
        let api = SheetsApi::new(&client);
        let err = read(&api, &opts(Some("Other!A1"), Some("Mine")))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("already names a sheet"), "{err}");
    }

    // ── whole-workbook reads ───────────────────────────────────────────

    #[tokio::test]
    async fn whole_workbook_read_composes_get_then_batch_get() {
        let server = wiremock::MockServer::start().await;
        let client = sheets_client(&server).await;
        mount_spreadsheet_get(&["Q1", "My Sheet"])
            .expect(1)
            .mount(&server)
            .await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values:batchGet",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "valueRanges": [
                        {"range": "'Q1'!A1:B2", "values": [["1", "2"]]},
                        {"range": "'My Sheet'!A1:A1", "values": [["x"]]},
                    ],
                })),
            )
            .expect(1)
            .mount(&server)
            .await;

        let api = SheetsApi::new(&client);
        let outcome = read(&api, &opts(None, None)).await.unwrap();
        assert_eq!(outcome.spreadsheet_title.as_deref(), Some("Book"));
        assert_eq!(outcome.sheets.len(), 2);
        assert_eq!(outcome.sheets[0].title.as_deref(), Some("Q1"));
        assert_eq!(outcome.sheets[1].title.as_deref(), Some("My Sheet"));
    }

    #[tokio::test]
    async fn whole_workbook_read_uses_workbook_order_not_response_order() {
        let server = wiremock::MockServer::start().await;
        let client = sheets_client(&server).await;
        mount_spreadsheet_get(&["A", "B"]).mount(&server).await;
        // Server returns them the other way round; workbook order must win.
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values:batchGet",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "valueRanges": [
                        {"range": "'B'!A1", "values": [["b"]]},
                        {"range": "'A'!A1", "values": [["a"]]},
                    ],
                })),
            )
            .mount(&server)
            .await;

        let api = SheetsApi::new(&client);
        let outcome = read(&api, &opts(None, None)).await.unwrap();
        let titles: Vec<_> = outcome
            .sheets
            .iter()
            .map(|s| s.title.clone().unwrap())
            .collect();
        assert_eq!(titles, ["A", "B"]);
        assert_eq!(outcome.sheets[0].values[0][0], "a");
    }

    #[tokio::test]
    async fn whole_workbook_read_handles_an_empty_sheet_with_no_values_key() {
        let server = wiremock::MockServer::start().await;
        let client = sheets_client(&server).await;
        mount_spreadsheet_get(&["Blank"]).mount(&server).await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/v4/spreadsheets/sheet-1/values:batchGet",
            ))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "valueRanges": [{"range": "'Blank'!A1:Z1000"}],
                })),
            )
            .mount(&server)
            .await;

        let api = SheetsApi::new(&client);
        let outcome = read(&api, &opts(None, None)).await.unwrap();
        assert_eq!(outcome.sheets.len(), 1);
        assert!(outcome.sheets[0].values.is_empty());
    }

    #[tokio::test]
    async fn whole_workbook_read_with_no_grid_sheets_makes_no_batch_call() {
        let server = wiremock::MockServer::start().await;
        let client = sheets_client(&server).await;
        mount_spreadsheet_get(&[]).mount(&server).await;
        // No batchGet mock: an empty range list must never be sent.
        let api = SheetsApi::new(&client);
        let outcome = read(&api, &opts(None, None)).await.unwrap();
        assert!(outcome.sheets.is_empty());
        assert_eq!(outcome.spreadsheet_title.as_deref(), Some("Book"));
    }

    #[tokio::test]
    async fn whole_workbook_read_refuses_a_workbook_over_the_sheet_cap() {
        let server = wiremock::MockServer::start().await;
        let client = sheets_client(&server).await;
        let titles: Vec<String> = (0..=MAX_SHEETS_PER_READ).map(|i| format!("S{i}")).collect();
        let refs: Vec<&str> = titles.iter().map(String::as_str).collect();
        mount_spreadsheet_get(&refs).mount(&server).await;
        // No batchGet mock: it must refuse before fetching a partial book.

        let api = SheetsApi::new(&client);
        let err = read(&api, &opts(None, None)).await.unwrap_err();
        assert!(err.to_string().contains("sheet cap"), "{err}");
        assert!(err.to_string().contains("--sheet"), "{err}");
    }

    fn sheet(title: Option<&str>) -> SheetValues {
        SheetValues {
            title: title.map(str::to_string),
            range: None,
            values: Vec::new(),
        }
    }

    #[test]
    fn order_by_workbook_restores_declared_order() {
        let fetched = vec![sheet(Some("C")), sheet(Some("A")), sheet(Some("B"))];
        let titles = ["A".to_string(), "B".to_string(), "C".to_string()];
        let ordered = order_by_workbook(fetched, &titles);
        let got: Vec<_> = ordered.iter().map(|s| s.title.clone().unwrap()).collect();
        assert_eq!(got, ["A", "B", "C"]);
    }

    #[test]
    fn order_by_workbook_appends_unmatched_entries_rather_than_dropping_them() {
        let fetched = vec![sheet(None), sheet(Some("A"))];
        let titles = ["A".to_string()];
        let ordered = order_by_workbook(fetched, &titles);
        assert_eq!(ordered.len(), 2, "an unmatched range must not be dropped");
        assert_eq!(ordered[0].title.as_deref(), Some("A"));
        assert_eq!(ordered[1].title, None);
    }

    #[test]
    fn order_by_workbook_tolerates_a_title_the_server_did_not_return() {
        let fetched = vec![sheet(Some("A"))];
        let titles = ["A".to_string(), "Missing".to_string()];
        let ordered = order_by_workbook(fetched, &titles);
        assert_eq!(ordered.len(), 1);
    }

    #[test]
    fn order_by_workbook_keeps_both_of_two_identically_titled_ranges() {
        // Titles are unique in a real workbook, but the ordering pass must
        // not silently collapse duplicates if one ever appears.
        let fetched = vec![sheet(Some("A")), sheet(Some("A"))];
        let titles = ["A".to_string()];
        let ordered = order_by_workbook(fetched, &titles);
        assert_eq!(ordered.len(), 2);
    }

    #[test]
    fn is_whole_workbook_only_when_neither_selector_is_given() {
        let base = ReadOptions {
            spreadsheet_id: "s".to_string(),
            range: None,
            sheet: None,
            render: ValueRenderOption::Formatted,
        };
        assert!(base.is_whole_workbook());
        assert!(!ReadOptions {
            range: Some("A1".to_string()),
            ..base.clone()
        }
        .is_whole_workbook());
        assert!(!ReadOptions {
            sheet: Some("S".to_string()),
            ..base
        }
        .is_whole_workbook());
    }

    #[test]
    fn read_outcome_serialises_sheets_as_an_ordered_list() {
        let outcome = ReadOutcome {
            spreadsheet_id: "s".to_string(),
            spreadsheet_title: Some("Book".to_string()),
            sheets: vec![sheet(Some("Z")), sheet(Some("A"))],
        };
        let json = serde_json::to_value(&outcome).unwrap();
        let arr = json["sheets"].as_array().unwrap();
        assert_eq!(arr[0]["title"], "Z", "workbook order must survive");
        assert_eq!(arr[1]["title"], "A");
    }

    #[test]
    fn read_outcome_omits_absent_optional_fields() {
        let outcome = ReadOutcome {
            spreadsheet_id: "s".to_string(),
            spreadsheet_title: None,
            sheets: vec![sheet(None)],
        };
        let json = serde_json::to_value(&outcome).unwrap();
        assert!(json.get("spreadsheet_title").is_none());
        assert!(json["sheets"][0].get("title").is_none());
        assert!(json["sheets"][0].get("range").is_none());
    }
}
