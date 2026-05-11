# Extending the ADF schema

omni-dev round-trips between Atlassian Document Format (ADF) and JIRA-
Flavoured Markdown (JFM, see [ADR-0020](../adrs/adr-0020.md)). Three layers
have to agree for a new ADF node type to work end-to-end:

1. **Vendored upstream schema** at [`assets/adf-schema/`](../../assets/adf-schema/) — the JSON snapshot from `@atlaskit/adf-schema` npm.
2. **Hand-maintained quantifier table** at [`src/atlassian/adf_schema/mod.rs`](../../src/atlassian/adf_schema/mod.rs) — encodes arity (`+`, `*`, `?`) the upstream JSON loses ([ADR-0023](../adrs/adr-0023.md)).
3. **JFM converter and spec** at [`src/atlassian/convert.rs`](../../src/atlassian/convert.rs) and [`docs/specs/jfm.md`](../specs/jfm.md) — define how the node renders to/from markdown.

This recipe walks you through adding a new node type (we'll use
`inlineCard` as the running example) and wiring it into all three layers.

## Files you'll touch

| File | Edit |
|---|---|
| [`src/atlassian/adf.rs`](../../src/atlassian/adf.rs) | Add `impl AdfNode { pub fn inline_card(...) }` constructor. |
| [`src/atlassian/convert.rs`](../../src/atlassian/convert.rs) | Add a parse arm in `try_container_directive` or `try_leaf_directive`; add `from-adf` rendering. |
| [`src/atlassian/adf_schema/mod.rs`](../../src/atlassian/adf_schema/mod.rs) | Add a `CONTENT_ENTRIES` tuple for the new parent (if any) and add the atom to allowed-children sets where it can legally appear. |
| [`docs/specs/jfm.md`](../specs/jfm.md) | Add a row to *Supported Block Nodes* or *Supported Inline Nodes* (around lines 120–178). |
| [`tests/adf_schema_test.rs`](../../tests/adf_schema_test.rs) | Add validator tests for round-trip and nesting rules. |

If the upstream `@atlaskit/adf-schema` package has been refreshed and your
node is newly present in `full.json`, you may also need to **re-pin and
regenerate** (see *Refreshing the vendored schema* below).

## Walkthrough

### 1. Node constructor

Add an ergonomic constructor to
[`src/atlassian/adf.rs`](../../src/atlassian/adf.rs) so call sites
elsewhere don't have to assemble the JSON by hand. Follow the pattern of
existing constructors on `AdfNode` — same field-shape, same defaults.

### 2. JFM converter

The converter routes container directives (`:::name`) and leaf directives
(`::name`) into AST nodes. Entry points:

