//! YAML processing utilities

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Serialize data structure to YAML string
pub fn to_yaml<T: Serialize>(data: &T) -> Result<String> {
    serde_yaml::to_string(data).context("Failed to serialize data to YAML")
}

/// Deserialize YAML string to data structure
pub fn from_yaml<T: for<'de> Deserialize<'de>>(yaml: &str) -> Result<T> {
    serde_yaml::from_str(yaml).context("Failed to deserialize YAML")
}

/// Read and parse YAML file
pub fn read_yaml_file<T: for<'de> Deserialize<'de>, P: AsRef<Path>>(path: P) -> Result<T> {
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read file: {}", path.as_ref().display()))?;

    from_yaml(&content)
}

/// Write data structure to YAML file
pub fn write_yaml_file<T: Serialize, P: AsRef<Path>>(data: &T, path: P) -> Result<()> {
    let yaml_content = to_yaml(data)?;

    fs::write(&path, yaml_content)
        .with_context(|| format!("Failed to write file: {}", path.as_ref().display()))?;

    Ok(())
}
