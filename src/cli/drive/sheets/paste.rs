//! CLI commands for `omni-dev drive sheets cut-paste`/`copy-paste`/
//! `paste-data` (issue #1839, [ADR-0083](../../../../docs/adrs/adr-0083.md)
//! §4).
//!
//! `--paste-type` defaults to `normal` on `cut-paste`/`copy-paste` (the
//! API's own default and what "paste" means to a Sheets user) and to
//! `values` on `paste-data` (its delimited-text input carries no formats,
//! merges or validation for `normal` to add) — ADR-0083 §4 fixes both
//! defaults so this module doesn't pick its own. See
//! `crate::drive::sheets::paste`'s module doc for the gate composition per
//! paste type and the curated four-of-seven `PasteType` surface.

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::paste::{describe_lines, paste, read_data_text, PasteOptions, PasteVerb};
use crate::drive::sheets::types::{PasteOrientation, PasteType};

/// The `--paste-type` values this crate models — mirrors [`PasteType`]
/// with a `clap::ValueEnum` derive, since the engine type deliberately
/// carries no `clap` dependency (the same split `banding.rs`'s
/// `BandingAxisArg` uses). Curated: four of the API's seven variants — see
/// [`PasteType`]'s doc comment for the deferred three.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum PasteTypeArg {
    /// Values, formulas, formats and merges.
    Normal,
    /// Cell content only, evaluated formulas rendered to their values.
    Values,
    /// Cell content only, formulas preserved.
    Formula,
    /// The cell format only, excluding data validation.
    Format,
}

impl From<PasteTypeArg> for PasteType {
    fn from(value: PasteTypeArg) -> Self {
        match value {
            PasteTypeArg::Normal => Self::Normal,
            PasteTypeArg::Values => Self::Values,
            PasteTypeArg::Formula => Self::Formula,
            PasteTypeArg::Format => Self::Format,
        }
    }
}

/// `copy-paste`'s `--orientation` — mirrors [`PasteOrientation`].
#[derive(Debug, Clone, Copy, clap::ValueEnum, Default)]
pub enum OrientationArg {
    /// Rows stay rows, columns stay columns (the API's own default).
    #[default]
    Normal,
    /// Rows and columns are swapped.
    Transpose,
}

impl From<OrientationArg> for PasteOrientation {
    fn from(value: OrientationArg) -> Self {
        match value {
            OrientationArg::Normal => Self::Normal,
            OrientationArg::Transpose => Self::Transpose,
        }
    }
}

/// Moves a range to a destination cell, clearing the source. Gated by
/// **both** the `sheets-write` and `sheets-structure` write-permission
/// operations, whatever `--paste-type` names: the source is cleared in
/// full regardless of what is pasted (ADR-0083 §4).
#[derive(Parser)]
pub struct CutPasteCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Default sheet for `--source`/`--destination` when either lacks its
    /// own `Sheet!` prefix.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// The range to move. May carry its own `Sheet!` prefix. Must be a
    /// bounded rectangle.
    #[arg(long, value_name = "[SHEET!]A1_RANGE")]
    pub source: String,

    /// The single-cell destination. May carry its own `Sheet!` prefix.
    #[arg(long, value_name = "[SHEET!]A1")]
    pub destination: String,

    /// What to carry into the destination. The source is cleared in full
    /// either way.
    #[arg(long, value_enum, default_value_t = PasteTypeArg::Normal)]
    pub paste_type: PasteTypeArg,

    /// Reports the gate verdict, the region that would be overwritten at
    /// the destination, and the cells that would be cleared at the
    /// source, without calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl CutPasteCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = PasteOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: PasteVerb::CutPaste {
                sheet: self.sheet,
                source: self.source,
                destination: self.destination,
                paste_type: self.paste_type.into(),
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_paste(client, &opts, &self.output).await
    }
}

