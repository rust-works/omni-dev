//! YouTube transcript subcommands.

pub mod fetch;
pub mod info;
pub mod list_langs;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// YouTube: fetch captions, list available languages, and inspect video metadata.
#[derive(Parser)]
pub struct YoutubeCommand {
    /// The YouTube subcommand to execute.
    #[command(subcommand)]
    pub command: YoutubeSubcommands,
}

/// YouTube subcommands.
#[derive(Subcommand)]
pub enum YoutubeSubcommands {
    /// Fetches the transcript for a YouTube video.
    Fetch(fetch::FetchCommand),
    /// Lists the caption tracks available on a YouTube video.
    ListLangs(list_langs::ListLangsCommand),
    /// Shows top-level metadata (title, channel, duration, languages) for a YouTube video.
    Info(info::InfoCommand),
}

impl YoutubeCommand {
    /// Executes the YouTube subcommand.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            YoutubeSubcommands::Fetch(cmd) => cmd.execute().await,
            YoutubeSubcommands::ListLangs(cmd) => cmd.execute().await,
            YoutubeSubcommands::Info(cmd) => cmd.execute().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::transcript::format::CliFormat;

    #[test]
    fn youtube_subcommands_fetch_variant() {
        let cmd = YoutubeCommand {
            command: YoutubeSubcommands::Fetch(fetch::FetchCommand {
                url: "https://youtu.be/abc".to_string(),
                lang: "en".to_string(),
                format: CliFormat::Srt,
                auto: false,
                translate: None,
                output: None,
                client: None,
            }),
        };
        assert!(matches!(cmd.command, YoutubeSubcommands::Fetch(_)));
    }

    #[test]
    fn youtube_subcommands_list_langs_variant() {
        let cmd = YoutubeCommand {
            command: YoutubeSubcommands::ListLangs(list_langs::ListLangsCommand {
                url: "https://youtu.be/abc".to_string(),
                output: list_langs::ListLangsOutput::Table,
                client: None,
            }),
        };
        assert!(matches!(cmd.command, YoutubeSubcommands::ListLangs(_)));
    }

    #[test]
    fn youtube_subcommands_info_variant() {
        let cmd = YoutubeCommand {
            command: YoutubeSubcommands::Info(info::InfoCommand {
                url: "https://youtu.be/abc".to_string(),
                output: info::InfoOutput::Table,
                client: None,
            }),
        };
        assert!(matches!(cmd.command, YoutubeSubcommands::Info(_)));
    }
}
