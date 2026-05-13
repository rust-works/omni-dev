//! `omni-dev completions <shell>` — emit a shell completion script generated
//! from the live clap command tree.

use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::{generate, Shell};

use crate::cli::Cli;

/// Shell-completion-script generator subcommand.
#[derive(Parser)]
pub struct CompletionsCommand {
    /// Target shell (`bash`, `elvish`, `fish`, `powershell`, `zsh`).
    #[arg(value_enum)]
    pub shell: Shell,
}

impl CompletionsCommand {
    /// Writes the completion script for `self.shell` to stdout.
    pub fn execute(self) -> Result<()> {
        let mut cmd = Cli::command();
        generate(self.shell, &mut cmd, "omni-dev", &mut std::io::stdout());
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cli::Commands;

    fn parse_shell(arg: &str) -> Shell {
        let cli = Cli::try_parse_from(["omni-dev", "completions", arg]).unwrap();
        match cli.command {
            Commands::Completions(c) => c.shell,
            _ => panic!("expected completions subcommand"),
        }
    }

    #[test]
    fn clap_parses_completions_bash() {
        assert_eq!(parse_shell("bash"), Shell::Bash);
    }

    #[test]
    fn clap_parses_completions_zsh() {
        assert_eq!(parse_shell("zsh"), Shell::Zsh);
    }

    #[test]
    fn clap_parses_completions_fish() {
        assert_eq!(parse_shell("fish"), Shell::Fish);
    }

    #[test]
    fn clap_parses_completions_powershell() {
        assert_eq!(parse_shell("powershell"), Shell::PowerShell);
    }

    #[test]
    fn clap_parses_completions_elvish() {
        assert_eq!(parse_shell("elvish"), Shell::Elvish);
    }

    #[test]
    fn clap_rejects_unknown_shell() {
        let result = Cli::try_parse_from(["omni-dev", "completions", "banana"]);
        assert!(result.is_err());
    }
}
