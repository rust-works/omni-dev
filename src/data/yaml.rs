//! YAML processing utilities

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
use yaml_rust_davvid::YamlEmitter;

/// Serialize data structure to YAML string with proper multi-line formatting
pub fn to_yaml<T: Serialize>(data: &T) -> Result<String> {
    // First convert to serde_yaml::Value, then to yaml-rust format
    let serde_value = serde_yaml::to_value(data).context("Failed to serialize to serde value")?;
    let yaml_rust_value = convert_serde_to_yaml_rust(&serde_value)?;

    // Use yaml-rust emitter with multiline strings enabled
    let mut output = String::new();
    let mut emitter = YamlEmitter::new(&mut output);
    emitter.multiline_strings(true);
    emitter
        .dump(&yaml_rust_value)
        .context("Failed to emit YAML")?;

    Ok(output)
}

/// Convert serde_yaml::Value to yaml_rust_davvid::Yaml
fn convert_serde_to_yaml_rust(value: &serde_yaml::Value) -> Result<yaml_rust_davvid::Yaml> {
    use yaml_rust_davvid::Yaml;

    match value {
        serde_yaml::Value::Null => Ok(Yaml::Null),
        serde_yaml::Value::Bool(b) => Ok(Yaml::Boolean(*b)),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Yaml::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(Yaml::Real(f.to_string()))
            } else {
                Ok(Yaml::String(n.to_string()))
            }
        }
        serde_yaml::Value::String(s) => {
            // For now, just convert normally - yaml-rust will make formatting decisions
            Ok(Yaml::String(s.clone()))
        }
        serde_yaml::Value::Sequence(seq) => {
            let yaml_seq: Result<Vec<_>> = seq.iter().map(convert_serde_to_yaml_rust).collect();
            Ok(Yaml::Array(yaml_seq?))
        }
        serde_yaml::Value::Mapping(map) => {
            let mut yaml_map = yaml_rust_davvid::yaml::Hash::new();
            for (k, v) in map {
                let yaml_key = convert_serde_to_yaml_rust(k)?;
                let yaml_value = convert_serde_to_yaml_rust(v)?;
                yaml_map.insert(yaml_key, yaml_value);
            }
            Ok(Yaml::Hash(yaml_map))
        }
        serde_yaml::Value::Tagged(tagged) => {
            // Handle tagged values by converting the inner value
            convert_serde_to_yaml_rust(&tagged.value)
        }
    }
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
