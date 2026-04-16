//! CLI command for deleting JIRA issues.

use std::io::{self, BufRead, Write};

use anyhow::Result;
use clap::Parser;

use crate::atlassian::client::AtlassianClient;
use crate::cli::atlassian::helpers::create_client;

/// Deletes a JIRA issue.
#[derive(Parser)]
pub struct DeleteCommand {
    /// JIRA issue key (e.g., PROJ-123).
    pub key: String,

    /// Skips the confirmation prompt.
    #[arg(long)]
    pub force: bool,
}

impl DeleteCommand {
    /// Executes the delete command.
    pub async fn execute(self) -> Result<()> {
        let (client, _instance_url) = create_client()?;

        if !self.force {
            let issue = client.get_issue(&self.key).await?;
            let prompt = format_delete_prompt(&self.key, &issue.summary);
            if !confirm_with_reader(&prompt, &mut io::stdin().lock())? {
                println!("Cancelled.");
                return Ok(());
            }
        }

        run_delete(&client, &self.key).await
    }
}

/// Deletes a JIRA issue.
async fn run_delete(client: &AtlassianClient, key: &str) -> Result<()> {
    client.delete_issue(key).await?;
    println!("Deleted {key}.");
    Ok(())
}

/// Formats the deletion confirmation prompt.
fn format_delete_prompt(key: &str, summary: &str) -> String {
    format!("Delete {key} ({summary})? [y/N] ")
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
            key: "PROJ-42".to_string(),
            force: false,
        };
        assert_eq!(cmd.key, "PROJ-42");
        assert!(!cmd.force);
    }

    #[test]
    fn delete_command_force_mode() {
        let cmd = DeleteCommand {
            key: "PROJ-1".to_string(),
            force: true,
        };
        assert!(cmd.force);
    }

    // ── format_delete_prompt ───────────────────────────────────────

    #[test]
    fn format_prompt_includes_key_and_summary() {
        let prompt = format_delete_prompt("PROJ-123", "Fix the bug");
        assert_eq!(prompt, "Delete PROJ-123 (Fix the bug)? [y/N] ");
    }

    #[test]
    fn format_prompt_with_empty_summary() {
        let prompt = format_delete_prompt("PROJ-1", "");
        assert_eq!(prompt, "Delete PROJ-1 ()? [y/N] ");
    }

    #[test]
    fn format_prompt_with_special_chars() {
        let prompt = format_delete_prompt("PROJ-99", "Fix \"quotes\" & <angles>");
        assert!(prompt.contains("PROJ-99"));
        assert!(prompt.contains("Fix \"quotes\" & <angles>"));
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
