//! CLI surface for `omni-dev drive sheets sort-range` (issue #1842).

use anyhow::Result;
use clap::Parser;

use crate::cli::drive::format::{output_as, sanitize_for_terminal, OutputFormat};
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::sheets::client::SheetsClient;
use crate::drive::sheets::sort_range::{describe_lines, sort_range, SortRangeOptions};

/// Reorders the rows in a bounded range by one or more column keys.
///
/// `--sort-by` values use `COLUMN:asc` or `COLUMN:desc` and are applied in
/// the order given. This v1 command sorts by column values only; color-based
/// sort criteria are not yet supported. `--dry-run` does not read cells or
/// imitate Sheets' comparison rules: it reports the request and its record
/// integrity caveats instead.
#[derive(Parser)]
pub struct SortRangeCommand {
    /// Spreadsheet id (the `/d/<ID>/` segment of a Sheets URL).
    pub spreadsheet_id: String,

    /// Sheet (tab) title. Supplies the prefix for a bare `--range`.
    #[arg(long, value_name = "NAME")]
    pub sheet: Option<String>,

    /// Fully bounded A1 range to sort, optionally carrying its own `Sheet!`
    /// prefix (for example `A2:D100`).
    #[arg(long, value_name = "A1")]
    pub range: String,

    /// A column sort key, as `COLUMN:asc` or `COLUMN:desc`. Repeat to add
    /// lower-precedence keys.
    #[arg(long, value_name = "COLUMN:ORDER", required = true)]
    pub sort_by: Vec<String>,

    /// Reports the gate verdict and request shape without calling
    /// `spreadsheets.batchUpdate` or reading cell values.
    #[arg(long)]
    pub dry_run: bool,

    #[command(flatten)]
    pub lease: crate::cli::drive::helpers::LeaseTokenArg,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl SortRangeCommand {
    /// Runs the command against the shared Drive client.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let opts = SortRangeOptions {
            spreadsheet_id: self.spreadsheet_id,
            sheet: self.sheet,
            range: Some(self.range),
            sort_by: self.sort_by,
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        let sheets = SheetsClient::from_drive_client(client)?;
        let rules = helpers::active_account_rules()?;
        let outcome = sort_range(client, &sheets, &opts, &rules).await;
        if output_as(&outcome, &self.output)? {
            return Ok(());
        }
        let lines: Vec<String> = describe_lines(&outcome)
            .into_iter()
            .map(|line| sanitize_for_terminal(&line))
            .collect();
        println!("{}", lines.join("\n"));
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn requires_a_sort_key() {
        let err = SortRangeCommand::command()
            .try_get_matches_from(["sort-range", "sheet-1", "--range", "A1:B2"])
            .unwrap_err();
        assert!(err.to_string().contains("--sort-by"));
    }

    #[test]
    fn accepts_multiple_sort_keys() {
        SortRangeCommand::command()
            .try_get_matches_from([
                "sort-range",
                "sheet-1",
                "--range",
                "A1:B2",
                "--sort-by",
                "0:asc",
                "--sort-by",
                "1:desc",
            ])
            .unwrap();
    }
}
