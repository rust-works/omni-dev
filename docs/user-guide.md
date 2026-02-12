# omni-dev User Guide

A comprehensive guide to using omni-dev's AI-powered commit message
intelligence.

## Table of Contents

1. [Getting Started](#getting-started)
2. [Core Concepts](#core-concepts)
3. [Command Reference](#command-reference)
4. [Contextual Intelligence](#contextual-intelligence)
5. [Workflows](#workflows)
6. [Advanced Usage](#advanced-usage)
7. [Best Practices](#best-practices)

## Getting Started

### Prerequisites

1. **Install omni-dev**

   ```bash
   cargo install omni-dev
   ```

2. **Set up Claude API Key**

   ```bash
   # Get your API key from https://console.anthropic.com/
   export CLAUDE_API_KEY="your-api-key-here"
   
   # Add to your shell profile for persistence
   echo 'export CLAUDE_API_KEY="your-api-key-here"' >> ~/.bashrc
   ```

3. **Verify Installation**

   ```bash
   omni-dev --version
   omni-dev help-all  # See all available commands
   ```

### First Use

Transform your commit messages and create professional PRs in 4 steps:

```bash
# 1. Navigate to your git repository
cd your-project

# 2. Improve recent commits with AI intelligence
omni-dev git commit message twiddle 'HEAD~5..HEAD' --use-context

# 3. Review and apply the suggestions
# The tool will show you before/after and ask for confirmation

# 4. Create a professional PR with AI-generated description
omni-dev git branch create pr
# Analyzes your commits and generates comprehensive PR description
```

## Core Concepts

### The Four-Command Workflow

omni-dev follows a simple analyze → improve → apply → ship workflow:

```bash
# 📊 ANALYZE: See detailed commit information
omni-dev git commit message view 'HEAD~3..HEAD'

# 🤖 IMPROVE: Get AI-powered suggestions
omni-dev git commit message twiddle 'HEAD~3..HEAD' --use-context

# ✏️ APPLY: Apply specific amendments manually
omni-dev git commit message amend amendments.yaml

# 🚀 SHIP: Create professional PR with AI description
omni-dev git branch create pr
```

### Key Benefits

- **Contextual**: Understands your project structure and conventions
- **Safe**: Always asks for confirmation before making changes
- **Intelligent**: Uses actual code changes, not just file names
- **Batch-Aware**: Handles large commit ranges efficiently
- **Professional**: Generates conventional commit format

## Command Reference

### `twiddle` - AI-Powered Improvement

The main command for improving commit messages:

```bash
# Basic usage with contextual intelligence
omni-dev git commit message twiddle 'origin/main..HEAD' --use-context

# Common options
omni-dev git commit message twiddle [RANGE] [OPTIONS]
```

**Key Options:**

| Option | Description | Example |
|--------|-------------|---------|
| `--use-context` | Enable AI contextual intelligence | `--use-context` |
| `--concurrency N` | Number of parallel commit processors (default: 4) | `--concurrency 2` |
| `--no-coherence` | Skip cross-commit coherence refinement pass | `--no-coherence` |
| `--auto-apply` | Apply changes without confirmation | `--auto-apply` |
| `--save-only FILE` | Save suggestions to file instead of applying | `--save-only suggestions.yaml` |
| `--context-dir PATH` | Custom context directory | `--context-dir ./config` |
| `--no-context` | Disable contextual features | `--no-context` |
| `--edit` | Edit amendments in external editor before applying | `--edit` |

**Commit Range Examples:**

```bash
# Last 5 commits
omni-dev git commit message twiddle 'HEAD~5..HEAD'

# All commits on current branch vs main
omni-dev git commit message twiddle 'origin/main..HEAD'

# Specific range between commits
omni-dev git commit message twiddle 'abc123..def456'

# Single commit
omni-dev git commit message twiddle 'HEAD^..HEAD'
```

### `view` - Analysis and Inspection

Analyze commits without making changes:

```bash
# Analyze recent commits (YAML output)
omni-dev git commit message view 'HEAD~3..HEAD'

# Analyze current branch vs main
omni-dev git branch info main
```

The output includes:

- Commit metadata (hash, author, date, message)
- File changes and diff statistics
- Conventional commit type detection
- Proposed improvements
- Remote branch tracking

### `amend` - Manual Application

Apply specific amendments from a YAML file:

```bash
# Apply amendments from file
omni-dev git commit message amend amendments.yaml
```

YAML format:

```yaml
amendments:
  - commit: "abc123def456..."
    message: |
      feat(auth): implement OAuth2 authentication
      
      Add comprehensive authentication system:
      - OAuth2 integration with Google/GitHub
      - JWT token management
      - User session handling
      - Role-based access control
```

### `create pr` - AI-Powered Pull Request Creation

Generate professional pull requests with AI-analyzed descriptions:

```bash
# Create PR with AI-generated description
omni-dev git branch create pr

# Create PR for specific base branch
omni-dev git branch create pr main

# Common options
omni-dev git branch create pr [BASE_BRANCH] [OPTIONS]
```

**Key Options:**

| Option | Description | Example |
|--------|-------------|---------|
| `--auto-apply` | Create/update PR without confirmation | `--auto-apply` |
| `--save-only FILE` | Save PR details to YAML file instead of creating | `--save-only pr-details.yaml` |

**What it does:**

- Analyzes your branch commits and changes
- Generates comprehensive PR title and description using AI
- Fills in PR template sections automatically
- Handles both new PR creation and existing PR updates
- Creates YAML file with structured PR details for editing

**Requirements:**

- Clean working directory (no uncommitted changes)
- GitHub CLI (`gh`) installed and authenticated
- Branch pushed to remote (will push automatically if needed)
- Claude API key configured

**Example Output:**

The command creates a `pr-details.yaml` file with structure like:

```yaml
title: "feat(auth): implement comprehensive OAuth2 authentication system"
description: |
  # Pull Request

  ## Description
  This PR implements a comprehensive OAuth2 authentication system that enables
  users to sign in using Google and GitHub providers. The implementation includes
  secure token management, session handling, and role-based access control.

  ## Type of Change
  - [x] New feature (non-breaking change which adds functionality)

  ## Changes Made
  - Added OAuth2 integration with Google and GitHub providers
  - Implemented JWT token validation and refresh mechanisms
  - Created user session management system
  - Added role-based access control middleware
  - Updated authentication documentation

  ## Testing
  - [x] All existing tests pass
  - [x] New tests added for authentication flows
  - [x] Manual testing performed with both providers

  ## Additional Notes
  This implementation follows OAuth2 best practices and includes comprehensive
  error handling for edge cases.
```

## Contextual Intelligence

### Overview

Contextual intelligence makes omni-dev understand your project to provide better suggestions:

- **Project Context**: Conventions from `.omni-dev/` configuration
- **Branch Context**: Work type from branch naming patterns
- **File Context**: Architectural understanding of changed files
- **Pattern Context**: Recognition of work patterns across commits

### Setting Up Context

#### 1. Create Context Directory

```bash
mkdir .omni-dev
```

#### 2. Define Project Scopes (`.omni-dev/scopes.yaml`)

Tell omni-dev about your project's areas:

```yaml
scopes:
  - name: "auth"
    description: "Authentication and authorization systems"
    examples: 
      - "auth: add OAuth2 support"
      - "auth: fix token validation" 
    file_patterns:
      - "src/auth/**"
      - "auth.rs"
      - "middleware/auth.rs"

  - name: "api"  
    description: "REST API endpoints and handlers"
    examples:
      - "api: add user endpoints"
      - "api: improve error handling"
    file_patterns:
      - "src/api/**"
      - "handlers/**"
      - "routes/**"

  - name: "ui"
    description: "User interface components"
    examples:
      - "ui: add responsive navigation"  
      - "ui: fix mobile layout"
    file_patterns:
      - "src/components/**"
      - "*.vue"
      - "*.tsx"

  - name: "docs"
    description: "Documentation and guides"
    examples:
      - "docs: add API reference"
      - "docs: update installation guide"
    file_patterns:
      - "docs/**"
      - "*.md"
      - "README*"
```

#### 3. Set Commit Guidelines (`.omni-dev/commit-guidelines.md`)

Define your project's commit message standards:

```markdown
# Project Commit Guidelines

## Format
Use conventional commits: `type(scope): description`

## Types We Use
- `feat` - New features  
- `fix` - Bug fixes
- `docs` - Documentation changes
- `refactor` - Code restructuring
- `test` - Adding tests
- `chore` - Build/tooling changes

## Style Rules
- Keep subject line under 50 characters
- Use imperative mood: "Add feature" not "Added feature"  
- Capitalize first letter of description
- No period at end of subject line

## Our Scopes
- `auth` - Authentication systems
- `api` - Backend API changes
- `ui` - Frontend interface
- `db` - Database changes
- `deploy` - Deployment/infrastructure

## Examples
```

feat(auth): add OAuth2 Google integration
fix(api): resolve rate limiting edge case
docs(readme): update installation instructions
refactor(ui): extract common button component

```

### Branch Context Detection

omni-dev automatically detects work type from branch names:

| Branch Pattern | Detected Type | Example |
|----------------|---------------|---------|
| `feature/auth-system` | feature | Feature development |
| `fix/login-bug` | fix | Bug fix |
| `docs/api-guide` | docs | Documentation |
| `refactor/user-service` | refactor | Code restructuring |
| `JIRA-123-user-auth` | feature | Ticket-based |
| `username/feature-name` | feature | User branches |

### Intelligent Verbosity

omni-dev adjusts message detail based on change significance:

- **Comprehensive**: Major features, architectural changes
  - Multi-paragraph descriptions
  - Bulleted feature lists  
  - Impact statements

- **Detailed**: Moderate changes, multi-file updates
  - Subject + explanatory body
  - Key change highlights

- **Concise**: Minor changes, single-file updates
  - Clear conventional format
  - Essential information only

## Workflows

### Feature Branch Cleanup

Clean up commits before merging:

```bash
# 1. Work on feature branch with quick commits
git checkout -b feature/user-dashboard
git commit -m "wip"
git commit -m "fix stuff"  
git commit -m "add more"

# 2. Before merging, improve all commit messages
omni-dev git commit message twiddle 'main..HEAD' --use-context

# 3. Review suggestions and apply
# ✅ Professional commit history ready for review
```

### Complete Feature Development Workflow

End-to-end workflow from feature development to PR creation:

```bash
# 1. Create and work on feature branch
git checkout -b feature/user-authentication
# ... make changes and commits ...

# 2. Improve commit messages with AI
omni-dev git commit message twiddle 'main..HEAD' --use-context

# 3. Create professional PR with AI-generated description
omni-dev git branch create pr

# ✅ Complete: clean commits + comprehensive PR ready for team review
```

### PR Creation and Updates

Handle PR creation and updates efficiently:

```bash
# Create new PR with AI-generated description
omni-dev git branch create pr main

# If PR already exists, update it with new description
omni-dev git branch create pr --auto-apply

# Save PR details for review before creating
omni-dev git branch create pr --save-only review-pr.yaml
# Review and edit the file...
# Then create manually using GitHub CLI or web interface
```

### Collaborative PR Workflow

Work with existing PRs and team feedback:

```bash
# Update existing PR after new commits
git add . && git commit -m "address review feedback"
omni-dev git branch create pr  # Updates existing PR

# Generate PR description without creating (for draft PRs)
omni-dev git branch create pr --save-only draft-pr.yaml
# Use the content to update draft PR manually
```

### Large Codebase Migration

Handle large commit ranges efficiently:

```bash
# Process 100+ commits with parallel processing
omni-dev git commit message twiddle 'HEAD~100..HEAD' --concurrency 5

# Save suggestions for review before applying
omni-dev git commit message twiddle 'HEAD~50..HEAD' --save-only review.yaml

# Review the file, then apply manually
omni-dev git commit message amend review.yaml
```

### Legacy Repository Cleanup

Improve old commit messages:

```bash
# Analyze what needs improvement
omni-dev git commit message view 'HEAD~20..HEAD'

# Apply contextual improvements
omni-dev git commit message twiddle 'HEAD~20..HEAD' --use-context

# For very old commits, might need specific handling
git rebase -i HEAD~20  # Interactive rebase first if needed
```

### Team Onboarding

Set up consistent commit standards:

```bash
# 1. Set up project context (one-time setup)
mkdir .omni-dev
# Create scopes.yaml and commit-guidelines.md

# 2. Add to team documentation
echo "Use: omni-dev git commit message twiddle 'main..HEAD' --use-context" >> CONTRIBUTING.md

# 3. Include in CI/PR checks
# Add validation that commit messages follow conventions
```

## Advanced Usage

### Custom Context Directory

Use a different location for context files:

```bash
# Use custom context directory
omni-dev git commit message twiddle 'HEAD~5..HEAD' --context-dir ./project-config

# Context files would be in:
# ./project-config/scopes.yaml
# ./project-config/commit-guidelines.md
```

### Concurrency Configuration

Adjust parallel processing based on your needs:

```bash
# Lower concurrency for complex commits (reduces API load)
omni-dev git commit message twiddle 'HEAD~20..HEAD' --concurrency 2

# Higher concurrency for faster processing
omni-dev git commit message twiddle 'HEAD~20..HEAD' --concurrency 8

# Skip coherence pass for independent commits
omni-dev git commit message twiddle 'HEAD~10..HEAD' --no-coherence
```

### Integration with Git Hooks

Set up automatic improvement in git hooks:

```bash
# .git/hooks/pre-push (make executable)
#!/bin/bash
echo "🤖 Analyzing commit messages..."
omni-dev git commit message view 'origin/main..HEAD' --quiet || {
    echo "❌ Commit analysis failed"
    echo "💡 Consider running: omni-dev git commit message twiddle 'origin/main..HEAD' --use-context"
    exit 1
}
```

### Save and Review Workflow

For high-stakes changes, save suggestions first:

```bash
# 1. Save suggestions to file
omni-dev git commit message twiddle 'HEAD~10..HEAD' --save-only suggestions.yaml

# 2. Review the suggestions file
cat suggestions.yaml

# 3. Edit if needed, then apply
omni-dev git commit message amend suggestions.yaml
```

## Best Practices

### 1. Use Contextual Intelligence

Always use `--use-context` for best results:

```bash
# ✅ Good - uses project context
omni-dev git commit message twiddle 'main..HEAD' --use-context

# ⚠️ Basic - misses project-specific intelligence  
omni-dev git commit message twiddle 'main..HEAD'
```

### 2. Set Up Project Context

Invest time in setting up `.omni-dev/` configuration:

- Define meaningful scopes for your project
- Document your commit conventions
- Include file pattern matching for accuracy

### 3. Batch Size Guidelines

| Repository Size | Suggested Batch Size | Reasoning |
|----------------|---------------------|-----------|
| Small projects | 6-8 commits | Faster processing |
| Medium projects | 4-5 commits | Balanced accuracy/speed |
| Large projects | 2-3 commits | More context per batch |
| Complex changes | 1-2 commits | Maximum accuracy |

### 4. Review Before Applying

For important branches, always review suggestions:

```bash
# Save first, review, then apply
omni-dev git commit message twiddle 'main..HEAD' --save-only review.yaml
# Review the file...
omni-dev git commit message amend review.yaml
```

### 5. Clean Working Directory

Always ensure clean working directory:

```bash
# Check status first
git status

# Commit or stash changes before running omni-dev
git add . && git commit -m "temp" || git stash
omni-dev git commit message twiddle 'HEAD~5..HEAD' --use-context
```

### 6. API Key Security

Keep your Claude API key secure:

```bash
# Use environment variables, not command line arguments
export CLAUDE_API_KEY="sk-..."

# Add to .env files (not committed to git)
echo "CLAUDE_API_KEY=sk-..." >> .env

# Don't hardcode in scripts
```

### 7. Integration with Team Workflow

Make it part of your team's process:

```bash
# Add to PR template
echo "- [ ] Run \`omni-dev git commit message twiddle 'main..HEAD' --use-context\`" >> .github/pull_request_template.md

# Document in CONTRIBUTING.md
echo "Before creating a PR, clean up commit messages with omni-dev" >> CONTRIBUTING.md

# Add PR creation to workflow
echo "Create PR with: \`omni-dev git branch create pr\`" >> CONTRIBUTING.md
```

### 8. PR Creation Best Practices

Optimize your PR creation workflow:

```bash
# ✅ Good - Clean commits first, then create PR
omni-dev git commit message twiddle 'main..HEAD' --use-context
omni-dev git branch create pr

# ✅ Good - Review PR details before creating
omni-dev git branch create pr --save-only review.yaml
# Edit file if needed, then use GitHub CLI or web interface

# ⚠️ Caution - Ensure working directory is clean
git status  # Check for uncommitted changes first

# ✅ Good - Use base branch when not default
omni-dev git branch create pr develop  # For non-main base branches
```

## Troubleshooting

See [Troubleshooting Guide](troubleshooting.md) for common issues and solutions.

## Need Help?

- 📖 [Configuration Guide](configuration.md) - Detailed setup instructions
- 🔧 [Troubleshooting](troubleshooting.md) - Common issues  
- 📝 [Examples](examples.md) - Real-world usage examples
- 💬 [GitHub Discussions](https://github.com/rust-works/omni-dev/discussions) - Community support
- 🐛 [GitHub Issues](https://github.com/rust-works/omni-dev/issues) - Bug reports and features
