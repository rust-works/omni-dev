# omni-dev

[![Crates.io](https://img.shields.io/crates/v/omni-dev.svg)](https://crates.io/crates/omni-dev)
[![Documentation](https://docs.rs/omni-dev/badge.svg)](https://docs.rs/omni-dev)
[![Build Status](https://github.com/rust-works/omni-dev/workflows/CI/badge.svg)](https://github.com/rust-works/omni-dev/actions)
[![License: BSD-3-Clause](https://img.shields.io/badge/License-BSD%203--Clause-blue.svg)](LICENSE)

A powerful Git commit message analysis and amendment toolkit written in Rust.

## Features

- 🔍 **Commit Analysis**: Comprehensive analysis of git commits with YAML output
- ✏️ **Smart Amendment**: Amend single or multiple commit messages safely
- 🎯 **Conventional Commits**: Automatic detection and suggestions for conventional commit format
- 🛡️ **Safety First**: Working directory validation and error recovery
- 📊 **Rich Information**: File changes, diff summaries, and remote branch tracking
- ⚡ **Fast & Reliable**: Built with Rust for memory safety and performance

## Installation

### From crates.io

```bash
cargo install omni-dev
```

### From source

```bash
git clone https://github.com/rust-works/omni-dev.git
cd omni-dev
cargo build --release
```

## Usage

### Command Line Interface

```bash
# View and analyze commits
omni-dev git commit message view HEAD~3..HEAD

# Amend commit messages from a YAML file
omni-dev git commit message amend amendments.yaml

# Get help
omni-dev --help
```

### Viewing Commits

Analyze commits in a range and get comprehensive information:

```bash
# Analyze recent commits
omni-dev git commit message view HEAD~5..HEAD

# Analyze commits since main branch
omni-dev git commit message view origin/main..HEAD
```

This outputs detailed YAML with:
- Commit metadata (hash, author, date)
- File changes and diff statistics
- Conventional commit type detection
- Proposed commit message improvements
- Remote branch tracking information

### Amending Commits

Create a YAML file with your desired commit message changes:

```yaml
amendments:
  - commit: "abc123def456..."
    message: |
      feat: add user authentication system
      
      Implement OAuth 2.0 authentication with JWT tokens:
      - Add login and logout endpoints  
      - Implement token validation middleware
      - Add user session management
```

Then apply the amendments:

```bash
omni-dev git commit message amend amendments.yaml
```

The tool safely handles:
- Single HEAD commit amendments
- Multi-commit amendments via interactive rebase
- Working directory safety checks
- Automatic error recovery

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

4. Run tests:
   ```bash
   cargo test
   ```

5. Run clippy for linting:
   ```bash
   cargo clippy
   ```

6. Format code:
   ```bash
   cargo fmt
   ```

## Documentation

- [API Documentation](https://docs.rs/omni-dev)
- [Project Plan](docs/plan/project.md)
- [Field Documentation](docs/plan/project.md) - Complete specification of YAML output fields

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for a list of changes in each version.

## License

This project is licensed under the BSD 3-Clause License - see the [LICENSE](LICENSE) file for details.

## Support

- 📋 [Issues](https://github.com/rust-works/omni-dev/issues)
- 💬 [Discussions](https://github.com/rust-works/omni-dev/discussions)

## Acknowledgments

- Thanks to all contributors who help make this project better!
- Built with ❤️ using Rust