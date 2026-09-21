//! CLI command for `omni-dev drive sheets auto-fill` (issue #1840,
//! [ADR-0083](../../../../../docs/adrs/adr-0083.md) §1).
//!
//! Gated by
//! [`DriveOperation::SheetsWrite`](crate::drive::write_gate::DriveOperation::SheetsWrite)
//! alone — see that variant's doc comment and `auto_fill.rs`'s module docs
//! for the live-verification item that could move it to a two-operation
//! gate.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::cli::drive::sheets::format::DimensionArg;
use crate::drive::client::DriveClient;
use crate::drive::sheets::auto_fill::{auto_fill, describe_lines, AutoFillForm, AutoFillOptions};
use crate::drive::sheets::client::SheetsClient;

/// Extends a series from source cells into an adjacent destination, using
/// Sheets' own pattern-detection heuristics (dates, numbers, days-of-week,
/// or whatever pattern the source cells show).
///
/// Exactly one of `--range`/`--source` is required. `--range` names the
/// whole region and lets Sheets decide for itself which cells are the
/// source and which are filled; `--source`/`--dimension`/`--fill-length`
/// names the source explicitly and computes the destination locally.
/// **The filled values can never be previewed** — they are computed
/// server-side by Sheets' own series detection, which this crate cannot
/// reproduce locally, before or after the request. `--dry-run` (and the
/// real run) instead report the destination range and the count and A1
/// locations of the non-blank cells within it that would be (or were)
/// overwritten — never their values.
#[derive(Parser)]
#[command(group(clap::ArgGroup::new("input_form")
    .args(["range", "source"])
    .required(true)))]
pub struct AutoFillCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`/
    /// `--source`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Form A: the whole region, optionally carrying its own `Sheet!`
    /// prefix. Sheets examines it and decides for itself which cells are
    /// the source and which are filled — so the destination reported by
    /// `--dry-run` is only ever an upper bound on what will be
    /// overwritten.
    #[arg(long, value_name = "A1")]
    pub range: Option<String>,

    /// Form B: the cells holding the series to extend, optionally carrying
    /// its own `Sheet!` prefix. Requires `--dimension` and `--fill-length`.
    #[arg(long, value_name = "A1", requires_all = ["dimension", "fill_length"])]
    pub source: Option<String>,

    /// Form B: which axis `--fill-length` extends `--source` along.
    #[arg(long, value_enum, requires = "source")]
    pub dimension: Option<DimensionArg>,

    /// Form B: how many rows/columns to fill, extending from `--source`'s
    /// edge. Negative fills backward (up or left) instead of forward
    /// (down or right).
    #[arg(
        long,
        value_name = "N",
        requires = "source",
        allow_hyphen_values = true
    )]
    pub fill_length: Option<i64>,

    /// Fills using the alternate series Sheets would not otherwise choose
    /// — e.g. a copy instead of a linear progression for a plain numeric
    /// run, or vice versa.
    #[arg(long)]
    pub alternate_series: bool,

    /// Reports the gate verdict, the destination range, and the count and
    /// A1 locations of the non-blank cells that would be overwritten —
    /// never the values Sheets would fill, which cannot be previewed.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl AutoFillCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let form = match (self.source, self.dimension, self.fill_length) {
            (Some(source), Some(dimension), Some(fill_length)) => {
                AutoFillForm::SourceAndDestination {
                    sheet: self.sheet,
                    source: Some(source),
                    dimension: dimension.engine(),
                    fill_length,
                }
            }
            // The `input_form` `ArgGroup` plus each flag's own `requires`
            // guarantee `--source` never arrives without both
            // `--dimension` and `--fill-length` — this arm is `--range`
            // (or clap has already refused the command).
            _ => AutoFillForm::Range {
                sheet: self.sheet,
                range: self.range,
            },
        };
        let opts = AutoFillOptions {
            spreadsheet_id: self.spreadsheet_id,
            form,
            use_alternate_series: self.alternate_series,
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_auto_fill(client, &opts, &self.output).await
    }
}

async fn run_auto_fill(
    client: &DriveClient,
    opts: &AutoFillOptions,
    output: &OutputFormat,
) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = auto_fill(client, &sheets, opts, &rules).await;
    if output_as(&outcome, output)? {
        return Ok(());
    }
    let lines: Vec<String> = describe_lines(&outcome)
        .into_iter()
        .map(|line| sanitize_for_terminal(&line))
        .collect();
    println!("{}", lines.join("\n"));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn range_and_source_are_mutually_exclusive() {
        let err = AutoFillCommand::command()
            .try_get_matches_from([
                "auto-fill",
                "sheet-1",
                "--range",
                "A1:A2",
                "--source",
                "A1:A2",
                "--dimension",
                "rows",
                "--fill-length",
                "1",
            ])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn one_of_range_or_source_is_required() {
        let err = AutoFillCommand::command()
            .try_get_matches_from(["auto-fill", "sheet-1"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn source_without_dimension_or_fill_length_is_refused() {
        let err = AutoFillCommand::command()
            .try_get_matches_from(["auto-fill", "sheet-1", "--source", "A1:A2"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn a_negative_fill_length_parses() {
        let matches = AutoFillCommand::command()
            .try_get_matches_from([
                "auto-fill",
                "sheet-1",
                "--source",
                "A4:A5",
                "--dimension",
                "rows",
                "--fill-length",
                "-3",
            ])
            .unwrap();
        assert_eq!(matches.get_one::<i64>("fill_length").copied(), Some(-3));
    }

    #[test]
    fn range_form_parses_with_no_dimension_or_fill_length() {
        AutoFillCommand::command()
            .try_get_matches_from(["auto-fill", "sheet-1", "--range", "A1:A10"])
            .unwrap();
    }
}
