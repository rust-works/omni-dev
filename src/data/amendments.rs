//! Amendment data structures and validation

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Amendment file structure
#[derive(Debug, Serialize, Deserialize)]
pub struct AmendmentFile {
    /// List of commit amendments to apply
    pub amendments: Vec<Amendment>,
}

/// Individual commit amendment
#[derive(Debug, Serialize, Deserialize)]
pub struct Amendment {
    /// Full 40-character SHA-1 commit hash
    pub commit: String,
    /// New commit message
    pub message: String,
}

impl AmendmentFile {
    /// Load amendments from YAML file
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(&path).with_context(|| {
            format!("Failed to read amendment file: {}", path.as_ref().display())
        })?;

        let amendment_file: AmendmentFile =
            serde_yaml::from_str(&content).context("Failed to parse YAML amendment file")?;

        amendment_file.validate()?;

        Ok(amendment_file)
    }

    /// Validate amendment file structure and content
    pub fn validate(&self) -> Result<()> {
        // Empty amendments are allowed - they indicate no changes are needed
        for (i, amendment) in self.amendments.iter().enumerate() {
            amendment
                .validate()
                .with_context(|| format!("Invalid amendment at index {}", i))?;
        }

        Ok(())
    }

    /// Save amendments to YAML file
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let yaml_content =
            serde_yaml::to_string(self).context("Failed to serialize amendments to YAML")?;

        fs::write(&path, yaml_content).with_context(|| {
            format!(
                "Failed to write amendment file: {}",
                path.as_ref().display()
            )
        })?;

        Ok(())
    }
}

impl Amendment {
    /// Create a new amendment
    pub fn new(commit: String, message: String) -> Self {
        Self { commit, message }
    }

    /// Validate amendment structure
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
