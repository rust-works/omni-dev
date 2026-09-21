//! Sheets v4 API façade — typed wrappers over the handful of endpoints the
//! CLI needs, mirroring `crate::drive::files_api::FilesApi`'s shape.
//!
//! Free `build_*_url` functions take a literal `base_url` so they are
//! unit-testable without a client, exactly like `files_api.rs`'s.

use anyhow::{Context, Result};
use url::Url;

use crate::drive::api_client::GoogleApiClient;
use crate::drive::files_api::{append_write_scope_hint, WriteCapability};
use crate::drive::sheets::a1;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::types::{
    AppendValuesResponse, BatchGetValuesResponse, BatchUpdateRequest, BatchUpdateRequestItem,
    BatchUpdateResponse, ClearValuesResponse, CopySheetToAnotherSpreadsheetRequest, DataFilter,
    SearchDeveloperMetadataRequest, SearchDeveloperMetadataResponse, SheetProperties, Spreadsheet,
    UpdateValuesResponse, ValueRange,
};

/// `fields` mask for `spreadsheets.get`.
///
/// **Not optional.** An unmasked `spreadsheets.get` embeds every cell of
/// every sheet in the response, so on a large workbook the difference
/// between sending this and not is an out-of-memory failure rather than a
/// slower request. We only ever need the tab list.
const SPREADSHEET_FIELDS: &str = "spreadsheetId,properties.title,\
    sheets.properties(sheetId,title,index,hidden,gridProperties(rowCount,columnCount))";

/// `fields` mask for `spreadsheets.get` when protected ranges are needed
/// too (issue #1643's `list-protections`/`update-protection`/
/// `unprotect-range`, which must resolve an *existing* protection before
/// they can act on it). A superset of [`SPREADSHEET_FIELDS`], kept separate
/// so every other caller — `sheets info`, `structure.rs`, `format.rs`,
/// `validation.rs` — never pays for data it doesn't use.
const SPREADSHEET_FIELDS_WITH_PROTECTIONS: &str = "spreadsheetId,properties.title,\
    sheets.properties(sheetId,title,index,hidden,gridProperties(rowCount,columnCount)),\
    sheets.protectedRanges(protectedRangeId,range,description,warningOnly,editors.users)";

/// `fields` mask for `spreadsheets.get` when the basic filter and filter
/// views are needed too (issue #1794's `set-basic-filter`/
/// `list-filter-views`/`update-filter-view`/`delete-filter-view`, which must
/// resolve an *existing* filter view before they can act on one). A
/// superset of [`SPREADSHEET_FIELDS`], kept separate for the same reason
/// [`SPREADSHEET_FIELDS_WITH_PROTECTIONS`] is.
const SPREADSHEET_FIELDS_WITH_FILTER_VIEWS: &str = "spreadsheetId,properties.title,\
    sheets.properties(sheetId,title,index,hidden,gridProperties(rowCount,columnCount)),\
    sheets.basicFilter(range,sortSpecs,criteria),\
    sheets.filterViews(filterViewId,title,range,sortSpecs,criteria)";

/// `fields` mask for `spreadsheets.get` when conditional format rules are
/// needed too (issue #1793's `add-conditional-format`/
/// `update-conditional-format`/`delete-conditional-format`/
/// `list-conditional-formats`, all four of which share one fetch — see
/// [`SheetsApi::get_spreadsheet_with_conditional_formats`]'s doc comment for
/// why `add` uses the same wider mask as the other three despite not
/// strictly needing it). A superset of [`SPREADSHEET_FIELDS`], kept separate
/// for the same reason [`SPREADSHEET_FIELDS_WITH_PROTECTIONS`] is.
const SPREADSHEET_FIELDS_WITH_CONDITIONAL_FORMATS: &str = "spreadsheetId,properties.title,\
    sheets.properties(sheetId,title,index,hidden,gridProperties(rowCount,columnCount)),\
    sheets.conditionalFormats(ranges,booleanRule,gradientRule)";

/// `fields` mask for `spreadsheets.get` when named ranges are needed too
/// (issue #1796's `list-named-ranges`/`update-named-range`/
/// `delete-named-range`, which must resolve an *existing* named range by
/// name before they can act on one). A superset of [`SPREADSHEET_FIELDS`],
/// kept separate for the same reason as
/// [`SPREADSHEET_FIELDS_WITH_PROTECTIONS`]: every other caller never pays
/// for data it doesn't use. Named ranges are workbook-scoped, so
/// `namedRanges` sits at the top level, not nested under `sheets` the way
/// `protectedRanges` is.
const SPREADSHEET_FIELDS_WITH_NAMED_RANGES: &str = "spreadsheetId,properties.title,\
    sheets.properties(sheetId,title,index,hidden,gridProperties(rowCount,columnCount)),\
    namedRanges(namedRangeId,name,range)";

