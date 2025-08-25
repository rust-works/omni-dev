# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2025-08-26

### Added
- **Git Branch Analysis**: New `omni-dev git branch info` command for comprehensive branch analysis
  - Branch-aware commit analysis with automatic range calculation
  - Current branch detection and validation
  - Base branch comparison (defaults to main/master)
  - Enhanced YAML output including branch context
- **GitHub Integration**: GitHub CLI integration for enhanced functionality
  - Accurate main branch detection using GitHub API
  - Pull request information retrieval and display
  - PR template support with conditional YAML output
  - GitHub repository URI parsing and validation
- **Git Commit Analysis**: Comprehensive commit analysis with YAML output
  - Commit metadata extraction (hash, author, date)
  - File change analysis and diff statistics
  - Conventional commit type detection
  - Remote branch tracking and main branch detection
  - Working directory status reporting
- **Commit Message Amendment**: Safe and reliable commit message modification
  - HEAD commit amendment using `git commit --amend`
  - Multi-commit amendment via individual interactive rebases
  - Shell-script-inspired strategy for reliable rebase operations
  - YAML-based amendment file format with validation
- **Safety Features**: Comprehensive safety checks and error handling
  - Working directory cleanliness validation (ignoring build artifacts)
  - Commit existence and accessibility validation
  - Automatic rebase abort and error recovery
  - Prevention of amendments to potentially problematic commits
- **CLI Interface**: Full-featured command-line interface
  - `omni-dev git commit message view [range]` - Analyze and view commits
  - `omni-dev git commit message amend <yaml-file>` - Amend commit messages
  - `omni-dev git branch info [base-branch]` - Analyze branch commits
  - Rich help system and error reporting
- **Testing Infrastructure**: Comprehensive test suite
  - Integration tests with temporary git repositories
  - Amendment functionality validation
  - YAML parsing and validation tests
  - Error handling and edge case testing

### Changed
- Complete rewrite of core functionality to focus on git commit operations
- Updated CLI interface to provide git-specific commands
- Enhanced error handling with detailed context and recovery options
- Remote information now uses `uri` field instead of `url` for consistency

### Fixed
- Working directory safety checks now properly ignore git-ignored files
- Multi-commit amendment reliability improved with individual rebase strategy
- Clippy linting warnings resolved (needless_borrows_for_generic_args)
- Compilation warnings eliminated through dead code cleanup

## [0.1.0] - 2024-08-24

### Added
- Initial release of omni-dev
- Basic project structure and configuration
- CLI application with version and help commands
- Core application framework with configuration support
- Utility functions for input validation and byte formatting
- Comprehensive test suite
- GitHub Actions CI/CD pipeline
- Documentation and community files (README, CONTRIBUTING, CODE_OF_CONDUCT)
- BSD 3-Clause license

[Unreleased]: https://github.com/rust-works/omni-dev/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/rust-works/omni-dev/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/rust-works/omni-dev/releases/tag/v0.1.0