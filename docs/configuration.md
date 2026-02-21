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

All configuration files support local overrides. If a file exists in
`.omni-dev/local/`, it takes precedence over the shared project
configuration in `.omni-dev/`.

**Important**: Add `.omni-dev/local/` to your `.gitignore` to keep personal
configurations private.

### Config File Resolution

For each configuration file (e.g., `scopes.yaml`, `commit-guidelines.md`),
omni-dev checks the following locations in order and uses the first match:

| Priority | Location                                     | Purpose                         |
|----------|----------------------------------------------|---------------------------------|
| 1        | `{dir}/local/{filename}`                     | Personal overrides (gitignored) |
| 2        | `{dir}/{filename}`                           | Shared project configuration    |
| 3        | `$XDG_CONFIG_HOME/omni-dev/{filename}`       | XDG global config               |
| 4        | `$HOME/.omni-dev/{filename}`                 | Legacy global fallback          |

Where `{dir}` is the active config directory, resolved as follows:

### Config Directory Resolution

The config directory (`.omni-dev/`) is itself resolved through a priority
chain:

| Priority | Source                              | Description                                  |
|----------|-------------------------------------|----------------------------------------------|
| 1        | `--context-dir` CLI flag            | Explicit override; disables walk-up           |
| 2        | `OMNI_DEV_CONFIG_DIR` env var       | Environment override; disables walk-up        |
| 3        | Walk-up discovery                   | Nearest `.omni-dev/` from CWD to repo root   |
| 4        | `.omni-dev` (relative to CWD)       | Default fallback                              |

**Walk-up discovery** searches from the current working directory upward
through parent directories, stopping at the repository root (`.git`
boundary). The first directory containing a `.omni-dev/` subdirectory wins.
This is especially useful in monorepos where subdirectories need different
configuration.

**XDG compliance**: When `$XDG_CONFIG_HOME` is set, omni-dev checks
`$XDG_CONFIG_HOME/omni-dev/` for global config files. When unset, it
defaults to `$HOME/.config/omni-dev/`. The legacy `$HOME/.omni-dev/` path
is still supported as a final fallback.

See [Configuration Best Practices](configuration-best-practices.md) for
guidance on writing effective configuration files.

### 1. Scope Definitions (`.omni-dev/scopes.yaml`)

**Purpose**: Define project-specific scopes and their meanings for use in conventional commit messages.

#### What are Scopes?

Scopes are used in conventional commit messages to indicate which part of the codebase a change affects. They appear in the format: `type(scope): description`

For example:
- `feat(auth): add OAuth2 login`
- `fix(api): resolve rate limiting bug`
- `docs(readme): update installation steps`

#### Creating Your First Scope

**Step 1: Create the configuration directory**
```bash
mkdir -p .omni-dev
```

**Step 2: Create the scopes.yaml file**
```bash
touch .omni-dev/scopes.yaml
```

**Step 3: Define your scopes**
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

#### Scope Definition Fields

- **name** (required): The identifier used in commit messages
- **description** (required): Clear explanation of what this scope covers
- **examples** (required): 2-3 example commit messages using this scope
- **file_patterns** (required): Glob patterns to match files belonging to this scope

#### How Scopes Are Used

1. **During commit analysis**: omni-dev examines changed files and suggests appropriate scopes based on file_patterns
2. **In commit messages**: Scopes appear as `type(scope): description`
3. **For organization**: Scopes help categorize changes and make commit history more searchable

#### Scope Selection Logic

When multiple scopes match changed files:
- omni-dev prioritizes scopes with more specific file patterns
- If patterns have equal specificity, all matching scopes are suggested
- You can override automatic detection by specifying the scope manually

#### Handling Overlapping Patterns

If file patterns overlap between scopes:
```yaml
scopes:
  - name: "api"
    file_patterns: ["src/api/**"]

  - name: "auth"
    file_patterns: ["src/api/auth/**"]  # More specific
```