/// `fields` mask for `spreadsheets.get` when charts and slicers are needed
/// too (issue #1797's `add-chart`/`update-chart`/`delete-chart`/
/// `list-charts`/`add-slicer`/`update-slicer`/`delete-slicer`/
/// `list-slicers`, all eight of which share one fetch, mirroring
/// `SPREADSHEET_FIELDS_WITH_CONDITIONAL_FORMATS`'s reuse across its four
/// verbs). Deliberately requests `sheets.charts`/`sheets.slicers`
/// **unmasked below the object level** — every other wider mask in this
/// file narrows to the specific sub-fields each verb reads, but
/// `update-chart` must merge onto the chart's *entire* existing spec (see
/// [`crate::drive::sheets::types::UpdateChartSpecRequest`]'s doc comment),
/// so nothing here can be safely left out. A superset of
/// [`SPREADSHEET_FIELDS`], kept separate for the same reason
/// [`SPREADSHEET_FIELDS_WITH_PROTECTIONS`] is.
const SPREADSHEET_FIELDS_WITH_EMBEDDED_OBJECTS: &str = "spreadsheetId,properties.title,\
    sheets.properties(sheetId,title,index,hidden,gridProperties(rowCount,columnCount)),\
    sheets.charts,sheets.slicers";

/// Maximum ranges sent in a single `values.batchGet`.
///
/// Each range is a percent-encoded, quoted sheet title in the query string,
/// so a workbook with hundreds of tabs would otherwise build a URL past what
/// servers and proxies accept. Chunking keeps each request bounded; the
/// engine stitches the chunks back together.
pub(crate) const MAX_RANGES_PER_BATCH: usize = 50;

/// How the API should render cell values.
///
/// Engine-layer, deliberately free of any `clap` derive — the CLI keeps its
/// own `ValueEnum` mirror, the same split `crate::cli::drive::permissions::check`
/// uses for `DriveOperation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValueRenderOption {
    /// Locale-formatted strings, as displayed in the UI (`"1,234.50"`,
    /// `"$5.00"`). The default because it matches both the spreadsheet as
    /// the user sees it and what `drive read --content`'s CSV export already
    /// produces for a Sheet today.
    #[default]
    Formatted,
    /// Raw typed values — JSON numbers and booleans rather than strings.
    /// What you want when feeding the output to something that will do
    /// arithmetic on it.
    Unformatted,
    /// The formula text (`=SUM(A1:A3)`) rather than its result.
    Formula,
}

impl ValueRenderOption {
    /// The wire value for the `valueRenderOption` query parameter.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Formatted => "FORMATTED_VALUE",
            Self::Unformatted => "UNFORMATTED_VALUE",
            Self::Formula => "FORMULA",
        }
    }
}

/// How the API should interpret the values being written.
///
/// Engine-layer, `clap`-free, like [`ValueRenderOption`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValueInputOption {
    /// Parse each value the way typing it into the UI would: `=SUM(A1:A3)`
    /// becomes a formula, `2026-09-06` becomes a date, `1,234` becomes a
    /// number.
    ///
    /// The default because it is what a person means by "write this into the
    /// sheet". It is also the one option whose *wrong* value silently
    /// mangles data rather than erroring — neither choice fails, you just
    /// get formulas you meant as text or the reverse — which is why both
    /// spellings are spelled out in `--help` and in `docs/drive.md`. Note
    /// `--dry-run` does *not* echo it: there is no wrong-looking output to
    /// spot, so it has to be chosen deliberately.
    #[default]
    UserEntered,
    /// Store every value verbatim as a string. A leading `=` stays literal
    /// text rather than becoming a formula.
    Raw,
}

impl ValueInputOption {
    /// The wire value for the `valueInputOption` query parameter.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserEntered => "USER_ENTERED",
            Self::Raw => "RAW",
        }
    }
}

/// Sheets API façade.
#[derive(Debug)]
pub struct SheetsApi<'a> {
    client: &'a SheetsClient,
}

impl<'a> SheetsApi<'a> {
    /// Wraps an existing [`SheetsClient`].
    #[must_use]
    pub fn new(client: &'a SheetsClient) -> Self {
        Self { client }
    }

    /// Fetches a spreadsheet's metadata — its title and the list of sheets.
    ///
    /// Always `fields`-masked; see [`SPREADSHEET_FIELDS`].
    pub async fn get_spreadsheet(&self, spreadsheet_id: &str) -> Result<Spreadsheet> {
        let url = build_spreadsheet_get_url(self.client.base_url(), spreadsheet_id)?;
        self.client
            .transport()
            .get_parsed(url.as_str(), "Failed to parse Sheets spreadsheet metadata")
            .await
    }

    /// Fetches a spreadsheet's metadata **including protected ranges** —
    /// the one read the protection verbs need that no other caller does.
    /// See [`SPREADSHEET_FIELDS_WITH_PROTECTIONS`].
    pub async fn get_spreadsheet_with_protections(
        &self,
        spreadsheet_id: &str,
    ) -> Result<Spreadsheet> {
        let url =
            build_spreadsheet_get_with_protections_url(self.client.base_url(), spreadsheet_id)?;
        self.client
            .transport()
            .get_parsed(
                url.as_str(),
                "Failed to parse Sheets spreadsheet metadata (with protections)",
            )
            .await
    }

