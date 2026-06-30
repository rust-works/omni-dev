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

Every tool's `description` and parameter doc comments follow the checklist in
[STYLE-0029](STYLE_GUIDE.md#style-0029-mcp-tool--parameter-description-checklist):
a one-line summary, concrete example values, allowed enum values, the expected
wire format, directional semantics where order matters, and a `Mirrors
\`omni-dev <subcommand>\`` cross-reference. Each equivalent CLI subcommand
carries the reverse reference (`mirrors the \`<tool>\` MCP tool`) in its
`--help`, so the two surfaces stay discoverable from each other.

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
| `jira_create` | Create a new issue. Supports `custom_fields` (a `{name-or-id: value}` map resolved against the create screen) for fields a project requires at create time |
| `jira_bulk_create` | Create many issues and (optionally) wire dependency links between them in one call — for epic decomposition. See [Bulk create + link](#bulk-create--link-jira_bulk_create) |
| `jira_write` | Update an issue body, `assignee`, `reporter`, or arbitrary `fields`. At least one of `content` or another field is required. (Set the parent for hierarchy via the `jira_link_parent` tool.) |
| `jira_transition` | Apply or list workflow transitions (call with `list = true` first to discover names) |
| `jira_comment` | Add a comment to an issue |
| `jira_dev` | Fetch development info (commits, branches, PRs) attached to an issue |
| `jira_user_search` | Resolve a display name or email substring to an Atlassian `accountId` (call before `jira_write` for assignee/reporter) |
| `jira_delete` | Permanently delete an issue. Requires `confirm: true` |

### JIRA — extensions (25 tools)

Sprints, boards, watchers, worklogs, links, field metadata, attachments,
project listing and create-screen introspection, and changelog history.

| Family | Tools |
|--------|-------|
| Sprints | `jira_sprint_list`, `jira_sprint_issues`, `jira_sprint_add`, `jira_sprint_create`, `jira_sprint_update` |
| Boards | `jira_board_list`, `jira_board_issues` |
| Watchers | `jira_watcher_list`, `jira_watcher_add`, `jira_watcher_remove` (requires `confirm: true`) |
| Worklogs | `jira_worklog_list`, `jira_worklog_add` |
| Links | `jira_link_list`, `jira_link_types`, `jira_link_create`, `jira_link_parent`, `jira_link_remove` (requires `confirm: true`), `jira_link_remote_list` — one tool per `omni-dev atlassian jira link` subcommand |
| Fields | `jira_field_list`, `jira_field_options` (custom-field discovery — see [user guide](user-guide.md#jira-fields)) |
| Attachments | `jira_attachment_download`, `jira_attachment_images` |
| Projects | `jira_project_list`, `jira_project_create_meta` (pre-flight required/allowed fields — see [user guide](user-guide.md#jira-fields)) |
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
| `confluence_label_remove` | Remove a label from a page. Requires `confirm: true` |
| `confluence_user_search` | Resolve a display name or email to an Atlassian `accountId` |
| `confluence_compare` | Structural diff between two versions of a page. `detail`: `summary`, `outline` (default), or `full`. Returns drill-in cursors. See [user guide](user-guide.md#confluence-comparing-pages) |
| `confluence_compare_section` | Drill into a single section delta via a cursor returned from `confluence_compare`. `format`: `unified`, `side_by_side`, `markdown_inline` |

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
| `ai_chat` | One-shot chat with the configured Claude model. Supports `system_prompt` override (CLI doesn't). See [user guide](user-guide.md#ai-chat--conversational-ai) |
| `claude_skills_sync` | Push omni-dev skills into the project's `.claude/skills/` via symlinks. See [user guide](user-guide.md#ai-claude-skills--distribute-skills-across-repositories) |
| `claude_skills_clean` | Remove omni-dev-managed skill symlinks and exclude-block entries |
| `claude_skills_status` | Report which omni-dev skill symlinks are present and current |
| `config_models_show` | List supported AI models and token limits |

## Resources

The server exposes URI-addressable content alongside tools.

| URI template | Returns |
|--------------|---------|
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

### Bulk create + link (`jira_bulk_create`)

`jira_bulk_create` collapses an epic-decomposition workflow — create N child
issues, then wire N dependency links between them — into a single tool call,
saving the per-call model round-trips an N-step loop would cost.

- **`issues`**: an array of specs (`project`, `summary`, optional `description`
  in JFM, `issue_type` defaulting to `Task`, and an optional local `alias`).
  Created in order. May be empty to only link existing issues.
- **`links`**: an optional array of `{ link_type, inward, outward }`. Each
  endpoint is resolved **alias-first, then as a literal key**: a string
  matching an `alias` minted in this batch uses that issue's new key; otherwise
  it is treated as an existing issue key. (So don't reuse a real key as an
  alias.) Links are created after all issues.
- **`fail_fast`** (default `false`): when `false`, every record is attempted
  (continue-on-error); when `true`, the first failed create or link stops all
  further calls.

The tool returns a YAML report — `issues[]` (`alias`, `ok`, `key`, `self_url`
or `error`), `links[]` (`ok` or `error`, including
`skipped: alias "…" was not created` when an endpoint's create failed), and a
`summary` (`issues_created`, `issues_failed`, `links_created`, `links_failed`,
`stopped_early`).

**No rollback.** JIRA has no cross-call transaction, so nothing is undone on
failure — `fail_fast` only stops issuing *further* calls. The report always
lists exactly which issues/links succeeded, so a retry can re-send only the
remainder (referencing already-created issues by their returned keys) without
creating duplicates.

### `dry_run: true` on mutating tools

The mutating Atlassian tools accept an optional `dry_run` flag, mirroring the
CLI's `--dry-run`:

- `jira_create`
- `jira_write`
- `jira_link_create`
- `jira_link_parent`
- `jira_link_remove`
- `confluence_create`
- `confluence_write`

When `dry_run: true`, the tool performs all local resolution and ADF
validation (so malformed JFM/ADF still errors) but stops short of the network
call, returning the would-be HTTP request as YAML — `dry_run: true`, the
`method` and `path`, and the request `body` (for JIRA, the exact
`{"fields": {…}}` payload; for Confluence, the caller-supplied fields plus the
resolved ADF, with server-resolved values like the numeric space id and the
next version number filled in at send time). Use it to validate required fields
and formatting before committing an irreversible mutation. For `jira_link_remove`,
a dry-run previews the `DELETE` without requiring the destructive `confirm`
guard. See [issue #1048](https://github.com/rust-works/omni-dev/issues/1048).

### `confirm: true` on destructive tools

Five destructive Atlassian tools refuse to run unless the caller explicitly
passes `confirm: true`:

- `jira_delete`
- `jira_link_remove`
- `jira_watcher_remove`
- `confluence_delete`
- `confluence_label_remove`

Each returns an error message of the form `Refusing to <verb> <target>: pass
`confirm: true` to authorise this destructive operation.` when called without
the parameter. This guards against accidental destruction during exploratory
tool calls. See the [Destructive Commands callout](user-guide.md#destructive-commands)
in the user guide and [ADR-0027](adrs/adr-0027.md) for the design rationale.

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
  directory unless an explicit `repo_path` parameter overrides it. Confirm the
  assistant launched the server from inside the repo you expected.
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
