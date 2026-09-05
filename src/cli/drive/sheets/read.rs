//! CLI command for `omni-dev drive sheets read`.
//!
//! The `Table` output format renders **CSV**, which is what a grid of cells
//! actually is. That follows the in-tree convention that `Table` means "one
//! command, one rendering, not a literal grid"
//! (`crate::cli::drive::read::render_metadata_table`), and keeps `-o` the
//! single output selector rather than forking a second `--format` flag that
//! could contradict it.

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::drive::client::DriveClient;
use crate::drive::sheets::api::{SheetsApi, ValueRenderOption};
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::read::{read, ReadOptions, ReadOutcome};

/// `--render`'s value set — a thin CLI-layer copy of [`ValueRenderOption`],
/// kept separate so the engine module has no `clap` dependency (mirrors
/// `crate::cli::drive::permissions::check::OperationArg`).
#[derive(Debug, Clone, Copy, Default, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum RenderArg {
    /// Locale-formatted strings, exactly as displayed in the UI.
    #[default]
    Formatted,
    /// Raw typed values — numbers and booleans rather than strings.
    Unformatted,
    /// Formula text (`=SUM(A1:A3)`) rather than the computed result.
    Formula,
}

impl From<RenderArg> for ValueRenderOption {
    fn from(arg: RenderArg) -> Self {
        match arg {
            RenderArg::Formatted => Self::Formatted,
            RenderArg::Unformatted => Self::Unformatted,
            RenderArg::Formula => Self::Formula,
        }
    }
}

/// Reads cell values from one range, or from every sheet.
#[derive(Parser)]
pub struct ReadCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// A1 range to read, optionally carrying its own `Sheet!` prefix (e.g.
    /// `A1:C10`, `'My Sheet'!A:A`). Combined with `--sheet` when bare.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Sheet (tab) title to read. Supplies the prefix for a bare `--range`,
    /// or selects the whole tab on its own. Conflicts with a `--range` that
    /// already names a sheet.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// How cell values are rendered. `formatted` matches the spreadsheet as
    /// displayed; `unformatted` yields raw typed numbers rather than
    /// locale-formatted strings; `formula` yields formula text.
    #[arg(long, value_enum, default_value_t = RenderArg::Formatted)]
    pub render: RenderArg,

    /// Output format. The default `table` emits CSV — one block per sheet,
    /// each preceded by a `# <title>` comment line when reading more than
    /// one. Use `json`/`yaml` to keep values typed and rows ragged.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ReadCommand {
    /// Runs the command, deriving a Sheets client from the shared Drive one.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let sheets = SheetsClient::from_drive_client(client)?;
        let opts = ReadOptions {
            spreadsheet_id: self.spreadsheet_id,
            range: self.range,
            sheet: self.sheet,
            render: self.render.into(),
        };
        run_read(&sheets, &opts, &self.output).await
    }
}

