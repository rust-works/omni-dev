# Configuration Guide

Complete guide to configuring omni-dev's contextual intelligence for your
project.

## Overview

omni-dev's contextual intelligence system learns about your project to
provide better commit message suggestions. Configuration happens through the
`.omni-dev/` directory in your repository root.

## Quick Setup

```bash
# 1. Create configuration directory
mkdir .omni-dev

# 2. Set up Claude API key
export CLAUDE_API_KEY="your-api-key-here"

# 3. Create basic configuration files
# (See detailed examples below)
```

## Configuration Files

### Local Override Support

**NEW**: All configuration files now support local overrides! If a file exists in `.omni-dev/local/`, it will take precedence over the shared project configuration in `.omni-dev/`.

**Priority Order**:

1. `.omni-dev/local/{filename}` - **Local override (highest priority)**
2. `.omni-dev/{filename}` - Shared project configuration

This allows developers to customize their personal workflow without affecting team settings.

**Important**: Add `.omni-dev/local/` to your `.gitignore` to keep personal configurations private.

### 1. Scope Definitions (`.omni-dev/scopes.yaml`)

**Purpose**: Define project-specific scopes and their meanings.

**Format**:

```yaml
scopes:
  - name: "scope-name"
    description: "What this scope covers"
    examples:
      - "scope: example message 1"
      - "scope: example message 2"
    file_patterns:
      - "path/pattern/**"
      - "*.extension"
```

#### Example Configurations

**Web Application**:

```yaml
scopes:
  - name: "auth"
    description: "Authentication and authorization"
    examples:
      - "auth: add OAuth2 Google integration"
      - "auth: fix JWT token validation"
    file_patterns:
      - "src/auth/**"
      - "middleware/auth.js"
      - "auth.rs"

  - name: "api"
    description: "Backend API endpoints"
    examples:
      - "api: add user management endpoints"
      - "api: improve error handling"
    file_patterns:
      - "src/api/**"
      - "routes/**"
      - "controllers/**"

  - name: "ui"
    description: "Frontend user interface"
    examples:
      - "ui: add responsive navigation"
      - "ui: fix mobile layout issues"
    file_patterns:
      - "src/components/**"
      - "pages/**"
      - "*.vue"
      - "*.tsx"
      - "*.jsx"

  - name: "db"
    description: "Database schema and migrations"
    examples:
      - "db: add user profiles table"
      - "db: optimize query performance"
    file_patterns:
      - "migrations/**"
      - "schema/**"
      - "*.sql"

  - name: "deploy"
    description: "Deployment and infrastructure"
    examples:
      - "deploy: add Docker configuration"
      - "deploy: update CI/CD pipeline"
    file_patterns:
      - "Dockerfile"
      - ".github/workflows/**"
      - "docker-compose.yml"
      - "terraform/**"
```

**Rust Project**:

```yaml
scopes:
  - name: "core"
    description: "Core library functionality"
    examples:
      - "core: add async processing support"
      - "core: improve error handling"
    file_patterns:
      - "src/lib.rs"
      - "src/core/**"

  - name: "cli"
    description: "Command-line interface"
    examples:
      - "cli: add new subcommand"
      - "cli: improve help output"
    file_patterns:
      - "src/cli/**"
      - "src/main.rs"

  - name: "api"
    description: "Public API surface"
    examples:
      - "api: add builder pattern"
      - "api: deprecate old methods"
    file_patterns:
      - "src/api.rs"
      - "src/**/public.rs"

  - name: "tests"
    description: "Test utilities and fixtures"
    examples:
      - "tests: add integration tests"
      - "tests: improve test coverage"
    file_patterns:
      - "tests/**"
      - "src/**/tests.rs"
      - "benches/**"
```

**Microservices**:

```yaml
scopes:
  - name: "user-service"
    description: "User management service"
    examples:
      - "user-service: add profile endpoints"
      - "user-service: fix authentication bug"
    file_patterns:
      - "services/user/**"
      - "user-service/**"

  - name: "order-service"
    description: "Order processing service"
    examples:
      - "order-service: implement payment flow"
      - "order-service: add order validation"
    file_patterns:
      - "services/order/**"
      - "order-service/**"

  - name: "shared"
    description: "Shared libraries and utilities"
    examples:
      - "shared: add logging utilities"
      - "shared: update common types"
    file_patterns:
      - "shared/**"
      - "common/**"
      - "lib/**"
```

### 2. Commit Guidelines (`.omni-dev/commit-guidelines.md`)

**Purpose**: Document your project's commit message conventions.

**Template**:

```markdown
# Project Commit Guidelines

## Format
[Your conventional commit format]

## Types
[List of commit types your project uses]

## Scopes  
[Description of your scopes]

## Style Rules
[Your specific style preferences]

## Examples
[Good examples from your project]
```

#### Example Configurations

**Standard Project**:

```markdown
# Commit Guidelines

## Format
Use conventional commits: `type(scope): description`

Optional body and footer:
```

type(scope): short description

Longer description explaining what and why.

- Bullet points for complex changes
- Breaking changes noted in footer

Fixes #123

```

## Types We Use
- `feat` - New features and enhancements
- `fix` - Bug fixes and patches
- `docs` - Documentation changes only
- `refactor` - Code restructuring without behavior change
- `test` - Adding or updating tests
- `chore` - Build system, dependencies, tooling
- `style` - Code formatting, whitespace, linting
- `perf` - Performance improvements

## Scopes
See `.omni-dev/scopes.yaml` for complete list.

Common scopes:
- `auth` - Authentication systems
- `api` - Backend API changes  
- `ui` - Frontend interface
- `db` - Database related changes

## Style Rules
1. Keep subject line under 50 characters
2. Use imperative mood: "Add feature" not "Added feature"
3. Capitalize first letter of description
4. No period at end of subject line
5. Use body to explain what and why, not how

## Breaking Changes
Mark breaking changes with `BREAKING CHANGE:` in footer:

```

feat(api): add new user authentication

BREAKING CHANGE: Authentication now requires API key in header

```

## Examples

### Good Examples
```

feat(auth): add OAuth2 Google integration
fix(ui): resolve mobile navigation collapse issue
docs(readme): update installation instructions
refactor(core): extract common validation logic
test(auth): add integration tests for login flow
chore(deps): update React to v18.2.0

```

### Examples to Avoid
```

❌ Fix stuff
❌ Update files  
❌ WIP
❌ Fixed the bug in authentication
❌ Adding new feature

```
```

**Enterprise Project**:

```markdown
# Commit Message Standards

## Required Format
`[JIRA-ID] type(scope): description`

Example: `[PROJ-123] feat(auth): add SSO integration`

## Approval Process
All commits must:
1. Reference a Jira ticket
2. Follow conventional commit format  
3. Include scope from approved list
4. Pass automated commit message validation

## Types (Mandatory)
- `feat` - New feature (minor version bump)
- `fix` - Bug fix (patch version bump)  
- `chore` - Maintenance (no version bump)
- `docs` - Documentation only
- `refactor` - Code restructuring
- `test` - Test additions/updates
- `breaking` - Breaking change (major version bump)

## Scopes (Required)
Must use one of the approved scopes from scopes.yaml.
Contact architecture team to add new scopes.

## Review Requirements
- Breaking changes require architecture review
- Database changes require DBA review
- Security-related changes require security review

## Validation
Commits are validated by:
1. Pre-commit hooks
2. CI/CD pipeline
3. PR merge checks

## Examples
```

[PROJ-123] feat(auth): integrate with corporate SSO
[PROJ-124] fix(api): resolve rate limiting edge case
[PROJ-125] chore(deps): update security dependencies

```
```

## Environment Setup

### Claude API Key

**Required**: omni-dev needs a Claude API key for AI features.

#### Get Your API Key

