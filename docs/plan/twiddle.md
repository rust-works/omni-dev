# Twiddle Command Implementation Plan

## Implementation Status

**Phase 1: Core Implementation** - ✅ **COMPLETED** (2025-01-07)
- All core functionality implemented and working
- Claude API integration operational
- Basic CLI structure in place
- Error handling and validation complete

**Phase 2: User Experience** - ✅ **COMPLETED**  
- ✅ Progress indicators implemented (`src/cli/git.rs:247,256,277`)
- ✅ Confirmation prompts working (`src/cli/git.rs:345-356`)  
- ✅ Preview functionality operational (`src/cli/git.rs:324-342`)
- ✅ Comprehensive error messages via ClaudeError enum

**Phase 3: Contextual Intelligence** - 🔄 **PLANNED**
- 🔄 Project-level context discovery (.omni-dev/, .gitmessage, CONTRIBUTING.md)
- 🔄 Branch-aware commit analysis and work pattern detection
- 🔄 Multi-commit range context understanding
- 🔄 File-based architectural context recognition
- 🔄 Enhanced Claude prompting with project-specific guidelines

**Current Status**: Ready for production use with full Phase 1 & 2 functionality. Phase 3 (contextual intelligence) and subsequent phases remain for future development.

### Key Accomplishments
- ✅ Full `omni-dev git commit message twiddle` command implementation
- ✅ Claude API integration with proper error handling  
- ✅ Async/await support with Tokio runtime
- ✅ Repository view generation reusing existing ViewCommand logic
- ✅ Amendment application reusing existing AmendmentHandler
- ✅ User confirmation prompts and preview functionality
- ✅ Comprehensive CLI argument support (`--model`, `--auto-apply`, `--save-only`)
- ✅ Environment variable support (`CLAUDE_API_KEY`, `ANTHROPIC_API_KEY`)
- ✅ Claude Code templates for interactive usage

## Overview

The `omni-dev git commit message twiddle` command is a new feature that combines the functionality of the existing `view` and `amend` commands with Claude AI integration to automatically generate commit message improvements.

## Command Flow

### Basic Flow (Phase 1 & 2 - Implemented)
```
omni-dev git commit message twiddle [COMMIT_RANGE]
    ↓
1. Execute view command logic → YAML output
    ↓  
2. Send YAML to Claude API → Amendment suggestions
    ↓
3. Execute amend command logic → Apply amendments
```

### Enhanced Contextual Flow (Phase 3 - Planned)
```
omni-dev git commit message twiddle [COMMIT_RANGE] --use-context
    ↓
1. Project Context Discovery (.omni-dev/, .gitmessage, CONTRIBUTING.md)
    ↓
2. Branch Analysis (naming patterns, work type detection)
    ↓
3. Commit Range Analysis (work patterns, scope consistency)
    ↓
4. File-based Context (architectural layers, change impact)
    ↓
5. Enhanced Repository View (with contextual metadata)
    ↓
6. Context-aware Claude prompting → Intelligent suggestions
    ↓
7. Apply amendments with context validation
```

## Existing Commands Analysis

### View Command (`src/cli/git.rs:123-192`)
- **Purpose**: Analyzes commits and outputs repository information in YAML format
- **Key functionality**:
  - Opens git repository using `GitRepository::open()`
  - Gets working directory status
  - Fetches remote information
  - Parses commit range and retrieves commits
  - Creates `RepositoryView` with all data
  - Updates field presence tracking
  - Outputs structured YAML via `crate::data::to_yaml()`

### Amend Command (`src/cli/git.rs:194-212`)  
- **Purpose**: Amends commit messages based on YAML configuration file
- **Key functionality**:
  - Uses `AmendmentHandler::new()` to create handler
  - Calls `handler.apply_amendments(yaml_file)` 
  - Performs safety checks (working directory clean, commits exist)
  - Handles HEAD-only amendments vs. interactive rebase
  - Uses git commands to modify commit messages

### Amendment Data Structure (`src/data/amendments.rs`)
- `AmendmentFile`: Contains array of `Amendment` objects
- `Amendment`: Has `commit` (40-char SHA-1 hash) and `message` fields
- Validation ensures proper hash format and non-empty messages
- Serializable to/from YAML format

