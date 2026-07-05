//! `omni-dev transcript youtube list-langs` — show all caption tracks on a
//! video.

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};

use crate::transcript::source::{LanguageInfo, TrackKind, TranscriptSource};
use crate::transcript::sources::youtube::Youtube;

/// Lists the caption tracks available on a YouTube video.
#[derive(Parser)]
pub struct ListLangsCommand {
    /// YouTube video URL or bare 11-character video ID.
    pub url: String,

    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = ListLangsOutput::Table)]
    pub output: ListLangsOutput,
}

/// Output format for `list-langs`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum ListLangsOutput {
    /// Human-readable table with `code`, `kind`, `name` columns.
    Table,
    /// Pretty-printed JSON array of `LanguageInfo`.
    Json,
}

impl ListLangsCommand {
    /// Fetches the caption-track list and prints it.
    pub async fn execute(self) -> Result<()> {
        let yt = Youtube::new()?;
        let langs = yt.list_languages(&self.url).await?;
        match self.output {
            ListLangsOutput::Table => print_table(&langs),
            ListLangsOutput::Json => print_json(&langs)?,
        }
        Ok(())
    }
}

fn print_table(langs: &[LanguageInfo]) {
    if langs.is_empty() {
        println!("(no caption tracks available)");
        return;
    }

    let code_width = "code"
        .len()
        .max(langs.iter().map(|l| l.code.len()).max().unwrap_or(0));
    let kind_width = "kind".len().max(
        langs
            .iter()
            .map(|l| kind_str(l.kind).len())
            .max()
            .unwrap_or(0),
    );

    println!(
        "{:<code_w$}  {:<kind_w$}  name",
        "code",
        "kind",
        code_w = code_width,
        kind_w = kind_width,
    );
    println!(
        "{:-<code_w$}  {:-<kind_w$}  {:-<name_w$}",
        "",
        "",
        "",
        code_w = code_width,
        kind_w = kind_width,
        name_w = "name".len(),
    );
    for lang in langs {
        println!(
            "{:<code_w$}  {:<kind_w$}  {}",
            lang.code,
            kind_str(lang.kind),
            lang.name,
            code_w = code_width,
            kind_w = kind_width,
        );
    }
}

fn print_json(langs: &[LanguageInfo]) -> Result<()> {
    let json =
        serde_json::to_string_pretty(langs).context("Failed to serialize languages as JSON")?;
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use clap::{CommandFactory, FromArgMatches};

    fn parse(args: &[&str]) -> ListLangsCommand {
        let cmd = ListLangsCommand::command().no_binary_name(true);
        let matches = cmd.try_get_matches_from(args).unwrap();
        ListLangsCommand::from_arg_matches(&matches).unwrap()
    }

    #[test]
    fn list_langs_command_defaults() {
        let cmd = parse(&["abc"]);
        assert_eq!(cmd.url, "abc");
        assert_eq!(cmd.output, ListLangsOutput::Table);
    }

    #[test]
    fn list_langs_command_json_output() {
        let cmd = parse(&["abc", "--output", "json"]);
        assert_eq!(cmd.output, ListLangsOutput::Json);
    }

    #[test]
    fn kind_str_lowercase() {
        assert_eq!(kind_str(TrackKind::Manual), "manual");
        assert_eq!(kind_str(TrackKind::Auto), "auto");
        assert_eq!(kind_str(TrackKind::Translated), "translated");
    }

    #[test]
    fn print_json_round_trips() {
        // Sanity check that LanguageInfo serializes to JSON without error.
        let langs = vec![LanguageInfo {
            code: "en".into(),
            name: "English".into(),
            kind: TrackKind::Manual,
        }];
        print_json(&langs).unwrap();
    }

    #[test]
    fn print_table_handles_empty_input() {
        // No panic on an empty language list.
        print_table(&[]);
    }

    #[test]
    fn print_table_handles_populated_input() {
        let langs = vec![
            LanguageInfo {
                code: "en".into(),
                name: "English".into(),
                kind: TrackKind::Manual,
            },
            LanguageInfo {
                code: "es-419".into(),
                name: "Spanish (Latin America)".into(),
                kind: TrackKind::Auto,
            },
        ];
        print_table(&langs);
    }
}
