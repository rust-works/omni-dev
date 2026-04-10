# omni-dev User Guide

A comprehensive guide to using omni-dev's AI-powered commit message
intelligence.

## Table of Contents

1. [Getting Started](#getting-started)
2. [Core Concepts](#core-concepts)
3. [Command Reference](#command-reference)
4. [Atlassian Integration](#atlassian---jira-and-confluence-integration)
5. [Contextual Intelligence](#contextual-intelligence)
6. [Workflows](#workflows)
7. [Advanced Usage](#advanced-usage)
8. [Best Practices](#best-practices)

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

### `atlassian` - JIRA and Confluence Integration

Read, edit, and manage JIRA issues and Confluence pages from the command line.
Content is represented as JFM (JIRA-Flavored Markdown) with YAML frontmatter,
enabling round-trip editing between your editor and Atlassian Cloud.

See the [JFM Specification](plan/jfm.md) for full technical details on the
markdown format.

#### Authentication Setup

Configure your Atlassian Cloud credentials:

```bash
# Interactive credential setup (prompts for instance URL, email, API token)
omni-dev atlassian auth login

# Verify credentials work
omni-dev atlassian auth status
```

Credentials are stored in `~/.omni-dev/settings.json`. You can also use
environment variables:

```bash
export ATLASSIAN_INSTANCE_URL=https://myorg.atlassian.net
export ATLASSIAN_EMAIL=you@example.com
export ATLASSIAN_API_TOKEN=your-token
```

Environment variables take precedence over the settings file.

#### JIRA: Reading and Writing Issues

```bash
# Read an issue as JFM markdown
omni-dev atlassian jira read PROJ-123
omni-dev atlassian jira read PROJ-123 -o issue.md
omni-dev atlassian jira read PROJ-123 --format adf   # raw ADF JSON

# Write changes back (prompts for confirmation)
omni-dev atlassian jira write PROJ-123 issue.md
omni-dev atlassian jira write PROJ-123 issue.md --force
omni-dev atlassian jira write PROJ-123 issue.md --dry-run

# Interactive edit: fetch -> $EDITOR -> push
omni-dev atlassian jira edit PROJ-123
```

The edit command opens an interactive loop:
1. Fetches the issue and writes JFM to a temp file
2. Prompts: `[A]ccept, [S]how, [E]dit, or [Q]uit?`
3. On accept, converts back to ADF and pushes changes

JFM output example:

```markdown
---
type: jira
instance: https://myorg.atlassian.net
key: PROJ-123
summary: Implement user authentication
status: In Progress
issue_type: Story
assignee: Alice Smith
labels:
  - backend
---

This story covers the implementation of OAuth2-based authentication...
```

#### JIRA: Search

Search issues using JQL or convenience flags:

```bash
# Raw JQL
omni-dev atlassian jira search --jql "project = PROJ AND status = Open"

# Convenience flags (combined with AND)
omni-dev atlassian jira search --project PROJ --status "In Progress"
omni-dev atlassian jira search --assignee alice --limit 100

# Fetch all results (auto-paginates)
omni-dev atlassian jira search --jql "project = PROJ" --limit 0
```

Output is a formatted table: `KEY | STATUS | ASSIGNEE | SUMMARY`.

#### JIRA: Create Issues

Create issues from JFM markdown or CLI flags:

```bash
# From JFM file (project, type, summary from frontmatter)
omni-dev atlassian jira create issue.md

# From CLI flags
omni-dev atlassian jira create issue.md --project PROJ --type Bug --summary "Fix login"

# From ADF JSON (all metadata via flags)
omni-dev atlassian jira create body.json --format adf --project PROJ --summary "Title"

# Preview without creating
omni-dev atlassian jira create issue.md --dry-run
```

Prints the created issue key (e.g., `PROJ-124`) to stdout.

#### JIRA: Transitions

List and execute workflow transitions:

```bash
# List available transitions
omni-dev atlassian jira transition PROJ-123

# Execute a transition by name (case-insensitive)
omni-dev atlassian jira transition PROJ-123 "In Progress"

# Execute by ID
omni-dev atlassian jira transition PROJ-123 21
```

#### JIRA: Comments

```bash
# List comments on an issue
omni-dev atlassian jira comment list PROJ-123

# Add a comment from a file
omni-dev atlassian jira comment add PROJ-123 comment.md

# Add from stdin
echo "This is a comment" | omni-dev atlassian jira comment add PROJ-123

# Add ADF JSON comment
omni-dev atlassian jira comment add PROJ-123 body.json --format adf
```

#### JIRA: Delete Issues

```bash
# Delete with confirmation prompt
omni-dev atlassian jira delete PROJ-123

# Skip confirmation
omni-dev atlassian jira delete PROJ-123 --force
```

#### JIRA: Projects

```bash
# List all accessible projects
omni-dev atlassian jira project list
omni-dev atlassian jira project list --limit 100
```

#### JIRA: Fields

```bash
# List all field definitions
omni-dev atlassian jira field list

# Search by name
omni-dev atlassian jira field list --search "story"

# Show options for a custom field (auto-discovers context)
omni-dev atlassian jira field options --field-id customfield_10001

# Specify context explicitly
omni-dev atlassian jira field options --field-id customfield_10001 --context-id 12345
```

#### JIRA: Agile Boards

```bash
# List boards
omni-dev atlassian jira board list
omni-dev atlassian jira board list --project PROJ --type scrum

# List issues on a board
omni-dev atlassian jira board issues --board-id 1
omni-dev atlassian jira board issues --board-id 1 --jql "status = Open"
```

#### JIRA: Sprints

```bash
# List sprints for a board
omni-dev atlassian jira sprint list --board-id 1
omni-dev atlassian jira sprint list --board-id 1 --state active

# List issues in a sprint
omni-dev atlassian jira sprint issues --sprint-id 10

# Add issues to a sprint
omni-dev atlassian jira sprint add --sprint-id 10 --issues PROJ-1,PROJ-2,PROJ-3
```

#### JIRA: Issue Links

```bash
# List links on an issue (shows link IDs)
omni-dev atlassian jira link list PROJ-123

# List available link types
omni-dev atlassian jira link types

# Create a link
omni-dev atlassian jira link create --type Blocks --inward PROJ-1 --outward PROJ-2

# Remove a link by ID (get IDs from `link list`)
omni-dev atlassian jira link remove --link-id 12345

# Link an issue to an epic
omni-dev atlassian jira link epic --epic EPIC-1 --issue PROJ-2
```

#### JIRA: Changelog

View change history for one or more issues:

```bash
omni-dev atlassian jira changelog --keys PROJ-1
omni-dev atlassian jira changelog --keys PROJ-1,PROJ-2 --limit 100
```

#### JIRA: Attachments

```bash
# Download all attachments
omni-dev atlassian jira attachment download --key PROJ-123
omni-dev atlassian jira attachment download --key PROJ-123 --output-dir ./files

# Filter by filename
omni-dev atlassian jira attachment download --key PROJ-123 --filter screenshot

# Download only images (png, jpeg, gif, svg, webp)
omni-dev atlassian jira attachment images --key PROJ-123
omni-dev atlassian jira attachment images --key PROJ-123 --output-dir ./images
```

#### Confluence: Reading and Writing Pages

```bash
# Read a page as JFM markdown
omni-dev atlassian confluence read 12345
omni-dev atlassian confluence read 12345 -o page.md
omni-dev atlassian confluence read 12345 --format adf

# Write changes back
omni-dev atlassian confluence write 12345 page.md
omni-dev atlassian confluence write 12345 page.md --force
omni-dev atlassian confluence write 12345 page.md --dry-run

# Interactive edit
omni-dev atlassian confluence edit 12345
```

Confluence JFM output example:

```markdown
---
type: confluence
instance: https://myorg.atlassian.net
page_id: "12345"
title: Architecture Overview
space_key: ENG
status: current
version: 7
---

# Architecture Overview

Page body content here...
```

#### Confluence: Search

Search pages using CQL or convenience flags:

```bash
# Raw CQL
omni-dev atlassian confluence search --cql "space = ENG AND title ~ 'auth'"

# Convenience flags
omni-dev atlassian confluence search --space ENG
omni-dev atlassian confluence search --title architecture
omni-dev atlassian confluence search --space ENG --title auth --limit 100
```

#### Confluence: Create Pages

```bash
# From JFM file
omni-dev atlassian confluence create page.md

# From CLI flags
omni-dev atlassian confluence create page.md --space ENG --title "New Page"

# With parent page
omni-dev atlassian confluence create page.md --space ENG --title "Child" --parent 12345

# Preview
omni-dev atlassian confluence create page.md --dry-run
```

#### Confluence: Delete Pages

```bash
# Delete (moves to trash, prompts for confirmation)
omni-dev atlassian confluence delete 12345

# Skip confirmation
omni-dev atlassian confluence delete 12345 --force

# Permanently purge (requires space admin)
omni-dev atlassian confluence delete 12345 --force --purge
```

#### Offline Format Conversion

Convert between JFM markdown and ADF JSON locally without credentials:

```bash
# Markdown to ADF JSON
omni-dev atlassian convert to-adf issue.md
omni-dev atlassian convert to-adf issue.md --compact

# ADF JSON to markdown
omni-dev atlassian convert from-adf issue.json

# Pipe for inspection
cat issue.md | omni-dev atlassian convert to-adf | jq .
```

#### Auto-Pagination

All commands that query paginated endpoints auto-paginate transparently.
Use `--limit` to control how many results are fetched:

```bash
# Default: up to 50 results
omni-dev atlassian jira search --project PROJ

# Fetch more
omni-dev atlassian jira search --project PROJ --limit 200

# Fetch all (no limit)
omni-dev atlassian jira search --project PROJ --limit 0
```

#### JFM Markdown Syntax

JFM supports standard GitHub-Flavored Markdown plus directives for
JIRA-specific elements:

**Standard markdown**: headings, bold, italic, code, strikethrough, links,
images, lists, task lists, tables, code blocks, blockquotes, horizontal rules.

**Inline directives** for JIRA constructs without markdown equivalents:

```markdown
Status: :status[In Progress]{color=blue}
Assigned to: :mention[Alice]{id=abc123}
Due: :date[2026-04-15]
Emoji: :smile:
```

**Container directives** for panels and other blocks:

```markdown
:::panel{type=info}
This is an info panel with **rich** content inside.
:::

:::expand{title="Click to expand"}
Hidden content here.
:::
```

**Leaf block directives** for smart links and cards:

```markdown
::card[https://example.com/page]
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
