# JFM (JIRA-Flavored Markdown) Specification

## Overview

JFM provides bidirectional conversion between Markdown and Atlassian Document
Format (ADF), enabling JIRA Cloud issues and Confluence Cloud pages to be
read, edited, and updated as local markdown files. It integrates with the
JIRA Cloud REST API v3 and Confluence Cloud REST API v2 for fetching and
pushing content.

## Architecture

```
┌──────────────┐     ┌───────────────┐     ┌──────────────────┐
│  CLI Layer   │────▶│  JFM Library  │────▶│  JIRA Cloud API  │
│  (cli/jfm/)  │     │  (src/jfm/)   │     │  REST API v3     │
└──────────────┘     └───────┬───────┘     └──────────────────┘
                             │
                    ┌────────┼────────┐
                    │        │        │
              ┌─────┴──┐ ┌──┴───┐ ┌──┴──────────────────┐
              │Document│ │ MD ↔ │ │ Confluence Cloud API │
              │ Parser │ │  ADF │ │   REST API v2        │
              └────────┘ └──────┘ └─────────────────────┘
```

### Module Structure

| Module           | File                | Purpose                                      |
|------------------|---------------------|----------------------------------------------|
| `adf`            | `adf.rs`            | ADF type definitions and node constructors    |
| `api`            | `api.rs`            | `AtlassianApi` trait, `ContentItem`, `ContentMetadata` |
| `attrs`          | `attrs.rs`          | Pandoc-style attribute parsing (`{key=val}`)  |
| `auth`           | `auth.rs`           | Credential load/save from settings file       |
| `client`         | `client.rs`         | Shared Atlassian HTTP transport (Basic Auth)  |
| `confluence_api` | `confluence_api.rs` | Confluence REST API v2 implementation         |
| `convert`        | `convert.rs`        | Bidirectional Markdown ↔ ADF conversion       |
| `directive`      | `directive.rs`      | Generic directive parsing (inline/leaf/container) |
| `document`       | `document.rs`       | JFM document format (frontmatter + body)      |
| `error`          | `error.rs`          | Error types for the JFM subsystem             |
| `jira_api`       | `jira_api.rs`       | JIRA REST API v3 implementation               |
| `target`         | `target.rs`         | Target resolution (auto-detect JIRA/Confluence)|

## JFM Document Format

A JFM document consists of YAML frontmatter followed by a markdown body,
separated by `---` delimiters. The `type` field in the frontmatter
discriminates between JIRA and Confluence content.

### JIRA Issue

```markdown
---
type: jira
instance: https://myorg.atlassian.net
key: PROJ-123
summary: Issue title here
status: In Progress
issue_type: Story
assignee: Alice Smith
priority: High
labels:
  - backend
  - auth
---

Markdown body content describing the issue.
```

### Confluence Page

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

