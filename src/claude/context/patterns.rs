//! Work pattern detection and analysis.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::data::context::{
    ArchitecturalImpact, ChangeSignificance, CommitRangeContext, ScopeAnalysis, WorkPattern,
};
use crate::git::CommitInfo;

/// Work pattern analyzer for commit ranges.
pub struct WorkPatternAnalyzer;

impl WorkPatternAnalyzer {
    /// Analyzes a range of commits to detect work patterns.
    pub fn analyze_commit_range(commits: &[CommitInfo]) -> CommitRangeContext {
        let mut context = CommitRangeContext::default();

        if commits.is_empty() {
            return context;
        }

        // Collect basic information
        context.related_commits = commits.iter().map(|c| c.hash.clone()).collect();
        context.common_files = Self::find_common_files(commits);

        // Analyze work pattern
        context.work_pattern = Self::detect_work_pattern(commits);

        // Analyze scope consistency
        context.scope_consistency = Self::analyze_scope_consistency(commits);

        // Determine architectural impact
        context.architectural_impact = Self::determine_architectural_impact(commits);

        // Determine change significance
        context.change_significance = Self::determine_change_significance(commits);

        context
    }

    /// Finds files that appear in multiple commits.
    fn find_common_files(commits: &[CommitInfo]) -> Vec<PathBuf> {
        let mut file_counts: HashMap<String, usize> = HashMap::new();

        for commit in commits {
            for file_change in &commit.analysis.file_changes.file_list {
                *file_counts.entry(file_change.file.clone()).or_insert(0) += 1;
            }
        }

        // Return files that appear in more than one commit or are significant
        file_counts
            .into_iter()
            .filter(|(_, count)| *count > 1 || commits.len() == 1)
            .map(|(file, _)| PathBuf::from(file))
            .collect()
    }

    /// Detects the overall work pattern across commits.
    fn detect_work_pattern(commits: &[CommitInfo]) -> WorkPattern {
        if commits.len() == 1 {
            return Self::detect_single_commit_pattern(&commits[0]);
        }

        let commit_messages: Vec<&str> = commits
            .iter()
            .map(|c| c.original_message.as_str())
            .collect();

        // Check for refactoring patterns
        if Self::is_refactoring_pattern(&commit_messages) {
            return WorkPattern::Refactoring;
        }

        // Check for documentation patterns
        if Self::is_documentation_pattern(&commit_messages) {
            return WorkPattern::Documentation;
        }

        // Check for bug hunt patterns
        if Self::is_bug_hunt_pattern(&commit_messages) {
            return WorkPattern::BugHunt;
        }

        // Check for configuration patterns
        if Self::is_configuration_pattern(commits) {
            return WorkPattern::Configuration;
        }

        // Default to sequential development
        WorkPattern::Sequential
    }

    /// Detects the pattern for a single commit.
    fn detect_single_commit_pattern(commit: &CommitInfo) -> WorkPattern {
        let message_lower = commit.original_message.to_lowercase();
        let file_changes = &commit.analysis.file_changes;

        // Documentation pattern
        if message_lower.contains("doc")
            || file_changes
                .file_list
                .iter()
                .any(|f| f.file.ends_with(".md") || f.file.contains("doc"))
        {
            return WorkPattern::Documentation;
        }

        // Configuration pattern
        if message_lower.contains("config")
            || file_changes
                .file_list
                .iter()
                .any(|f| is_config_file(&f.file))
        {
            return WorkPattern::Configuration;
        }

        // Refactoring pattern
        if message_lower.contains("refactor") || message_lower.contains("cleanup") {
            return WorkPattern::Refactoring;
        }

        // Bug fix pattern
        if message_lower.contains("fix") || message_lower.contains("bug") {
            return WorkPattern::BugHunt;
        }

        WorkPattern::Sequential
    }

    /// Checks if commits follow a refactoring pattern.
    fn is_refactoring_pattern(messages: &[&str]) -> bool {
        let refactor_keywords = [
            "refactor",
            "cleanup",
            "reorganize",
            "restructure",
            "simplify",
        ];
        let refactor_count = messages
            .iter()
            .filter(|msg| {
                let msg_lower = msg.to_lowercase();
                refactor_keywords
                    .iter()
                    .any(|keyword| msg_lower.contains(keyword))
            })
            .count();

        refactor_count as f32 / messages.len() as f32 > 0.5
    }