    /// Fetches a spreadsheet's metadata **including the basic filter and
    /// filter views** — the one read `filter.rs`'s verbs need that no other
    /// caller does. See [`SPREADSHEET_FIELDS_WITH_FILTER_VIEWS`].
    pub async fn get_spreadsheet_with_filter_views(
        &self,
        spreadsheet_id: &str,
    ) -> Result<Spreadsheet> {
        let url =
            build_spreadsheet_get_with_filter_views_url(self.client.base_url(), spreadsheet_id)?;
        self.client
            .transport()
            .get_parsed(
                url.as_str(),
                "Failed to parse Sheets spreadsheet metadata (with filter views)",
            )
            .await
    }

    /// Fetches a spreadsheet's metadata **including conditional format
    /// rules** — shared by all four `conditional_format.rs` verbs (issue
    /// #1793), mirroring [`Self::get_spreadsheet_with_protections`]'s reuse
    /// across all four protection verbs. `add-conditional-format` doesn't
    /// strictly need the existing list, but reusing this one fetch instead
    /// of adding a second, narrower one keeps `Sheet.conditional_formats`
    /// consistently populated whenever any conditional-format verb runs.
    /// See [`SPREADSHEET_FIELDS_WITH_CONDITIONAL_FORMATS`].
    pub async fn get_spreadsheet_with_conditional_formats(
        &self,
        spreadsheet_id: &str,
    ) -> Result<Spreadsheet> {
        let url = build_spreadsheet_get_with_conditional_formats_url(
            self.client.base_url(),
            spreadsheet_id,
        )?;
        self.client
            .transport()
            .get_parsed(
                url.as_str(),
                "Failed to parse Sheets spreadsheet metadata (with conditional formats)",
            )
            .await
    }

    /// Fetches a spreadsheet's metadata **including named ranges** — the
    /// one read the named-range verbs need that no other caller does. See
    /// [`SPREADSHEET_FIELDS_WITH_NAMED_RANGES`].
    pub async fn get_spreadsheet_with_named_ranges(
        &self,
        spreadsheet_id: &str,
    ) -> Result<Spreadsheet> {
        let url =
            build_spreadsheet_get_with_named_ranges_url(self.client.base_url(), spreadsheet_id)?;
        self.client
            .transport()
            .get_parsed(
                url.as_str(),
                "Failed to parse Sheets spreadsheet metadata (with named ranges)",
            )
            .await
    }

    /// Fetches a spreadsheet's metadata **including charts and slicers** —
    /// shared by all eight `embedded_object.rs` verbs (issue #1797), the
    /// same way [`Self::get_spreadsheet_with_conditional_formats`] is
    /// shared by its four. See
    /// [`SPREADSHEET_FIELDS_WITH_EMBEDDED_OBJECTS`].
    pub async fn get_spreadsheet_with_embedded_objects(
        &self,
        spreadsheet_id: &str,
    ) -> Result<Spreadsheet> {
        let url = build_spreadsheet_get_with_embedded_objects_url(
            self.client.base_url(),
            spreadsheet_id,
        )?;
        self.client
            .transport()
            .get_parsed(
                url.as_str(),
                "Failed to parse Sheets spreadsheet metadata (with embedded objects)",
            )
            .await
    }

    /// Fetches the cell values of a single A1 range.
    pub async fn values_get(
        &self,
        spreadsheet_id: &str,
        range: &str,
        render: ValueRenderOption,
    ) -> Result<ValueRange> {
        let url = build_values_get_url(self.client.base_url(), spreadsheet_id, range, render)?;
        self.client
            .transport()
            .get_parsed(url.as_str(), "Failed to parse Sheets values response")
            .await
    }

    /// Fetches the cell values of several A1 ranges in one request.
    ///
    /// Callers must match results on each [`ValueRange::range`], never on the
    /// order of `ranges` — see that field's docs.
    pub async fn values_batch_get(
        &self,
        spreadsheet_id: &str,
        ranges: &[String],
        render: ValueRenderOption,
    ) -> Result<BatchGetValuesResponse> {
        let url =
            build_values_batch_get_url(self.client.base_url(), spreadsheet_id, ranges, render)?;
        self.client
            .transport()
            .get_parsed(url.as_str(), "Failed to parse Sheets batchGet response")
            .await
    }

    /// Fetches every sheet named in `titles` via chunked `values.batchGet`,
    /// pairing each result with the sheet title the server echoed back —
    /// the "quote each title, batchGet in [`MAX_RANGES_PER_BATCH`]-sized
    /// chunks, resolve each result's title from its own echoed `range`"
    /// skeleton `read.rs::read_whole_workbook` and
    /// `named_range.rs::scan_referencing_formulas` both need, factored out
    /// once so a batching or echo-matching fix lands in one place.
    ///
    /// A result's title is `None`, never dropped, when
    /// [`crate::drive::sheets::a1::sheet_title_of`] can't parse it back out
    /// of the echoed range — the caller decides what that's worth, exactly
    /// as `read_whole_workbook`'s `order_by_workbook` already does.
    pub(crate) async fn batch_get_every_sheet(
        &self,
        spreadsheet_id: &str,
        titles: &[String],
        render: ValueRenderOption,
    ) -> Result<Vec<(Option<String>, ValueRange)>> {
        let mut out = Vec::with_capacity(titles.len());
        for chunk in titles.chunks(MAX_RANGES_PER_BATCH) {
            let ranges: Vec<String> = chunk.iter().map(|t| a1::quote_sheet_title(t)).collect();
            let response = self
                .values_batch_get(spreadsheet_id, &ranges, render)
                .await?;
            for value_range in response.value_ranges {
                let title = value_range.range.as_deref().and_then(a1::sheet_title_of);
                out.push((title, value_range));
            }
        }
        Ok(out)
    }

