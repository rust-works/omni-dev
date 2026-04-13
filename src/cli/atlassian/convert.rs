//! CLI commands for local JFM <-> ADF conversion.

use std::fs;
use std::io::{self, Read};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::atlassian::adf::AdfDocument;
use crate::atlassian::convert::markdown_to_adf;

/// Converts between JFM markdown and ADF JSON.
#[derive(Parser)]
pub struct ConvertCommand {
    /// The conversion direction.
    #[command(subcommand)]
    pub command: ConvertSubcommands,
}

/// Conversion subcommands.
#[derive(Subcommand)]
pub enum ConvertSubcommands {
    /// Converts JFM markdown to ADF JSON.
    #[command(name = "to-adf")]
    ToAdf(ToAdfCommand),
    /// Converts ADF JSON to JFM markdown.
    #[command(name = "from-adf")]
    FromAdf(FromAdfCommand),
}

impl ConvertCommand {
    /// Executes the convert command.
    pub fn execute(self) -> Result<()> {
        match self.command {
            ConvertSubcommands::ToAdf(cmd) => cmd.execute(),
            ConvertSubcommands::FromAdf(cmd) => cmd.execute(),
        }
    }
}

/// Converts JFM markdown to ADF JSON.
#[derive(Parser)]
pub struct ToAdfCommand {
    /// Input file (reads from stdin if omitted or "-").
    pub file: Option<String>,

    /// Outputs compact JSON instead of pretty-printed.
    #[arg(long)]
    pub compact: bool,
}

impl ToAdfCommand {
    /// Reads markdown input and outputs ADF JSON.
    pub fn execute(self) -> Result<()> {
        let input = read_input(self.file.as_deref())?;
        let doc = markdown_to_adf(&input)?;

        let json = if self.compact {
            serde_json::to_string(&doc).context("Failed to serialize ADF JSON")?
        } else {
            serde_json::to_string_pretty(&doc).context("Failed to serialize ADF JSON")?
        };

        println!("{json}");
        Ok(())
    }
}

/// Converts ADF JSON to JFM markdown.
#[derive(Parser)]
pub struct FromAdfCommand {
    /// Input file (reads from stdin if omitted or "-").
    pub file: Option<String>,

    /// Omit localId attributes from output for readability.
    #[arg(long)]
    pub strip_local_ids: bool,
}

impl FromAdfCommand {
    /// Reads ADF JSON and outputs JFM markdown.
    pub fn execute(self) -> Result<()> {
        use crate::atlassian::convert::{adf_to_markdown_with_options, RenderOptions};

        let input = read_input(self.file.as_deref())?;
        let doc: AdfDocument =
            serde_json::from_str(&input).context("Failed to parse ADF JSON input")?;

        let opts = RenderOptions {
            strip_local_ids: self.strip_local_ids,
        };
        let markdown = adf_to_markdown_with_options(&doc, &opts)?;

        print!("{markdown}");
        Ok(())
    }
}

/// Reads input from a file path or stdin.
fn read_input(file: Option<&str>) -> Result<String> {
    match file {
        Some("-") | None => {
            let mut buf = String::new();
            io::stdin()
                .read_to_string(&mut buf)
                .context("Failed to read from stdin")?;
            Ok(buf)
        }
        Some(path) => {
            fs::read_to_string(path).with_context(|| format!("Failed to read file: {path}"))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn read_input_from_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("input.md");
        fs::write(&file_path, "# Hello\n\nBody text").unwrap();

        let content = read_input(Some(file_path.to_str().unwrap())).unwrap();
        assert_eq!(content, "# Hello\n\nBody text");
    }

    #[test]
    fn read_input_missing_file() {
        let result = read_input(Some("/nonexistent/path/file.md"));
        assert!(result.is_err());
    }

    #[test]
    fn to_adf_from_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("input.md");
        fs::write(&file_path, "# Title\n\nParagraph.").unwrap();

        let cmd = ToAdfCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            compact: false,
        };
        assert!(cmd.execute().is_ok());
    }

    #[test]
    fn to_adf_compact_from_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("input.md");
        fs::write(&file_path, "Hello world").unwrap();

        let cmd = ToAdfCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            compact: true,
        };
        assert!(cmd.execute().is_ok());
    }

    #[test]
    fn from_adf_from_file() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("input.json");
        let adf = r#"{"version":1,"type":"doc","content":[{"type":"paragraph","content":[{"type":"text","text":"Hello"}]}]}"#;
        fs::write(&file_path, adf).unwrap();

        let cmd = FromAdfCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            strip_local_ids: false,
        };
        assert!(cmd.execute().is_ok());
    }

    #[test]
    fn from_adf_invalid_json() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("bad.json");
        fs::write(&file_path, "not json").unwrap();

        let cmd = FromAdfCommand {
            file: Some(file_path.to_str().unwrap().to_string()),
            strip_local_ids: false,
        };
        assert!(cmd.execute().is_err());
    }

    #[test]
    fn convert_command_to_adf_dispatch() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("input.md");
        fs::write(&file_path, "# Test").unwrap();

        let cmd = ConvertCommand {
            command: ConvertSubcommands::ToAdf(ToAdfCommand {
                file: Some(file_path.to_str().unwrap().to_string()),
                compact: false,
            }),
        };
        assert!(cmd.execute().is_ok());
    }

    #[test]
    fn convert_command_from_adf_dispatch() {
        let temp_dir = tempfile::tempdir().unwrap();
        let file_path = temp_dir.path().join("input.json");
        let adf = r#"{"version":1,"type":"doc","content":[]}"#;
        fs::write(&file_path, adf).unwrap();

        let cmd = ConvertCommand {
            command: ConvertSubcommands::FromAdf(FromAdfCommand {
                file: Some(file_path.to_str().unwrap().to_string()),
                strip_local_ids: false,
            }),
        };
        assert!(cmd.execute().is_ok());
    }
}