/// Copies a range to a destination, spilling a larger source past the
/// destination's end or repeating a smaller one to fill it (the API's own
/// rule; see `crate::drive::sheets::paste::copy_paste_extent`). Gated by
/// `--paste-type` (ADR-0083 §4): a value-only type needs `sheets-write`
/// alone, a presentation-only type `sheets-structure` alone, and `normal`
/// (the default) needs both.
#[derive(Parser)]
pub struct CopyPasteCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Default sheet for `--source`/`--destination` when either lacks its
    /// own `Sheet!` prefix.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// The range to copy from. May carry its own `Sheet!` prefix. Must be
    /// a bounded rectangle.
    #[arg(long, value_name = "[SHEET!]A1_RANGE")]
    pub source: String,

    /// The destination range (a single cell is a valid anchor). May carry
    /// its own `Sheet!` prefix. Must be a bounded rectangle.
    #[arg(long, value_name = "[SHEET!]A1_RANGE")]
    pub destination: String,

    /// What to carry over. `normal` (the default) needs both the
    /// `sheets-write` and `sheets-structure` operations — a bare
    /// `copy-paste` under a `sheets-write`-only grant is refused; pass
    /// `--paste-type values` for a values-only copy that grant already
    /// covers.
    #[arg(long, value_enum, default_value_t = PasteTypeArg::Normal)]
    pub paste_type: PasteTypeArg,

    /// Swaps rows and columns before pasting.
    #[arg(long, value_enum, default_value_t = OrientationArg::Normal)]
    pub orientation: OrientationArg,

    /// Reports the gate verdict and the region that would be overwritten
    /// at the destination, without calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl CopyPasteCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = PasteOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: PasteVerb::CopyPaste {
                sheet: self.sheet,
                source: self.source,
                destination: self.destination,
                paste_type: self.paste_type.into(),
                orientation: self.orientation.into(),
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_paste(client, &opts, &self.output).await
    }
}

/// Pastes delimited text into a range anchored at a destination cell, as
/// if pasted from the clipboard. `delimiter`-form only — the API's `html`
/// alternative is a documented cut (ADR-0083 §4); see
/// `crate::drive::sheets::paste`'s module doc.
#[derive(Parser)]
pub struct PasteDataCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Default sheet for `--destination` when it lacks its own `Sheet!`
    /// prefix.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// The single-cell destination. May carry its own `Sheet!` prefix.
    #[arg(long, value_name = "[SHEET!]A1")]
    pub destination: String,

    /// The delimited text to paste: a local file path, or `-` to read
    /// stdin. Sent verbatim — never parsed into cells locally.
    #[arg(long, value_name = "PATH|-")]
    pub data: String,

    /// The delimiter splitting `--data` into columns.
    #[arg(long, default_value = "\t")]
    pub delimiter: String,

    /// What to carry over. Defaults to `values`, not `normal`: delimited
    /// text carries no formats, merges or validation for `normal` to add.
    /// `normal` is still selectable — the API does not document a normal
    /// paste of delimited text as doing anything beyond values — and
    /// resolves both operations.
    #[arg(long, value_enum, default_value_t = PasteTypeArg::Values)]
    pub paste_type: PasteTypeArg,

    /// Reports the gate verdict and the region that would be overwritten
    /// at the destination, without calling `spreadsheets.batchUpdate`.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl PasteDataCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let data = read_data_text(&self.data)?;
        let opts = PasteOptions {
            spreadsheet_id: self.spreadsheet_id,
            verb: PasteVerb::PasteData {
                sheet: self.sheet,
                destination: self.destination,
                data,
                delimiter: self.delimiter,
                paste_type: self.paste_type.into(),
            },
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        run_paste(client, &opts, &self.output).await
    }
}

async fn run_paste(client: &DriveClient, opts: &PasteOptions, output: &OutputFormat) -> Result<()> {
    let sheets = SheetsClient::from_drive_client(client)?;
    let rules = helpers::active_account_rules()?;
    let outcome = paste(client, &sheets, opts, &rules).await;
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

    #[test]
    fn paste_type_arg_converts_to_the_engine_type() {
        assert!(matches!(
            PasteType::from(PasteTypeArg::Normal),
            PasteType::Normal
        ));
        assert!(matches!(
            PasteType::from(PasteTypeArg::Values),
            PasteType::Values
        ));
        assert!(matches!(
            PasteType::from(PasteTypeArg::Formula),
            PasteType::Formula
        ));
        assert!(matches!(
            PasteType::from(PasteTypeArg::Format),
            PasteType::Format
        ));
    }

    #[test]
    fn orientation_arg_converts_to_the_engine_type() {
        assert!(matches!(
            PasteOrientation::from(OrientationArg::Normal),
            PasteOrientation::Normal
        ));
        assert!(matches!(
            PasteOrientation::from(OrientationArg::Transpose),
            PasteOrientation::Transpose
        ));
    }
}
