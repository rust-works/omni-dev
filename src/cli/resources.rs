//! `omni-dev resources` — embedded reference content (specs, etc.) shared
//! with the MCP `omni-dev://specs/{name}` resource family.

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};

use crate::resources;

/// Embedded resource operations.
#[derive(Parser)]
pub struct ResourcesCommand {
    /// Resources subcommand to execute.
    #[command(subcommand)]
    pub command: ResourcesSubcommands,
}

/// Resources subcommands.
#[derive(Subcommand)]
pub enum ResourcesSubcommands {
    /// Print the raw content of an embedded resource to stdout.
    Show(ShowCommand),
    /// List every embedded resource id, one per line.
    List(ListCommand),
}

/// `omni-dev resources show <id>`.
#[derive(Parser)]
pub struct ShowCommand {
    /// Resource id. Accepts `specs/jfm` or `omni-dev://specs/jfm` (the
    /// `omni-dev://` scheme is stripped before lookup).
    pub id: String,
}

/// `omni-dev resources list`.
#[derive(Parser)]
pub struct ListCommand {}

impl ResourcesCommand {
    /// Executes the resources command.
    pub fn execute(self) -> Result<()> {
        match self.command {
            ResourcesSubcommands::Show(c) => c.execute(),
            ResourcesSubcommands::List(c) => c.execute(),
        }
    }
}

impl ShowCommand {
    /// Executes `resources show`.
    pub fn execute(self) -> Result<()> {
        let canonical = normalize_id(&self.id);
        match resources::get(canonical) {
            Some(r) => {
                // Raw content only — no header. `print!` (not `println!`) so a
                // trailing newline is not appended to content that may not
                // have one.
                print!("{}", r.content);
                Ok(())
            }
            None => Err(anyhow!(
                "unknown resource `{id}`; known: {known}",
                id = self.id,
                known = resources::known_ids_csv(),
            )),
        }
    }
}

impl ListCommand {
    /// Executes `resources list`.
    pub fn execute(self) -> Result<()> {
        for id in resources::ids() {
            println!("{id}");
        }
        Ok(())
    }
}

/// Strips a single leading `omni-dev://` scheme if present, returning the
/// canonical path-style id.
fn normalize_id(input: &str) -> &str {
    input.strip_prefix("omni-dev://").unwrap_or(input)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Commands};

    #[test]
    fn normalize_id_strips_scheme() {
        assert_eq!(normalize_id("omni-dev://specs/jfm"), "specs/jfm");
    }

    #[test]
    fn normalize_id_passes_through_plain() {
        assert_eq!(normalize_id("specs/jfm"), "specs/jfm");
    }

    #[test]
    fn normalize_id_strips_only_one_scheme() {
        // `strip_prefix` removes at most one occurrence.
        assert_eq!(
            normalize_id("omni-dev://omni-dev://specs/jfm"),
            "omni-dev://specs/jfm"
        );
    }

    #[test]
    fn normalize_id_does_not_strip_other_schemes() {
        assert_eq!(normalize_id("jira://issue/X-1"), "jira://issue/X-1");
        assert_eq!(
            normalize_id("git://repo/commits/HEAD"),
            "git://repo/commits/HEAD"
        );
    }

    #[test]
    fn show_unknown_id_errors_with_known_list() {
        let cmd = ShowCommand {
            id: "specs/does-not-exist".into(),
        };
        let err = cmd.execute().expect_err("unknown id must error");
        let chain = format!("{err:#}");
        assert!(
            chain.contains("unknown resource"),
            "missing prefix: {chain}"
        );
        assert!(chain.contains("specs/jfm"), "known list missing: {chain}");
    }

    #[test]
    fn clap_parses_show_command() {
        let cli = Cli::try_parse_from(["omni-dev", "resources", "show", "specs/jfm"]).unwrap();
        match cli.command {
            Commands::Resources(ResourcesCommand {
                command: ResourcesSubcommands::Show(c),
            }) => assert_eq!(c.id, "specs/jfm"),
            _ => panic!("expected resources show"),
        }
    }

    #[test]
    fn clap_parses_list_command() {
        let cli = Cli::try_parse_from(["omni-dev", "resources", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Resources(ResourcesCommand {
                command: ResourcesSubcommands::List(_)
            })
        ));
    }

    #[test]
    fn clap_parses_show_with_omni_dev_uri() {
        let cli =
            Cli::try_parse_from(["omni-dev", "resources", "show", "omni-dev://specs/jfm"]).unwrap();
        match cli.command {
            Commands::Resources(ResourcesCommand {
                command: ResourcesSubcommands::Show(c),
            }) => assert_eq!(c.id, "omni-dev://specs/jfm"),
            _ => panic!("expected resources show"),
        }
    }
}
