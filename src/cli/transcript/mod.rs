//! Transcript and caption fetching from media platforms.
//!
//! Provider-namespaced: each source (YouTube today; Vimeo, podcast feeds, …
//! later) lives under its own subcommand so per-source argument shapes and
//! help text stay clean.

pub mod format;
pub mod youtube;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Transcript and caption fetching from media platforms.
#[derive(Parser)]
pub struct TranscriptCommand {
    /// The transcript subcommand to execute.
    #[command(subcommand)]
    pub command: TranscriptSubcommands,
}

/// Transcript subcommands, one per media platform.
#[derive(Subcommand)]
pub enum TranscriptSubcommands {
    /// YouTube: fetch captions, list available languages, and inspect video metadata.
    Youtube(youtube::YoutubeCommand),
}

impl TranscriptCommand {
    /// Dispatches to the selected provider.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            TranscriptSubcommands::Youtube(cmd) => cmd.execute().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_subcommands_youtube_variant() {
        let cmd = TranscriptCommand {
            command: TranscriptSubcommands::Youtube(youtube::YoutubeCommand {
                command: youtube::YoutubeSubcommands::Info(youtube::info::InfoCommand {
                    url: "https://youtu.be/dQw4w9WgXcQ".to_string(),
                    output: youtube::info::InfoOutput::Table,
                    client: None,
                }),
            }),
        };
        assert!(matches!(cmd.command, TranscriptSubcommands::Youtube(_)));
    }
}