## Proposed Architecture

### 1. New Command Structure

Add to `MessageSubcommands` in `src/cli/git.rs:47-53`:
```rust
/// AI-powered commit message improvement using Claude
Twiddle(TwiddleCommand),
```

### 2. Enhanced TwiddleCommand with Contextual Support

#### Phase 3 Enhancement: Contextual Intelligence System

The contextual system transforms twiddle from a basic formatter into an intelligent, project-aware commit improvement tool.

```rust
/// Twiddle command options (Phase 1 & 2 - Implemented)
#[derive(Parser)]
pub struct TwiddleCommand {
    /// Commit range to analyze and improve (e.g., HEAD~3..HEAD, abc123..def456)
    #[arg(value_name = "COMMIT_RANGE")]
    pub commit_range: Option<String>,
    
    /// Claude API model to use (defaults to claude-3-5-sonnet-20241022)
    #[arg(long, default_value = "claude-3-5-sonnet-20241022")]
    pub model: String,
    
    /// Skip confirmation prompt and apply amendments automatically
    #[arg(long)]
    pub auto_apply: bool,
    
    /// Save generated amendments to file without applying
    #[arg(long, value_name = "FILE")]
    pub save_only: Option<String>,
    
    // Phase 3 Contextual Enhancements - PLANNED
    /// Use additional project context for better suggestions
    #[arg(long, default_value = "true")]
    pub use_context: bool,
    
    /// Path to custom context directory (defaults to .omni-dev/)
    #[arg(long)]
    pub context_dir: Option<PathBuf>,
    
    /// Specify work context (e.g., "feature: user authentication")
    #[arg(long)]
    pub work_context: Option<String>,
    
    /// Override detected branch context
    #[arg(long)]
    pub branch_context: Option<String>,
}
```

### 3. Claude API Integration

#### Dependencies to Add
Add to `Cargo.toml`:
```toml
[dependencies]
# Existing dependencies...
anthropic = "1.0"  # or latest version
tokio = { version = "1.0", features = ["full"] }
```

#### Claude Client Module (`src/claude/mod.rs`)
```rust
//! Claude API integration for commit message improvement

use anyhow::{Context, Result};
use anthropic::{Client, CompletionRequest};
use crate::data::{RepositoryView, amendments::AmendmentFile};

pub struct ClaudeClient {
    client: Client,
    model: String,
}

impl ClaudeClient {
    /// Create new Claude client from environment variable
    pub fn new(model: String) -> Result<Self> {
        let api_key = std::env::var("CLAUDE_API_KEY")
            .or_else(|_| std::env::var("ANTHROPIC_API_KEY"))
            .context("Claude API key not found. Set CLAUDE_API_KEY or ANTHROPIC_API_KEY environment variable")?;
        
        let client = Client::new(api_key);
        Ok(Self { client, model })
    }
    
    /// Generate commit message amendments from repository view
    pub async fn generate_amendments(&self, repo_view: &RepositoryView) -> Result<AmendmentFile> {
        // Implementation details below
    }
}
```

### 4. Claude Prompt Engineering

#### System Prompt
```
You are an expert software engineer helping improve git commit messages. You will receive a YAML representation of a git repository with commit information. Your task is to analyze the commits and suggest improvements to make them follow conventional commit format and best practices.

Rules:
1. Follow conventional commit format: type(scope): description
2. Types: feat, fix, docs, style, refactor, test, chore, ci, build, perf
3. Keep subject lines under 50 characters when possible
4. Use imperative mood ("Add feature" not "Added feature")
5. Provide clear, concise descriptions of what the commit does
6. Only suggest changes for commits that would benefit from improvement
7. Preserve the commit's original intent while improving clarity

Respond with a YAML amendment file in this exact format:
```yaml
amendments:
  - commit: "full-40-character-sha1-hash"
    message: "improved commit message"
  - commit: "another-full-40-character-sha1-hash"  
    message: "another improved commit message"
```
```

#### User Prompt Template
```
Please analyze the following repository information and suggest commit message improvements:

{YAML_REPOSITORY_VIEW}

Focus on commits that:
- Don't follow conventional commit format
- Have unclear or vague descriptions
- Use past tense instead of imperative mood
- Are too verbose or too brief
- Could benefit from proper type/scope classification

Only include commits that actually need improvement. If all commits are already well-formatted, return an empty amendments array.
```

