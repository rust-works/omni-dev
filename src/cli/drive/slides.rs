//! Google Slides read and guarded replacement commands.
pub(crate) mod info;
pub(crate) mod read;
pub(crate) mod write;
use crate::drive::client::DriveClient;
use anyhow::Result;
use clap::{Parser, Subcommand};

/// Reads Slides objects and replaces text on ordinary slides.
#[derive(Parser)]
pub struct SlidesCommand {
    /// Slides subcommand.
    #[command(subcommand)]
    pub command: SlidesSubcommands,
}
/// Typed Slides verbs; no arbitrary batch or object deletion.
#[derive(Subcommand)]
pub enum SlidesSubcommands {
    /// Shows title, revision and ordinary slide object IDs.
    Info(info::InfoCommand),
    /// Reads shapes, table cells, groups and speaker notes with object IDs.
    Read(read::ReadCommand),
    /// Replaces literal text on ordinary slides, gated by slides-write and
    /// revision control. Requires drive.file or drive OAuth scope.
    Replace(write::ReplaceCommand),
}
impl SlidesCommand {
    /// Dispatches using the shared Drive OAuth session.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        match self.command {
            SlidesSubcommands::Info(cmd) => cmd.execute(client).await,
            SlidesSubcommands::Read(cmd) => cmd.execute(client).await,
            SlidesSubcommands::Replace(cmd) => cmd.execute(client).await,
        }
    }
}
