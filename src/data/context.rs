//! Context data structures for enhanced commit message analysis

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Complete context information for intelligent commit message improvement
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommitContext {
    /// Project-level context and conventions
    pub project: ProjectContext,
    /// Branch analysis and work pattern detection
    pub branch: BranchContext,
    /// Multi-commit analysis and work patterns
    pub range: CommitRangeContext,
    /// File-specific context and architectural understanding
    pub files: Vec<FileContext>,
    /// User-provided context information
    pub user_provided: Option<String>,
}

/// Project-level context discovered from configuration files
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectContext {
    /// Project-specific commit guidelines from .omni-dev/commit-guidelines.md
    pub commit_guidelines: Option<String>,
    /// Project-specific PR guidelines from .omni-dev/pr-guidelines.md
    pub pr_guidelines: Option<String>,
    /// Valid scopes and their descriptions from .omni-dev/scopes.yaml
    pub valid_scopes: Vec<ScopeDefinition>,
    /// Feature-specific context from .omni-dev/context/
    pub feature_contexts: HashMap<String, FeatureContext>,
    /// Parsed conventions from CONTRIBUTING.md
    pub project_conventions: ProjectConventions,
    /// Detected ecosystem (rust, node, python, etc.)
    pub ecosystem: Ecosystem,
}

/// Definition of a valid scope in the project
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScopeDefinition {
    /// Name of the scope
    pub name: String,
    /// Human-readable description of the scope
    pub description: String,
    /// Example commit messages using this scope
    pub examples: Vec<String>,
    /// File patterns that match this scope
    pub file_patterns: Vec<String>,
}

/// Context for a specific feature or work area
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureContext {
    /// Name of the feature context
    pub name: String,
    /// Description of the feature or work area
    pub description: String,
    /// Associated scope for this feature
    pub scope: String,
    /// Specific conventions for this feature
    pub conventions: Vec<String>,
}

/// Project conventions parsed from documentation
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectConventions {
    /// Expected commit message format
    pub commit_format: Option<String>,
    /// Required trailers like Signed-off-by
    pub required_trailers: Vec<String>,
    /// Preferred commit types for this project
    pub preferred_types: Vec<String>,
    /// Scope usage requirements and definitions
    pub scope_requirements: ScopeRequirements,
}

/// Requirements and validation rules for commit scopes
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScopeRequirements {
    /// Whether a scope is required in commit messages
    pub required: bool,
    /// List of valid scope names
    pub valid_scopes: Vec<String>,
    /// Mapping from file patterns to suggested scopes
    pub scope_mapping: HashMap<String, Vec<String>>, // file patterns -> scope
}

/// Detected project ecosystem
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum Ecosystem {
    #[default]
    /// Unknown or undetected ecosystem
    Unknown,
    /// Rust ecosystem with Cargo
    Rust,
    /// Node.js ecosystem with npm/yarn
    Node,
    /// Python ecosystem with pip/poetry
    Python,
    /// Go ecosystem with go modules
    Go,
    /// Java ecosystem with Maven/Gradle
    Java,
    /// Generic project without specific ecosystem
    Generic,
}

/// Branch analysis and work pattern detection
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BranchContext {
    /// Type of work being performed on this branch
    pub work_type: WorkType,
    /// Extracted scope from branch name
    pub scope: Option<String>,
    /// Associated ticket or issue ID
    pub ticket_id: Option<String>,
    /// Human-readable description of the branch purpose
    pub description: String,
    /// Whether this is a feature branch (vs main/master)
    pub is_feature_branch: bool,
    /// Base branch this was created from
    pub base_branch: Option<String>,
}

/// Type of work being performed
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum WorkType {
    #[default]
    /// Unknown or unspecified work type
    Unknown,
    /// New feature development
    Feature,
    /// Bug fix
    Fix,
    /// Documentation changes
    Docs,
    /// Code refactoring
    Refactor,
    /// Maintenance tasks
    Chore,
    /// Test-related changes
    Test,
    /// CI/CD pipeline changes
    Ci,
    /// Build system changes
    Build,
    /// Performance improvements
    Perf,
}

impl std::str::FromStr for WorkType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "feature" | "feat" => Ok(WorkType::Feature),
            "fix" | "bugfix" => Ok(WorkType::Fix),
            "docs" | "doc" => Ok(WorkType::Docs),
            "refactor" | "refact" => Ok(WorkType::Refactor),
            "chore" => Ok(WorkType::Chore),
            "test" | "tests" => Ok(WorkType::Test),
            "ci" => Ok(WorkType::Ci),
            "build" => Ok(WorkType::Build),
            "perf" | "performance" => Ok(WorkType::Perf),
            _ => Ok(WorkType::Unknown),
        }
    }
}

impl std::fmt::Display for WorkType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WorkType::Unknown => write!(f, "unknown work"),
            WorkType::Feature => write!(f, "feature development"),
            WorkType::Fix => write!(f, "bug fix"),
            WorkType::Docs => write!(f, "documentation update"),
            WorkType::Refactor => write!(f, "refactoring"),
            WorkType::Chore => write!(f, "maintenance"),
            WorkType::Test => write!(f, "testing"),
            WorkType::Ci => write!(f, "CI/CD"),
            WorkType::Build => write!(f, "build system"),
            WorkType::Perf => write!(f, "performance improvement"),
        }
    }
}

