//! Parsing `--values` for `drive sheets write`/`append`.
//!
//! Pure: reads a `&str` and returns rows. The file/stdin read lives in the
//! caller so these functions are testable without touching the filesystem.
//!
//! CSV parsing goes through the `csv` crate rather than a hand-rolled split.
//! That is a deliberate dependency: a quoted field containing commas,
//! doubled quotes or an embedded newline is exactly where a hand-rolled
//! parser goes subtly wrong, and with `--input user-entered` the wrong
//! answer lands in real cells rather than erroring.

use anyhow::{Context, Result};
use clap::ValueEnum;

/// How to interpret the `--values` payload.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum ValuesFormat {
    /// Infer from the path's extension: `.json` is JSON, everything else —
    /// including stdin — is CSV.
    #[default]
    Auto,
    /// RFC 4180 CSV.
    Csv,
    /// A JSON array of arrays.
    Json,
}

impl ValuesFormat {
    /// Resolves `Auto` against the source path.
    ///
    /// Stdin (`-`) has no extension to read, so it resolves to CSV; pass
    /// `--values-format json` to override.
    #[must_use]
    pub fn resolve(self, source: &str) -> Self {
        match self {
            Self::Auto => {
                if std::path::Path::new(source)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
                {
                    Self::Json
                } else {
                    Self::Csv
                }
            }
            other => other,
        }
    }
}

/// Parses `content` into row-major cell values.
pub fn parse(content: &str, format: ValuesFormat) -> Result<Vec<Vec<String>>> {
    match format {
        ValuesFormat::Json => parse_json(content),
        // `resolve` is expected to have run first; treating a stray `Auto`
        // as CSV matches its own default rather than panicking.
        ValuesFormat::Csv | ValuesFormat::Auto => parse_csv(content),
    }
}

/// Parses RFC 4180 CSV, preserving ragged rows.
///
/// Raggedness is **not** normalised here: the Sheets API accepts rows of
/// differing length, and padding on the way in would silently write empty
/// strings over cells the caller never mentioned. `--dry-run` reports the
/// dimensions so a ragged or transposed input is visible before it lands.
fn parse_csv(content: &str) -> Result<Vec<Vec<String>>> {
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(content.as_bytes());
    let mut rows = Vec::new();
    for (index, record) in reader.records().enumerate() {
        let record = record.with_context(|| format!("Failed to parse CSV row {}", index + 1))?;
        rows.push(record.iter().map(str::to_string).collect());
    }
    Ok(rows)
}

/// Parses a JSON array of arrays.
///
/// Scalars are stringified rather than rejected — `[[1, true, "x"]]` is a
/// natural thing to write, and the API takes strings for every cell anyway.
/// `null` becomes an empty cell. A nested array or object is an error: there
/// is no sensible single-cell rendering of one, and silently writing
/// `{"a":1}` into a cell would be worse than refusing.
fn parse_json(content: &str) -> Result<Vec<Vec<String>>> {
    let parsed: serde_json::Value =
        serde_json::from_str(content).context("Failed to parse --values as JSON")?;
    let rows = parsed
        .as_array()
        .context("--values JSON must be an array of arrays (rows of cells)")?;

    let mut out = Vec::with_capacity(rows.len());
    for (row_index, row) in rows.iter().enumerate() {
        let cells = row.as_array().with_context(|| {
            format!(
                "--values JSON row {} is not an array; expected an array of arrays",
                row_index + 1
            )
        })?;
        let mut parsed_row = Vec::with_capacity(cells.len());
        for (col_index, cell) in cells.iter().enumerate() {
            parsed_row.push(cell_to_string(cell).with_context(|| {
                format!(
                    "--values JSON row {}, column {}",
                    row_index + 1,
                    col_index + 1
                )
            })?);
        }
        out.push(parsed_row);
    }
    Ok(out)
}

