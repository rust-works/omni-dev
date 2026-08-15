//! CLI command for `omni-dev drive account list`.

use std::io::Write;

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::drive::format::{output_as, OutputFormat};
use crate::drive::account::{self, AccountSummary};
use crate::utils::settings::Settings;

/// Lists configured Drive accounts.
#[derive(Parser)]
pub struct ListCommand {
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = OutputFormat::Table)]
    pub output: OutputFormat,
}

impl ListCommand {
    /// Reads accounts from `~/.omni-dev/settings.json` only — no network
    /// call, no secret ever rendered.
    pub fn execute(self) -> Result<()> {
        let settings = Settings::load().unwrap_or_default();
        let accounts = account::list_accounts(&settings.drive);
        run_list(&accounts, &self.output)
    }
}

/// Emits `accounts` in the requested format. Split from
/// [`ListCommand::execute`] so tests can exercise rendering directly
/// against a constructed list, without touching `HOME`.
fn run_list(accounts: &[AccountSummary], output: &OutputFormat) -> Result<()> {
    if output_as(&accounts.to_vec(), output)? {
        return Ok(());
    }
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    render_account_table(accounts, &mut handle)
}

/// Renders accounts as an aligned text table: `NAME | EMAIL | SCOPE |
/// DEFAULT`. An empty input prints a message pointing at `drive auth
/// login --account <name>`.
fn render_account_table(accounts: &[AccountSummary], out: &mut dyn Write) -> Result<()> {
    if accounts.is_empty() {
        writeln!(
            out,
            "No named Drive accounts configured. Run `omni-dev drive auth login --account <name>` \
             to create one."
        )
        .context("Failed to write empty-table message")?;
        return Ok(());
    }

    let name_width = "NAME"
        .len()
        .max(accounts.iter().map(|a| a.name.len()).max().unwrap_or(0));
    let email_strings: Vec<&str> = accounts
        .iter()
        .map(|a| a.email_address.as_deref().unwrap_or("-"))
        .collect();
    let email_width = "EMAIL"
        .len()
        .max(email_strings.iter().map(|s| s.len()).max().unwrap_or(0));
    let scope_strings: Vec<&str> = accounts
        .iter()
        .map(|a| a.scope.as_deref().unwrap_or("-"))
        .collect();
    let scope_width = "SCOPE"
        .len()
        .max(scope_strings.iter().map(|s| s.len()).max().unwrap_or(0));

    writeln!(
        out,
        "{:<name_width$}  {:<email_width$}  {:<scope_width$}  DEFAULT",
        "NAME", "EMAIL", "SCOPE"
    )
    .context("Failed to write header row")?;
    for (i, account) in accounts.iter().enumerate() {
        writeln!(
            out,
            "{:<name_width$}  {:<email_width$}  {:<scope_width$}  {}",
            account.name,
            email_strings[i],
            scope_strings[i],
            if account.is_default { "*" } else { "" },
        )
        .context("Failed to write account row")?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sample(name: &str, is_default: bool) -> AccountSummary {
        AccountSummary {
            name: name.to_string(),
            email_address: Some(format!("{name}@example.com")),
            scope: Some("readonly".to_string()),
            is_default,
        }
    }

    #[test]
    fn render_table_empty_points_at_login() {
        let mut buf = Vec::new();
        render_account_table(&[], &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("drive auth login --account"));
    }

    #[test]
    fn render_table_writes_header_and_marks_default() {
        let accounts = [sample("work", true), sample("personal", false)];
        let mut buf = Vec::new();
        render_account_table(&accounts, &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains("NAME"));
        assert!(out.contains("EMAIL"));
        assert!(out.contains("SCOPE"));
        assert!(out.contains("DEFAULT"));
        assert!(out.contains("work"));
        assert!(out.contains("personal"));
        let work_line = out
            .lines()
            .find(|l| l.contains("work@example.com"))
            .unwrap();
        assert!(work_line.trim_end().ends_with('*'));
        let personal_line = out
            .lines()
            .find(|l| l.contains("personal@example.com"))
            .unwrap();
        assert!(!personal_line.trim_end().ends_with('*'));
    }

    #[test]
    fn render_table_uses_dash_for_missing_email_and_scope() {
        let account = AccountSummary {
            name: "bare".to_string(),
            email_address: None,
            scope: None,
            is_default: false,
        };
        let mut buf = Vec::new();
        render_account_table(&[account], &mut buf).unwrap();
        let out = String::from_utf8(buf).unwrap();
        assert!(out.contains('-'));
    }

    #[test]
    fn run_list_table_path_writes_to_stdout() {
        run_list(&[sample("work", true)], &OutputFormat::Table).unwrap();
    }

    #[test]
    fn run_list_json_path_returns_ok() {
        run_list(&[sample("work", true)], &OutputFormat::Json).unwrap();
    }
}
