//! Typed anchored formatting commands.
use crate::cli::drive::format::OutputFormat;
use crate::cli::drive::helpers;
use crate::drive::client::DriveClient;
use crate::drive::docs::client::DocsClient;
use crate::drive::docs::style::{Alignment, NamedStyle, ParagraphStyle, StylePatch, TextStyle};
use crate::drive::docs::write::{WriteOptions, WritePayload};
use anyhow::Result;
use clap::{ArgGroup, Args, Parser};

/// Shared literal anchor selection and write controls.
#[derive(Args)]
#[command(group(ArgGroup::new("selection").required(true).args(["match_text", "from"])))]
pub struct Selection {
    /// Document id.
    pub document_id: String,
    /// One unique literal body match.
    #[arg(long = "match", conflicts_with = "to")]
    pub match_text: Option<String>,
    /// Start of an inclusive anchored range.
    #[arg(long, requires = "to")]
    pub from: Option<String>,
    /// End of an inclusive anchored range.
    #[arg(long, requires = "from")]
    pub to: Option<String>,
    /// Match anchors using Unicode simple case folding.
    #[arg(long)]
    pub ignore_case: bool,
    /// Report the resolved effect without writing.
    #[arg(long)]
    pub dry_run: bool,
    #[command(flatten)]
    pub lease: helpers::LeaseTokenArg,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

/// Sets explicit character style properties under docs-format.
#[derive(Parser)]
#[command(group(ArgGroup::new("style").required(true).multiple(true).args(["bold", "italic", "underline", "strikethrough"])))]
pub struct TextStyleCommand {
    #[command(flatten)]
    pub selection: Selection,
    /// Set bold explicitly (true or false).
    #[arg(long, action = clap::ArgAction::Set)]
    pub bold: Option<bool>,
    /// Set italic explicitly (true or false).
    #[arg(long, action = clap::ArgAction::Set)]
    pub italic: Option<bool>,
    /// Set underline explicitly (true or false).
    #[arg(long, action = clap::ArgAction::Set)]
    pub underline: Option<bool>,
    /// Set strikethrough explicitly (true or false).
    #[arg(long, action = clap::ArgAction::Set)]
    pub strikethrough: Option<bool>,
}

/// Sets style on every whole paragraph touched by the anchored range.
#[derive(Parser)]
#[command(group(ArgGroup::new("style").required(true).multiple(true).args(["alignment", "named_style"])))]
pub struct ParagraphStyleCommand {
    #[command(flatten)]
    pub selection: Selection,
    /// Set paragraph alignment.
    #[arg(long, value_enum)]
    pub alignment: Option<Alignment>,
    /// Set a named paragraph style; may change inherited formatting.
    #[arg(long, value_enum)]
    pub named_style: Option<NamedStyle>,
}

impl TextStyleCommand {
    /// Run through the shared gated write engine.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        self.selection
            .execute(
                client,
                StylePatch::Text(TextStyle {
                    bold: self.bold,
                    italic: self.italic,
                    underline: self.underline,
                    strikethrough: self.strikethrough,
                }),
            )
            .await
    }
}
impl ParagraphStyleCommand {
    /// Run through the shared gated write engine.
    pub async fn execute(self, client: &DriveClient) -> Result<()> {
        self.selection
            .execute(
                client,
                StylePatch::Paragraph(ParagraphStyle {
                    alignment: self.alignment,
                    named_style_type: self.named_style,
                }),
            )
            .await
    }
}
impl Selection {
    async fn execute(self, client: &DriveClient, style: StylePatch) -> Result<()> {
        let docs = DocsClient::from_drive_client(client)?;
        let (from, to) = match (self.match_text, self.from, self.to) {
            (Some(text), None, None) => (text, None),
            (None, Some(from), Some(to)) => (from, Some(to)),
            _ => anyhow::bail!("use --match or both --from and --to"),
        };
        let opts = WriteOptions {
            document_id: self.document_id,
            payload: WritePayload::Format {
                from,
                to,
                match_case: !self.ignore_case,
                style,
            },
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    #[test]
    fn formatting_requires_one_selection_and_explicit_style_values() {
        for args in [
            vec!["text-style", "d", "--match", "x"],
            vec!["text-style", "d", "--bold", "true"],
            vec!["text-style", "d", "--from", "x", "--bold", "true"],
            vec![
                "text-style",
                "d",
                "--match",
                "x",
                "--from",
                "y",
                "--to",
                "z",
                "--bold",
                "true",
            ],
            vec!["text-style", "d", "--match", "x", "--bold"],
        ] {
            assert!(TextStyleCommand::try_parse_from(args).is_err());
        }
        let cmd = TextStyleCommand::try_parse_from([
            "text-style",
            "d",
            "--match",
            "x",
            "--bold",
            "false",
            "--italic",
            "true",
        ])
        .unwrap();
        assert_eq!(cmd.bold, Some(false));
        assert_eq!(cmd.italic, Some(true));
        assert!(!cmd.selection.ignore_case);
        assert!(ParagraphStyleCommand::try_parse_from([
            "paragraph-style",
            "d",
            "--match",
            "x",
            "--alignment",
            "bogus"
        ])
        .is_err());
        assert!(ParagraphStyleCommand::try_parse_from([
            "paragraph-style",
            "d",
            "--from",
            "x",
            "--to",
            "y",
            "--named-style",
            "heading1",
            "--alignment",
            "center"
        ])
        .is_ok());
    }
}