### 5. Contextual Intelligence System (Phase 3)

#### 5.1. Multi-Layer Context Architecture

The contextual system provides intelligent, project-aware commit message improvement through multiple layers of context discovery and analysis.

```rust
/// Core context structures for enhanced commit analysis
pub struct CommitContext {
    pub project: ProjectContext,
    pub branch: BranchContext,
    pub range: CommitRangeContext,
    pub files: Vec<FileContext>,
    pub user_provided: Option<String>,
}

/// Project-level context discovered from configuration files
pub struct ProjectContext {
    pub commit_guidelines: Option<String>,      // From .omni-dev/commit-guidelines.md
    pub commit_template: Option<String>,        // From .gitmessage or .omni-dev/commit-template.txt
    pub valid_scopes: Vec<ScopeDefinition>,     // From .omni-dev/scopes.yaml
    pub feature_contexts: HashMap<String, FeatureContext>, // From .omni-dev/context/
    pub project_conventions: ProjectConventions, // Parsed from CONTRIBUTING.md
}

/// Branch analysis and work pattern detection
pub struct BranchContext {
    pub work_type: WorkType,                    // feature, fix, docs, refactor, chore
    pub scope: Option<String>,                  // Extracted from branch naming
    pub ticket_id: Option<String>,              // JIRA-123, #456, etc.
    pub description: String,                    // Parsed from branch name
    pub is_feature_branch: bool,
}

/// Multi-commit analysis and work patterns
pub struct CommitRangeContext {
    pub related_commits: Vec<CommitHash>,
    pub common_files: Vec<PathBuf>,
    pub work_pattern: WorkPattern,              // sequential, refactoring, bug-hunt
    pub scope_consistency: ScopeAnalysis,
    pub architectural_impact: ArchitecturalImpact,
}

/// File-based context and architectural understanding
pub struct FileContext {
    pub file_purpose: FilePurpose,              // config, test, docs, core-logic
    pub architectural_layer: Layer,             // ui, business, data, infrastructure  
    pub change_impact: Impact,                  // breaking, additive, fix, style
    pub project_significance: Significance,     // critical, important, routine
}
```

#### 5.2. Project Context Discovery

**Convention-Based Discovery Priority**:
1. `.omni-dev/` directory (project-specific)
2. Standard git files (`.gitmessage`)
3. Documentation parsing (`CONTRIBUTING.md`, `README.md`)
4. Ecosystem conventions (Rust, Node.js, Python, etc.)

```rust
impl ProjectContext {
    pub fn discover(repo_path: &Path) -> Result<Self> {
        let mut context = Self::default();
        
        // 1. Check .omni-dev/ directory
        let omni_dev_dir = repo_path.join(".omni-dev");
        if omni_dev_dir.exists() {
            context.load_omni_dev_config(&omni_dev_dir)?;
        }
        
        // 2. Standard git configuration
        if let Ok(template) = fs::read_to_string(repo_path.join(".gitmessage")) {
            context.commit_template = Some(template);
        }
        
        // 3. Parse documentation
        context.parse_contributing_guidelines(repo_path)?;
        context.detect_ecosystem_conventions(repo_path)?;
        
        Ok(context)
    }
    
    fn load_omni_dev_config(&mut self, dir: &Path) -> Result<()> {
        // Load commit-guidelines.md
        if let Ok(guidelines) = fs::read_to_string(dir.join("commit-guidelines.md")) {
            self.commit_guidelines = Some(guidelines);
        }
        
        // Load scopes.yaml
        if let Ok(scopes_yaml) = fs::read_to_string(dir.join("scopes.yaml")) {
            self.valid_scopes = serde_yaml::from_str(&scopes_yaml)?;
        }
        
        // Load feature contexts
        let contexts_dir = dir.join("context/feature-contexts");
        if contexts_dir.exists() {
            self.load_feature_contexts(&contexts_dir)?;
        }
        
        Ok(())
    }
}
```

#### 5.3. Enhanced Claude Prompting

