//! Shared display formatting and utility functions for git CLI commands.
//!
//! This module contains pure functions extracted from `check`, `twiddle`, and
//! `create_pr` command modules to eliminate duplication and enable unit testing.

use crate::data::check::{CommitIssue, IssueSeverity};
use crate::data::context::{FileContext, ProjectSignificance};

/// Truncates a commit hash to [`SHORT_HASH_LEN`](crate::git::SHORT_HASH_LEN) characters.
pub(crate) fn truncate_hash(hash: &str) -> &str {
    let len = crate::git::SHORT_HASH_LEN;
    if hash.len() > len {
        &hash[..len]
    } else {
        hash
    }
}

/// Returns an ANSI-colored severity label with fixed-width padding.
pub(crate) fn format_severity_label(severity: IssueSeverity) -> &'static str {
    match severity {
        IssueSeverity::Error => "\x1b[31mERROR\x1b[0m  ",
        IssueSeverity::Warning => "\x1b[33mWARNING\x1b[0m",
        IssueSeverity::Info => "\x1b[36mINFO\x1b[0m   ",
    }
}

/// Returns an emoji icon representing the commit check result.
///
/// - Passing commits get a checkmark.
/// - Commits with errors get a cross.
/// - Commits with only warnings/info get a warning sign.
pub(crate) fn determine_commit_icon(passes: bool, issues: &[CommitIssue]) -> &'static str {
    if passes {
        "\u{2705}"
    } else if issues.iter().any(|i| i.severity == IssueSeverity::Error) {
        "\u{274c}"
    } else {
        "\u{26a0}\u{fe0f} "
    }
}

/// Resolves a short hash prefix to a full hash from a list of candidates.
///
/// Matches when either the candidate starts with the short hash or the short
/// hash starts with the candidate (bidirectional prefix matching).
pub(crate) fn resolve_short_hash<'a>(short: &str, candidates: &'a [String]) -> Option<&'a str> {
    candidates.iter().find_map(|c| {
        if c.starts_with(short) || short.starts_with(c.as_str()) {
            Some(c.as_str())
        } else {
            None
        }
    })
}

/// Formats a file analysis summary with file count and critical file count.
///
/// Returns `None` when the file list is empty.
pub(crate) fn format_file_analysis(files: &[FileContext]) -> Option<String> {
    if files.is_empty() {
        return None;
    }

    let critical_count = files
        .iter()
        .filter(|f| matches!(f.project_significance, ProjectSignificance::Critical))
        .count();

    if critical_count > 0 {
        Some(format!(
            "\u{1f4c2} Files: {} analyzed ({critical_count} critical)",
            files.len()
        ))
    } else {
        Some(format!("\u{1f4c2} Files: {} analyzed", files.len()))
    }
}

