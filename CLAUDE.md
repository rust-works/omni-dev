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
- `src/cli/atlassian/` - Atlassian JIRA/Confluence CLI commands
- `src/atlassian/` - Atlassian API client, ADF/JFM conversion, document format
- `src/data/` - Data structures and YAML output formatting
- `src/core/` - Core application logic
- `src/utils/` - Utility functions

### Configuration
- `Cargo.toml` - Rust package configuration and dependencies
- `.github/` - GitHub Actions CI/CD workflows
- `.claude/skills/` - Claude skill definitions

### Documentation
- `README.md` - Main project documentation
- `CHANGELOG.md` - Version history and changes
- `CONTRIBUTING.md` - Contribution guidelines
- `docs/STYLE_GUIDE.md` - Project conventions for code, documentation, and other artifacts
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
2. **Follow the Style Guide**: Before writing or reviewing code, documentation, or other project artifacts, consult [docs/STYLE_GUIDE.md](docs/STYLE_GUIDE.md). Use the task-to-tag lookup table at the top of the guide to identify relevant tags, then search for those tags (e.g., `grep "Tags:.*code-style" docs/STYLE_GUIDE.md`). Read and follow the matched rules. Do not skip this step.
3. **Configuration Changes**: When modifying config loading or scope resolution, consult [docs/configuration-best-practices.md](docs/configuration-best-practices.md) and [docs/plan/config-internals.md](docs/plan/config-internals.md)
4. **Test Changes**: Run tests after modifications
5. **CLI Surface Changes**: After any change to `src/cli/**`, `src/main.rs`, or any `#[derive(Parser)]` / `#[derive(Subcommand)]` / `#[arg(...)]` site, invoke the [`update-snapshots`](.claude/skills/update-snapshots/SKILL.md) skill to review and update `insta` golden snapshots — most often [tests/snapshots/integration_test__help_all_output.snap](tests/snapshots/integration_test__help_all_output.snap). Do **not** assume `cargo test` passing in isolation surfaces drift before you've inspected the new snapshot: golden tests fail loudly, but only after the full suite has run, and the fix (`cargo insta accept`) must only be applied when the diff matches the *intended* CLI change. If the diff contains anything you did not intend, investigate the regression instead of accepting.
6. **Conventional Commits**: Use proper commit message format (see `.omni-dev/commit-guidelines.md`)
7. **Incremental Changes**: Make focused, reviewable changes

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

### AI Backend Dispatch
Backends are selected inside `src/claude/client.rs::create_default_claude_client` in this order:

1. `OMNI_DEV_AI_BACKEND=claude-cli` (or `--ai-backend claude-cli`) → `ClaudeCliAiClient` in `src/claude/ai/claude_cli.rs`.
2. `USE_OLLAMA=true` → `OpenAiAiClient::new_ollama` in `src/claude/ai/openai.rs`.
3. `USE_OPENAI=true` → `OpenAiAiClient::new_openai` in `src/claude/ai/openai.rs`.
4. `CLAUDE_CODE_USE_BEDROCK=true` → `BedrockAiClient` in `src/claude/ai/bedrock.rs`.
5. Default → `ClaudeAiClient` in `src/claude/ai/claude.rs` (direct Anthropic API).

Preflight (`src/utils/preflight.rs`) mirrors this switch and must change in lock-step when adding backends.

User-facing details — required env vars, model selection, Claude CLI sandbox semantics, the `--claude-cli-allow-tools` / `--claude-cli-allow-mcp` escape hatches, the `--claude-cli-max-budget-usd` spending cap, and per-backend troubleshooting — live in [docs/ai-backends.md](docs/ai-backends.md). Keep it in sync when changing any of those surfaces.

Architectural rationale for the sandboxed `claude-cli` subprocess backend — threat model, sandbox flag choices, escape-hatch design, budget-cap enforcement — lives in [ADR-0028](docs/adrs/adr-0028.md).

Dev-only notes:
- `ClaudeCliAiClient::run` is the warn site for both escape hatches, the INFO-level `total_cost_usd` log, and the post-response WARN when reported cost exceeds the configured cap.
- `--beta-header` is ignored for the `claude-cli` backend (`claude`'s `--betas` flag has different semantics).

### Browser Bridge
The `omni-dev browser bridge` command tree drives HTTP requests **through an authenticated browser tab** (Grafana/Loki, SSO-gated dashboards) without exfiltrating the browser's cookies/tokens — a *confused deputy by design*. It is a two-plane local server joined by an `id`-keyed correlator:

- `src/cli/browser.rs` + `src/cli/browser/` — the CLI surface: `bridge serve` (`bridge.rs`, the long-lived server), `bridge request` (`request.rs`, the thin client), and `bridge harvest <platform> <object>` (`harvest.rs`, best-effort scrapers). Both clients send a `ControlRequest` to `POST /__bridge/request` via the shared `src/browser/client.rs::BridgeClient` rather than opening their own socket.
- `src/browser/harvest/` — the harvest engines (`facebook.rs` = own-timeline pagination). These drive **reverse-engineered, undocumented** site internals: best-effort, re-harvest every volatile `doc_id`/token/provider flag per run (never hardcoded), fail with staged actionable errors on drift, and only ever use the connected tab's own session. The Facebook recipe is documented in [docs/browser-bridge.md](docs/browser-bridge.md) and issue #922.
- `src/browser/bridge.rs` — server core: the HTTP control plane (axum, default `127.0.0.1:9998`), the WebSocket plane the browser connects to (default `127.0.0.1:9999`), the `Correlator` (per-`id` channel), the transparent proxy, and `dispatch`/`start_stream`.
- `src/browser/protocol.rs` — the wire types (`ControlRequest`, `Command`, `BrowserReply`/`ResponseEnvelope`, the streaming `StreamItem`/`StreamLine`/`CancelCommand`, `StatusResponse`/`TabInfo`). New optional fields use `#[serde(default, skip_serializing_if = ...)]` to keep older clients byte-identical on the wire.
- `src/browser/auth.rs` — the **load-bearing** security primitives: token generation/resolution, `constant_time_eq`, the `X-Omni-Bridge` / `X-Omni-Bridge-Target` header constants, Host/Origin/Sec-Fetch-Site guards, and `validate_outbound_url` (server-side outbound scope; the in-page snippet is never trusted).
- `src/templates/browser-bridge.js` — the snippet pasted into the DevTools console (rendered by `src/browser/snippet.rs`); it reads `cmd.stream` / `cmd.credentials` and base64-encodes non-text bodies.

The security model is **core, not an add-on**: both planes are authenticated and default-closed. When touching the trust boundary (auth guards, outbound scope, token handling, the planes), keep [ADR-0036](docs/adrs/adr-0036.md) and the operator guide [docs/browser-bridge.md](docs/browser-bridge.md) in sync. Changes to the CLI surface require the [`update-snapshots`](.claude/skills/update-snapshots/SKILL.md) skill (see Code Changes §5).

### Skill Structure
Claude skills are organized in `.claude/skills/`, one subdirectory per skill with a `SKILL.md` file.

### Working with Git
Common git operations in this project:
- `git log --format=%H` - Get commit hashes
- `git show --stat <commit>` - Get diff summaries
- `git branch -r --contains <commit>` - Check remote branch containment
- `git status --porcelain` - Get working directory status

### Git Worktrees
New git worktrees should be created in the `.work/` directory of the current project (e.g., `git worktree add .work/<branch-name> <branch-name>`). The `.work/` directory is gitignored and keeps worktrees scoped to the project rather than scattered across sibling directories.

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