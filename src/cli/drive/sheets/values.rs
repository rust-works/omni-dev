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

/// Parses RFC 4180 CSV, preserving ragged rows **and blank lines**.
///
/// Raggedness is **not** normalised here: the Sheets API accepts rows of
/// differing length, and padding on the way in would silently write empty
/// strings over cells the caller never mentioned. `--dry-run` reports the
/// dimensions so a ragged or transposed input is visible before it lands.
///
/// The `csv` crate silently skips blank lines and has no option to turn
/// that off — but row *position* is the whole point of `--values`: a blank
/// line in the middle of pasted data must land on the sheet row it visually
/// occupies, or every later row silently shifts up by one (#1936). So this
/// does not hand `content` to the reader unmodified and collect one row per
/// [`csv::StringRecord`]; it also reconstructs the blank lines the reader
/// swallowed, using its own line-position tracking rather than a
/// line-based pre-split — a naive `content.split('\n')` would corrupt a
/// quoted field that itself contains a blank line (`"a\n\nb"`).
///
/// How it works: `csv_core` (which underlies the reader) advances its line
/// counter for every `\n` it consumes, skipped blank lines included, and
/// [`csv::Reader::position`] reports the line reached right after a
/// successful [`csv::Reader::read_record`] call. So for each record, the
/// number of lines that call consumed beyond the record's own content (one
/// line per embedded newline in a quoted field) and its own terminator is
/// exactly the count of blank lines immediately preceding it. Trailing
/// blank lines (after the last record, or in an all-blank input) fall out
/// the same way, comparing the reader's final EOF position to the last
/// line accounted for. The one wrinkle is the file's very last record: it
/// might have no terminator at all (a file with no final newline), so its
/// blank-line count is provisionally computed assuming one and corrected
/// once the loop confirms it really was the last record.
///
/// `content` is normalised from CRLF to LF up front. This is not the
/// naive line-split the crate itself warns against — it doesn't need to
/// know where quotes are, since replacing every `\r\n` with `\n` doesn't
/// change which byte positions are line breaks, only which one or two
/// bytes represent them. It sidesteps a genuine `csv_core` quirk: a
/// `\r`-initiated terminator is registered one byte at a time (it must
/// peek the following byte to tell `\r\n` from a bare `\r`), and when that
/// peek would be the last byte in the file, the increment is deferred to a
/// *following* read call that never comes — misreading a correctly
/// CRLF-terminated last line as a phantom trailing blank row. Normalising
/// first means every terminator csv_core sees is a plain `\n`, so this
/// never happens. The trade-off is minor: an embedded `\r\n` inside a
/// quoted multi-line field is written back as a bare `\n`, losing the
/// carriage return.
///
/// The reader is also built with `.terminator(csv::Terminator::Any(b'\n'))`
/// to force `\n` as the *only* record terminator. By default the `csv`
/// crate also treats a bare `\r` outside quotes as a terminator — RFC 4180
/// has no such rule, but `csv_core`'s line counter only counts `\n`, so a
/// bare `\r` splits a record without advancing the line count, the same
/// desync #1936 was about (`"a\r\rb"` silently lost its blank line). With
/// this set, a bare `\r` is ordinary field content instead.
///
/// **Design decision — a blank line is one empty cell (`[""]`), not zero
/// cells (`[]`).** This matches RFC 4180 (an empty line is a row with one
/// empty field) and how a quoted `""` line already parses, so the two
/// blank forms can't disagree — pinned by
/// `csv_quoted_empty_and_blank_line_agree`. A live check against the
/// Sheets API also confirmed it operationally: `values.update` accepts a
/// `[]` row and keeps later rows in position, but `values.append` silently
/// drops a *trailing* `[]` row from the write itself — exactly this
/// issue's row-loss bug, reintroduced through the API layer — while
/// `[""]` writes every row including a trailing blank one (see the PR
/// description for the exact request/response pairs).
fn parse_csv(content: &str) -> Result<Vec<Vec<String>>> {
    let content = content.replace("\r\n", "\n");
    // A blank line needs a newline to exist at all, so a file with no
    // final newline can only ever be missing one on its very last
    // (necessarily non-blank) record — never a trailing blank line.
    let missing_final_newline = !content.is_empty() && !content.ends_with('\n');

    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        // Force LF as the only record terminator: by default the `csv`
        // crate also treats a bare `\r` outside quotes as one (RFC 4180
        // has no such rule; this is the crate's own leniency), and unlike
        // `\n` it does not advance `csv_core`'s line counter — a
        // differential fuzzer found this desynchronises row counting from
        // record counting exactly like #1936 (`"a\r\rb"` parses as two
        // records but the blank line between them is invisible to the
        // line-position math below, so it vanishes instead of becoming a
        // row). With this set, a bare `\r` is ordinary field content.
        .terminator(csv::Terminator::Any(b'\n'))
        .from_reader(content.as_bytes());

    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut record = csv::StringRecord::new();
    // The line the reader had fully accounted for as of the last record
    // pushed (or 1, at the start).
    let mut line = 1u64;
    // The row index, consumed-line count and embedded-newline count of the
    // most recently pushed record, kept so its assumed terminator can be
    // corrected once the loop confirms whether it really was the last one.
    let mut last: Option<(usize, u64, u64)> = None;

    loop {
        // Blank rows already pushed are on the same 1-based footing as the
        // sheet row a parse error at this point would land on.
        let row_number = rows.len() + 1;
        let more = reader
            .read_record(&mut record)
            .with_context(|| format!("Failed to parse CSV row {row_number}"))?;
        if !more {
            break;
        }
        let after = reader.position().line();
        let consumed = after - line;
        let embedded_newlines: u64 = record
            .iter()
            .map(|field| field.matches('\n').count() as u64)
            .sum();
        // Assume this record ended in a real terminator (true for every
        // record except possibly the file's last, corrected below).
        let blanks = consumed.saturating_sub(embedded_newlines + 1);
        rows.extend((0..blanks).map(|_| vec![String::new()]));
        rows.push(record.iter().map(str::to_string).collect());
        last = Some((rows.len() - 1, consumed, embedded_newlines));
        line = after;
    }

    if missing_final_newline {
        if let Some((idx, consumed, embedded_newlines)) = last {
            // The last record didn't actually have the terminator assumed
            // above, so it borrowed one blank line's worth of credit that
            // was never there. Restore it as a blank row immediately
            // before that record, but only if there really was one: when
            // `consumed` was already too small to cover even the assumed
            // terminator, the assumption cost nothing to begin with.
            if consumed > embedded_newlines {
                rows.insert(idx, vec![String::new()]);
            }
        } // omni-dev: coverage ignore-line reason="this closing brace of the `if let Some(...) = last` (no else) reports 0 hits under llvm-cov even though both inner branches are exercised (csv_interior_blank_line_before_an_unterminated_last_row_is_kept hits the insert path, csv_single_trailing_newline_adds_no_row hits the no-insert path); `last` is always `Some` once `missing_final_newline` is true, since the loop above always runs at least once for non-empty content, so there is no reachable skip path — the same llvm-cov region-attribution artifact on an if-let's closing brace as src/utils/settings.rs:1096"
    } else {
        let trailing = (reader.position().line() - line) as usize;
        rows.extend((0..trailing).map(|_| vec![String::new()]));
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
    fn csv_of_a_single_newline_is_one_blank_row() {
        // Unlike `""`, `"\n"` contains one (blank) line, so it is one row.
        assert_eq!(parse("\n", ValuesFormat::Csv).unwrap(), vec![vec![""]]);
    }

    #[test]
    fn csv_preserves_empty_cells() {
        let rows = parse("a,,c\n", ValuesFormat::Csv).unwrap();
        assert_eq!(rows, vec![vec!["a", "", "c"]]);
    }

    // ── CSV blank lines (#1936) ──────────────────────────────────────────

    #[test]
    fn csv_preserves_interior_blank_lines_as_rows() {
        // "p\nq\n\nr\n" is four physical lines: p, q, a blank one, then r.
        // The blank line must land on its own row, in position, rather
        // than being dropped and shifting r up to row 3.
        let rows = parse("p\nq\n\nr\n", ValuesFormat::Csv).unwrap();
        assert_eq!(
            rows,
            vec![vec!["p"], vec!["q"], vec![""], vec!["r"]],
            "the blank line is row 3, between q and r"
        );
    }

    #[test]
    fn csv_preserves_consecutive_and_trailing_blank_lines() {
        // The live repro from the issue: two separated blank lines, one of
        // them the file's last content-bearing line before EOF.
        let rows = parse("p\nq\n\nr\n\ns\n", ValuesFormat::Csv).unwrap();
        assert_eq!(
            rows,
            vec![
                vec!["p"],
                vec!["q"],
                vec![""],
                vec!["r"],
                vec![""],
                vec!["s"],
            ]
        );
    }

    #[test]
    fn csv_single_trailing_newline_adds_no_row() {
        // Every text file and every `printf` ends with exactly one
        // newline; that convention must not itself produce a blank row.
        assert_eq!(parse("a\n", ValuesFormat::Csv).unwrap(), vec![vec!["a"]]);
        assert_eq!(parse("a", ValuesFormat::Csv).unwrap(), vec![vec!["a"]]);
    }

    #[test]
    fn csv_trailing_blank_line_is_a_row() {
        let rows = parse("a\n\n", ValuesFormat::Csv).unwrap();
        assert_eq!(rows, vec![vec!["a"], vec![""]]);
    }

    #[test]
    fn csv_leading_blank_line_is_a_row() {
        let rows = parse("\na\n", ValuesFormat::Csv).unwrap();
        assert_eq!(rows, vec![vec![""], vec!["a"]]);
    }

    #[test]
    fn csv_crlf_blank_lines_are_rows() {
        let rows = parse("a\r\n\r\nb\r\n", ValuesFormat::Csv).unwrap();
        assert_eq!(rows, vec![vec!["a"], vec![""], vec!["b"]]);
    }

    #[test]
    fn csv_bare_cr_is_not_a_line_break() {
        // A differential fuzzer found the `csv` crate treats a bare `\r`
        // outside quotes as a record terminator by default, but its line
        // counter (which the blank-line math above relies on) does not
        // advance for one — desynchronising row counting from record
        // counting and silently losing the blank line, #1936 all over
        // again. `.terminator(Any(b'\n'))` forces every bare `\r` to be
        // ordinary content instead, so none of these split into extra
        // records or drop a row.
        assert_eq!(
            parse("a\rb", ValuesFormat::Csv).unwrap(),
            vec![vec!["a\rb"]]
        );
        assert_eq!(
            parse("a\r\rb", ValuesFormat::Csv).unwrap(),
            vec![vec!["a\r\rb"]]
        );
        assert_eq!(parse("\r", ValuesFormat::Csv).unwrap(), vec![vec!["\r"]]);
        assert_eq!(
            parse("\r\r", ValuesFormat::Csv).unwrap(),
            vec![vec!["\r\r"]]
        );
        assert_eq!(
            parse("\r\r\r", ValuesFormat::Csv).unwrap(),
            vec![vec!["\r\r\r"]]
        );
    }

    #[test]
    fn csv_bare_cr_does_not_swallow_a_real_blank_line() {
        // Before the terminator fix, "x\ny\r\rz\n" parsed the bare `\r\r`
        // as a record boundary invisible to the line counter, dropping the
        // blank line between y and z. With `\r` forced to content, the
        // input's only real line break is the `\n` between y and z... but
        // that `\n` is inside neither a quote nor immediately doubled, so
        // this is one row per `\n`-delimited line, with `\r\r` as content
        // on y's line.
        let rows = parse("x\ny\r\rz\n", ValuesFormat::Csv).unwrap();
        assert_eq!(rows, vec![vec!["x"], vec!["y\r\rz"]]);
    }

    #[test]
    fn csv_blank_line_inside_a_quoted_field_is_content_not_a_row() {
        // Guards against a naive line-split regression: the blank line
        // inside the quotes is part of the first cell's value, not a
        // separate row.
        let rows = parse("\"x\n\ny\",z\n", ValuesFormat::Csv).unwrap();
        assert_eq!(rows, vec![vec!["x\n\ny", "z"]]);
    }

    #[test]
    fn csv_quoted_empty_and_blank_line_agree() {
        // Pins design decision 1: a blank line and an explicit `""` parse
        // to the same one-empty-cell row, so the two ways of writing "no
        // content on this row" can't disagree with each other.
        let rows = parse("\"\"\n\n", ValuesFormat::Csv).unwrap();
        assert_eq!(rows, vec![vec![""], vec![""]]);
        assert_eq!(rows[0], rows[1]);
    }

    #[test]
    fn csv_interior_blank_line_before_an_unterminated_last_row_is_kept() {
        // A blank line immediately before the file's final,
        // newline-less record: the missing-final-newline correction must
        // not eat this genuine blank row.
        let rows = parse("a\n\nb", ValuesFormat::Csv).unwrap();
        assert_eq!(rows, vec![vec!["a"], vec![""], vec!["b"]]);
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
