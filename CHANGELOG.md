# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0] - 2025-09-09

### Added
- **File-based Amendment Workflow**: Complete overhaul of the twiddle command user experience
  - Save amendments to temporary YAML files instead of printing to stdout
  - Interactive menu system with [A]pply/[S]how/[Q]uit options 
  - File content preview functionality for reviewing changes before applying
  - Better user feedback and more granular control over amendment process
  - Preserved backward compatibility with `--auto-apply` and `--save-only` options

- **Local Configuration Overrides**: Personal workflow customization system
  - Support for `.omni-dev/local/` directory to override shared project settings
  - Local override capability for all configuration files (scopes, guidelines, templates)
  - Priority system: local overrides take precedence over shared project configuration
  - Automatic `.gitignore` exclusion to keep personal settings private
  - Comprehensive documentation for setup and usage patterns

- **Structured Debug Logging**: Professional logging system using `RUST_LOG`
  - Integration with `tracing` and `tracing-subscriber` for structured logging
  - Module-specific debug control (e.g., `RUST_LOG=omni_dev::claude=debug`)
  - Detailed diagnostic information for troubleshooting configuration and API issues
  - Comprehensive documentation in troubleshooting guide
  - Replaced custom verbose flag with industry-standard logging approach

### Improved
- **YAML Output Formatting**: Enhanced readability of amendment files
  - Automatic conversion of multiline commit messages to YAML literal block scalars (`|`)
  - Proper formatting instead of escaped newlines in quoted strings
  - Better preserved indentation and structure in generated files
  - Improved user experience when reviewing amendment content

### Removed
- **Verbose Flag**: Removed `--verbose`/`-v` CLI option in favor of `RUST_LOG` environment variable
  - More flexible and powerful debugging control through standard Rust logging
  - Better performance with zero overhead when logging is disabled
  - Industry-standard approach familiar to Rust developers

### Documentation
- **Comprehensive RUST_LOG Documentation**: Added detailed logging guides
  - Basic usage examples and log level explanations
  - Module-specific targeting for focused debugging
  - Common troubleshooting scenarios with specific commands
  - Updated README.md with debugging section
- **Local Override Documentation**: Complete guide for personal configuration management
  - Setup instructions and best practices
  - Real-world usage examples and patterns
  - Integration with team workflows

## [0.5.0] - 2025-09-01

### Changed
- **Diff Output Format**: Modified YAML output to write diff content to external files instead of embedding in YAML
  - Changed `diff_content` field to `diff_file` in `CommitAnalysis` struct for improved memory usage
  - Diff content now written to temporary files in AI scratch directory
  - Enables AI assistants to access detailed diff information through file reads
  - Maintains backward compatibility with similar data structure
  - Updated field documentation for AI assistant guidance

## [0.4.1] - 2025-08-29

### Fixed
- **Rebase Operations**: Fixed short commit hash ambiguity in interactive rebase operations
  - Modified rebase sequence generation to use full commit hashes instead of 7-character truncated hashes
  - Eliminates "short object ID is ambiguous" errors when multiple git objects share the same hash prefix
  - Ensures reliable commit amendment operations regardless of repository size

## [0.4.0] - 2025-08-29

### Added
- **Command Template Management**: New command template system for enhanced CLI experience
  - Added `pr-update` command template generation for pull request workflow automation
  - Implemented comprehensive command template management system
  - Enhanced Claude slash command integration with structured templates
- **AI Scratch Directory Support**: Added AI scratch directory configuration support
  - Integrated AI_SCRATCH environment variable support for enhanced AI assistant workflows
  - Added scratch directory path handling in command templates
- **Version Information Enhancement**: Added version information to command outputs
  - Commands now include version context for better debugging and support
  - Enhanced output format with version tracking
- **Documentation Improvements**: Enhanced slash command documentation structure
  - Improved Claude command file organization and documentation
  - Added comprehensive AI assistant guide and release documentation
  - Better structured troubleshooting information in slash commands

## [0.3.0] - 2025-08-26

### Added
- **Field Presence Tracking**: Enhanced YAML output with explicit field presence indicators
  - Added `present: bool` field to `FieldDocumentation` struct for AI assistant guidance
  - Implemented `update_field_presence()` method on `RepositoryView` to dynamically track available fields
  - Added comprehensive AI assistant guidance in field explanation text
  - Included git command mappings for better field documentation
- **Enhanced Command Structure**: Reorganized Claude command files with improved analysis instructions
  - Added commit-twiddle commands for debug, release, and standard modes
  - Added pr-create commands with enhanced PR workflow decision guidance
  - Standardized command structure across all variants with detailed field checking instructions

### Changed
- **Data Structure Improvements**: Reordered RepositoryView fields to place commits last for better readability
  - Summary fields (explanation, working_directory, remotes, branch_info, pr_template, branch_prs) now appear before detailed commit analysis
  - Improved YAML output organization and user experience

### Fixed
- **Code Quality**: Resolved clippy warnings for better code quality
  - Replaced deprecated `map_or(false, |prs| !prs.is_empty())` patterns with `is_some_and(|prs| !prs.is_empty())`
  - Maintained proper borrowing semantics with `.as_ref()` calls

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

[Unreleased]: https://github.com/rust-works/omni-dev/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/rust-works/omni-dev/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/rust-works/omni-dev/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/rust-works/omni-dev/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/rust-works/omni-dev/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/rust-works/omni-dev/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/rust-works/omni-dev/releases/tag/v0.1.0