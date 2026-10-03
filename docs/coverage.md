# Coverage diff

`omni-dev coverage diff` attributes a per-line coverage report to a git diff and
reports **patch coverage** — the share of the lines a change *added* that are
covered by tests — plus the actionable list of uncovered new lines, per-file
project deltas, and indirect coverage changes on unchanged code.

It is the engine behind the project's PR coverage comment (rendered in CI by the
[`action-works/omni-dev-coverage-check`](https://github.com/action-works/omni-dev-coverage-check)
composite action), but it is a plain CLI command you can also run locally to
check a branch before you push.

## What it computes

Given a coverage report and a diff, it produces:

- **Patch coverage** — fraction of *added* lines that are covered (the headline
  number, and what `--fail-under-patch` gates on).
- **Overall line coverage** — the project-wide total (what `--fail-under-lines`
  gates on).
- **Uncovered new lines** — an actionable `file:line` list of added lines with no
  coverage (optionally collapsed into ranges, e.g. `9-11`, with
  `--collapse-ranges`).
- **Per-file project deltas** — each touched file's overall covered-line change
  (requires a `--baseline-report`).
- **Indirect coverage changes** — coverage that flipped on lines the diff never
  touched (also requires a baseline).

Line coverage only; branch-coverage data in the report is ignored.

## Inputs

- `--report <PATH>` (**required**, repeatable) — the head coverage report. Three
  formats are accepted and **auto-detected** from content: lcov trace files,
  llvm-cov JSON (`cargo llvm-cov report --json`), and Cobertura XML. Override
  detection with `--report-format <auto|lcov|llvm-cov-json|cobertura>`. Pass it
  once per shard to merge a [sharded run](#sharded-runs).
- `--base-ref <REV>` / `--head-ref <REV>` — the revisions to diff. Defaults are
  the merge-base of `origin/main` and `HEAD` for the base, and `HEAD` for the
  head (the revision the report was measured at).
- `--baseline-report <PATH>` (+ `--baseline-report-format`) — an optional
  *base-side* report. Supplying it enables the project-delta and
  indirect-change sections; without it you still get patch coverage and the
  uncovered-line list.

## Quick start

```bash
# 1. Produce a per-line report for the working tree (example: cargo-llvm-cov).
cargo llvm-cov --no-report      # instrument + run tests
cargo llvm-cov report --lcov --output-path head.lcov

# 2. Attribute it to the diff against the default merge-base.
omni-dev coverage diff --report head.lcov

# 3. Gate a branch locally: fail if patch coverage is under 80%, or overall
#    line coverage is under 70%.
omni-dev coverage diff --report head.lcov --fail-under-patch 80 --fail-under-lines 70

# 4. Full report with project deltas, as JSON for tooling.
omni-dev coverage diff \
  --report head.lcov --baseline-report base.lcov \
  -o json
```

## Output formats

`-o`/`--output` selects the renderer:

- `markdown` (default) — the PR-comment layout. The markdown footer can carry CI
  context via `--artifact-url`, `--run-url`, `--base-sha`, `--head-sha`, and
  `--commit-url` (a SHA-link prefix); these only affect the rendered links.
- `yaml` / `json` — structured output for scripting and downstream tooling.

## Gating

`--fail-under-patch <PCT>` makes the command exit non-zero when patch coverage is
below `<PCT>` percent, so it can fail a CI step or a local pre-push check. Without
it the command only reports and always exits zero.

`--fail-under-lines <PCT>` is the overall counterpart: it exits non-zero when
line coverage across the whole head report is below `<PCT>` percent. It gates on
the same `Total` the markdown headline prints, so it moves with the
`--ignore-filename-regex` list and `ignore` markers, exactly as the patch gate
does. Both gates can be set together, and every gate that fails is named in the
error.

- **The figure is lcov-derived.** It is covered lines over executable lines as
  counted from the report's per-line records, so it can differ slightly from
  `cargo llvm-cov report --summary-only`, which counts from the profile data and
  the instrumented binaries. Pick a threshold against this command's number, not
  against that one. Measured on this repository's own unit-test suite (about 97%
  covered), the lcov-derived figure was **0.16 pp higher** than llvm-cov's summary:
  the lcov lists about 6% fewer lines (324,274 against 344,949), and the lines it
  omits are mostly covered ones. The cause was not investigated, and the gap will
  differ per codebase. `ignore` markers and `--ignore-filename-regex` move the gate
  figure further, since llvm-cov's summary knows nothing of them.
- **An unmeasurable total fails.** A report with no executable lines — empty, or
  fully excluded by `--ignore-filename-regex` — fails `--fail-under-lines`. This is
  deliberately unlike `--fail-under-patch`, where "no added lines" is a property of
  the change and passes. A gate that passed on an empty report would let a
  silently failed coverage run through.

