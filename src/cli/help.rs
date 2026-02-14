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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_generator_default() {
        let gen = HelpGenerator::default();
        assert_eq!(gen.app.get_name(), "omni-dev");
    }

    #[test]
    fn generate_all_help_contains_all_top_level_commands() {
        let gen = HelpGenerator::new();
        let output = gen.generate_all_help().unwrap();
        assert!(output.contains("omni-dev ai"));
        assert!(output.contains("omni-dev git"));
        assert!(output.contains("omni-dev commands"));
        assert!(output.contains("omni-dev config"));
        assert!(output.contains("omni-dev help-all"));
    }

    #[test]
    fn generate_all_help_contains_nested_commands() {
        let gen = HelpGenerator::new();
        let output = gen.generate_all_help().unwrap();
        // Deeply nested commands should be present
        assert!(output.contains("omni-dev git commit message view"));
        assert!(output.contains("omni-dev git commit message amend"));
        assert!(output.contains("omni-dev git commit message twiddle"));
        assert!(output.contains("omni-dev git commit message check"));
        assert!(output.contains("omni-dev git branch info"));
        assert!(output.contains("omni-dev git branch create pr"));
    }

    #[test]
    fn generate_all_help_uses_section_separators() {
        let gen = HelpGenerator::new();
        let output = gen.generate_all_help().unwrap();
        let separator = "=".repeat(80);
        assert!(output.contains(&separator));
    }

    #[test]
    fn generate_all_help_is_deterministic() {
        let gen1 = HelpGenerator::new();
        let gen2 = HelpGenerator::new();
        let output1 = gen1.generate_all_help().unwrap();
        let output2 = gen2.generate_all_help().unwrap();
        assert_eq!(output1, output2, "Help output should be deterministic");
    }

    #[test]
    fn render_command_help_includes_about() {
        let gen = HelpGenerator::new();
        let help = gen.render_command_help(&gen.app, "");
        // The main app help should include the about text
        assert!(help.contains("comprehensive development toolkit"));
    }

    #[test]
    fn styled_str_to_string_plain_text() {
        let gen = HelpGenerator::new();
        let styled = StyledStr::from("hello world");
        let result = gen.styled_str_to_string(&styled);
        assert_eq!(result, "hello world");
    }
}
