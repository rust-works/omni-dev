# Project Plan: omni-dev CLI Git Tools

## Overview

This project implements a CLI tool that provides the functionality of the existing `.claude/commands/twiddle-msg` bash script but with a Rust-based CLI interface. The tool will support two main subcommands:

- `omni-dev git commit message view [commit-range]` - Analyze git commits and output comprehensive repository information in YAML format
- `omni-dev git commit message amend <yaml-file>` - Amend commit messages based on a YAML configuration file

## Project Architecture

### Core Components

1. **CLI Interface Layer** (`src/cli/`)
   - Command line argument parsing using `clap`
   - Subcommand routing and validation
   - User input/output handling with colored terminal output

2. **Git Operations Layer** (`src/git/`)
   - Repository analysis and validation
   - Commit message extraction and modification
   - Working directory status checking
   - Remote branch detection and main branch identification
   - Interactive rebase operations for non-HEAD commits

3. **Data Processing Layer** (`src/data/`)
   - YAML parsing and generation
   - JSON intermediate processing
   - Commit analysis and conventional commit type detection
   - File change analysis and scope determination

4. **Core Application Layer** (`src/core/`)
   - Main application logic coordination
   - Error handling and validation
   - Configuration management

### Key Features to Implement

#### 1. Git Repository Analysis (`git commit message view` command)
- **Working Directory Status**: Check for uncommitted changes, untracked files
- **Remote Analysis**: Detect all remotes, identify main branches (main/master), get URLs
- **Commit Analysis**: For each commit in range:
  - Extract metadata (hash, author, date, original message)
  - Detect conventional commit type (feat, fix, docs, test, chore, etc.)
  - Analyze file changes to determine scope
  - Generate proposed commit messages
  - Check if commit exists in remote main branches
  - Provide detailed file change statistics

#### 2. Commit Amendment (`git commit message amend` command)
- **YAML Processing**: Parse amendment files with commit hash → message mapping
- **Safety Checks**: 
  - Validate working directory is clean
  - Prevent amendment of commits already in remote main branches
  - Skip merge commits and empty commits
- **Amendment Operations**:
  - Simple HEAD amendment for latest commits
  - Interactive rebase for older commits in history
  - Preserve commit metadata and trailers

#### 3. Data Output Format
- **Structured YAML Output**: Comprehensive repository information including:
  - Field explanations and documentation
  - Working directory status
  - Remote repository information
  - Per-commit analysis with conventional commit suggestions
  - File change details and diff summaries

### Technical Specifications

#### Dependencies
```toml
clap = { version = "4.0", features = ["derive", "color"] }
git2 = "0.18"
serde = { version = "1.0", features = ["derive"] }
serde_yaml = "0.9"
serde_json = "1.0"
termcolor = "1.1"
anyhow = "1.0"
chrono = { version = "0.4", features = ["serde"] }
```

#### CLI Interface Structure
```
omni-dev git commit message <subcommand> [options] [args...]

Subcommands:
  view [commit-range]    Output commit analysis in YAML format
  amend <yaml-file>      Amend commits with messages from YAML file

Examples:
  omni-dev git commit message view HEAD~3..HEAD
  omni-dev git commit message amend amendments.yml
```

#### YAML Amendment File Format
```yaml
amendments:
  - commit: "full-40-character-sha1-hash"
    message: "New commit message"
  - commit: "another-full-commit-hash"
    message: "Another new commit message"
```

### Implementation Plan

#### Phase 1: Foundation
1. **Project Structure Setup**
   - Update `Cargo.toml` with required dependencies
   - Create modular directory structure (`cli/`, `git/`, `data/`)
   - Set up basic CLI interface with `clap`

2. **Core Git Operations**
   - Implement repository detection and validation
   - Create git repository wrapper with basic operations
   - Add working directory status checking

#### Phase 2: Repository Analysis
1. **Remote Detection**
   - Implement remote repository enumeration
   - Add main branch detection logic (main/master/develop)
   - Create remote information structures

2. **Commit Analysis Engine**
   - Build commit metadata extraction
   - Implement file change analysis
   - Add conventional commit type detection
   - Create scope analysis based on file paths

