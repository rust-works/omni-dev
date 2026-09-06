//! Sheets v4 API façade — typed wrappers over the handful of endpoints the
//! CLI needs, mirroring `crate::drive::files_api::FilesApi`'s shape.
//!
//! Free `build_*_url` functions take a literal `base_url` so they are
//! unit-testable without a client, exactly like `files_api.rs`'s.

use anyhow::{Context, Result};
use url::Url;

use crate::drive::api_client::GoogleApiClient;
use crate::drive::files_api::{append_write_scope_hint, WriteCapability};
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::types::{
    AppendValuesResponse, BatchGetValuesResponse, ClearValuesResponse, Spreadsheet,
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
}

/// Appends `segments` to `url`'s path, percent-encoding each one.
///
/// **This is the load-bearing detail of this module.** A Sheets range goes in
/// the URL *path* and is caller-influenced text that routinely contains
/// characters with URL meaning: a sheet titled with a `#` truncates the path
/// into a fragment, a `?` starts a query string, a `/` invents a path
/// segment, and a space is simply invalid. Every one of those silently reads
/// or writes the wrong cells rather than erroring.
///
/// `files_api.rs`'s `format!("/drive/v3/files/{file_id}")` sites are safe
/// only because Drive ids are `[A-Za-z0-9_-]`; do not generalise from them.
fn push_path_segments(url: &mut Url, segments: &[&str]) -> Result<()> {
    let mut path = url
        .path_segments_mut()
        .map_err(|()| anyhow::anyhow!("Invalid Sheets base URL: cannot be a base"))?;
    for segment in segments {
        path.push(segment);
    }
    Ok(())
}

fn build_spreadsheet_get_url(base_url: &str, spreadsheet_id: &str) -> Result<Url> {
    let mut url = GoogleApiClient::api_url(base_url, "/v4/spreadsheets")
        .context("Invalid Sheets base URL")?;
    push_path_segments(&mut url, &[spreadsheet_id])?;
    url.query_pairs_mut()
        .append_pair("fields", SPREADSHEET_FIELDS);
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
    push_path_segments(&mut url, &[spreadsheet_id, "values", range])?;
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
    push_path_segments(&mut url, &[spreadsheet_id, "values:batchGet"])?;
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
    push_path_segments(&mut url, &[spreadsheet_id, "values", range])?;
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
    push_path_segments(
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
    push_path_segments(
        &mut url,
        &[spreadsheet_id, "values", &format!("{range}:clear")],
    )?;
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