    /// Checks if commits follow a documentation pattern.
    fn is_documentation_pattern(messages: &[&str]) -> bool {
        let doc_keywords = ["doc", "readme", "comment", "guide", "manual"];
        let doc_count = messages
            .iter()
            .filter(|msg| {
                let msg_lower = msg.to_lowercase();
                doc_keywords
                    .iter()
                    .any(|keyword| msg_lower.contains(keyword))
            })
            .count();

        doc_count as f32 / messages.len() as f32 > 0.6
    }

    /// Checks if commits follow a bug hunting pattern.
    fn is_bug_hunt_pattern(messages: &[&str]) -> bool {
        let bug_keywords = ["fix", "bug", "issue", "error", "problem", "debug"];
        let bug_count = messages
            .iter()
            .filter(|msg| {
                let msg_lower = msg.to_lowercase();
                bug_keywords
                    .iter()
                    .any(|keyword| msg_lower.contains(keyword))
            })
            .count();

        bug_count as f32 / messages.len() as f32 > 0.4
    }

    /// Checks if commits follow a configuration pattern.
    fn is_configuration_pattern(commits: &[CommitInfo]) -> bool {
        let config_file_count = commits
            .iter()
            .filter(|commit| {
                commit
                    .analysis
                    .file_changes
                    .file_list
                    .iter()
                    .any(|f| is_config_file(&f.file))
            })
            .count();

        config_file_count as f32 / commits.len() as f32 > 0.5
    }

    /// Analyzes consistency of scopes across commits.
    fn analyze_scope_consistency(commits: &[CommitInfo]) -> ScopeAnalysis {
        let mut scope_counts: HashMap<String, usize> = HashMap::new();
        let mut detected_scopes = Vec::new();

        for commit in commits {
            let scope = &commit.analysis.detected_scope;
            if !scope.is_empty() {
                *scope_counts.entry(scope.clone()).or_insert(0) += 1;
                detected_scopes.push(scope.clone());
            }
        }

        let consistent_scope = scope_counts
            .iter()
            .max_by_key(|(_, count)| *count)
            .map(|(scope, _)| scope.clone());

        let confidence = if let Some(ref scope) = consistent_scope {
            let scope_count = scope_counts.get(scope).unwrap_or(&0);
            *scope_count as f32 / commits.len() as f32
        } else {
            0.0
        };

        ScopeAnalysis {
            consistent_scope,
            scope_changes: detected_scopes,
            confidence,
        }
    }

    /// Determines the architectural impact of the commit range.
    fn determine_architectural_impact(commits: &[CommitInfo]) -> ArchitecturalImpact {
        let total_files_changed: usize = commits
            .iter()
            .map(|c| c.analysis.file_changes.total_files)
            .sum();

        let has_critical_files = commits.iter().any(|commit| {
            commit
                .analysis
                .file_changes
                .file_list
                .iter()
                .any(|f| is_critical_file(&f.file))
        });

        let has_breaking_changes = commits.iter().any(|commit| {
            commit.analysis.file_changes.files_deleted > 0
                || commit
                    .analysis
                    .file_changes
                    .file_list
                    .iter()
                    .any(|f| f.status == "D" && is_public_interface(&f.file))
        });

        if has_breaking_changes {
            ArchitecturalImpact::Breaking
        } else if has_critical_files || total_files_changed > 20 {
            ArchitecturalImpact::Significant
        } else if total_files_changed > 5 {
            ArchitecturalImpact::Moderate
        } else {
            ArchitecturalImpact::Minimal
        }
    }

    /// Determines the significance of changes for commit message detail.
    fn determine_change_significance(commits: &[CommitInfo]) -> ChangeSignificance {
        let total_lines_changed: i32 = commits
            .iter()
            .map(|commit| {
                // Estimate lines changed from diff summary
                estimate_lines_changed(&commit.analysis.diff_summary)
            })
            .sum();

        let has_new_features = commits.iter().any(|commit| {
            let msg_lower = commit.original_message.to_lowercase();
            msg_lower.contains("feat")
                || msg_lower.contains("add")
                || msg_lower.contains("implement")
        });

        let has_major_files = commits.iter().any(|commit| {
            commit
                .analysis
                .file_changes
                .file_list
                .iter()
                .any(|f| is_critical_file(&f.file))
        });

        if total_lines_changed > 500 || has_major_files {
            ChangeSignificance::Critical
        } else if total_lines_changed > 100 || has_new_features {
            ChangeSignificance::Major
        } else if total_lines_changed > 20 {
            ChangeSignificance::Moderate
        } else {
            ChangeSignificance::Minor
        }
    }
}