**Context-Aware System Prompt Generation**:
```rust
pub fn generate_contextual_system_prompt(context: &CommitContext) -> String {
    let mut prompt = SYSTEM_PROMPT.to_string();
    
    // Add project-specific guidelines
    if let Some(guidelines) = &context.project.commit_guidelines {
        prompt.push_str(&format!("\n\nProject-specific commit guidelines:\n{}", guidelines));
    }
    
    // Add valid scopes
    if !context.project.valid_scopes.is_empty() {
        let scopes = context.project.valid_scopes.iter()
            .map(|s| format!("- {}: {}", s.name, s.description))
            .collect::<Vec<_>>()
            .join("\n");
        prompt.push_str(&format!("\n\nValid scopes for this project:\n{}", scopes));
    }
    
    // Add branch context
    if context.branch.is_feature_branch {
        prompt.push_str(&format!(
            "\n\nBranch context: This is a {} working on {}. Consider this context when improving commit messages.",
            context.branch.work_type,
            context.branch.description
        ));
    }
    
    // Add work pattern context
    match context.range.work_pattern {
        WorkPattern::Sequential => {
            prompt.push_str("\n\nWork pattern: Sequential feature development. Ensure commit messages show logical progression.");
        }
        WorkPattern::Refactoring => {
            prompt.push_str("\n\nWork pattern: Refactoring work. Focus on clarity about what's being restructured and why.");
        }
        WorkPattern::BugHunt => {
            prompt.push_str("\n\nWork pattern: Bug investigation. Emphasize debugging steps and fixes clearly.");
        }
    }
    
    prompt
}
```

#### 5.4. Configuration File Examples

**.omni-dev/commit-guidelines.md**:
```markdown
# Project Commit Guidelines

## Scopes
- `auth`: Authentication and authorization systems  
- `api`: Public API endpoints and contracts
- `db`: Database operations and schema changes
- `ui`: User interface components and styling
- `config`: Configuration files and environment setup

## Conventions  
- Always reference issue numbers when available: `Fixes #123`
- Use present tense imperatives: "Add feature" not "Added feature"
- Keep subject line under 50 characters
- Include breaking change notes in commit footer
- Use co-authored trailers for pair programming

## Examples
Good: `feat(auth): add JWT token validation with expiry`
Bad: `fixed login stuff`
```

**.omni-dev/scopes.yaml**:
```yaml
scopes:
  - name: auth
    description: Authentication and authorization systems
    examples: ["login", "jwt", "oauth", "permissions"]
    files: ["src/auth/", "middleware/auth.rs"]
  
  - name: api
    description: Public API endpoints and contracts  
    examples: ["routes", "handlers", "schemas", "validation"]
    files: ["src/api/", "src/handlers/"]
    
  - name: db
    description: Database operations and schema
    examples: ["migrations", "queries", "models", "indexes"]  
    files: ["migrations/", "src/models/", "src/db/"]
```

#### 5.5. Branch Analysis and Work Pattern Detection

```rust
impl BranchContext {
    pub fn analyze(branch_name: &str) -> Result<Self> {
        let mut context = Self::default();
        
        // Parse branch naming conventions
        if let Some(captures) = BRANCH_PATTERN.captures(branch_name) {
            context.work_type = captures.name("type")
                .map(|m| WorkType::from_str(m.as_str()))
                .transpose()?
                .unwrap_or(WorkType::Unknown);
                
            context.scope = captures.name("scope")
                .map(|m| m.as_str().to_string());
                
            context.description = captures.name("desc")
                .map(|m| m.as_str().replace('-', ' '))
                .unwrap_or_default();
                
            // Extract ticket references
            context.ticket_id = extract_ticket_references(branch_name);
        }
        
        context.is_feature_branch = matches!(
            context.work_type, 
            WorkType::Feature | WorkType::Fix | WorkType::Refactor
        );
        
        Ok(context)
    }
}

// Regex patterns for branch naming conventions
static BRANCH_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<type>feature|feat|fix|docs|refactor|chore)/(?P<scope>[^/]*)/(?P<desc>.+)$|^(?P<type>feature|feat|fix|docs|refactor|chore)/(?P<desc>[^/]+)$").unwrap()
});
```

#### 5.6. Enhanced User Experience

**Context-Aware Command Output**:
```bash
$ omni-dev git commit message twiddle HEAD~3..HEAD --use-context

