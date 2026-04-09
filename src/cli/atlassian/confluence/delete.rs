//! CLI command for deleting Confluence pages.

use std::io::{self, BufRead, Write};

use anyhow::Result;
use clap::Parser;

use crate::atlassian::api::AtlassianApi;
use crate::atlassian::confluence_api::ConfluenceApi;
use crate::cli::atlassian::helpers::create_client;

/// Deletes a Confluence page.
#[derive(Parser)]
pub struct DeleteCommand {
    /// Confluence page ID (e.g., 12345678).
    pub id: String,

    /// Skips the confirmation prompt.
    #[arg(long)]
    pub force: bool,
}

impl DeleteCommand {
    /// Executes the delete command.
    pub async fn execute(self) -> Result<()> {
        let (client, instance_url) = create_client()?;
        let api = ConfluenceApi::new(client);

        if !self.force {
            let item = api.get_content(&self.id).await?;
            let prompt = format_delete_prompt(&self.id, &item.title);
            if !confirm_with_reader(&prompt, &mut io::stdin().lock())? {
                println!("Cancelled.");
                return Ok(());
            }
        }

        api.delete_page(&self.id).await?;
        println!("Deleted page {} from {}.", self.id, instance_url);

        Ok(())
    }
}

/// Formats the deletion confirmation prompt.
fn format_delete_prompt(id: &str, title: &str) -> String {
    format!("Delete page {id} ({title})? [y/N] ")
}

/// Prompts the user for confirmation using the given reader for input.
fn confirm_with_reader(prompt: &str, reader: &mut dyn BufRead) -> Result<bool> {
    print!("{prompt}");
    io::stdout().flush()?;

    let mut answer = String::new();
    reader.read_line(&mut answer)?;
    Ok(answer.trim().eq_ignore_ascii_case("y"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // ── DeleteCommand struct ───────────────────────────────────────

    #[test]
    fn delete_command_struct_fields() {
        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: false,
        };
        assert_eq!(cmd.id, "12345");
        assert!(!cmd.force);
    }

    #[test]
    fn delete_command_force_mode() {
        let cmd = DeleteCommand {
            id: "12345".to_string(),
            force: true,
        };
        assert!(cmd.force);
    }

    // ── format_delete_prompt ───────────────────────────────────────

    #[test]
    fn format_prompt_includes_id_and_title() {
        let prompt = format_delete_prompt("12345", "Architecture Overview");
        assert_eq!(prompt, "Delete page 12345 (Architecture Overview)? [y/N] ");
    }

    #[test]
    fn format_prompt_with_empty_title() {
        let prompt = format_delete_prompt("99999", "");
        assert_eq!(prompt, "Delete page 99999 ()? [y/N] ");
    }

    // ── confirm_with_reader ────────────────────────────────────────

    #[test]
    fn confirm_yes_lowercase() {
        let mut input = Cursor::new(b"y\n");
        assert!(confirm_with_reader("Delete? ", &mut input).unwrap());
    }

    #[test]
    fn confirm_yes_uppercase() {
        let mut input = Cursor::new(b"Y\n");
        assert!(confirm_with_reader("Delete? ", &mut input).unwrap());
    }

    #[test]
    fn confirm_no() {
        let mut input = Cursor::new(b"n\n");
        assert!(!confirm_with_reader("Delete? ", &mut input).unwrap());
    }

    #[test]
    fn confirm_empty_is_no() {
        let mut input = Cursor::new(b"\n");
        assert!(!confirm_with_reader("Delete? ", &mut input).unwrap());
    }

    #[test]
    fn confirm_random_text_is_no() {
        let mut input = Cursor::new(b"maybe\n");
        assert!(!confirm_with_reader("Delete? ", &mut input).unwrap());
    }

    #[test]
    fn confirm_yes_with_whitespace() {
        let mut input = Cursor::new(b"  y  \n");
        assert!(confirm_with_reader("Delete? ", &mut input).unwrap());
    }
}
