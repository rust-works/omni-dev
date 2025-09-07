//! Work pattern detection and analysis

use crate::data::context::{
    ArchitecturalImpact, ChangeSignificance, CommitRangeContext, ScopeAnalysis, WorkPattern,
};
use crate::git::CommitInfo;
use std::collections::HashMap;
use std::path::PathBuf;

/// Work pattern analyzer for commit ranges
pub struct WorkPatternAnalyzer;

impl WorkPatternAnalyzer {
    /// Analyze a range of commits to detect work patterns
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

    /// Find files that appear in multiple commits
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

    /// Detect the overall work pattern across commits
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

    /// Detect pattern for a single commit
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

    /// Check if commits follow a refactoring pattern
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

    /// Check if commits follow a documentation pattern
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

    /// Check if commits follow a bug hunting pattern
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

    /// Check if commits follow a configuration pattern
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

    /// Analyze consistency of scopes across commits
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

    /// Determine the architectural impact of the commit range
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

    /// Determine the significance of changes for commit message detail
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

/// Check if a file is a configuration file
fn is_config_file(file_path: &str) -> bool {
    let config_extensions = [".toml", ".json", ".yaml", ".yml", ".ini", ".cfg"];
    let config_names = ["Cargo.toml", "package.json", "go.mod", "pom.xml"];

    config_extensions.iter().any(|ext| file_path.ends_with(ext))
        || config_names.iter().any(|name| file_path.contains(name))
}

/// Check if a file is critical to the project
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

/// Check if a file is part of public interface
fn is_public_interface(file_path: &str) -> bool {
    file_path.contains("lib.rs")
        || file_path.contains("mod.rs")
        || file_path.contains("api")
        || file_path.contains("interface")
        || file_path.ends_with(".proto")
        || file_path.ends_with(".graphql")
}

/// Estimate lines changed from diff summary
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
