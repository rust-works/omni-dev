# CI Path-Split: Fast-Path Docs/Skills PRs Past Heavyweight Required Checks

A branch-protection required check is satisfied by **any** run that reports the
right status *name*. A cheap path-aware "gate" can therefore let documentation-
and skills-only PRs report every required check as green in seconds, while code
PRs still run the full Rust pipeline. This repo implements the pattern in
[`.github/workflows/ci.yml`](../.github/workflows/ci.yml) (issue #609); this doc
explains it so it can be reused elsewhere.

## The problem

Branch protection on `main` requires a set of checks — in this repo `Test
(stable)`, `Rustfmt`, `Clippy`, and `Docs`. Every one of them compiles the crate,
so a PR that only edits a `.md` file or a `.claude/skills/` manifest waits several
minutes for results that can't possibly change. That friction is exactly what
motivated distributing skills out-of-repo via symlinks
([`ai claude skills sync`](user-guide.md#ai-claude-skills--distribute-skills-across-repositories),
issue #558). Splitting CI removes the friction at its source: skills and docs can
live in the repo, travel with clones and worktrees for free, and still merge in
seconds.

## The principle

GitHub matches required status checks by **name**. Branch protection asks "did a
check named `Clippy` report success?" — it does not care *which* workflow or job
produced it. So there are two levers:

1. **Path filters** decide whether the expensive work runs for a given PR.
2. **The check name** stays constant either way, so the required context is always
   reported.

Combine them and a docs-only PR can publish a passing `Clippy` context without
ever invoking `cargo clippy`.

## The as-built pattern (single gate)

This repo uses a **single gate job** whose one boolean output every other job
keys off. There is no second workflow to keep in sync.

```yaml
on:
  pull_request:
    branches: [ main ]

jobs:
  # One source of truth for the split.
  gate:
    name: Gate
    runs-on: ubuntu-latest
    outputs:
      code: ${{ steps.decide.outputs.code }}
    steps:
    - uses: actions/checkout@v7
    - uses: dorny/paths-filter@v3
      id: filter
      if: github.event_name == 'pull_request'
      with:
        filters: |
          code:
            - '**'          # start from "everything is code"...
            - '!*.md'       # ...then subtract the doc-only paths
            - '!docs/**'
            - '!.claude/skills/**'
    - id: decide
      run: |
        # Non-PR events (main pushes, release tags) always run full CI.
        if [ "${{ github.event_name }}" != "pull_request" ]; then
          echo "code=true" >> "$GITHUB_OUTPUT"
        else
          echo "code=${{ steps.filter.outputs.code }}" >> "$GITHUB_OUTPUT"
        fi

  # A REQUIRED context: the job always runs (so the context is always
  # reported), but every step is guarded on the gate. On the fast path the
  # job is a genuine *passing* job with zero steps executed.
  clippy:
    name: Clippy
    needs: gate
    runs-on: ubuntu-latest
    steps:
    - uses: actions/checkout@v7
      if: needs.gate.outputs.code == 'true'
    - uses: dtolnay/rust-toolchain@stable
      if: needs.gate.outputs.code == 'true'
      with: { components: clippy }
    - name: Run clippy
      if: needs.gate.outputs.code == 'true'
      run: cargo clippy --all-targets -- -D warnings

  # A NON-required job: skip it wholesale with a job-level `if`.
  coverage:
    name: Coverage
    needs: gate
    if: needs.gate.outputs.code == 'true'
    runs-on: ubuntu-latest
    steps:
    - run: echo "full-CI-only work"
```

The mechanics that make it safe:

- **Required contexts guard steps, not the job.** `Test`, `Clippy`, `Rustfmt`, and
  `Docs` always run as jobs; on the fast path each simply executes no steps. That
  is a `success` conclusion — deliberately *not* a `skipped` one, because branch
  protection treats a skipped job's context inconsistently. Non-required jobs can
  safely use a job-level `if` and vanish entirely.
- **The filter is negative, so it fails safe.** It starts from `'**'` (everything
  counts as code) and *subtracts* known doc paths. Anything a contributor adds
  that isn't explicitly a doc — a new source dir, a config file, a build script —
  runs full CI by default. There is no "unmatched path produces no status"
  deadlock (see Caveats).
- **`*.md` is scoped to the repo root, not `**/*.md`.** Markdown under
  `src/templates/` is compiled into the binary with `include_str!` and covered by
  unit tests, so editing it *is* a code change. Fast-pathing `**/*.md` would let a
  template edit skip the tests that guard it. `ci.yml` is the source of truth for
  the exact glob set — mirror it there, don't re-derive it here.
- **Non-PR events always take the full path.** Pushes to `main` and release tags
  bypass the filter and run everything.

## Caveats

- **Path filters must be exhaustive.** If a PR touches paths matched by *neither*
  the code set nor the doc set, no run is produced, the required context never
  reports, and the PR is stuck "waiting" forever. The single-gate negative filter
  above is inherently exhaustive (`'**'` matches everything, minus a short
  subtract-list). A twin-workflow split (below) must add an explicit catch-all.
- **Matrix jobs expand into per-context names.** `Test` with a `rust: [stable,
  beta, nightly]` matrix publishes `Test (stable)`, `Test (beta)`, … — only the
  contexts actually listed in branch protection need to stay green, but each must
  still be *produced*, which the always-run-the-job approach guarantees.
- **Keep the doc glob and the branch-protection set in one head.** Adding a new
  required check, or a new top-level doc directory, means revisiting the gate.

## Alternatives (and why this repo didn't use them)

- **Twin workflows** — the pattern originally sketched in issue #598: a heavyweight
  `ci.yml` with `paths`/`paths-ignore`, plus a no-op `ci-skip.yml` that runs on the
  inverse paths and publishes stub jobs with the *same names* (`Clippy`, `Test
  (stable)`, …) that immediately succeed. It has a simple mental model, but the
  stub names must be hand-mirrored against the required-check set and silently rot
  the moment the matrix, a job name, or the protection set changes — a stale stub
  reports green for a check that no longer means anything, or a renamed required
  check is left with no producer on the fast path. The single gate keeps one
  source of truth, which is why this repo chose it.
- **Native `paths-ignore` + "skipped counts as success."** GitHub's branch-
  protection UI can treat a *skipped* required check as passing. Where that
  setting is available and enabled, a plain `paths-ignore:` on the workflow is the
  least-code option — **try it first.** The workflow-level split here is the
  fallback for when that setting isn't available or its skipped-vs-success
  semantics can't be relied on.

## When to prefer this over `skills sync`

This CI split and [`ai claude skills sync`](user-guide.md#ai-claude-skills--distribute-skills-across-repositories)
(issue #558) both target the same goal — iterating on skills without slow CI.
Prefer the CI split when:

- You control the CI workflow.
- Required checks are the main iteration blocker (not team policy or cross-repo
  skill sharing).
- Skills naturally belong with the project — its history, collaborators, and
  clones.

Prefer `skills sync` when CI config is outside your control, or skills genuinely
belong in a separate source-of-truth repo distributed to many consumers.

## See also

- [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) — the live
  implementation and the source of truth for the exact path globs.
- [`ai claude skills` — Distribute Skills Across Repositories](user-guide.md#ai-claude-skills--distribute-skills-across-repositories)
  — the symlink-based alternative to keeping skills in-repo.