🔍 Discovering project context...
  ✓ Found commit guidelines in .omni-dev/commit-guidelines.md
  ✓ Loaded 5 project scopes from .omni-dev/scopes.yaml  
  ✓ Detected feature branch: feature/auth/user-login
  ✓ Work pattern: Sequential feature development
  ✓ Common scope detected: auth (authentication systems)
  
🧠 Branch context analysis:
  Type: Feature development
  Scope: auth  
  Work: user login functionality
  Pattern: 3 commits building toward complete feature
  
🤖 Analyzing commits with enhanced context...
  Using project guidelines for auth scope
  Applying sequential development pattern
  Considering authentication domain expertise
  
📝 Found 2 commits that could be improved:

  abc1234 → feat(auth): implement JWT token validation with expiry checks
           (enhanced: added security considerations from project guidelines)
           (before: "add jwt stuff")
           
  def5678 → feat(auth): add secure user login endpoint with rate limiting  
           (enhanced: follows API endpoint patterns and security requirements)
           (before: "login endpoint")

❓ Apply these context-enhanced amendments? [y/N]
```

### 6. Implementation Flow

#### TwiddleCommand::execute() Logic
```rust
impl TwiddleCommand {
    pub async fn execute(self) -> Result<()> {
        // 1. Generate repository view (reuse ViewCommand logic)
        let repo_view = self.generate_repository_view().await?;
        
        // 2. Initialize Claude client
        let claude_client = ClaudeClient::new(self.model)?;
        
        // 3. Generate amendments via Claude API
        println!("🤖 Analyzing commits with Claude AI...");
        let amendments = claude_client.generate_amendments(&repo_view).await?;
        
        // 4. Handle different output modes
        if let Some(save_path) = self.save_only {
            amendments.save_to_file(save_path)?;
            println!("💾 Amendments saved to file");
            return Ok(());
        }
        
        // 5. Show preview and get confirmation
        if !amendments.amendments.is_empty() {
            self.show_amendment_preview(&amendments)?;
            
            if !self.auto_apply && !self.get_user_confirmation()? {
                println!("❌ Amendment cancelled by user");
                return Ok(());
            }
            
            // 6. Apply amendments (reuse AmendCommand logic)
            self.apply_amendments(amendments).await?;
            println!("✅ Commit messages improved successfully!");
        } else {
            println!("✨ All commit messages are already well-formatted!");
        }
        
        Ok(())
    }
}
```

### 6. Error Handling & Safety

#### API Key Validation
- Check for `CLAUDE_API_KEY` or `ANTHROPIC_API_KEY` environment variables
- Provide clear error message if missing
- Support loading from `.env` files if present

#### Safety Checks (Reuse from AmendCommand)
- Ensure working directory is clean
- Validate commits exist and are amendable
- Check for conflicts with remote branches
- Provide rollback capability

#### Network Error Handling
- Handle API rate limits gracefully
- Retry logic for transient failures  
- Offline mode fallback (skip Claude, just show current commits)
- Timeout configuration

### 7. User Experience

#### Progress Indicators
```
🔍 Analyzing repository...
🤖 Generating improvements with Claude AI...
📝 Found 3 commits that could be improved:

  abc1234 → feat: add user authentication module
  def5678 → fix: resolve memory leak in parser
  ghi9012 → docs: update API documentation

❓ Apply these amendments? [y/N]
```

#### Configuration Options
- Model selection (support different Claude models)
- Custom prompt templates
- Skip confirmation for CI/automation
- Output formatting options

### 8. Testing Strategy

#### Unit Tests
- Mock Claude API responses
- Test amendment generation logic
- Validate YAML parsing/generation
- Test error handling scenarios

#### Integration Tests  
- End-to-end workflow with test repositories
- API key validation
- Network failure scenarios
- Git repository state validation

#### Golden Tests
- Snapshot test for generated amendments
- Consistent output format validation
- Regression testing for prompt changes

### 9. Documentation

#### Command Help
```
USAGE:
    omni-dev git commit message twiddle [OPTIONS] [COMMIT_RANGE]

ARGS:
    <COMMIT_RANGE>    Commit range to analyze (e.g., HEAD~3..HEAD) [default: HEAD~5..HEAD]