/// Splits an editor command string into the executable and its arguments.
///
/// Handles editors specified with arguments, e.g. `"code --wait"` becomes
/// `("code", vec!["--wait"])`.
pub(crate) fn parse_editor_command(editor: &str) -> (&str, Vec<&str>) {
    let mut parts = editor.split_whitespace();
    let cmd = parts.next().unwrap_or(editor);
    let args: Vec<&str> = parts.collect();
    (cmd, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- truncate_hash ---

    #[test]
    fn truncate_hash_long() {
        let hash = "abc1234567890abcdef1234567890abcdef123456";
        let result = truncate_hash(hash);
        assert_eq!(result.len(), crate::git::SHORT_HASH_LEN);
        assert_eq!(result, &hash[..crate::git::SHORT_HASH_LEN]);
    }

    #[test]
    fn truncate_hash_short() {
        let hash = "abc12";
        assert_eq!(truncate_hash(hash), "abc12");
    }

    #[test]
    fn truncate_hash_exact() {
        let hash = "abcd1234"; // exactly SHORT_HASH_LEN (8)
        assert_eq!(truncate_hash(hash), "abcd1234");
    }

    #[test]
    fn truncate_hash_empty() {
        assert_eq!(truncate_hash(""), "");
    }

    // --- format_severity_label ---

    #[test]
    fn severity_label_error() {
        let label = format_severity_label(IssueSeverity::Error);
        assert!(label.contains("ERROR"));
        assert!(label.contains("\x1b[31m")); // red
    }

    #[test]
    fn severity_label_warning() {
        let label = format_severity_label(IssueSeverity::Warning);
        assert!(label.contains("WARNING"));
        assert!(label.contains("\x1b[33m")); // yellow
    }

    #[test]
    fn severity_label_info() {
        let label = format_severity_label(IssueSeverity::Info);
        assert!(label.contains("INFO"));
        assert!(label.contains("\x1b[36m")); // cyan
    }

    // --- determine_commit_icon ---

    #[test]
    fn icon_passing() {
        let icon = determine_commit_icon(true, &[]);
        assert_eq!(icon, "\u{2705}");
    }

    #[test]
    fn icon_errors() {
        let issues = vec![CommitIssue {
            severity: IssueSeverity::Error,
            section: "subject".to_string(),
            rule: "length".to_string(),
            explanation: "too long".to_string(),
        }];
        let icon = determine_commit_icon(false, &issues);
        assert_eq!(icon, "\u{274c}");
    }

    #[test]
    fn icon_warnings_only() {
        let issues = vec![CommitIssue {
            severity: IssueSeverity::Warning,
            section: "body".to_string(),
            rule: "style".to_string(),
            explanation: "minor style issue".to_string(),
        }];
        let icon = determine_commit_icon(false, &issues);
        assert_eq!(icon, "\u{26a0}\u{fe0f} ");
    }

    // --- resolve_short_hash ---

    #[test]
    fn resolve_hash_prefix_match() {
        let candidates = vec![
            "abc1234567890abcdef1234567890abcdef123456".to_string(),
            "def1234567890abcdef1234567890abcdef123456".to_string(),
        ];
        let result = resolve_short_hash("abc1234", &candidates);
        assert_eq!(
            result,
            Some("abc1234567890abcdef1234567890abcdef123456" as &str)
        );
    }

    #[test]
    fn resolve_hash_reverse_match() {
        // Short candidate, long search hash (bidirectional matching)
        let candidates = vec!["abc1234".to_string()];
        let result = resolve_short_hash("abc1234567890abcdef1234567890abcdef123456", &candidates);
        assert_eq!(result, Some("abc1234" as &str));
    }

    #[test]
    fn resolve_hash_no_match() {
        let candidates = vec!["abc1234567890abcdef1234567890abcdef123456".to_string()];
        let result = resolve_short_hash("zzz9999", &candidates);
        assert_eq!(result, None);
    }

    // --- parse_editor_command ---

    #[test]
    fn parse_editor_simple() {
        let (cmd, args) = parse_editor_command("vim");
        assert_eq!(cmd, "vim");
        assert!(args.is_empty());
    }

    #[test]
    fn parse_editor_with_args() {
        let (cmd, args) = parse_editor_command("code --wait --new-window");
        assert_eq!(cmd, "code");
        assert_eq!(args, vec!["--wait", "--new-window"]);
    }

    // --- format_file_analysis ---

    use crate::data::context::{ArchitecturalLayer, ChangeImpact, FilePurpose};
    use std::path::PathBuf;

    fn make_file_context(path: &str, significance: ProjectSignificance) -> FileContext {
        FileContext {
            path: PathBuf::from(path),
            file_purpose: FilePurpose::CoreLogic,
            architectural_layer: ArchitecturalLayer::Business,
            change_impact: ChangeImpact::Modification,
            project_significance: significance,
        }
    }

    #[test]
    fn file_analysis_empty() {
        assert!(format_file_analysis(&[]).is_none());
    }

    #[test]
    fn file_analysis_no_critical() {
        let files = vec![
            make_file_context("src/foo.rs", ProjectSignificance::Routine),
            make_file_context("src/bar.rs", ProjectSignificance::Important),
        ];
        let label = format_file_analysis(&files).unwrap();
        assert!(label.contains("2 analyzed"));
        assert!(!label.contains("critical"));
    }

    #[test]
    fn file_analysis_with_critical() {
        let files = vec![
            make_file_context("src/main.rs", ProjectSignificance::Critical),
            make_file_context("src/foo.rs", ProjectSignificance::Routine),
            make_file_context("src/lib.rs", ProjectSignificance::Critical),
        ];
        let label = format_file_analysis(&files).unwrap();
        assert!(label.contains("3 analyzed"));
        assert!(label.contains("2 critical"));
    }

    #[test]
    fn file_analysis_single_file() {
        let files = vec![make_file_context(
            "src/foo.rs",
            ProjectSignificance::Routine,
        )];
        let label = format_file_analysis(&files).unwrap();
        assert!(label.contains("1 analyzed"));
    }
}
