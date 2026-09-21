# ADR-0082: Banded Ranges via `spreadsheets.batchUpdate`

## Status

✅ Accepted (2026-09-21)

**Extends [ADR-0078](adr-0078.md), reverses none.** ADR-0078 §1 already
generalizes the gate reasoning this ADR applies: a presentational change
over a range, destroying no data, joins `SheetsStructure` — the same
category `unmerge-cells`/`clear-data-validation` sit in. This ADR is the
one-capability design pass issue #1832 (split out of #1830) asked for; it
resolves the two questions the issue's own scope note left open and ships
the implementation alongside it.

## Context

`src/drive/sheets/` has no request type for banded ranges — alternating
row/column colors (`addBanding`/`updateBanding`/`deleteBanding`), and
`spreadsheets.get` never reads `bandedRanges` back. Four verbs: `add-banding`,
`update-banding`, `delete-banding`, `list-bandings`.

Two questions needed settling before implementation, both already answered
by precedent elsewhere in the crate rather than by anything new:

1. **Which color fields to model.** `BandingProperties` carries
   `headerColor`/`firstBandColor`/`secondBandColor`/`footerColor` as both a
   plain `Color` and a `*ColorStyle` wrapper. The Sheets v4 discovery
   document marks every plain-`Color` field deprecated in favor of its
   `*ColorStyle` counterpart, and the `*ColorStyle` field wins when both are
   set — so the plain fields are never worth sending. `format-cells`
   (ADR-0078 §5) already settled the same question for cell formatting:
   `*ColorStyle` only, RGB only (never the `themeColor` arm).
2. **How many axes a call may set.** A `BandedRange` can carry both
   `rowProperties` and `columnProperties` at once (a checkerboard effect).
   That combination has no real-world user asking for it and would double
   every color flag for it.

## Decision

### 1. Joins `SheetsStructure` — no new operation

A banding is presentation applied to a range; removing one destroys no grid
data. That is exactly the "property of the sheet, not the sheet's data"
test ADR-0078 §1 established and every later tranche (ADR-0081 §§1-3)
reused for conditional formats, filter views, named ranges, charts and
slicers. Banded ranges add no new fact to weigh — they join the same
operation, not a new one, and `write_gate.rs`'s `DriveOperation::SheetsStructure`
doc comment is extended to say so.

### 2. `*ColorStyle` only, RGB only — the `format-cells` cut, repeated verbatim

`BandingProperties` in this crate models only `header_color_style`/
`first_band_color_style`/`second_band_color_style`/`footer_color_style`,
each an `Option<ColorStyle>`; `ColorStyle` itself continues to model only
the `rgbColor` arm (`types.rs`'s existing doc comment on `ColorStyle`,
unchanged). CLI color flags take `#RRGGBB` — the hex syntax is itself the
statement of the cut, per `format-cells`' own convention, with the
theme-color omission recorded only in the type's doc comment.

### 3. One axis per call, `--axis rows|columns` (default `rows`)

`add-banding`/`update-banding` set exactly one of `row_properties`/
`column_properties`, selected by `--axis`. Setting both in one call is not
exposed. A documented cut, not a silent gap — named in `add-banding
--help` and the `banding.rs` module doc, matching `filter.rs`'s own framing
for its `duplicate-filter-view` and condition-surface cuts.

### 4. Id-addressed like a filter view, not range-matched like a protection

A `BandedRange` carries a server-assigned `bandedRangeId`, a stable handle
with no ambiguous-match case — unlike a protected range, which has no
API-side id and must be resolved by exact `GridRange` equality
(`protection.rs::find_existing_protection`, ADR-0078 §7). `update-banding`/
`delete-banding` take `--banded-range-id` directly, discovered via
`list-bandings`; `banding.rs` mirrors `filter.rs`'s
`find_existing_filter_view`/`RefusedFilterViewNotFound` shape rather than
`protection.rs`'s. `update-banding` merges a changed color onto the
selected axis's *existing* properties client-side before sending, the same
"Sheets replaces a named field wholesale, never per-entry" reasoning
`filter.rs::build_update` documents for `sortSpecs`/`criteria`.

### 5. `list-bandings` is a plain read, ungated

Follows `list-protections`/`list-filter-views`/`list-named-ranges`'s
established convention: a caller must be able to see what exists (and its
id) before an id-addressed verb can act on one at all. No new
`SheetsApi::get_spreadsheet_with_banding` reasoning is needed beyond the
existing "one widened `fields` mask per capability" pattern (`api.rs`).

### 6. One `drivemutation` record per verb; one new context field

`banded_range_id` joins `DriveMutationOutcome` — server-assigned for
`add-banding`, resolved-against for `update-banding`/`delete-banding` —
mirroring `filter_view_id`'s precedent exactly.

## Consequences

- **No new OAuth grant and no new trust boundary.** Same as every prior
  Sheets ADR: every new request rides the scopes already covered.
- **`sheets-structure` now covers one more capability.** An operator who
  granted it before this ADR shipped is not retroactively exposed to
  anything that destroys data — the binding constraint on that variant
  holds — but is exposed to banded ranges, which this ADR argues is within
  what granting "restructure this workbook" already implied, the same
  argument ADR-0078/ADR-0081 made for their own additions.
- **A user who wants both row and column banding on one range, or a theme
  color, must use the Sheets UI.** Documented in `docs/drive.md`, the same
  stance every prior curated-surface ADR takes.
- **Sync obligation.** `docs/drive.md` documents the four new verbs and the
  `sheets-structure` row's expanded scope; `docs/log.md` documents the new
  `sheets-add-banding`/`sheets-update-banding`/`sheets-delete-banding`
  operations and the `banded_range_id` context field. Keep both in sync
  when this ADR's decisions change.
