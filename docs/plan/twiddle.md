# Twiddle Command Implementation Plan

## Overview

The `omni-dev git commit message twiddle` command is a new feature that combines the functionality of the existing `view` and `amend` commands with Claude AI integration to automatically generate commit message improvements.

## Command Flow

```
omni-dev git commit message twiddle [COMMIT_RANGE]
    ↓
1. Execute view command logic → YAML output
    ↓  
2. Send YAML to Claude API → Amendment suggestions
    ↓
3. Execute amend command logic → Apply amendments
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

### 2. TwiddleCommand Implementation

```rust
/// Twiddle command options
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

### 5. Implementation Flow

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
        --model <MODEL>           Claude model to use [default: claude-3-5-sonnet-20241022]
        --auto-apply             Skip confirmation and apply changes automatically
        --save-only <FILE>       Save amendments to file without applying
    -h, --help                   Print help information

EXAMPLES:
    # Improve last 3 commits
    omni-dev git commit message twiddle HEAD~3..HEAD
    
    # Improve all commits since main branch
    omni-dev git commit message twiddle main..HEAD
    
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

#### Phase 1: Core Implementation
1. Add `TwiddleCommand` structure to CLI
2. Implement basic Claude API integration
3. Create repository view generation
4. Add amendment application logic
5. Basic error handling and validation

#### Phase 2: User Experience
1. Progress indicators and confirmation prompts
2. Preview functionality for amendments
3. Comprehensive error messages
4. Help documentation and examples

#### Phase 3: Polish & Testing
1. Comprehensive test suite
2. Performance optimizations
3. Advanced configuration options
4. Documentation and examples

#### Phase 4: Edge Case Handling
1. Large commit range optimization (chunking strategies)
2. API token limit management and context window monitoring
3. Partial failure recovery and retry mechanisms
4. Network resilience and offline fallback modes
5. Memory usage optimization for massive repositories
6. Concurrent processing safety and lock management
7. Git repository state validation and corruption recovery

## File Structure Changes

```
src/
├── cli/
│   └── git.rs                 # Add TwiddleCommand
├── claude/                    # NEW MODULE  
│   ├── mod.rs                 # Claude client and API integration
│   ├── prompts.rs             # Prompt templates and engineering
│   └── error.rs               # Claude-specific error handling
├── data/
│   └── amendments.rs          # Existing - no changes needed
└── git/
    └── amendment.rs           # Existing - reuse logic

docs/plan/twiddle.md          # This file
```

This plan provides a comprehensive roadmap for implementing the `twiddle` command that seamlessly integrates Claude AI capabilities with the existing omni-dev architecture while maintaining code quality, safety, and user experience standards.