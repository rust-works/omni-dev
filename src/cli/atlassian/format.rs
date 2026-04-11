//! Shared format types for Atlassian CLI commands.

use anyhow::{Context, Result};
use clap::ValueEnum;
use serde::Serialize;

/// Output/input format for Atlassian content (read/write/create commands).
#[derive(Clone, Debug, Default, ValueEnum)]
pub enum ContentFormat {
    /// JFM markdown with YAML frontmatter.
    #[default]
    Jfm,
    /// Raw Atlassian Document Format JSON.
    Adf,
}

/// Display format for list/table commands.
#[derive(Clone, Debug, Default, ValueEnum)]
pub enum OutputFormat {
    /// Human-readable table.
    #[default]
    Table,
    /// JSON.
    Json,
    /// YAML.
    Yaml,
}

/// Serializes data in the requested output format.
/// Returns `Ok(true)` if data was printed (json/yaml), `Ok(false)` if the
/// caller should handle table output.
pub fn output_as<T: Serialize>(data: &T, format: &OutputFormat) -> Result<bool> {
    match format {
        OutputFormat::Table => Ok(false),
        OutputFormat::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(data).context("Failed to serialize as JSON")?
            );
            Ok(true)
        }
        OutputFormat::Yaml => {
            print!(
                "{}",
                serde_yaml::to_string(data).context("Failed to serialize as YAML")?
            );
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_jfm() {
        let format = ContentFormat::default();
        assert!(matches!(format, ContentFormat::Jfm));
    }

    #[test]
    fn jfm_variant() {
        let format = ContentFormat::Jfm;
        assert!(matches!(format, ContentFormat::Jfm));
    }

    #[test]
    fn adf_variant() {
        let format = ContentFormat::Adf;
        assert!(matches!(format, ContentFormat::Adf));
    }

    #[test]
    fn debug_format() {
        assert_eq!(format!("{:?}", ContentFormat::Jfm), "Jfm");
        assert_eq!(format!("{:?}", ContentFormat::Adf), "Adf");
    }

    #[test]
    fn clone() {
        let format = ContentFormat::Adf;
        let cloned = format.clone();
        assert!(matches!(cloned, ContentFormat::Adf));
    }

    // ── OutputFormat ───────────────────────────────────────────────

    #[test]
    fn output_default_is_table() {
        assert!(matches!(OutputFormat::default(), OutputFormat::Table));
    }

    #[test]
    fn output_json_variant() {
        assert!(matches!(OutputFormat::Json, OutputFormat::Json));
    }

    #[test]
    fn output_yaml_variant() {
        assert!(matches!(OutputFormat::Yaml, OutputFormat::Yaml));
    }

    // ── output_as ──────────────────────────────────────────────────

    #[test]
    fn output_as_table_returns_false() {
        let data = vec![1, 2, 3];
        assert!(!output_as(&data, &OutputFormat::Table).unwrap());
    }

    #[test]
    fn output_as_json_returns_true() {
        let data = vec![1, 2, 3];
        assert!(output_as(&data, &OutputFormat::Json).unwrap());
    }

    #[test]
    fn output_as_yaml_returns_true() {
        let data = vec![1, 2, 3];
        assert!(output_as(&data, &OutputFormat::Yaml).unwrap());
    }
}