Because the gate needs only the report, it works wherever `coverage diff` does —
including when the report was produced elsewhere (for example, from several CI
jobs, see [Sharded runs](#sharded-runs)) and the profile data is not on the
machine running the gate.

## Diff scoping

By default the project-delta and indirect-change sections are scoped to **files
the diff touches**. Coverage is measured by two independent instrumented runs
(baseline vs head), so lines in untouched files can flip covered↔uncovered purely
from run-to-run variance and surface as phantom deltas. Genuine cross-file
effects still surface via a magnitude-gated "notable unchanged" note. Pass
`--all-files` to restore the unscoped (noisier) report. Patch coverage is
unaffected by this scoping.

The **headline total** applies the same tolerance the per-file sections do: a
move smaller than 0.05 pp renders neutral (`⚪`), and the direction emoji is
taken from the *rounded* value that is printed beside it, so a delta displayed
as `0 pp` is never painted red or green. When the total does move but no
per-file row and no "unchanged files also moved" entry accounts for it, the
headline is annotated `_(not attributable to this diff)_` — the move is then, by
construction, cross-run variance spread across files rather than an effect of
the PR. The percentage itself is always the real measured value.

## Path normalisation

Report paths are made repo-relative by stripping the repository working-directory
prefix. Override the stripped prefix with `--strip-prefix <PATH>` when the report
was generated under a different root (for example, a container build path that
differs from the checkout location).

## Sharded runs

When the instrumented test run is split across several concurrent CI jobs, each
job produces its own report. Pass them all to one aggregation step, one `--report`
each, and everything — the patch gate, `--fail-under-lines`, the deltas, the PR
comment — is computed over the merged result:

```bash
omni-dev coverage diff \
  --report shard-1.lcov --report shard-2.lcov --report shard-3.lcov \
  --baseline-report base.lcov \
  --fail-under-patch 80 --fail-under-lines 70
```

**How shards combine.** The merge is a *union*: the files are unioned, and for a
line present in several shards the larger hit count wins. In practice:

- A line **any** shard covered is covered, which is the point of sharding.
- A line only **one** shard instrumented is judged by that shard alone — it is
  neither covered nor uncovered "elsewhere". With a normal run every shard
  instruments the same code and lists every line, so this only matters when
  shards are built from different configurations.
- Hit counts are **maxed, not summed**. Nothing in the line-coverage output reads
  the count itself, so a total across shards is not available.
- Shard order never changes the output, and each shard may be in a different
  format (they are detected independently; an explicit `--report-format` applies
  to all of them).

**A bad shard cannot lower coverage unnoticed.** Merging ignores a shard that
contributes nothing, so a shard whose job failed would otherwise just make the
result look slightly worse. With more than one `--report`:

- a shard that is missing, unparseable, or has **no executable lines** fails the
  run, naming the shard (checked before `--ignore-filename-regex`, so a shard that
  only covers excluded files is not mistaken for a failed one);
- a shard whose absolute paths **all** fall outside the `--strip-prefix` root draws
  a warning on stderr: it was probably measured under a different workspace root,
  and its files would key under paths that exist nowhere in the repository. A shard
  with some in-tree paths is left alone, because reports legitimately name a few
  out-of-tree files (the standard library, vendored sources).

A single `--report` behaves exactly as it did before sharding existed: neither
check applies to it.

**Limits.**

- `--strip-prefix` is one value, so every shard has to share a workspace root.
  Shards from the same CI runner image do; a mix of, say, Linux and macOS runners
  does not, and the warning above is the signal.
- `--baseline-report` takes **one** report, so a baseline from a sharded run must
  be a single file. For lcov, joining the shards is equivalent to merging them —
  repeated records for a file are unioned the same way — but join them **with a
  newline between files**. `cargo llvm-cov` writes no newline after its final
  `end_of_record`, so a plain `cat shard-*.lcov > base.lcov` glues one shard's last
  record onto the next shard's first line, and the parser then silently drops that
  next file's header and understates coverage (about 0.14 pp on this repository's
  own test suite). Use `for f in shard-*.lcov; do cat "$f"; echo; done > base.lcov`.
  Unlike `--report`, joining gives no empty-shard check.
- The merge-base baseline recompute and `codecov.json` /
  `coverage-summary.txt` come from `cargo llvm-cov` in the reusable action and are
  not part of this command.

## Excluding files (CPU-conditional / non-deterministic coverage)

Some source files have coverage that is **inherently non-deterministic across
runs** — a region gated on a *runtime* CPU-feature check (`is_x86_feature_detected!`,
runtime SIMD-level dispatch) is compiled and counted in the denominator but only
*executed* on a host whose CPU has the instruction. When the baseline and head
runs draw different runner CPUs, the file's coverage swings with no source
change, surfacing as phantom deltas or in the "unchanged files also moved" note.

`--ignore-filename-regex <REGEX>` excludes files whose repo-relative path matches
any of the given regexes from **both** the head and baseline reports before the
diff. Filtering both sides symmetrically keeps the total, per-file deltas, patch
coverage, indirect-change list, and the `--fail-under-patch` gate computed over
the same denominator, so an excluded file can never produce a spurious "moved"
entry. Matching is **unanchored** (partial), the same semantics as
`cargo llvm-cov --ignore-filename-regex`, and is applied **after** `--strip-prefix`
normalisation, so patterns match the repo-relative path. The flag is repeatable
and comma-separated; an empty pattern is treated as a no-op (a bare regex would
match every path).

### Declaring the ignore-list persistently in repo config

Because these files are CPU-conditional forever — a property of the
*repository*, not of a single command line — the ignore-list can be declared once
in version control under the same `.omni-dev/` directory omni-dev already
discovers. Create `.omni-dev/coverage.yaml`:

```yaml
# .omni-dev/coverage.yaml
diff:
  # repo-relative path regexes; same unanchored semantics as
  # --ignore-filename-regex, applied after --strip-prefix normalisation
  ignore-filename-regex:
    - 'src/bits/popcount\.rs'   # AVX-512 VPOPCNTDQ path is CPU-gated
    - 'src/dsv/simd/.*'         # runtime SIMD dispatch fallback arms
    - 'src/yaml/simd/.*'
```

- **Discovery** follows the standard config resolution used by the other
  commands: `--context-dir <PATH>` wins, else `OMNI_DEV_CONFIG_DIR`, else a
  walk-up for the nearest `.omni-dev/` from the repo root, plus the usual
  `local/` override and XDG/home fallbacks.
- **Union, not replacement.** The config list is set-unioned with any
  `--ignore-filename-regex` passed on the command line, so the flag still works
  and only ever *adds* to the config list.
- **Empty / missing is a no-op** — behavior is unchanged when the file is absent
  or its list is empty. A present-but-malformed `coverage.yaml`, or an invalid
  regex from either source, is a hard error (it fails loudly rather than
  silently letting the excluded noise back in). Unknown keys are ignored, so the
  schema can grow without breaking older binaries.

This is the recommended path when omni-dev runs **through a wrapper** (such as
the `action-works/omni-dev-coverage-check` action) that does not thread the flag
through: omni-dev reads `.omni-dev/coverage.yaml` directly from the checkout, so
no wrapper change is needed.

## Excluding or tolerating regions in source

Excluding a whole file is usually too blunt. The noise is typically **one
function** — a CPU-gated dispatch helper is ten lines inside a two-hundred-line
file, and ignoring the file hides far more real coverage than noise.

Config cannot name the region either: line numbers are invalidated by every edit
above them, and function *extents* are absent from the lcov `FN:` records (they
carry a start line and a mangled symbol, nothing more). So the region is
delimited **in the source**, with a comment that travels with the code:

```rust
// omni-dev: coverage tolerate reason="CPU-gated: the avx512f arm only executes on Zen 4+ runners"
fn detect_fast_bmi2() -> bool {
    if is_x86_feature_detected!("avx512f") {
        return true;
    }
    cpuid_amd_zen3_or_later()
}
// omni-dev: coverage end
```

omni-dev scans **each revision's own source** — head from the working tree, base
from the base revision's blob — so no line number is ever recorded, and a region
that moves, grows, or disappears between base and head is handled by
construction.

### The two kinds

| Kind       | Effect on the percentages (total, per-file, patch)     | Effect on the delta signals (headline Δ, per-file Δ, notable, indirect) |
|------------|--------------------------------------------------------|-------------------------------------------------------------------------|
| `ignore`   | lines removed from **both** reports before analysis     | none — the lines no longer exist                                        |
| `tolerate` | lines kept; the reported coverage stays honest          | flips **masked**: each tolerated head line is scored with its baseline hit status |

`ignore` is the scoped twin of `ignore-filename-regex`. `tolerate` is the new
capability: **the number stays truthful, but the noise stops moving the needle.**

**Prefer `tolerate`.** Reach for `ignore` only when the code is genuinely
unreachable on CI and counting it in the denominator is simply wrong. A
tolerated region still shows its real coverage in the total and in the per-file
table; it just cannot manufacture a delta.

### Syntax

```text
omni-dev: coverage ignore reason="…"     … omni-dev: coverage end
omni-dev: coverage tolerate reason="…"   … omni-dev: coverage end
omni-dev: coverage ignore-line reason="…"
omni-dev: coverage tolerate-line reason="…"
```

- **Matching is a plain substring** anywhere on a line, so any comment syntax
  works — `//`, `#`, `--`, `<!-- -->`. Nothing parses the host language, which
  also means a marker inside a string literal *is* matched.
- **Both marker lines are inside the region.** They are comments, so they are
  never executable and never appear in a report.
- **`reason="…"` is mandatory** on every region start and single-line marker.
  Silencing has to be explained at the site.
- **Malformed markers are hard errors** naming `path:line`: a missing or empty
  reason, an unterminated quote, a nested region, an `end` with no start, or a
  region left open at end-of-file. An unterminated region is never widened
  silently to EOF — that would silence an unbounded amount of code nobody looked
  at.

### Check marker syntax locally

```bash
omni-dev coverage lint-markers
omni-dev coverage lint-markers src/bits/popcount.rs
omni-dev coverage lint-markers -C /path/to/repo src/bits/popcount.rs
```

With no paths, the command checks every tracked `.rs` file in the Git index
against its current working-tree contents, including staged additions and
unstaged edits. Deleted worktree files are skipped. Explicit paths select only
those files, including untracked files, and are relative to the repository root
unless absolute. Malformed markers print the same `path:line` diagnostics as
`coverage diff` and make the command exit nonzero. No coverage report or
`llvm-cov` run is needed. `scripts/build.sh` runs this check after formatting.

### How masking works, precisely

For each head file, the tolerated head lines are scored with the hit status of
their **base counterpart** (found through the same diff alignment used for
indirect changes; identity for a file the diff never touched). A tolerated line
with no counterpart — an added line — keeps its real status.

That effective coverage is what feeds the headline Δ, the per-file Δ, the
"unchanged files also moved" gate, and the indirect-change list. The **displayed
percentages are always the real, measured values**. So the motivating case
renders as `Total: 92.79% ⚪ 0 pp` rather than `🔴 -0.01 pp`, with `92.79%` still
the true number.

Two consequences worth stating outright:

- **Added lines inside a `tolerate` region stay in the patch-coverage
  denominator.** New code should still be tested even if it flaps later. Added
  lines inside an `ignore` region leave it, because they are gone from the head
  report entirely.
- **Without a baseline** (`--baseline-report` absent), `ignore` still applies —
  it shapes the total and the patch — while `tolerate` is a no-op, since there
  are no deltas to mask.

### Markers are never invisible

Whenever a marker applies, the markdown comment gains a collapsed note listing
each region's path, kind, line span as observed at head, and reason; the YAML and
JSON views carry the same information in a `markers` array. A reviewer can always
see what was silenced and why.

### Interaction with the file-level ignore-list

File-level exclusion wins. A file excluded by `--ignore-filename-regex` or
`coverage.yaml` is never read, so markers inside it are never scanned — and never
report errors.

## CI usage

In CI, prefer the reusable
[`action-works/omni-dev-coverage-check`](https://github.com/action-works/omni-dev-coverage-check)
action, which wraps the cargo-llvm-cov run, the merge-base baseline computation,
the sticky PR comment, and the line gate around this command. See
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml) for how this project
wires it up.

## Flag reference

| Flag | Purpose |
|------|---------|
| `--report <PATH>` | Head coverage report (required); repeat once per shard to merge a [sharded run](#sharded-runs) |
| `--report-format <FMT>` | Format of every `--report`: `auto` (default) \| `lcov` \| `llvm-cov-json` \| `cobertura` |
| `--base-ref <REV>` | Base revision (default: merge-base of `origin/main` and `HEAD`) |
| `--head-ref <REV>` | Head revision the report was measured at (default: `HEAD`) |
| `--baseline-report <PATH>` | Base-side report; enables project deltas + indirect changes |
| `--baseline-report-format <FMT>` | Format of `--baseline-report` (auto-detected by default) |
| `-o, --output <FMT>` | `markdown` (default) \| `yaml` \| `json` |
| `--fail-under-patch <PCT>` | Exit non-zero when patch coverage is below `<PCT>` |
| `--fail-under-lines <PCT>` | Exit non-zero when overall line coverage is below `<PCT>`, or the report has no executable lines |
| `--collapse-ranges` | Collapse consecutive uncovered new lines into ranges |
| `--all-files` | Report deltas/indirect changes for all files, not just touched ones |
| `--strip-prefix <PATH>` | Prefix stripped from report paths to make them repo-relative |
| `--ignore-filename-regex <REGEX>` | Exclude matching files from both reports (repeatable/comma-separated); unioned with `.omni-dev/coverage.yaml` |
| `--context-dir <PATH>` | Config dir searched for `coverage.yaml` (default: discovered `.omni-dev/`, honoring `OMNI_DEV_CONFIG_DIR`) |
| `-C, --repo <PATH>` | Operate as if started in `<PATH>` (like `git -C`) |
| `--artifact-url` / `--run-url` / `--commit-url` | Markdown-footer CI links |
| `--base-sha` / `--head-sha` | SHAs shown in the markdown `Comparing` line |
