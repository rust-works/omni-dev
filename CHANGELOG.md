# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.12.0] - 2025-12-25

### Added
- **Commit Message Validation Command**: New `check` command for validating commit messages against project guidelines
  - AI-powered analysis with configurable severity levels (error, warning, info)
  - Multiple output formats (text, JSON, YAML) for CI/CD integration
  - Batch processing support for large commit ranges
  - Smart exit codes for pipeline integration (0=pass, 1=errors, 2=warnings in strict mode)
  - Optional suggestion generation for improved commit messages
  - Color-coded severity indicators in text output
- **Fresh Mode for Twiddle**: Generate commit messages from scratch ignoring existing messages
  - New `--fresh` flag for twiddle command
  - Forces AI to analyze only diff content for completely fresh suggestions
  - Useful for poorly-written or misleading original messages
- **Base Branch Support**: Explicit base branch selection for PR creation and updates
  - New `--base` flag for `create pr` command
  - Intelligent base branch resolution with fallback logic
  - Interactive confirmation when changing base branch on updates
  - Better visibility of target branches in PR operations
- **Comprehensive Gemini Model Support**: Full Google Gemini model catalog
  - Gemini 3.0 Pro and Flash (preview models)
  - Gemini 2.5 series (Pro, Flash, Flash-Lite)
  - Legacy support for Gemini 2.0 and 1.5 series
  - Three-tier system (flagship, balanced, fast) for model selection

### Changed
- **AI Model Registry Update**: Updated to latest model releases (December 2025)
  - Added Claude 4.5 series (Opus, Sonnet, Haiku) as current generation
  - Updated default Claude model to claude-sonnet-4-5-20250929
  - Added OpenAI GPT-5.2, o3/o4 reasoning models, and GPT-4.1 series
  - Marked legacy models appropriately for deprecation visibility

### Refactored
- **Commit Guidelines Template**: Extracted default guidelines to shared template file
  - Single source of truth in `src/templates/default-commit-guidelines.md`
  - Consistent guidelines between twiddle and check commands
  - Easier maintenance and editing as markdown

### Documentation
- **Enhanced Commit Guidelines**: Comprehensive guidelines with severity levels
  - Detailed type and scope tables with clear use cases
  - Subject line rules with imperative mood requirements
  - Accuracy requirements section for truthful descriptions
  - Severity level mapping for CI/CD integration
- **Commit Message Check Plan**: Detailed implementation specification
  - Design philosophy for guideline-driven validation
  - Command structure and output format examples
  - CI integration patterns and exit code behavior

## [0.11.0] - 2025-12-10

### Added
- **Draft PR Support**: New draft PR functionality with configurable defaults
  - Added `--draft` flag to PR creation command for creating draft pull requests
  - Configurable default draft status via `.omni-dev/pr-config.yaml`
  - Enhanced PR workflow with draft mode for work-in-progress changes
- **No-AI Mode for Twiddle**: Direct YAML output without AI processing
  - Added `--no-ai` flag to twiddle command for direct YAML generation
  - Enables manual editing workflows without AI-powered amendment
  - Better integration with custom automation pipelines

### Documentation
- **AI-Generated PR Guidelines**: Comprehensive documentation for PR description generation
  - Detailed guidelines for AI-powered PR description creation
  - Best practices and examples for effective PR generation
  - Enhanced documentation for team collaboration

### Fixed
- **PR Creation Branch Handling**: Improved head branch parameter handling
  - Fixed explicit head branch parameter in `gh pr create` command
  - Better handling of upstream branch configuration
  - More reliable PR creation workflow

## [0.10.0] - 2025-09-30

### Added
- **Branch Information in Twiddle**: Enhanced twiddle repository view with branch information
  - Branch context now included in commit analysis and AI-powered amendments
  - Better understanding of current branch status for more targeted suggestions
  - Improved repository view completeness for AI assistants

### Enhanced
- **AI Model Configuration**: Updated default models to Claude Opus 4.1
  - Latest AI model specifications for improved performance
  - Enhanced model registry with updated token limits and capabilities
  - Better AI response quality and accuracy
- **PR Command User Experience**: Improved PR command UX by showing context early
  - Faster feedback for users during PR creation process
  - Better progress indicators and context display
  - Enhanced user interface clarity
- **PR Template Integration**: Enhanced PR template location exposure in repository views
  - PR template location now visible in repository analysis
  - Better integration between template system and PR creation workflow
  - Improved AI understanding of project PR standards

### Documentation
- **Comprehensive Scope Documentation**: Added detailed scope documentation and usage examples
  - Complete guide for scope usage patterns and best practices
  - Real-world examples and configuration scenarios
  - Enhanced developer documentation for project customization

## [0.9.0] - 2025-09-18

