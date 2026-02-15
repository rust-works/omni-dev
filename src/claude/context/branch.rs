//! Branch analysis and work pattern detection.

use std::str::FromStr;
use std::sync::LazyLock;

use anyhow::Result;
use regex::Regex;

use crate::data::context::{BranchContext, WorkType};

/// Maximum branch name length considered characteristic of GitHub Flow (short, flat names).
const GITHUB_FLOW_MAX_BRANCH_LEN: usize = 50;

/// Branch naming pattern analyzer.
pub struct BranchAnalyzer;

impl BranchAnalyzer {
    /// Analyzes a branch name and extracts context information.
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

    /// Analyzes multiple branch names to understand the branching strategy.
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
            if branch.len() < GITHUB_FLOW_MAX_BRANCH_LEN && !branch.contains('/') {
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

/// Branching strategy patterns.
#[derive(Debug, Clone)]
pub enum BranchingStrategy {
    /// Git Flow branching model with feature/, release/, hotfix/ branches.
    GitFlow,
    /// GitHub Flow with simple feature branches and main branch.
    GitHubFlow,
    /// Conventional commits with type-based branch naming.
    ConventionalCommits,
    /// Unknown or mixed branching strategy.
    Unknown,
}

// Branch naming pattern regexes
#[allow(clippy::unwrap_used)] // Compile-time constant regex pattern
static STANDARD_BRANCH_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<type>feature|feat|fix|bugfix|docs?|refactor|chore|test|ci|build|perf|hotfix|release)/(?:(?P<scope>[^/]+)/)?(?P<desc>.+)$").unwrap()
});

#[allow(clippy::unwrap_used)] // Compile-time constant regex pattern
static TICKET_BRANCH_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?P<ticket>[A-Z]+-\d+|issue-\d+|#\d+)-(?P<desc>.+)$").unwrap());

#[allow(clippy::unwrap_used)] // Compile-time constant regex pattern
static USER_BRANCH_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9_-]+/(?P<desc>.+)$").unwrap());