    /// Overwrites the cells of `range`.
    ///
    /// `pub(in crate::drive)` so the CLI cannot reach it without going
    /// through the gated engine — the same no-bypass-by-construction fence
    /// `FilesApi::create`/`upload`/`edit_content` sit behind.
    pub(in crate::drive) async fn values_update(
        &self,
        spreadsheet_id: &str,
        range: &str,
        values: &[Vec<String>],
        input: ValueInputOption,
    ) -> Result<UpdateValuesResponse> {
        let url = build_values_update_url(self.client.base_url(), spreadsheet_id, range, input)?;
        let body = serde_json::json!({ "range": range, "values": values });
        let response = self
            .client
            .transport()
            .put_json(url.as_str(), &body)
            .await?;
        self.client
            .transport()
            .parse_response(response, "Failed to parse Sheets update response")
            .await
            .map_err(|err| append_write_scope_hint(err, WriteCapability::EditContent))
    }

    /// Appends rows after the last row of the table containing `range`.
    pub(in crate::drive) async fn values_append(
        &self,
        spreadsheet_id: &str,
        range: &str,
        values: &[Vec<String>],
        input: ValueInputOption,
    ) -> Result<AppendValuesResponse> {
        let url = build_values_append_url(self.client.base_url(), spreadsheet_id, range, input)?;
        let body = serde_json::json!({ "range": range, "values": values });
        let response = self
            .client
            .transport()
            .post_json(url.as_str(), &body)
            .await?;
        self.client
            .transport()
            .parse_response(response, "Failed to parse Sheets append response")
            .await
            .map_err(|err| append_write_scope_hint(err, WriteCapability::EditContent))
    }

    /// Clears the values in `range`, leaving formatting intact.
    pub(in crate::drive) async fn values_clear(
        &self,
        spreadsheet_id: &str,
        range: &str,
    ) -> Result<ClearValuesResponse> {
        let url = build_values_clear_url(self.client.base_url(), spreadsheet_id, range)?;
        let response = self
            .client
            .transport()
            .post_json(url.as_str(), &serde_json::json!({}))
            .await?;
        self.client
            .transport()
            .parse_response(response, "Failed to parse Sheets clear response")
            .await
            .map_err(|err| append_write_scope_hint(err, WriteCapability::EditContent))
    }

    /// Applies structural `requests` to a spreadsheet (issue #1613).
    ///
    /// `pub(in crate::drive)` like the other mutating methods: that fence is
    /// what makes gate bypass impossible by construction, since no module
    /// outside `crate::drive` — where every engine runs the write gate first
    /// — can reach it.
    ///
    /// Takes already-built [`BatchUpdateRequestItem`]s rather than a raw
    /// JSON body, so the only requests expressible are the ones that type
    /// models. That is the type-level half of the guarantee its doc comment
    /// describes.
    pub(in crate::drive) async fn batch_update(
        &self,
        spreadsheet_id: &str,
        requests: Vec<BatchUpdateRequestItem>,
    ) -> Result<BatchUpdateResponse> {
        let url = build_batch_update_url(self.client.base_url(), spreadsheet_id)?;
        let body = BatchUpdateRequest { requests };
        let response = self
            .client
            .transport()
            .post_json(url.as_str(), &body)
            .await?;
        self.client
            .transport()
            .parse_response(response, "Failed to parse Sheets batchUpdate response")
            .await
            .map_err(|err| append_write_scope_hint(err, WriteCapability::EditContent))
    }

    /// Copies one sheet from `spreadsheet_id` into `destination_spreadsheet_id`
    /// (ADR-0080 §10 / issue #1676's typed native-document restore path — a
    /// deleted sheet's Drive-copy backup still contains it, so `drive lease
    /// restore` copies it straight back into the live spreadsheet rather
    /// than requiring a manual copy-back via the Drive UI).
    ///
    /// `pub(in crate::drive)` like the other mutating methods here — the
    /// same no-bypass-by-construction fence.
    pub(in crate::drive) async fn copy_to(
        &self,
        spreadsheet_id: &str,
        sheet_id: i64,
        destination_spreadsheet_id: &str,
    ) -> Result<SheetProperties> {
        let url = build_copy_to_url(self.client.base_url(), spreadsheet_id, sheet_id)?;
        let body = CopySheetToAnotherSpreadsheetRequest {
            destination_spreadsheet_id: destination_spreadsheet_id.to_string(),
        };
        let response = self
            .client
            .transport()
            .post_json(url.as_str(), &body)
            .await?;
        self.client
            .transport()
            .parse_response(response, "Failed to parse Sheets copyTo response")
            .await
            .map_err(|err| append_write_scope_hint(err, WriteCapability::EditContent))
    }

