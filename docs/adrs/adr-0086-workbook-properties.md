# ADR-0086: Workbook Properties via `updateSpreadsheetProperties`

## Status

✅ Accepted (2026-09-22)

**Extends [ADR-0078](adr-0078.md) and [ADR-0081](adr-0081.md), reverses
none.** ADR-0078 §1's "a property of the sheet, not the sheet's data" test —
already reused by ADR-0081 §§1–4, ADR-0082 §1, ADR-0084 and ADR-0085 §1 for
every other addition to `SheetsStructure` — is applied here one level up, to
a property of the *workbook* rather than of any one sheet. ADR-0081 §2's
"value effect reached indirectly" reasoning for named-range deletion is
applied again, to iterative calculation. This ADR is the one-capability
design pass issue #1836 (split out of tracker #1830) asked for, resolving
the gate-reasoning question the issue itself raised rather than leaving it
assumed.

## Context

`updateSpreadsheetProperties` on `spreadsheets.batchUpdate` was entirely
unimplemented: no verb touched `locale`, `timeZone`, `autoRecalc`,
`iterativeCalculationSettings`, or `spreadsheetTheme`. `create` sets a title
via the Drive API, not this request, so even title-via-`batchUpdate` was
unexercised — and stays that way (§2).

Two questions needed settling before implementation:

1. **Whether `iterativeCalculationSettings` belongs in `sheets-structure` at
   all.** Unlike locale/time-zone/auto-recalc, which only change
   presentation, turning iterative calculation on changes what an
   *unchanged* circular-reference formula elsewhere in the workbook
   evaluates to — a value effect, which is exactly the class of concern
   ADR-0081 §2 raised for `delete-named-range` before concluding it still
   belongs in `sheets-structure` (§9).
2. **Whether `spreadsheetTheme` ships in v1.** It is a large nested type
   (font family plus a full color palette) with no established shape
   elsewhere in this crate to reuse.

## Decision

### 1. Joins `SheetsStructure` — no new operation

None of the four fields writes, moves or deletes a stored cell value or
formula, so this capability adds no new fact to weigh and joins
`SheetsStructure` rather than earning a variant, the same conclusion
ADR-0085 §1 reached for sheet-view properties. `write_gate.rs`'s
`DriveOperation::SheetsStructure` doc comment is extended to name the new
verb. `SheetsDelete`'s, `SheetsWrite`'s and `SheetsProtection`'s binding
clauses are untouched.

### 2. A new verb, `update-workbook-properties` — the one with no sheet target

Every other structural verb in this family names a `--sheet`. This one acts
on the workbook itself, addressed by the `spreadsheetId` already in the
URL. `StructureVerb::sheet_title` changes its return type from `&str` to
`Option<&str>` to represent this — the one variant with nothing to return —
and `resolve_sheet` returns `Ok(None)` for it before ever consulting the
sheet list, rather than the verb being given a synthetic or unused sheet
title. No other verb's behavior changes: every existing arm of
`sheet_title`/`resolve_sheet` still returns `Some`.

`--title` is deliberately not one of this verb's flags. `drive rename`
already owns the title via the Drive API (`files.update`), works on every
file type, and is ungated; a second title path under a different gate buys
nothing and makes "which command renames?" a question with two answers.

### 3. `spreadsheetTheme` stays out of scope; so does `importFunctionsExternalUrlAccessAllowed`

`spreadsheetTheme` is left for a future issue — a documented cut, matching
this crate's established pattern of shipping a subset and saying so, not a
silent gap. It is neither read nor written by this verb or by `sheets info`.

`importFunctionsExternalUrlAccessAllowed`, a newer boolean on the same
proto, is also out of scope. It is called out specifically because, if ever
added, it is not a plain `sheets-structure` question the way the four fields
here are: it lets `IMPORT*` formulas reach external URLs, which is an
outbound-access grant, not presentation — a different decision than this
ADR makes.

### 4. `--auto-recalc` excludes Sheets' own "unspecified" value

`RecalculationInterval::Unspecified` (`RECALCULATION_INTERVAL_UNSPECIFIED`)
exists on the wire type, because a `spreadsheets.get` response can carry it,
but the CLI-facing `AutoRecalcArg` omits it — it is Sheets' way of saying the
field was never set, never a value a caller would deliberately choose to
write.

### 5. Iterative calculation is presence-based, not a boolean field

Sheets has no "enabled" flag for iterative calculation: the mere *presence*
of `iterativeCalculationSettings` on the workbook's properties is what turns
it on, and its *absence* is what turns it off. `--iterative-calculation off`
therefore clears the field by naming it in the field mask while leaving it
unpopulated in the request body — the same field-mask-names-it-but-body-
omits-it idiom ADR-0085 §5 introduced for `--clear-tab-color`, applied here
to a settings object instead of a color. `IterativeCalculationToggle` exists
purely to carry the caller's on/off *intent* through `build_request`, since
the wire type itself has no vocabulary for "off".

### 6. The field mask names exactly what was populated

Same discipline as ADR-0085 §4: the mask is built from the flags actually
passed, so an omitted `--locale` leaves the current locale alone rather than
resetting it. At least one of `--locale`/`--time-zone`/`--auto-recalc`/
`--iterative-calculation` is required — an empty mask is refused
client-side rather than sent as a no-op request.