/// Reads and renders.
///
/// Split from [`ReadCommand::execute`] so tests can inject a wiremock client
/// without going through the credential-loading path.
async fn run_read(client: &SheetsClient, opts: &ReadOptions, output: &OutputFormat) -> Result<()> {
    let outcome = read(&SheetsApi::new(client), opts).await?;
    if output_as(&outcome, output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_csv(&outcome, &mut handle)
}

/// Renders every sheet as CSV, padded to a rectangle.
///
/// Two deliberate choices, both of which change what the caller sees:
///
/// - **Rows are padded** to the widest row in that sheet. The API truncates
///   trailing empty cells, so the raw rows are ragged and a ragged CSV is
///   malformed. `-o json` preserves the raggedness instead.
/// - **Cell values are not passed through `sanitize_for_terminal`.** They
///   are *content*, not chrome, and stripping control characters would
///   silently destroy legitimate multi-line cell text and make the CSV
///   non-round-trippable. This matches `drive read --content`, which already
///   prints file content verbatim, and `output_as`, which does not sanitize
///   the JSON/YAML paths either. Only the `# <title>` header lines — which
///   are chrome — are sanitized.
fn render_csv(outcome: &ReadOutcome, out: &mut dyn std::io::Write) -> Result<()> {
    let multi = outcome.sheets.len() > 1;
    for (idx, sheet) in outcome.sheets.iter().enumerate() {
        if multi {
            if idx > 0 {
                writeln!(out).context("Failed to write CSV separator")?;
            }
            let title = sheet.title.as_deref().unwrap_or("(untitled)");
            writeln!(out, "# {}", sanitize_for_terminal(title))
                .context("Failed to write CSV sheet header")?;
        }
        write_sheet_csv(&sheet.values, out)?;
    }
    Ok(())
}

/// Writes one sheet's rows as CSV, padded to the widest row.
fn write_sheet_csv(values: &[Vec<serde_json::Value>], out: &mut dyn std::io::Write) -> Result<()> {
    let width = values.iter().map(Vec::len).max().unwrap_or(0);
    if width == 0 {
        return Ok(());
    }
    let mut writer = csv::WriterBuilder::new().from_writer(out);
    for row in values {
        let mut record: Vec<String> = row.iter().map(cell_to_string).collect();
        record.resize(width, String::new());
        writer
            .write_record(&record)
            .context("Failed to write a CSV row")?;
    }
    writer.flush().context("Failed to flush CSV output")
}

/// Renders one cell as text.
///
/// A JSON string is emitted as its own contents, not re-quoted — otherwise
/// every `formatted` cell would arrive wrapped in literal double quotes.
/// `null` (an explicitly empty cell) becomes the empty string.
fn cell_to_string(cell: &serde_json::Value) -> String {
    match cell {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::drive::sheets::read::SheetValues;

    fn sheet(title: Option<&str>, values: &[&[serde_json::Value]]) -> SheetValues {
        SheetValues {
            title: title.map(str::to_string),
            range: None,
            values: values.iter().map(|row| row.to_vec()).collect(),
        }
    }

    fn outcome(sheets: Vec<SheetValues>) -> ReadOutcome {
        ReadOutcome {
            spreadsheet_id: "s".to_string(),
            spreadsheet_title: None,
            sheets,
        }
    }

    fn render(o: &ReadOutcome) -> String {
        let mut buf = Vec::new();
        render_csv(o, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }

    // ── cell_to_string ─────────────────────────────────────────────────

    #[test]
    fn cell_to_string_unwraps_strings_rather_than_requoting_them() {
        assert_eq!(cell_to_string(&serde_json::json!("hi")), "hi");
    }

    #[test]
    fn cell_to_string_renders_typed_values_and_null() {
        assert_eq!(cell_to_string(&serde_json::json!(1234.5)), "1234.5");
        assert_eq!(cell_to_string(&serde_json::json!(true)), "true");
        assert_eq!(cell_to_string(&serde_json::Value::Null), "");
    }

    // ── render_csv ─────────────────────────────────────────────────────

    #[test]
    fn single_sheet_renders_without_a_header_line() {
        let text = render(&outcome(vec![sheet(
            Some("Q1"),
            &[&[serde_json::json!("a"), serde_json::json!("b")]],
        )]));
        assert_eq!(text, "a,b\n");
    }

    #[test]
    fn multiple_sheets_get_a_commented_title_and_a_blank_separator() {
        let text = render(&outcome(vec![
            sheet(Some("Q1"), &[&[serde_json::json!("a")]]),
            sheet(Some("My Sheet"), &[&[serde_json::json!("b")]]),
        ]));
        assert_eq!(text, "# Q1\na\n\n# My Sheet\nb\n");
    }

    #[test]
    fn ragged_rows_are_padded_to_the_widest_row() {
        // The API truncates trailing empty cells, so this is the normal
        // shape of a real response — an unpadded CSV would be malformed.
        let text = render(&outcome(vec![sheet(
            Some("S"),
            &[
                &[
                    serde_json::json!("a"),
                    serde_json::json!("b"),
                    serde_json::json!("c"),
                ],
                &[serde_json::json!("d")],
            ],
        )]));
        assert_eq!(text, "a,b,c\nd,,\n");
    }

    #[test]
    fn an_empty_sheet_renders_as_nothing_but_its_header() {
        let text = render(&outcome(vec![
            sheet(Some("Blank"), &[]),
            sheet(Some("Full"), &[&[serde_json::json!("x")]]),
        ]));
        assert_eq!(text, "# Blank\n\n# Full\nx\n");
    }

    #[test]
    fn a_single_empty_sheet_renders_as_empty_output() {
        assert_eq!(render(&outcome(vec![sheet(Some("Blank"), &[])])), "");
    }

    #[test]
    fn cells_containing_commas_quotes_and_newlines_are_csv_quoted() {
        let text = render(&outcome(vec![sheet(
            Some("S"),
            &[&[
                serde_json::json!("a,b"),
                serde_json::json!("say \"hi\""),
                serde_json::json!("line1\nline2"),
            ]],
        )]));
        // Round-trips through a real CSV reader — the point of taking the
        // dependency rather than hand-rolling the quoting.
        let mut reader = csv::ReaderBuilder::new()
            .has_headers(false)
            .from_reader(text.as_bytes());
        let record = reader.records().next().unwrap().unwrap();
        assert_eq!(&record[0], "a,b");
        assert_eq!(&record[1], "say \"hi\"");
        assert_eq!(
            &record[2], "line1\nline2",
            "a multi-line cell must survive intact"
        );
    }

    #[test]
    fn a_sheet_with_no_title_still_renders_under_a_placeholder_header() {
        let text = render(&outcome(vec![
            sheet(None, &[&[serde_json::json!("a")]]),
            sheet(Some("B"), &[&[serde_json::json!("b")]]),
        ]));
        assert!(text.contains("# (untitled)"), "{text}");
        assert!(text.contains('a'), "{text}");
    }

    #[test]
    fn a_control_sequence_in_a_sheet_title_is_stripped_from_the_header() {
        // Titles are chrome, so they are sanitized...
        let text = render(&outcome(vec![
            sheet(Some("evil\x1b[31mtab"), &[&[serde_json::json!("a")]]),
            sheet(Some("B"), &[&[serde_json::json!("b")]]),
        ]));
        assert!(text.contains("# evil[31mtab"), "{text:?}");
    }

    #[test]
    fn cell_content_is_emitted_verbatim_not_sanitized() {
        // ...but cell values are content. Stripping control characters here
        // would silently corrupt real data, and `drive read --content`
        // already prints file content verbatim.
        let text = render(&outcome(vec![sheet(
            Some("S"),
            &[&[serde_json::json!("multi\nline")]],
        )]));
        assert!(text.contains("multi\nline"), "{text:?}");
    }

    // ── RenderArg ──────────────────────────────────────────────────────

    #[test]
    fn render_arg_maps_onto_the_engine_option() {
        assert_eq!(
            ValueRenderOption::from(RenderArg::Formatted),
            ValueRenderOption::Formatted
        );
        assert_eq!(
            ValueRenderOption::from(RenderArg::Unformatted),
            ValueRenderOption::Unformatted
        );
        assert_eq!(
            ValueRenderOption::from(RenderArg::Formula),
            ValueRenderOption::Formula
        );
    }

    #[test]
    fn render_arg_defaults_to_formatted() {
        assert_eq!(
            ValueRenderOption::from(RenderArg::default()),
            ValueRenderOption::Formatted
        );
    }
}
