//! Help command implementation for comprehensive CLI documentation.

use anyhow::Result;
use clap::{builder::StyledStr, Command, CommandFactory, Parser};

/// Help command for displaying comprehensive usage information.
#[derive(Parser)]
pub struct HelpCommand {
    // No subcommands needed - this command shows all help
}

/// Help generator for creating comprehensive CLI documentation.
pub struct HelpGenerator {
    app: Command,
}

impl HelpGenerator {
    /// Creates a new help generator with the current CLI app.
    pub fn new() -> Self {
        use crate::cli::Cli;

        // Build the clap app to get the command structure
        let app = Cli::command();

        Self { app }
    }
}

impl Default for HelpGenerator {
    fn default() -> Self {
        Self::new()
    }
}

impl HelpGenerator {
    /// Generates comprehensive help for all commands.
    pub fn generate_all_help(&self) -> Result<String> {
        let mut help_sections = Vec::new();

        // Add main app help
        let main_help = self.render_command_help(&self.app, "");
        help_sections.push(main_help);

        // Collect help for all subcommands recursively
        self.collect_help_recursive(&self.app, "", &mut help_sections);

        // Join all sections with separators
        let separator = format!("\n\n{}\n\n", "=".repeat(80));
        Ok(help_sections.join(&separator))
    }

    /// Recursively collects help for all subcommands.
    ///
    /// IMPORTANT: Commands are sorted lexicographically to ensure consistent,
    /// predictable output order. This is critical for:
    /// - User experience (predictable command discovery)
    /// - Golden/snapshot tests (deterministic output)
    /// - Documentation generation (stable ordering)
    ///
    /// When adding new commands, ensure this sorting is preserved.
    fn collect_help_recursive(&self, cmd: &Command, prefix: &str, help_sections: &mut Vec<String>) {
        // Collect all subcommands and sort them lexicographically by name
        let mut subcommands: Vec<_> = cmd.get_subcommands().collect();
        subcommands.sort_by(|a, b| a.get_name().cmp(b.get_name()));

        for subcmd in subcommands {
            // Skip the help command itself to avoid infinite recursion
            if subcmd.get_name() == "help" {
                continue;
            }

            let current_path = if prefix.is_empty() {
                subcmd.get_name().to_string()
            } else {
                format!("{} {}", prefix, subcmd.get_name())
            };

            // Render help for this subcommand
            let subcmd_help = self.render_command_help(subcmd, &current_path);
            help_sections.push(subcmd_help);

            // Recursively collect help for nested subcommands (also sorted)
            self.collect_help_recursive(subcmd, &current_path, help_sections);
        }
    }

    /// Renders help for a specific command.
    fn render_command_help(&self, cmd: &Command, path: &str) -> String {
        let mut output = String::new();

        // Command header
        let cmd_name = if path.is_empty() {
            cmd.get_name().to_string()
        } else {
            format!("omni-dev {path}")
        };

        let about = cmd.get_about().map_or_else(
            || "No description available".to_string(),
            |s| self.styled_str_to_string(s),
        );

        output.push_str(&format!("{cmd_name} - {about}\n\n"));

        // Render the actual help content
        let help_str = cmd.clone().render_help();
        output.push_str(&help_str.to_string());

        output
    }

    /// Converts a `StyledStr` to a regular `String` (removes ANSI codes for plain text).
    fn styled_str_to_string(&self, styled: &StyledStr) -> String {
        styled.to_string()
    }
}

impl HelpCommand {
    /// Executes the help command, showing comprehensive help for all commands.
    pub fn execute(self) -> Result<()> {
        let generator = HelpGenerator::new();
        let help_output = generator.generate_all_help()?;
        println!("{help_output}");
        Ok(())
    }
}
