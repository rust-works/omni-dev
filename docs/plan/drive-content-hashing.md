# Project Plan: Drive Content Hashes & Duplicate Detection

**Status:** Built

## Overview
Drive API v3's `files` resource exposes `md5Checksum`/`sha1Checksum`/`sha256Checksum`
for binary-content files (absent for folders and Google-native Docs/Sheets/Slides),
but omni-dev's `DriveFile` struct doesn't request or model them. Unlike Gmail's
list-then-hydrate pattern, Drive's `files.list` already returns full metadata per hit
via a hand-maintained `fields=` selector, so surfacing these fields — and any "bulk"
capability built on top of them — needs no new fetch mechanism, just widening that
selector. This unlocks three things in sequence: surfacing the checksums everywhere
Drive metadata already flows, a new `drive dedupe` command that groups search results
by content hash, and a `--verify` flag on `drive read --content` that locally
recomputes SHA-256 and checks it against Drive's reported value.

Tracks [issue #1556](https://github.com/rust-works/omni-dev/issues/1556).

## Goals
- Surface `md5Checksum`/`sha1Checksum`/`sha256Checksum` through the Drive CLI and MCP
  surface, wherever Drive metadata already flows.
- Add duplicate-content detection (`drive dedupe`) built on the existing bulk-search
  path, with no new fetch mechanism.
- Add local integrity verification (`--verify` on `drive read --content`) using the
  already-present `sha2` dependency, no new crate.
- Keep the Drive integration read-only throughout — dedupe and verify are both
  search + local computation, no writes.

## Current CLI Structure
```
omni-dev drive
├── auth {login, status, logout}
├── account {list, add, remove, default}
├── search <QUERY> [--limit] [-o]
└── read <FILE_ID> [--content] [--export-mime-type] [--out-file] [-o]

Target:
omni-dev drive
├── auth {login, status, logout}
├── account {list, add, remove, default}
├── search <QUERY> [--limit] [-o]
├── read <FILE_ID> [--content] [--export-mime-type] [--out-file] [--verify] [-o]
└── dedupe <QUERY> [--limit] [-o]
```

## Implementation Plan

### Phase 1: Core — surface checksums
1. **`src/drive/types.rs`** — add `md5_checksum`/`sha1_checksum`/`sha256_checksum:
   Option<String>` to `DriveFile`, placed after `size`, each with an explicit
   `rename` attribute (unlike `size`, whose Rust name already equals its JSON key).
   Doc-comment the binary-content-only caveat and md5's broader historical coverage.
2. **`src/drive/files_api.rs`** — add the three fields to both `LIST_FIELDS` and
   `GET_FIELDS`. Update the `fully_populated_drive_file()` test fixture (a
   compile-time forcing function, since it builds every field explicitly) — this
   pairs with the existing `get_fields_requests_every_drive_file_field` /
   `list_fields_requests_every_drive_file_field_except_export_links` tests, which
   assert the selector strings stay in sync with the struct automatically.
3. **`src/cli/drive/search.rs`** — no change; checksums reachable only via `-o
   json/yaml/jsonl`.
4. **`src/cli/drive/read.rs`** — `render_metadata_table` gains three new optional
   lines (Md5Checksum/Sha1Checksum/Sha256Checksum), appended after the existing
   WebViewLink line.
5. **`src/mcp/drive_tools.rs`** — extend `drive_search`'s tool description to mention
   the three fields.
6. **`docs/drive.md`** — extend the `## Read`/`## Search` field lists and add a short
   "Content hashes" callout.

### Phase 2: Bulk — `drive dedupe` command
1. **`src/cli/drive.rs`** — register a new `dedupe` module, `DriveSubcommands::Dedupe`
   variant, and dispatch arm, following the existing `search`/`read` pattern exactly.
2. **`src/cli/drive/dedupe.rs`** (new) — `DedupeCommand` mirrors `SearchCommand`'s
   shape (`query`, `limit`, `output`). `run_dedupe` reuses `FilesApi::search_all`
   (already auto-paginates to a 10,000-file cap) and a pure `group_duplicates`
   function that groups files by `md5_checksum` (skipping `None`), keeping only
   groups with 2+ files, using a `BTreeMap` (or a post-sort) for deterministic
   ordering. Table output: `HASH | COUNT | FILES`, implemented self-contained in
   `dedupe.rs` (matching the existing precedent that `search.rs`/`read.rs` each
   implement their own table rendering rather than sharing a utility).
3. **`src/mcp/drive_tools.rs`** — new `drive_dedupe` tool + `DriveDedupeParams`,
   mirroring `drive_search`/`DriveSearchParams`.
4. **`docs/drive.md`** — new `## Duplicate detection` section, added to the ToC.

### Phase 3: Integrity — `--verify` on `drive read --content`
1. **`src/cli/drive/read.rs`** — add `--verify` flag to `ReadCommand`. In
   `run_read_content`, bail early if `--verify` is combined with a Google-native file
   (no checksum exists for exported bytes); otherwise, after the download, hash the
   bytes with `sha2::Sha256::digest` (already a project pattern via `auth.rs`'s PKCE
   `code_challenge`), hex-encode via a small new `pub(crate)` helper (no `hex` crate
   dependency needed), and compare case-insensitively against `sha256Checksum`. Print
   a one-line confirmation to stderr on success.
2. **`src/mcp/drive_tools.rs`** — mirror `verify` on `DriveFileReadParams` /
   `run_file_read_content`, reusing the same helpers from `read.rs` (following the
   existing precedent that MCP already reuses `resolve_export_mime_type`/`is_texty`
   from that file).
3. **`docs/drive.md`** — extend `## Read` with a `--verify` paragraph.

## Technical Considerations

### Serde rename pitfall
`size` has no `rename` attribute because its Rust field name already matches the
Drive API's JSON key. The three new checksum fields do not have that luxury — their
snake_case Rust names (`md5_checksum`) don't match Drive's camelCase wire format
(`md5Checksum`), so each needs an explicit `rename = "..."`, or it silently
deserializes to `None` forever.

### Selector/struct sync tests are a free correctness net
`get_fields_requests_every_drive_file_field` and
`list_fields_requests_every_drive_file_field_except_export_links` serialize the test
fixture and assert every resulting JSON key appears in `GET_FIELDS`/`LIST_FIELDS`.
Get the fixture right and these tests catch any selector drift automatically.

### Hex encoding without a new dependency
No `hex` crate is present in the project. `sha2::Sha256::digest()` returns a byte
slice; a small local `to_hex_string` helper (pre-allocate, write two hex chars per
byte) avoids adding a dependency for something this small.

### Deterministic dedupe grouping order
`group_duplicates` must not rely on `HashMap` iteration order, which is
nondeterministic — group into a `BTreeMap` (or sort the result by checksum) so table
output and tests are stable across runs.

### CLI surface changes require snapshot updates
Both `DedupeCommand` (new) and `ReadCommand::verify` (new field) change
`#[derive(Parser)]` surface, so `tests/snapshots/integration_test__help_all_output.snap`
needs regeneration via the `update-snapshots` skill after Phases 2 and 3.

### Clippy conventions
The repo denies `unwrap_used`/`expect_used` outside `#[cfg(test)]`. All new
fallible paths (checksum comparison, missing-checksum errors, MCP param handling)
use `anyhow::ensure!`/`anyhow::bail!`/`.context(...)`, matching every existing error
path in `src/drive/` and `src/cli/drive/`.

## Success Criteria
1. ✅ `drive search`/`drive read -o json` include `md5Checksum`/`sha1Checksum`/
   `sha256Checksum` when Drive returns them.
2. ✅ `drive read` (table output) shows the checksum lines when present.
3. ✅ `drive dedupe <query>` groups files sharing an `md5Checksum` into rows, skipping
   files without one.
4. ✅ `drive read <id> --content --verify` succeeds on a matching download and fails
   clearly on a mismatch, a missing checksum, or a Google-native file.
5. ✅ `drive_search`/`drive_file_read`/`drive_dedupe` MCP tools expose the same
   behavior.
6. ✅ `cargo test --lib` and `cargo clippy --all-targets` pass; help-output snapshots
   regenerated.
7. ✅ `docs/drive.md` documents all three phases.

## Future Enhancements
Explicitly deferred (from issue #1556), not to be silently expanded into this work:
- Local MD5/SHA1 computation — no existing dependency; only SHA-256 verification is
  "free" via the already-present `sha2` crate.
- Revision-level checksums (`files.revisions[].md5Checksum`) — omni-dev has no
  `revisions` API wrapper at all today.
- Any write/mutation Drive operations — the integration stays deliberately read-only.
- A `--by` flag to choose the dedupe grouping key instead of `md5Checksum` — starting
  with the single broadest-coverage key (YAGNI).

## Timeline
- Phase 1: core struct/selector/table/MCP/docs changes — small, mechanical.
- Phase 2: new `dedupe` command + tests — moderate, one new file.
- Phase 3: `--verify` flag + tests — moderate, touches an existing hot path.

## Dependencies
- No new external crates — `sha2` (already present) covers SHA-256; no `hex` crate
  needed (small local helper instead).
- Phase 2 and Phase 3 both depend on Phase 1's new `DriveFile` fields.