    /// Searches for developer-metadata entries matching `filters`
    /// (issue #1795, [ADR-0081](../../../docs/adrs/adr-0081.md) §4).
    ///
    /// Read-only, so — unlike [`Self::batch_update`]/[`Self::copy_to`] —
    /// this is plain `pub`, not `pub(in crate::drive)`, and never goes
    /// through [`append_write_scope_hint`]: nothing here mutates.
    /// `developer_metadata.rs` is the one caller, and it always includes
    /// `visibility: DOCUMENT_VISIBILITY` in every filter it builds, so the
    /// server itself never returns a `PROJECT`-visibility entry to begin
    /// with.
    pub async fn search_developer_metadata(
        &self,
        spreadsheet_id: &str,
        filters: Vec<DataFilter>,
    ) -> Result<SearchDeveloperMetadataResponse> {
        let url = build_developer_metadata_search_url(self.client.base_url(), spreadsheet_id)?;
        let body = SearchDeveloperMetadataRequest {
            data_filters: filters,
        };
        let response = self
            .client
            .transport()
            .post_json(url.as_str(), &body)
            .await?;
        self.client
            .transport()
            .parse_response(
                response,
                "Failed to parse Sheets developerMetadata.search response",
            )
            .await
    }
}

fn build_spreadsheet_get_url(base_url: &str, spreadsheet_id: &str) -> Result<Url> {
    let mut url = GoogleApiClient::api_url(base_url, "/v4/spreadsheets")
        .context("Invalid Sheets base URL")?;
    GoogleApiClient::push_path_segments(&mut url, &[spreadsheet_id])?;
    url.query_pairs_mut()
        .append_pair("fields", SPREADSHEET_FIELDS);
    Ok(url)
}

fn build_spreadsheet_get_with_protections_url(base_url: &str, spreadsheet_id: &str) -> Result<Url> {
    let mut url = GoogleApiClient::api_url(base_url, "/v4/spreadsheets")
        .context("Invalid Sheets base URL")?;
    GoogleApiClient::push_path_segments(&mut url, &[spreadsheet_id])?;
    url.query_pairs_mut()
        .append_pair("fields", SPREADSHEET_FIELDS_WITH_PROTECTIONS);
    Ok(url)
}

fn build_spreadsheet_get_with_filter_views_url(
    base_url: &str,
    spreadsheet_id: &str,
) -> Result<Url> {
    let mut url = GoogleApiClient::api_url(base_url, "/v4/spreadsheets")
        .context("Invalid Sheets base URL")?;
    GoogleApiClient::push_path_segments(&mut url, &[spreadsheet_id])?;
    url.query_pairs_mut()
        .append_pair("fields", SPREADSHEET_FIELDS_WITH_FILTER_VIEWS);
    Ok(url)
}

fn build_spreadsheet_get_with_conditional_formats_url(
    base_url: &str,
    spreadsheet_id: &str,
) -> Result<Url> {
    let mut url = GoogleApiClient::api_url(base_url, "/v4/spreadsheets")
        .context("Invalid Sheets base URL")?;
    GoogleApiClient::push_path_segments(&mut url, &[spreadsheet_id])?;
    url.query_pairs_mut()
        .append_pair("fields", SPREADSHEET_FIELDS_WITH_CONDITIONAL_FORMATS);
    Ok(url)
}

fn build_spreadsheet_get_with_named_ranges_url(
    base_url: &str,
    spreadsheet_id: &str,
) -> Result<Url> {
    let mut url = GoogleApiClient::api_url(base_url, "/v4/spreadsheets")
        .context("Invalid Sheets base URL")?;
    GoogleApiClient::push_path_segments(&mut url, &[spreadsheet_id])?;
    url.query_pairs_mut()
        .append_pair("fields", SPREADSHEET_FIELDS_WITH_NAMED_RANGES);
    Ok(url)
}

fn build_spreadsheet_get_with_embedded_objects_url(
    base_url: &str,
    spreadsheet_id: &str,
) -> Result<Url> {
    let mut url = GoogleApiClient::api_url(base_url, "/v4/spreadsheets")
        .context("Invalid Sheets base URL")?;
    GoogleApiClient::push_path_segments(&mut url, &[spreadsheet_id])?;
    url.query_pairs_mut()
        .append_pair("fields", SPREADSHEET_FIELDS_WITH_EMBEDDED_OBJECTS);
    Ok(url)
}

fn build_values_get_url(
    base_url: &str,
    spreadsheet_id: &str,
    range: &str,
    render: ValueRenderOption,
) -> Result<Url> {
    let mut url = GoogleApiClient::api_url(base_url, "/v4/spreadsheets")
        .context("Invalid Sheets base URL")?;
    GoogleApiClient::push_path_segments(&mut url, &[spreadsheet_id, "values", range])?;
    url.query_pairs_mut()
        .append_pair("valueRenderOption", render.as_str());
    Ok(url)
}