### 7. Iterative-calculation sub-flags require the toggle

`--iterative-calculation-max-iterations`/
`--iterative-calculation-convergence-threshold` are refused unless paired
with `--iterative-calculation on` — including when paired with `off`, which
would otherwise silently discard the values. Both `validate_verb_args`
checks (empty mask, orphaned sub-flags) run before `build_request`, so a
`--dry-run` reports the same refusal a real run would.

### 8. `SPREADSHEET_FIELDS`'s base mask is widened, not given a new `_WITH_*` sibling

Every other capability that needs more of `spreadsheets.get`'s response than
the base `properties.title`/`sheets.properties(...)` gets its own
`SPREADSHEET_FIELDS_WITH_*` constant and fetch method — protections, filter
views, conditional formats, named ranges, embedded objects, pivot tables —
because each of those is a *list* that grows with workbook content, so
paying for it on every call would cost real bytes on a large workbook. This
capability instead widens the base `SPREADSHEET_FIELDS` mask itself:
`locale`/`timeZone`/`autoRecalc`/`iterativeCalculationSettings` are four
fixed scalar/small-object fields on the `properties` object every structural
verb already fetches for `properties.title`, so including them
unconditionally costs every other caller nothing measurable, and
`structure()` has exactly one `get_spreadsheet` call site shared by all
fifteen `StructureVerb`s — branching it per verb would be new complexity for
no real savings. One consequence: `sheets info`, which also calls the base
`get_spreadsheet`, receives these fields for free.

### 9. Gate reasoning: `sheets-structure`, not a data-mutating operation — the sentence the issue asked for

The issue asked this be settled rather than assumed. None of the four
fields writes, moves or deletes a stored cell value or formula, but three of
them change what *unchanged* formulas evaluate to:

- `iterativeCalculationSettings` — on: circular references converge to a
  number instead of erroring; off: those numbers become errors again.
- `timeZone` — shifts `NOW()`/`TODAY()`; `autoRecalc` changes *when*
  volatile functions recompute; `locale` changes display formatting,
  separators and function-name localisation of unchanged cells.

That is exactly the shape ADR-0081 §2 already settled for
`delete-named-range` ("an unchanged formula evaluating differently") under
`sheets-structure`, and it is strictly *more* recoverable than that case:
the inverse of every one of these is setting the property back to its
previous value, which the dry-run output and the `drivemutation` record both
carry (§10). No new `DriveOperation`.

### 10. `sheets info`'s table render and the request log both reuse one summary function

`workbook_properties_summary` — a prose join such as `"locale -> 'en_US',
auto-recalc -> HOUR"` — is the single source of truth for the dry-run
preview, the real-run confirmation, and the request log's `fields_changed`
(reusing that existing context key rather than introducing one of its own,
the same reuse ADR-0085 §9 made for `update-sheet-properties`). Its
`describe_bounds` half for the iterative-calculation qualifier
(`" (max 50 iterations, threshold 0.01)"`) lives on
`IterativeCalculationSettings` itself in `types.rs`, so `sheets info`'s
table renderer — which needed its own presence-checked lines for `Locale:`/
`Time zone:`/`Recalculation:`/`Iterative calculation:`, since only `-o
json`/`-o yaml` get the new fields for free via `Serialize` — reuses the
same helper rather than reformatting the bounds a third way. `sheets info`
shows iterative calculation only when *on*: the base mask always requests
`iterativeCalculationSettings`, so an absent value there means Sheets
reported it off, the same "flag the exception, not the default" convention
hidden sheets already use.

## Consequences

- **No new OAuth grant and no new trust boundary.** Same as every prior
  Sheets ADR: the new request rides the scopes already covered.
- **`sheets-structure` covers one more capability**, including one whose
  effect is a workbook-wide value change (iterative calculation) rather
  than pure presentation — an operator who granted the operation before
  this ADR shipped is exposed to that, the same trade ADR-0081 §2 already
  made for named-range deletion under the same operation.
- **`StructureVerb::sheet_title` is `Option<&str>` for every verb now**,
  not just this one — a small, one-time signature widening that makes "no
  sheet target" representable instead of forcing a placeholder.
- **`spreadsheetTheme` and `importFunctionsExternalUrlAccessAllowed` remain
  unreachable.** Documented cuts (§3), not gaps: the former needs a future
  issue if wanted, the latter needs its own gate analysis if ever added,
  precisely because it is not a presentation-only field.
- **`sheets info`'s table output gained four new conditional lines** and
  `render_info_table`'s doc comment states the presence-vs-default
  reasoning, so a future reader does not mistake "iterative calculation
  line absent" for "not fetched".
- **Sync obligation, closed in this same change.** `docs/drive.md`'s
  structural-verbs section, its `sheets-structure` policy-table row, and its
  `sheets info` example; `src/drive/write_gate.rs`'s `SheetsStructure` doc
  comment; `docs/log.md`'s operation list; and `CHANGELOG.md` all name
  `update-workbook-properties` and this ADR, the same obligation ADR-0085's
  Consequences recorded for itself.
