# ADR-0077: Sheet, Row, Column and Range Deletion via `spreadsheets.batchUpdate`

## Status

✅ Accepted (2026-09-08)

**Extends [ADR-0075](adr-0075.md), and supersedes its §3/§4 claim that
`deleteSheet`/`deleteDimension`/`deleteRange` stay structurally
unreachable.** ADR-0075 shipped only the additive half of
`spreadsheets.batchUpdate` — `addSheet`, `updateSheetProperties`,
`insertDimension` — and deferred deletion to "its own design pass", pinning
the deferral with a grep-guard test (`no_destructive_request_is_reachable`)
and a binding doc comment on `DriveOperation::SheetsStructure` itself: "row
and sheet deletion must not later join this variant either."

This ADR is that design pass (issue #1623). It ships `deleteSheet`,
`deleteDimension` (rows and columns) and `deleteRange`, removes the
grep-guard test that forbade them, and — as ADR-0075 §1's binding clause
required — routes every one of them through a **new** gate operation rather
than the existing `sheets-structure`.

## Context

ADR-0075 §2/§3 named exactly why deletion was cut where it was:

- **`deleteDimension` is unbounded by its own arguments.** Deleting rows
  5–10 also silently rewrites every formula referencing them across the
  whole workbook.
- **`deleteSheet` destroys data with no per-cell footprint.**

Both break ADR-0073's Consequences claim that "blast radius per call is
smaller than `drive edit`'s" — the claim ADR-0075 §3 confirmed still held for
the additive subset. `deleteRange` was not in ADR-0075's v1 discussion at
all (issue #1613 predates it being named explicitly), but issue #1623 raised
it alongside the other two: it shifts cells rather than whole rows/columns,
a "meaningfully different hazard" worth deciding rather than silently
including or excluding.

Three questions issue #1623 asked, and this ADR settles all three:

1. A new gate operation, or something stronger — interactive confirmation,
   a two-phase check-then-execute pattern like the worktrees `close` op
   ([ADR-0049](adr-0049.md))?
2. What does `--dry-run` show — structural facts only, or a preview of the
   affected cell content (an extra `values.get` read, and a data-exposure
   question)?
3. Is `deleteRange` in scope alongside `deleteSheet`/`deleteDimension`, or
   deferred again?

## Decision

### 1. A new `DriveOperation::SheetsDelete`, not a reused `SheetsStructure`

The identical argument ADR-0075 §1 made for `SheetsStructure` over
`SheetsWrite`, one level up: every `allow: ["sheets-structure"]` rule that
exists today was written when deletion was **impossible**. Reusing
`SheetsStructure` would retroactively convert those grants into "may destroy
data in this folder" — no config change, no re-consent. This is exactly the
widening ADR-0075 §1's binding clause exists to prevent, and this ADR
honors it rather than reopening it.

Fail-closed in both directions, like every other operation in this enum.
Forwards: defaults `Deny`. Backwards: an older binary reading a config
naming `sheets-delete` fails `Settings::load()`, and `active_account_rules`
degrades to an empty rule set, which denies everything.

One operation covers all four verbs (`delete-sheet`, `delete-rows`,
`delete-columns`, `delete-range`), not four. This mirrors
`SheetsStructure` already covering three unrelated additive verbs — the
gate's vocabulary tracks a *risk class*, not individual API calls — and
answers issue #1623's "is `deleteRange` in scope" question directly: yes,
under the same operation as the other two, since a rectangular delete is
no less "sheets-delete" than a dimension delete is.

`StructureVerb::gate_operation` is the single function that decides which
of the two operations a verb checks — every call site reads it from the
verb rather than assuming one, which is what makes the split impossible to
get backwards by omission.

### 2. Deletion is now typed and gated, not gated behind confirmation or `--force`

ADR-0075 §3 explicitly raised, and declined, a confirmation/`--force`
pattern for deletion — "the single most dangerous operation in the tree...
the worst possible place to introduce it" — and issue #1623 asked that this
stance be re-examined deliberately rather than inherited. Re-examined, and
kept: ADR-0070 §6 and ADR-0073 §9 already establish that **no** operation in
this integration gets an interactive confirmation or a `--force` escape
hatch, additive or destructive, cell-level or structural. Introducing one
only for deletion would be exactly the inconsistency ADR-0075 §3 warned
against, and the permission gate — an explicit, auditable, per-folder or
per-file `allow` grant — is already the consent mechanism every other write
in this tool relies on. A `--dry-run` that tells the truth about what would
be destroyed (§4 below) is the other half of that mechanism, and together
they are judged sufficient here as everywhere else in this integration.

[ADR-0049](adr-0049.md)'s two-phase check-then-execute pattern (the
worktrees `close` op) was the other alternative issue #1623 raised, and it
is not adopted. That pattern exists because the worktrees daemon is a
long-lived process a client can round-trip against twice — check, then
confirm. `drive` is a stateless per-invocation CLI with no daemon and no
session to hang a `confirmed` flag off; two phases here would mean two
separate command invocations with no way to guarantee the second is the
same request the first one previewed. `--dry-run` already gives the
single-invocation equivalent of the check phase, honestly, before anything
is sent.

### 3. Typed verbs, no raw request passthrough — unchanged

ADR-0075 §2's ban on a raw `spreadsheets.batchUpdate --requests
requests.json` passthrough is not reopened by this ADR. `deleteSheet`,
`deleteDimension` and `deleteRange` gain typed `BatchUpdateRequestItem`
variants (`DeleteSheetRequest`, `DeleteDimensionRequest`,
`DeleteRangeRequest`) exactly as the additive verbs have — the finite,
named set of constructible requests is what the gate, `--dry-run` and the
request log all describe exactly, and that property survives deletion
gaining a Rust representation. There is still no way to ask `omni-dev` to
send an arbitrary `Request`, however it is spelled.

### 4. `no_destructive_request_is_reachable` is removed; deletion is now reachable through one path only

ADR-0075 §4's grep-guard test asserted an absence: no production line named
`deleteSheet`/`deleteDimension`/`deleteRange`. That absence is no longer
true by design, so the test is deleted rather than weakened — a passing
test that asserts the opposite of what ships would be worse than no test.

What replaces it is a *presence* property instead of an absence one:
`every_delete_verb_gates_on_sheets_delete_not_sheets_structure` pins
`StructureVerb::gate_operation`'s mapping, and
`a_sheets_structure_rule_alone_does_not_permit_deletion` /
`a_sheets_delete_rule_alone_does_not_permit_a_structural_edit` exercise the
actual non-widening property end to end (mirroring ADR-0075's own
`a_sheets_write_rule_alone_does_not_permit_a_structural_edit`) — a folder
granted one operation must not gain the other. Deletion is reachable now,
but only through `structure.rs`'s gated, validated path, never through a
passthrough (§3), and never under the operation an existing
`sheets-structure` rule already grants (§1).

### 5. `--dry-run` stays structural-only; a fixed caveat replaces a formula-impact read

Issue #1623 asked whether a destructive `--dry-run` should also preview the
affected cell content via `values.get`. Decided against: the preview names
what would be destroyed structurally — a `delete-sheet`'s sheet name, id and
size; a `delete-rows`/`delete-columns`'s row/column span and the resulting
dimensions, the same before-and-after shape ADR-0075 §6 established for
`insert-rows`/`insert-columns`, shrinking instead of growing; a
`delete-range`'s rectangle and shift direction — using only the
`spreadsheets.get` response the engine already fetches for every structural
dry run (ADR-0075 §5). No extra read is issued, and no cell content ever
appears in the CLI's output or in the request log.

```
Would delete 3 row(s) 5-7 of 'Q2' (sheetId 118293) in 'Budget'
  (500 rows -> 497; existing rows 8-500 shift up; formulas elsewhere in the
   workbook that reference the deleted rows may break, which cannot be
   checked automatically)
```

In place of a content preview, every destructive preview — and every real
destructive outcome — states a fixed caveat: formulas elsewhere in the
workbook may reference what would be deleted, and this cannot be checked
from the target sheet's own dimensions. That is not a hedge; it is the
honest limit of what `spreadsheets.get` can tell a dry run.
`deleteDimension`'s unboundedness (§ Context above, and ADR-0075 §2) is
exactly this: checking it would mean reading every other sheet's formulas,
which is a different and much larger read than the one this surface already
does, and the data-exposure question issue #1623 raised applies to that read
too. Naming the limit is judged better than either silently ignoring it or
paying for a read that still could not answer it completely.

Every real (non-`--dry-run`) destructive outcome also states how to
recover, since there is no `files.delete` or undo anywhere in this
integration:

```
Deleted sheet 'Q2' (sheetId 118293) from 'Budget'; this cannot be undone
through omni-dev — use Google Drive's version history to recover it if
needed
```

This answers issue #1623's "is there any recovery story?" question:
Drive's own version history remains the only one, and the tool says so at
the point of destruction rather than leaving it undocumented.

### 6. Bounds validation mirrors the insert verbs, inverted

`delete-rows`/`delete-columns` reuse `--at`/`--count`, 1-based inclusive,
converted through the same `dimension_range` function `insert-rows`/
`insert-columns` already use — the single conversion site stays single.
Unlike insert, deletion has **no append-boundary case**: every row/column
named must already exist, so the exclusive end index may never exceed the
sheet's current size (insert's `at == current + 1` legal-append case has no
analogue here). `delete-range` takes all four bounds
(`--start-row`/`--end-row`/`--start-column`/`--end-column`) together,
1-based inclusive, converted into a fully-bounded `GridRange` — this crate
never sends an open-ended `GridRange` on either axis, since that would just
be a worse-spelled `delete-rows`/`delete-columns`. `--shift` is required and
names which way the remaining cells close the gap (`rows` = cells below
shift up, `columns` = cells to the right shift left), matching the Sheets
API's own `shiftDimension` field.

As with ADR-0075 §6, only *positions* are bounded against the workbook's
real current size; no argument is checked against an invented upper bound —
Sheets remains the authority on how large or small a sheet may actually
get.

### 7. Logging follows the existing shape; one new context key for `delete-range`

Same one-record-per-verb-and-per-request shape ADR-0075 §7 established.
`operation` becomes `sheets-delete-sheet`/`sheets-delete-rows`/
`sheets-delete-columns`/`sheets-delete-range`. `delete-sheet` and the
dimension deletes reuse the existing `sheet_id`/`sheet_title`/
`dimension_range` context keys exactly as the additive verbs do.
`delete-range` cannot: `dimension_range` only ever spans one axis, and a
rectangle needs both, so `DriveMutationOutcome` gains one more
omit-if-absent key, `grid_range` (e.g. `"rows 2-4, columns 2-3"`), the
`deleteRange` analogue of `dimension_range`. [docs/log.md](../log.md)
documents it alongside the rest.

## Consequences

- **No new OAuth grant and no new trust boundary.** `deleteSheet`/
  `deleteDimension`/`deleteRange` are more methods on a host the same Sheets
  bearer token already reaches; ADR-0073 §1's Consequences cover this
  unchanged.
- **ADR-0075 §1's binding clause is honored, not reopened.** An existing
  `allow: ["sheets-structure"]` rule keeps meaning exactly what it meant
  before this ADR; deletion requires a separate, explicit grant.
- **The permission vocabulary grows to seven operations.** The same standing
  rule ADR-0073 §3 and ADR-0075's Consequences give: growth alongside
  genuinely new capability is correct, and deletion is exactly that.
- **A refusal stays exactly as auditable as a success.** A gate-refused
  delete produces a `drivemutation` record with zero Sheets calls made,
  matching every other operation in this integration.
- **A destructive `--dry-run` costs the same one read an additive one
  does** — no more, despite deleting being the more consequential
  operation. The formula-impact caveat is the deliberate alternative to a
  more expensive and still-incomplete check.
- **There is still no recovery path but Drive's own version history**, and
  every real destructive outcome says so — a real limitation, stated rather
  than worked around, the same stance ADR-0075's Consequences and ADR-0061
  both take on their own hardest edges.
- **`no_destructive_request_is_reachable` is gone.** What replaces it —
  `every_delete_verb_gates_on_sheets_delete_not_sheets_structure` and the
  two cross-operation isolation tests (§4) — pins a presence property
  (routes through the right gate) rather than an absence one (cannot be
  constructed at all), because the latter is no longer true and asserting
  it would be a lie the test suite told.
- **Sync obligation.** [docs/drive.md](../drive.md) documents the four new
  verbs, the `sheets-delete` row in the write-permission table, and removes
  the "no deletion, at all" limitation it used to state;
  [docs/log.md](../log.md) documents the new `drivemutation` operations and
  the `grid_range` context field. Keep both in sync when this ADR's
  decisions change.
