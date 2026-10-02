//! CLI text replacement; permission/revision/lease enforcement is in the engine.
use crate::cli::drive::{
    format::{output_as, sanitize_for_terminal, OutputFormat},
    helpers,
};
use crate::drive::client::DriveClient;
use crate::drive::slides::{
    client::SlidesClient,
    write::{describe, write, WriteOptions},
};
use crate::drive::write_gate::FolderPermissionRule;
use anyhow::Result;
use clap::Parser;

/// Replaces literal text on ordinary slides. Notes/templates are excluded.
#[derive(Parser)]
pub struct ReplaceCommand {
    /// Presentation id (the /d/<ID>/ segment of a Slides URL).
    pub presentation_id: String,
    /// Literal text to find (not a regex).
    #[arg(long)]
    pub search: String,
    /// Replacement text; may be empty to remove matches.
    #[arg(long)]
    pub replace: String,
    /// Match case-insensitively. Matching is case-sensitive by default.
    #[arg(long)]
    pub ignore_case: bool,
    /// Restrict to an ordinary slide object ID. Repeat for multiple slides.
    #[arg(long = "slide", value_name = "OBJECT_ID")]
    pub slides: Vec<String>,
    /// Report snapshot estimates without mutating or consuming a lease.
    #[arg(long)]
    pub dry_run: bool,
    #[command(flatten)]
    pub lease: helpers::LeaseTokenArg,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}
impl ReplaceCommand {
    /// Builds options and delegates to the guarded runner.
    pub async fn execute(self, drive: &DriveClient) -> Result<()> {
        let slides = SlidesClient::from_drive_client(drive)?;
        let opts = WriteOptions {
            presentation_id: self.presentation_id,
            search: self.search,
            replace: self.replace,
            match_case: !self.ignore_case,
            slides: self.slides,
            dry_run: self.dry_run,
            lease_token: self.lease.lease,
            ledger_path: helpers::resolve_ledger_path(self.dry_run)?,
        };
        let rules = helpers::active_account_rules()?;
        run_write(drive, &slides, &opts, &rules, &self.output).await
    }
}
async fn run_write(
    drive: &DriveClient,
    slides: &SlidesClient,
    opts: &WriteOptions,
    rules: &[FolderPermissionRule],
    output: &OutputFormat,
) -> Result<()> {
    let outcome = write(drive, slides, opts, rules).await;
    if !output_as(&outcome, output)? {
        println!("{}", sanitize_for_terminal(&describe(&outcome)));
    }
    Ok(())
}
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    #[test]
    fn defaults_to_case_sensitive_and_accepts_repeated_slide_filters() {
        let cmd = ReplaceCommand::try_parse_from([
            "replace",
            "p",
            "--search",
            "a",
            "--replace",
            "",
            "--slide",
            "s1",
            "--slide",
            "s2",
        ])
        .unwrap();
        assert!(!cmd.ignore_case);
        assert_eq!(cmd.slides, ["s1", "s2"]);
        assert!(
            ReplaceCommand::try_parse_from([
                "replace",
                "p",
                "--search",
                "a",
                "--replace",
                "b",
                "--ignore-case"
            ])
            .unwrap()
            .ignore_case
        );
    }
}
