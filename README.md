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

# Process large commit ranges with automatic batching
omni-dev git commit message twiddle 'HEAD~20..HEAD' --batch-size 5

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

### ✏️ Manual Amendment

```bash
# Apply specific amendments from YAML file
omni-dev git commit message amend amendments.yaml
```

### ⚙️ Configuration Commands

```bash
# Show supported AI models and their specifications
omni-dev config models show

# View model information with token limits and capabilities
omni-dev config models show | grep -A5 "claude-sonnet-4"
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

# Custom batch size for very large ranges
omni-dev git commit message twiddle 'main..HEAD' --batch-size 2
```

### Command Options

| Option | Description | Example |
|--------|-------------|---------|
| `--use-context` | Enable contextual intelligence | `--use-context` |
| `--batch-size N` | Set batch size for large ranges | `--batch-size 3` |
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
- **Git**: Any modern version

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