fn cell_to_string(cell: &serde_json::Value) -> Result<String> {
    match cell {
        serde_json::Value::String(s) => Ok(s.clone()),
        serde_json::Value::Null => Ok(String::new()),
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => Ok(cell.to_string()),
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => Err(anyhow::anyhow!(
            "a cell must be a string, number, boolean or null, not {cell}"
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── ValuesFormat::resolve ──────────────────────────────────────────

    #[test]
    fn auto_resolves_json_only_for_a_json_extension() {
        assert_eq!(ValuesFormat::Auto.resolve("cells.json"), ValuesFormat::Json);
        assert_eq!(ValuesFormat::Auto.resolve("cells.JSON"), ValuesFormat::Json);
        assert_eq!(ValuesFormat::Auto.resolve("cells.csv"), ValuesFormat::Csv);
        assert_eq!(ValuesFormat::Auto.resolve("cells"), ValuesFormat::Csv);
    }

    #[test]
    fn auto_resolves_stdin_to_csv() {
        assert_eq!(ValuesFormat::Auto.resolve("-"), ValuesFormat::Csv);
    }

    #[test]
    fn an_explicit_format_overrides_the_extension() {
        assert_eq!(ValuesFormat::Csv.resolve("cells.json"), ValuesFormat::Csv);
        assert_eq!(ValuesFormat::Json.resolve("-"), ValuesFormat::Json);
    }

    // ── CSV ────────────────────────────────────────────────────────────

    #[test]
    fn csv_parses_a_simple_grid() {
        let rows = parse("a,b\nc,d\n", ValuesFormat::Csv).unwrap();
        assert_eq!(rows, vec![vec!["a", "b"], vec!["c", "d"]]);
    }

    #[test]
    fn csv_handles_quoted_commas_quotes_and_embedded_newlines() {
        // The whole reason for taking the `csv` dependency.
        let input = "\"a,b\",\"say \"\"hi\"\"\",\"line1\nline2\"\n";
        let rows = parse(input, ValuesFormat::Csv).unwrap();
        assert_eq!(rows, vec![vec!["a,b", "say \"hi\"", "line1\nline2"]]);
    }

    #[test]
    fn csv_preserves_ragged_rows_rather_than_padding_them() {
        // Padding here would write empty strings over cells the caller
        // never mentioned.
        let rows = parse("a,b,c\nd\n", ValuesFormat::Csv).unwrap();
        assert_eq!(rows, vec![vec!["a", "b", "c"], vec!["d"]]);
    }

    #[test]
    fn csv_keeps_a_leading_equals_intact_for_the_input_option_to_decide() {
        let rows = parse("=SUM(A1:A3)\n", ValuesFormat::Csv).unwrap();
        assert_eq!(rows, vec![vec!["=SUM(A1:A3)"]]);
    }

    #[test]
    fn csv_treats_the_first_row_as_data_not_a_header() {
        let rows = parse("Region,Revenue\nNorth,1200\n", ValuesFormat::Csv).unwrap();
        assert_eq!(rows.len(), 2, "the header row must be written too");
        assert_eq!(rows[0], vec!["Region", "Revenue"]);
    }

    #[test]
    fn csv_of_empty_input_is_no_rows() {
        assert!(parse("", ValuesFormat::Csv).unwrap().is_empty());
    }

    #[test]
    fn csv_preserves_empty_cells() {
        let rows = parse("a,,c\n", ValuesFormat::Csv).unwrap();
        assert_eq!(rows, vec![vec!["a", "", "c"]]);
    }

    // ── JSON ───────────────────────────────────────────────────────────

    #[test]
    fn json_parses_an_array_of_arrays() {
        let rows = parse(r#"[["a","b"],["c"]]"#, ValuesFormat::Json).unwrap();
        assert_eq!(rows, vec![vec!["a", "b"], vec!["c"]]);
    }

    #[test]
    fn json_stringifies_scalars_and_empties_null() {
        let rows = parse(r#"[[1, 2.5, true, null, "x"]]"#, ValuesFormat::Json).unwrap();
        assert_eq!(rows, vec![vec!["1", "2.5", "true", "", "x"]]);
    }

    #[test]
    fn json_rejects_a_top_level_object() {
        let err = parse(r#"{"a": 1}"#, ValuesFormat::Json).unwrap_err();
        assert!(err.to_string().contains("array of arrays"), "{err}");
    }

    #[test]
    fn json_rejects_a_row_that_is_not_an_array_and_names_it() {
        let err = parse(r#"[["a"], "oops"]"#, ValuesFormat::Json).unwrap_err();
        assert!(err.to_string().contains("row 2"), "{err}");
    }

    #[test]
    fn json_rejects_a_nested_cell_and_names_its_position() {
        let err = parse(r#"[["a", {"b": 1}]]"#, ValuesFormat::Json).unwrap_err();
        let chain = format!("{err:#}");
        assert!(chain.contains("row 1"), "{chain}");
        assert!(chain.contains("column 2"), "{chain}");
    }

    #[test]
    fn json_rejects_malformed_input() {
        let err = parse("[[", ValuesFormat::Json).unwrap_err();
        assert!(
            err.to_string().contains("Failed to parse --values"),
            "{err}"
        );
    }

    #[test]
    fn json_of_an_empty_array_is_no_rows() {
        assert!(parse("[]", ValuesFormat::Json).unwrap().is_empty());
    }

    // ── dispatch ───────────────────────────────────────────────────────

    #[test]
    fn a_stray_auto_falls_back_to_csv() {
        let rows = parse("a,b\n", ValuesFormat::Auto).unwrap();
        assert_eq!(rows, vec![vec!["a", "b"]]);
    }
}
