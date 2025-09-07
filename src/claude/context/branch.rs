//! Branch analysis and work pattern detection

use crate::data::context::{BranchContext, WorkType};
use anyhow::Result;
use once_cell::sync::Lazy;
use regex::Regex;
use std::str::FromStr;

/// Branch naming pattern analyzer
pub struct BranchAnalyzer;

impl BranchAnalyzer {
    /// Analyze branch name and extract context information
    pub fn analyze(branch_name: &str) -> Result<BranchContext> {
        let mut context = BranchContext::default();

        // Parse different branch naming conventions
        if let Some(captures) = STANDARD_BRANCH_PATTERN.captures(branch_name) {
            // Standard pattern: type/scope/description or type/description
            context.work_type = captures
                .name("type")
                .map(|m| WorkType::from_str(m.as_str()))
                .transpose()?
                .unwrap_or(WorkType::Unknown);

            context.scope = captures.name("scope").map(|m| m.as_str().to_string());

            context.description = captures
                .name("desc")
                .map(|m| m.as_str().replace(['-', '_'], " "))
                .unwrap_or_default();
        } else if let Some(captures) = TICKET_BRANCH_PATTERN.captures(branch_name) {
            // Ticket-based pattern: JIRA-123-description, issue-456-description
            context.ticket_id = captures.name("ticket").map(|m| m.as_str().to_string());
            context.description = captures
                .name("desc")
                .map(|m| m.as_str().replace(['-', '_'], " "))
                .unwrap_or_default();

            // Infer work type from description or ticket prefix
            context.work_type = infer_work_type_from_description(&context.description);
        } else if let Some(captures) = USER_BRANCH_PATTERN.captures(branch_name) {
            // User-based pattern: username/feature-description
            context.description = captures
                .name("desc")
                .map(|m| m.as_str().replace(['-', '_'], " "))
                .unwrap_or_default();

            context.work_type = infer_work_type_from_description(&context.description);
        } else {
            // Fallback: try to extract any meaningful information
            context.description = branch_name.replace(['-', '_'], " ");
            context.work_type = infer_work_type_from_description(&context.description);
        }

        // Extract ticket references from anywhere in the branch name
        context.ticket_id = context
            .ticket_id
            .or_else(|| extract_ticket_references(branch_name));

        // Determine if this is a feature branch
        context.is_feature_branch = matches!(
            context.work_type,
            WorkType::Feature | WorkType::Fix | WorkType::Refactor
        );

        // Clean up description
        context.description = clean_description(&context.description);

        Ok(context)
    }

    /// Analyze multiple branch names to understand branching strategy
    pub fn analyze_branching_strategy(branches: &[String]) -> BranchingStrategy {
        let mut has_gitflow = false;
        let mut has_github_flow = false;
        let mut has_conventional = false;

        for branch in branches {
            if branch.starts_with("feature/")
                || branch.starts_with("release/")
                || branch.starts_with("hotfix/")
            {
                has_gitflow = true;
            }
            if branch.contains("feat/") || branch.contains("fix/") || branch.contains("docs/") {
                has_conventional = true;
            }
            if branch.len() < 50 && !branch.contains('/') {
                has_github_flow = true;
            }
        }

        if has_gitflow {
            BranchingStrategy::GitFlow
        } else if has_conventional {
            BranchingStrategy::ConventionalCommits
        } else if has_github_flow {
            BranchingStrategy::GitHubFlow
        } else {
            BranchingStrategy::Unknown
        }
    }
}

/// Branching strategy patterns
#[derive(Debug, Clone)]
pub enum BranchingStrategy {
    /// Git Flow branching model with feature/, release/, hotfix/ branches
    GitFlow,
    /// GitHub Flow with simple feature branches and main branch
    GitHubFlow,
    /// Conventional commits with type-based branch naming
    ConventionalCommits,
    /// Unknown or mixed branching strategy
    Unknown,
}

// Branch naming pattern regexes
static STANDARD_BRANCH_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<type>feature|feat|fix|bugfix|docs?|refactor|chore|test|ci|build|perf|hotfix|release)/(?:(?P<scope>[^/]+)/)?(?P<desc>.+)$").unwrap()
});

static TICKET_BRANCH_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(?P<ticket>[A-Z]+-\d+|issue-\d+|#\d+)-(?P<desc>.+)$").unwrap());

static USER_BRANCH_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[a-zA-Z0-9_-]+/(?P<desc>.+)$").unwrap());

static TICKET_REFERENCE_PATTERN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"([A-Z]+-\d+|#\d+|issue-\d+)").unwrap());

/// Extract ticket references from branch name
fn extract_ticket_references(branch_name: &str) -> Option<String> {
    TICKET_REFERENCE_PATTERN
        .find(branch_name)
        .map(|m| m.as_str().to_string())
}

/// Infer work type from description keywords
fn infer_work_type_from_description(description: &str) -> WorkType {
    let desc_lower = description.to_lowercase();

    if desc_lower.contains("fix") || desc_lower.contains("bug") || desc_lower.contains("issue") {
        WorkType::Fix
    } else if desc_lower.contains("doc") || desc_lower.contains("readme") {
        WorkType::Docs
    } else if desc_lower.contains("test") {
        WorkType::Test
    } else if desc_lower.contains("refactor") || desc_lower.contains("cleanup") {
        WorkType::Refactor
    } else if desc_lower.contains("build") || desc_lower.contains("config") {
        WorkType::Build
    } else if desc_lower.contains("ci") || desc_lower.contains("workflow") {
        WorkType::Ci
    } else if desc_lower.contains("perf") || desc_lower.contains("performance") {
        WorkType::Perf
    } else if desc_lower.contains("chore") || desc_lower.contains("maintenance") {
        WorkType::Chore
    } else {
        // Default to feature for most other cases
        WorkType::Feature
    }
}

/// Clean up and normalize description text
fn clean_description(description: &str) -> String {
    let mut cleaned = description.trim().to_string();

    // Remove common prefixes
    let prefixes = [
        "add ",
        "implement ",
        "create ",
        "update ",
        "fix ",
        "remove ",
    ];
    for prefix in &prefixes {
        if cleaned.to_lowercase().starts_with(prefix) {
            cleaned = cleaned[prefix.len()..].to_string();
            break;
        }
    }

    // Ensure proper capitalization
    if let Some(first_char) = cleaned.chars().next() {
        cleaned = first_char.to_uppercase().collect::<String>() + &cleaned[first_char.len_utf8()..];
    }

    cleaned
}
