# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
- **Empty Task Checkboxes**: Recognise `- [ ]` and `- [x]` markdown task markers
  even when the checkbox is not followed by a trailing space. Fixes ADF
  round-trip drift where empty `taskItem` nodes were parsed as `listItem`
  nodes containing literal `[ ]` text (issue #548).
- **Literal Checkbox Text in Bullet Lists**: Escape the leading `[` when
  rendering a `bulletList` item whose literal text begins with a sequence
  that looks like a task checkbox marker (`[ ]`, `[x]`, or `[X]` followed by
  space, newline, or end). Prevents `to-adf` from falsely promoting these
  bullet items to `taskList`/`taskItem` on round-trip (issue #548).
- **ADF Round-Trip URL Brackets** (#551): Preserve square brackets in URLs
  embedded in link-marked text so ADF→JFM→ADF round-trips no longer leak
  `\[`/`\]` escapes or split the text into corrupted `inlineCard` nodes.

## [0.21.0] - 2026-04-17

### Added
- **Confluence Label Commands**: Label management commands for adding, removing, and listing page labels
- **Confluence User Search**: Search for Confluence users by query
- **Confluence Page Comments**: List and add comments on Confluence pages
- **JIRA Watcher Commands**: List, add, and remove watchers on JIRA issues
- **JIRA Worklog Commands**: Time tracking with worklog list and add commands
- **JIRA Sprint Management**: Create and update sprint commands
- **JIRA Dev Status**: Dev status command for viewing development information on issues
- **PR Auto-Push**: `--no-push` flag to skip branch push on PR creation
- **ADF Node Support**: Added `mediaInline`, `placeholder`, `table caption`, `mediaSingle caption`, and `expand localId/parameters` node support
- **ADF Annotation Marks**: Full annotation mark support for ADF/markdown conversion
- **ADF localId Round-Trip**: Comprehensive `localId` round-trip support with `--strip-local-ids` option
- **ADF Border Mark**: Border mark support for media and table cell/header nodes
- **ADF Breakout Width**: Optional width parameter for breakout marks

### Fixed
- **ADF Round-Trip Fidelity** (50+ fixes): Extensive improvements to ADF/markdown round-trip conversion:
  - Preserve mark ordering (link, annotation, strong/em/strike, code) across conversions
  - Prevent consecutive paragraphs from merging in blockquotes, list items, and task items
  - Preserve `localId` on caption, listItem/mediaSingle, layout columns, media, table, and paragraph nodes
  - Handle nested taskList, taskItem, and ordered list nodes correctly
  - Preserve hardBreak nodes in paragraphs, headings, list items, and table cells
  - Preserve trailing/leading whitespace in headings, list items, table cells, and text nodes
  - Escape backticks, backslashes, asterisks, and underscores in plain text to prevent misinterpretation
  - Preserve NBSP content in list item paragraphs and NBSP-only paragraphs
  - Preserve empty language attrs in codeBlock, empty attrs on tableCell, and integer colwidth values
  - Handle parentheses in link/image URLs and bracket-link ambiguity
  - Prevent bare URLs and URL link text from becoming inlineCard nodes
  - Preserve emoji shortName with/without colons, date timestamps, and mention localId attribution
  - Preserve parameters block in bodiedExtension and extension layout/localId attrs
  - Preserve multiple annotation marks, table cell attrs, embedCard attrs, and mediaSingle mode
  - Preserve content after directive table blocks and whitespace-only text after hardBreak
  - Reject pipe syntax for tableCell-only first rows and intraword underscores as emphasis
  - Escape trailing double-spaces to prevent hardBreak misinterpretation
  - Ensure blank lines between consecutive block nodes in table cells
- **Deterministic AI Scopes**: Make AI-generated commit scopes deterministic by post-processing with file-pattern logic
- **CI Scope Guidance**: Improve CI scope guidance in type selection rules

### Changed
- **Function Naming**: Rename `execute_*` helper functions to `run_*` across Atlassian CLI commands
- **Function Extraction**: Extract `run_download`, `run_images`, `run_transition` into standalone functions

### Documentation
- **ADR-0020**: Add ADR for JFM markdown dialect for ADF interchange
- **JFM Spec**: Expand JFM spec with table, media, localId, escaping, and annotation mark sections
- **Style Guide**: Update style guide to clarify `run_*` function extraction rules and add testing style rules

### CI/CD
- Bump `EmbarkStudios/cargo-deny-action` to 2.0.17
- Bump `codecov/codecov-action` from 5 to 6
- Bump `cachix/cachix-action` from 16 to 17
- Bump Rust minor patch dependencies

### Security
- Update `rustls-webpki` to 0.103.12 for RUSTSEC-2026-0098

## [0.20.0] - 2026-04-12

### Added
- **Confluence Download**: Recursive page tree download with concurrent workers
  - BFS tree traversal via Confluence children API
  - Bounded parallel downloads with configurable concurrency (`--concurrency`)
  - Directory tree mirroring with `{id}-{slug}/index.{md,json}` structure
  - Manifest-based resume (`--resume`) with ID-aware page tracking
  - Per-page `meta.json` with untruncated titles and parent IDs
  - Backup-before-clobber with `--on-conflict backup|skip|overwrite`
  - Append-mode `download.log` recording all actions per run
  - Configurable max depth (`--max-depth`)
- **Structured Output**: `--output json|yaml|table` flag on all list/table commands
  - Added `Serialize` derives to all public Atlassian data types
  - `OutputFormat` enum with `output_as()` helper
- **HTTP 429 Rate Limiting**: Automatic retry with `Retry-After` header support
  - All transport methods (`get_json`, `post_json`, `put_json`, `delete`, `get_bytes`) retry on 429
  - Exponential backoff fallback when no `Retry-After` header is present
  - Configurable max retries (default: 3)

### Fixed
- **Nested Container Directives**: Container directives (`:::expand`, `:::panel`) inside table cells, layout columns, and other containers are now correctly parsed with depth tracking
- **hardBreak in Table Cells**: Tables containing `hardBreak` nodes now fall back to directive form instead of pipe tables, preventing row corruption on round-trip
- **Multi-Paragraph Containers**: Panels, expands, layout columns, and extensions with multiple paragraphs now render with blank-line separators, preserving paragraph boundaries on round-trip
- **Commit Message Generator**: Added type selection rules to align generator with checker expectations — prevents incorrect type selection (e.g., `docs` for source code changes)

## [0.19.0] - 2026-04-10

### Added
- **Atlassian Integration**: Comprehensive JIRA and Confluence CLI commands via JFM (JIRA-Flavored Markdown) format
  - Read and write JIRA and Confluence content as JFM markdown
  - JIRA issue create, delete, search (JQL), and transition commands
  - JIRA comment list and add commands
  - JIRA issue link management and link list commands
  - JIRA issue attachment download commands
  - JIRA issue changelog command
  - JIRA agile board and sprint management commands
  - JIRA project list and field listing/options commands
  - Auto-discover field context when fetching JIRA field options
  - Confluence search (CQL), create, and delete commands with purge flag
  - `post_json` and `delete` methods on `AtlassianClient`
- **Auto-Pagination**: Automatic pagination for all Atlassian API methods
- **Claude CLI Model Resolve**: `claude cli model resolve` command for model resolution diagnostics

### Changed
- **CLI Help Text**: Improved help text and converted key arguments to positional for Atlassian commands

### Fixed
- **Confluence Delete Error**: Improved 404 error message for confluence delete command
- **JIRA Search Endpoint**: Updated search endpoint and handle missing `total` field in response

### Security
- **CI Hardening**: Pin `cargo-deny-action` version and update vulnerable dependencies

### Testing
- **Field Context Error Handling**: Added test for `get_field_contexts` 404 error response

### Documentation
- **Atlassian User Guide**: Comprehensive command reference for all Atlassian CLI commands
- **JFM Specification**: Moved JFM spec from plan to specs directory with revised content
- **Atlassian Scope**: Added `atlassian` scope to scope list
- **v0.18.0 Retrospective**: Added release retrospective document

## [0.18.0] - 2026-02-26

### Added
- **Split Dispatch for Large Diffs**: Intelligent per-file diff splitting when commits exceed token budgets
  - Per-file and per-hunk unified diff parser for granular diff handling
  - Per-file diff storage with `FileDiffRef` struct tracking byte lengths
  - Greedy file-packing algorithm for token-budget-constrained splitting
  - Split dispatch across amendment, check, and multi-commit operations
  - Per-hunk diff override support for partial commit views
  - Placeholder substitution for oversized diffs instead of hard failures
- **File-Level Context Analysis**: Hook-based file analyzer adds semantic context to commit pipelines
- **Walk-Up Config Directory Discovery**: Config resolution walks up the directory tree to find `.omni-dev/` directories
- **XDG Base Directory Compliance**: Config resolution follows XDG standards for fallback locations
- **`OMNI_DEV_CONFIG_DIR` Environment Variable**: Explicit config directory override for all commands
- **Config Source Tracking**: Diagnostic output shows where each config file was loaded from
- **`--quiet` Flag**: Suppress interactive retry prompts in twiddle
- **`--context-dir` Option**: Explicit context directory for create-pr command
- **`--refine` Flag**: Opt-in to refine mode (fresh mode is now the default for twiddle)
- **Amendment Parse Retry Logic**: Automatic retry on amendment parse and AI request failures
- **Claude Sonnet 4.6 Model**: Added to model registry and set as default
- **Preflight Checks Expanded**: Applied consistently to amend, view, and info commands
- **Configurable Mock AI Client**: Shared test utility for integration testing

### Changed
- **Fresh Mode Default**: Twiddle now generates messages from scratch by default; use `--refine` to amend existing messages
- **Default AI Model**: Claude Sonnet 4.6 replaces previous default
- **Registry Default Model**: Uses model registry default instead of hardcoded model strings

### Removed
- **`--batch-size` Flag**: Removed deprecated flag from check and twiddle commands (use `--concurrency` instead)
- **Progressive Diff Reduction**: Removed fallback strategy in favor of split dispatch
- **Dead Utils Module**: Removed unused `utils/general` module and its re-exports

### Fixed
- **Batch Processing Failure Tracking**: Track failed commit indices when batch processing errors occur
- **Progress Counter**: Increment only on success path to avoid inflated counts
- **Split Dispatch Overhead**: Correctly subtract prompt overhead from chunk capacity
- **Token Estimation**: Use conservative estimation for code diffs
- **Error Chain Display**: Print full error chain for failed commits in twiddle
- **Tracing Output**: Write tracing output to stderr instead of stdout
- **Stdin EOF Loop**: Prevent infinite loop on EOF in interactive prompts
- **Provider String Matching**: Use exact string matching in prompt_style selection
- **Field Presence Tracking**: Add missing `branch_prs[].base` field
- **Batch-Size Deprecation**: Proper deprecation warning for `--batch-size` flag

### Security
- **Dependency Advisories**: Update `bytes` and `git2` to resolve security advisories
- **CI Hardening**: Add security audit, dependency policy, and secret scanning workflows

### Refactored
- **Generic Repository View**: Make `RepositoryView` and `CommitInfo` generic over inner types
- **Single Async Runtime**: Migrate command execution to single tokio runtime
- **Consolidated Config Resolution**: Unified config resolution into discovery module
- **Panic-Free Operations**: Replace panicking operations with proper error handling across all modules
- **Pure Logic Extraction**: Extract and test pure logic from twiddle, create-pr, and check commands
- **Interactive Retry Extraction**: Extract interactive retry loop and `read_interactive_line` helper into testable methods
- **Shared Formatting Utilities**: Extract formatting module for reuse across commands
- **AI Client Helpers**: Extract shared helpers for AI client implementations
- **Deduplicated Models Embed**: Shared constant for `models.yaml` embedding

### Testing
- **Property-Based Tests**: Added proptest-based tests across 7 modules
- **Comprehensive Unit Tests**: Added tests for 6+ previously untested modules
- **Split Dispatch Integration Tests**: Integration tests with prompt recording
- **Test Directory Isolation**: Relocated temp directories to project-local `tmp/` folder
- **Parallel Test Safety**: Fix process-wide CWD mutation causing parallel test failures

### Documentation
- **Architecture Decision Records**: Added ADRs 0004–0019 covering embedded templates, hierarchical config resolution, two-view data model, preflight validation, deterministic pre-validation, token-budget batch planning, multi-layer retry, model registry, severity levels, self-describing YAML, provider-specific prompts, dual error handling, hierarchical CLI, per-file diff splitting, context detection, and ecosystem scope auto-detection
- **Architecture Documentation**: Comprehensive architecture docs for the codebase
- **Style Guide**: Broadened scope, added STYLE-0022 (ADR format) and STYLE-0023 (commit validation)
- **Config Resolution Docs**: Four-tier config resolution with walk-up discovery and XDG support

### CI/CD
- **Code Coverage Enforcement**: Enforce minimum coverage threshold and fail on codecov errors
- **GitHub Actions Updates**: Bump actions/checkout to v6, codecov-action to v5, cachix/install-nix-action to v31, cachix/cachix-action to v16
- **Clippy Pedantic Lints**: Enable pedantic and nursery lint groups

### Dependencies
- `crossterm` 0.28 → 0.29
- `dirs` 5.0 → 6.0
- `ssh2-config` 0.6 → 0.7
- `thiserror` 1.x → 2.x
- `reqwest` 0.12 → 0.13
- Rust minor-patch group with 13 updates

## [0.17.0] - 2026-02-13

### Added
- **Ecosystem Default Scopes**: Automatic scope detection based on project ecosystem
  - Detects Rust, Node.js, Python, Go, and Java projects from marker files (Cargo.toml, package.json, etc.)
  - Merges ecosystem-specific default scopes (e.g., `cargo`, `lib`, `core`, `test` for Rust)
  - Skips defaults that conflict with existing custom scopes in `scopes.yaml`
  - Works consistently across twiddle, check, and PR creation commands
- **Scope Pre-Validation**: Deterministic scope checks before AI processing
  - Validates scope format (e.g., multi-scope comma separation without spaces)
  - Verifies scope validity against the merged scope list before sending to AI
  - Passing checks recorded in `pre_validated_checks` field so the AI skips re-checking them
  - Prevents AI from contradicting deterministic validations

### Fixed
- **Config Loading**: Always load `.omni-dev/` configuration regardless of directory existence
  - Previously skipped config loading when the context directory didn't exist as a directory
  - Now correctly resolves individual config files even when the parent directory is absent
  - Fixes scope and guideline loading in projects without an explicit `.omni-dev/` directory

### Refactored
- **Scope Loading Consolidation**: Unified scope loading across all commands
  - Extracted `load_project_scopes()` as a single entry point for scope resolution
  - Consistent config file priority (local override → project → home fallback) everywhere
  - Eliminated duplicated scope loading logic between twiddle and check commands

### Documentation
- **Configuration Best Practices**: New guide for `.omni-dev/` configuration
  - Scope definition patterns, file pattern matching, and local override workflows
  - Troubleshooting guide for common configuration issues
- **Configuration Internals**: New technical reference for configuration resolution
  - Detailed explanation of config file priority, ecosystem detection, and scope merging
  - Architecture diagrams for the discovery pipeline

## [0.16.0] - 2026-02-12

### Added
- **Parallel Map-Reduce Processing**: Replaced sequential batch processing with concurrent commit processing
  - Each commit processed individually in parallel using semaphore-based concurrency control
  - New `--concurrency` flag (default: 4) replaces deprecated `--batch-size`
  - Real-time progress feedback with atomic completion counters
  - Graceful failure handling continues processing remaining commits
- **Cross-Commit Coherence Pass**: Optional AI refinement for consistency across commit messages
  - Ensures consistent scope usage, terminology, and message quality across a commit set
  - New `--no-coherence` flag to skip the coherence pass when not needed
  - Automatically skipped when all commits fit in a single batch
- **Token-Budget-Aware Commit Batching**: Intelligent grouping using first-fit-decreasing bin-packing
  - Groups commits into batches that fit within the AI model's token budget
  - Estimates tokens from file metadata without reading full content
  - Split-and-retry fallback for oversized batches with progressive diff reduction
  - Reduces API calls from O(n) to O(batches) while maintaining quality
- **Progressive Diff Reduction**: Four-level fallback for token budget optimization
  - Automatically reduces diff detail when prompts exceed model limits: Full → Truncated → StatOnly → FileListOnly
  - Precise truncation calculations with tokens-to-chars conversion
  - Maximizes context sent to AI while respecting model constraints
- **Token Budget Validation**: Pre-flight token estimation and budget check before all AI requests
  - Estimates prompt token count using a character-based heuristic with 10% safety margin
  - Validates prompts fit within the model's input context window minus reserved output tokens
  - Returns a clear `PromptTooLarge` error instead of letting the API reject oversized requests
  - Covers all AI call paths: twiddle, check, PR creation, and raw message sending
- **HTTP Request Timeout Configuration**: Configurable timeout for AI client HTTP requests
- **Enhanced YAML Formatting**: Improved multi-line commit message formatting in YAML output

### Changed
- **Deprecated `--batch-size`**: Replaced by `--concurrency` flag with clearer semantics; `--batch-size` remains as a hidden backward-compatible alias

### Refactored
- **Module Structure Flattening**: Converted `mod.rs` files to direct module files across claude, cli, data, and git modules
- **Git CLI Split**: Split monolithic git module into focused subcommand modules
- **YAML Payload Reduction**: Reduced per-commit YAML payload size for more efficient AI analysis
- **Dead Code Removal**: Removed unused core module scaffolding

### Fixed
- **Error Handling**: Improved error handling and configuration parsing in AI client

### Documentation
- **Architecture Decision Records**: Introduced ADR framework with ADR-0001 (YAML as primary data exchange format)
- **Style Guide Enhancements**: Added tag-based categorization system, task-to-tag lookup table, and STYLE-0020 single-purpose commit guidelines
- **Commit Guidelines**: Enhanced with multi-scope support and practical examples
- **Module Layout Guidance**: Refined examples and guidance for module organization
- **Documentation Updates**: Updated all docs to reflect `--concurrency` replacing `--batch-size`

## [0.15.0] - 2026-02-08

### Added
- **Beta Header Support**: New `--beta-header` flag for twiddle and check commands
  - Enables enhanced model capabilities like 1M context window and 128K output tokens
  - Format: `--beta-header key:value` (e.g., `--beta-header anthropic-beta:context-1m-2025-08-07`)
  - Validates beta headers against the model registry with helpful error messages
  - Beta-aware token limits automatically applied to API requests and display
  - Debug logging for active beta headers sent with API requests
- **Interactive Chat Command**: New `omni-dev chat` command for conversational AI interaction
  - Interactive Claude AI chat session with streaming-style responses
  - Configurable system prompts and model selection
  - Multi-line input support and conversation history
- **Interactive Twiddle Mode for Check**: New `--twiddle` flag on check command
  - Automatically runs twiddle to fix failing commit messages after check identifies issues
  - Streamlined workflow for validating and correcting commits in one step
- **Intelligent Retry Mechanism**: Smart retry for twiddle commit validation
  - Automatically retries failed commit message generation with refined prompts
  - Configurable retry limits with exponential backoff
  - Improved success rates for challenging commit messages
- **Deterministic Scope Pre-Validation**: Rule-based validation before AI processing
  - Catches common scope formatting issues (e.g., extra spaces) without API calls
  - Reduces unnecessary AI requests for deterministic formatting rules

### Changed
- **Model Catalog Update**: Updated AI model registry to February 2026
  - Added Claude Opus 4.6 as current flagship model
  - Added beta header definitions for models supporting extended context and output
  - Updated model specifications and tier classifications

### CI/CD
- **Enhanced Commit-Check Workflow**: Improved CI validation pipeline
  - Added concurrency control to prevent redundant workflow runs
  - Updated GitHub Actions to latest versions

### Documentation
- **Context Window Documentation**: Added documentation for context window limitations and fallback behavior

## [0.14.0] - 2026-02-08

### Added
- **Scope Refinement via File Patterns**: Intelligent scope detection that matches changed file paths against configured scope patterns from `.omni-dev/scopes.yaml`
  - Pattern matching using globset for project-specific scope rules
  - Specificity-based matching prioritizes more specific patterns
  - Support for negation patterns and multi-scope matching
  - Fallback to original detection when no patterns match
  - Applied across twiddle, check, and validation commands
- **Preflight Validation System**: Comprehensive early failure detection for AI and GitHub commands
  - AI provider detection and credential validation for Claude, Bedrock, OpenAI, and Ollama
  - GitHub CLI availability and authentication checks
  - Clear, actionable error messages with resolution guidance
  - Integrated into twiddle, create-pr, and check commands
- **Working Directory Validation**: Early cleanliness check before expensive twiddle operations
  - Detects staged changes, unstaged modifications, and untracked files
  - Provides detailed error messages showing specific uncommitted files
  - Prevents wasted AI processing time on dirty working directories
- **Model Parameter for create-pr**: Added `--model` flag to create-pr command for model selection

### Changed
- **Scope Definitions Loading**: Simplified and consolidated scope definitions loading logic in twiddle command
  - Scope refinement now works consistently with or without contextual intelligence
  - Same logic pattern applied to both full and batch processing modes

## [0.13.1] - 2025-01-07

### Fixed
- **Bedrock Client Selection Logic**: Fixed inverted conditional that prevented Bedrock from being used
  - Setting `CLAUDE_CODE_USE_BEDROCK=true` now correctly uses Bedrock client
  - Removed confusing `CLAUDE_CODE_SKIP_BEDROCK_AUTH` requirement
  - Users only need `CLAUDE_CODE_USE_BEDROCK=true`, `ANTHROPIC_AUTH_TOKEN`, and `ANTHROPIC_BEDROCK_BASE_URL`
- **CI Publish Ordering**: Publish to crates.io only after all platform builds succeed

### Added
- **Scope Definitions**: Added `release` and `workflows` scopes for better commit categorization
  - `release`: Version bumps, changelog updates, release preparation
  - `workflows`: GitHub Actions and CI/CD pipeline changes

### Changed
- **CI Commit Check**: Trigger commit validation on push to main branch
- **CI Workflow**: Removed version pinning from commit-check workflow

## [0.13.0] - 2025-12-27

### Added
- **Post-Twiddle Validation**: New `--check` flag for twiddle command
  - Automatically validates commit messages after applying amendments
  - Runs full AI-powered analysis against project guidelines
  - Supports batched processing for large commit ranges
  - Single-step workflow: improve and validate in one command
- **Guidance File Diagnostics**: Enhanced diagnostic output for loaded configuration
  - Shows status of commit guidelines, scopes, and other guidance files
  - Clear visibility into which configuration files are being used
  - Helps troubleshoot configuration issues
- **Scope Validation in Check**: Enhanced commit message checking with scope awareness
  - Validates commit scopes against project-defined scope list
  - Reports invalid or missing scopes as warnings

### Changed
- **CI Workflow Enhancement**: Added commit message validation for pull requests
  - New GitHub Actions workflow validates PR commit messages
  - Automatic quality enforcement on all pull requests

### Documentation
- **Release Process Restructure**: Comprehensive overhaul of release documentation
  - Reorganized for automated CI/CD workflow with clear manual vs automated steps
  - Added documentation review phase before version updates
  - Enhanced with CI monitoring commands and verification steps
  - Improved release skill with complete automation guidance
- **README Updates**: Added documentation for check command and new twiddle options
  - New section for commit message validation command
  - Updated options table with `--fresh` and `--check` flags

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

[Unreleased]: https://github.com/rust-works/omni-dev/compare/v0.21.0...HEAD
[0.21.0]: https://github.com/rust-works/omni-dev/compare/v0.20.0...v0.21.0
[0.20.0]: https://github.com/rust-works/omni-dev/compare/v0.19.0...v0.20.0
[0.19.0]: https://github.com/rust-works/omni-dev/compare/v0.18.0...v0.19.0
[0.18.0]: https://github.com/rust-works/omni-dev/compare/v0.17.0...v0.18.0
[0.17.0]: https://github.com/rust-works/omni-dev/compare/v0.16.0...v0.17.0
[0.16.0]: https://github.com/rust-works/omni-dev/compare/v0.15.0...v0.16.0
[0.15.0]: https://github.com/rust-works/omni-dev/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/rust-works/omni-dev/compare/v0.13.1...v0.14.0
[0.13.1]: https://github.com/rust-works/omni-dev/compare/v0.13.0...v0.13.1
[0.13.0]: https://github.com/rust-works/omni-dev/compare/v0.12.0...v0.13.0
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