Page body content here.
```

### JIRA Frontmatter Fields

| Field        | Required | Description                              |
|--------------|----------|------------------------------------------|
| `type`       | Yes      | Always `"jira"`                          |
| `instance`   | Yes      | Atlassian Cloud instance URL             |
| `key`        | Yes      | JIRA issue key (e.g., `PROJ-123`)        |
| `summary`    | Yes      | Issue title/summary                      |
| `status`     | No       | Issue status (read-only from JIRA)       |
| `issue_type` | No       | Issue type (Bug, Story, Task, etc.)      |
| `assignee`   | No       | Assigned user display name               |
| `priority`   | No       | Issue priority level                     |
| `labels`     | No       | List of issue labels                     |

### Confluence Frontmatter Fields

| Field        | Required | Description                              |
|--------------|----------|------------------------------------------|
| `type`       | Yes      | Always `"confluence"`                    |
| `instance`   | Yes      | Atlassian Cloud instance URL             |
| `page_id`    | Yes      | Confluence page ID                       |
| `title`      | Yes      | Page title                               |
| `space_key`  | Yes      | Space key (e.g., `ENG`)                  |
| `status`     | No       | Page status (`"current"` or `"draft"`)   |
| `version`    | No       | Page version number (for optimistic locking) |
| `parent_id`  | No       | Parent page ID                           |

### Issue Key Validation

Issue keys must match the pattern `^[A-Z][A-Z0-9]+-\d+$`:
- Starts with an uppercase letter
- Followed by uppercase letters or digits
- A hyphen
- One or more digits

### Parsing Rules

- Frontmatter must begin at the first line with exactly `---`
- Frontmatter ends at the next `---` on its own line
- The body may safely contain `---` (only the first occurrence after the
  opening delimiter closes the frontmatter)
- Empty body is valid
- Trailing newlines are preserved
- Optional fields omitted from YAML when `None` or empty

## Atlassian Document Format (ADF)

ADF is JIRA's native rich-text format. JFM converts between markdown and
ADF v1.

### ADF Structure

```json
{
  "version": 1,
  "type": "doc",
  "content": [
    {
      "type": "paragraph",
      "content": [
        { "type": "text", "text": "Hello " },
        { "type": "text", "text": "world", "marks": [{ "type": "strong" }] }
      ]
    }
  ]
}
```

### Supported Block Nodes

| ADF Node Type     | Markdown Equivalent                         |
|-------------------|---------------------------------------------|
| `heading`         | `# H1` through `###### H6`                 |
| `paragraph`       | Plain text                                  |
| `codeBlock`       | Fenced code blocks (`` ``` ``)              |
| `bulletList`      | `- item` or `* item`                        |
| `orderedList`     | `1. item`                                   |
| `taskList`        | `- [ ] todo` / `- [x] done`                |
| `blockquote`      | `> text`                                    |
| `rule`            | `---`, `***`, or `___`                      |
| `table`           | Pipe-delimited tables with `\|---\|` separator |
| `mediaSingle`     | `![alt](url)`                               |
| `panel`           | `:::panel{type=info}` container directive   |
| `expand`          | `:::expand{title=...}` container directive  |
| `layoutSection`   | `:::layout` with `:::column` children       |
| `decisionList`    | Container with decision items               |
| `extension`       | Leaf or bodied extension blocks             |

### Supported Inline Nodes and Marks

| ADF Type      | Markdown Equivalent                         |
|---------------|---------------------------------------------|
| `strong`      | `**bold**`                                  |
| `em`          | `*italic*`                                  |
| `code`        | `` `code` ``                                |
| `strike`      | `~~strikethrough~~`                         |
| `link`        | `[text](url)`                               |
| `emoji`       | `:shortcode:` (e.g., `:smile:`)             |
| `status`      | `:status[text]{color=...}`                  |
| `date`        | `:date[2026-04-15]`                         |
| `mention`     | `:mention[Name]{id=...}`                    |
| `hardBreak`   | Literal newline                             |
| `underline`   | Bracketed span: `[text]{underline}`         |
| `textColor`   | Bracketed span: `[text]{color=#hex}`        |
| `subsup`      | Mark for subscript/superscript              |

### Unsupported Node Handling

ADF nodes that cannot be represented in markdown are serialized as fenced
code blocks with language `adf-unsupported`:

````markdown
```adf-unsupported
{"type":"unknownNode","attrs":{"key":"value"}}
```
````

On conversion back to ADF, these blocks are deserialized and restored to
their original ADF structure, enabling lossless round-trips for unsupported
content.

## Generic Directive System

JFM uses the CommonMark Generic Directives proposal to represent ADF-specific
constructs that have no native markdown equivalent. Three directive levels
are supported:

### Inline Directives

Syntax: `:name[content]{attrs}`

Used for inline semantic elements within text:

```markdown
The status is :status[In Progress]{color=blue} and assigned to
:mention[Alice]{id=abc123}.

The deadline is :date[2026-04-15].
```

- Content in `[...]` is **required**
- Attributes in `{...}` are optional
- Name must be alphabetic characters and hyphens

### Leaf Block Directives

Syntax: `::name[content]{attrs}`

Used for standalone block-level elements:

```markdown
::card[https://example.com/page]{width=80}
```

- Exactly two colons (not three)
- Content in `[...]` is optional
- Must occupy its own line

### Container Directives

Syntax: `:::name{attrs}` ... `:::`

Used for block-level containers wrapping other content:

```markdown
:::panel{type=info}
This is an informational panel with **rich** content.

- Item one
- Item two
:::
```

- Three or more colons to open
- Closed by matching colon count with no name
- Content between open/close is parsed as markdown
- Attributes are optional

### Attribute Syntax

Attributes follow Pandoc-style `{key=value flag}` syntax:

```
{type=info}                          # simple key-value
{color="bright red"}                 # quoted value with spaces
{bg=#DEEBFF numbered}               # mixed key-value and flag
{title="Click to expand"}           # quoted string
{params='{"jql":"project=PROJ"}'}   # single-quoted JSON value
```

- Keys: alphanumeric, hyphens, underscores
- Values: unquoted (stop at whitespace/`}`) or quoted (single/double)
- Flags: bare words treated as boolean true
- Round-trip safe: `parse → render → parse` preserves structure

## Markdown ↔ ADF Conversion

### Markdown to ADF

The converter uses a line-oriented parser that processes blocks in order:

1. Headings (`# ` through `###### `)
2. Horizontal rules (`---`, `***`, `___`)
3. Container directives (`:::name{attrs}` ... `:::`)
4. Fenced code blocks (`` ``` ``)
5. Tables (pipe-delimited with separator row)
6. Blockquotes (`> `)
7. Lists (`- `, `* `, `1. `, `- [ ] `, `- [x] `)
8. Leaf directives (`::name[content]{attrs}`)
9. Images (`![alt](url)`)
10. Paragraphs (default fallback)

Inline content within paragraphs is parsed for:
- Bold, italic, code, strikethrough
- Links and bare URLs
- Inline directives (status, date, mention, emoji)
- Bracketed spans with attributes (`[text]{color=red}`)

### ADF to Markdown

Block nodes are rendered to their markdown equivalents. Inline nodes
have marks applied (bold, italic, etc.) and semantic nodes render as
directives.

### Block Attributes

Block-level attributes can follow a block on a separate line:

```markdown
# Section Title
{align=center breakout=wide}
```

Supported attributes: `align`, `indent`, `breakout`.

## Atlassian Cloud API Integration

### Authentication

- **Method**: HTTP Basic Auth (base64-encoded `email:api_token`)
- **Credential storage**: `~/.omni-dev/settings.json` in the `env` map
- **Required keys**:
  - `ATLASSIAN_INSTANCE_URL`
  - `ATLASSIAN_EMAIL`
  - `ATLASSIAN_API_TOKEN`
- Same credentials serve both JIRA and Confluence (same Atlassian instance)

### Client Architecture

The `AtlassianClient` struct provides shared HTTP transport (auth headers,
timeouts). Backend-specific logic is in `JiraApi` and `ConfluenceApi`, both
implementing the `AtlassianApi` trait. Auto-detection in `target.rs`
selects the correct backend based on the identifier pattern.

### JIRA API Endpoints

| Operation       | Method | Endpoint                         |
|-----------------|--------|----------------------------------|
| Fetch issue     | GET    | `/rest/api/3/issue/{key}`        |
| Update issue    | PUT    | `/rest/api/3/issue/{key}`        |
| Verify auth     | GET    | `/rest/api/3/myself`             |

### Confluence API Endpoints

| Operation       | Method | Endpoint                                            |
|-----------------|--------|-----------------------------------------------------|
| Fetch page      | GET    | `/wiki/api/v2/pages/{id}?body-format=atlas_doc_format` |
| Update page     | PUT    | `/wiki/api/v2/pages/{id}`                           |
| Fetch space     | GET    | `/wiki/api/v2/spaces/{id}`                          |

### Confluence Update Details

- Page updates require an incremented `version.number`
- Current version is fetched before writing (optimistic locking)
- ADF is sent as a JSON string in `body.value` with
  `body.representation = "atlas_doc_format"`

### Configuration

- HTTP timeout: 30 seconds
- Content-Type: `application/json`
- Instance URL: HTTPS, trailing slash normalized

## Error Types

| Error                  | Cause                                        |
|------------------------|----------------------------------------------|
| `CredentialsNotFound`  | No credentials in `~/.omni-dev/settings.json`|
| `ApiRequestFailed`     | HTTP error from JIRA (includes status + body)|
| `InvalidDocument`      | JFM parse error (bad YAML, missing delimiters)|
| `ConversionError`      | ADF conversion failure                       |

## Target Auto-Detection

The `<ID>` argument is auto-detected based on pattern:

| Pattern                    | Target     | Example      |
|----------------------------|------------|--------------|
| `[A-Z][A-Z0-9]+-\d+`      | JIRA       | `PROJ-123`   |
| `^\d+$`                    | Confluence | `12345`      |
| Other                      | Error      | `My Page`    |

Explicit flags `--jira` and `--confluence` override auto-detection.

## CLI Commands

| Command                            | Purpose                              |
|------------------------------------|--------------------------------------|
| `omni-dev jfm auth login`         | Configure Atlassian credentials      |
| `omni-dev jfm auth status`        | Verify authentication                |
| `omni-dev jfm read <ID>`          | Fetch content as JFM markdown        |
| `omni-dev jfm write <ID> [FILE]`  | Push JFM markdown to JIRA/Confluence |
| `omni-dev jfm edit <ID>`          | Interactive fetch-edit-push cycle    |
| `omni-dev jfm convert to-adf`     | Convert markdown to ADF JSON         |
| `omni-dev jfm convert from-adf`   | Convert ADF JSON to markdown         |

All read/write/edit commands accept `--jira` or `--confluence` flags to
override auto-detection.

See the [User Guide](../user-guide.md) for detailed command usage and
examples.

## Data Flow

### Read

```
resolve_target(ID) → create_api_for_target() → api.get_content()
→ ContentItem → content_item_to_document() → render() → stdout/file
```

### Write

```
file/stdin → JfmDocument::parse() → markdown_to_adf()
→ resolve_target(ID) → create_api_for_target() → api.update_content()
```

### Edit

```
resolve_target(ID) → api.get_content() → JFM doc → temp file
→ [edit loop: show/edit/accept/quit] → api.update_content() if changed
```

### Convert (local, no auth)

```
markdown → markdown_to_adf() → ADF JSON    (to-adf)
ADF JSON → adf_to_markdown() → markdown    (from-adf)
```