- [`MarkdownParser::try_container_directive`](../../src/atlassian/convert.rs#L505) — block-level `:::` syntax.
- [`MarkdownParser::try_leaf_directive`](../../src/atlassian/convert.rs#L865) — leaf `::` syntax for inline-ish blocks.
- Inline marks/nodes are parsed elsewhere in the same file; grep for the
  pattern that matches the closest existing node type.

Conversion is **bidirectional**: also add a rendering arm in the `from-adf`
path so an `inlineCard` node round-trips back to its JFM directive. Inline
test modules nearby (`nested_expand_inside_panel`, etc.) show the
roundtrip-assertion pattern.

### 3. Content model — `CONTENT_ENTRIES`

[`src/atlassian/adf_schema/mod.rs:597`](../../src/atlassian/adf_schema/mod.rs#L597)
holds the alphabetically-sorted list of `(parent, &[ContentTerm])` tuples.
Two edits, usually:

- **If your node is a parent** (it has children), add a tuple defining the
  allowed atoms and quantifier:

  ```rust
  // myNewNode — definitions/myNewNode_node
  // upstream: paragraph+
  (
      "myNewNode",
      &[ContentTerm {
          atoms: &["paragraph"],
          quant: Quantifier::OneOrMore,
      }],
  ),
  ```

  `Quantifier` variants are `OneOrMore`, `ZeroOrMore`, `ZeroOrOne`, `Exactly`.
  Keep the tuple alphabetised by parent name.

- **If your node is a child** (it can appear inside other nodes), add the
  atom to each parent's `atoms` array that should permit it. Walk the
  upstream JSON schema definition (in
  [`assets/adf-schema/full.json`](../../assets/adf-schema/full.json)) and
  copy what it says — don't guess.

The hand-maintained table is reconciled against the upstream JSON by
[`tests/adf_schema_drift_bin_test.rs`](../../tests/adf_schema_drift_bin_test.rs)
— if you miss a parent or add an atom upstream doesn't allow, that test
fails. Inline comments in `mod.rs` (search for "LENIENT") flag a small
allowlist of deliberate deviations; mirror that style if you have a
documented reason to diverge.

### 4. JFM spec

Add a row in [`docs/specs/jfm.md`](../specs/jfm.md) under *Supported Block
Nodes* (lines 120–144) or *Supported Inline Nodes* (lines 145–158). Each
row names the ADF type, the JFM directive syntax, and any notable
constraints. If your node has unusual nesting rules, add a short note
under *Common pitfalls* (around line 244).

The spec is **served as an MCP resource** at `omni-dev://specs/jfm` — see
[`src/mcp/specs.rs`](../../src/mcp/specs.rs). It's `include_str!`'d at
compile time, so your change ships in the same binary that supports the
new node.

### 5. Tests

Three layers, three tests:

1. **Schema validator test** in
   [`tests/adf_schema_test.rs`](../../tests/adf_schema_test.rs) — assert
   that an `inlineCard` inside a disallowed parent is flagged
   (`DisallowedChild` violation) and that an `inlineCard` inside an
   allowed parent passes. Look at `expand_inside_panel_is_flagged_via_public_api`
   for the shape.
2. **Round-trip test** as an inline `#[cfg(test)]` in
   [`src/atlassian/convert.rs`](../../src/atlassian/convert.rs) — JFM →
   ADF → JFM should be lossless. The neighbouring `nested_expand_*` tests
   are good models.
3. **Drift-detector** runs automatically — no new test needed, but it
   will fail if your `CONTENT_ENTRIES` edits desync from
   `UPSTREAM_ENTRIES`. The fix is usually to re-run codegen (see below).

## Refreshing the vendored schema

Most contributors won't need this — only when the upstream
`@atlaskit/adf-schema` npm package has been bumped and your new node lives
in the new version. The full workflow is in
[`assets/adf-schema/README.md`](../../assets/adf-schema/README.md). Short
version:

1. Pull the new tarball, extract `dist/json-schema/v1/full.json` into
   `assets/adf-schema/full.json`.
2. Update [`assets/adf-schema/provenance.json`](../../assets/adf-schema/provenance.json)
   with the new version, URL, and SHA-256s.
3. Run `cargo run --bin adf-schema-codegen` to regenerate
   [`src/atlassian/adf_schema/generated.rs`](../../src/atlassian/adf_schema/generated.rs).
4. Run `cargo run --bin adf-schema-codegen -- --check` to confirm the
   committed file matches what codegen now produces. CI runs this same
   check.

## Validation is type-level

All write paths take `&ValidatedAdfDocument`, not `&AdfDocument`. The only
fallible constructor is `ValidatedAdfDocument::try_new(...)` at
[`src/atlassian/adf_validated.rs:240`](../../src/atlassian/adf_validated.rs#L240),
which runs `adf_schema::validate_document` under the hood. This makes "I
forgot to validate" a compile error — see
[ADR-0025](../adrs/adr-0025.md).

Standalone conversion paths (the `convert to-adf` CLI, dry-run printers,
some round-trip tests) intentionally bypass validation so they can echo
arbitrary ADF including legacy-test fixtures. **Don't perpetuate this for
new node types** — your tests should always exercise the validator on the
new node.

## Gotchas

- **Don't edit `generated.rs` by hand.** It's regenerated by
  `cargo run --bin adf-schema-codegen` from the vendored JSON.
- **The drift detector is your friend.** If you add an entry to
  `CONTENT_ENTRIES` that upstream doesn't have, the drift test in
  [`tests/adf_schema_drift_bin_test.rs`](../../tests/adf_schema_drift_bin_test.rs)
  will tell you which atoms are misaligned. Read its error message before
  changing anything else.
- **Legacy tests intentionally produce invalid ADF.** Inline tests like
  `nested_expand_inside_panel` document the converter's pre-ADR-0023
  behaviour and assert on structurally invalid output. Don't copy that
  pattern for new node types — the goal is the opposite.
- **Permissive on unknowns.** Per [ADR-0023](../adrs/adr-0023.md), parents
  the validator doesn't know about are treated as opaque, preserving the
  `adf-unsupported` escape hatch from [ADR-0020](../adrs/adr-0020.md). A
  brand-new upstream node will round-trip silently until you encode it
  here — so a missing recipe step manifests as "my new node works in JSON
  but the converter ignores it", not a hard error.

## ADRs

- [ADR-0020](../adrs/adr-0020.md) — JFM: A Markdown Dialect for Bidirectional ADF Interchange.
- [ADR-0023](../adrs/adr-0023.md) — Data-Driven ADF Content-Model Schema and Validator.
- [ADR-0025](../adrs/adr-0025.md) — Wire ADF Schema Validator into the API Send Path via `ValidatedAdfDocument`.
