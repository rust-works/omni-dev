//! YAML processing utilities.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use yaml_rust_davvid::YamlEmitter;

/// Serializes a data structure to a YAML string with proper multi-line formatting.
pub fn to_yaml<T: Serialize>(data: &T) -> Result<String> {
    use tracing::debug;

    debug!("Starting YAML serialization with hybrid approach");

    // First convert to serde_yaml::Value, then to yaml-rust format
    let serde_value = serde_yaml::to_value(data).context("Failed to serialize to serde value")?;
    debug!("Converted to serde_yaml::Value successfully");

    let yaml_rust_value = convert_serde_to_yaml_rust(&serde_value)?;
    debug!("Converted to yaml-rust format successfully");

    // Use yaml-rust emitter with multiline strings enabled
    let mut output = String::new();
    let mut emitter = YamlEmitter::new(&mut output);
    emitter.multiline_strings(true);
    debug!("Created YamlEmitter with multiline_strings(true)");

    emitter
        .dump(&yaml_rust_value)
        .context("Failed to emit YAML")?;

    debug!(
        output_length = output.len(),
        output_preview = %output.lines().take(10).collect::<Vec<_>>().join("\\n"),
        "YAML serialization completed"
    );

    Ok(output)
}

/// Converts a `serde_yaml::Value` to `yaml_rust_davvid::Yaml`.
fn convert_serde_to_yaml_rust(value: &serde_yaml::Value) -> Result<yaml_rust_davvid::Yaml> {
    use tracing::debug;
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
            debug!(
                string_length = s.len(),
                string_preview = %s.lines().take(3).collect::<Vec<_>>().join("\\n"),
                "Converting string value to yaml-rust"
            );
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

/// Deserializes a YAML string to a data structure.
pub fn from_yaml<T: for<'de> Deserialize<'de>>(yaml: &str) -> Result<T> {
    use tracing::debug;

    debug!(
        yaml_length = yaml.len(),
        yaml_preview = %yaml.lines().take(10).collect::<Vec<_>>().join("\\n"),
        "Deserializing YAML using serde_yaml"
    );

    let result = serde_yaml::from_str(yaml).context("Failed to deserialize YAML");

    debug!(
        success = result.is_ok(),
        error = result
            .as_ref()
            .err()
            .map(std::string::ToString::to_string)
            .unwrap_or_default(),
        "YAML deserialization result"
    );

    result
}

/// Reads and parses a YAML file.
pub fn read_yaml_file<T: for<'de> Deserialize<'de>, P: AsRef<Path>>(path: P) -> Result<T> {
    let content = fs::read_to_string(&path)
        .with_context(|| format!("Failed to read file: {}", path.as_ref().display()))?;

    from_yaml(&content)
}

/// Writes a data structure to a YAML file.
pub fn write_yaml_file<T: Serialize, P: AsRef<Path>>(data: &T, path: P) -> Result<()> {
    let yaml_content = to_yaml(data)?;

    fs::write(&path, yaml_content)
        .with_context(|| format!("Failed to write file: {}", path.as_ref().display()))?;

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
    struct TestDiffContent {
        diff_content: String,
        description: String,
    }

    #[test]
    fn multiline_yaml_with_literal_blocks() {
        let test_data = TestDiffContent {
            diff_content: "diff --git a/file.txt b/file.txt\nindex 123..456 100644\n--- a/file.txt\n+++ b/file.txt\n@@ -1,3 +1,3 @@\n-old line\n+new line".to_string(),
            description: "This is a\nmultiline\ndescription".to_string(),
        };

        let yaml_output = to_yaml(&test_data).unwrap();
        println!("YAML Output:\n{yaml_output}");

        // Should use literal block scalar (|) for multiline strings
        assert!(yaml_output.contains("diff_content: |"));
        assert!(yaml_output.contains("description: |"));

        // Should contain the actual content without escaped newlines
        assert!(yaml_output.contains("diff --git"));
        assert!(yaml_output.contains("--- a/file.txt"));
        assert!(yaml_output.contains("+++ b/file.txt"));

        // Should not contain escaped newlines in the output
        assert!(!yaml_output.contains("\\n"));

        // Should round-trip correctly (accounting for trailing newlines added by literal blocks)
        let deserialized: TestDiffContent = from_yaml(&yaml_output).unwrap();

        // The description should be preserved exactly
        assert_eq!(test_data.description, deserialized.description);

        // The diff_content may have a trailing newline added by YAML literal block formatting
        assert!(
            deserialized.diff_content == test_data.diff_content
                || deserialized.diff_content == format!("{}\n", test_data.diff_content)
        );
    }

    #[test]
    fn yaml_round_trip_preserves_content() {
        let original = TestDiffContent {
            diff_content: "line1\nline2\nline3".to_string(),
            description: "desc line1\ndesc line2".to_string(),
        };

        let yaml_output = to_yaml(&original).unwrap();
        let deserialized: TestDiffContent = from_yaml(&yaml_output).unwrap();

        // YAML literal blocks may add trailing newlines, so check content preservation
        assert_eq!(original.description, deserialized.description);
        assert!(
            deserialized.diff_content == original.diff_content
                || deserialized.diff_content == format!("{}\n", original.diff_content)
        );
    }

    #[test]
    fn ai_response_like_yaml_parsing() {
        // Simulate EXACTLY what the AI is generating based on the debug logs
        let ai_response_yaml = r#"title: "deps(test): upgrade hedgehog-extras to 0.10.0.0"
description: |
  # Changelog

  ```yaml
  - description: |
      Upgrade hedgehog-extras dependency from 0.7.1+ to ^>=0.10.0.0 to access newer testing utilities and improvements. Updated type constraints and imports to maintain compatibility with the enhanced testing framework.
    type:
      - test           # fixes/modifies tests
      - maintenance    # not directly related to the code
  ```

  # Context

  This PR upgrades the `hedgehog-extras` testing library from version 0.7.1+ to 0.10.0.0 to leverage newer testing utilities and improvements. The upgrade requires several compatibility changes to maintain existing test functionality while accessing the enhanced testing framework capabilities.

  The changes ensure that the Cardano CLI test suite continues to work correctly with the updated dependency while taking advantage of improvements in the newer version of hedgehog-extras.

  # How to trust this PR

  **Key areas to review:**

  1. **Dependency constraint update** in `cardano-cli/cardano-cli.cabal` - verify the version constraint change from `>=0.7.1` to `^>=0.10`

  2. **Type signature enhancement** in `Test/Cli/Run/Hash.hs` - the `hash_trip_fun` function now includes additional type constraints (`MonadBaseControl IO m` and `H.MonadAssertion m`) required by hedgehog-extras 0.10

  3. **Import additions** - new imports for `FlexibleContexts` language extension and `MonadBaseControl` to support the updated API

  **Commands to verify the changes:**
  ```bash
  # Verify the project builds with new dependencies
  cabal build cardano-cli-test-lib

  # Run the hash tests specifically
  cabal test cardano-cli-test --test-options="--pattern Hash"

  # Check that all tests still pass
  cabal test cardano-cli-test
  ```

  **Specific changes made:**

  - **cabal.project**: Updated Hackage index-state from 2025-06-22 to 2025-09-10 for latest package availability
  - **cardano-cli.cabal**: Changed hedgehog-extras constraint from `>=0.7.1` to `^>=0.10`
  - **Test/Cli/Run/Hash.hs**:
    - Added `FlexibleContexts` language extension
    - Imported `MonadBaseControl` from `Control.Monad.Trans.Control`
    - Extended `hash_trip_fun` type signature with `MonadBaseControl IO m` and `H.MonadAssertion m` constraints
  - **flake.lock**: Updated dependency hashes to reflect the new package versions

  The type constraint additions are necessary because hedgehog-extras 0.10 has enhanced its monad transformer support, requiring these additional capabilities for proper test execution.

  # Checklist

  - [x] Commit sequence broadly makes sense and commits have useful messages
  - [x] New tests are added if needed and existing tests are updated. See [Running tests](https://github.com/input-output-hk/cardano-node-wiki/wiki/Running-tests) for more details
  - [x] Self-reviewed the diff"#;

        // This should parse correctly using our hybrid approach
        #[derive(serde::Deserialize)]
        struct PrContent {
            title: String,
            description: String,
        }

        println!("Testing YAML parsing with AI response...");
        println!("Input length: {} chars", ai_response_yaml.len());
        println!(
            "First 200 chars: {}",
            &ai_response_yaml[..200.min(ai_response_yaml.len())]
        );

        let pr_content: PrContent = from_yaml(ai_response_yaml).unwrap();

        println!("Parsed title: {}", pr_content.title);
        println!(
            "Parsed description length: {}",
            pr_content.description.len()
        );
        println!("Description first 3 lines:");
        for (i, line) in pr_content.description.lines().take(3).enumerate() {
            println!("  {}: {}", i + 1, line);
        }

        assert_eq!(
            pr_content.title,
            "deps(test): upgrade hedgehog-extras to 0.10.0.0"
        );
        assert!(pr_content.description.contains("# Changelog"));
        assert!(pr_content.description.contains("# How to trust this PR"));
        assert!(pr_content.description.contains("**Key areas to review:**"));
        assert!(pr_content.description.contains("# Checklist"));

        // Verify the multiline content is preserved properly
        let lines: Vec<&str> = pr_content.description.lines().collect();
        assert!(
            lines.len() > 20,
            "Should have many lines, got {}",
            lines.len()
        );

        // Verify the description is much longer than 11 characters
        assert!(
            pr_content.description.len() > 100,
            "Description should be long, got {}",
            pr_content.description.len()
        );
    }

    // ── Edge cases for YAML serialization ────────────────────────────

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct NullableFields {
        required: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        optional: Option<String>,
    }

    #[test]
    fn yaml_optional_field_none_skipped() {
        let data = NullableFields {
            required: "present".to_string(),
            optional: None,
        };
        let yaml = to_yaml(&data).unwrap();
        assert!(yaml.contains("required:"));
        assert!(!yaml.contains("optional:"));
    }

    #[test]
    fn yaml_optional_field_some_included() {
        let data = NullableFields {
            required: "present".to_string(),
            optional: Some("also present".to_string()),
        };
        let yaml = to_yaml(&data).unwrap();
        assert!(yaml.contains("required:"));
        assert!(yaml.contains("optional:"));
        assert!(yaml.contains("also present"));
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct NestedData {
        outer: String,
        inner: InnerData,
    }

    #[derive(Debug, Serialize, Deserialize, PartialEq)]
    struct InnerData {
        value: i32,
        items: Vec<String>,
    }

    #[test]
    fn yaml_nested_structure_roundtrip() {
        let data = NestedData {
            outer: "top".to_string(),
            inner: InnerData {
                value: 42,
                items: vec!["a".to_string(), "b".to_string(), "c".to_string()],
            },
        };
        let yaml = to_yaml(&data).unwrap();
        let restored: NestedData = from_yaml(&yaml).unwrap();
        assert_eq!(restored, data);
    }

    #[test]
    fn yaml_empty_sequence() {
        let data = InnerData {
            value: 0,
            items: vec![],
        };
        let yaml = to_yaml(&data).unwrap();
        let restored: InnerData = from_yaml(&yaml).unwrap();
        assert_eq!(restored.items.len(), 0);
    }

    #[test]
    fn yaml_special_characters_roundtrip() {
        let data = TestDiffContent {
            diff_content: "line with 'quotes' and \"double quotes\"".to_string(),
            description: "colons: here, #hashes, [brackets], {braces}".to_string(),
        };
        let yaml = to_yaml(&data).unwrap();
        let restored: TestDiffContent = from_yaml(&yaml).unwrap();
        assert_eq!(restored.diff_content, data.diff_content);
        assert_eq!(restored.description, data.description);
    }

    #[test]
    fn yaml_boolean_and_numeric_roundtrip() {
        #[derive(Debug, Serialize, Deserialize, PartialEq)]
        struct MixedTypes {
            flag: bool,
            count: i64,
            ratio: f64,
            name: String,
        }

        let data = MixedTypes {
            flag: true,
            count: 42,
            ratio: 1.5,
            name: "test".to_string(),
        };
        let yaml = to_yaml(&data).unwrap();
        let restored: MixedTypes = from_yaml(&yaml).unwrap();
        assert_eq!(restored, data);
    }

    #[test]
    fn yaml_file_roundtrip() -> Result<()> {
        use tempfile::TempDir;

        let dir = {
            std::fs::create_dir_all("tmp")?;
            TempDir::new_in("tmp")?
        };
        let path = dir.path().join("test.yaml");

        let data = TestDiffContent {
            diff_content: "diff content here\nwith lines".to_string(),
            description: "a description".to_string(),
        };

        write_yaml_file(&data, &path)?;
        let restored: TestDiffContent = read_yaml_file(&path)?;
        assert_eq!(restored.description, data.description);
        Ok(())
    }

    #[test]
    fn yaml_read_nonexistent_file_fails() {
        let result: Result<TestDiffContent> = read_yaml_file("/nonexistent/path.yaml");
        assert!(result.is_err());
    }

    #[test]
    fn from_yaml_invalid_input() {
        let result: Result<TestDiffContent> = from_yaml("not: valid: yaml: [{{");
        assert!(result.is_err());
    }
}