#[allow(clippy::unwrap_used)] // Compile-time constant regex pattern
static TICKET_REFERENCE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([A-Z]+-\d+|#\d+|issue-\d+)").unwrap());

/// Extracts ticket references from a branch name.
fn extract_ticket_references(branch_name: &str) -> Option<String> {
    TICKET_REFERENCE_PATTERN
        .find(branch_name)
        .map(|m| m.as_str().to_string())
}

/// Infers the work type from description keywords.
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

/// Cleans up and normalizes description text.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::context::WorkType;

    // ── BranchAnalyzer::analyze ──────────────────────────────────────

    #[test]
    fn standard_branch_feat_with_scope() -> anyhow::Result<()> {
        let ctx = BranchAnalyzer::analyze("feat/auth/add-login")?;
        assert!(matches!(ctx.work_type, WorkType::Feature));
        assert_eq!(ctx.scope, Some("auth".to_string()));
        assert!(ctx.is_feature_branch);
        Ok(())
    }

    #[test]
    fn standard_branch_fix_no_scope() -> anyhow::Result<()> {
        let ctx = BranchAnalyzer::analyze("fix/crash-on-startup")?;
        assert!(matches!(ctx.work_type, WorkType::Fix));
        assert!(ctx.scope.is_none());
        assert!(ctx.is_feature_branch);
        Ok(())
    }

    #[test]
    fn standard_branch_refactor() -> anyhow::Result<()> {
        let ctx = BranchAnalyzer::analyze("refactor/cleanup-modules")?;
        assert!(matches!(ctx.work_type, WorkType::Refactor));
        assert!(ctx.is_feature_branch);
        Ok(())
    }

    #[test]
    fn standard_branch_docs() -> anyhow::Result<()> {
        let ctx = BranchAnalyzer::analyze("docs/update-readme")?;
        assert!(matches!(ctx.work_type, WorkType::Docs));
        assert!(!ctx.is_feature_branch); // Docs is not a feature branch
        Ok(())
    }

    #[test]
    fn ticket_branch_jira() -> anyhow::Result<()> {
        let ctx = BranchAnalyzer::analyze("PROJ-123-add-feature")?;
        assert_eq!(ctx.ticket_id, Some("PROJ-123".to_string()));
        Ok(())
    }

    #[test]
    fn ticket_branch_issue() -> anyhow::Result<()> {
        let ctx = BranchAnalyzer::analyze("issue-456-fix-bug")?;
        assert_eq!(ctx.ticket_id, Some("issue-456".to_string()));
        assert!(matches!(ctx.work_type, WorkType::Fix));
        Ok(())
    }

    #[test]
    fn user_branch() -> anyhow::Result<()> {
        let ctx = BranchAnalyzer::analyze("johndoe/add-dark-mode")?;
        assert!(matches!(ctx.work_type, WorkType::Feature));
        Ok(())
    }

    #[test]
    fn simple_branch_name() -> anyhow::Result<()> {
        let ctx = BranchAnalyzer::analyze("add-login-page")?;
        assert!(matches!(ctx.work_type, WorkType::Feature));
        Ok(())
    }

    #[test]
    fn main_branch() -> anyhow::Result<()> {
        let ctx = BranchAnalyzer::analyze("main")?;
        // "main" has no type keywords → defaults to Feature via infer_work_type_from_description
        // but is_feature_branch is set based on work_type
        assert!(matches!(ctx.work_type, WorkType::Feature));
        Ok(())
    }

    // ── analyze_branching_strategy ───────────────────────────────────

    #[test]
    fn gitflow_branches() {
        let branches: Vec<String> = vec![
            "feature/add-auth".to_string(),
            "release/1.0".to_string(),
            "main".to_string(),
        ];
        assert!(matches!(
            BranchAnalyzer::analyze_branching_strategy(&branches),
            BranchingStrategy::GitFlow
        ));
    }

    #[test]
    fn conventional_branches() {
        let branches: Vec<String> = vec!["feat/add-auth".to_string(), "fix/crash".to_string()];
        assert!(matches!(
            BranchAnalyzer::analyze_branching_strategy(&branches),
            BranchingStrategy::ConventionalCommits
        ));
    }

    #[test]
    fn github_flow_branches() {
        let branches: Vec<String> = vec!["short-name".to_string(), "another-branch".to_string()];
        assert!(matches!(
            BranchAnalyzer::analyze_branching_strategy(&branches),
            BranchingStrategy::GitHubFlow
        ));
    }

    #[test]
    fn empty_branches_unknown() {
        assert!(matches!(
            BranchAnalyzer::analyze_branching_strategy(&[]),
            BranchingStrategy::Unknown
        ));
    }

    // ── infer_work_type_from_description ─────────────────────────────

    #[test]
    fn infer_fix_keywords() {
        assert!(matches!(
            infer_work_type_from_description("fix login bug"),
            WorkType::Fix
        ));
        assert!(matches!(
            infer_work_type_from_description("resolve issue"),
            WorkType::Fix
        ));
    }

    #[test]
    fn infer_various_types() {
        assert!(matches!(
            infer_work_type_from_description("update docs"),
            WorkType::Docs
        ));
        assert!(matches!(
            infer_work_type_from_description("add test cases"),
            WorkType::Test
        ));
        assert!(matches!(
            infer_work_type_from_description("refactor modules"),
            WorkType::Refactor
        ));
        assert!(matches!(
            infer_work_type_from_description("ci pipeline"),
            WorkType::Ci
        ));
        assert!(matches!(
            infer_work_type_from_description("build config"),
            WorkType::Build
        ));
        assert!(matches!(
            infer_work_type_from_description("performance tuning"),
            WorkType::Perf
        ));
        assert!(matches!(
            infer_work_type_from_description("chore maintenance"),
            WorkType::Chore
        ));
    }

    #[test]
    fn infer_default_feature() {
        assert!(matches!(
            infer_work_type_from_description("add login page"),
            WorkType::Feature
        ));
    }

    // ── clean_description ────────────────────────────────────────────

    #[test]
    fn clean_removes_prefixes() {
        assert_eq!(clean_description("add login page"), "Login page");
        assert_eq!(clean_description("implement auth"), "Auth");
        assert_eq!(clean_description("fix crash"), "Crash");
    }

    #[test]
    fn clean_capitalizes() {
        assert_eq!(clean_description("some description"), "Some description");
    }

    #[test]
    fn clean_trims_whitespace() {
        assert_eq!(clean_description("  hello  "), "Hello");
    }

    // ── extract_ticket_references ────────────────────────────────────

    #[test]
    fn extract_jira_ticket() {
        assert_eq!(
            extract_ticket_references("PROJ-123-some-work"),
            Some("PROJ-123".to_string())
        );
    }

    #[test]
    fn extract_issue_reference() {
        assert_eq!(
            extract_ticket_references("issue-456-fix"),
            Some("issue-456".to_string())
        );
    }

    #[test]
    fn extract_hash_reference() {
        assert_eq!(
            extract_ticket_references("work-on-#789"),
            Some("#789".to_string())
        );
    }

    #[test]
    fn no_ticket_reference() {
        assert_eq!(extract_ticket_references("plain-branch-name"), None);
    }
}