#### Phase 3: Data Processing
1. **YAML Output Generation**
   - Implement structured data output with field documentation
   - Add comprehensive commit information formatting
   - Create working directory and remote status reporting

2. **YAML Input Processing**
   - Build YAML amendment file parser
   - Add validation for commit hashes and message format
   - Implement safety checks for amendment operations

#### Phase 4: Amendment Operations
1. **Simple Amendment**
   - Implement HEAD commit message amendment
   - Add commit metadata preservation

2. **Complex Amendment (Rebase)**
   - Build interactive rebase wrapper
   - Implement multi-commit amendment with proper ordering
   - Add robust error handling and recovery

#### Phase 5: Testing and Validation
1. **Unit Testing**
   - Test all core functionality modules
   - Validate git operations in various repository states
   - Test YAML parsing and generation

2. **Integration Testing**
   - Test complete workflows with real git repositories
   - Validate amendment operations don't corrupt repository
   - Test error handling and edge cases

### Error Handling Strategy

#### Validation Levels
1. **Input Validation**
   - CLI argument validation
   - File existence and format checking
   - Git repository validation

2. **Operation Safety**
   - Working directory cleanliness checks
   - Remote branch ancestry validation
   - Commit existence verification

3. **Recovery Mechanisms**
   - Graceful failure for invalid operations
   - Clear error messages with suggested fixes
   - Automatic cleanup for failed rebase operations

### Security Considerations

1. **Git Repository Safety**
   - Never modify repositories with uncommitted changes
   - Refuse to amend commits already in remote main branches
   - Validate all commit hashes before operations

2. **Input Validation**
   - Sanitize YAML input to prevent injection
   - Validate file paths and commit ranges
   - Limit resource usage for large repositories

### Performance Considerations

1. **Git Operations**
   - Use libgit2 for efficient repository access
   - Minimize git command executions
   - Cache expensive operations (remote branch detection)

2. **Data Processing**
   - Stream processing for large commit ranges
   - Efficient YAML/JSON serialization
   - Memory-conscious file change analysis

### Compatibility

1. **Git Versions**
   - Support git 2.0+ repositories
   - Handle various remote URL formats (SSH, HTTPS)
   - Compatible with GitHub, GitLab, and other Git hosting

2. **Operating Systems**
   - Cross-platform Rust implementation
   - Handle platform-specific path separators
   - Terminal color support detection

### Future Enhancements

1. **Additional Features**
   - Configuration file support
   - Custom commit type detection rules
   - Integration with commit message templates

2. **Performance Optimizations**
   - Parallel processing for multiple commits
   - Incremental analysis caching
   - Background remote branch updates

### Success Criteria

1. **Functional Parity**
   - Complete replacement for bash script functionality
   - Identical YAML output format
   - Same amendment operation safety

2. **Improved User Experience**
   - Faster execution than shell script
   - Better error messages and validation
   - Cross-platform compatibility

3. **Code Quality**
   - Comprehensive test coverage (>90%)
   - Clean, maintainable Rust code
   - Proper error handling throughout

### Timeline Estimate

- **Phase 1 (Foundation)**: 2-3 days
- **Phase 2 (Analysis)**: 3-4 days  
- **Phase 3 (Data Processing)**: 2-3 days
- **Phase 4 (Amendment)**: 4-5 days
- **Phase 5 (Testing)**: 2-3 days

**Total Estimated Time**: 13-18 days

### Risks and Mitigation

1. **Git2 Library Complexity**
   - Risk: libgit2 binding complexity
   - Mitigation: Start with simple operations, expand gradually

2. **Interactive Rebase Challenges**
   - Risk: Complex rebase operations may fail
   - Mitigation: Implement robust error handling and recovery

3. **Cross-platform Compatibility**
   - Risk: Platform-specific git behaviors
   - Mitigation: Extensive testing on multiple platforms

This comprehensive plan provides a roadmap for implementing a robust, safe, and efficient replacement for the existing bash script while maintaining full compatibility and adding improvements in performance and user experience.