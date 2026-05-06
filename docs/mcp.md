# MCP Server Reference

omni-dev ships an optional **Model Context Protocol** server, `omni-dev-mcp`,
that lets AI assistants (Claude Desktop, Claude Code, Cursor, the MCP
Inspector, custom agents) call omni-dev over stdio instead of shelling out to
the CLI. Tools and resources mirror the CLI surface so anything you can do
with `omni-dev` you can also do over MCP.

The server is delivered as a **second binary** alongside the regular
`omni-dev` CLI. See [ADR-0021](adrs/adr-0021.md) for the architectural
rationale. The default `cargo install omni-dev` build is unchanged — no MCP
dependencies are linked unless the `mcp` Cargo feature is enabled.

## Install

```bash
cargo install omni-dev --features mcp
```

This produces both `omni-dev` (the CLI) and `omni-dev-mcp` (the MCP server).

## Setup

### Claude Desktop

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

### Claude Code

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

### Smoke-test with the MCP Inspector

```bash
npx @modelcontextprotocol/inspector omni-dev-mcp
```

The Inspector opens a browser UI where you can list tools and resources, call
any tool interactively, and fetch resources against the working directory.

## Tool catalog

All tools serialise responses as YAML to match the CLI's `-o yaml` output.
Read tools accept an optional `output_file` parameter (see [Cross-cutting
parameters](#cross-cutting-parameters)). Destructive tools require an
explicit `confirm: true`.

### Git (5 tools)

| Tool | Purpose | CLI equivalent |
|------|---------|----------------|
| `git_branch_info` | Branch + remote + PR information | `omni-dev git branch info` |
| `git_check_commits` | Validate commit messages against guidelines | `omni-dev git commit message check` |
| `git_view_commits` | YAML commit analysis for a range | `omni-dev git commit message view` |
| `git_twiddle_commits` | AI-powered commit message improvement | `omni-dev git commit message twiddle` |
| `git_create_pr` | AI-drafted PR title + body, optionally pushed | `omni-dev git branch create pr` |

### JIRA — core (10 tools)

| Tool | Purpose |
|------|---------|
| `jira_read` | Fetch a single issue (JFM markdown or ADF JSON). Supports `output_file`, `--fields`, `--all-fields` |
| `jira_search` | JQL search; returns matching issues as YAML |
| `jira_create` | Create a new issue. Supports `set_field` for custom fields |
| `jira_write` | Update an issue body, `parent`, `assignee`, `reporter`, or arbitrary `fields`. At least one of `content` or another field is required |
| `jira_transition` | Apply or list workflow transitions (call with `list = true` first to discover names) |
| `jira_comment` | Add a comment to an issue |
| `jira_link` | Manage issue links: `create` (typed link), `parent` (set system parent), or list/remove |
| `jira_dev` | Fetch development info (commits, branches, PRs) attached to an issue |
| `jira_user_search` | Resolve a display name or email substring to an Atlassian `accountId` (call before `jira_write` for assignee/reporter) |
| `jira_delete` | Permanently delete an issue. Requires `confirm: true` |

### JIRA — extensions (18 tools)

Sprints, boards, watchers, worklogs, field metadata, attachments, project
listing, and changelog history.

| Family | Tools |
|--------|-------|
| Sprints | `jira_sprint_list`, `jira_sprint_issues`, `jira_sprint_add`, `jira_sprint_create`, `jira_sprint_update` |
| Boards | `jira_board_list`, `jira_board_issues` |
| Watchers | `jira_watcher_list`, `jira_watcher_add`, `jira_watcher_remove` |
| Worklogs | `jira_worklog_list`, `jira_worklog_add` |
| Fields | `jira_field_list`, `jira_field_options` |
| Attachments | `jira_attachment_download`, `jira_attachment_images` |
| Projects | `jira_project_list` |
| History | `jira_changelog` |

### Confluence (13 tools)

| Tool | Purpose |
|------|---------|
| `confluence_read` | Fetch a page (JFM or ADF). Supports `output_file` |
| `confluence_search` | CQL search |
| `confluence_create` | Create a new page |
| `confluence_write` | Replace a page's content |
| `confluence_delete` | Delete a page. Requires `confirm: true` |
| `confluence_download` | Recursive download of a page tree to disk |
| `confluence_children` | List direct children of a page |
| `confluence_comment_list` | List comments on a page |
| `confluence_comment_add` | Add a comment to a page |
| `confluence_label_list` | List labels on a page |
| `confluence_label_add` | Add one or more labels to a page |
| `confluence_label_remove` | Remove a label from a page |
| `confluence_user_search` | Resolve a display name or email to an Atlassian `accountId` |

### Atlassian — shared (2 tools)

| Tool | Purpose |
|------|---------|
| `atlassian_auth_status` | Boolean credential-presence flags only — never emits secret values |
| `atlassian_convert` | Bidirectional JFM ↔ ADF conversion (offline, no network) |

### Datadog (14 tools)

Read-only access to Datadog v1/v2 endpoints. Authentication uses
`DATADOG_API_KEY` + `DATADOG_APP_KEY` + `DATADOG_SITE` (or values stored via
`omni-dev datadog auth login`).

| Tool | Purpose |
|------|---------|
| `datadog_auth_status` | Credential-presence flags only |
| `datadog_metrics_query` | Point-in-time timeseries query (`/api/v1/query`) |
| `datadog_metrics_catalog_list` | List available metric names (`/api/v1/metrics`) |
| `datadog_monitor_list` | Filter monitors by name/tag/limit |
| `datadog_monitor_get` | Fetch a single monitor by id |
| `datadog_monitor_search` | Faceted monitor search |
| `datadog_dashboard_list` | List dashboards |
| `datadog_dashboard_get` | Fetch a single dashboard (widgets preserved as raw JSON) |
| `datadog_logs_search` | Logs search (`POST /api/v2/logs/events/search`); auto-paginates (default `limit=100`; `0` = all up to 10 000) |
| `datadog_events_list` | Events feed; auto-paginates (default `limit=100`; `0` = all up to 10 000) |
| `datadog_slo_list` | List SLOs (auto-paginates, hard cap 10 000) |
| `datadog_slo_get` | Fetch a single SLO by id |
| `datadog_hosts_list` | List active hosts |
| `datadog_downtime_list` | List downtimes; supports `active_only` |

### AI / Config (5 tools)

| Tool | Purpose |
|------|---------|
| `ai_chat` | One-shot chat with the configured Claude model |
| `claude_skills_sync` | Push omni-dev skills into the project's `.claude/skills/` |
| `claude_skills_clean` | Remove omni-dev-managed skills from `.claude/skills/` |
| `claude_skills_status` | Report which omni-dev skills are present and current |
| `config_models_show` | List supported AI models and token limits |

## Resources

The server exposes URI-addressable content alongside tools.

| URI template | Returns |
|--------------|---------|
| `git://repo/commits/{range}` | YAML commit analysis (mirrors `git_view_commits`) |
| `jira://issue/{key}` | JIRA issue body as JFM markdown |
| `jira://issue/{key}.adf` | JIRA issue body as ADF JSON |
| `confluence://page/{id}` | Confluence page as JFM markdown |
| `confluence://page/{id}.adf` | Confluence page body as ADF JSON |
| `omni-dev://specs/{name}` | Reference specifications embedded in the binary |

The currently shipped spec resource is `omni-dev://specs/jfm`, the
JIRA-Flavoured Markdown reference. AI clients should fetch this before
writing content for `jira_write`, `jira_create`, `jira_comment`,
`confluence_write`, or `confluence_create`.

## Cross-cutting parameters

### `output_file` on read tools

`confluence_read` and `jira_read` accept an optional `output_file` path. When
set, the rendered content is written to that path and the tool returns a
short YAML summary (`path`, `bytes`, `format`) instead of the inline body.
This prevents large pages from blowing past the assistant's context window —
the assistant can then page through the file with offset/limit using its
filesystem read tool. The same pattern is built into `confluence_download`
and `jira_attachment_download` by default.

### `confirm: true` on destructive tools

`jira_delete` and `confluence_delete` refuse to run unless the caller
explicitly passes `confirm: true`. This guards against accidental destruction
during exploratory tool calls.

### Per-call client construction

JIRA, Confluence, and Datadog tools build a fresh client per invocation, so
credential changes (e.g. `omni-dev datadog auth login`) take effect without
restarting the MCP server.

## Troubleshooting

- **Logs go to stderr.** MCP uses stdin/stdout for protocol framing, so
  tracing output is routed to stderr — tail your client's MCP log pane or
  run the binary in a terminal to see it.
- **Verbose tracing:** `RUST_LOG=debug omni-dev-mcp` turns on debug-level
  logs. Module-scoped filters work too, e.g.
  `RUST_LOG=omni_dev::mcp=trace`.
- **"Failed to open git repository":** the assistant runs `omni-dev-mcp` with
  its own working directory. Tools that open a git repository use that
  directory unless an explicit `repo_path` parameter (or a resource URI
  placing you elsewhere) overrides it. Confirm the assistant launched the
  server from inside the repo you expected.
- **Tool not found:** confirm the binary was built with `--features mcp`.
  `omni-dev-mcp --help` should print without error if the build succeeded.
- **Atlassian / Datadog tools return auth errors:** run the matching
  `*_auth_status` tool first to confirm credentials are visible to the
  process. Environment variables exported in your shell are not inherited
  unless the MCP client launched the server from that shell.

## See also

- [ADR-0021](adrs/adr-0021.md) — MCP server via second binary
- [JIRA-Flavoured Markdown spec](specs/jfm.md) — also served as
  `omni-dev://specs/jfm`
- [User Guide — Atlassian Integration](user-guide.md#atlassian---jira-and-confluence-integration)
- [User Guide — Datadog Integration](user-guide.md#datadog-integration)
