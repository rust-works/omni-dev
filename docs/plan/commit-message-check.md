# Plan: `omni-dev git commit message check` Command

## Overview

Add a new `omni-dev git commit message check` command that analyzes commit messages against the actual code changes (diffs) and **project-defined commit guidelines**, producing a report of issues without modifying any commits.

## Design Philosophy

**The check command should be guideline-driven, not opinionated.**

Rather than hardcoding specific rules (like "subject must be under 50 chars" or "must use imperative mood"), the command should:

1. Read commit guidelines from the project's context directory
2. Use AI to evaluate commits against those guidelines
3. Report violations of the project's own standards

This means:
- Different projects can have different rules
- The check command doesn't impose conventional commit format unless the project requires it
- All quality criteria are configurable via commit guidelines

## Commit Guidelines Structure

Projects define their commit guidelines in `.omni-dev/commit-guidelines.md` (or similar). The check command reads and enforces these.

### Example Commit Guidelines File

```markdown
# Commit Message Guidelines

## Format
- Use conventional commit format: `<type>(<scope>): <description>`
- Valid types: feat, fix, docs, style, refactor, test, chore, ci, perf, build
- Valid scopes: cli, git, data, claude, api
- Subject line must be under 72 characters
- Use imperative mood ("add" not "added")

## Content
- Description must be specific, not vague
- Body required for changes over 100 lines
- Reference issue numbers in footer when applicable

## Accuracy
- Commit type must match actual changes
- Scope must match files modified
- Description must reflect what was actually done
```

The AI parses these guidelines and checks commits against them.

## Command Structure

```
omni-dev git commit message check [OPTIONS] [COMMIT_RANGE]
```

### Arguments

| Argument | Description |
|----------|-------------|
| `COMMIT_RANGE` | Optional commit range (e.g., `HEAD~3..HEAD`). Defaults to commits ahead of main branch. |

### Options

| Option | Description |
|--------|-------------|
| `--model <MODEL>` | AI model to use for analysis |
| `--context-dir <PATH>` | Custom context directory for guidelines |
| `--guidelines <PATH>` | Explicit path to guidelines file |
| `--format <FORMAT>` | Output format: `text` (default), `json`, `yaml` |
| `--strict` | Exit with error code if any issues found |
| `--quiet` | Only show issues, suppress informational output |

## Implementation Steps

### Step 1: Add CLI Structure

**File:** [src/cli/git.rs](src/cli/git.rs)

1. Add `Check` variant to `MessageSubcommands` enum
2. Create `CheckCommand` struct with options
3. Implement `CheckCommand::execute()` method

### Step 2: Define Check Result Types

**File:** [src/data/check.rs](src/data/check.rs) (new file)

```rust
pub struct CheckReport {
    pub commits: Vec<CommitCheckResult>,
    pub summary: CheckSummary,
}

pub struct CommitCheckResult {
    pub hash: String,
    pub message: String,
    pub issues: Vec<CommitIssue>,
    pub passes: bool,
}

pub struct CommitIssue {
    pub severity: IssueSeverity,
    pub guideline: String,       // Which guideline was violated
    pub explanation: String,     // How it was violated
    pub suggestion: Option<String>,
}

pub enum IssueSeverity {
    Error,
    Warning,
    Info,
}
```

### Step 3: Create AI Check Prompt

**File:** [src/claude/prompts.rs](src/claude/prompts.rs)

Add a new prompt that:
1. Receives commit data (message + diff)
2. Receives project guidelines
3. Returns structured analysis of guideline violations

```rust
pub const CHECK_SYSTEM_PROMPT: &str = r#"You are a commit message reviewer.
You will receive:
1. Project commit guidelines
2. Commit information including the message and diff

Your task is to check if each commit message follows the project's guidelines.

For each commit, analyze:
- Does the message format match what the guidelines require?
- Does the content meet the guidelines' quality standards?
- Does the message accurately describe the actual code changes in the diff?

Report violations as structured YAML..."#;
```

### Step 4: Implement Check Logic

**File:** [src/cli/git.rs](src/cli/git.rs)

The check flow:
1. Load project commit guidelines (from context directory)
2. Generate repository view with commits and diffs
3. Send to AI with check prompt
4. Parse and display results

### Step 5: Output Formatting

Implement formatters for text, JSON, and YAML output.

---

## What the AI Checks (Guideline-Driven)

The AI evaluates commits against whatever the project's guidelines specify. Common guideline categories include:

### Format Rules (if specified in guidelines)
- Conventional commit structure
- Subject line length limits
- Required sections (body, footer)
- Scope requirements

### Content Rules (if specified in guidelines)
- Verb tense requirements
- Specificity requirements
- Body requirements for large changes
- Issue reference requirements

### Accuracy Rules (always checked against diff)
- Does the commit type match the actual changes?
- Does the scope match files modified?
- Does the description reflect what was done?
- Are important changes mentioned?

The accuracy checks are the core value-add: comparing what the message *claims* against what the diff *shows*.

---

## Example Output

### Text Format (Default)

```
Checking 3 commits...

❌ abc1234 - "update stuff"
   ERROR  [format] missing_type: No conventional commit type found
   ERROR  [content] vague_description: Description "update stuff" is too vague

⚠️  def5678 - "feat(cli): Added new command for checking"
   WARNING [content] past_tense: Use imperative mood - "add" instead of "Added"
   WARNING [format] description_too_long: Subject line is 58 characters (recommended: 50)

✅ ghi9012 - "fix(git): resolve parsing error for merge commits"
   No issues found

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Summary: 3 commits checked
  2 errors, 2 warnings, 0 info
  1 commit passed, 2 commits have issues
```

### With AI Analysis

```
Checking 2 commits with AI analysis...

⚠️  abc1234 - "feat(api): add endpoint"
   WARNING [accuracy] missing_key_changes: Message doesn't mention the rate limiting
           logic that was added (see lines 45-67 in src/api/handler.rs)
   INFO    [content] empty_body_for_complex_change: 127 lines changed across 4 files;
           consider adding a body to explain the implementation

   Suggestion:
   feat(api): add user registration endpoint

   Implement POST /api/users endpoint with:
   - Input validation for email and password
   - Rate limiting (5 requests/minute per IP)
   - Password hashing with bcrypt

   Closes #123
```

---

## Exit Codes

| Code | Meaning |
|------|---------|
| 0 | All commits pass (no errors, possibly warnings) |
| 1 | One or more commits have errors |
| 2 | One or more commits have warnings (only with `--strict`) |

---

## Integration Points

### Reusable Components from Twiddle

1. `generate_repository_view()` - Get commit data with diffs
2. `collect_context()` - Load project guidelines and context
3. `ClaudeClient` - AI analysis infrastructure
4. `CommitAnalysis` - Existing type/scope detection

### New Components

1. `CheckReport` - Result data structure
2. Static check functions - Non-AI validation
3. AI check prompts - Accuracy analysis prompts
4. Output formatters - Text/JSON/YAML rendering

---

## Future Enhancements

1. **CI Integration** - GitHub Actions output format
2. **Pre-commit Hook** - Integration with git hooks
3. **Fix Suggestions** - Generate corrected messages
4. **Batch Mode** - Process multiple branches/PRs
5. **Custom Rules** - User-defined check rules
6. **Ignore Patterns** - Skip certain commits or rules