In this case, changes to `src/api/auth/login.js` would suggest the `auth` scope due to its more specific pattern.

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
# Use CLI flag
omni-dev git commit message twiddle 'HEAD~5..HEAD' \
  --context-dir ./config

# Or use environment variable
export OMNI_DEV_CONFIG_DIR=./config
omni-dev git commit message twiddle 'HEAD~5..HEAD'
```

Both `--context-dir` and `OMNI_DEV_CONFIG_DIR` disable walk-up discovery,
giving you full control over which config directory is used.

### Multiple Configuration Sets (Monorepos)

For monorepos, walk-up discovery automatically selects the right config.
Place `.omni-dev/` directories at each package level:

```
repo/
├── .git/
├── .omni-dev/                    # Root config (fallback)
│   └── scopes.yaml
├── packages/
│   ├── frontend/
│   │   ├── .omni-dev/            # Frontend-specific config
│   │   │   └── scopes.yaml
│   │   └── src/
│   └── backend/
│       ├── .omni-dev/            # Backend-specific config
│       │   └── scopes.yaml
│       └── src/
```

Running from `repo/packages/frontend/src/` automatically uses the frontend
config. Running from `repo/` uses the root config. No `--context-dir`
needed.

If you prefer explicit control, you can still use `--context-dir`:

```bash
# Explicit override
omni-dev git commit message twiddle 'HEAD~5..HEAD' \
  --context-dir ./packages/frontend/.omni-dev
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

## Real-World Usage Examples

### How Scopes Appear in Commits

Here's how scopes are used in actual commit messages:

**Basic Usage**:
```bash
# Format: type(scope): description
git commit -m "feat(auth): add two-factor authentication"
git commit -m "fix(api): resolve timeout on large payloads"
git commit -m "docs(readme): update API examples"
```

**With omni-dev**:
```bash
# omni-dev analyzes your changes and suggests the appropriate scope
$ omni-dev git commit message twiddle HEAD --use-context

# Output might suggest:
# Based on changes to src/auth/login.js and src/auth/2fa.js:
# Suggested scope: auth
# Suggested message: feat(auth): implement two-factor authentication flow
```

### Scope Usage in Different Scenarios

**1. Single File Change**:
```bash
# Changed: src/api/users.js
# omni-dev suggests: fix(api): validate email format in user creation
```

**2. Multiple Files, Same Scope**:
```bash
# Changed: src/ui/Button.jsx, src/ui/Modal.jsx, src/ui/theme.css
# omni-dev suggests: refactor(ui): update component styling to new design system
```

**3. Multiple Files, Different Scopes**:
```bash
# Changed: src/api/auth.js, docs/API.md
# omni-dev suggests multiple options:
# - feat(api): add OAuth provider with documentation
# - feat(api,docs): implement OAuth and update API docs
# You choose the most appropriate one
```

**4. No Matching Scope**:
```bash
# Changed: new-feature/experimental.js (no pattern matches)
# omni-dev suggests: feat: add experimental feature
# (No scope when patterns don't match)
```

### Working with Scope Overrides

Sometimes you need to override the suggested scope:

```bash
# File changed: src/utils/logger.js
# Pattern matches: "shared" scope
# But this change is auth-specific

# Override with your preferred scope:
git commit -m "fix(auth): improve auth error logging detail"
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

**Recommended**: Use the [omni-dev-commit-check](https://github.com/action-works/omni-dev-commit-check) GitHub Action for PR commit validation with built-in PR integration.

**Manual setup** (if you need more control):

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
          omni-dev git commit message check 'origin/main..HEAD' || {
            echo "Commit validation failed"
            echo "Run: omni-dev git commit message twiddle 'origin/main..HEAD' --use-context"
            exit 1
          }
```

See [Configuration Best Practices](configuration-best-practices.md#enforcing-check-in-ci) for exit code semantics and how twiddle-generated commits interact with check.

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
   omni-dev git commit message twiddle 'HEAD~20..HEAD' --concurrency 3
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
