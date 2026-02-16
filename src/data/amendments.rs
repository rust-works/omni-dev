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
    /// Brief summary of what this commit changes (for cross-commit coherence).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl AmendmentFile {
    /// Loads amendments from a YAML file.
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(&path).with_context(|| {
            format!("Failed to read amendment file: {}", path.as_ref().display())
        })?;

        let amendment_file: Self =
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
                .with_context(|| format!("Invalid amendment at index {i}"))?;
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
                                result.push_str(&format!("{indent_str}message: |\n"));

                                // Process the content, converting \n to actual newlines
                                let unescaped = quoted_content.replace("\\n", "\n");
                                for (line_idx, content_line) in unescaped.lines().enumerate() {
                                    if line_idx == 0 && content_line.trim().is_empty() {
                                        // Skip leading empty line
                                        continue;
                                    }
                                    result.push_str(&format!("{indent_str}  {content_line}\n"));
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
        Self {
            commit,
            message,
            summary: None,
        }
    }

    /// Validates amendment structure.
    pub fn validate(&self) -> Result<()> {
        // Validate commit hash format
        if self.commit.len() != crate::git::FULL_HASH_LEN {
            anyhow::bail!(
                "Commit hash must be exactly {} characters long, got: {}",
                crate::git::FULL_HASH_LEN,
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── Amendment::validate ──────────────────────────────────────────

    #[test]
    fn valid_amendment() {
        let amendment = Amendment::new("a".repeat(40), "feat: add feature".to_string());
        assert!(amendment.validate().is_ok());
    }

    #[test]
    fn short_hash_rejected() {
        let amendment = Amendment::new("abc1234".to_string(), "feat: add feature".to_string());
        let err = amendment.validate().unwrap_err();
        assert!(err.to_string().contains("exactly"));
    }

    #[test]
    fn uppercase_hash_rejected() {
        let amendment = Amendment::new("A".repeat(40), "feat: add feature".to_string());
        let err = amendment.validate().unwrap_err();
        assert!(err.to_string().contains("lowercase"));
    }

    #[test]
    fn non_hex_hash_rejected() {
        let amendment = Amendment::new("g".repeat(40), "feat: add feature".to_string());
        let err = amendment.validate().unwrap_err();
        assert!(err.to_string().contains("hexadecimal"));
    }

    #[test]
    fn empty_message_rejected() {
        let amendment = Amendment::new("a".repeat(40), "   ".to_string());
        let err = amendment.validate().unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn valid_hex_digits() {
        // All valid hex chars: 0-9, a-f
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let amendment = Amendment::new(hash.to_string(), "fix: something".to_string());
        assert!(amendment.validate().is_ok());
    }

    // ── AmendmentFile::validate ──────────────────────────────────────

    #[test]
    fn validate_empty_amendments_ok() {
        let file = AmendmentFile { amendments: vec![] };
        assert!(file.validate().is_ok());
    }

    #[test]
    fn validate_propagates_amendment_errors() {
        let file = AmendmentFile {
            amendments: vec![Amendment::new("short".to_string(), "msg".to_string())],
        };
        let err = file.validate().unwrap_err();
        assert!(err.to_string().contains("index 0"));
    }

    // ── AmendmentFile round-trip ─────────────────────────────────────

    #[test]
    fn save_and_load_roundtrip() -> Result<()> {
        let dir = TempDir::new_in(".")?;
        let path = dir.path().join("amendments.yaml");

        let original = AmendmentFile {
            amendments: vec![
                Amendment {
                    commit: "a".repeat(40),
                    message: "feat(cli): add new command".to_string(),
                    summary: Some("Adds the twiddle command".to_string()),
                },
                Amendment {
                    commit: "b".repeat(40),
                    message: "fix(git): resolve rebase issue\n\nDetailed body here.".to_string(),
                    summary: None,
                },
            ],
        };

        original.save_to_file(&path)?;
        let loaded = AmendmentFile::load_from_file(&path)?;

        assert_eq!(loaded.amendments.len(), 2);
        assert_eq!(loaded.amendments[0].commit, "a".repeat(40));
        assert_eq!(loaded.amendments[0].message, "feat(cli): add new command");
        assert_eq!(loaded.amendments[1].commit, "b".repeat(40));
        assert!(loaded.amendments[1]
            .message
            .contains("resolve rebase issue"));
        Ok(())
    }

    #[test]
    fn load_invalid_yaml_fails() -> Result<()> {
        let dir = TempDir::new_in(".")?;
        let path = dir.path().join("bad.yaml");
        fs::write(&path, "not: valid: yaml: [{{")?;
        assert!(AmendmentFile::load_from_file(&path).is_err());
        Ok(())
    }

    #[test]
    fn load_nonexistent_file_fails() {
        assert!(AmendmentFile::load_from_file("/nonexistent/path.yaml").is_err());
    }

    // ── property tests ────────────────────────────────────────────

    mod prop {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn valid_hex_hash_nonempty_msg_validates(
                hash in "[0-9a-f]{40}",
                msg in "[^\t\n\r\x0b\x0c ].{0,200}",
            ) {
                let amendment = Amendment::new(hash, msg);
                prop_assert!(amendment.validate().is_ok());
            }

            #[test]
            fn wrong_length_hash_rejects(
                len in (1_usize..80).prop_filter("not 40", |l| *l != 40),
            ) {
                let hash: String = "a".repeat(len);
                let amendment = Amendment::new(hash, "valid message".to_string());
                prop_assert!(amendment.validate().is_err());
            }

            #[test]
            fn non_hex_char_in_hash_rejects(
                pos in 0_usize..40,
                bad_idx in 0_usize..20,
            ) {
                let bad_chars = "ghijklmnopqrstuvwxyz";
                let bad_char = bad_chars.as_bytes()[bad_idx % bad_chars.len()] as char;
                let mut chars: Vec<char> = "a".repeat(40).chars().collect();
                chars[pos] = bad_char;
                let hash: String = chars.into_iter().collect();
                let amendment = Amendment::new(hash, "valid message".to_string());
                prop_assert!(amendment.validate().is_err());
            }

            #[test]
            fn whitespace_only_message_rejects(
                hash in "[0-9a-f]{40}",
                ws in "[ \t\n]{1,20}",
            ) {
                let amendment = Amendment::new(hash, ws);
                prop_assert!(amendment.validate().is_err());
            }

            #[test]
            fn roundtrip_save_load(
                count in 1_usize..5,
            ) {
                let dir = tempfile::TempDir::new_in(".").unwrap();
                let path = dir.path().join("amendments.yaml");
                let amendments: Vec<Amendment> = (0..count)
                    .map(|i| {
                        let hash = format!("{i:0>40x}");
                        Amendment::new(hash, format!("feat: message {i}"))
                    })
                    .collect();
                let original = AmendmentFile { amendments };
                original.save_to_file(&path).unwrap();
                let loaded = AmendmentFile::load_from_file(&path).unwrap();
                prop_assert_eq!(loaded.amendments.len(), original.amendments.len());
                for (orig, load) in original.amendments.iter().zip(loaded.amendments.iter()) {
                    prop_assert_eq!(&orig.commit, &load.commit);
                    // Messages may differ slightly due to YAML block scalar formatting
                    prop_assert!(load.message.contains(orig.message.lines().next().unwrap()));
                }
            }
        }
    }
}
