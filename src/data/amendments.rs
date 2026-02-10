//! Amendment data structures and validation.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Amendment file structure.
#[derive(Debug, Serialize, Deserialize)]
pub struct AmendmentFile {
    /// List of commit amendments to apply.
    pub amendments: Vec<Amendment>,
}

/// Individual commit amendment.
#[derive(Debug, Serialize, Deserialize)]
pub struct Amendment {
    /// Full 40-character SHA-1 commit hash.
    pub commit: String,
    /// New commit message.
    pub message: String,
}

impl AmendmentFile {
    /// Loads amendments from a YAML file.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(&path).with_context(|| {
            format!("Failed to read amendment file: {}", path.as_ref().display())
        })?;

        let amendment_file: AmendmentFile =
            crate::data::from_yaml(&content).context("Failed to parse YAML amendment file")?;

        amendment_file.validate()?;

        Ok(amendment_file)
    }

    /// Validates amendment file structure and content.
    pub fn validate(&self) -> Result<()> {
        // Empty amendments are allowed - they indicate no changes are needed
        for (i, amendment) in self.amendments.iter().enumerate() {
            amendment
                .validate()
                .with_context(|| format!("Invalid amendment at index {}", i))?;
        }

        Ok(())
    }

    /// Saves amendments to a YAML file with proper multiline formatting.
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let yaml_content =
            serde_yaml::to_string(self).context("Failed to serialize amendments to YAML")?;

        // Post-process YAML to use literal block scalars for multiline messages
        let formatted_yaml = self.format_multiline_yaml(&yaml_content);

        fs::write(&path, formatted_yaml).with_context(|| {
            format!(
                "Failed to write amendment file: {}",
                path.as_ref().display()
            )
        })?;

        Ok(())
    }

    /// Formats YAML to use literal block scalars for multiline messages.
    fn format_multiline_yaml(&self, yaml: &str) -> String {
        let mut result = String::new();
        let lines: Vec<&str> = yaml.lines().collect();
        let mut i = 0;

        while i < lines.len() {
            let line = lines[i];

            // Check if this is a message field with a quoted multiline string
            if line.trim_start().starts_with("message:") && line.contains('"') {
                let indent = line.len() - line.trim_start().len();
                let indent_str = " ".repeat(indent);

                // Extract the quoted content
                if let Some(start_quote) = line.find('"') {
                    if let Some(end_quote) = line.rfind('"') {
                        if start_quote != end_quote {
                            let quoted_content = &line[start_quote + 1..end_quote];

                            // Check if it contains newlines (multiline content)
                            if quoted_content.contains("\\n") {
                                // Convert to literal block scalar format
                                result.push_str(&format!("{}message: |\n", indent_str));

                                // Process the content, converting \n to actual newlines
                                let unescaped = quoted_content.replace("\\n", "\n");
                                for (line_idx, content_line) in unescaped.lines().enumerate() {
                                    if line_idx == 0 && content_line.trim().is_empty() {
                                        // Skip leading empty line
                                        continue;
                                    }
                                    result.push_str(&format!("{}  {}\n", indent_str, content_line));
                                }
                                i += 1;
                                continue;
                            }
                        }
                    }
                }
            }

            // Default: just copy the line as-is
            result.push_str(line);
            result.push('\n');
            i += 1;
        }

        result
    }
}

impl Amendment {
    /// Creates a new amendment.
    pub fn new(commit: String, message: String) -> Self {
        Self { commit, message }
    }

    /// Validates amendment structure.
    pub fn validate(&self) -> Result<()> {
        // Validate commit hash format
        if self.commit.len() != 40 {
            anyhow::bail!(
                "Commit hash must be exactly 40 characters long, got: {}",
                self.commit.len()
            );
        }

        if !self.commit.chars().all(|c| c.is_ascii_hexdigit()) {
            anyhow::bail!("Commit hash must contain only hexadecimal characters");
        }

        if !self
            .commit
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        {
            anyhow::bail!("Commit hash must be lowercase");
        }

        // Validate message content
        if self.message.trim().is_empty() {
            anyhow::bail!("Commit message cannot be empty");
        }

        Ok(())
    }
}
