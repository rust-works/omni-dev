//! Transcript and caption fetching from media platforms.
//!
//! Provider-namespaced: each source (YouTube today; Vimeo, podcast feeds, …
//! later) lives under its own subcommand so per-source argument shapes and
//! help text stay clean.

pub mod fetch;
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

/// Transcript subcommands: a provider-less auto-detecting `fetch`, plus one
/// namespace per media platform.
#[derive(Subcommand)]
pub enum TranscriptSubcommands {
    /// Fetch a transcript, auto-detecting the source from the locator.
    Fetch(fetch::FetchCommand),
    /// YouTube: fetch captions, list available languages, and inspect video metadata.
    Youtube(youtube::YoutubeCommand),
}

impl TranscriptCommand {
    /// Dispatches to the selected provider.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            TranscriptSubcommands::Fetch(cmd) => cmd.execute().await,
            TranscriptSubcommands::Youtube(cmd) => cmd.execute().await,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cli::transcript::format::CliFormat;

    #[test]
    fn transcript_subcommands_fetch_variant() {
        let cmd = TranscriptCommand {
            command: TranscriptSubcommands::Fetch(fetch::FetchCommand {
                url: "https://youtu.be/dQw4w9WgXcQ".to_string(),
                lang: "en".to_string(),
                format: CliFormat::Srt,
                auto: false,
                translate: None,
                output: None,
            }),
        };
        assert!(matches!(cmd.command, TranscriptSubcommands::Fetch(_)));
    }

    /// Drives the `Fetch` dispatch arm end-to-end: an unrecognised locator
    /// fails fast with the typed `InvalidLocator` error before any network
    /// call — exercising `TranscriptCommand::execute` → `FetchCommand::execute`
    /// → `detect`.
    #[tokio::test]
    async fn fetch_dispatch_surfaces_unrecognised_locator() {
        let cmd = TranscriptCommand {
            command: TranscriptSubcommands::Fetch(fetch::FetchCommand {
                url: "https://vimeo.com/76979871".to_string(),
                lang: "en".to_string(),
                format: CliFormat::Srt,
                auto: false,
                translate: None,
                output: None,
            }),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(err.to_string().contains("invalid transcript locator"));
    }

    #[test]
    fn transcript_subcommands_youtube_variant() {
        let cmd = TranscriptCommand {
            command: TranscriptSubcommands::Youtube(youtube::YoutubeCommand {
                command: youtube::YoutubeSubcommands::Info(youtube::info::InfoCommand {
                    url: "https://youtu.be/dQw4w9WgXcQ".to_string(),
                    output: youtube::info::InfoOutput::Table,
                }),
            }),
        };
        assert!(matches!(cmd.command, TranscriptSubcommands::Youtube(_)));
    }
}
