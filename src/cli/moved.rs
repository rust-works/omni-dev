//! Hidden `omni-dev gmail` / `omni-dev drive` stubs.
//!
//! The Gmail and Drive command trees (Drive includes Docs, Sheets and Slides)
//! moved to [rust-works/gwi](https://github.com/rust-works/gwi) (#2203,
//! [ADR-0095](../../docs/adrs/adr-0095.md)). For one release the old command
//! names stay behind as hidden stubs that accept any arguments, do nothing and
//! exit non-zero with a message naming the replacement, so a script or muscle
//! memory fails with directions instead of "unrecognized subcommand". The
//! following minor release deletes the stubs.

use anyhow::{bail, Result};
use clap::Parser;

/// Install command for gwi, valid before and after its crates.io release.
const GWI_INSTALL: &str = "cargo install --git https://github.com/rust-works/gwi";

/// Arguments of a removed top-level command, accepted and ignored so that
/// every historical invocation (including `--help`) reaches the message.
#[derive(Parser)]
#[command(disable_help_flag = true, disable_help_subcommand = true)]
pub struct MovedCommand {
    /// Ignored; the command has moved.
    #[arg(
        trailing_var_arg = true,
        allow_hyphen_values = true,
        num_args = 0..,
        hide = true
    )]
    args: Vec<String>,
}

impl MovedCommand {
    /// Always fails, telling the user where `command` went.
    pub fn execute(self, command: &str) -> Result<()> {
        bail!("{}", moved_message(command));
    }
}

/// The message printed by a stub for `command` (`gmail` or `drive`).
fn moved_message(command: &str) -> String {
    format!(
        "`omni-dev {command}` has moved to gwi (Google Workspace Interface) and is no longer part of omni-dev.\n\
         \n\
         Install:   {GWI_INSTALL}\n\
         Migrate:   gwi import        (copies your settings and Drive lease ledger from ~/.omni-dev; nothing is moved or deleted)\n\
         Use:       gwi {command} ...   (same subcommands and flags)\n\
         MCP:       the Gmail and Drive tools are served by `gwi-mcp`; add it as its own MCP server\n\
         \n\
         Environment variables read by this command changed from the OMNI_DEV_ prefix to GWI_ (for example \
         OMNI_DEV_{upper}_ACCOUNT is now GWI_{upper}_ACCOUNT); the GMAIL_* and DRIVE_* credential names are unchanged.\n\
         See https://github.com/rust-works/gwi and docs/plan/gwi-extraction.md.",
        upper = command.to_uppercase(),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Commands};

    fn parse(args: &[&str]) -> Cli {
        let mut argv = vec!["omni-dev"];
        argv.extend_from_slice(args);
        Cli::try_parse_from(argv).unwrap()
    }

    #[test]
    fn message_names_the_replacement_and_the_migration_step() {
        let msg = moved_message("gmail");
        assert!(msg.contains("`omni-dev gmail` has moved to gwi"));
        assert!(msg.contains("https://github.com/rust-works/gwi"));
        assert!(msg.contains("gwi import"));
        assert!(msg.contains("gwi gmail ..."));
        assert!(msg.contains("gwi-mcp"));
        assert!(msg.contains("OMNI_DEV_GMAIL_ACCOUNT is now GWI_GMAIL_ACCOUNT"));
    }

    #[test]
    fn drive_message_uses_the_drive_names() {
        let msg = moved_message("drive");
        assert!(msg.contains("`omni-dev drive` has moved to gwi"));
        assert!(msg.contains("OMNI_DEV_DRIVE_ACCOUNT is now GWI_DRIVE_ACCOUNT"));
    }

    #[tokio::test]
    async fn every_historical_invocation_reaches_the_message() {
        for (command, args) in [
            ("gmail", vec!["gmail"]),
            ("gmail", vec!["gmail", "auth", "status"]),
            (
                "gmail",
                vec!["gmail", "--account", "me", "search", "--query", "x"],
            ),
            ("gmail", vec!["gmail", "--help"]),
            (
                "drive",
                vec!["drive", "sheets", "write", "-r", "A1", "--values", "1"],
            ),
            ("drive", vec!["drive", "-h"]),
        ] {
            let err = parse(&args).execute().await.unwrap_err();
            assert!(
                err.to_string()
                    .starts_with(&format!("`omni-dev {command}` has moved to gwi")),
                "{args:?}: {err}"
            );
        }
    }

    #[test]
    fn stubs_are_hidden_from_help() {
        use clap::CommandFactory;
        let cmd = Cli::command();
        for name in ["gmail", "drive"] {
            let sub = cmd.find_subcommand(name).expect("stub is registered");
            assert!(sub.is_hide_set(), "{name} must be hidden");
        }
        assert!(matches!(parse(&["gmail"]).command, Commands::Gmail(_)));
        assert!(matches!(parse(&["drive"]).command, Commands::Drive(_)));
    }
}