OPTIONS:
    Phase 1 & 2 (Implemented):
        --model <MODEL>           Claude model to use [default: claude-3-5-sonnet-20241022]
        --auto-apply             Skip confirmation and apply changes automatically
        --save-only <FILE>       Save amendments to file without applying
    
    Phase 3 (Contextual Intelligence - Planned):
        --use-context            Use project context for enhanced suggestions [default: true]
        --context-dir <DIR>      Path to custom context directory [default: .omni-dev]
        --work-context <TEXT>    Specify work context (e.g., "feature: user auth")
        --branch-context <TEXT>  Override detected branch context
        --no-context             Disable contextual analysis (Phase 1 behavior)
    
    -h, --help                   Print help information

EXAMPLES:
    # Basic usage (Phase 1 & 2)
    omni-dev git commit message twiddle HEAD~3..HEAD
    
    # With enhanced context (Phase 3)
    omni-dev git commit message twiddle HEAD~3..HEAD --use-context
    
    # Custom work context
    omni-dev git commit message twiddle --work-context "feature: user authentication system"
    
    # Use custom context directory
    omni-dev git commit message twiddle --context-dir .config/commits
    
    # Disable context (fallback to basic mode)
    omni-dev git commit message twiddle --no-context
    
    # Save amendments without applying
    omni-dev git commit message twiddle --save-only amendments.yaml
    
    # Auto-apply without confirmation (useful for CI)
    omni-dev git commit message twiddle --auto-apply
```

#### Environment Setup
```bash
# Set Claude API key
export CLAUDE_API_KEY="your-api-key-here"
# OR
export ANTHROPIC_API_KEY="your-api-key-here"

# Optional: Configure default model
export OMNI_DEV_CLAUDE_MODEL="claude-3-5-sonnet-20241022"
```

### 10. Future Enhancements

#### Advanced Features
- Custom prompt templates via config files
- Batch processing for large repositories
- Integration with conventional commit linting
- Git hook integration for automatic improvement
- Team-specific style guidelines

#### Performance Optimizations
- Caching for repeated repository analysis
- Incremental analysis for large histories
- Rate limit management and queuing

#### Edge Case Scenarios (Future Phase 4)
- **Large commit ranges (>50 commits)**: Implement intelligent chunking with context overlap
- **API token limits**: Monitor input size and split requests near Claude's context window limits
- **Partial API failures**: Handle cases where Claude only processes some commits successfully
- **Network connectivity**: Graceful degradation and retry logic for intermittent failures
- **Massive repositories**: Memory-efficient streaming for repositories with thousands of commits
- **Concurrent usage**: Handle multiple twiddle commands running simultaneously
- **Git state corruption**: Detect and recover from interrupted git operations
- **API rate limiting**: Implement exponential backoff and request queuing
- **Invalid commit ranges**: Better validation and user feedback for malformed ranges
- **Empty amendment responses**: Handle cases where Claude determines no improvements needed

### 11. Implementation Priority

#### Phase 1: Core Implementation ✅ COMPLETED
1. ✅ Add `TwiddleCommand` structure to CLI (`src/cli/git.rs:54,75`)
2. ✅ Implement basic Claude API integration (`src/claude/client.rs:46-154`)
3. ✅ Create repository view generation (`src/cli/git.rs:286-349`)
4. ✅ Add amendment application logic (`src/cli/git.rs:364-381`)
5. ✅ Basic error handling and validation (`src/claude/error.rs:6-33`)

#### Phase 2: User Experience ✅ COMPLETED  
1. ✅ Progress indicators and confirmation prompts (`src/cli/git.rs:247,256,270`)
2. ✅ Preview functionality for amendments (`src/cli/git.rs:324-342`)
3. ✅ Comprehensive error messages (`src/claude/error.rs`)
4. ✅ Help documentation and examples (CLI help text, templates)

#### Phase 3: Contextual Intelligence 🔄 **PLANNED**
1. 🔄 Project-level context discovery system (`.omni-dev/`, `.gitmessage`, docs parsing)
2. 🔄 Branch analysis and work pattern detection
3. 🔄 Multi-commit range context understanding
4. 🔄 File-based architectural context recognition  
5. 🔄 Enhanced Claude prompting with project-specific guidelines
6. 🔄 Context-aware CLI options (`--use-context`, `--work-context`)
7. 🔄 Configuration file standards (commit-guidelines.md, scopes.yaml)

#### Phase 4: Polish & Testing 🔄 **TODO**
1. 🔄 Comprehensive test suite (unit, integration, golden tests)
2. 🔄 Performance optimizations for context discovery
3. 🔄 Advanced configuration options and templates
4. 🔄 Documentation and usage examples

#### Phase 5: Edge Case Handling 🔄 **TODO**
1. 🔄 Large commit range optimization (chunking strategies)
2. 🔄 API token limit management and context window monitoring
3. 🔄 Partial failure recovery and retry mechanisms
4. 🔄 Network resilience and offline fallback modes
5. 🔄 Memory usage optimization for massive repositories
6. 🔄 Concurrent processing safety and lock management
7. 🔄 Git repository state validation and corruption recovery

## File Structure Changes

### Phase 1 & 2 - ✅ COMPLETED
```
src/
├── cli/
│   └── git.rs                 # ✅ Added TwiddleCommand (lines 54,75,242-381)
├── claude/                    # ✅ NEW MODULE IMPLEMENTED
│   ├── mod.rs                 # ✅ Claude client exports
│   ├── client.rs              # ✅ Full Claude API client implementation  
│   ├── prompts.rs             # ✅ System & user prompt templates
│   └── error.rs               # ✅ Claude-specific error handling
├── data/
│   └── amendments.rs          # ✅ Existing - reused as planned
├── git/
│   └── amendment.rs           # ✅ Existing - reused AmendmentHandler
├── templates/
│   └── commit-twiddle.md      # ✅ Claude Code template
└── .claude/commands/
    └── commit-twiddle.md      # ✅ Claude Code command definition

