//! Typed table CLI; all writes delegate to the existing Docs engine.

use crate::cli::drive::{format::OutputFormat, helpers};
use crate::drive::client::DriveClient;
use crate::drive::docs::{
    anchor::Side,
    client::DocsClient,
    table::{TableEdit, TableVerb},
    write::{WriteOptions, WritePayload},
};
use anyhow::Result;
use clap::{ArgGroup, Args, Parser};

/// Shared target, lease and output options.
#[derive(Args)]
pub struct Common {
    /// Google document id.
    pub document_id: String,
    /// Resolve effects without mutating; does not authorize a subsequent write.
    #[arg(long)]
    pub dry_run: bool,
    /// Match the literal anchor with Unicode simple case folding.
    #[arg(long)]
    pub ignore_case: bool,
    #[command(flatten)]
    pub lease: helpers::LeaseTokenArg,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Insert an empty table in an ordinary top-level body paragraph.
#[derive(Parser)]
#[command(group(ArgGroup::new("anchor").required(true).args(["before", "after"])))]
pub struct InsertTableCommand {
    #[command(flatten)]
    pub common: Common,
    /// Insert before this unique literal body anchor.
    #[arg(long)]
    pub before: Option<String>,
    /// Insert after this unique literal body anchor.
    #[arg(long)]
    pub after: Option<String>,
    /// Positive row count; total grid is limited to 10000 cells.
    #[arg(long)]
    pub rows: i64,
    /// Positive column count.
    #[arg(long)]
    pub columns: i64,
}

/// Insert one empty row/column relative to the cell containing unique text.
#[derive(Parser)]
#[command(group(ArgGroup::new("side").required(true).args(["before", "after"])))]
pub struct InsertDimensionCommand {
    #[command(flatten)]
    pub common: Common,
    /// Unique literal text in a cell of a top-level, unmerged table.
    #[arg(long)]
    pub cell: String,
    /// Insert above the row or left of the column.
    #[arg(long)]
    pub before: bool,
    /// Insert below the row or right of the column.
    #[arg(long)]
    pub after: bool,
}

/// Remove one row/column; the final dimension cannot be removed.
#[derive(Parser)]
pub struct DeleteDimensionCommand {
    #[command(flatten)]
    pub common: Common,
    /// Unique literal text in a cell of a top-level, unmerged table.
    #[arg(long)]
    pub cell: String,
}

impl Common {
    async fn run(self, client: &DriveClient, edit: TableEdit) -> Result<()> {
        let docs = DocsClient::from_drive_client(client)?;
        let opts = WriteOptions {
            document_id: self.document_id,
            payload: WritePayload::Table(edit),
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        super::write::run_write(
            client,
            &docs,
            &opts,
            &helpers::active_account_rules()?,
            &self.output,
        )
        .await
    }
}

impl InsertTableCommand {
    /// Apply through docs-structure.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        let (anchor, side) = match (self.before, self.after) {
            (Some(anchor), None) => (anchor, Side::Before),
            (None, Some(anchor)) => (anchor, Side::After),
            _ => anyhow::bail!("exactly one of --before or --after is required"),
        };
        let edit = TableEdit::Insert {
            anchor,
            side,
            rows: self.rows,
            columns: self.columns,
            match_case: !self.common.ignore_case,
        };
        self.common.run(client, edit).await
    }
}

impl InsertDimensionCommand {
    /// Apply through docs-structure.
    pub async fn execute(self, client: &DriveClient, verb: TableVerb) -> Result<()> {
        let edit = TableEdit::Dimension {
            verb,
            cell: self.cell,
            after: self.after,
            match_case: !self.common.ignore_case,
        };
        self.common.run(client, edit).await
    }
}

impl DeleteDimensionCommand {
    /// Apply through docs-table-delete.
    pub async fn execute(self, client: &DriveClient, verb: TableVerb) -> Result<()> {
        let edit = TableEdit::Dimension {
            verb,
            cell: self.cell,
            after: false,
            match_case: !self.common.ignore_case,
        };
        self.common.run(client, edit).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn insertion_requires_exactly_one_side_and_complete_dimensions() {
        assert!(InsertDimensionCommand::try_parse_from(["row", "d", "--cell", "x"]).is_err());
        assert!(InsertDimensionCommand::try_parse_from([
            "row", "d", "--cell", "x", "--before", "--after"
        ])
        .is_err());
        assert!(
            InsertDimensionCommand::try_parse_from(["row", "d", "--cell", "x", "--after"]).is_ok()
        );
        assert!(
            InsertTableCommand::try_parse_from(["table", "d", "--before", "x", "--rows", "2"])
                .is_err()
        );
        assert!(InsertTableCommand::try_parse_from([
            "table",
            "d",
            "--before",
            "x",
            "--rows",
            "2",
            "--columns",
            "3"
        ])
        .is_ok());
        assert!(
            DeleteDimensionCommand::try_parse_from(["delete", "d", "--cell", "x", "--after"])
                .is_err()
        );
    }
}
