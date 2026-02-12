# Claude AI Assistant Guide

This document provides guidance for AI assistants (particularly Claude) working with the omni-dev project.

## Project Overview

omni-dev is a powerful Git commit message analysis and amendment toolkit written in Rust. It provides:

- Comprehensive commit analysis with YAML output
- Branch-aware commit analysis 
- Safe commit message amendment capabilities
- GitHub integration for PR and remote information
- Conventional commit detection and suggestions

## Key Files and Structure

### Core Source Files
- `src/main.rs` - CLI entry point
- `src/lib.rs` - Library exports
- `src/cli/` - Command-line interface implementation
- `src/data/` - Data structures and YAML output formatting
- `src/core/` - Core application logic
- `src/utils/` - Utility functions

### Configuration
- `Cargo.toml` - Rust package configuration and dependencies
- `.github/` - GitHub Actions CI/CD workflows
- `.claude/commands/` - Claude-specific command definitions

### Documentation
- `README.md` - Main project documentation
- `CHANGELOG.md` - Version history and changes
- `CONTRIBUTING.md` - Contribution guidelines
- `docs/STYLE_GUIDE.md` - Coding conventions (error handling, naming, visibility, imports, etc.)
- `docs/RELEASE.md` - Release process documentation
- `docs/plan/` - Project planning and specifications

## Development Workflow

### Code Quality Standards
- **Build Script**: Run `./scripts/build.sh` for complete validation (recommended)
- **Tests**: Run `cargo test` before commits
- **Linting**: Use `cargo clippy -- -D warnings` for code quality
- **Formatting**: Apply `cargo fmt` for consistent style
- **Documentation**: Maintain doc comments for public APIs

### Commit Message Format
Follow conventional commit format:
```
<type>(<scope>): <description>

<body>

<footer>
```

Common types: `feat`, `fix`, `docs`, `chore`, `refactor`, `test`

### Branch Strategy
- `main` - Production-ready code
- Feature branches - `feature/description` or `username/feature-description`
- Release branches - Tagged as `vX.Y.Z`

## AI Assistant Guidelines

### Code Changes
1. **Read Before Writing**: Always read existing files before making changes
2. **Follow the Style Guide**: Before writing or reviewing code, consult [docs/STYLE_GUIDE.md](docs/STYLE_GUIDE.md). Use the task-to-tag lookup table at the top of the guide to identify relevant tags, then search for those tags (e.g., `grep "Tags:.*code-style" docs/STYLE_GUIDE.md`). Read and follow the matched rules. Do not skip this step.
3. **Test Changes**: Run tests after modifications
4. **Conventional Commits**: Use proper commit message format (see `.omni-dev/commit-guidelines.md`)
5. **Incremental Changes**: Make focused, reviewable changes

### Release Process
When preparing releases, follow the comprehensive guide in [docs/RELEASE.md](docs/RELEASE.md):

1. Update version in `Cargo.toml`
2. Update `CHANGELOG.md` with release notes
3. Run quality checks (`cargo test`, `cargo clippy`)
4. Commit changes with conventional commit format
5. Create annotated git tag
6. Push commits and tag
7. Create GitHub release
8. Publish to crates.io

### Understanding YAML Output
The project generates structured YAML output with field presence tracking:

- **Field Documentation**: Each output field is documented with presence indicators
- **AI Guidance**: Look for `present: true` fields in the explanation section
- **Dynamic Tracking**: The `update_field_presence()` method tracks which fields are available

### AI Response Parsing - CRITICAL UNDERSTANDING
**IMPORTANT**: When working with AI-generated responses in this project, understand the correct data structure:

- **AI responses are VALID YAML** with `title` and `description` fields
- **The `description` field VALUE contains markdown content**, including embedded code blocks
- **Embedded ```yaml blocks are CONTENT, not structure** - they're part of the description string
- **NEVER attempt to "unwrap" or extract content between markdown code fences**
- **Use simple `content.trim()` parsing** - complex extraction logic breaks the YAML structure

**Example of correct AI response structure**:
```yaml
title: "PR title here"
description: |
  # Section
  
  ```yaml
  - some: nested content
  ```
  
  This is all part of the description field value.
```

**Common Mistake**: Treating embedded ```yaml blocks as if they need extraction. They don't - they're just content within the description field.

**Correct Approach**: Parse the entire response as YAML directly. The markdown formatting (including code blocks) is the intended content of the description field.

### AI Model Configuration
The project includes a comprehensive model registry system:

- **Model Registry**: `src/claude/model_config.rs` manages AI model specifications
- **Model Templates**: `src/templates/models.yaml` defines supported Claude models with token limits
- **Fuzzy Matching**: Supports various identifier formats (Bedrock, AWS, regional)
- **Configuration Commands**: Use `omni-dev config models show` to view available models
- **Dynamic Limits**: Token limits are automatically applied based on model specifications

### Command Structure
Claude commands are organized in `.claude/commands/`:
- `commit-twiddle*` - Commit message modification commands
- `pr-create*` - Pull request creation commands
- Variants include debug, release, and standard modes

### Working with Git
Common git operations in this project:
- `git log --format=%H` - Get commit hashes
- `git show --stat <commit>` - Get diff summaries
- `git branch -r --contains <commit>` - Check remote branch containment
- `git status --porcelain` - Get working directory status

## Testing Approach

### Test Types
- **Unit Tests**: In `src/` files using `#[cfg(test)]`
- **Integration Tests**: In `tests/` directory
- **Golden Tests**: Using `insta` crate for snapshot testing

### Test Data
- Temporary git repositories for integration tests
- YAML fixtures for parsing tests
- Golden files for output validation

## Common Patterns

### Error Handling
```rust
use anyhow::{Context, Result};

fn operation() -> Result<()> {
    // Use .context() for error chain building
    some_operation()
        .context("Failed to perform operation")?;
    Ok(())
}
```

### YAML Serialization
```rust
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Data {
    #[serde(skip_serializing_if = "Option::is_none")]
    optional_field: Option<String>,
}
```

### Git Operations
```rust
use git2::Repository;

let repo = Repository::open(".")?;
let head = repo.head()?;
let commit = head.peel_to_commit()?;
```

## Troubleshooting

### Common Issues
- **Clippy Warnings**: Use suggested fixes or add `#[allow(clippy::rule)]` with justification
- **Test Failures**: Check for timing issues with git operations
- **YAML Formatting**: Ensure proper serialization attributes

### Debug Commands
```bash
# Verbose test output
cargo test -- --nocapture

# Specific test
cargo test test_name

# Debug build
cargo build --verbose
```

## References

- [Rust Documentation](https://doc.rust-lang.org/)
- [git2 Crate Documentation](https://docs.rs/git2/)
- [Clap CLI Framework](https://docs.rs/clap/)
- [Serde Serialization](https://serde.rs/)
- [Release Process](docs/RELEASE.md) - Complete release workflow

## Best Practices

1. **Read the Full Context**: Understand the existing codebase before making changes
2. **Follow Rust Idioms**: Use idiomatic Rust patterns and conventions
3. **Maintain Safety**: Leverage Rust's safety features and error handling
4. **Document Changes**: Update documentation when adding features
5. **Test Thoroughly**: Ensure changes don't break existing functionality
6. **Follow Semver**: Use appropriate version bumps for changes