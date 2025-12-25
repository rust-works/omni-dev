# Plan: `omni-dev git commit message check` Command

## Overview

Add a new `omni-dev git commit message check` command that analyzes commit messages against the actual code changes (diffs) and **project-defined commit guidelines**, producing a report of issues without modifying any commits.

Key characteristics:
- **Non-interactive**: Designed for CI pipelines and automated checks
- **Exit codes**: Returns error exit code when severity-error violations are found
- **Suggestions included**: Provides corrected messages with explanations by default

## Design Philosophy

**The check command should be guideline-driven, not opinionated.**

Rather than hardcoding specific rules (like "subject must be under 50 chars" or "must use imperative mood"), the command should:

1. Read commit guidelines from the project's context directory (falls back to built-in defaults)
2. Use AI to evaluate commits against those guidelines
3. Report violations with severity levels defined in the guidelines
4. Suggest corrections with explanations

This means:
- Different projects can have different rules
- The check command doesn't impose conventional commit format unless the project requires it
- Severity levels (error/warning/info) are defined in the guidelines themselves
- Projects without guidelines get sensible defaults

## Commit Guidelines Structure

Projects define their commit guidelines in `.omni-dev/commit-guidelines.md`. The check command reads and enforces these. Guidelines should specify severity levels for violations.

### Example Commit Guidelines File

```markdown
# Commit Message Guidelines

## Severity Levels

| Severity | Sections                      |
|----------|-------------------------------|
| error    | Format, Subject Line, Accuracy |
| warning  | Content                       |
| info     | Style                         |

## Format
- Use conventional commit format: `<type>(<scope>): <description>`
- Valid types: feat, fix, docs, style, refactor, test, chore, ci, perf, build
- Valid scopes: cli, git, data, claude, api

## Subject Line
- Subject line must be under 72 characters
- Use imperative mood ("add" not "added")

## Content
- Description must be specific, not vague
- Body required for changes over 100 lines

## Style
- Use lowercase for description
- No period at end of subject line

## Accuracy
- Commit type must match actual changes
- Scope must match files modified
- Description must reflect what was actually done
```

The "Severity Levels" section maps section names to severity levels.

### Default Guidelines

When no project guidelines exist, the command uses built-in defaults based on conventional commit format with sensible severity levels.

## Command Structure

```
omni-dev git commit message check [OPTIONS] [COMMIT_RANGE]
```

### Arguments

| Argument       | Description                                                                              |
|----------------|------------------------------------------------------------------------------------------|
| `COMMIT_RANGE` | Optional commit range (e.g., `HEAD~3..HEAD`). Defaults to commits ahead of main branch.  |

### Options

| Option                 | Description                                                    |
|------------------------|----------------------------------------------------------------|
| `--model <MODEL>`      | AI model to use for analysis                                   |
| `--context-dir <PATH>` | Custom context directory for guidelines                        |
| `--guidelines <PATH>`  | Explicit path to guidelines file                               |
| `--format <FORMAT>`    | Output format: `text` (default), `json`, `yaml`                |
| `--strict`             | Exit with error code if any issues found (including warnings)  |
| `--quiet`              | Only show errors/warnings, suppress info-level output          |
| `--verbose`            | Show detailed analysis including passing commits               |
| `--show-passing`       | Include passing commits in output (hidden by default)          |
| `--batch-size <N>`     | Number of commits to process per AI request (default: 4)       |
| `--no-suggestions`     | Skip generating corrected message suggestions                  |

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
    pub suggestion: Option<CommitSuggestion>,
    pub passes: bool,
}

pub struct CommitIssue {
    pub severity: IssueSeverity,
    pub guideline: String,       // Which guideline section was violated
    pub rule: String,            // Specific rule violated
    pub explanation: String,     // Why this is a violation
}