1. Visit [Anthropic Console](https://console.anthropic.com/)
2. Sign up/login to your account
3. Navigate to API Keys section
4. Generate a new API key

#### Configure the Key

**Option 1: Environment Variable (Recommended)**

```bash
export CLAUDE_API_KEY="sk-ant-api03-..."

# Make it permanent (choose your shell)
echo 'export CLAUDE_API_KEY="sk-ant-api03-..."' >> ~/.bashrc  # bash
echo 'export CLAUDE_API_KEY="sk-ant-api03-..."' >> ~/.zshrc   # zsh
```

**Option 2: Project .env File**

```bash
# Create .env file (DO NOT commit to git)
echo "CLAUDE_API_KEY=sk-ant-api03-..." >> .env

# Add to .gitignore
echo ".env" >> .gitignore
```

**Option 3: CI/CD Secrets**
For automated workflows, store the key as a secret:

- GitHub Actions: Repository Settings → Secrets → `CLAUDE_API_KEY`
- GitLab CI: Settings → CI/CD → Variables → `CLAUDE_API_KEY`

### Directory Structure

Recommended `.omni-dev/` structure:

```
.omni-dev/
├── scopes.yaml              # Required: Project scopes
├── commit-guidelines.md     # Required: Commit standards
├── local/                   # Optional: Local overrides (add to .gitignore)
│   ├── scopes.yaml          # Personal scope definitions
│   ├── commit-guidelines.md # Personal commit guidelines
│   └── context/             # Personal feature contexts
│       └── feature-contexts/
└── examples/               # Optional: Usage examples
    ├── good-commits.md
    └── before-after.md
```

### Local Override Examples

#### Personal Scope Additions

**Team config** (`.omni-dev/scopes.yaml`):

```yaml
scopes:
  - name: "api"
    description: "Backend API changes"
    file_patterns: ["src/api/**"]
  - name: "ui"
    description: "Frontend changes"
    file_patterns: ["src/ui/**"]
```

**Your personal config** (`.omni-dev/local/scopes.yaml`):

```yaml
scopes:
  - name: "api"
    description: "Backend API changes"
    file_patterns: ["src/api/**"]
  - name: "ui"
    description: "Frontend changes"
    file_patterns: ["src/ui/**"]
  # Personal addition
  - name: "experimental"
    description: "[LOCAL] My experimental features"
    examples:
      - "experimental: try new auth approach"
    file_patterns: ["experiments/**", "sandbox/**"]
```

### Setting Up Local Overrides

1. **Create local directory**:

   ```bash
   mkdir -p .omni-dev/local
   ```

2. **Add to .gitignore**:

   ```bash
   echo ".omni-dev/local/" >> .gitignore
   ```

3. **Copy and customize**:

   ```bash
   # Start with team config
   cp .omni-dev/scopes.yaml .omni-dev/local/scopes.yaml
   
   # Customize for your workflow
   vim .omni-dev/local/scopes.yaml
   ```

## Advanced Configuration

### Custom Context Directory

Use a different directory for configuration:

```bash
# Use custom directory
omni-dev git commit message twiddle 'HEAD~5..HEAD' \
  --context-dir ./config

# Configuration files would be:
# ./config/scopes.yaml
# ./config/commit-guidelines.md
```

### Multiple Configuration Sets

For monorepos with different standards per service:

```
configs/
├── frontend/
│   ├── scopes.yaml
│   └── commit-guidelines.md
├── backend/  
│   ├── scopes.yaml
│   └── commit-guidelines.md
└── shared/
    ├── scopes.yaml
    └── commit-guidelines.md
```

Usage:

```bash
# Frontend commits
omni-dev git commit message twiddle 'HEAD~5..HEAD' \
  --context-dir ./configs/frontend

# Backend commits
omni-dev git commit message twiddle 'HEAD~5..HEAD' \
  --context-dir ./configs/backend
```

### File Pattern Matching

Scope file patterns support glob patterns:

```yaml
file_patterns:
  - "src/**/*.js"           # All JS files in src/
  - "components/**"         # Everything in components/
  - "*.md"                  # All markdown files
  - "test/**/*.spec.js"     # Test files
  - "!node_modules/**"      # Exclude node_modules
```

## Validation and Testing

### Validate Configuration

Check if your configuration is working:

```bash
# Test with a small range first
omni-dev git commit message twiddle 'HEAD^..HEAD' --use-context

# Check what context is being detected
omni-dev git commit message view 'HEAD^..HEAD'
```

### Common Issues

**Scope Not Detected**:

- Check file_patterns in scopes.yaml
- Ensure patterns match your file structure
- Use forward slashes even on Windows

**API Key Issues**:

```bash
# Test API key is working
echo $CLAUDE_API_KEY  # Should show your key

# Check for extra spaces/characters
export CLAUDE_API_KEY="$(echo $CLAUDE_API_KEY | tr -d '[:space:]')"
```

**Context Not Loading**:

- Ensure `.omni-dev/` directory exists
- Check file permissions (must be readable)
- Validate YAML syntax in scopes.yaml

## Best Practices

### 1. Start Simple

Begin with basic configuration and expand:

```yaml
# Start with just 3-4 main scopes
scopes:
  - name: "api"
    description: "Backend changes"
    file_patterns: ["src/api/**", "api/**"]
    
  - name: "ui"  
    description: "Frontend changes"
    file_patterns: ["src/ui/**", "components/**"]
    
  - name: "docs"
    description: "Documentation"
    file_patterns: ["*.md", "docs/**"]
```

### 2. Use Meaningful Scope Names

```yaml
# ✅ Good - clear and specific
- name: "auth"
- name: "payment"
- name: "user-profile"

# ❌ Avoid - too generic or unclear  
- name: "stuff"
- name: "misc" 
- name: "changes"
```

### 3. Include File Patterns

Always define file patterns for accurate scope detection:

```yaml
# ✅ Good - specific patterns
file_patterns:
  - "src/auth/**"
  - "middleware/auth.js"
  - "auth.rs"

# ❌ Missing - omni-dev can't auto-detect scope
file_patterns: []
```

### 4. Document Your Conventions

Keep guidelines up-to-date and accessible:

```bash
# Link from main README
echo "See [.omni-dev/commit-guidelines.md](.omni-dev/commit-guidelines.md) for commit standards" >> README.md

# Include in PR template
echo "- [ ] Commits follow [project guidelines](.omni-dev/commit-guidelines.md)" >> .github/pull_request_template.md
```

### 5. Version Your Configuration

Track configuration changes:

```bash
# Include .omni-dev/ in git
git add .omni-dev/
git commit -m "feat(config): add omni-dev contextual intelligence setup"

# Document major changes
echo "## v2.0.0 - Updated scopes and guidelines" >> .omni-dev/CHANGELOG.md
```

## Team Setup

### Onboarding Checklist

For new team members:

```bash
# 1. Install omni-dev
cargo install omni-dev

# 2. Set up API key  
export CLAUDE_API_KEY="team-shared-key-or-individual-key"

# 3. Test configuration
omni-dev git commit message view HEAD --use-context

# 4. Review project guidelines
cat .omni-dev/commit-guidelines.md
```

### Shared Configuration

**Option 1: Shared API Key**

- Use organization/team API key
- Store in team password manager
- Include in onboarding documentation

**Option 2: Individual API Keys**

- Each developer gets own key
- Better usage tracking and limits
- Include setup in CONTRIBUTING.md

### CI/CD Integration

Add commit message validation:

```yaml
# .github/workflows/commits.yml
name: Commit Validation
on: [pull_request]

jobs:
  validate-commits:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
        with:
          fetch-depth: 0
      
      - name: Install omni-dev
        run: cargo install omni-dev
        
      - name: Validate commit messages
        env:
          CLAUDE_API_KEY: ${{ secrets.CLAUDE_API_KEY }}
        run: |
          omni-dev git commit message view 'origin/main..HEAD' || {
            echo "❌ Commit validation failed"
            echo "💡 Run: omni-dev git commit message twiddle 'origin/main..HEAD' --use-context"
            exit 1
          }
```

## Migration Guide

### From Manual Process

If you're currently writing commit messages manually:

1. **Analyze Current Pattern**:

   ```bash
   # See what patterns exist  
   git log --oneline -20 | cut -d' ' -f2- | sort | uniq -c | sort -nr
   ```

2. **Create Initial Configuration**:
   - Extract common scopes from existing messages
   - Document current conventions
   - Set up basic scopes.yaml

3. **Gradual Adoption**:

   ```bash
   # Start with new commits only
   omni-dev git commit message twiddle 'HEAD~5..HEAD' --use-context
   
   # Gradually clean up older commits
   omni-dev git commit message twiddle 'HEAD~20..HEAD' --batch-size 3
   ```

### From Other Tools

**From Commitizen**:

- Map your existing scopes to omni-dev format
- Import scope descriptions and examples
- Update team documentation

**From Custom Scripts**:

- Extract configuration from existing tools
- Migrate file pattern matching rules
- Test with small batches first

## Troubleshooting

### Configuration Issues

**Scopes Not Working**:

1. Check YAML syntax: `cat .omni-dev/scopes.yaml | python -m yaml`
2. Verify file patterns match your structure
3. Test with debug output: `RUST_LOG=omni_dev=debug omni-dev git commit message view HEAD --use-context`

**Guidelines Not Loading**:

1. Ensure `.omni-dev/commit-guidelines.md` exists
2. Check file permissions
3. Verify markdown formatting

**API Key Problems**:

```bash
# Debug API key issues
echo "Key starts with: $(echo $CLAUDE_API_KEY | head -c 10)..."
echo "Key length: $(echo $CLAUDE_API_KEY | wc -c)"

# Test API access with debug output
RUST_LOG=omni_dev=debug omni-dev git commit message view HEAD --use-context
```

## Example Setups

Complete example configurations for different project types:

- [React/TypeScript Frontend](examples/frontend-config.md)
- [Rust CLI Application](examples/rust-config.md)  
- [Node.js API Server](examples/backend-config.md)
- [Python Data Science](examples/datascience-config.md)
- [Enterprise Monorepo](examples/enterprise-config.md)

## Need Help?

- 📖 [User Guide](user-guide.md) - Complete usage guide
- 🔧 [Troubleshooting](troubleshooting.md) - Common issues
- 📝 [Examples](examples.md) - Real-world examples  
- 💬 [GitHub Discussions](https://github.com/rust-works/omni-dev/discussions) - Community support