/// Multi-commit analysis and work patterns
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommitRangeContext {
    /// Related commit hashes in this analysis
    pub related_commits: Vec<String>, // commit hashes
    /// Files that appear in multiple commits
    pub common_files: Vec<PathBuf>,
    /// Detected work pattern across commits
    pub work_pattern: WorkPattern,
    /// Analysis of scope consistency
    pub scope_consistency: ScopeAnalysis,
    /// Overall architectural impact assessment
    pub architectural_impact: ArchitecturalImpact,
    /// Significance of changes for commit message detail
    pub change_significance: ChangeSignificance,
}

/// Detected work pattern across commits
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum WorkPattern {
    #[default]
    /// Unknown or undetected pattern
    Unknown,
    /// Building feature step by step
    Sequential,
    /// Multiple small cleanup commits
    Refactoring,
    /// Investigation and fixes
    BugHunt,
    /// Documentation updates
    Documentation,
    /// Config and setup changes
    Configuration,
}

/// Analysis of scope consistency across commits
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ScopeAnalysis {
    /// Most consistent scope across commits if any
    pub consistent_scope: Option<String>,
    /// All scope changes detected
    pub scope_changes: Vec<String>,
    /// Confidence level in scope consistency (0.0-1.0)
    pub confidence: f32,
}

/// Impact on system architecture
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum ArchitecturalImpact {
    #[default]
    /// Small changes, no architecture impact
    Minimal,
    /// Some architectural changes
    Moderate,
    /// Major architectural changes
    Significant,
    /// Breaking changes
    Breaking,
}

/// Significance of changes for commit message detail level
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub enum ChangeSignificance {
    #[default]
    /// Simple fix or small addition
    Minor,
    /// Notable feature or improvement
    Moderate,
    /// Significant enhancement or new capability
    Major,
    /// Major system changes or breaking changes
    Critical,
}

/// File-based context and architectural understanding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileContext {
    /// Path to the file
    pub path: PathBuf,
    /// Purpose of this file in the project
    pub file_purpose: FilePurpose,
    /// Architectural layer this file belongs to
    pub architectural_layer: ArchitecturalLayer,
    /// Impact of changes to this file
    pub change_impact: ChangeImpact,
    /// Significance of this file in the project
    pub project_significance: ProjectSignificance,
}

/// Purpose of the file in the project
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FilePurpose {
    /// Configuration files
    Config,
    /// Test files
    Test,
    /// Docs and README files
    Documentation,
    /// Main application logic
    CoreLogic,
    /// API definitions, public interfaces
    Interface,
    /// Build and deployment files
    Build,
    /// Development tools and scripts
    Tooling,
}

/// Architectural layer of the file
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum ArchitecturalLayer {
    /// UI, CLI, web interfaces
    Presentation,
    /// Core business logic
    Business,
    /// Data access, models, storage
    Data,
    /// System, network, deployment
    Infrastructure,
    /// Cross-cutting concerns (logging, auth, etc.)
    Cross,
}

/// Impact of changes to this file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChangeImpact {
    /// Formatting, comments, style changes
    Style,
    /// New functionality, non-breaking
    Additive,
    /// Changing existing functionality
    Modification,
    /// Breaking existing functionality
    Breaking,
    /// Security, safety, or critical fixes
    Critical,
}

/// Significance of file in project
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ProjectSignificance {
    /// Common, everyday files
    Routine,
    /// Key files but not critical
    Important,
    /// Core files that define the project
    Critical,
}

impl CommitContext {
    /// Create a new empty context
    pub fn new() -> Self {
        Self::default()
    }

    /// Check if this context suggests a significant change needing detailed commit message
    pub fn is_significant_change(&self) -> bool {
        matches!(
            self.range.change_significance,
            ChangeSignificance::Major | ChangeSignificance::Critical
        ) || matches!(
            self.range.architectural_impact,
            ArchitecturalImpact::Significant | ArchitecturalImpact::Breaking
        ) || self.files.iter().any(|f| {
            matches!(f.project_significance, ProjectSignificance::Critical)
                || matches!(
                    f.change_impact,
                    ChangeImpact::Breaking | ChangeImpact::Critical
                )
        })
    }

    /// Get suggested commit message verbosity level
    pub fn suggested_verbosity(&self) -> VerbosityLevel {
        if self.is_significant_change() {
            VerbosityLevel::Comprehensive
        } else if matches!(self.range.change_significance, ChangeSignificance::Moderate)
            || self.files.len() > 1
            || self.files.iter().any(|f| {
                matches!(
                    f.architectural_layer,
                    ArchitecturalLayer::Presentation | ArchitecturalLayer::Business
                )
            })
        {
            // Be more generous with Detailed level for twiddle system:
            // - Moderate changes
            // - Multiple files
            // - UI/CLI or business logic changes
            VerbosityLevel::Detailed
        } else {
            VerbosityLevel::Concise
        }
    }
}

/// Suggested level of detail for commit messages
#[derive(Debug, Clone, Copy)]
pub enum VerbosityLevel {
    /// Single line, basic conventional commit
    Concise,
    /// Subject + brief body paragraph
    Detailed,
    /// Subject + detailed multi-paragraph body + lists
    Comprehensive,
}
