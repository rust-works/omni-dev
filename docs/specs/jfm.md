# JFM (JIRA-Flavored Markdown) Specification

## Overview

JFM provides bidirectional conversion between Markdown and Atlassian Document
Format (ADF), enabling JIRA Cloud issues and Confluence Cloud pages to be
read, edited, and updated as local markdown files.

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
| `key`        | No       | JIRA issue key (e.g., `PROJ-123`). Absent when creating a new issue. |
| `project`    | No       | Project key (e.g., `PROJ`). Used for issue creation when `key` is absent. |
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
| `page_id`    | No       | Confluence page ID. Absent when creating a new page. |
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
| `annotation`  | Bracketed span: `[text]{annotation-id=... annotation-type=...}` |

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
- Round-trip safe: `parse -> render -> parse` preserves structure

## Markdown to ADF Conversion

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
- Bracketed spans with attributes (`[text]{color=red}`, `[text]{annotation-id=...}`)

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

### Inline Attribute Marks

Bracketed spans `[text]{attrs}` represent inline marks that have no native
markdown syntax. Multiple attributes can be combined in a single span.

#### Underline

```markdown
[underlined text]{underline}
```

#### Annotation (Inline Comments)

Confluence inline comments attach an `annotation` mark to highlighted text.
The mark links the text span to a comment thread stored in Confluence's
comment system. JFM preserves these marks for round-trip fidelity:

```markdown
[highlighted text]{annotation-id="abc123" annotation-type=inlineComment}
```

- `annotation-id`: the annotation identifier (required)
- `annotation-type`: the annotation type, typically `inlineComment` (required)
- Annotations can coexist with other marks (bold, italic, etc.):
  `[**bold comment**]{annotation-id="abc123" annotation-type=inlineComment}`

## Authentication

- **Method**: HTTP Basic Auth (base64-encoded `email:api_token`)
- **Credential sources** (checked in order):
  1. Environment variables
  2. `~/.omni-dev/settings.json` `env` map
- **Required keys**:
  - `ATLASSIAN_INSTANCE_URL`
  - `ATLASSIAN_EMAIL`
  - `ATLASSIAN_API_TOKEN`
- Same credentials serve both JIRA and Confluence (same Atlassian instance)

## Error Types

| Error                  | Cause                                        |
|------------------------|----------------------------------------------|
| `CredentialsNotFound`  | No credentials configured                    |
| `ApiRequestFailed`     | HTTP error from API (includes status + body) |
| `InvalidDocument`      | JFM parse error (bad YAML, missing delimiters)|
| `ConversionError`      | ADF conversion failure                       |