### Added
- **AI-Powered Pull Request Creation**: New `git create pr` command with intelligent PR generation
  - Automatically generates PR titles and descriptions using AI analysis of commits and diffs
  - Supports both interactive creation and save-only modes for review
  - Integrates with GitHub CLI for seamless PR creation and updates
  - Context-aware analysis using project-specific guidelines and branch information
- **PR Guidelines System**: Project-specific PR description guidelines support
  - New `.omni-dev/pr-guidelines.md` configuration file for PR generation guidance
  - Separate from commit guidelines to allow different standards for PRs vs commits
  - Local override support with priority: local > project > global
  - Integration with AI prompts for project-consistent PR descriptions
- **Enhanced PR Template**: Significantly improved `.github/pull_request_template.md`
  - Added comprehensive sections for testing, performance, security, and deployment
  - Better structure and guidance for thorough PR descriptions
  - Includes examples and best practices for different types of changes
- **YAML Output Format**: New structured output format for PR details
  - `pr-details.yaml` replaces `pr_description.md` for better structured data
  - Complete PR content serialization including title and description
  - Better integration with automation and tooling workflows

### Enhanced
- **Context-Aware AI Generation**: PR creation now uses full project context
  - Leverages branch analysis, work patterns, and architectural understanding
  - Project-specific scope validation and suggestions for PR organization
  - Enhanced prompts that incorporate both commit analysis and PR best practices
- **Command-Specific Guidance Display**: Improved user interface clarity
  - Twiddle command shows only commit guidelines (focused on commit messages)
  - PR creation command shows only PR guidelines (focused on PR descriptions)
  - Eliminates confusion about which guidelines are being used for each operation
- **Comprehensive Documentation**: Updated user guides and README
  - Added complete workflow documentation for PR creation feature
  - Enhanced examples and usage patterns for both commit and PR workflows
  - Better organization of feature documentation and command references

### Fixed
- **YAML Parsing Robustness**: Improved Claude API response processing
  - Better handling of markdown-wrapped YAML responses from AI
  - Consistent parsing logic across commit amendments and PR generation
  - Enhanced error diagnostics for malformed AI responses

## [0.8.0] - 2025-09-17

### Added
- **AI Model Configuration System**: New `config models show` command to view available AI models
  - Complete model registry with token limits and specifications
  - Support for both standard Claude and AWS Bedrock identifier formats
  - Model information display in twiddle command output
- **Interactive Amendment Editing**: `--edit` option for twiddle command
  - Integration with `OMNI_DEV_EDITOR` and `EDITOR` environment variables
  - Manual review and editing of AI-generated amendments before applying
- **Build Automation Script**: New `scripts/build.sh` for standardized builds
  - Combines cargo build, format checking, and clippy analysis
  - Comprehensive error handling and progress indicators

### Enhanced
- **Contextual Intelligence System**: Significantly improved commit message generation
  - Home directory fallback support for all `.omni-dev` configuration files
  - Literal template reproduction ensures AI follows project formats exactly
  - Enhanced diagnostic output showing guidance file status and sources
- **AI Client Logging**: Improved debugging and observability
  - Enhanced logging for API requests and responses
  - Better error handling and diagnostics for troubleshooting

### Removed
- **Commit Template System**: Removed template functionality to simplify configuration
  - Projects should use commit guidelines instead of templates
  - Eliminates conflicts between templates and guidelines
  - **BREAKING**: `.gitmessage` and commit template files are no longer loaded

## [0.7.0] - 2025-09-14

### Added
- **AWS Bedrock AI Client**: Complete integration with AWS Bedrock for Claude AI model access
  - Implemented `BedrockAiClient` with full AWS API support
  - Added comprehensive logging and diagnostics for troubleshooting
  - Support for AWS credentials and region configuration
  - Integration with existing `AiClient` trait architecture
- **AI Client Architecture**: Extensible AI provider system
  - New `AiClient` trait for pluggable AI providers
  - Provider selection and configuration management
  - Support for multiple AI service backends
- **Settings Management System**: Enhanced configuration handling
  - New settings management utilities for AI provider configuration
  - Environment-based configuration support
  - Structured settings validation and loading

### Improved
- **Code Quality**: Resolved clippy warnings for better maintainability
  - Fixed `vec_init_then_push` patterns with `vec![]` macro usage
  - Improved code consistency and performance

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

[Unreleased]: https://github.com/rust-works/omni-dev/compare/v0.12.0...HEAD
[0.12.0]: https://github.com/rust-works/omni-dev/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/rust-works/omni-dev/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/rust-works/omni-dev/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/rust-works/omni-dev/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/rust-works/omni-dev/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/rust-works/omni-dev/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/rust-works/omni-dev/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/rust-works/omni-dev/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/rust-works/omni-dev/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/rust-works/omni-dev/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/rust-works/omni-dev/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/rust-works/omni-dev/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/rust-works/omni-dev/releases/tag/v0.1.0