fn build_values_batch_get_url(
    base_url: &str,
    spreadsheet_id: &str,
    ranges: &[String],
    render: ValueRenderOption,
) -> Result<Url> {
    anyhow::ensure!(
        !ranges.is_empty(),
        "values.batchGet requires at least one range"
    );
    anyhow::ensure!(
        ranges.len() <= MAX_RANGES_PER_BATCH,
        "values.batchGet was given {} ranges, over the {MAX_RANGES_PER_BATCH} per-request cap; \
         callers must chunk",
        ranges.len()
    );
    let mut url = GoogleApiClient::api_url(base_url, "/v4/spreadsheets")
        .context("Invalid Sheets base URL")?;
    // `:batchGet` is a suffix on the `values` segment, not a segment of its
    // own; `:` carries no meaning inside a path segment so it survives
    // encoding untouched.
    GoogleApiClient::push_path_segments(&mut url, &[spreadsheet_id, "values:batchGet"])?;
    {
        let mut pairs = url.query_pairs_mut();
        for range in ranges {
            pairs.append_pair("ranges", range);
        }
        pairs.append_pair("valueRenderOption", render.as_str());
    }
    Ok(url)
}

fn build_values_update_url(
    base_url: &str,
    spreadsheet_id: &str,
    range: &str,
    input: ValueInputOption,
) -> Result<Url> {
    let mut url = GoogleApiClient::api_url(base_url, "/v4/spreadsheets")
        .context("Invalid Sheets base URL")?;
    GoogleApiClient::push_path_segments(&mut url, &[spreadsheet_id, "values", range])?;
    url.query_pairs_mut()
        .append_pair("valueInputOption", input.as_str());
    Ok(url)
}

fn build_values_append_url(
    base_url: &str,
    spreadsheet_id: &str,
    range: &str,
    input: ValueInputOption,
) -> Result<Url> {
    let mut url = GoogleApiClient::api_url(base_url, "/v4/spreadsheets")
        .context("Invalid Sheets base URL")?;
    GoogleApiClient::push_path_segments(
        &mut url,
        &[spreadsheet_id, "values", &format!("{range}:append")],
    )?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("valueInputOption", input.as_str());
        // Insert whole rows rather than overwriting whatever sits below the
        // table; `OVERWRITE` is the API default and is the destructive one.
        pairs.append_pair("insertDataOption", "INSERT_ROWS");
    }
    Ok(url)
}

fn build_values_clear_url(base_url: &str, spreadsheet_id: &str, range: &str) -> Result<Url> {
    let mut url = GoogleApiClient::api_url(base_url, "/v4/spreadsheets")
        .context("Invalid Sheets base URL")?;
    GoogleApiClient::push_path_segments(
        &mut url,
        &[spreadsheet_id, "values", &format!("{range}:clear")],
    )?;
    Ok(url)
}

/// `POST /v4/spreadsheets/{id}:batchUpdate`.
///
/// Unlike the `values.*` builders, nothing caller-influenced reaches the
/// path — a spreadsheet id is `[A-Za-z0-9_-]`. The `:batchUpdate` suffix
/// rides the id segment, following the `values:batchGet` precedent above;
/// `:` carries no meaning inside a path segment, so it survives encoding.
fn build_batch_update_url(base_url: &str, spreadsheet_id: &str) -> Result<Url> {
    let mut url = GoogleApiClient::api_url(base_url, "/v4/spreadsheets")
        .context("Invalid Sheets base URL")?;
    GoogleApiClient::push_path_segments(&mut url, &[&format!("{spreadsheet_id}:batchUpdate")])?;
    Ok(url)
}

/// `POST /v4/spreadsheets/{spreadsheetId}/sheets/{sheetId}:copyTo`.
///
/// The `:copyTo` suffix rides the sheet-id segment, same reasoning as
/// [`build_batch_update_url`]'s `:batchUpdate` suffix — a sheet id is a
/// plain integer, so nothing caller-influenced reaches the path here either.
fn build_copy_to_url(base_url: &str, spreadsheet_id: &str, sheet_id: i64) -> Result<Url> {
    let mut url = GoogleApiClient::api_url(base_url, "/v4/spreadsheets")
        .context("Invalid Sheets base URL")?;
    GoogleApiClient::push_path_segments(
        &mut url,
        &[spreadsheet_id, "sheets", &format!("{sheet_id}:copyTo")],
    )?;
    Ok(url)
}

