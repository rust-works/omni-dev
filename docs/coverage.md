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
- **Uncovered new lines** — an actionable `file:line` list of added lines with no
  coverage (optionally collapsed into ranges, e.g. `9-11`, with
  `--collapse-ranges`).
- **Per-file project deltas** — each touched file's overall covered-line change
  (requires a `--baseline-report`).
- **Indirect coverage changes** — coverage that flipped on lines the diff never
  touched (also requires a baseline).

Line coverage only; branch-coverage data in the report is ignored.

## Inputs

- `--report <PATH>` (**required**) — the head coverage report. Three formats are
  accepted and **auto-detected** from content: lcov trace files, llvm-cov JSON
  (`cargo llvm-cov report --json`), and Cobertura XML. Override detection with
  `--report-format <auto|lcov|llvm-cov-json|cobertura>`.
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

# 3. Gate a branch locally: fail if patch coverage is under 80%.
omni-dev coverage diff --report head.lcov --fail-under-patch 80

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

## Diff scoping

By default the project-delta and indirect-change sections are scoped to **files
the diff touches**. Coverage is measured by two independent instrumented runs
(baseline vs head), so lines in untouched files can flip covered↔uncovered purely
from run-to-run variance and surface as phantom deltas. Genuine cross-file
effects still surface via a magnitude-gated "notable unchanged" note. Pass
`--all-files` to restore the unscoped (noisier) report. Patch coverage and the
total are unaffected by this scoping.

## Path normalisation

Report paths are made repo-relative by stripping the repository working-directory
prefix. Override the stripped prefix with `--strip-prefix <PATH>` when the report
was generated under a different root (for example, a container build path that
differs from the checkout location).

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
| `--report <PATH>` | Head coverage report (required) |
| `--report-format <FMT>` | `auto` (default) \| `lcov` \| `llvm-cov-json` \| `cobertura` |
| `--base-ref <REV>` | Base revision (default: merge-base of `origin/main` and `HEAD`) |
| `--head-ref <REV>` | Head revision the report was measured at (default: `HEAD`) |
| `--baseline-report <PATH>` | Base-side report; enables project deltas + indirect changes |
| `--baseline-report-format <FMT>` | Format of `--baseline-report` (auto-detected by default) |
| `-o, --output <FMT>` | `markdown` (default) \| `yaml` \| `json` |
| `--fail-under-patch <PCT>` | Exit non-zero when patch coverage is below `<PCT>` |
| `--collapse-ranges` | Collapse consecutive uncovered new lines into ranges |
| `--all-files` | Report deltas/indirect changes for all files, not just touched ones |
| `--strip-prefix <PATH>` | Prefix stripped from report paths to make them repo-relative |
| `-C, --repo <PATH>` | Operate as if started in `<PATH>` (like `git -C`) |
| `--artifact-url` / `--run-url` / `--commit-url` | Markdown-footer CI links |
| `--base-sha` / `--head-sha` | SHAs shown in the markdown `Comparing` line |
