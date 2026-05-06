//! `omni-dev transcript youtube info` — show top-level metadata about a video.

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};

use crate::transcript::source::{MediaInfo, TrackKind, TranscriptSource};
use crate::transcript::sources::youtube::Youtube;

/// Shows top-level metadata (title, channel, duration, languages) for a YouTube video.
#[derive(Parser)]
pub struct InfoCommand {
    /// YouTube video URL or bare 11-character video ID.
    pub url: String,

    /// Output format.
    #[arg(long, value_enum, default_value_t = InfoOutput::Table)]
    pub output: InfoOutput,
}

/// Output format for `info`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum InfoOutput {
    /// Human-readable key/value listing.
    Table,
    /// Pretty-printed JSON of `MediaInfo`.
    Json,
}

impl InfoCommand {
    /// Fetches the metadata and prints it.
    pub async fn execute(self) -> Result<()> {
        let yt = Youtube::new()?;
        let info = yt.info(&self.url).await?;
        match self.output {
            InfoOutput::Table => print_table(&info),
            InfoOutput::Json => print_json(&info)?,
        }
        Ok(())
    }
}

fn print_table(info: &MediaInfo) {
    println!("Source:    {}", info.source);
    println!("ID:        {}", info.locator_id);
    println!("Title:     {}", info.title);
    if let Some(author) = &info.author {
        println!("Channel:   {author}");
    }
    if let Some(duration_ms) = info.duration_ms {
        println!("Duration:  {}", format_duration(duration_ms));
    }
    if info.languages.is_empty() {
        println!("Languages: (none)");
    } else {
        println!("Languages:");
        for lang in &info.languages {
            println!("  - {} [{}] {}", lang.code, kind_str(lang.kind), lang.name);
        }
    }
}

fn print_json(info: &MediaInfo) -> Result<()> {
    let json =
        serde_json::to_string_pretty(info).context("Failed to serialize MediaInfo as JSON")?;
    println!("{json}");
    Ok(())
}

fn kind_str(kind: TrackKind) -> &'static str {
    match kind {
        TrackKind::Manual => "manual",
        TrackKind::Auto => "auto",
        TrackKind::Translated => "translated",
    }
}

fn format_duration(ms: u64) -> String {
    let total_secs = ms / 1_000;
    let hours = total_secs / 3_600;
    let minutes = (total_secs % 3_600) / 60;
    let seconds = total_secs % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::transcript::source::LanguageInfo;
    use clap::{CommandFactory, FromArgMatches};

    fn parse(args: &[&str]) -> InfoCommand {
        let cmd = InfoCommand::command().no_binary_name(true);
        let matches = cmd.try_get_matches_from(args).unwrap();
        InfoCommand::from_arg_matches(&matches).unwrap()
    }

    #[test]
    fn info_command_defaults() {
        let cmd = parse(&["abc"]);
        assert_eq!(cmd.url, "abc");
        assert_eq!(cmd.output, InfoOutput::Table);
    }

    #[test]
    fn info_command_json_output() {
        let cmd = parse(&["abc", "--output", "json"]);
        assert_eq!(cmd.output, InfoOutput::Json);
    }

    #[test]
    fn format_duration_under_an_hour() {
        assert_eq!(format_duration(0), "00:00");
        assert_eq!(format_duration(59_000), "00:59");
        assert_eq!(format_duration(212_000), "03:32");
    }

    #[test]
    fn format_duration_over_an_hour() {
        assert_eq!(format_duration(3_600_000), "01:00:00");
        assert_eq!(format_duration(3_661_000), "01:01:01");
    }

    #[test]
    fn print_table_handles_full_info() {
        let info = MediaInfo {
            source: "youtube".into(),
            locator_id: "abc".into(),
            title: "Title".into(),
            author: Some("Channel".into()),
            duration_ms: Some(60_000),
            languages: vec![LanguageInfo {
                code: "en".into(),
                name: "English".into(),
                kind: TrackKind::Manual,
            }],
        };
        print_table(&info);
    }

    #[test]
    fn print_table_handles_minimal_info() {
        let info = MediaInfo {
            source: "youtube".into(),
            locator_id: "abc".into(),
            title: "Title".into(),
            author: None,
            duration_ms: None,
            languages: vec![],
        };
        print_table(&info);
    }

    #[test]
    fn print_json_round_trips() {
        let info = MediaInfo {
            source: "youtube".into(),
            locator_id: "abc".into(),
            title: "Title".into(),
            author: None,
            duration_ms: None,
            languages: vec![],
        };
        print_json(&info).unwrap();
    }
}
