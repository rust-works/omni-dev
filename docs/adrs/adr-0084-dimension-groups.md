# ADR-0084: Dimension Groups (Row/Column Outlining) via `spreadsheets.batchUpdate`

## Status

✅ Accepted (2026-09-21)

**Extends [ADR-0078](adr-0078.md) and [ADR-0083](adr-0083.md), reverses
none.** ADR-0078 §1 already generalizes the gate reasoning this ADR
applies: a presentational change over a span, destroying no data, joins
`SheetsStructure`. ADR-0083's Context already named dimension groups as
"clean `sheets-structure` reuse on the same reasoning ADR-0081 §1
recorded" — this ADR is the one-capability design pass issue #1833 (split
out of #1830) asked for; it settles the addressing and depth questions the
issue's own scope note left open and ships the implementation alongside
it.

## Context

`src/drive/sheets/` has no request type for dimension groups — the
collapsible +/- outline over a row or column span
(`addDimensionGroup`/`updateDimensionGroup`/`deleteDimensionGroup`), and
`spreadsheets.get` never reads `rowGroups`/`columnGroups` back. Four
verbs: `add-dimension-group`, `update-dimension-group`,
`delete-dimension-group`, `list-dimension-groups`.

Three questions needed settling before implementation:

1. **Whether to validate nesting depth client-side.** Issue #1833's own
   scope note proposed capping it at 2, "the way other range-based verbs
   validate their `GridRange` inputs".
2. **How to address an existing group for `update`/`delete`.** A dimension
   group carries no server-assigned id, unlike a banded range
   ([ADR-0082](adr-0082-banded-ranges.md) §4) or a filter view.
3. **Which fields `update-dimension-group` may change.**

## Decision

### 1. No client-side depth cap — a deliberate deviation from the issue's scope note

`addDimensionGroupRequest` carries only a `range`; the server derives the
new group's `depth` from how that range overlaps existing groups on the
same axis (a superset increments an existing group's depth and gives the
new group that shallower depth; a subset or partial overlap creates a new,
deeper one — see the Sheets API reference for `AddDimensionGroupRequest`).
Predicting that outcome client-side would mean re-implementing those
overlap rules in this crate, purely to enforce a cap. No maximum nesting
depth is documented anywhere in the Sheets API reference to validate
against in the first place — unlike a `GridRange`'s bounds, which are
checked against a *fact already in hand* (the sheet's current row/column
count), a depth cap here would be checked against a number nobody has
published. This crate follows the same stance ADR-0073 §7 already takes
for `--count` on `insert-rows`/`insert-columns`: Sheets is the authority on
how large or how deeply nested a structure may get; this crate validates
only what it can compute from data it already has. Only the span itself
(`--start`/`--end`, 1-based inclusive) is validated, against the sheet's
current extent — the same check `auto-resize-dimension` makes.

### 2. Addressed by `(range, depth)` — no id, unlike a banded range

Unlike `BandedRange`, `DimensionGroup` carries no `bandedRangeId`-style
handle. `updateDimensionGroup`'s own wire request already requires
`dimensionGroup.range` *and* `dimensionGroup.depth` to select a target, so
this crate exposes exactly that: `update-dimension-group` takes
`--sheet`/`--dimension`/`--start`/`--end` to resolve a span, plus an
optional `--depth`. When the span resolves to exactly one group,
`--depth` may be omitted; when the API's own overlap rules have produced
more than one group at that exact span (this happens whenever a group is
added over a range equal to an existing one — see Decision 1), the
ambiguity is refused, naming the depths found, and `--depth` disambiguates.
This is the same ambiguous-match shape
`protection.rs::find_existing_protection` already uses for a protected
range, which likewise has no id of its own.

`delete-dimension-group` takes the same `--sheet`/`--dimension`/`--start`/
`--end` span, requiring an **exact** match among the groups
`list-dimension-groups` would show. The API's own partial-span delete — a
`range` that only partially overlaps an existing group decrements that
group's depth rather than removing anything (the API reference's own
example: deleting over `D:E` leaves a depth-1 group over `B:D` and a
depth-2 group over `C:C`) — is not exposed. Sending an unexpected partial
overlap would silently *demote* an unrelated group rather than remove the
one the caller named; refusing anything but an exact match is the same
"never promise a change the real run then surprises you with" stance
`banding.rs`'s id-only addressing takes for a different reason.
`deleteDimensionGroupRequest`'s wire shape carries only a `range`, no
`depth`, so when an exact match resolves to more than one group (again,
same-span groups at different depths), this crate does not refuse as
ambiguous — the request itself has no way to name one depth over another,
and Sheets decides. The outcome reports the depth acted on only when the
match was unambiguous.

### 3. `update-dimension-group` changes only `collapsed`

`range` is the group's identity (changing it would mean addressing a
*different* group, not updating this one) and `depth` is server-derived —
neither is a field this crate could sensibly let a caller set on update.
`collapsed` is the only field left in `DimensionGroup`, so
`update-dimension-group --collapsed <BOOL>` is required (not optional the
way `format-cells`' flags are — there is no "leave it unset" case), and
the request's `fields` mask is always exactly `"collapsed"`.

### 4. One `drivemutation` record per verb; no new context field

`sheet_id` and `dimension_range` (already used by `structure.rs`'s
insert/delete-dimension verbs) are reused verbatim rather than adding new
ones — a dimension group's location *is* a dimension range, in the same
`"ROWS 5:7"` 1-based-inclusive shape. `fields_changed` (already used by
`format.rs`'s dimension-property verbs) records `"collapsed=true"` /
`"collapsed=false"` for `update-dimension-group`; no id field is added,
since a dimension group carries none.

## Consequences

- **No new OAuth grant and no new trust boundary.** Same as every prior
  Sheets ADR: every new request rides the scopes already covered.
- **`sheets-structure` now covers one more capability.** An operator who
  granted it before this ADR shipped is exposed to dimension groups, which
  this ADR argues is within what granting "restructure this workbook"
  already implied — the same argument ADR-0078/ADR-0081/ADR-0082 made for
  their own additions.
- **A caller cannot predict the depth a new group will get, only request a
  span.** `add-dimension-group`'s outcome never reports a depth (`None`,
  always) — `list-dimension-groups` is how a caller discovers what the
  server actually assigned.
- **The API's partial-span delete/decrement behavior is not reachable
  through this crate.** A caller who wants to *shrink* a group's depth
  without removing it entirely must use the Sheets UI, or a
  `delete-dimension-group` over the group's exact original span followed
  by a fresh `add-dimension-group` over the new, narrower span.
- **Sync obligation.** `docs/drive.md` documents the four new verbs and
  the `sheets-structure` row's expanded scope; `docs/log.md` documents the
  new `sheets-add-dimension-group`/`sheets-update-dimension-group`/
  `sheets-delete-dimension-group` operations and the reused
  `dimension_range`/`fields_changed` context fields. Keep both in sync
  when this ADR's decisions change.
