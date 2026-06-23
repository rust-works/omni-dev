# Why JFM? The Advantage over Raw ADF

omni-dev reads, edits, and writes JIRA issues and Confluence pages as
**JFM** — JIRA-Flavored Markdown, a Markdown dialect that round-trips
losslessly with Atlassian Document Format (ADF). This page explains why
that layer exists and what it buys you over working with raw ADF JSON
directly — the format the Confluence REST API and the official Atlassian
(Rovo) MCP server expose.

For the syntax itself, see the [JFM specification](specs/jfm.md). For the
design decisions, see [ADR-0020](adrs/adr-0020.md) (the dialect) and
[ADR-0029](adrs/adr-0029.md) (the converter strategy).

## The problem with editing raw ADF

ADF is a deeply nested JSON tree. A single Confluence page — a panel, a
table, a couple of lists — is easily thousands of lines of JSON. To *edit*
such a page through a raw-ADF interface, an agent must emit the **entire**
tree back, including every node, attribute, and mark it did not intend to
touch. Two things go wrong:

- **Verbosity.** The same content is several times larger in ADF than in
  Markdown — more tokens to read, more to emit, slower, and far harder to
  diff or review.
- **No preservation guarantee.** Structural details the author isn't even
  thinking about — panel types, layout columns, status lozenges, and
  especially **inline review-comment anchors** — live in the tree and must
  be reproduced exactly or they are silently dropped on the next write.

JFM removes both problems: the human or model edits a compact, readable
Markdown document while a **deterministic converter** owns the ADF.

## The advantages

### 1. A deterministic, lossless round-trip

JFM ↔ ADF conversion is performed by a deterministic converter, not by the
language model. omni-dev reads a page as JFM, you (or an agent) edit the
Markdown, and the converter rebuilds the ADF. Every node it understands —
the full vendored `@atlaskit/adf-schema` node set — round-trips, and any
node it does not recognise is preserved verbatim through an
unsupported-node escape hatch (see [ADR-0029](adrs/adr-0029.md)). The
fidelity is a property of the code, not of how carefully the model
reproduced JSON.

### 2. Anchored review comments survive edits

Inline (anchored) review comments are stored in the page body as
`annotation` marks wrapping the commented text. JFM surfaces each as a
bracketed span:

```text
[anchored text]{annotation-id="…" annotation-type=inlineComment}
```

so the anchor travels with the text through an edit. Rewrite the
surrounding prose and the comment stays attached to exactly the right
words. Interfaces that flatten the body to plain Markdown drop the anchor
and orphan the comment; raw-ADF interfaces preserve it only if the model
reproduces the mark exactly, every time.

### 3. Far fewer tokens

For an identical, moderately complex page (heading, panel, table, lists,
task list, code block, inline marks), the JFM form is **several times
smaller** than the equivalent ADF JSON — roughly 4–10× fewer bytes
depending on formatting. Fewer tokens to read and emit means lower cost,
lower latency, and edits a human can actually review in a diff.

### 4. Human- and model-readable

JFM is Markdown. It renders in any editor, reviews cleanly in a pull
request, and is far easier for both people and language models to reason
about than a wall of nested JSON.

### 5. Pre-flight schema validation

Before any write, omni-dev validates the resulting ADF against a
data-driven content-model schema — node nesting and arity — and rejects an
invalid document with an actionable error rather than letting the API fail
opaquely (see [ADR-0023](adrs/adr-0023.md) and
[ADR-0025](adrs/adr-0025.md)).

### 6. Offline conversion, no credentials

`omni-dev atlassian convert to-adf` / `from-adf` (and the
`atlassian_convert` MCP tool) convert in either direction with no network
and no Atlassian credentials — useful for authoring, testing, and CI.

## What JFM is *not* claiming

JFM's value is **not** that language models cannot write ADF. Current
models author valid ADF directly with high reliability when constructing a
document from scratch. The advantage is narrower and more durable:

- **Guarantee, not best effort.** On an *edit*, the converter guarantees
  that untouched structure — panels, layouts, comment anchors — survives.
  Raw ADF puts that burden on the model to re-emit the whole tree correctly
  every time; JFM removes the burden entirely.
- **Efficiency and reviewability.** Even when a model produces correct ADF,
  the Markdown form is smaller, cheaper, and reviewable.

In short: raw ADF *can* be correct; JFM is correct **by construction**, and
cheaper and clearer while it is at it.

## See also

- [JFM specification](specs/jfm.md) — the syntax and the full ADF node
  coverage.
- [ADR-0020](adrs/adr-0020.md) — JFM as a Markdown dialect for bidirectional
  ADF interchange.
- [ADR-0029](adrs/adr-0029.md) — the JFM ↔ ADF converter strategy.
- The "How omni-dev Compares" tables in the
  [README](../README.md#-how-omni-dev-compares).