pub struct CommitSuggestion {
    pub suggested_message: String,
    pub explanation: String,     // Why this message is better
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
2. Receives project guidelines with severity annotations
3. Returns structured analysis of guideline violations

```rust
pub const CHECK_SYSTEM_PROMPT: &str = r#"You are a commit message reviewer.

You will receive:
1. Project commit guidelines (with severity annotations)
2. Commit information including the message and diff

## Severity Levels

The guidelines contain a "Severity Levels" section with a table mapping sections to severities:

```markdown
## Severity Levels

| Severity | Sections                       |
|----------|--------------------------------|
| error    | Format, Subject Line, Accuracy |
| warning  | Content                        |
| info     | Style                          |
```

Meaning:
- `error` = Violations block CI (exit code 1)
- `warning` = Advisory issues (exit code 0, or 2 with --strict)
- `info` = Suggestions only (never affect exit code)

Sections not listed default to `warning`.

## Your Task

For each commit:
1. Check if the message follows each guideline section
2. Compare the message against the actual diff to verify accuracy
3. Report violations with the severity from that section's annotation
4. Suggest a corrected message if there are issues

## Response Format

Respond with YAML only:

```yaml
checks:
  - commit: "abc123..."
    passes: false
    issues:
      - severity: error
        section: "Subject Line"
        rule: "Keep under 72 characters"
        explanation: "Subject is 85 characters"
      - severity: warning
        section: "Body Guidelines"
        rule: "Body required for large changes"
        explanation: "142 lines changed but no body provided"
    suggestion:
      message: |
        feat(api): add user endpoint

        Implement POST /api/users with validation.
      explanation: |
        - Shortened subject to under 72 chars
        - Added body explaining the change
```
"#;
```

The prompt teaches the AI to:
1. Parse the "Severity Levels" table from the guidelines
2. Map each violation to its section's severity level
3. Return structured YAML with explicit `severity: error|warning|info` fields

### Step 4: Implement Check Logic

**File:** [src/cli/git.rs](src/cli/git.rs)

The check flow:
1. Load project commit guidelines (from context directory, or use defaults)
2. Generate repository view with commits and diffs
3. Handle batching for large commit ranges (like twiddle)
4. Send to AI with check prompt
5. Parse YAML response and extract severity from each issue
6. Determine exit code based on severities
7. Display results with suggestions

### Step 5: Parse AI Response

The AI returns YAML with explicit severity levels:

```yaml
checks:
  - commit: "abc123..."
    issues:
      - severity: error      # <-- parsed directly from response
        section: "Accuracy"
        rule: "Type must match changes"
        explanation: "Used 'feat' but changes are a bug fix"
```

The command parses this YAML and maps the `severity` string to `IssueSeverity` enum:

```rust
fn parse_severity(s: &str) -> IssueSeverity {
    match s.to_lowercase().as_str() {
        "error" => IssueSeverity::Error,
        "warning" => IssueSeverity::Warning,
        "info" => IssueSeverity::Info,
        _ => IssueSeverity::Warning,  // default
    }
}
```

The AI determines severity by looking up the violated section in the guidelines' "Severity Levels" table. The command trusts the AI's severity assignment.

### Step 6: Error Handling

**No commits in range:**
```rust
if commits.is_empty() {
    eprintln!("error: no commits found in range");
    std::process::exit(3);
}
```

**AI response parsing with retry:**
```rust
const MAX_RETRIES: u32 = 2;

async fn check_with_retry(client: &ClaudeClient, prompt: &str) -> Result<CheckResponse> {
    let mut last_error = None;

    for attempt in 0..=MAX_RETRIES {
        match client.check_commits(prompt).await {
            Ok(response) => match parse_check_response(&response) {
                Ok(parsed) => return Ok(parsed),
                Err(e) => {
                    if attempt < MAX_RETRIES {
                        eprintln!("warning: failed to parse AI response (attempt {}), retrying...", attempt + 1);
                    }
                    last_error = Some(e);
                }
            },
            Err(e) => {
                if attempt < MAX_RETRIES {
                    eprintln!("warning: AI request failed (attempt {}), retrying...", attempt + 1);
                }
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap())
}
```

On final failure, exit with code 4.

### Step 7: Output Formatting

Implement formatters for text, JSON, and YAML output.

### Step 8: Exit Code Logic

```rust
fn determine_exit_code(report: &CheckReport, strict: bool) -> i32 {
    let has_errors = report.commits.iter()
        .any(|c| c.issues.iter().any(|i| i.severity == IssueSeverity::Error));
    let has_warnings = report.commits.iter()
        .any(|c| c.issues.iter().any(|i| i.severity == IssueSeverity::Warning));

    if has_errors {
        1
    } else if strict && has_warnings {
        2
    } else {
        0
    }
}
```

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
   ERROR   [Format] No conventional commit type found
   ERROR   [Content] Description "update stuff" is too vague

⚠️  def5678 - "feat(cli): Added new command for checking"
   WARNING [Content] Use imperative mood - "add" instead of "Added"

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Summary: 3 commits checked
  2 errors, 1 warning
  1 passed, 2 with issues
```

Note: Passing commits are hidden by default. Use `--show-passing` to include them:

```
Checking 3 commits...

❌ abc1234 - "update stuff"
   ...

⚠️  def5678 - "feat(cli): Added new command for checking"
   ...

✅ ghi9012 - "fix(git): resolve parsing error for merge commits"

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Summary: 3 commits checked
  2 errors, 1 warning
  1 passed, 2 with issues
```

### With Suggestions (Default)

```
Checking 2 commits...

⚠️  abc1234 - "feat(api): add endpoint"
   ERROR   [Accuracy] Message doesn't mention rate limiting logic added in
           src/api/handler.rs:45-67
   WARNING [Content] 127 lines changed across 4 files; body recommended for
           complex changes