/// Checks if a file is a configuration file.
fn is_config_file(file_path: &str) -> bool {
    let config_extensions = [".toml", ".json", ".yaml", ".yml", ".ini", ".cfg"];
    let config_names = ["Cargo.toml", "package.json", "go.mod", "pom.xml"];

    config_extensions.iter().any(|ext| file_path.ends_with(ext))
        || config_names.iter().any(|name| file_path.contains(name))
}

/// Checks if a file is critical to the project.
fn is_critical_file(file_path: &str) -> bool {
    let critical_files = [
        "main.rs",
        "lib.rs",
        "index.js",
        "main.py",
        "main.go",
        "Cargo.toml",
        "package.json",
        "go.mod",
        "pom.xml",
    ];

    critical_files.iter().any(|name| file_path.contains(name))
        || file_path.contains("src/lib.rs")
        || file_path.contains("src/main.rs")
}

/// Checks if a file is part of the public interface.
fn is_public_interface(file_path: &str) -> bool {
    file_path.contains("lib.rs")
        || file_path.contains("mod.rs")
        || file_path.contains("api")
        || file_path.contains("interface")
        || file_path.ends_with(".proto")
        || file_path.ends_with(".graphql")
}

/// Estimates lines changed from a diff summary.
fn estimate_lines_changed(diff_summary: &str) -> i32 {
    let mut total = 0;

    for line in diff_summary.lines() {
        if let Some(changes_part) = line.split('|').nth(1) {
            if let Some(numbers_part) = changes_part.split_whitespace().next() {
                if let Ok(num) = numbers_part.parse::<i32>() {
                    total += num;
                }
            }
        }
    }

    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::commit::{CommitAnalysis, CommitInfo, FileChange, FileChanges};

    fn make_commit(message: &str, files: Vec<(&str, &str)>) -> CommitInfo {
        CommitInfo {
            hash: "a".repeat(40),
            author: "Test <test@test.com>".to_string(),
            date: chrono::Utc::now().fixed_offset(),
            original_message: message.to_string(),
            in_main_branches: Vec::new(),
            analysis: CommitAnalysis {
                detected_type: String::new(),
                detected_scope: String::new(),
                proposed_message: String::new(),
                file_changes: FileChanges {
                    total_files: files.len(),
                    files_added: files.iter().filter(|(s, _)| *s == "A").count(),
                    files_deleted: files.iter().filter(|(s, _)| *s == "D").count(),
                    file_list: files
                        .into_iter()
                        .map(|(status, file)| FileChange {
                            status: status.to_string(),
                            file: file.to_string(),
                        })
                        .collect(),
                },
                diff_summary: String::new(),
                diff_file: String::new(),
                file_diffs: Vec::new(),
            },
        }
    }

    fn make_commit_with_scope(message: &str, scope: &str) -> CommitInfo {
        let mut commit = make_commit(message, vec![]);
        commit.analysis.detected_scope = scope.to_string();
        commit
    }

    // ── is_config_file ─────────────────────────────────────────────

    #[test]
    fn config_file_toml() {
        assert!(is_config_file("Cargo.toml"));
    }

    #[test]
    fn config_file_json() {
        assert!(is_config_file("package.json"));
    }

    #[test]
    fn config_file_yaml() {
        assert!(is_config_file("config.yaml"));
    }

    #[test]
    fn not_config_file_rs() {
        assert!(!is_config_file("src/main.rs"));
    }

    // ── is_critical_file ───────────────────────────────────────────

    #[test]
    fn critical_file_main_rs() {
        assert!(is_critical_file("src/main.rs"));
    }

    #[test]
    fn critical_file_lib_rs() {
        assert!(is_critical_file("src/lib.rs"));
    }

    #[test]
    fn critical_file_cargo_toml() {
        assert!(is_critical_file("Cargo.toml"));
    }

    #[test]
    fn not_critical_file_helper() {
        assert!(!is_critical_file("src/utils/helper.rs"));
    }

    // ── is_public_interface ────────────────────────────────────────

    #[test]
    fn public_interface_lib_rs() {
        assert!(is_public_interface("src/lib.rs"));
    }

    #[test]
    fn public_interface_mod_rs() {
        assert!(is_public_interface("src/cli/mod.rs"));
    }

    #[test]
    fn public_interface_proto() {
        assert!(is_public_interface("api/service.proto"));
    }

    #[test]
    fn not_public_interface_internal() {
        assert!(!is_public_interface("src/utils/helper.rs"));
    }

    // ── estimate_lines_changed ─────────────────────────────────────

    #[test]
    fn estimate_lines_empty() {
        assert_eq!(estimate_lines_changed(""), 0);
    }

    #[test]
    fn estimate_lines_single_file() {
        assert_eq!(estimate_lines_changed(" src/main.rs | 10 ++++"), 10);
    }

    #[test]
    fn estimate_lines_multiple_files() {
        let summary = " src/main.rs | 10 ++++\n src/lib.rs | 5 ++";
        assert_eq!(estimate_lines_changed(summary), 15);
    }

    #[test]
    fn estimate_lines_no_numbers() {
        assert_eq!(estimate_lines_changed("no pipe here"), 0);
    }

    // ── detect_single_commit_pattern ───────────────────────────────

    #[test]
    fn single_commit_doc_pattern() {
        let commit = make_commit("Update README", vec![("M", "README.md")]);
        assert!(matches!(
            WorkPatternAnalyzer::detect_work_pattern(&[commit]),
            WorkPattern::Documentation
        ));
    }

    #[test]
    fn single_commit_config_pattern() {
        let commit = make_commit("Update config", vec![("M", "settings.toml")]);
        assert!(matches!(
            WorkPatternAnalyzer::detect_work_pattern(&[commit]),
            WorkPattern::Configuration
        ));
    }

    #[test]
    fn single_commit_refactor_pattern() {
        let commit = make_commit("refactor: simplify logic", vec![("M", "src/core.rs")]);
        assert!(matches!(
            WorkPatternAnalyzer::detect_work_pattern(&[commit]),
            WorkPattern::Refactoring
        ));
    }

    #[test]
    fn single_commit_bugfix_pattern() {
        let commit = make_commit("fix: resolve crash", vec![("M", "src/handler.rs")]);
        assert!(matches!(
            WorkPatternAnalyzer::detect_work_pattern(&[commit]),
            WorkPattern::BugHunt
        ));
    }

    #[test]
    fn single_commit_sequential_default() {
        let commit = make_commit("feat: add feature", vec![("A", "src/new.rs")]);
        assert!(matches!(
            WorkPatternAnalyzer::detect_work_pattern(&[commit]),
            WorkPattern::Sequential
        ));
    }

    // ── multi-commit pattern detection ─────────────────────────────

    #[test]
    fn multi_commit_refactoring_pattern() {
        let commits = vec![
            make_commit("refactor: extract module", vec![]),
            make_commit("cleanup: remove dead code", vec![]),
            make_commit("simplify: reduce complexity", vec![]),
        ];
        assert!(matches!(
            WorkPatternAnalyzer::detect_work_pattern(&commits),
            WorkPattern::Refactoring
        ));
    }

    #[test]
    fn multi_commit_documentation_pattern() {
        let commits = vec![
            make_commit("doc: add API guide", vec![]),
            make_commit("docs: update readme", vec![]),
            make_commit("readme: add examples", vec![]),
            make_commit("manual: update install guide", vec![]),
        ];
        assert!(matches!(
            WorkPatternAnalyzer::detect_work_pattern(&commits),
            WorkPattern::Documentation
        ));
    }

    #[test]
    fn multi_commit_bug_hunt_pattern() {
        let commits = vec![
            make_commit("fix: null pointer", vec![]),
            make_commit("debug: add logging", vec![]),
            make_commit("fix: race condition", vec![]),
        ];
        assert!(matches!(
            WorkPatternAnalyzer::detect_work_pattern(&commits),
            WorkPattern::BugHunt
        ));
    }

    // ── scope consistency analysis ─────────────────────────────────

    #[test]
    fn scope_consistency_all_same() {
        let commits = vec![
            make_commit_with_scope("feat(cli): add flag", "cli"),
            make_commit_with_scope("fix(cli): fix bug", "cli"),
        ];
        let analysis = WorkPatternAnalyzer::analyze_scope_consistency(&commits);
        assert_eq!(analysis.consistent_scope, Some("cli".to_string()));
        assert!(
            (analysis.confidence - 1.0).abs() < f32::EPSILON,
            "confidence should be 1.0 for consistent scope"
        );
    }

    #[test]
    fn scope_consistency_mixed() {
        let commits = vec![
            make_commit_with_scope("feat(cli): add flag", "cli"),
            make_commit_with_scope("fix(git): fix bug", "git"),
            make_commit_with_scope("feat(cli): another", "cli"),
        ];
        let analysis = WorkPatternAnalyzer::analyze_scope_consistency(&commits);
        assert_eq!(analysis.consistent_scope, Some("cli".to_string()));
    }

    #[test]
    fn scope_consistency_empty_scopes() {
        let commits = vec![
            make_commit_with_scope("update stuff", ""),
            make_commit_with_scope("more stuff", ""),
        ];
        let analysis = WorkPatternAnalyzer::analyze_scope_consistency(&commits);
        assert!(
            analysis.confidence.abs() < f32::EPSILON,
            "confidence should be 0.0 for empty scopes"
        );
    }

    // ── architectural impact ───────────────────────────────────────

    #[test]
    fn architectural_impact_breaking() {
        let commit = make_commit("remove API", vec![("D", "src/lib.rs")]);
        let impact = WorkPatternAnalyzer::determine_architectural_impact(&[commit]);
        assert!(matches!(impact, ArchitecturalImpact::Breaking));
    }

    #[test]
    fn architectural_impact_significant_critical_files() {
        let commit = make_commit("update main", vec![("M", "src/main.rs")]);
        let impact = WorkPatternAnalyzer::determine_architectural_impact(&[commit]);
        assert!(matches!(impact, ArchitecturalImpact::Significant));
    }

    #[test]
    fn architectural_impact_minimal() {
        let commit = make_commit("small fix", vec![("M", "src/utils/helper.rs")]);
        let impact = WorkPatternAnalyzer::determine_architectural_impact(&[commit]);
        assert!(matches!(impact, ArchitecturalImpact::Minimal));
    }

    // ── change significance ────────────────────────────────────────

    #[test]
    fn change_significance_critical_with_major_files() {
        let commit = make_commit("big change", vec![("M", "src/main.rs")]);
        let significance = WorkPatternAnalyzer::determine_change_significance(&[commit]);
        assert!(matches!(significance, ChangeSignificance::Critical));
    }

    #[test]
    fn change_significance_major_with_feat() {
        let commit = make_commit("feat: add new feature", vec![("A", "src/new.rs")]);
        let significance = WorkPatternAnalyzer::determine_change_significance(&[commit]);
        assert!(matches!(significance, ChangeSignificance::Major));
    }

    #[test]
    fn change_significance_minor_small_change() {
        let commit = make_commit("tweak", vec![("M", "src/utils/helper.rs")]);
        let significance = WorkPatternAnalyzer::determine_change_significance(&[commit]);
        assert!(matches!(significance, ChangeSignificance::Minor));
    }

    // ── analyze_commit_range integration ───────────────────────────

    #[test]
    fn analyze_commit_range_empty() {
        let context = WorkPatternAnalyzer::analyze_commit_range(&[]);
        assert!(context.related_commits.is_empty());
        assert!(context.common_files.is_empty());
    }

    #[test]
    fn analyze_commit_range_single_commit() {
        let commit = make_commit("feat: add feature", vec![("A", "src/new.rs")]);
        let context = WorkPatternAnalyzer::analyze_commit_range(&[commit]);
        assert_eq!(context.related_commits.len(), 1);
        assert_eq!(context.common_files.len(), 1);
    }

    #[test]
    fn analyze_commit_range_common_files() {
        let commits = vec![
            make_commit("first", vec![("M", "src/main.rs"), ("M", "src/lib.rs")]),
            make_commit("second", vec![("M", "src/main.rs")]),
        ];
        let context = WorkPatternAnalyzer::analyze_commit_range(&commits);
        // src/main.rs appears in both commits
        assert!(context
            .common_files
            .iter()
            .any(|f| f.to_string_lossy() == "src/main.rs"));
    }

    // ── property tests ────────────────────────────────────────────

    mod prop {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn estimate_lines_nonnegative(s in ".*") {
                prop_assert!(estimate_lines_changed(&s) >= 0);
            }

            #[test]
            fn estimate_lines_structured_input(n in 0_u16..10000) {
                let input = format!(" src/main.rs | {n} ++++");
                let result = estimate_lines_changed(&input);
                prop_assert!(result >= i32::from(n));
            }

            #[test]
            fn classification_deterministic(s in ".*") {
                prop_assert_eq!(is_config_file(&s), is_config_file(&s));
                prop_assert_eq!(is_critical_file(&s), is_critical_file(&s));
                prop_assert_eq!(is_public_interface(&s), is_public_interface(&s));
            }
        }
    }
}