docs/plan/twiddle.md          # ✅ This file (updated with contextual plan)
Cargo.toml                    # ✅ Added reqwest, tokio dependencies
```

### Phase 3 Contextual Intelligence - 🔄 PLANNED  
```
src/
├── claude/
│   ├── context/               # 🔄 NEW - Contextual intelligence module
│   │   ├── mod.rs            # 🔄 Context system exports
│   │   ├── discovery.rs      # 🔄 Project context discovery
│   │   ├── branch.rs         # 🔄 Branch analysis and patterns
│   │   ├── files.rs          # 🔄 File-based context recognition
│   │   └── patterns.rs       # 🔄 Work pattern detection
│   └── prompts.rs            # 🔄 Enhanced with contextual prompting
├── cli/
│   └── git.rs                # 🔄 Extended TwiddleCommand with context options
└── data/
    ├── context.rs            # 🔄 NEW - Context data structures  
    └── amendments.rs         # 🔄 Enhanced with context validation

# Project Configuration Examples (User-created)
.omni-dev/                    # 🔄 NEW - Project-specific context directory
├── commit-guidelines.md      # 🔄 Project commit conventions
├── commit-template.txt       # 🔄 Default commit message template
├── scopes.yaml              # 🔄 Valid scopes and descriptions
└── context/
    └── feature-contexts/     # 🔄 Feature-specific context files

.gitmessage                   # 🔄 Standard git commit template support
```

## Summary

This plan provides a comprehensive roadmap for implementing the `twiddle` command that evolves from a basic Claude AI-powered commit formatter into an intelligent, context-aware commit improvement system.

**Current State (Phases 1 & 2 - Completed)**: Production-ready twiddle command with Claude AI integration, user confirmation, and comprehensive error handling.

**Future Vision (Phase 3 - Contextual Intelligence)**: Transform twiddle into a project-aware system that:
- Understands project-specific commit conventions and guidelines  
- Analyzes branch naming patterns and work types
- Recognizes architectural contexts and file purposes
- Provides context-enhanced Claude prompting for superior suggestions
- Supports popular open source conventions out-of-the-box

The contextual system respects existing project standards while providing intelligent enhancements, making omni-dev's twiddle command a powerful tool for maintaining consistent, high-quality commit messages across diverse development workflows.

**Architecture Philosophy**: The design maintains backward compatibility while enabling progressive enhancement—users benefit from basic functionality immediately, with contextual intelligence available when configured. The system gracefully handles missing context, ensuring robust operation in any environment.