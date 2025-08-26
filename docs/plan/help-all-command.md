# Project Plan: Comprehensive Help Command (`help all`)

## Overview
Implement a `help all` subcommand that displays the complete CLI help hierarchy for omni-dev in a single output, eliminating the need to run multiple help commands.

## Goals
- Provide complete CLI documentation in one command
- Show all subcommands, options, and usage patterns
- Maintain clear hierarchy and formatting
- Enable easy documentation generation and golden testing

## Current CLI Structure
```
omni-dev
├── git
│   ├── commit
│   │   ├── message
│   │   │   ├── view [RANGE]
│   │   │   └── amend <YAML_FILE>
│   └── branch
│       └── info [BASE_BRANCH]
└── help [COMMAND]
```

## Implementation Plan

### Phase 1: Command Structure Design
1. **Add Help Command Category**
   - Create new `HelpCommand` struct in `src/cli/mod.rs`
   - Add `Help(HelpCommand)` to main `Commands` enum
   - Support both `help` and `help all` syntax

2. **Command Parsing**
   - `help` - show main help (existing behavior)
   - `help all` - show comprehensive help tree
   - `help <subcommand>` - show specific subcommand help (existing behavior)

### Phase 2: Help Generation Engine
1. **Help Tree Structure**
   - Create `HelpNode` struct to represent command hierarchy
   - Implement recursive help collection from clap
   - Format with proper indentation and hierarchy

2. **Output Format**
   ```
   omni-dev - A comprehensive development toolkit
   
   USAGE:
       omni-dev <COMMAND>
   
   COMMANDS:
       git                    Git-related operations
       help                   Print help information
   
   ---
   
   omni-dev git - Git-related operations
   
   USAGE:
       omni-dev git <COMMAND>
   
   COMMANDS:
       commit                 Commit-related operations  
       branch                 Branch-related operations
   
   [... continues for all subcommands ...]
   ```

### Phase 3: Implementation Details

#### 3.1 File Structure
- `src/cli/help.rs` - New help command module
- `src/cli/mod.rs` - Updated to include help commands
- Update imports and exports

#### 3.2 Core Components

1. **HelpCommand Struct**
   ```rust
   #[derive(Parser)]
   pub struct HelpCommand {
       #[command(subcommand)]
       pub command: Option<HelpSubcommands>,
   }
   
   #[derive(Subcommand)]
   pub enum HelpSubcommands {
       /// Show comprehensive help for all commands
       All,
   }
   ```

2. **Help Generator**
   ```rust
   pub struct HelpGenerator {
       app: Command,
   }
   
   impl HelpGenerator {
       pub fn new() -> Self { ... }
       pub fn generate_all_help(&self) -> String { ... }
       fn collect_help_recursive(&self, cmd: &Command, prefix: &str) -> Vec<String> { ... }
   }
   ```

3. **Help Formatting**
   - Clear section separators
   - Consistent indentation (2 spaces per level)
   - Command path breadcrumbs
   - Proper spacing between sections

### Phase 4: Integration
1. **Update Main CLI**
   - Add `Help(HelpCommand)` to `Commands` enum
   - Implement execution logic in `Cli::execute()`
   - Maintain backward compatibility with existing help

2. **Error Handling**
   - Graceful fallback if help generation fails
   - Clear error messages for malformed help requests

### Phase 5: Testing & Documentation
1. **Unit Tests**
   - Test help tree generation
   - Verify output format consistency
   - Test all command variations

2. **Integration Tests**
   - Golden tests for complete help output
   - Verify help content matches actual commands

3. **Documentation Updates**
   - Update README.md with new help command
   - Add examples to CHANGELOG.md

## Technical Considerations

### Clap Integration
- Use `clap::Command::get_subcommands()` for traversal
- Leverage `clap::Command::render_help()` for formatting
- Maintain clap's built-in help styling

### Performance
- Cache help content on first generation
- Lazy evaluation of help tree
- Minimal memory overhead

### Extensibility
- Design for easy addition of new subcommands
- Support custom help sections
- Allow help filtering/search in future

## Success Criteria
1. ✅ `omni-dev help all` shows complete CLI hierarchy
2. ✅ Output is well-formatted and readable
3. ✅ All existing help functionality preserved
4. ✅ Golden tests validate output consistency
5. ✅ Documentation updated with examples

## Future Enhancements
- `help all --format json` for machine-readable output
- `help search <term>` for command discovery
- Interactive help browser
- Man page generation from help output

## Timeline
- Phase 1-2: 1-2 hours (command structure + help engine)
- Phase 3: 2-3 hours (implementation)
- Phase 4: 1 hour (integration)
- Phase 5: 1-2 hours (testing + docs)

**Total Estimated Time: 5-8 hours**

## Dependencies
- No new external dependencies required
- Uses existing `clap` functionality
- Compatible with current CLI architecture