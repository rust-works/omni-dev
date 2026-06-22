# omni-dev User Guide

A comprehensive guide to using omni-dev's AI-powered commit message
intelligence.

## Table of Contents

1. [Your First Improvement](#your-first-improvement)
2. [Getting Started](#getting-started)
3. [Core Concepts](#core-concepts)
4. [Command Reference](#command-reference)
5. [Claude Code Integration](#claude-code-integration)
6. [Atlassian Integration](#atlassian---jira-and-confluence-integration)
7. [Datadog Integration](#datadog-integration)
8. [Contextual Intelligence](#contextual-intelligence)
9. [Workflows](#workflows)
10. [Advanced Usage](#advanced-usage)
11. [Best Practices](#best-practices)

## Your First Improvement

The fastest way to learn omni-dev is to run it on a throwaway commit you
control end to end. This tutorial takes about 5 minutes in any git repo
and walks through the three core commands — `view`, `twiddle`, `check`.

First-time setup (install + auth + `.omni-dev/`) is covered in
[Getting Started](getting-started.md). This tutorial assumes you've done
that already.

### Step 1 — Make a deliberately bad commit on a scratch branch

```bash
git checkout -b omni-dev-tutorial
echo "" >> README.md
git add README.md
git commit -m "wip"
```

### Step 2 — Inspect it with `view`

```bash
omni-dev git commit message view 'HEAD~1..HEAD'
```

Expected: YAML output describing the commit, its diff, and the
field-presence summary. Skim it — you don't have to read it all.

### Step 3 — Improve it with `twiddle`

```bash
omni-dev git commit message twiddle 'HEAD~1..HEAD'
```

Expected: omni-dev prints a suggested rewritten message (something like
`docs(readme): add trailing newline`), shows a before/after diff, and
prompts `Apply these amendments? [y/N]`. Press `y`.

### Step 4 — Verify with `git log`

```bash
git log --oneline HEAD~1..HEAD
```

Expected: the subject line is now the AI-suggested message.

### Step 5 — Validate against your guidelines with `check`

```bash
omni-dev git commit message check 'HEAD~1..HEAD'
```

Expected: the check passes (exit 0). If you have project-specific scopes
in `.omni-dev/scopes.yaml` that the suggestion didn't use, re-run
`twiddle` — see the [Configuration Guide](configuration.md) to teach
omni-dev about your scopes.

### Cleanup

```bash
git checkout - && git branch -D omni-dev-tutorial
```

### What just happened

You ran the three core commands — `view` (analyse), `twiddle` (improve),
`check` (validate) — that together cover the full omni-dev workflow.
Everything else in this guide builds on these three.

## Getting Started

### Prerequisites

1. **Install omni-dev**

   ```bash
   cargo install omni-dev
   ```

2. **Authenticate.** By default, `export CLAUDE_API_KEY="sk-ant-..."`.
   See [Authentication](configuration.md#authentication) for the full
   reference (alternative env-var names, `.env` files, CI/CD secrets,
   non-Anthropic backends).

3. **Verify Installation**

   ```bash
   omni-dev --version
   omni-dev help-all  # See all available commands
   ```

### First Use

Transform your commit messages and create professional PRs in 4 steps:

```bash
# 1. Navigate to your git repository
cd your-project

# 2. Improve recent commits with AI intelligence
omni-dev git commit message twiddle 'HEAD~5..HEAD' --use-context

# 3. Review and apply the suggestions
# The tool will show you before/after and ask for confirmation

# 4. Create a professional PR with AI-generated description
omni-dev git branch create pr
# Analyzes your commits and generates comprehensive PR description
```

## Core Concepts

### The Four-Command Workflow

omni-dev follows a simple analyze → improve → apply → ship workflow:

```bash
# 📊 ANALYZE: See detailed commit information
omni-dev git commit message view 'HEAD~3..HEAD'

# 🤖 IMPROVE: Get AI-powered suggestions
omni-dev git commit message twiddle 'HEAD~3..HEAD' --use-context

# ✏️ APPLY: Apply specific amendments manually
omni-dev git commit message amend amendments.yaml

# 🚀 SHIP: Create professional PR with AI description
omni-dev git branch create pr
```

### Key Benefits

- **Contextual**: Understands your project structure and conventions
- **Safe**: Always asks for confirmation before making changes
- **Intelligent**: Uses actual code changes, not just file names
- **Batch-Aware**: Handles large commit ranges efficiently
- **Professional**: Generates conventional commit format

## Command Reference

### `twiddle` - AI-Powered Improvement

The main command for improving commit messages:

```bash
# Basic usage with contextual intelligence
omni-dev git commit message twiddle 'origin/main..HEAD' --use-context

# Common options
omni-dev git commit message twiddle [RANGE] [OPTIONS]
```

**Key Options:**

| Option | Description | Example |
|--------|-------------|---------|
| `--use-context` | Enable AI contextual intelligence | `--use-context` |
| `--concurrency N` | Number of parallel commit processors (default: 4) | `--concurrency 2` |
| `--no-coherence` | Skip cross-commit coherence refinement pass | `--no-coherence` |
| `--auto-apply` | Apply changes without confirmation | `--auto-apply` |
| `--save-only FILE` | Save suggestions to file instead of applying | `--save-only suggestions.yaml` |
| `--context-dir PATH` | Custom context directory | `--context-dir ./config` |
| `--no-context` | Disable contextual features | `--no-context` |

**Commit Range Examples:**

```bash
# Last 5 commits
omni-dev git commit message twiddle 'HEAD~5..HEAD'

# All commits on current branch vs main
omni-dev git commit message twiddle 'origin/main..HEAD'

# Specific range between commits
omni-dev git commit message twiddle 'abc123..def456'

# Single commit
omni-dev git commit message twiddle 'HEAD^..HEAD'
```

### `view` - Analysis and Inspection

Analyze commits without making changes:

```bash
# Analyze recent commits (YAML output)
omni-dev git commit message view 'HEAD~3..HEAD'

# Analyze current branch vs main
omni-dev git branch info main
```

The output includes:

- Commit metadata (hash, author, date, message)
- File changes and diff statistics
- Conventional commit type detection
- Proposed improvements
- Remote branch tracking

### `amend` - Manual Application

Apply specific amendments from a YAML file:

```bash
# Apply amendments from file
omni-dev git commit message amend amendments.yaml
```

YAML format:

```yaml
amendments:
  - commit: "abc123def456..."
    message: |
      feat(auth): implement OAuth2 authentication
      
      Add comprehensive authentication system:
      - OAuth2 integration with Google/GitHub
      - JWT token management
      - User session handling
      - Role-based access control
```

### `check` - Commit Message Validation

Validate commit messages against guidelines without modifying anything.
Useful in CI, pre-push hooks, and as a non-destructive sibling to `twiddle`.

```bash
# Default range: commits ahead of main
omni-dev git commit message check

# Explicit range
omni-dev git commit message check 'HEAD~5..HEAD'

# CI-friendly: exit non-zero on any issue (warnings included)
omni-dev git commit message check --strict

# Quiet output (errors/warnings only)
omni-dev git commit message check --quiet

# Show analysis for passing commits too
omni-dev git commit message check --verbose
omni-dev git commit message check --show-passing

# Structured output for tooling
omni-dev git commit message check --format json
omni-dev git commit message check --format yaml

# Offer to apply suggested fixes when issues are found
omni-dev git commit message check --twiddle
```

**Key Options:**

| Option | Description |
|--------|-------------|
| `--strict` | Exit non-zero if any issue is reported (including warnings) |
| `--quiet` | Suppress info-level output |
| `--verbose` | Include detailed analysis for every commit |
| `--show-passing` | Include passing commits in the report |
| `--format text\|json\|yaml` | Output format (default `text`) |
| `--no-coherence` | Skip the cross-commit coherence pass |
| `--no-suggestions` | Skip generating corrected message suggestions |
| `--twiddle` | When issues are found, prompt to apply suggested fixes |
| `--guidelines PATH` | Use a guidelines file outside `.omni-dev/` |
| `--context-dir PATH` | Custom context directory |
| `--concurrency N` | Maximum concurrent AI requests (default 4) |
| `--model MODEL` / `--beta-header KEY:VALUE` | Override the Claude model and beta headers |

### `create pr` - AI-Powered Pull Request Creation

Generate professional pull requests with AI-analyzed descriptions:

```bash
# Create PR with AI-generated description
omni-dev git branch create pr

# Create PR for specific base branch
omni-dev git branch create pr main

# Common options
omni-dev git branch create pr [BASE_BRANCH] [OPTIONS]
```

**Key Options:**

| Option | Description | Example |
|--------|-------------|---------|
| `--base BRANCH` | Base branch (defaults to `main` / `master`) | `--base release/2.x` |
| `--ready` | Force the PR to open as ready-for-review | `--ready` |
| `--draft` | Force the PR to open as a draft | `--draft` |
| `--no-push` | Skip the implicit `git push` before creating the PR | `--no-push` |
| `--model MODEL` | Override the Claude model used to draft the PR body | `--model claude-opus-4-7` |
| `--context-dir PATH` | Custom context directory (defaults to `.omni-dev/`) | `--context-dir ./config` |
| `--auto-apply` | Create/update PR without confirmation | `--auto-apply` |
| `--save-only FILE` | Save PR details to YAML file instead of creating | `--save-only pr-details.yaml` |
| `--from-commits` | Drive PR generation from commit messages instead of the diff (faster, no diff bytes are sent to the AI) | `--from-commits` |

**What it does:**

- Analyzes your branch commits and changes
- Generates comprehensive PR title and description using AI
- Fills in PR template sections automatically
- Handles both new PR creation and existing PR updates
- Creates YAML file with structured PR details for editing

**Requirements:**

- Clean working directory (no uncommitted changes)
- GitHub CLI (`gh`) installed and authenticated
- Branch pushed to remote (will push automatically if needed)
- Claude API key configured

**Example Output:**

The command creates a `pr-details.yaml` file with structure like:

```yaml
title: "feat(auth): implement comprehensive OAuth2 authentication system"
description: |
  # Pull Request

  ## Description
  This PR implements a comprehensive OAuth2 authentication system that enables
  users to sign in using Google and GitHub providers. The implementation includes
  secure token management, session handling, and role-based access control.

  ## Type of Change
  - [x] New feature (non-breaking change which adds functionality)

  ## Changes Made
  - Added OAuth2 integration with Google and GitHub providers
  - Implemented JWT token validation and refresh mechanisms
  - Created user session management system
  - Added role-based access control middleware
  - Updated authentication documentation

  ## Testing
  - [x] All existing tests pass
  - [x] New tests added for authentication flows
  - [x] Manual testing performed with both providers

  ## Additional Notes
  This implementation follows OAuth2 best practices and includes comprehensive
  error handling for edge cases.
```

## Claude Code Integration

omni-dev ships a family of subcommands that integrate with [Claude
Code](https://docs.anthropic.com/claude/docs/claude-code) — Anthropic's
agentic coding CLI. These cover four use cases:

- **Conversational AI** (`ai chat`) — one-shot Q&A against the configured model.
- **Conversation history export** (`ai claude history sync`) — mirror Claude
  Code sessions to disk for analysis or coaching workflows.
- **Skill distribution** (`ai claude skills`) — share `.claude/skills/`
  definitions across repositories and worktrees.
- **Command-template generation** (`commands generate`) — bootstrap canonical
  slash-commands into a project's `.claude/commands/` directory.

All of these honour the same AI backend dispatch as the rest of omni-dev
(see [Configuration Guide — AI Backend Selection](configuration.md#ai-backend-selection)).

### `ai chat` — Conversational AI

A lightweight CLI front-end for chatting with the configured Claude model.
Use it when you want open-ended Q&A — `twiddle` is for commit-message
amendments, `check` is for commit-message validation, `view` is for
analysis, and `ai chat` is for everything else (a sanity check on a
config, a rubber-duck on an error message, a quick "summarise this
output").

Each prompt is **single-turn from the model's perspective** — the CLI
wraps an interactive loop around independent calls and does not pass prior
turns back. The interactive UX feels like multi-turn but the model sees
each prompt fresh. If you need true multi-turn reasoning with full
context, use Claude Code itself.

The CLI uses a hard-coded system prompt of "You are a helpful assistant.";
MCP callers can override it via the `system_prompt` parameter (see the
[`ai_chat`](mcp.md#ai--config-5-tools) tool entry).

```bash
# Start an interactive session against the configured backend
omni-dev ai chat

# Override the model for this session only
omni-dev ai chat --model claude-opus-4-7
```

The session reads stdin one line at a time and prints the response. Type
`Ctrl+D` (EOF) to exit.

### `ai claude history sync` — Export Conversation History

Export Claude Code conversation history to a target directory as one
`.jsonl` (and optionally `.md`) per chat, grouped by encoded project slug.
Re-running is idempotent: unchanged sessions are skipped, modified sessions
are rewritten via tempfile + rename, and source `mtime` is preserved on the
target so downstream tooling can sort chronologically.

```bash
# Basic export to ~/coaching/claude-history
omni-dev ai claude history sync --target ~/coaching/claude-history

# Both formats side-by-side (markdown is LLM-friendly with YAML frontmatter)
omni-dev ai claude history sync --target ~/history --output-format jsonl,markdown

# Restrict to one project (encoded slug or decoded cwd path)
omni-dev ai claude history sync --target ~/history --project -Users-jky-wrk

# Window: relative duration or RFC 3339
omni-dev ai claude history sync --target ~/history --since 7d
omni-dev ai claude history sync --target ~/history --since 2026-04-01T00:00:00Z

# Hide system-side events from markdown (jsonl is byte-identical regardless)
omni-dev ai claude history sync --target ~/history --output-format markdown --exclude-system

# Preview without touching the target
omni-dev ai claude history sync --target ~/history --dry-run

# Delete target files for sessions removed upstream (scoped to listed formats)
omni-dev ai claude history sync --target ~/history --prune
```

The export is a behavioural transcript — prompts, responses, thinking,
tool calls, tool-result metadata, and structured agent-to-user interactions
(`AskUserQuestion`, denials, interrupts). Sub-agent internal turns, large
tool-output sidecars, and auto-memory are deliberately excluded. See
`omni-dev ai claude history sync --help` for the complete flag reference.

### `ai claude skills` — Distribute Skills Across Repositories

A [Claude Code skill](https://docs.anthropic.com/claude/docs/claude-code-skills)
is a directory under `.claude/skills/<name>/` containing a `SKILL.md`
manifest plus any supporting files. Claude Code auto-discovers skills from
the working directory, so the canonical pattern is to keep them in a
project repository.

When the same skill set needs to be available across multiple repositories
or worktrees, copying becomes a maintenance burden. `ai claude skills`
solves this by **symlinking** each `<target>/.claude/skills/<name>` to
`<source>/.claude/skills/<name>` and adding a managed block to
`<target>/.git/info/exclude` so git ignores the symlinks. Updates in the
source are seen immediately by every target; `clean` removes both the
symlinks and the exclude-block entries; `status` reports residue.

```bash
# Sync skills from the current repo into itself and all its worktrees
omni-dev ai claude skills sync --worktrees

# Sync from a canonical source into a specific target
omni-dev ai claude skills sync --source ~/wrk/canonical --target ~/wrk/feature-branch

# Preview what would change
omni-dev ai claude skills sync --source ~/wrk/canonical --target ~/wrk/feature-branch --dry-run

# Inspect symlinks and exclude entries left behind by a prior sync
omni-dev ai claude skills status
omni-dev ai claude skills status --worktrees --format yaml

# Remove the symlinks and exclude-block entries
omni-dev ai claude skills clean --worktrees
omni-dev ai claude skills clean --dry-run
```

**End-to-end walkthrough**:

```bash
# 1. Add a new skill in your canonical repo
mkdir -p ~/wrk/canonical/.claude/skills/my-skill
$EDITOR ~/wrk/canonical/.claude/skills/my-skill/SKILL.md

# 2. Push it into every worktree of a downstream repo
cd ~/wrk/downstream
omni-dev ai claude skills sync --source ~/wrk/canonical --worktrees

# 3. Verify Claude Code picks it up (the symlink appears in the listing)
omni-dev ai claude skills status

# 4. Later, when you no longer want this skill set, clean up
omni-dev ai claude skills clean --worktrees
```

Same source and target is a no-op (the command short-circuits). The
target's `.git/info/exclude` is the only file modified outside
`.claude/skills/`, and the managed block is delimited so manual entries
are preserved across syncs and cleans.

See also: [`claude_skills_sync` / `claude_skills_status` / `claude_skills_clean`](mcp.md#ai--config-5-tools)
for the MCP equivalents.

### `ai claude cli model resolve` — Model Resolution Diagnostics

Print how Claude Code resolves the active model in the current directory
(useful when project / user / env settings disagree).

```bash
omni-dev ai claude cli model resolve
```

### `commands generate` — Generate Claude Code Slash-Commands

Generates [Claude Code slash-command](https://docs.anthropic.com/claude/docs/claude-code-slash-commands)
templates into the project's `.claude/commands/` directory. Each template
is a self-contained workflow manifest (YAML frontmatter declaring allowed
tools, argument hints, and model selection, followed by step-by-step
instructions) that drives a multi-step omni-dev operation from inside a
Claude Code session.

Three templates ship with omni-dev:

| Subcommand | Output file | Purpose |
|------------|-------------|---------|
| `commit-twiddle` | `.claude/commands/commit-twiddle.md` | Invoke `omni-dev twiddle`, review the suggested amendments, apply them. |
| `pr-create` | `.claude/commands/pr-create.md` | Run the `view` → `twiddle` → `branch create pr` pipeline end-to-end. |
| `pr-update` | `.claude/commands/pr-update.md` | Update an existing PR's body from the current commit set. |
| `all` | All three of the above | Bootstrap a fresh project. |

```bash
# Bootstrap all three templates
omni-dev commands generate all

# Or generate them individually
omni-dev commands generate commit-twiddle
omni-dev commands generate pr-create
omni-dev commands generate pr-update
```

Run from the repository root; the command will create `.claude/commands/`
if it does not exist and writes one file per subcommand (e.g. `✅ Generated
.claude/commands/commit-twiddle.md`). Commit the files to share the
workflows with your team — Claude Code picks them up automatically, so
collaborators can invoke `/commit-twiddle`, `/pr-create`, or `/pr-update`
inside a Claude Code session with no extra setup.

### `atlassian` - JIRA and Confluence Integration

Read, edit, and manage JIRA issues and Confluence pages from the command line.
Content is represented as JFM (JIRA-Flavored Markdown) with YAML frontmatter,
enabling round-trip editing between your editor and Atlassian Cloud.

See the [JFM Specification](specs/jfm.md) for full technical details on the
markdown format.

#### Authentication Setup

Configure your Atlassian Cloud credentials:

```bash
# Interactive credential setup (prompts for instance URL, email, API token)
omni-dev atlassian auth login

# Verify credentials work
omni-dev atlassian auth status
```

Credentials are stored in `~/.omni-dev/settings.json`. You can also use
environment variables:

```bash
export ATLASSIAN_INSTANCE_URL=https://myorg.atlassian.net
export ATLASSIAN_EMAIL=you@example.com
export ATLASSIAN_API_TOKEN=your-token
```

Environment variables take precedence over the settings file.

#### Destructive Commands

> **⚠️ Destructive commands require confirmation.**
>
> Five Atlassian subcommands prompt for confirmation by default and refuse
> to run unless either the user explicitly confirms (CLI) or the caller
> opts in (MCP):
>
> - `omni-dev atlassian jira delete <KEY>`
> - `omni-dev atlassian jira link remove --link-id <ID>`
> - `omni-dev atlassian jira watcher remove --user <ACCOUNT_ID> <KEY>`
> - `omni-dev atlassian confluence delete <ID>`
> - `omni-dev atlassian confluence label remove --labels <LABELS> <ID>`
>
> **CLI behaviour.** Each command prompts on stdin:
>
> ```text
> Delete PROJ-123 (Fix login)? [y/N]
> ```
>
> Typing anything other than `y` (case-insensitive, whitespace-trimmed)
> prints `Cancelled.` and exits without calling the API. Two escape
> hatches:
>
> - `--force` skips the prompt — for scripts.
> - `--dry-run` prints `Would delete PROJ-123 (Fix login).` and exits without
>   calling the API. `--dry-run` takes precedence over `--force`, so a
>   scripted `--force` invocation can be sanity-checked by adding
>   `--dry-run` without removing the force flag.
>
> **MCP behaviour.** The matching MCP tools (`jira_delete`,
> `jira_link_remove`, `jira_watcher_remove`, `confluence_delete`,
> `confluence_label_remove`) require an explicit `confirm: true` parameter
> and refuse to run otherwise:
>
> ```text
> Refusing to delete PROJ-123: pass `confirm: true` to authorise this irreversible operation.
> ```
>
> Interactive prompts catch human accidents; `--force` keeps scripts
> working; `--dry-run` is a server-free preview; the MCP `confirm: true`
> requirement gives assistants an explicit opt-in. See
> [ADR-0027](adrs/adr-0027.md) for the full design rationale.

#### JIRA: Reading and Writing Issues

```bash
# Read an issue as JFM markdown
omni-dev atlassian jira read PROJ-123
omni-dev atlassian jira read PROJ-123 -o issue.md
omni-dev atlassian jira read PROJ-123 --format adf   # raw ADF JSON

# Include specific custom fields
omni-dev atlassian jira read PROJ-123 --fields "Acceptance Criteria,customfield_19300"

# Include every populated custom field
omni-dev atlassian jira read PROJ-123 --all-fields

# Write changes back (prompts for confirmation)
omni-dev atlassian jira write PROJ-123 issue.md
omni-dev atlassian jira write PROJ-123 issue.md --force
omni-dev atlassian jira write PROJ-123 issue.md --dry-run

# Update fields without re-posting the description body
omni-dev atlassian jira write PROJ-123 --no-content --assignee 5b10a2844c20165700ede21g
omni-dev atlassian jira write PROJ-123 --no-content --parent EPIC-1
omni-dev atlassian jira write PROJ-123 --no-content --reporter "" --set-field "Priority=High"

# Interactive edit: fetch -> $EDITOR -> push
omni-dev atlassian jira edit PROJ-123
```

`--assignee` / `--reporter` take an Atlassian `accountId` — pass the empty
string `""` to clear, or `"-1"` to trigger automatic assignment. Use
[`jira user search`](#jira-user-search) to resolve a display name or email
to an `accountId`. `--parent` sets JIRA's system parent field (Epic → Story
or Story → Sub-task); it is distinct from "Composition" links created via
[`jira link`](#jira-issue-links).

The edit command opens an interactive loop:
1. Fetches the issue and writes JFM to a temp file
2. Prompts: `[A]ccept, [S]how, [E]dit, or [Q]uit?`
3. On accept, converts back to ADF and pushes changes

JFM output example:

```markdown
---
type: jira
instance: https://myorg.atlassian.net
key: PROJ-123
summary: Implement user authentication
status: In Progress
issue_type: Story
assignee: Alice Smith
labels:
  - backend
---

This story covers the implementation of OAuth2-based authentication...
```

#### JIRA: Search

Search issues using JQL or convenience flags:

```bash
# Raw JQL
omni-dev atlassian jira search --jql "project = PROJ AND status = Open"

# Convenience flags (combined with AND)
omni-dev atlassian jira search --project PROJ --status "In Progress"
omni-dev atlassian jira search --assignee alice --limit 100

# Fetch all results (auto-paginates)
omni-dev atlassian jira search --jql "project = PROJ" --limit 0
```

Output is a formatted table: `KEY | STATUS | ASSIGNEE | SUMMARY`.

#### JIRA: Create Issues

Create issues from JFM markdown or CLI flags:

```bash
# From JFM file (project, type, summary from frontmatter)
omni-dev atlassian jira create issue.md

# From CLI flags
omni-dev atlassian jira create issue.md --project PROJ --type Bug --summary "Fix login"

# From ADF JSON (all metadata via flags)
omni-dev atlassian jira create body.json --format adf --project PROJ --summary "Title"

# Set custom fields inline (repeatable)
omni-dev atlassian jira create issue.md --set-field "Story Points=5" \
  --set-field "Sprint=customfield_10020"

# Preview without creating
omni-dev atlassian jira create issue.md --dry-run
```

Prints the created issue key (e.g., `PROJ-124`) to stdout. `--set-field`
values are parsed as YAML scalars (numbers, bools) when possible, falling
back to strings; entries override the frontmatter `custom_fields:` map for
the same name.

#### JIRA: Transitions

List and execute workflow transitions:

```bash
# List available transitions
omni-dev atlassian jira transition PROJ-123

# Execute a transition by name (case-insensitive)
omni-dev atlassian jira transition PROJ-123 "In Progress"

# Execute by ID
omni-dev atlassian jira transition PROJ-123 21
```

#### JIRA: Comments

```bash
# List comments on an issue
omni-dev atlassian jira comment list PROJ-123

# Add a comment from a file
omni-dev atlassian jira comment add PROJ-123 comment.md

# Add from stdin
echo "This is a comment" | omni-dev atlassian jira comment add PROJ-123

# Add ADF JSON comment
omni-dev atlassian jira comment add PROJ-123 body.json --format adf
```

#### JIRA: Delete Issues

```bash
# Delete with confirmation prompt
omni-dev atlassian jira delete PROJ-123

# Skip confirmation
omni-dev atlassian jira delete PROJ-123 --force
```

#### JIRA: Projects

```bash
# List all accessible projects
omni-dev atlassian jira project list
omni-dev atlassian jira project list --limit 100
```

#### JIRA: Fields

JIRA installations are heavily customised — most real projects add custom
fields (story points, epic links, severity, customer impact, etc.) and
each gets an opaque ID like `customfield_10001`. The names you see in the
JIRA web UI are display labels; the API only accepts the IDs. The `field`
subcommand is how you discover them.

A field can have different option lists per **context** (per-project,
per-issue-type, or per-screen scoping). Most fields have exactly one
context, in which case `field options` will auto-discover it. Fields with
multiple contexts require `--context-id` to pick the right option set.

```bash
# List all field definitions (display name → customfield_NNNNN mapping)
omni-dev atlassian jira field list

# Search by name (case-insensitive substring match)
omni-dev atlassian jira field list --search "story"
omni-dev atlassian jira field list --search "severity"

# Show options for a custom field (auto-discovers context)
omni-dev atlassian jira field options --field-id customfield_10001

# Specify context explicitly when the field has multiple contexts
omni-dev atlassian jira field options --field-id customfield_10001 --context-id 12345

# Machine-readable output for scripting
omni-dev atlassian jira field list --search "epic" -o yaml
omni-dev atlassian jira field options --field-id customfield_10001 -o json
```

Output formats: `table` (default, human-readable), `json`, `yaml`,
`yamls` (YAML stream), and `jsonl` (JSON Lines).

**End-to-end walkthrough** — set a custom field on a new issue:

```bash
# 1. Find the field by name
omni-dev atlassian jira field list --search "story points"
# → customfield_10016: "Story Points"

# 2. (Number fields have no options — skip step 3.)
#    For an enum-style field, list its allowed values:
omni-dev atlassian jira field list --search "severity"
# → customfield_10042: "Severity"
omni-dev atlassian jira field options --field-id customfield_10042
# → "Low", "Medium", "High", "Critical"

# 3. Pass the field ID and value when creating or writing the issue
#    (see `jira create` / `jira write` for the syntax).
```

See also: [`jira_field_list` / `jira_field_options`](mcp.md#jira--extensions-18-tools)
for the MCP equivalents (`search` and `field_id` parameters mirror the CLI
flags).

#### JIRA: Agile Boards

```bash
# List boards
omni-dev atlassian jira board list
omni-dev atlassian jira board list --project PROJ --type scrum

# List issues on a board
omni-dev atlassian jira board issues --board-id 1
omni-dev atlassian jira board issues --board-id 1 --jql "status = Open"
```

#### JIRA: Sprints

```bash
# List sprints for a board
omni-dev atlassian jira sprint list --board-id 1
omni-dev atlassian jira sprint list --board-id 1 --state active

# List issues in a sprint
omni-dev atlassian jira sprint issues --sprint-id 10
omni-dev atlassian jira sprint issues --sprint-id 10 --jql "status = Open"

# Add issues to a sprint
omni-dev atlassian jira sprint add --sprint-id 10 --issues PROJ-1,PROJ-2,PROJ-3

# Create a new sprint (start/end dates and goal optional)
omni-dev atlassian jira sprint create --board-id 1 --name "Sprint 42" \
  --start-date 2026-05-01 --end-date 2026-05-14 --goal "Ship checkout v2"

# Update an existing sprint (only supplied fields change)
omni-dev atlassian jira sprint update --sprint-id 10 --state active
omni-dev atlassian jira sprint update --sprint-id 10 --name "Sprint 42 (extended)" \
  --end-date 2026-05-21
```

#### JIRA: Watchers

```bash
# List watchers
omni-dev atlassian jira watcher list PROJ-123

# Add or remove a watcher (account ID — use `jira user search` to resolve)
omni-dev atlassian jira watcher add PROJ-123 --user 5b10a2844c20165700ede21g
omni-dev atlassian jira watcher remove PROJ-123 --user 5b10a2844c20165700ede21g
```

#### JIRA: Worklogs

```bash
# List worklog entries
omni-dev atlassian jira worklog list PROJ-123
omni-dev atlassian jira worklog list PROJ-123 --limit 100

# Log time (`--time-spent` accepts JIRA duration format: "2h 30m", "1d", "45m")
omni-dev atlassian jira worklog add PROJ-123 --time-spent "2h 30m" \
  --comment "Investigated cache invalidation"
omni-dev atlassian jira worklog add PROJ-123 --time-spent 1d \
  --started "2026-04-16T09:00:00.000+0000"
```

#### JIRA: User Search

Resolve a display name or email substring to an Atlassian `accountId` —
required input for `jira write --assignee/--reporter` and `jira watcher
add/remove`.

```bash
omni-dev atlassian jira user search --query "Alice"
omni-dev atlassian jira user search --query "@example.com" --limit 100
```

#### JIRA: Development Info

Show linked PRs, branches, and repositories for an issue (requires JIRA's
GitHub/Bitbucket integration).

```bash
omni-dev atlassian jira dev PROJ-123
omni-dev atlassian jira dev PROJ-123 --type pullrequest
omni-dev atlassian jira dev PROJ-123 --app GitHub --summary
```

#### JIRA: Issue Links

```bash
# List links on an issue (shows link IDs)
omni-dev atlassian jira link list PROJ-123

# List available link types
omni-dev atlassian jira link types

# Create a link
omni-dev atlassian jira link create --type Blocks --inward PROJ-1 --outward PROJ-2

# Remove a link by ID (get IDs from `link list`)
omni-dev atlassian jira link remove --link-id 12345

# Link an issue to an epic
omni-dev atlassian jira link epic --epic EPIC-1 --issue PROJ-2
```

#### JIRA: Changelog

View change history for one or more issues:

```bash
omni-dev atlassian jira changelog --keys PROJ-1
omni-dev atlassian jira changelog --keys PROJ-1,PROJ-2 --limit 100
```

#### JIRA: Attachments

```bash
# Download all attachments
omni-dev atlassian jira attachment download --key PROJ-123
omni-dev atlassian jira attachment download --key PROJ-123 --output-dir ./files

# Filter by filename
omni-dev atlassian jira attachment download --key PROJ-123 --filter screenshot

# Download only images (png, jpeg, gif, svg, webp)
omni-dev atlassian jira attachment images --key PROJ-123
omni-dev atlassian jira attachment images --key PROJ-123 --output-dir ./images
```

#### Confluence: Reading and Writing Pages

```bash
# Read a page as JFM markdown
omni-dev atlassian confluence read 12345
omni-dev atlassian confluence read 12345 -o page.md
omni-dev atlassian confluence read 12345 --format adf

# Write changes back
omni-dev atlassian confluence write 12345 page.md
omni-dev atlassian confluence write 12345 page.md --force
omni-dev atlassian confluence write 12345 page.md --dry-run

# Interactive edit
omni-dev atlassian confluence edit 12345
```

Confluence JFM output example:

```markdown
---
type: confluence
instance: https://myorg.atlassian.net
page_id: "12345"
title: Architecture Overview
space_key: ENG
status: current
version: 7
---

# Architecture Overview

Page body content here...
```

#### Confluence: Comparing Pages

Compares two versions of a Confluence page using a **structurally-aware
diff**: the engine walks the ADF (Atlassian Document Format) tree and
splits each version into heading-delimited sections (paths like
`/h2#background`, `/h3#implementation`). The output describes *which
sections* changed and *how* — not raw text deltas — which makes it
ergonomic for AI agents and human reviewers alike.

Two commands:

- `omni-dev atlassian confluence compare run <PAGE_ID>` — diff two versions
  of a page; emits a YAML envelope with per-section change summaries and
  drill-in cursors.
- `omni-dev atlassian confluence compare section --cursor <CURSOR>` —
  drill into a single section using a cursor returned by `run`.

**Detail levels** (`--detail`):

- `summary` — aggregate counts only (sections added, modified, removed;
  characters changed). Smallest output.
- `outline` (default) — per-section change kind, one-line summaries, and
  drill-in cursors for `compare section`. Ideal balance for surveying a
  page.
- `full` — embeds full per-section deltas. Budget-truncated if the output
  exceeds `--budget` (default ~16 KiB ≈ 4000 tokens).

**Version selectors** for `--from` and `--to`:

- `latest` — the most recent version (default `--to`).
- `previous` — the version before `--to` (default `--from`).
- `v-N` — `N` versions back from `--to` (e.g. `v-3`).
- A bare integer — that exact version number.
- An ISO 8601 timestamp — the version that was current at that time.

**Filtering and trimming**:

- `--filter-section /h2#name` — restrict to sections matching the given
  path. Repeatable.
- `--min-change-chars <N>` — drop sections with fewer than `N` characters
  of changed text. Useful for ignoring formatting-only edits.
- `--ignore-whitespace` — collapse runs of whitespace inside text nodes
  before diffing.
- `--include body,title,labels,metadata` — choose which top-level fields
  to diff. Default: `body,title,metadata` (labels and other metadata
  excluded by default).
- `--budget <BYTES>` — output budget. Defaults to `16384` (~16 KiB).

```bash
# Outline of changes between the previous and latest versions
omni-dev atlassian confluence compare run 12345

# Compare a specific version range
omni-dev atlassian confluence compare run 12345 --from v-5 --to latest

# Compare by date (ISO 8601)
omni-dev atlassian confluence compare run 12345 \
    --from 2026-01-01T00:00:00Z --to 2026-05-11T00:00:00Z

# Just the totals
omni-dev atlassian confluence compare run 12345 --detail summary

# Full deltas, larger budget
omni-dev atlassian confluence compare run 12345 --detail full --budget 65536

# Restrict to specific sections and ignore whitespace
omni-dev atlassian confluence compare run 12345 \
    --filter-section /h2#background --filter-section /h2#design \
    --ignore-whitespace

# Drill into a single section using a cursor returned by `run`
omni-dev atlassian confluence compare section --cursor <CURSOR> --format unified
omni-dev atlassian confluence compare section --cursor <CURSOR> --format side-by-side
omni-dev atlassian confluence compare section --cursor <CURSOR> --format markdown-inline
```

**End-to-end walkthrough**:

```bash
# 1. Survey the changes between previous and latest
omni-dev atlassian confluence compare run 12345
# → outline with per-section summaries and cursors

# 2. Drill into a section flagged as modified
omni-dev atlassian confluence compare section --cursor <CURSOR_FROM_STEP_1>
# → unified diff for that section only
```

See also: [`confluence_compare` / `confluence_compare_section`](mcp.md#confluence-13-tools)
for the MCP equivalents.

#### Confluence: Search

Search pages using CQL or convenience flags:

```bash
# Raw CQL
omni-dev atlassian confluence search --cql "space = ENG AND title ~ 'auth'"

# Convenience flags
omni-dev atlassian confluence search --space ENG
omni-dev atlassian confluence search --title architecture
omni-dev atlassian confluence search --space ENG --title auth --limit 100
```

#### Confluence: Create Pages

```bash
# From JFM file
omni-dev atlassian confluence create page.md

# From CLI flags
omni-dev atlassian confluence create page.md --space ENG --title "New Page"

# With parent page
omni-dev atlassian confluence create page.md --space ENG --title "Child" --parent 12345

# Preview
omni-dev atlassian confluence create page.md --dry-run
```

#### Confluence: Delete Pages

```bash
# Delete (moves to trash, prompts for confirmation)
omni-dev atlassian confluence delete 12345

# Skip confirmation
omni-dev atlassian confluence delete 12345 --force

# Permanently purge (requires space admin)
omni-dev atlassian confluence delete 12345 --force --purge
```

#### Confluence: Children

List direct children of a page or top-level pages in a space.

```bash
# Direct children of a page
omni-dev atlassian confluence children 12345

# Top-level pages in a space (no parent ID)
omni-dev atlassian confluence children --space ENG

# Recursive tree (--max-depth 0 = unlimited)
omni-dev atlassian confluence children 12345 --recursive
omni-dev atlassian confluence children --space ENG --recursive --max-depth 3
```

#### Confluence: Comments

```bash
# List comments
omni-dev atlassian confluence comment list 12345
omni-dev atlassian confluence comment list 12345 --limit 100

# Add a comment from a file or stdin
omni-dev atlassian confluence comment add 12345 comment.md
echo "Looks good" | omni-dev atlassian confluence comment add 12345

# Add an ADF JSON comment
omni-dev atlassian confluence comment add 12345 body.json --format adf
```

#### Confluence: Labels

```bash
# List labels on a page
omni-dev atlassian confluence label list 12345

# Add or remove labels (comma-separated)
omni-dev atlassian confluence label add 12345 --labels architecture,reviewed
omni-dev atlassian confluence label remove 12345 --labels deprecated
```

#### Confluence: User Search

```bash
omni-dev atlassian confluence user search --query "Alice"
omni-dev atlassian confluence user search --query "@example.com" --limit 50
```

#### Confluence: Bulk Download

Recursively download a page tree (or an entire space) to disk. Each page is
written to a `<title>.md` (or `.adf.json`) file mirroring the page tree.

```bash
# Download a subtree starting at a single page
omni-dev atlassian confluence download 12345 --output-dir ./pages

# Download every top-level page in a space
omni-dev atlassian confluence download --space ENG --output-dir ./eng-docs

# Download as raw ADF JSON instead of JFM
omni-dev atlassian confluence download 12345 --format adf

# Filter by title (case-insensitive substring; non-matching parents are
# still traversed so deeply-nested matches still surface)
omni-dev atlassian confluence download --space ENG --title-filter "auth"

# Resume after an interrupted run (uses a manifest to skip done pages)
omni-dev atlassian confluence download --space ENG --resume

# Tune concurrency and depth
omni-dev atlassian confluence download --space ENG --concurrency 16 --max-depth 5

# Conflict resolution when a file already exists
omni-dev atlassian confluence download --space ENG --on-conflict overwrite
omni-dev atlassian confluence download --space ENG --on-conflict skip

# Also fetch each page's attachment binaries into an `attachments/`
# subdirectory beside its content file (full-page snapshots)
omni-dev atlassian confluence download --space ENG --include-attachments
```

#### Confluence: Attachments

Manage attachment binaries on a page. Use `list` to discover attachment IDs,
then `download` to pull a single binary off the page without dropping out to
`curl` (credentials stay inside the wrapper).

```bash
# List a page's attachments (the ID column feeds `download`/`delete`)
omni-dev atlassian confluence attachment list 12345

# Download one attachment by ID — defaults to its filename in the cwd
omni-dev atlassian confluence attachment download att-98765

# Write it to an explicit path (an existing directory is joined with the
# attachment's filename)
omni-dev atlassian confluence attachment download att-98765 --output ./diagram.png
omni-dev atlassian confluence attachment download att-98765 --output ./downloads/
```

To capture a whole page tree *with* its attachment binaries in one command,
use `confluence download --include-attachments` (above).

`--on-conflict` accepts `backup` (default — writes `.bak` and overwrites),
`skip`, or `overwrite`.

#### Offline Format Conversion

Convert between JFM markdown and ADF JSON locally without credentials:

```bash
# Markdown to ADF JSON
omni-dev atlassian convert to-adf issue.md
omni-dev atlassian convert to-adf issue.md --compact

# ADF JSON to markdown
omni-dev atlassian convert from-adf issue.json
omni-dev atlassian convert from-adf issue.json --strip-local-ids   # cleaner output

# Pipe for inspection
cat issue.md | omni-dev atlassian convert to-adf | jq .
```

`--strip-local-ids` drops the `localId` attributes ADF emits on tables,
panels, etc. — useful when the rendered markdown is going to a human
reviewer rather than back into Atlassian.

#### Auto-Pagination

All commands that query paginated endpoints auto-paginate transparently.
Use `--limit` to control how many results are fetched:

```bash
# Default: up to 50 results
omni-dev atlassian jira search --project PROJ

# Fetch more
omni-dev atlassian jira search --project PROJ --limit 200

# Fetch all (no limit)
omni-dev atlassian jira search --project PROJ --limit 0
```

#### JFM Markdown Syntax

JFM supports standard GitHub-Flavored Markdown plus directives for
JIRA-specific elements:

**Standard markdown**: headings, bold, italic, code, strikethrough, links,
images, lists, task lists, tables, code blocks, blockquotes, horizontal rules.

**Inline directives** for JIRA constructs without markdown equivalents:

```markdown
Status: :status[In Progress]{color=blue}
Assigned to: :mention[Alice]{id=abc123}
Due: :date[2026-04-15]
Emoji: :smile:
```

**Container directives** for panels and other blocks:

```markdown
:::panel{type=info}
This is an info panel with **rich** content inside.
:::

:::expand{title="Click to expand"}
Hidden content here.
:::
```

**Leaf block directives** for smart links and cards:

```markdown
::card[https://example.com/page]
```

## Datadog Integration

omni-dev exposes read-only access to the Datadog v1/v2 APIs through the
`omni-dev datadog` command tree. The full reference — authentication,
every family's CLI subcommands with worked examples and sample output,
rate-limit behaviour, and troubleshooting — lives in
**[docs/datadog.md](datadog.md)**.

Quick orientation:

```bash
# One-time credential setup (writes ~/.omni-dev/settings.json)
omni-dev datadog auth login
omni-dev datadog auth status

# Examples from the nine capability families
omni-dev datadog metrics query --query 'avg:system.cpu.user{*}' --from 15m
omni-dev datadog monitor list --tags env:prod
omni-dev datadog dashboard list
omni-dev datadog logs search --filter 'service:api status:error' --from 1h
omni-dev datadog events list --filter 'service:api' --sources kubernetes
omni-dev datadog slo list --tags team:platform
omni-dev datadog downtime list --active-only
omni-dev datadog hosts list --filter env:prod
```

Every Datadog CLI subcommand has a matching `datadog_*` MCP tool — see
[docs/mcp.md](mcp.md#datadog-14-tools).

## Contextual Intelligence

### Overview

Contextual intelligence makes omni-dev understand your project to provide better suggestions:

- **Project Context**: Conventions from `.omni-dev/` configuration
- **Branch Context**: Work type from branch naming patterns
- **File Context**: Architectural understanding of changed files
- **Pattern Context**: Recognition of work patterns across commits

### Setting Up Context

#### 1. Create Context Directory

```bash
mkdir .omni-dev
```

#### 2. Define Project Scopes (`.omni-dev/scopes.yaml`)

Tell omni-dev about your project's areas. See
[`omni-dev-directory.md`](omni-dev-directory.md#scopesyaml) for the file's
format contract and validation behaviour.

```yaml
scopes:
  - name: "auth"
    description: "Authentication and authorization systems"
    examples: 
      - "auth: add OAuth2 support"
      - "auth: fix token validation" 
    file_patterns:
      - "src/auth/**"
      - "auth.rs"
      - "middleware/auth.rs"

  - name: "api"  
    description: "REST API endpoints and handlers"
    examples:
      - "api: add user endpoints"
      - "api: improve error handling"
    file_patterns:
      - "src/api/**"
      - "handlers/**"
      - "routes/**"

  - name: "ui"
    description: "User interface components"
    examples:
      - "ui: add responsive navigation"  
      - "ui: fix mobile layout"
    file_patterns:
      - "src/components/**"
      - "*.vue"
      - "*.tsx"

  - name: "docs"
    description: "Documentation and guides"
    examples:
      - "docs: add API reference"
      - "docs: update installation guide"
    file_patterns:
      - "docs/**"
      - "*.md"
      - "README*"
```

#### 3. Set Commit Guidelines (`.omni-dev/commit-guidelines.md`)

Define your project's commit message standards. See
[`omni-dev-directory.md`](omni-dev-directory.md#commit-guidelinesmd) for the
full format contract — including precedence between project-scope, user-scope,
and global fallbacks — and the validation messages omni-dev emits on a
malformed file.

```markdown
# Project Commit Guidelines

## Format
Use conventional commits: `type(scope): description`

## Types We Use
- `feat` - New features  
- `fix` - Bug fixes
- `docs` - Documentation changes
- `refactor` - Code restructuring
- `test` - Adding tests
- `chore` - Build/tooling changes

## Style Rules
- Keep subject line under 50 characters
- Use imperative mood: "Add feature" not "Added feature"  
- Capitalize first letter of description
- No period at end of subject line

## Our Scopes
- `auth` - Authentication systems
- `api` - Backend API changes
- `ui` - Frontend interface
- `db` - Database changes
- `deploy` - Deployment/infrastructure

## Examples
```

feat(auth): add OAuth2 Google integration
fix(api): resolve rate limiting edge case
docs(readme): update installation instructions
refactor(ui): extract common button component

```

### Branch Context Detection

omni-dev automatically detects work type from branch names:

| Branch Pattern | Detected Type | Example |
|----------------|---------------|---------|
| `feature/auth-system` | feature | Feature development |
| `fix/login-bug` | fix | Bug fix |
| `docs/api-guide` | docs | Documentation |
| `refactor/user-service` | refactor | Code restructuring |
| `JIRA-123-user-auth` | feature | Ticket-based |
| `username/feature-name` | feature | User branches |

### Intelligent Verbosity

omni-dev adjusts message detail based on change significance:

- **Comprehensive**: Major features, architectural changes
  - Multi-paragraph descriptions
  - Bulleted feature lists  
  - Impact statements

- **Detailed**: Moderate changes, multi-file updates
  - Subject + explanatory body
  - Key change highlights

- **Concise**: Minor changes, single-file updates
  - Clear conventional format
  - Essential information only

## Workflows

### Feature Branch Cleanup

Clean up commits before merging:

```bash
# 1. Work on feature branch with quick commits
git checkout -b feature/user-dashboard
git commit -m "wip"
git commit -m "fix stuff"  
git commit -m "add more"

# 2. Before merging, improve all commit messages
omni-dev git commit message twiddle 'main..HEAD' --use-context

# 3. Review suggestions and apply
# ✅ Professional commit history ready for review
```

### Complete Feature Development Workflow

End-to-end workflow from feature development to PR creation:

```bash
# 1. Create and work on feature branch
git checkout -b feature/user-authentication
# ... make changes and commits ...

# 2. Improve commit messages with AI
omni-dev git commit message twiddle 'main..HEAD' --use-context

# 3. Create professional PR with AI-generated description
omni-dev git branch create pr

# ✅ Complete: clean commits + comprehensive PR ready for team review
```

### PR Creation and Updates

Handle PR creation and updates efficiently:

```bash
# Create new PR with AI-generated description
omni-dev git branch create pr main

# If PR already exists, update it with new description
omni-dev git branch create pr --auto-apply

# Save PR details for review before creating
omni-dev git branch create pr --save-only review-pr.yaml
# Review and edit the file...
# Then create manually using GitHub CLI or web interface

# Drive the PR description from commit messages (no diff sent to AI)
# Useful when the branch's commits are well-crafted and convey the intent
omni-dev git branch create pr --from-commits
```

### Collaborative PR Workflow

Work with existing PRs and team feedback:

```bash
# Update existing PR after new commits
git add . && git commit -m "address review feedback"
omni-dev git branch create pr  # Updates existing PR

# Generate PR description without creating (for draft PRs)
omni-dev git branch create pr --save-only draft-pr.yaml
# Use the content to update draft PR manually
```

### Large Codebase Migration

Handle large commit ranges efficiently:

```bash
# Process 100+ commits with parallel processing
omni-dev git commit message twiddle 'HEAD~100..HEAD' --concurrency 5

# Save suggestions for review before applying
omni-dev git commit message twiddle 'HEAD~50..HEAD' --save-only review.yaml

# Review the file, then apply manually
omni-dev git commit message amend review.yaml
```

### Legacy Repository Cleanup

Improve old commit messages:

```bash
# Analyze what needs improvement
omni-dev git commit message view 'HEAD~20..HEAD'

# Apply contextual improvements
omni-dev git commit message twiddle 'HEAD~20..HEAD' --use-context

# For very old commits, might need specific handling
git rebase -i HEAD~20  # Interactive rebase first if needed
```

### Team Onboarding

Set up consistent commit standards:

```bash
# 1. Set up project context (one-time setup)
mkdir .omni-dev
# Create scopes.yaml and commit-guidelines.md

# 2. Add to team documentation
echo "Use: omni-dev git commit message twiddle 'main..HEAD' --use-context" >> CONTRIBUTING.md

# 3. Include in CI/PR checks
# Add validation that commit messages follow conventions
```

## Advanced Usage

### Custom Context Directory

Use a different location for context files:

```bash
# Use custom context directory
omni-dev git commit message twiddle 'HEAD~5..HEAD' --context-dir ./project-config

# Context files would be in:
# ./project-config/scopes.yaml
# ./project-config/commit-guidelines.md
```

### Concurrency Configuration

Adjust parallel processing based on your needs:

```bash
# Lower concurrency for complex commits (reduces API load)
omni-dev git commit message twiddle 'HEAD~20..HEAD' --concurrency 2

# Higher concurrency for faster processing
omni-dev git commit message twiddle 'HEAD~20..HEAD' --concurrency 8

# Skip coherence pass for independent commits
omni-dev git commit message twiddle 'HEAD~10..HEAD' --no-coherence
```

### Integration with Git Hooks

Set up automatic improvement in git hooks:

```bash
# .git/hooks/pre-push (make executable)
#!/bin/bash
echo "🤖 Analyzing commit messages..."
omni-dev git commit message view 'origin/main..HEAD' --quiet || {
    echo "❌ Commit analysis failed"
    echo "💡 Consider running: omni-dev git commit message twiddle 'origin/main..HEAD' --use-context"
    exit 1
}
```

### Save and Review Workflow

For high-stakes changes, save suggestions first:

```bash
# 1. Save suggestions to file
omni-dev git commit message twiddle 'HEAD~10..HEAD' --save-only suggestions.yaml

# 2. Review the suggestions file
cat suggestions.yaml

# 3. Edit if needed, then apply
omni-dev git commit message amend suggestions.yaml
```

## Best Practices

### 1. Use Contextual Intelligence

Always use `--use-context` for best results:

```bash
# ✅ Good - uses project context
omni-dev git commit message twiddle 'main..HEAD' --use-context

# ⚠️ Basic - misses project-specific intelligence  
omni-dev git commit message twiddle 'main..HEAD'
```

### 2. Set Up Project Context

Invest time in setting up `.omni-dev/` configuration:

- Define meaningful scopes for your project
- Document your commit conventions
- Include file pattern matching for accuracy

### 3. Batch Size Guidelines

| Repository Size | Suggested Batch Size | Reasoning |
|----------------|---------------------|-----------|
| Small projects | 6-8 commits | Faster processing |
| Medium projects | 4-5 commits | Balanced accuracy/speed |
| Large projects | 2-3 commits | More context per batch |
| Complex changes | 1-2 commits | Maximum accuracy |

### 4. Review Before Applying

For important branches, always review suggestions:

```bash
# Save first, review, then apply
omni-dev git commit message twiddle 'main..HEAD' --save-only review.yaml
# Review the file...
omni-dev git commit message amend review.yaml
```

### 5. Clean Working Directory

Always ensure clean working directory:

```bash
# Check status first
git status

# Commit or stash changes before running omni-dev
git add . && git commit -m "temp" || git stash
omni-dev git commit message twiddle 'HEAD~5..HEAD' --use-context
```

### 6. API Key Security

Keep your Claude API key in an environment variable or `.env` file;
never in command-line arguments or scripts. See
[Authentication](configuration.md#authentication) for the canonical
setup guide.

### 7. Integration with Team Workflow

Make it part of your team's process:

```bash
# Add to PR template
echo "- [ ] Run \`omni-dev git commit message twiddle 'main..HEAD' --use-context\`" >> .github/pull_request_template.md

# Document in CONTRIBUTING.md
echo "Before creating a PR, clean up commit messages with omni-dev" >> CONTRIBUTING.md

# Add PR creation to workflow
echo "Create PR with: \`omni-dev git branch create pr\`" >> CONTRIBUTING.md
```

### 8. PR Creation Best Practices

Optimize your PR creation workflow:

```bash
# ✅ Good - Clean commits first, then create PR
omni-dev git commit message twiddle 'main..HEAD' --use-context
omni-dev git branch create pr

# ✅ Good - Review PR details before creating
omni-dev git branch create pr --save-only review.yaml
# Edit file if needed, then use GitHub CLI or web interface

# ⚠️ Caution - Ensure working directory is clean
git status  # Check for uncommitted changes first

# ✅ Good - Use base branch when not default
omni-dev git branch create pr develop  # For non-main base branches
```

## Troubleshooting

See [Troubleshooting Guide](troubleshooting.md) for common issues and solutions.

## Need Help?

- 📖 [Configuration Guide](configuration.md) - Detailed setup instructions
- 🔧 [Troubleshooting](troubleshooting.md) - Common issues  
- 📝 [Examples](examples.md) - Real-world usage examples
- 💬 [GitHub Discussions](https://github.com/rust-works/omni-dev/discussions) - Community support
- 🐛 [GitHub Issues](https://github.com/rust-works/omni-dev/issues) - Bug reports and features