/// `POST /v4/spreadsheets/{spreadsheetId}/developerMetadata:search`.
///
/// Same `:suffix`-on-segment trick as [`build_copy_to_url`]'s `:copyTo` —
/// `spreadsheet_id` is always `[A-Za-z0-9_-]`, so nothing caller-controlled
/// reaches the path unescaped.
fn build_developer_metadata_search_url(base_url: &str, spreadsheet_id: &str) -> Result<Url> {
    let mut url = GoogleApiClient::api_url(base_url, "/v4/spreadsheets")
        .context("Invalid Sheets base URL")?;
    GoogleApiClient::push_path_segments(&mut url, &[spreadsheet_id, "developerMetadata:search"])?;
    Ok(url)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const BASE: &str = "https://sheets.googleapis.com";

    // ── spreadsheets.get ───────────────────────────────────────────────

    #[test]
    fn spreadsheet_get_url_masks_fields() {
        let url = build_spreadsheet_get_url(BASE, "sheet-1").unwrap();
        assert_eq!(url.path(), "/v4/spreadsheets/sheet-1");
        let fields = url
            .query_pairs()
            .find(|(k, _)| k == "fields")
            .map(|(_, v)| v.to_string())
            .expect("fields mask must always be sent");
        assert!(fields.contains("sheets.properties"));
        assert!(fields.contains("title"));
    }

    #[test]
    fn spreadsheet_get_with_conditional_formats_url_masks_the_wider_fields() {
        let url = build_spreadsheet_get_with_conditional_formats_url(BASE, "sheet-1").unwrap();
        assert_eq!(url.path(), "/v4/spreadsheets/sheet-1");
        let fields = url
            .query_pairs()
            .find(|(k, _)| k == "fields")
            .map(|(_, v)| v.to_string())
            .expect("fields mask must always be sent");
        assert!(fields.contains("sheets.conditionalFormats"));
        assert!(fields.contains("booleanRule"));
        assert!(fields.contains("gradientRule"));
    }

    #[tokio::test]
    async fn get_spreadsheet_with_embedded_objects_surfaces_an_unparseable_base_url() {
        // The `?` on the URL builder is the only failure this method can
        // reach before the network call, so a base URL the `url` crate
        // cannot parse is what exercises it — and it must surface as an
        // error, never a panic.
        use crate::drive::auth::{DriveCredentials, DriveGrantedScopes};
        use crate::drive::client::DriveClient;
        use crate::drive::sheets::client::SHEETS_API_URL;
        use crate::test_support::env::MapEnv;
        use crate::utils::secret::Secret;

        let credentials = DriveCredentials {
            client_id: "client-1".to_string(),
            client_secret: Secret::new("secret-1"),
            refresh_token: Secret::new("refresh-1"),
            scope: DriveGrantedScopes::READONLY,
        };
        let drive = DriveClient::new("https://www.googleapis.com", &credentials).unwrap();
        let env = MapEnv::new().with(SHEETS_API_URL, "not a url");
        let sheets = SheetsClient::from_drive_client_with(&env, &drive).unwrap();

        let err = SheetsApi::new(&sheets)
            .get_spreadsheet_with_embedded_objects("sheet-1")
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("Invalid Sheets base URL"),
            "{err:#}"
        );
    }

    #[test]
    fn spreadsheet_get_with_embedded_objects_url_masks_the_wider_fields() {
        let url = build_spreadsheet_get_with_embedded_objects_url(BASE, "sheet-1").unwrap();
        assert_eq!(url.path(), "/v4/spreadsheets/sheet-1");
        let fields = url
            .query_pairs()
            .find(|(k, _)| k == "fields")
            .map(|(_, v)| v.to_string())
            .expect("fields mask must always be sent");
        assert!(fields.contains("sheets.charts"));
        assert!(fields.contains("sheets.slicers"));
    }

    // ── values.get: encoding is the whole point ────────────────────────

    #[test]
    fn values_get_url_percent_encodes_a_space_in_a_quoted_title() {
        let url = build_values_get_url(
            BASE,
            "sheet-1",
            "'My Sheet'!A1:B2",
            ValueRenderOption::Formatted,
        )
        .unwrap();
        assert!(
            url.as_str().contains("'My%20Sheet'!A1:B2"),
            "space must be encoded: {url}"
        );
    }

    #[test]
    fn values_get_url_encodes_characters_that_would_reshape_the_url() {
        // Each of these silently corrupts the request if interpolated raw:
        // `#` truncates to a fragment, `?` starts a query, `/` invents a
        // path segment.
        for (title, encoded) in [("A#B", "%23"), ("A?B", "%3F"), ("A/B", "%2F")] {
            let range = format!("'{title}'!A1");
            let url =
                build_values_get_url(BASE, "s", &range, ValueRenderOption::Formatted).unwrap();
            assert!(
                url.as_str().contains(encoded),
                "{title:?} must encode to {encoded}: {url}"
            );
            assert!(url.fragment().is_none(), "{title:?} leaked a fragment");
            assert!(url.query().is_some_and(|q| !q.contains("!A1")));
            // The range must remain ONE path segment: /v4/spreadsheets/s/
            // values/<range> is exactly five.
            let segments: Vec<&str> = url.path_segments().unwrap().collect();
            assert_eq!(segments.len(), 5, "{title:?} split the path: {url}");
            assert_eq!(segments[3], "values", "{title:?} shifted the path: {url}");
        }
    }

    #[test]
    fn values_get_url_round_trips_the_range_through_decoding() {
        let range = "'Bob''s Sheet'!A1:C9";
        let url = build_values_get_url(BASE, "s", range, ValueRenderOption::Formatted).unwrap();
        let decoded = url
            .path_segments()
            .unwrap()
            .next_back()
            .map(percent_decode)
            .unwrap();
        assert_eq!(decoded, range);
    }

    fn percent_decode(segment: &str) -> String {
        percent_encoding::percent_decode_str(segment)
            .decode_utf8()
            .unwrap()
            .to_string()
    }

    #[test]
    fn values_get_url_sends_the_render_option() {
        for (render, wire) in [
            (ValueRenderOption::Formatted, "FORMATTED_VALUE"),
            (ValueRenderOption::Unformatted, "UNFORMATTED_VALUE"),
            (ValueRenderOption::Formula, "FORMULA"),
        ] {
            let url = build_values_get_url(BASE, "s", "A1", render).unwrap();
            let got = url
                .query_pairs()
                .find(|(k, _)| k == "valueRenderOption")
                .map(|(_, v)| v.to_string())
                .unwrap();
            assert_eq!(got, wire);
        }
    }

    // ── values.batchGet ────────────────────────────────────────────────

    #[test]
    fn batch_get_url_repeats_the_ranges_parameter() {
        let ranges = vec!["'A'!A1:B2".to_string(), "'B'!A1".to_string()];
        let url =
            build_values_batch_get_url(BASE, "s", &ranges, ValueRenderOption::Formatted).unwrap();
        assert_eq!(url.path(), "/v4/spreadsheets/s/values:batchGet");
        let got: Vec<String> = url
            .query_pairs()
            .filter(|(k, _)| k == "ranges")
            .map(|(_, v)| v.to_string())
            .collect();
        assert_eq!(got, ranges);
    }

    #[test]
    fn batch_get_url_keeps_the_colon_suffix_unencoded() {
        let url = build_values_batch_get_url(
            BASE,
            "s",
            &["A1".to_string()],
            ValueRenderOption::Formatted,
        )
        .unwrap();
        assert!(
            url.as_str().contains("/values:batchGet?"),
            "the :batchGet suffix must survive path encoding: {url}"
        );
    }

    #[test]
    fn batch_get_url_rejects_an_empty_range_list() {
        let err =
            build_values_batch_get_url(BASE, "s", &[], ValueRenderOption::Formatted).unwrap_err();
        assert!(err.to_string().contains("at least one range"), "{err}");
    }

    #[test]
    fn batch_get_url_rejects_more_ranges_than_the_cap() {
        let ranges: Vec<String> = (0..=MAX_RANGES_PER_BATCH)
            .map(|i| format!("S{i}"))
            .collect();
        let err = build_values_batch_get_url(BASE, "s", &ranges, ValueRenderOption::Formatted)
            .unwrap_err();
        assert!(err.to_string().contains("per-request cap"), "{err}");
    }

    // ── values.update / values.append / values.clear ──────────────────

    #[test]
    fn update_url_sends_the_value_input_option() {
        let url =
            build_values_update_url(BASE, "s", "A1:B2", ValueInputOption::UserEntered).unwrap();
        assert_eq!(url.path(), "/v4/spreadsheets/s/values/A1:B2");
        let input = url
            .query_pairs()
            .find(|(k, _)| k == "valueInputOption")
            .map(|(_, v)| v.to_string());
        assert_eq!(input, Some("USER_ENTERED".to_string()));
    }

    #[test]
    fn update_url_percent_encodes_a_quoted_sheet_title() {
        let url =
            build_values_update_url(BASE, "s", "'My Sheet'!A1", ValueInputOption::Raw).unwrap();
        assert!(url.path().contains("'My%20Sheet'"), "{url}");
    }

    #[test]
    fn append_url_keeps_the_colon_suffix_and_sets_insert_rows() {
        let url =
            build_values_append_url(BASE, "s", "A1:B2", ValueInputOption::UserEntered).unwrap();
        assert!(url.as_str().contains("/values/A1:B2:append"), "{url}");
        let insert_data_option = url
            .query_pairs()
            .find(|(k, _)| k == "insertDataOption")
            .map(|(_, v)| v.to_string());
        assert_eq!(insert_data_option, Some("INSERT_ROWS".to_string()));
        let input = url
            .query_pairs()
            .find(|(k, _)| k == "valueInputOption")
            .map(|(_, v)| v.to_string());
        assert_eq!(input, Some("USER_ENTERED".to_string()));
    }

    #[test]
    fn clear_url_keeps_the_colon_suffix_and_sends_no_query() {
        let url = build_values_clear_url(BASE, "s", "A1:B2").unwrap();
        assert!(url.as_str().contains("/values/A1:B2:clear"), "{url}");
        assert!(url.query().is_none(), "{url}");
    }

    // ── spreadsheets.sheets.copyTo ─────────────────────────────────────

    #[test]
    fn copy_to_url_keeps_the_colon_suffix_on_the_sheet_id_segment() {
        let url = build_copy_to_url(BASE, "dest-or-source-id", 12345).unwrap();
        assert_eq!(
            url.path(),
            "/v4/spreadsheets/dest-or-source-id/sheets/12345:copyTo"
        );
        assert!(url.query().is_none(), "{url}");
    }

    // ── base URL handling ──────────────────────────────────────────────

    #[test]
    fn urls_respect_a_wiremock_style_base_with_a_port() {
        let url = build_values_get_url(
            "http://127.0.0.1:9123",
            "s",
            "'My Sheet'!A1",
            ValueRenderOption::Formatted,
        )
        .unwrap();
        assert_eq!(url.host_str(), Some("127.0.0.1"));
        assert_eq!(url.port(), Some(9123));
        assert!(url.path().starts_with("/v4/spreadsheets/s/values/"));
    }

    #[test]
    fn render_option_default_is_formatted() {
        assert_eq!(ValueRenderOption::default(), ValueRenderOption::Formatted);
    }
}