   Suggested message:
   ┌────────────────────────────────────────────────────────────────────┐
   │ feat(api): add user registration endpoint with rate limiting       │
   │                                                                    │
   │ Implement POST /api/users endpoint with:                           │
   │ - Input validation for email and password                          │
   │ - Rate limiting (5 requests/minute per IP)                         │
   │ - Password hashing with bcrypt                                     │
   │                                                                    │
   | Closes #123                                                       |
   └────────────────────────────────────────────────────────────────────┘

   Why this is better:
   - Mentions rate limiting which is a significant feature in the diff
   - Includes body explaining the implementation details
   - Lists the key components added
```

### CI Mode (--quiet)

```
abc1234: ERROR [Accuracy] missing rate limiting mention
abc1234: WARNING [Content] body recommended for 127-line change
def5678: OK
```

---

## Exit Codes

| Code | Meaning                                                   |
|------|-----------------------------------------------------------|
| 0    | All commits pass (no errors, possibly warnings)           |
| 1    | One or more commits have errors                           |
| 2    | One or more commits have warnings (only with `--strict`)  |
| 3    | No commits found in range                                 |
| 4    | AI response parse error (after retries exhausted)         |

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

## Design Decisions

| Decision              | Choice                  | Rationale                                        |
|-----------------------|-------------------------|--------------------------------------------------|
| Missing guidelines    | Use built-in defaults   | Projects without guidelines still get value      |
| Severity levels       | Defined in guidelines   | Projects control what's blocking vs advisory     |
| Suggestions           | Included by default     | Helps developers fix issues immediately          |
| Interactive mode      | None (use twiddle)      | Check is for CI; twiddle is for interactive use  |
| Batching              | Yes (like twiddle)      | Handle large commit ranges efficiently           |
| Caching               | No                      | Keep implementation simple                       |
| Twiddle integration   | None                    | Commands remain independent                      |
| AI parse errors       | Retry up to 2 times     | Improve reliability without infinite loops       |
| No commits in range   | Exit code 3             | Distinct error for empty input                   |
| Passing commits       | Hidden by default       | Focus output on issues; use --show-passing       |
| Cost awareness        | None                    | Keep UX simple; users manage their own API usage |

## Difference from Twiddle

| Aspect          | `check`                    | `twiddle`              |
|-----------------|----------------------------|------------------------|
| Purpose         | Validate and report        | Fix and amend          |
| Interactive     | No                         | Yes                    |
| Modifies commits| No                         | Yes                    |
| Exit codes      | Yes (for CI)               | No                     |
| Suggestions     | Shows but doesn't apply    | Applies amendments     |
| Use case        | CI pipelines, pre-push hooks | Developer workflow   |

## Future Enhancements

1. **GitHub Actions output** - `--format github` for annotations
2. **Pre-commit/pre-push hooks** - Integration with git hooks
3. **Ignore patterns** - Skip certain commits or rules
4. **Watch mode** - Re-check on commit
