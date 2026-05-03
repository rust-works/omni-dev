# omni-dev

[![Crates.io](https://img.shields.io/crates/v/omni-dev.svg)](https://crates.io/crates/omni-dev)
[![Documentation](https://docs.rs/omni-dev/badge.svg)](https://docs.rs/omni-dev)
[![Build Status](https://github.com/rust-works/omni-dev/workflows/CI/badge.svg)](https://github.com/rust-works/omni-dev/actions)
[![License: BSD-3-Clause](https://img.shields.io/badge/License-BSD%203--Clause-blue.svg)](LICENSE)

An intelligent Git commit message toolkit with AI-powered contextual
intelligence. Transform messy commit histories into professional,
conventional commit formats with project-aware suggestions.

## ✨ Key Features

- 🤖 **AI-Powered Intelligence**: Claude AI analyzes your code changes to
  suggest meaningful commit messages and PR descriptions
- 🧠 **Contextual Awareness**: Understands your project structure,
  conventions, and work patterns
- 🔍 **Comprehensive Analysis**: Deep analysis of commits, branches, and
  file changes
- ✏️ **Smart Amendments**: Safely improve single or multiple commit messages
- 🚀 **PR Creation**: Generate professional pull requests with AI-powered
  descriptions
- 📦 **Automatic Batching**: Handles large commit ranges intelligently
- 🎯 **Conventional Commits**: Automatic detection and formatting
- 🛡️ **Safety First**: Working directory validation and error recovery
- ⚡ **Fast & Reliable**: Built with Rust for memory safety and performance

## 🚀 Quick Start

### Installation

```bash
# Install from crates.io
cargo install omni-dev

# Install with Nix
nix profile install github:rust-works/omni-dev

# Install with Nix flakes (development)
nix run github:rust-works/omni-dev

# Enable binary cache for faster builds (optional)
cachix use omni-dev

# Set up Claude API key (required for AI features)
export CLAUDE_API_KEY="your-api-key-here"
```

#### Nix Binary Cache (Optional)

For faster Nix builds, you can use the binary cache:

```bash
# Install cachix if you don't have it
nix profile install nixpkgs#cachix

# Enable the omni-dev binary cache
cachix use omni-dev

# Now Nix installations will use pre-built binaries instead of compiling from source
nix profile install github:rust-works/omni-dev
```

### 🎬 See It In Action

[![asciicast](https://asciinema.org/a/eJJf5Aj8N26JoCaUsAFVH8dqz.svg)](https://asciinema.org/a/eJJf5Aj8N26JoCaUsAFVH8dqz)

*Watch omni-dev transform messy commits into professional ones with AI-powered analysis*

### 30-Second Demo

Transform your commit messages and create professional PRs with AI intelligence:

```bash
# Analyze and improve commit messages in your current branch
omni-dev git commit message twiddle 'origin/main..HEAD' --use-context

# Before: "fix stuff", "wip", "update files"
# After:  "feat(auth): implement OAuth2 authentication system"
#         "docs(api): add comprehensive endpoint documentation"
#         "fix(ui): resolve mobile responsive layout issues"

# Create a professional PR with AI-generated description
omni-dev git branch create pr
# 🎉 Generates comprehensive PR with detailed description, testing info, and more
```

## 📋 Core Commands

### 🤖 AI-Powered Commit Improvement (`twiddle`)

The star feature - intelligently improve your commit messages with real-time model information display:

```bash
# Improve commits with contextual intelligence
omni-dev git commit message twiddle 'origin/main..HEAD' --use-context

# Process large commit ranges with parallel processing
omni-dev git commit message twiddle 'HEAD~20..HEAD' --concurrency 5

# Save suggestions to file for review
omni-dev git commit message twiddle 'HEAD~5..HEAD' \
  --save-only suggestions.yaml

# Auto-apply improvements without confirmation
omni-dev git commit message twiddle 'HEAD~3..HEAD' --auto-apply
```

### 🔍 Analysis Commands

```bash
# Analyze commits in detail (YAML output)
omni-dev git commit message view 'HEAD~3..HEAD'

# Analyze current branch vs main
omni-dev git branch info main

# Get comprehensive help
omni-dev help-all
```

### 🚀 AI-Powered PR Creation

Create professional pull requests with AI-generated descriptions:

```bash
# Generate and create PR with AI-powered description
omni-dev git branch create pr

# Create PR with specific base branch
omni-dev git branch create pr main

# Save PR details to file without creating
omni-dev git branch create pr --save-only pr-description.yaml

# Auto-create without confirmation
omni-dev git branch create pr --auto-apply
```

### 📝 Atlassian Integration

Read, write, and manage JIRA issues and Confluence pages from the command line:

```bash
# Authenticate with Atlassian Cloud
omni-dev atlassian auth login

# Check authentication status
omni-dev atlassian auth status

# Fetch a JIRA issue as markdown
omni-dev atlassian jira read PROJ-123

# Fetch as raw ADF JSON
omni-dev atlassian jira read PROJ-123 --format adf

# Push markdown changes back to JIRA
omni-dev atlassian jira write PROJ-123 issue.md

# Interactive edit: fetch, edit in $EDITOR, push
omni-dev atlassian jira edit PROJ-123

# Search issues with JQL
omni-dev atlassian jira search --project PROJ --status Open

# Create an issue
omni-dev atlassian jira create issue.md --project PROJ --summary "Fix bug"

# Transition an issue
omni-dev atlassian jira transition PROJ-123 "In Progress"

# Confluence: read, search, create pages
omni-dev atlassian confluence read 12345
omni-dev atlassian confluence search --space ENG --title auth
omni-dev atlassian confluence create page.md --space ENG --title "New Page"

# Convert markdown to ADF JSON (offline)
omni-dev atlassian convert to-adf input.md
```

### 📊 Datadog Integration (read-only)

Authenticate against the Datadog API. Subsequent slices add metrics, monitor,
dashboard, and logs subcommands.

```bash
# Configure Datadog API credentials (prompts for API key, APP key, and site)
omni-dev datadog auth login

# Verify the credentials by calling /api/v1/validate
omni-dev datadog auth status

# Remove Datadog credentials from ~/.omni-dev/settings.json
omni-dev datadog auth logout
```

`DATADOG_SITE` defaults to `datadoghq.com`. Other regions (`datadoghq.eu`,
`us3.datadoghq.com`, `us5.datadoghq.com`, `ap1.datadoghq.com`, `ddog-gov.com`)
are recognised without warning. Environment variables `DATADOG_API_KEY`,
`DATADOG_APP_KEY`, `DATADOG_SITE` override the stored settings.

For on-prem or proxied Datadog installs, set `DATADOG_API_URL` to the full
API base URL (e.g. `https://datadog.corp.example`) — it overrides the
site-derived URL entirely.

### ✏️ Manual Amendment

```bash
# Apply specific amendments from YAML file
omni-dev git commit message amend amendments.yaml
```

### 🔌 MCP Server

omni-dev ships an optional **Model Context Protocol** server so AI assistants
(Claude Desktop, Claude Code, the MCP Inspector, custom agents) can call
omni-dev over stdio instead of shelling out to the CLI.

Tools currently exposed:

- `git_view_commits` — YAML commit analysis (mirrors `omni-dev git commit message view`)

Resources exposed via URI templates:

| URI template                   | Returns                   |
|--------------------------------|---------------------------|
| `git://repo/commits/{range}`   | YAML commit analysis      |
| `jira://issue/{key}`           | JIRA issue as JFM         |
| `jira://issue/{key}.adf`       | JIRA issue body as ADF    |
| `confluence://page/{id}`       | Confluence page as JFM    |
| `confluence://page/{id}.adf`   | Confluence page body as ADF |

#### Install

```bash
cargo install omni-dev --features mcp
```

This adds a second binary, `omni-dev-mcp`, alongside the regular `omni-dev`
CLI. The default `cargo install omni-dev` build is unchanged — no MCP
dependencies are pulled in unless the `mcp` feature is enabled.

#### Claude Desktop

Edit `~/Library/Application Support/Claude/claude_desktop_config.json` on
macOS (or `%APPDATA%\Claude\claude_desktop_config.json` on Windows):

```json
{
  "mcpServers": {
    "omni-dev": {
      "command": "omni-dev-mcp"
    }
  }
}
```

#### Claude Code

Per-project — create `.mcp.json` at the repo root:

```json
{
  "mcpServers": {
    "omni-dev": {
      "command": "omni-dev-mcp"
    }
  }
}
```

Or register globally with the Claude Code CLI:

```bash
claude mcp add omni-dev omni-dev-mcp
```

#### Smoke-test with the MCP Inspector

```bash
npx @modelcontextprotocol/inspector omni-dev-mcp
```

The Inspector opens a browser UI where you can list tools and resources,
call `git_view_commits`, and fetch `git://repo/commits/HEAD` against the
current working directory.

#### Troubleshooting

- **Logs go to stderr.** MCP uses stdin/stdout for protocol framing, so
  tracing output is routed to stderr — tail your client's MCP log pane or
  run the binary in a terminal to see it.
- **Verbose tracing:** `RUST_LOG=debug omni-dev-mcp` turns on debug-level
  logs. Module-scoped filters work too, e.g.
  `RUST_LOG=omni_dev::mcp=trace`.
- **Permission errors:** the assistant runs `omni-dev-mcp` with its own
  working directory. Tools that open a git repository use that directory
  unless an explicit `repo_path` parameter (or a resource URI placing you
  elsewhere) overrides it. If tool calls fail with "failed to open git
  repository", confirm the assistant launched the server from inside the
  repo you expected.

### ⚙️ Configuration Commands

```bash
# Show supported AI models and their specifications
omni-dev config models show

# View model information with token limits and capabilities
omni-dev config models show | grep -A5 "claude-opus-4.1"
```

## 🧠 Contextual Intelligence

omni-dev understands your project context to provide better suggestions:

### Project Configuration

Create `.omni-dev/` directory in your repo root:

```bash
mkdir .omni-dev
```

#### Scope Definitions (`.omni-dev/scopes.yaml`)

```yaml
scopes:
  - name: "auth"
    description: "Authentication and authorization systems"
    examples: ["auth: add OAuth2 support", "auth: fix token validation"]
    file_patterns: ["src/auth/**", "auth.rs"]
  
  - name: "api"
    description: "REST API endpoints and handlers"  
    examples: ["api: add user endpoints", "api: improve error responses"]
    file_patterns: ["src/api/**", "handlers/**"]
```

#### Commit Guidelines (`.omni-dev/commit-guidelines.md`)

```markdown
# Project Commit Guidelines

## Format
- Use conventional commits: `type(scope): description`
- Keep subject line under 50 characters
- Use imperative mood: "Add feature" not "Added feature"

## Our Scopes
- `auth` - Authentication systems
- `api` - REST API changes
- `ui` - Frontend/UI components
```

## 🎯 Advanced Features

### Intelligent Context Detection

omni-dev automatically detects:

- **Project Conventions**: From `.omni-dev/`, `CONTRIBUTING.md`
- **Work Patterns**: Feature development, bug fixes, documentation,
  refactoring
- **Branch Context**: Extracts work type from branch names
  (`feature/auth-system`)
- **File Architecture**: Understands UI, API, core logic, configuration
  changes
- **Change Significance**: Adjusts detail level based on impact

### Automatic Batching

Large commit ranges are automatically split into manageable batches:

```bash
# Processes 50 commits in batches of 4 (default)
omni-dev git commit message twiddle 'HEAD~50..HEAD' --use-context

# Custom concurrency for very large ranges
omni-dev git commit message twiddle 'main..HEAD' --concurrency 2
```

### Command Options

| Option | Description | Example |
|--------|-------------|---------|
| `--use-context` | Enable contextual intelligence | `--use-context` |
| `--concurrency N` | Number of parallel commit processors (default: 4) | `--concurrency 3` |
| `--no-coherence` | Skip cross-commit coherence refinement pass | `--no-coherence` |
| `--context-dir PATH` | Custom context directory | `--context-dir ./config` |
| `--auto-apply` | Apply without confirmation | `--auto-apply` |
| `--save-only FILE` | Save to file without applying | `--save-only fixes.yaml` |
| `--edit` | Edit amendments in external editor | `--edit` |

## 📖 Real-World Examples

### Before & After

**Before**: Messy commit history

```text
e4b2c1a fix stuff
a8d9f3e wip
c7e1b4f update files
9f2a6d8 more changes
```

**After**: Professional commit messages

```text
e4b2c1a feat(auth): implement JWT token validation system
a8d9f3e docs(api): add comprehensive OpenAPI documentation
c7e1b4f fix(ui): resolve mobile responsive layout issues
9f2a6d8 refactor(core): optimize database query performance
```

### Workflow Integration

```bash
# 1. Work on your feature branch
git checkout -b feature/user-dashboard

# 2. Make commits (don't worry about perfect messages)
git commit -m "wip"
git commit -m "fix stuff"
git commit -m "add more features"

# 3. Before merging, improve all commit messages
omni-dev git commit message twiddle 'main..HEAD' --use-context

# 4. Create professional PR with AI-generated description
omni-dev git branch create pr

# ✅ Professional commit history + comprehensive PR description ready for review
```

## Contributing

We welcome contributions! Please see our [Contributing Guidelines](CONTRIBUTING.md) for details.

### Development Setup

1. Clone the repository:

   ```bash
   git clone https://github.com/rust-works/omni-dev.git
   cd omni-dev
   ```

2. Install Rust (if you haven't already):

   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

3. Build the project:

   ```bash
   cargo build
   ```

4. Run the build script (includes tests, linting, and formatting):

   ```bash
   ./scripts/build.sh
   ```

   Or run individual steps:

   ```bash
   cargo test         # Run tests
   cargo clippy       # Run linting
   cargo fmt          # Format code
   ```

## 📚 Documentation

- **[User Guide](docs/user-guide.md)** - Comprehensive usage guide with examples
- **[Configuration Guide](docs/configuration.md)** - Set up contextual
  intelligence
- **[API Documentation](https://docs.rs/omni-dev)** - Rust API reference
- **[Troubleshooting](docs/troubleshooting.md)** - Common issues and
  solutions
- **[Examples](docs/examples.md)** - Real-world usage examples
- [Release Process](docs/RELEASE.md) - For contributors

## 🔧 Requirements

- **Rust**: 1.70+ (for installation from source)
- **Claude API Key**: Required for AI-powered features
  - Get your key from
    [Anthropic Console](https://console.anthropic.com/)
  - Set: `export CLAUDE_API_KEY="your-key"`
- **AI Model Selection**: Optional configuration for specific Claude models
  - View available models: `omni-dev config models show`
  - Configure via `~/.omni-dev/settings.json` or `ANTHROPIC_MODEL` environment variable
  - Supports standard identifiers and Bedrock-style formats
- **Atlassian Credentials** (for JIRA/Confluence features): Instance URL, email, and
  [API token](https://id.atlassian.com/manage-profile/security/api-tokens)
  - Configure with: `omni-dev atlassian auth login`
- **Datadog Credentials** (for Datadog features): API key, application key, and site
  - Configure with: `omni-dev datadog auth login`
- **Git**: Any modern version

### AI backend selection

By default, `omni-dev` calls the Anthropic API (or Bedrock/OpenAI/Ollama via
the `USE_*`/`CLAUDE_CODE_USE_BEDROCK` env vars). As an alternative, you can
route AI calls through an already-authenticated
[Claude Code CLI](https://github.com/anthropics/claude-code) session, avoiding
the need for a separate API key:

```bash
# Per-invocation flag:
omni-dev --ai-backend claude-cli git commit message twiddle <range>

# Or set persistently:
export OMNI_DEV_AI_BACKEND=claude-cli
```

The flag takes precedence over the environment variable.

**Model selection** follows the precedence chain
`--model` → `CLAUDE_MODEL` → `CLAUDE_CODE_MODEL` → `ANTHROPIC_MODEL` → registry
default. Short aliases (`sonnet`, `opus`, `haiku`) and full identifiers
(`claude-sonnet-4-6`) are both accepted and forwarded verbatim to
`claude -p --model`.

**Sandboxing guarantees.** The `claude -p` subprocess is locked down so it
behaves as a pure prompt→completion service, not a nested agent with
filesystem or shell access:

- Built-in tools disabled (`--tools ""`).
- MCP servers blocked (`--strict-mcp-config` with no config).
- User/project/local settings ignored (`--setting-sources ""`).
- Slash commands / skills disabled.
- Session persistence disabled.
- Subprocess runs in a fresh temp directory (not your repo root).
- Environment is scrubbed of `CLAUDE_PROJECT_DIR`, `CLAUDE_CODE_*`, and
  `CLAUDE_PROJECT_*` before spawn.

**Cost baseline.** Because the `claude -p` session still loads a small system
prompt, each invocation has a floor cost (~$0.007 Haiku, ~$0.03 Sonnet,
~$0.15 Opus) before any user content. Back-to-back calls within one hour hit
the prompt cache and are much cheaper. If you pay per token, compare against
the default HTTP backend which has no such floor.

**Overrides.** Optional environment variables:

- `OMNI_DEV_CLAUDE_CLI_BIN` — path to the `claude` binary (default: resolved
  from `PATH`).
- `OMNI_DEV_CLAUDE_CLI_TIMEOUT_SECS` — subprocess timeout (default: 600).
- `OMNI_DEV_CLAUDE_CLI_STDOUT_MAX_BYTES` — stdout cap in bytes (default:
  4 MiB).
- `OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS` — **escape hatch** (default: disabled).
  See below.
- `OMNI_DEV_CLAUDE_CLI_ALLOW_MCP` — **escape hatch** for MCP server pickup
  (default: disabled). See below.
- `OMNI_DEV_CLAUDE_CLI_MAX_BUDGET_USD` — per-invocation spending cap in USD
  (default: none). See below.

The `--beta-header` flag is ignored with this backend (the CLI's `--betas`
flag is API-key-user-only and has different semantics).

#### Escape hatch: `--claude-cli-allow-tools`

By default the nested `claude -p` session is run with `--tools ""` and
cannot read, edit, or execute anything on your system. For deliberately
tool-capable use cases, you can weaken the sandbox:

```bash
omni-dev --ai-backend claude-cli --claude-cli-allow-tools git branch create pr
# or:
export OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS=true
```

**When enabled**, the nested session uses the CLI's default built-in tool
set (Read / Edit / Write / Bash / Glob / Grep). This means the session can
access your repository and run commands. Only enable it when you want that
behaviour. When active, omni-dev logs a warning on every invocation.

`--strict-mcp-config` and `--setting-sources ""` still apply unless you
*also* enable `--claude-cli-allow-mcp` (see below). The two escape hatches
are independent so you can grant tool access without exposing MCP server
credentials, and vice-versa.

#### Escape hatch: `--claude-cli-allow-mcp`

By default the nested `claude -p` session is run with `--strict-mcp-config`
and no `--mcp-config`, blocking every MCP server you have configured in
`~/.claude/settings.json`. To re-enable MCP server pickup:

```bash
omni-dev --ai-backend claude-cli --claude-cli-allow-mcp git branch create pr
# or:
export OMNI_DEV_CLAUDE_CLI_ALLOW_MCP=true
```

**When enabled**, the nested session can connect to any MCP server in your
user settings. Be aware that MCP servers commonly hold OAuth tokens (Gmail,
Drive, Slack) or expose internal network services; enabling this exposes
them to the nested session. Only enable it when you want that behaviour.
When active, omni-dev logs a warning on every invocation.

This flag is independent of `--claude-cli-allow-tools`. Built-in tools
remain disabled unless you enable that flag separately.

#### Spending cap: `--claude-cli-max-budget-usd`

Pass a per-invocation spending cap in USD:

```bash
omni-dev --ai-backend claude-cli --claude-cli-max-budget-usd 0.50 \
  git commit message twiddle HEAD~3..HEAD
# or:
export OMNI_DEV_CLAUDE_CLI_MAX_BUDGET_USD=0.50
```

The value is forwarded to `claude -p --max-budget-usd`. If the nested
session exceeds the cap, it aborts with an error rather than running away
with cost. Regardless of whether a cap is set, each invocation's
`total_cost_usd` is logged at INFO level for observability — run with
`RUST_LOG=omni_dev=info` to see it.

The cap is ignored when `--ai-backend` is not `claude-cli`. Non-positive,
non-finite, or non-numeric values are silently treated as no cap.

## 🐛 Debugging

For troubleshooting and detailed logging, use the `RUST_LOG` environment variable:

```bash
# Enable debug logging for omni-dev components
RUST_LOG=omni_dev=debug omni-dev git commit message twiddle ...

# Debug specific modules (e.g., context discovery)  
RUST_LOG=omni_dev::claude::context::discovery=debug omni-dev git commit message twiddle ...

# Show only errors and warnings
RUST_LOG=warn omni-dev git commit message twiddle ...
```

See [Troubleshooting Guide](docs/troubleshooting.md) for detailed debugging information.

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for a list of changes in each version.

## License

This project is licensed under the BSD 3-Clause License - see the
[LICENSE](LICENSE) file for details.

## Support

- 📋 [Issues](https://github.com/rust-works/omni-dev/issues)
- 💬 [Discussions](https://github.com/rust-works/omni-dev/discussions)

## Acknowledgments

- Thanks to all contributors who help make this project better!
- Built with ❤️ using Rust
