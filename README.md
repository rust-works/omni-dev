# omni-dev

[![Crates.io](https://img.shields.io/crates/v/omni-dev.svg)](https://crates.io/crates/omni-dev)
[![Documentation](https://docs.rs/omni-dev/badge.svg)](https://docs.rs/omni-dev)
[![Build Status](https://github.com/rust-works/omni-dev/workflows/CI/badge.svg)](https://github.com/rust-works/omni-dev/actions)
[![License: BSD-3-Clause](https://img.shields.io/badge/License-BSD%203--Clause-blue.svg)](LICENSE)

An intelligent Git commit message toolkit with AI-powered contextual
intelligence. Transform messy commit histories into professional,
conventional commit formats with project-aware suggestions.

## 🎬 See It In Action

[![asciicast](https://asciinema.org/a/eJJf5Aj8N26JoCaUsAFVH8dqz.svg)](https://asciinema.org/a/eJJf5Aj8N26JoCaUsAFVH8dqz)

*Watch omni-dev transform messy commits into professional ones with AI-powered analysis*

## 30-Second Demo

Transform your commit messages and create professional PRs with AI intelligence:

```bash
# Analyze and improve commit messages in your current branch
omni-dev git commit message twiddle 'origin/main..HEAD' --use-context

# Before: "fix stuff", "wip", "update files"
# After:  "feat(auth): implement OAuth2 authentication system"
#         "docs(api): add comprehensive endpoint documentation"
#         "fix(ui): resolve mobile responsive layout issues"

# Create a professional PR with AI-generated description
omni-dev git branch create pr
# 🎉 Generates comprehensive PR with detailed description, testing info, and more
```

## ✨ Key Features

- 🤖 **AI-Powered Intelligence**: Claude AI analyzes your code changes to
  suggest meaningful commit messages and PR descriptions
- 🧠 **Contextual Awareness**: Understands your project structure,
  conventions, and work patterns
- 🔍 **Comprehensive Analysis**: Deep analysis of commits, branches, and
  file changes
- ✏️ **Smart Amendments**: Safely improve single or multiple commit messages
- 🚀 **PR Creation**: Generate professional pull requests with AI-powered
  descriptions
- 📦 **Automatic Batching**: Handles large commit ranges intelligently
- 🎯 **Conventional Commits**: Automatic detection and formatting
- 🌐 **Browser Bridge**: Drive HTTP requests through an authenticated browser
  tab without exfiltrating cookies or tokens
- 🗂️ **Worktrees View**: One live view of every repo and git worktree open
  across all your VS Code windows
- 🛡️ **Safety First**: Working directory validation, protection against
  amending commits already in remote main branches, and error recovery
- ⚡ **Fast & Reliable**: Built with Rust for memory safety and performance

## 🚀 Quick Start

### Installation

```bash
# Install from crates.io
cargo install omni-dev

# Install with Nix
nix profile install github:rust-works/omni-dev

# Install with Nix flakes (development)
nix run github:rust-works/omni-dev
```

**Next step:** see [Getting Started](docs/getting-started.md) — a
10-minute walkthrough from authentication to your first AI-improved
commit. (For just the API-key reference, see
[Authentication](docs/configuration.md#authentication).)

#### Shell Completion

`omni-dev completions <shell>` prints a completion script to stdout for
`bash`, `zsh`, `fish`, `powershell`, or `elvish`. The quickest path is bash
per-user:

```bash
# Add to ~/.bashrc:
eval "$(omni-dev completions bash)"
```

See [docs/shell-completion.md](docs/shell-completion.md) for per-shell install
recipes, the `$fpath`/`compinit` setup zsh requires, and troubleshooting.

## 🆚 How omni-dev Compares

omni-dev sits in two adjacent spaces — AI commit-message tooling and
Atlassian/dev-workflow MCP servers. The tables below contrast the
incumbents on the dimensions a first-time reader is most likely to weigh.
In every cell, `✅` means full / native support, `⚠` means partial or
available only with caveats, and `❌` means not supported — and omni-dev's
own limitations are flagged just as honestly (the `⚠` marks in its own
columns).

Beyond these two niches, omni-dev also ships a supervised **daemon** that
hosts a **browser bridge** (an authenticated proxy that runs requests
through a logged-in browser tab for SSO-gated dashboards such as Grafana
and Loki), a **Snowflake** SQL service (one external-browser SSO session
reused for concurrent queries), and a **worktrees** registry (one live view of
the repos open across every VS Code window), plus a local append-only
**request log** (`omni-dev log`). These have no direct incumbent in either
table below, so
they are called out here rather than scored against tools that don't aim
for them.

### vs AI commit tools

|                                                       | omni-dev                            | [opencommit](https://github.com/di-sukharev/opencommit) | [aicommits](https://github.com/Nutlope/aicommits) |
|-------------------------------------------------------|-------------------------------------|---------------------------------------------------------|---------------------------------------------------|
| Rewrite existing commits in a range                   | ✅ `twiddle`                         | ❌ pre-commit only                                       | ❌ pre-commit only                                 |
| Parallel batched processing (long ranges)             | ✅ `--concurrency N`                 | ❌                                                       | ❌                                                 |
| AI-written PR descriptions                            | ✅ `git branch create pr`            | ⚠ GitHub Action only                                    | ❌                                                 |
| Project-context awareness                             | ✅ `--use-context`                   | ❌                                                       | ❌                                                 |
| Sandboxed `claude-cli` backend                        | ✅ [ADR-0028](docs/adrs/adr-0028.md) | ❌                                                       | ❌                                                 |
| Multi-backend (Anthropic / Bedrock / OpenAI / Ollama) | ✅                                   | ✅                                                       | ✅                                                 |
| Conventional Commits                                  | ✅                                   | ✅                                                       | ⚠ config                                          |
| Language / runtime                                    | Rust (static binary)                | Node.js                                                 | Node.js                                           |

### vs Atlassian-workflow MCP servers

omni-dev's MCP server also exposes Git tools (commit analysis, twiddling,
PR creation), Datadog tools, and an `ai_chat` proxy — surfaces the
Atlassian-focused servers don't aim for. The table below compares only
Atlassian capability depth.

|                                         | omni-dev MCP                                                                          | [sooperset/mcp-atlassian](https://github.com/sooperset/mcp-atlassian)             | [Atlassian official (Rovo)](https://github.com/atlassian/atlassian-mcp-server)                              |
|-----------------------------------------|---------------------------------------------------------------------------------------|-----------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------------------|
| Jira REST surface                       | ✅ 36 tools (agile, fields, dev panel, links, watchers, worklogs, versions, changelog) | ✅ 49 tools (above + JSM, proforma forms, SLA, batch ops)                          | ⚠ 14 tools (basic CRUD, search, transitions, worklogs only)                                                 |
| Confluence REST surface                 | ✅ 25 tools (history, diff, attachments, labels, spaces, inline + footer comments)     | ✅ 24 tools (history, diff, attachments, labels; **no inline comments / spaces**)  | ⚠ 12 tools (inline + footer comments, spaces; **no delete / move / history / diff / attachments / labels**) |
| Lossless JFM ↔ ADF round-trip           | ✅ full ADF node set (schema v56.1.3) + unsupported-node escape                        | ❌                                                                                 | ⚠ raw ADF, model-dependent                                                                                  |
| Anchored review-comment preservation    | ✅ annotation marks survive round-trip                                                 | ❌ anchor stripped, comments orphaned                                              | ⚠ ADF carries anchors; model-dependent                                                                      |
| Pre-flight ADF schema validation        | ✅ nesting + arity, before write                                                       | ❌                                                                                 | ❌                                                                                                           |
| Offline JFM ↔ ADF conversion (no creds) | ✅ `atlassian_convert`                                                                 | ❌                                                                                 | ❌                                                                                                           |
| Cloud + Server + Data Center            | ⚠ Cloud verified                                                                      | ✅ Cloud + Server (v6+) + DC (Jira v8.14+)                                         | ❌ Cloud only                                                                                                |
| Auth                                    | ⚠ API token only                                                                      | ✅ API token / PAT / OAuth 2.0                                                     | ✅ OAuth 2.1 / API token                                                                                     |

_Last verified: 2026-06-23. omni-dev and sooperset rows are live-tested — a
`tools/list` enumeration (omni-dev branch build vs
`ghcr.io/sooperset/mcp-atlassian:latest`) plus a live read→write→read fidelity
cycle on a complex page. Atlassian Rovo's server accepts the API token but
gates tool **execution** behind an org-admin grant, so its rows combine
Atlassian's
[Supported tools](https://support.atlassian.com/atlassian-rovo-mcp-server/docs/supported-tools/)
docs with the ADF-passthrough reasoning (raw ADF can round-trip, but only if
the model echoes it faithfully — no deterministic guarantee), not a live run.
Refresh quarterly or whenever a release-note search for the comparators flags
a relevant change._

## 📋 Core Commands

### 🤖 AI-Powered Commit Improvement (`twiddle`)

The star feature - intelligently improve your commit messages with real-time model information display:

```bash
# Improve commits with contextual intelligence
omni-dev git commit message twiddle 'origin/main..HEAD' --use-context

# Process large commit ranges with parallel processing
omni-dev git commit message twiddle 'HEAD~20..HEAD' --concurrency 5

# Save suggestions to file for review
omni-dev git commit message twiddle 'HEAD~5..HEAD' \
  --save-only suggestions.yaml

# Auto-apply improvements without confirmation
omni-dev git commit message twiddle 'HEAD~3..HEAD' --auto-apply
```

### 🔍 Analysis Commands

```bash
# Analyze commits in detail (YAML output)
omni-dev git commit message view 'HEAD~3..HEAD'

# Analyze current branch vs main
omni-dev git branch info main

# Get comprehensive help
omni-dev help-all
```

### 🚀 AI-Powered PR Creation

Create professional pull requests with AI-generated descriptions:

```bash
# Generate and create PR with AI-powered description
omni-dev git branch create pr

# Create PR with specific base branch
omni-dev git branch create pr main

# Save PR details to file without creating
omni-dev git branch create pr --save-only pr-description.yaml

# Auto-create without confirmation
omni-dev git branch create pr --auto-apply
```

### 📝 Atlassian Integration

Read, write, and manage JIRA issues and Confluence pages from the command line:

```bash
# Authenticate with Atlassian Cloud
omni-dev atlassian auth login

# Check authentication status
omni-dev atlassian auth status

# Fetch a JIRA issue as markdown
omni-dev atlassian jira read PROJ-123

# Fetch as raw ADF JSON
omni-dev atlassian jira read PROJ-123 --format adf

# Push markdown changes back to JIRA
omni-dev atlassian jira write PROJ-123 issue.md

# Interactive edit: fetch, edit in $EDITOR, push
omni-dev atlassian jira edit PROJ-123

# Search issues with JQL
omni-dev atlassian jira search --project PROJ --status Open

# Create an issue
omni-dev atlassian jira create issue.md --project PROJ --summary "Fix bug"

# Transition an issue
omni-dev atlassian jira transition PROJ-123 "In Progress"

# Confluence: read, search, create pages
omni-dev atlassian confluence read 12345
omni-dev atlassian confluence search --space ENG --title auth
omni-dev atlassian confluence create page.md --space ENG --title "New Page"

# Convert markdown to ADF JSON (offline)
omni-dev atlassian convert to-adf input.md
```

### 📊 Datadog Integration (read-only)

Authenticate against the Datadog API and query metrics, monitors, dashboards,
logs, events, SLOs, hosts, and downtimes. See the [Datadog integration
guide](docs/datadog.md) for the full subcommand reference, authentication
setup, rate-limit behaviour, and troubleshooting.

```bash
# Configure Datadog API credentials (prompts for API key, APP key, and site)
omni-dev datadog auth login

# Verify the credentials by calling /api/v1/validate
omni-dev datadog auth status

# Query metrics, monitors, dashboards, logs, and SLOs
omni-dev datadog metrics query --query 'avg:system.cpu.user{*}' --from 15m
omni-dev datadog monitor list --tags env:prod
omni-dev datadog dashboard list
omni-dev datadog logs search --filter 'service:api status:error' --from 1h
omni-dev datadog slo list --tags team:platform
```

`DATADOG_SITE` defaults to `datadoghq.com`. Other regions (`datadoghq.eu`,
`us3.datadoghq.com`, `us5.datadoghq.com`, `ap1.datadoghq.com`, `ddog-gov.com`)
are recognised without warning. Environment variables `DATADOG_API_KEY`,
`DATADOG_APP_KEY`, `DATADOG_SITE` override the stored settings. For on-prem
or proxied installs, set `DATADOG_API_URL` to override the site-derived URL.

All Datadog subcommands are also exposed as MCP tools (`datadog_*`) — see
[docs/mcp.md](docs/mcp.md#datadog-14-tools). For the full guide covering
every family with worked examples, see [docs/datadog.md](docs/datadog.md).

### 🎙️ Transcript Fetching

Pull captions and transcripts from external media platforms. YouTube is the
first supported source; the CLI namespace and library are designed so
additional sources (Vimeo, podcast RSS, generic VTT/SRT URLs) can be
added without restructuring. See [docs/transcript.md](docs/transcript.md)
for the full reference and the recipe for adding a new source.

```bash
# Fetch captions for a YouTube video as SubRip (default).
omni-dev transcript youtube fetch https://www.youtube.com/watch?v=jNQXAC9IVRw

# WebVTT to a file, falling through to auto-generated captions if needed.
omni-dev transcript youtube fetch jNQXAC9IVRw \
  --format vtt --auto --output me-at-the-zoo.vtt

# Synthesise a translated track when no native French track exists.
omni-dev transcript youtube fetch <url> --lang fr --translate fr

# List available caption tracks (manual + auto-generated).
omni-dev transcript youtube list-langs <url>

# Show video metadata (title, channel, duration, languages).
omni-dev transcript youtube info <url> --output json
```

`--format` accepts `srt`, `vtt`, `txt`, or `json`. Locators may be a
`watch?v=` URL, a `youtu.be/` short URL, a `/shorts/` or `/embed/` URL,
or a bare 11-character video ID. Age-gated and login-required videos
surface as a typed `PlayabilityRefused` error carrying YouTube's status
code rather than a generic HTTP failure.

### 🌐 Browser Bridge

Drive HTTP requests **through an authenticated browser tab**. When you are
investigating internal services (Grafana/Loki, internal dashboards, SSO-gated
admin panels), the browser already holds sessions — SSO, OAuth, cookies — that
are hard to replicate programmatically. The bridge issues requests inside the
browser's authenticated context **without exfiltrating cookies or tokens** (a
*confused deputy by design*). Both planes are authenticated and default-closed;
see [docs/browser-bridge.md](docs/browser-bridge.md) for the full guide and
[ADR-0036](docs/adrs/adr-0036.md) for the security rationale.

```bash
# Start the bridge; it prints the bound ports, a session token, and a JS
# snippet to paste into the DevTools console of the authenticated tab.
omni-dev browser bridge serve

# Drive requests through the tab (token from the bridge's stdout).
export OMNI_BRIDGE_TOKEN=<token printed by the bridge>
omni-dev browser bridge request --url /loki/api/v1/labels

# POST a JSON payload from a file, with a custom header.
omni-dev browser bridge request --url /api/foo --method POST \
  --body @payload.json --header "Accept: application/json"

# Stream a long-lived endpoint (SSE / chunked) instead of buffering.
omni-dev browser bridge request --url /api/events --stream

# Route to a specific tab when several are connected (by id or origin).
omni-dev browser bridge request --url /api/foo --target https://grafana.internal
```

Supports binary and streaming response bodies, multi-tab routing via
`X-Omni-Bridge-Target`, per-request `--credentials` and `--allow-origin`
overrides, and a transparent proxy for tools that speak plain HTTP.

### 🛰️ Daemon

Host long-lived services in one supervised process behind a private per-user
Unix-domain control socket. The browser bridge is the first service migrated
onto it (Snowflake and the worktrees registry followed), and on macOS an
optional menu-bar app gives live control. `daemon start` installs a launchd LaunchAgent for auto-start at
login, and `status` reports every hosted service. See
[Running under the daemon](docs/browser-bridge.md#running-under-the-daemon) and
[ADR-0039](docs/adrs/adr-0039.md) for the architecture.

```bash
# Start the background daemon (installs a launchd LaunchAgent on macOS)
omni-dev daemon start

# Per-service status (add --json for machines)
omni-dev daemon status

# Restart or stop it
omni-dev daemon restart
omni-dev daemon stop
```

The daemon is Unix-only — its control plane is a Unix-domain socket — while the
rest of omni-dev runs everywhere.

### ❄️ Snowflake

Authenticate a Snowflake session once via external-browser SSO, then run
concurrent arbitrary SQL across any account **without an SSO popup on every
query**. The daemon holds the session in memory and multiplexes a bounded pool,
so each query can still set its own warehouse/role/database/schema. See
[docs/snowflake-service.md](docs/snowflake-service.md).

```bash
# Run SQL (from an argument or stdin); the first query opens the SSO browser
omni-dev snowflake query "select current_version()"

# Per-query context overrides and JSON output
omni-dev snowflake query "select * from t limit 10" \
  --warehouse WH --role ANALYST --database DB --schema PUBLIC --format json

# Inspect or evict live sessions
omni-dev snowflake sessions
omni-dev snowflake disconnect --account <ACCOUNT> --user <USER>
```

Account/user/context default from `SNOWFLAKE_*` env vars then
`~/.omni-dev/settings.json` — no accounts are hardcoded. Runs on the daemon, so
it is Unix-only.

### 📓 Request Log

Every invocation and the HTTP requests it issues are recorded to a local,
append-only log you can search and tail. Best-effort and default-on; **no
secret is ever written** (auth headers are redacted, bodies opt-in). See
[docs/log.md](docs/log.md).

```bash
# Recent activity (one line each)
omni-dev log

# Filter by service and status class, or a query expression; follow live
omni-dev log --service jira --status 5xx
omni-dev log --query 'method:POST AND status:4xx' --follow

# Full records as JSON (byte-identical to the on-disk lines)
omni-dev log --format json -n 20
```

Set `OMNI_DEV_LOG_DISABLE=1` to turn it off, or `OMNI_DEV_LOG_BODIES=1` /
`OMNI_DEV_LOG_HEADERS=1` to opt into capturing bodies/headers.

### 🗂️ Worktrees

See every repo and git worktree open across **all** your VS Code windows in
one live view. A VS Code extension host is sandboxed per window — no extension
alone can see a sibling window's folders — so a small first-party companion
extension registers each window with the daemon, which aggregates them into a
single registry served back to the CLI, tray, and extension UI. The registry
is in-memory only; windows that crash without unregistering age out
automatically. See [docs/worktrees-service.md](docs/worktrees-service.md) and
[ADR-0040](docs/adrs/adr-0040.md).

```bash
# One line per open window and its folders (add --json for machines)
omni-dev worktrees list
```

Runs on the daemon, so it is Unix-only.

### 📈 Coverage Diff

Attribute a per-line coverage report to a git diff and report **patch
coverage** — the share of added lines that are tested — plus the uncovered new
lines, per-file deltas, and indirect coverage changes. Reads lcov, llvm-cov
JSON, or Cobertura XML (auto-detected), renders markdown/YAML/JSON, and can gate
a branch. It powers the project's PR coverage comment and runs locally too. See
[docs/coverage.md](docs/coverage.md).

```bash
# Patch coverage for the working tree against the default merge-base
omni-dev coverage diff --report head.lcov

# Fail if patch coverage is under 80% (a CI gate or a pre-push check)
omni-dev coverage diff --report head.lcov --fail-under-patch 80

# Full report with project deltas, as JSON
omni-dev coverage diff --report head.lcov --baseline-report base.lcov --format json
```

### ✏️ Manual Amendment

```bash
# Apply specific amendments from YAML file
omni-dev git commit message amend amendments.yaml
```

### 🧩 Claude Code Slash-Commands

Generate ready-to-use Claude Code slash-command templates into the
project's `.claude/commands/` directory. Each template is a self-contained
workflow that drives a multi-step omni-dev operation from inside a Claude
Code session.

```bash
# Generate all templates: commit-twiddle, pr-create, pr-update
omni-dev commands generate all

# Or individually
omni-dev commands generate commit-twiddle
omni-dev commands generate pr-create
omni-dev commands generate pr-update
```

Each subcommand writes `.claude/commands/<name>.md`. Commit the files to
share the workflows with collaborators — Claude Code picks them up
automatically, so anyone in the repo can invoke `/commit-twiddle`,
`/pr-create`, or `/pr-update` inside a Claude Code session. See the
[user guide](docs/user-guide.md#commands-generate--generate-claude-code-slash-commands)
for the full reference.

### 🗒️ Claude Conversation History

Export your Claude Code chat history to a directory of `.jsonl` files for
behavioural analysis, work-log generation, or downstream tooling. Re-running
acts as an idempotent sync: new chats are added, modified chats are
overwritten, unchanged chats are skipped.

```bash
# Mirror ~/.claude/projects to ./history/ (one .jsonl per chat, grouped by project slug)
omni-dev ai claude history sync --target ./history

# Limit to one project (encoded slug or decoded cwd path)
omni-dev ai claude history sync --target ./history --project /Users/me/work/repo

# Only sessions touched in the last week
omni-dev ai claude history sync --target ./history --since 7d

# Preview without writing, then prune target files for sessions removed upstream
omni-dev ai claude history sync --target ./history --dry-run --prune

# Render LLM-friendly markdown alongside the raw jsonl (one .md per session)
omni-dev ai claude history sync --target ./history --output-format jsonl,markdown

# Markdown only — suitable for piping into a coaching LLM
omni-dev ai claude history sync --target ./history --output-format markdown
```

The export is a **behavioural transcript**, not a faithful archive. The
top-level session jsonl captures all prompts, responses, thinking blocks, tool
calls, and tool-result metadata — the signal needed for analysis. Sub-agent
internal turns, large tool-output sidecars, PDF page rasters, and Claude's
auto-memory are deliberately excluded; they would bloat any LLM-ingested
corpus without adding interaction-pattern signal.

In-progress chats produce a valid jsonl prefix (the source size is captured
once at the start of the copy), so you can sync safely while a chat is open.
The target layout mirrors the source — `<target>/<slug>/<uuid>.jsonl` — and
source `mtime` is preserved on each target file so downstream tooling can
sort sessions chronologically without parsing every file.

`--output-format markdown` writes a derived `<target>/<slug>/<uuid>.md`
alongside (or instead of) the jsonl. Each markdown file has YAML frontmatter
with session metadata followed by `## User` / `## Assistant` turns; tool calls
render as `### Tool call: <name>` blocks, thinking blocks collapse into
`<details>`, and sub-agent (`Agent`) calls render the prompt argument only.

Agent-to-user interactions are surfaced as first-class structured events so
the analyst LLM sees what was actually asked and how the user responded:

- `AskUserQuestion` calls render as `### Agent question: <header>` with the
  question text and a bulleted list of options (with descriptions); the
  paired user reply renders as `## User response`.
- Tool denials show up as `**Tool result (<tool>, denied by user):**` —
  detected by the canonical "The user doesn't want to proceed with this tool
  use" sentinel Claude Code stuffs into the next `tool_result`.
- Tool interrupts (escape mid-execution) render as
  `**Tool result (<tool>, interrupted by user):**`.
- Errors (real tool failures, distinct from user denials) keep the
  `error` label; successes use `ok`.

System reminders, attachments, and permission-mode events are included by
default — pass `--exclude-system` to drop them. Markdown idempotency keys off
source mtime alone (the rendered length differs from the source length), and
`--prune` only deletes artifacts whose extension matches one of the formats
listed in `--output-format`.

See [docs/user-guide.md#ai-claude-history-sync--export-conversation-history](docs/user-guide.md#ai-claude-history-sync--export-conversation-history)
for the in-depth reference, and the broader [Claude Code Integration](docs/user-guide.md#claude-code-integration)
section for related commands (`ai chat`, `ai claude skills`).

### 🔌 MCP Server

omni-dev ships an optional **Model Context Protocol** server so AI assistants
(Claude Desktop, Claude Code, the MCP Inspector, custom agents) can call
omni-dev over stdio instead of shelling out to the CLI. The server is
delivered as a second binary, `omni-dev-mcp`, gated behind the `mcp` Cargo
feature (see [ADR-0021](docs/adrs/adr-0021.md)).

Tools cover six domains:

| Domain | Examples |
|--------|----------|
| **Git** (5) | `git_view_commits`, `git_branch_info`, `git_check_commits`, `git_twiddle_commits`, `git_create_pr` |
| **JIRA** (28) | core read/write/search/transition/comment/link/dev/delete; sprints, boards, watchers, worklogs, fields, attachments, projects, changelog |
| **Confluence** (13) | read/write/search/create/delete/download/children, comments, labels, user search |
| **Atlassian shared** (2) | `atlassian_auth_status`, `atlassian_convert` (offline JFM ↔ ADF) |
| **Datadog** (14) | metrics, monitors, dashboards, logs, events, SLOs, hosts, downtimes, metrics catalog |
| **AI / Config** (5) | `ai_chat` (one-shot chat), `claude_skills_*` (sync / clean / status for `.claude/skills/` distribution), `config_models_show` |

Resources exposed via URI templates:

| URI template                    | Returns                          |
|---------------------------------|----------------------------------|
| `git://repo/commits/{range}`    | YAML commit analysis             |
| `jira://issue/{key}`            | JIRA issue as JFM                |
| `jira://issue/{key}.adf`        | JIRA issue body as ADF           |
| `confluence://page/{id}`        | Confluence page as JFM           |
| `confluence://page/{id}.adf`    | Confluence page body as ADF      |
| `omni-dev://specs/{name}`       | Embedded reference specs (e.g. `jfm`) |

See [docs/mcp.md](docs/mcp.md) for the full tool catalog, resource
reference, cross-cutting parameters (`output_file`, `confirm`), and
troubleshooting.

#### Install

```bash
cargo install omni-dev --features mcp
```

This adds a second binary, `omni-dev-mcp`, alongside the regular `omni-dev`
CLI. The default `cargo install omni-dev` build is unchanged — no MCP
dependencies are pulled in unless the `mcp` feature is enabled.

#### Claude Desktop

Edit `~/Library/Application Support/Claude/claude_desktop_config.json` on
macOS (or `%APPDATA%\Claude\claude_desktop_config.json` on Windows):

```json
{
  "mcpServers": {
    "omni-dev": {
      "command": "omni-dev-mcp"
    }
  }
}
```

#### Claude Code

Per-project — create `.mcp.json` at the repo root:

```json
{
  "mcpServers": {
    "omni-dev": {
      "command": "omni-dev-mcp"
    }
  }
}
```

Or register globally with the Claude Code CLI:

```bash
claude mcp add omni-dev omni-dev-mcp
```

#### Smoke-test with the MCP Inspector

```bash
npx @modelcontextprotocol/inspector omni-dev-mcp
```

The Inspector opens a browser UI where you can list tools and resources,
call any tool interactively, and fetch resources against the current working
directory.

#### Configuration (`settings.json`)

Three server defaults can be set once in the `mcp` section of
`~/.omni-dev/settings.json` instead of per-invocation env vars or flags. All
three fields are optional; an absent `mcp` block leaves the built-in
behaviour unchanged.

```json
{
  "mcp": {
    "default_model": "claude-sonnet-4-6",
    "log_level": "info",
    "max_response_bytes": 102400
  }
}
```

| Field | Effect | Fallback |
|-------|--------|----------|
| `default_model` | Model for `ai_chat` when its `model` param is omitted | model registry default |
| `log_level` | Tracing filter directive for the server | `warn` (env `RUST_LOG` overrides) |
| `max_response_bytes` | Cap on a tool response before truncation (`0` disables) | 100 KB |

For troubleshooting (stderr logs, `RUST_LOG=debug`, "failed to open git
repository"), see [docs/mcp.md#troubleshooting](docs/mcp.md#troubleshooting).

### ⚙️ Configuration Commands

```bash
# Show supported AI models and their specifications
omni-dev config models show

# View model information with token limits and capabilities
omni-dev config models show | grep -A5 "claude-opus-4.1"
```

## 🧠 Contextual Intelligence

omni-dev understands your project context to provide better suggestions:

### Project Configuration

Create `.omni-dev/` directory in your repo root:

```bash
mkdir .omni-dev
```

#### Scope Definitions (`.omni-dev/scopes.yaml`)

```yaml
scopes:
  - name: "auth"
    description: "Authentication and authorization systems"
    examples: ["auth: add OAuth2 support", "auth: fix token validation"]
    file_patterns: ["src/auth/**", "auth.rs"]
  
  - name: "api"
    description: "REST API endpoints and handlers"  
    examples: ["api: add user endpoints", "api: improve error responses"]
    file_patterns: ["src/api/**", "handlers/**"]
```

#### Commit Guidelines (`.omni-dev/commit-guidelines.md`)

```markdown
# Project Commit Guidelines

## Format
- Use conventional commits: `type(scope): description`
- Keep subject line under 50 characters
- Use imperative mood: "Add feature" not "Added feature"

## Our Scopes
- `auth` - Authentication systems
- `api` - REST API changes
- `ui` - Frontend/UI components
```

## 🎯 Advanced Features

### Intelligent Context Detection

omni-dev automatically detects:

- **Project Conventions**: From `.omni-dev/`, `CONTRIBUTING.md`
- **Work Patterns**: Feature development, bug fixes, documentation,
  refactoring
- **Branch Context**: Extracts work type from branch names
  (`feature/auth-system`)
- **File Architecture**: Understands UI, API, core logic, configuration
  changes
- **Change Significance**: Adjusts detail level based on impact

### Automatic Batching

Large commit ranges are automatically split into manageable batches:

```bash
# Processes 50 commits in batches of 4 (default)
omni-dev git commit message twiddle 'HEAD~50..HEAD' --use-context

# Custom concurrency for very large ranges
omni-dev git commit message twiddle 'main..HEAD' --concurrency 2
```

### Command Options

| Option | Description | Example |
|--------|-------------|---------|
| `--fresh` | Generate fresh messages from the diffs alone (the default; conflicts with `--refine`) | `--fresh` |
| `--refine` | Refine the existing messages instead of starting fresh (conflicts with `--fresh`) | `--refine` |
| `--use-context` | Enable contextual intelligence | `--use-context` |
| `--work-context TEXT` | Describe the work being done to steer suggestions | `--work-context "feature: user auth"` |
| `--branch-context TEXT` | Override the context detected from the branch name | `--branch-context "bugfix: login flow"` |
| `--context-dir PATH` | Custom context directory | `--context-dir ./config` |
| `--model MODEL` | Claude API model to use (defaults from settings) | `--model claude-sonnet-4-5` |
| `--beta-header KEY:VALUE` | Beta header for API requests (model-gated) | `--beta-header key:value` |
| `--concurrency N` | Number of parallel commit processors (default: 4) | `--concurrency 3` |
| `--no-coherence` | Skip cross-commit coherence refinement pass | `--no-coherence` |
| `--no-ai` | Skip AI; output the repository analysis YAML only | `--no-ai` |
| `--auto-apply` | Apply without confirmation | `--auto-apply` |
| `--allow-pushed` | Allow amending commits already in remote main branches | `--allow-pushed` |
| `--check` | Validate the messages after applying | `--check` |
| `--save-only FILE` | Save to file without applying | `--save-only fixes.yaml` |
| `--quiet` | Only show errors/warnings | `--quiet` |

See the [User Guide's Key Options table](docs/user-guide.md#twiddle---ai-powered-improvement)
for the full reference; `omni-dev git commit message twiddle --help` is the
source of truth.

## 📖 Real-World Examples

### Before & After

**Before**: Messy commit history

```text
e4b2c1a fix stuff
a8d9f3e wip
c7e1b4f update files
9f2a6d8 more changes
```

**After**: Professional commit messages

```text
e4b2c1a feat(auth): implement JWT token validation system
a8d9f3e docs(api): add comprehensive OpenAPI documentation
c7e1b4f fix(ui): resolve mobile responsive layout issues
9f2a6d8 refactor(core): optimize database query performance
```

### Workflow Integration

```bash
# 1. Work on your feature branch
git checkout -b feature/user-dashboard

# 2. Make commits (don't worry about perfect messages)
git commit -m "wip"
git commit -m "fix stuff"
git commit -m "add more features"

# 3. Before merging, improve all commit messages
omni-dev git commit message twiddle 'main..HEAD' --use-context

# 4. Create professional PR with AI-generated description
omni-dev git branch create pr

# ✅ Professional commit history + comprehensive PR description ready for review
```

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

4. Run the build script (includes tests, linting, and formatting):

   ```bash
   ./scripts/build.sh
   ```

   Or run individual steps:

   ```bash
   cargo test         # Run tests
   cargo clippy       # Run linting
   cargo fmt          # Format code
   ```

## 📚 Documentation

- **[Getting Started](docs/getting-started.md)** - 10-minute walkthrough
  from install to first AI-improved commit (start here)
- **[User Guide](docs/user-guide.md)** - Comprehensive usage guide with examples
- **[Configuration Guide](docs/configuration.md)** - Set up contextual
  intelligence
- **[Why JFM?](docs/why-jfm.md)** - Why omni-dev edits Atlassian content as
  Markdown instead of raw ADF
- **[API Documentation](https://docs.rs/omni-dev)** - Rust API reference
- **[Troubleshooting](docs/troubleshooting.md)** - Common issues and
  solutions
- **[Examples](docs/examples.md)** - Real-world usage examples
- [Release Process](docs/RELEASE.md) - For contributors

## 🔧 Requirements

- **Rust**: 1.80+ (for installation from source)
- **Claude API Key**: Required for AI-powered features
  - See [Authentication](docs/configuration.md#authentication) for
    setup (env var, `.env`, or CI/CD secrets)
- **AI Model Selection**: Optional configuration for specific models
  - View available models: `omni-dev config models show`
  - Pick per-invocation with the global `--model` flag, or configure via
    `OMNI_DEV_MODEL` / the per-backend env chain (`CLAUDE_MODEL`,
    `CLAUDE_CODE_MODEL`, `ANTHROPIC_MODEL` for Claude-family backends;
    `OPENAI_MODEL`; `OLLAMA_MODEL`) or `~/.omni-dev/settings.json`
  - Supports standard identifiers and Bedrock-style formats
- **Atlassian Credentials** (for JIRA/Confluence features): Instance URL, email, and
  [API token](https://id.atlassian.com/manage-profile/security/api-tokens)
  - Configure with: `omni-dev atlassian auth login`
- **Datadog Credentials** (for Datadog features): API key, application key, and site
  - Configure with: `omni-dev datadog auth login`
- **Git**: Any modern version

### AI backend selection

omni-dev supports five AI backends. The global `--ai-backend` flag (or
`OMNI_DEV_AI_BACKEND`) selects one decisively — `default`, `claude-cli`,
`openai`, `ollama`, or `bedrock`:

- `--ai-backend claude-cli` — sandboxed `claude -p` subprocess that reuses
  your Claude Code session.
- `--ai-backend ollama` — local Ollama or LM Studio server.
- `--ai-backend openai` — OpenAI Chat Completions API.
- `--ai-backend bedrock` — AWS Bedrock.
- `--ai-backend default` *(or no flag)* — direct Anthropic API.

When `OMNI_DEV_AI_BACKEND` is unset, the legacy `USE_OLLAMA=true` /
`USE_OPENAI=true` / `CLAUDE_CODE_USE_BEDROCK=true` variables still select
their backends, in that order.

See the **[AI Backends Guide](docs/ai-backends.md)** for required env vars,
model selection, the Claude CLI sandbox and its escape hatches
(`--claude-cli-allow-tools`, `--claude-cli-allow-mcp`), the
`--claude-cli-max-budget-usd` spending cap, and per-backend troubleshooting.

## 🐛 Debugging

For troubleshooting and detailed logging, use the `RUST_LOG` environment variable:

```bash
# Enable debug logging for omni-dev components
RUST_LOG=omni_dev=debug omni-dev git commit message twiddle ...

# Debug specific modules (e.g., context discovery)  
RUST_LOG=omni_dev::claude::context::discovery=debug omni-dev git commit message twiddle ...

# Show only errors and warnings
RUST_LOG=warn omni-dev git commit message twiddle ...
```

See [Troubleshooting Guide](docs/troubleshooting.md) for detailed debugging information.

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for a list of changes in each version.

## License

This project is licensed under the BSD 3-Clause License - see the
[LICENSE](LICENSE) file for details.

## Support

- 📋 [Issues](https://github.com/rust-works/omni-dev/issues)
- 💬 [Discussions](https://github.com/rust-works/omni-dev/discussions)

## Acknowledgments

- Thanks to all contributors who help make this project better!
- Built with ❤️ using Rust
