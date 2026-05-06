//! Configuration-related CLI commands.

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::claude::model_config::{get_model_registry, ModelSource, MODELS_YAML};

/// Configuration operations.
#[derive(Parser)]
pub struct ConfigCommand {
    /// Configuration subcommand to execute.
    #[command(subcommand)]
    pub command: ConfigSubcommands,
}

/// Configuration subcommands.
#[derive(Subcommand)]
pub enum ConfigSubcommands {
    /// AI model configuration and information.
    Models(ModelsCommand),
}

/// Models operations.
#[derive(Parser)]
pub struct ModelsCommand {
    /// Models subcommand to execute.
    #[command(subcommand)]
    pub command: ModelsSubcommands,
}

/// Models subcommands.
#[derive(Subcommand)]
pub enum ModelsSubcommands {
    /// Shows the model catalog (merged user/project layers over the
    /// embedded `models.yaml`), annotating each entry with its source layer.
    Show(ShowCommand),
}

/// Show command options.
#[derive(Parser)]
pub struct ShowCommand {
    /// Show only the embedded `models.yaml` verbatim, ignoring any
    /// user/project overrides.
    #[arg(long)]
    pub embedded_only: bool,
}

impl ConfigCommand {
    /// Executes the config command.
    pub fn execute(self) -> Result<()> {
        match self.command {
            ConfigSubcommands::Models(models_cmd) => models_cmd.execute(),
        }
    }
}

impl ModelsCommand {
    /// Executes the models command.
    pub fn execute(self) -> Result<()> {
        match self.command {
            ModelsSubcommands::Show(show_cmd) => show_cmd.execute(),
        }
    }
}

impl ShowCommand {
    /// Executes the show command.
    pub fn execute(self) -> Result<()> {
        if self.embedded_only {
            print!("{MODELS_YAML}");
            return Ok(());
        }

        let registry = get_model_registry();
        let yaml = render_merged_yaml(registry.config())?;
        print!("{yaml}");
        Ok(())
    }
}

/// Serialises the merged configuration with each model and provider entry
/// carrying a `source: embedded|user|project|override` field. Returns the
/// rendered YAML text.
fn render_merged_yaml(config: &crate::claude::model_config::ModelConfiguration) -> Result<String> {
    let yaml = serde_yaml::to_string(config)?;
    Ok(prepend_layer_summary(&yaml, config))
}

fn prepend_layer_summary(
    yaml: &str,
    config: &crate::claude::model_config::ModelConfiguration,
) -> String {
    let mut counts: std::collections::BTreeMap<ModelSource, usize> =
        std::collections::BTreeMap::new();
    for spec in &config.models {
        *counts.entry(spec.source).or_default() += 1;
    }

    let mut header = String::new();
    header.push_str("# Merged model catalog (project > user > embedded).\n");
    header.push_str("# Each entry's `source:` field indicates the layer that contributed it.\n");
    header.push_str("# Models by source: ");
    let parts: Vec<String> = counts.iter().map(|(s, n)| format!("{s}={n}")).collect();
    if parts.is_empty() {
        header.push_str("(none)");
    } else {
        header.push_str(&parts.join(", "));
    }
    header.push_str(".\n#\n");

    let mut out = header;
    out.push_str(yaml);
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::claude::model_config::ModelRegistry;
    use std::io::Write;
    use std::path::Path;

    fn write(dir: &Path, name: &str, contents: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::File::create(&path)
            .unwrap()
            .write_all(contents.as_bytes())
            .unwrap();
        path
    }

    #[test]
    fn rendered_yaml_includes_source_for_each_entry() {
        let dir = tempfile::tempdir().unwrap();
        let user = write(
            dir.path(),
            "user.yaml",
            r#"
version: "1"
models:
  - provider: "claude"
    model: "Custom"
    api_identifier: "claude-custom-x"
    max_output_tokens: 1
    input_context: 1
    generation: 1.0
    tier: "flagship"
"#,
        );

        let registry = ModelRegistry::load_layered_from_paths(None, Some(&user), None).unwrap();
        let yaml = render_merged_yaml(registry.config()).unwrap();

        // Header summary mentions both layers.
        assert!(yaml.contains("Merged model catalog"));
        assert!(yaml.contains("embedded="));
        assert!(yaml.contains("user="));

        // Source field is present for the user-added entry…
        assert!(yaml.contains("api_identifier: claude-custom-x"));
        assert!(yaml.contains("source: user"));
        // …and for embedded entries.
        assert!(yaml.contains("source: embedded"));
    }

    #[test]
    fn embedded_only_flag_round_trips_embedded_yaml() {
        let cmd = ShowCommand {
            embedded_only: true,
        };
        // execute() prints to stdout; we just confirm it does not error and
        // that the underlying constant is what `--embedded-only` would emit.
        cmd.execute().unwrap();
        assert!(MODELS_YAML.contains("version: \"1\""));
    }

    #[test]
    fn layer_summary_handles_empty_models() {
        let config = crate::claude::model_config::ModelConfiguration {
            version: Some("1".into()),
            models: Vec::new(),
            providers: std::collections::HashMap::new(),
        };
        let summary = prepend_layer_summary("", &config);
        assert!(summary.contains("Models by source: (none)"));
    }
}
