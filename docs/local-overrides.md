# Local Override Configuration

Local overrides allow developers to customize their personal omni-dev workflow without affecting shared project settings.

## Overview

All `.omni-dev` configuration files now support local overrides through the `.omni-dev/local/` directory. Files in the local directory take precedence over shared project configurations.

## Priority Order

1. `.omni-dev/local/{filename}` - **Local override (highest priority)**
2. `.omni-dev/{filename}` - Shared project configuration

## Supported Override Files

- `commit-guidelines.md` - Personal commit guidelines
- `commit-template.txt` - Personal commit template  
- `scopes.yaml` - Personal scope definitions
- `context/feature-contexts/*.yaml` - Personal feature contexts

## Quick Setup

```bash
# 1. Create local override directory
mkdir -p .omni-dev/local

# 2. Add to .gitignore to keep personal settings private
echo ".omni-dev/local/" >> .gitignore

# 3. Copy team config as starting point
cp .omni-dev/scopes.yaml .omni-dev/local/scopes.yaml

# 4. Customize for your workflow
vim .omni-dev/local/scopes.yaml
```

## Examples

### Personal Scope Additions

Add personal scopes while keeping team standards:

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
  # Personal additions
  - name: "experimental"
    description: "[LOCAL] My experimental features"
    examples:
      - "experimental: try new auth approach"
      - "experimental: test performance optimization"
    file_patterns: ["experiments/**", "sandbox/**"]
  - name: "research"
    description: "[LOCAL] Research and prototyping"
    examples:
      - "research: investigate new algorithms"
    file_patterns: ["research/**", "prototypes/**"]
```

### Personal Commit Template

Customize your commit message structure:

**Your personal template** (`.omni-dev/local/commit-template.txt`):

```
# [type](scope): [description]

# What changed:
# - 

# Why it changed:
# - 

# Testing performed:
# - 

# Breaking changes (if any):
# 

# Fixes #(issue_number)
# Signed-off-by: Your Name <your@email.com>
```

### Personal Commit Guidelines

Override team guidelines with your preferred style:

**Your personal guidelines** (`.omni-dev/local/commit-guidelines.md`):

```markdown
# Personal Commit Guidelines

## My Preferred Format
Use detailed commit messages with context:

```

type(scope): brief description

Detailed explanation of what changed and why:

- Key change 1
- Key change 2

Testing:

- Unit tests added/updated
- Manual testing performed

Fixes #123

```

## Personal Rules
- Always include testing information
- Add ticket references
- Use signed-off-by for compliance
- Include breaking change notes when applicable
```

## Use Cases

### Individual Preferences

- Different commit message detail levels
- Additional personal scopes for experiments
- Custom templates with required fields
- Personal workflow optimizations

### Project Variations

- Different standards for different types of work
- Experimental features not in team config
- Client-specific requirements
- Compliance additions (signatures, tickets)

### Development Environments

- Different settings for different projects
- Environment-specific scopes (staging, dev, prod)
- Personal debugging and testing workflows

## Best Practices

### 1. Start with Team Config

Always begin by copying the shared configuration:

```bash
cp .omni-dev/scopes.yaml .omni-dev/local/scopes.yaml
```

### 2. Document Personal Changes

Mark personal additions clearly:

```yaml
- name: "experimental"
  description: "[LOCAL] My experimental features"  # Mark as local
```

### 3. Keep `.omni-dev/local/` Private

**Always** add to `.gitignore`:

```
.omni-dev/local/
```

### 4. Share Useful Patterns

If your local config proves valuable, propose it for team adoption.

### 5. Maintain Compatibility

Ensure your local config doesn't break team workflows or CI/CD.

### 6. Regular Updates

Periodically sync with team config updates:

```bash
# Review team changes
diff .omni-dev/scopes.yaml .omni-dev/local/scopes.yaml

# Update local config as needed
```

## Troubleshooting

### Local Config Not Loading

- Check file permissions (must be readable)
- Verify YAML syntax: `python -c "import yaml; yaml.safe_load(open('.omni-dev/local/scopes.yaml'))"`
- Ensure `.omni-dev/local/` directory exists

### Conflicts with Team Config

- Use `[LOCAL]` prefix in descriptions to identify personal additions
- Test with team members to ensure compatibility
- Keep personal scopes separate from team scopes when possible

### Version Control Issues

- Ensure `.omni-dev/local/` is in `.gitignore`
- Never commit personal configurations to shared repository
- Use separate branches if testing team config changes

## Advanced Usage

### Multiple Local Configs

For complex workflows, organize by context:

```
.omni-dev/local/
├── scopes-client-a.yaml
├── scopes-client-b.yaml
└── switch-config.sh
```

### Dynamic Configuration

Use scripts to switch between different local configs based on project context.

### Feature Context Overrides

Create personal feature contexts:

```
.omni-dev/local/context/feature-contexts/
├── my-auth-feature.yaml
├── experimental-ui.yaml
└── performance-testing.yaml
```

## Integration

Local overrides work seamlessly with all omni-dev features:

- Contextual intelligence uses your personal scopes
- Commit message generation respects your templates
- All CLI commands honor local configuration
- Batching and processing use personal settings

For more information, see:

- [Configuration Guide](configuration.md) - Complete setup instructions
- [User Guide](user-guide.md) - Usage examples and workflows
- [Examples](examples.md) - Real-world configuration examples
