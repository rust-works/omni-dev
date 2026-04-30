//! Shared helper for MCP read-style tools that optionally write to disk.
//!
//! Large pages returned inline by MCP tools can blow past the assistant's
//! context window. When a caller supplies an `output_file`, the rendered
//! content is written to disk and the tool returns a short YAML summary
//! pointing at the file so the assistant can page through it via its
//! filesystem read tool. See issue #631 for the motivating use case.

use anyhow::{Context, Result};
use serde::Serialize;

/// Summary returned to the assistant when a read tool wrote its output to a
/// file rather than returning the content inline.
#[derive(Debug, Serialize)]
pub struct WriteFileSummary {
    /// Path the content was written to (as supplied by the caller).
    pub path: String,
    /// Number of bytes written.
    pub bytes: usize,
    /// Output format identifier (e.g. `"jfm"`, `"adf"`).
    pub format: String,
}

/// Writes `content` to `path` and returns a YAML-encoded [`WriteFileSummary`].
pub fn write_to_file_yaml(path: &str, content: &str, format: &str) -> Result<String> {
    std::fs::write(path, content).with_context(|| format!("Failed to write to {path}"))?;
    let summary = WriteFileSummary {
        path: path.to_string(),
        bytes: content.len(),
        format: format.to_string(),
    };
    serde_yaml::to_string(&summary).context("Failed to serialize write summary as YAML")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn write_to_file_yaml_writes_content_and_summarises() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.md");
        let path_str = path.to_str().unwrap();

        let yaml = write_to_file_yaml(path_str, "hello world", "jfm").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello world");
        assert!(yaml.contains(&format!("path: {path_str}")));
        assert!(yaml.contains("bytes: 11"));
        assert!(yaml.contains("format: jfm"));
    }

    #[test]
    fn write_to_file_yaml_overwrites_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.md");
        std::fs::write(&path, "old").unwrap();

        write_to_file_yaml(path.to_str().unwrap(), "new", "jfm").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    #[test]
    fn write_to_file_yaml_reports_byte_count_for_unicode() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.md");

        // "héllo" = 6 bytes (é is 2 bytes in UTF-8).
        let yaml = write_to_file_yaml(path.to_str().unwrap(), "héllo", "jfm").unwrap();

        assert!(yaml.contains("bytes: 6"));
    }

    #[test]
    fn write_to_file_yaml_errors_on_invalid_path() {
        let err = write_to_file_yaml("/nonexistent_dir_zxq/file.txt", "data", "jfm").unwrap_err();
        assert!(err.to_string().contains("Failed to write"));
    }

    #[test]
    fn write_to_file_yaml_uses_supplied_format_label() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("out.json");
        let yaml = write_to_file_yaml(path.to_str().unwrap(), "{}", "adf").unwrap();
        assert!(yaml.contains("format: adf"));
    }
}
