//! Input validation at the MCP tool boundary.
//!
//! Per STYLE-0001, we validate at the system boundary and reject obviously
//! malformed input with a clear `invalid_params` MCP error rather than letting
//! it propagate into libgit2 or the Atlassian API where the error message
//! becomes less actionable.

use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;
use rmcp::ErrorData as McpError;

/// Matches well-formed git commit-ish ranges.
///
/// Accepts single ref specifiers (`HEAD`, `main`, `abc123`, `HEAD~3`, `HEAD^`,
/// `origin/main`) and two-dot / three-dot ranges (`a..b`, `a...b`). Matches
/// the character set git uses for ref names plus the revision selectors `~`,
/// `^`, and `@`. The primary goal is to reject obviously malformed input
/// (shell metacharacters, whitespace, semicolons) before libgit2 sees it.
#[allow(clippy::unwrap_used)] // Compile-time constant regex pattern
static RANGE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\w\-/.~^@]+(\.\.\.?[\w\-/.~^@]+)?$").unwrap());

/// Matches a JIRA issue key — one or more uppercase letters (optionally with
/// digits/underscores after the first) followed by a dash and digits.
#[allow(clippy::unwrap_used)] // Compile-time constant regex pattern
static JIRA_KEY_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Z][A-Z0-9_]+-\d+$").unwrap());

/// Matches a Confluence numeric id.
#[allow(clippy::unwrap_used)] // Compile-time constant regex pattern
static CONFLUENCE_ID_PATTERN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+$").unwrap());

/// Validates that `range` looks like a git range specifier.
pub fn validate_range(range: &str) -> Result<(), McpError> {
    if range.is_empty() {
        return Err(McpError::invalid_params(
            "range must not be empty".to_string(),
            None,
        ));
    }
    if !RANGE_PATTERN.is_match(range) {
        return Err(McpError::invalid_params(
            format!("range {range:?} is not a well-formed git range specifier"),
            None,
        ));
    }
    Ok(())
}

/// Validates that `repo_path` is absolute and points to an existing directory.
///
/// We require absolute paths because the MCP server's working directory is
/// determined by the client (which process launched it) and rarely matches
/// what the caller intends. Forcing absolute paths removes that ambiguity.
pub fn validate_repo_path(repo_path: &str) -> Result<(), McpError> {
    let path = Path::new(repo_path);
    if !path.is_absolute() {
        return Err(McpError::invalid_params(
            format!("repo_path {repo_path:?} must be an absolute path"),
            None,
        ));
    }
    if !path.exists() {
        return Err(McpError::invalid_params(
            format!("repo_path {repo_path:?} does not exist"),
            None,
        ));
    }
    if !path.is_dir() {
        return Err(McpError::invalid_params(
            format!("repo_path {repo_path:?} is not a directory"),
            None,
        ));
    }
    Ok(())
}

/// Validates that `key` is a well-formed JIRA issue key.
pub fn validate_jira_key(key: &str) -> Result<(), McpError> {
    if !JIRA_KEY_PATTERN.is_match(key) {
        return Err(McpError::invalid_params(
            format!("jira key {key:?} is not a well-formed issue key (expected e.g. ABC-123)"),
            None,
        ));
    }
    Ok(())
}

/// Validates that `id` is a Confluence numeric id.
pub fn validate_confluence_id(id: &str) -> Result<(), McpError> {
    if !CONFLUENCE_ID_PATTERN.is_match(id) {
        return Err(McpError::invalid_params(
            format!("confluence id {id:?} must be numeric"),
            None,
        ));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn range_accepts_single_ref() {
        validate_range("HEAD").unwrap();
        validate_range("main").unwrap();
        validate_range("origin/main").unwrap();
        validate_range("abc1234").unwrap();
    }

    #[test]
    fn range_accepts_revision_selectors() {
        validate_range("HEAD~3").unwrap();
        validate_range("HEAD^").unwrap();
        validate_range("HEAD@").unwrap();
        validate_range("v1.2.3").unwrap();
    }

    #[test]
    fn range_accepts_two_and_three_dot_ranges() {
        validate_range("HEAD~3..HEAD").unwrap();
        validate_range("main...feature").unwrap();
        validate_range("abc123..def456").unwrap();
    }

    #[test]
    fn range_rejects_empty() {
        let err = validate_range("").unwrap_err();
        assert!(err.message.contains("must not be empty"));
    }

    #[test]
    fn range_rejects_shell_metacharacters() {
        assert!(validate_range("HEAD; rm -rf /").is_err());
        assert!(validate_range("HEAD && evil").is_err());
        assert!(validate_range("`id`").is_err());
        assert!(validate_range("HEAD\nmain").is_err());
        assert!(validate_range("HEAD main").is_err());
    }

    #[test]
    fn range_rejects_whitespace_and_control_chars() {
        assert!(validate_range(" HEAD").is_err());
        assert!(validate_range("HEAD ").is_err());
        assert!(validate_range("HE AD").is_err());
        assert!(validate_range("HEAD\t").is_err());
    }

    #[test]
    fn repo_path_rejects_relative_path() {
        let err = validate_repo_path("./some/where").unwrap_err();
        assert!(err.message.contains("must be an absolute path"));
        let err = validate_repo_path("relative").unwrap_err();
        assert!(err.message.contains("must be an absolute path"));
    }

    #[test]
    fn repo_path_rejects_nonexistent_absolute_path() {
        let err = validate_repo_path("/this/path/does/not/exist/omnidev").unwrap_err();
        assert!(err.message.contains("does not exist"));
    }

    #[test]
    fn repo_path_rejects_file_not_dir() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let err = validate_repo_path(file.path().to_str().unwrap()).unwrap_err();
        assert!(err.message.contains("is not a directory"));
    }

    #[test]
    fn repo_path_accepts_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        validate_repo_path(dir.path().to_str().unwrap()).unwrap();
    }

    #[test]
    fn jira_key_accepts_well_formed_keys() {
        validate_jira_key("ABC-123").unwrap();
        validate_jira_key("PROJ1-1").unwrap();
        validate_jira_key("A_B-99").unwrap();
    }

    #[test]
    fn jira_key_rejects_malformed_keys() {
        assert!(validate_jira_key("abc-123").is_err());
        assert!(validate_jira_key("A-123").is_err()); // single letter prefix
        assert!(validate_jira_key("ABC123").is_err());
        assert!(validate_jira_key("ABC-").is_err());
        assert!(validate_jira_key("").is_err());
        assert!(validate_jira_key("ABC-12a").is_err());
    }

    #[test]
    fn confluence_id_accepts_digits() {
        validate_confluence_id("123").unwrap();
        validate_confluence_id("987654321").unwrap();
    }

    #[test]
    fn confluence_id_rejects_non_numeric() {
        assert!(validate_confluence_id("").is_err());
        assert!(validate_confluence_id("abc").is_err());
        assert!(validate_confluence_id("123abc").is_err());
        assert!(validate_confluence_id("12 34").is_err());
    }
}
