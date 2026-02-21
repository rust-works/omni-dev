# Configuration Best Practices

Practical guidance for writing effective omni-dev configuration, grounded in
real lessons learned from the project's own development.

## Table of Contents

1. [Config Resolution Priority](#config-resolution-priority)
2. [Ecosystem Defaults](#ecosystem-defaults)
3. [Writing Effective Scope Definitions](#writing-effective-scope-definitions)
4. [Writing Effective Commit Guidelines](#writing-effective-commit-guidelines)
5. [Scope Validity vs Appropriateness](#scope-validity-vs-appropriateness)
6. [Common Pitfalls](#common-pitfalls)
7. [Quality Checklist](#quality-checklist)

## Config Resolution Priority

### Config directory selection

omni-dev first determines *which* `.omni-dev/` directory to use:

| Priority | Source                        | Description                                |
|----------|-------------------------------|--------------------------------------------|
| 1        | `--context-dir` CLI flag      | Explicit override; disables walk-up        |
| 2        | `OMNI_DEV_CONFIG_DIR` env var | Environment override; disables walk-up     |
| 3        | Walk-up discovery             | Nearest `.omni-dev/` from CWD to repo root |
| 4        | `.omni-dev` (CWD-relative)    | Default fallback                           |

Walk-up discovery searches from the current working directory upward,
stopping at the repository root (`.git` boundary). The first directory
containing `.omni-dev/` wins. This makes monorepo subdirectories
automatically pick up their nearest config without needing `--context-dir`.

### Config file resolution

Once the config directory is selected, each configuration file is resolved
through a four-tier priority chain. The first file that exists wins:

| Priority    | Location                                 | Purpose                         |
|-------------|------------------------------------------|---------------------------------|
| 1 (highest) | `{dir}/local/{filename}`                 | Personal overrides (gitignored) |
| 2           | `{dir}/{filename}`                       | Shared project configuration    |
| 3           | `$XDG_CONFIG_HOME/omni-dev/{filename}`   | XDG global config               |
| 4 (lowest)  | `$HOME/.omni-dev/{filename}`             | Legacy global defaults          |

### How global config works

Global config applies to **all repositories** that don't have their own
config files. omni-dev checks two global locations:

1. **XDG path** (recommended): `$XDG_CONFIG_HOME/omni-dev/` — defaults to
   `$HOME/.config/omni-dev/` when `$XDG_CONFIG_HOME` is unset.
2. **Legacy path**: `$HOME/.omni-dev/` — still supported for backwards
   compatibility.

The XDG path is checked first. For new installations, use the XDG location.

A project does **not** need a `.omni-dev/` directory for omni-dev to work.
If none exists, the tool falls back to your global config. If neither exists,
ecosystem defaults are still applied (see below).

### When to use each tier

- **XDG global** (`$XDG_CONFIG_HOME/omni-dev/`): Generic defaults that work
  across most projects. Recommended for new installations. Keep these
  lightweight and broadly applicable.
- **Legacy global** (`$HOME/.omni-dev/`): Same purpose as XDG global.
  Existing setups continue to work.
- **Project** (`.omni-dev/`): Project-specific scopes, guidelines, and
  conventions. Commit this to version control so all team members share the
  same configuration.
- **Local** (`.omni-dev/local/`): Personal preferences that shouldn't be
  shared. Add `.omni-dev/local/` to `.gitignore`.
- **Env var** (`OMNI_DEV_CONFIG_DIR`): Useful for CI/CD or scripting when
  you need to point at a specific config directory without modifying
  command-line arguments.
- **Walk-up**: Automatic monorepo support — place `.omni-dev/` directories
  at package boundaries and run from within the package.

## Ecosystem Defaults

omni-dev auto-detects your project ecosystem by looking for marker files
(`Cargo.toml`, `package.json`, `pyproject.toml`, `go.mod`, `pom.xml`,
`build.gradle`) and provides default scopes tailored to that ecosystem.

### Supported ecosystems and their default scopes

**Rust** (detected via `Cargo.toml`):
`cargo`, `lib`, `cli`, `core`, `test`, `docs`, `ci`

**Node.js** (detected via `package.json`):
`deps`, `config`, `build`, `test`, `docs`

**Python** (detected via `pyproject.toml` or `requirements.txt`):
`deps`, `config`, `test`, `docs`

**Go** (detected via `go.mod`):
`mod`, `cmd`, `pkg`, `internal`, `test`, `docs`

**Java** (detected via `pom.xml` or `build.gradle`):
`build`, `config`, `test`, `docs`

### Merge behaviour

Ecosystem defaults are **additive** — they fill in gaps but never override
your custom scopes. If you define a scope with the same name as an ecosystem
default (e.g., you define `test` in your `scopes.yaml`), your definition
takes precedence and the ecosystem default for that name is skipped.

This means:
- A project with no `scopes.yaml` still gets useful scopes from ecosystem
  detection.
- A project with a partial `scopes.yaml` gets its custom scopes plus any
  ecosystem defaults that don't conflict by name.
- A project with comprehensive `scopes.yaml` effectively ignores ecosystem
  defaults (all names are already defined).

### When you don't need a `scopes.yaml` at all

If you're working on a standard project and the ecosystem defaults cover
your needs, you can skip creating a `scopes.yaml` entirely. The auto-detected
scopes will be used for both commit generation and validation.

## Writing Effective Scope Definitions

### Ordering does not affect resolution

The order of scopes in `scopes.yaml` does not change how omni-dev resolves
or validates them. Scopes are matched by **name**, not position. The only
effect of ordering is the sequence in which scopes appear in the prompt
sent to the AI. Ecosystem defaults are always appended after your
user-defined scopes.

### When multiple scopes match the same files

There is no deterministic "most specific pattern wins" logic. omni-dev sends
the full scope list (names, descriptions, file patterns) to the AI along
with the diff, and the AI decides which scope fits best. This means that
when two scopes both match the changed files, the result depends on the AI's
judgment — and that judgment can vary between runs.

The best way to get consistent scope suggestions is to minimise overlap so
only one scope clearly matches.

### Minimise file pattern overlap

When multiple scopes match the same files, omni-dev (and its AI) must guess
which scope is most appropriate. Overlapping patterns lead to inconsistent
scope suggestions.

**Problem — broad patterns that match everything:**

```yaml
# BAD: 'perf' matches every source file in the project
scopes:
  - name: perf
    description: "Performance improvements"
    file_patterns:
      - "src/**"
      - "**/*.rs"
      - "**/*.js"
      - "**/*.ts"

  - name: core
    description: "Core application logic"
    file_patterns:
      - "src/core/**"
```

Every file in `src/core/` matches both `perf` and `core`. The `perf` scope
is really a commit **type** (like `perf(core): optimize query`) not a scope
based on file location.

**Solution — use types for intent, scopes for location:**

```yaml
# GOOD: scopes define distinct areas of the codebase
scopes:
  - name: core
    description: "Core application logic and business rules"
    file_patterns:
      - "src/core/**"
      - "src/lib/**"

  - name: cli
    description: "Command-line interface"
    file_patterns:
      - "src/cli/**"
      - "src/main.rs"
```

Then express performance work through the commit type: `perf(core): optimize query`.

### Avoid overlapping CI/workflow patterns

Another common overlap is between `ci` and more specific workflow scopes:

```yaml
# BAD: ci and workflows both match the same files
scopes:
  - name: ci
    file_patterns:
      - ".github/**"

  - name: workflows
    file_patterns:
      - ".github/workflows/**"
```

Every file in `.github/workflows/` matches both scopes. Choose one approach:

```yaml
# GOOD: single scope for all CI
scopes:
  - name: ci
    description: "Continuous integration and deployment"
    file_patterns:
      - ".github/**"
      - ".gitlab-ci.yml"
```

Or if you truly need separation:

```yaml
# GOOD: non-overlapping split
scopes:
  - name: ci
    description: "CI configuration (non-workflow)"
    file_patterns:
      - ".github/dependabot.yml"
      - ".github/CODEOWNERS"

  - name: workflows
    description: "GitHub Actions workflows"
    file_patterns:
      - ".github/workflows/**"
```

### Write specific descriptions

Vague descriptions make it harder for the AI to suggest the right scope.

```yaml
# BAD: description doesn't help distinguish from other scopes
- name: core
  description: "Core stuff"

# GOOD: description clarifies what belongs here
- name: core
  description: "Core application logic and business rules (not CLI or API layers)"
```

### Provide meaningful examples

Examples teach the AI what kinds of commits belong to each scope:

```yaml
# GOOD: examples show real commit patterns
- name: claude
  description: "AI integration and prompt engineering"
  examples:
    - "feat(claude): add model registry for token limits"
    - "fix(claude): prevent prompt truncation on large diffs"
  file_patterns:
    - "src/claude/**"
```

## Writing Effective Commit Guidelines

The `commit-guidelines.md` file drives both commit generation (twiddle) and
commit validation (check). Getting it right is critical for consistent results.

### Include a severity levels table

The check command uses a severity levels table to determine which violations
are errors (blocking), warnings (advisory), or info (suggestions). Without
this table, all violations default to `warning`.

```markdown
## Severity Levels

| Severity | Sections                       |
|----------|--------------------------------|
| error    | Format, Subject Line, Accuracy |
| warning  | Content                        |
| info     | Style                          |
```

- **error**: Violations that cause the check command to exit with code 1
  (blocks CI).
- **warning**: Advisory issues — exit code 0 normally, exit code 2 with
  `--strict`.
- **info**: Suggestions only — never affect exit code.

### Write accuracy guardrails

The AI compares commit messages against the actual diff. Without explicit
rules about accuracy checking, the AI may over-flag or under-flag issues.

Good guidelines include rules like:

```markdown
## Accuracy

The commit type must match the actual changes:
- `feat` for new functionality only
- `fix` for bug fixes only
- `refactor` for behaviour-preserving restructuring
- `docs` for documentation-only changes

The scope must match the primary area of changed files.
The description must accurately reflect what the diff shows.
```

### Document multi-scope support

If your project allows multi-scope commits (e.g., `feat(cli,core): ...`),
document the format explicitly:

```markdown
## Multi-Scope Commits

When a change spans multiple scopes, list them comma-separated
without spaces: `type(scope1,scope2): description`
```

### Keep subject line limits consistent

Pick one limit and stick to it. Having "50 characters" in one place and
"72 characters" in another confuses both humans and AI.

## Scope Validity vs Appropriateness

The check command distinguishes between two kinds of scope issues:

### Scope validity (error severity)

A scope is **invalid** if it doesn't appear in the valid scopes list at all.
This is a deterministic check — omni-dev verifies it before the AI runs and
records it as an authoritative fact.

Example: if your valid scopes are `core`, `cli`, `api`, and someone uses
`foo(frontend): ...`, that's an error because `frontend` isn't in the list.

### Scope appropriateness (info severity)

A scope is **inappropriate** if it's in the valid list but a different scope
would better match the changed files. This is a judgment call made by the AI.

Example: using `core` when the changed files are all in `src/cli/` — the
scope is valid, but `cli` would be more appropriate. This is reported as an
info-level suggestion, not an error.

### How pre-validated checks prevent contradictions

omni-dev runs deterministic scope validation before sending data to the AI.
If a scope passes validation, the result is recorded in a `pre_validated_checks`
field that the AI treats as authoritative. This prevents the AI from
contradicting a known-good result (e.g., flagging a valid scope as invalid
because its training data doesn't include your custom scope names).

### How twiddle and check interact

Twiddle (generation) and check (validation) both use the same scope list
but make independent AI calls. This raises the question: can check disagree
with a scope that twiddle chose?

- **Scope validity**: No disagreement possible. Both commands load the same
  scope list via `load_project_scopes()`. Validity is verified
  deterministically before the AI runs. If twiddle picks a valid scope,
  check's pre-validation will confirm it's valid.
- **Scope appropriateness**: Disagreement is possible but harmless. Twiddle's
  AI might choose `core`, and check's AI might suggest `cli` would be a
  better fit. However, appropriateness is always info-level — it never
  marks a commit as failing and never affects CI exit codes.

The worst case: twiddle generates a commit with a valid scope, check passes
it but adds an info-level suggestion that a different scope might be more
appropriate. The commit still passes all checks.

### Enforcing check in CI

If you require `omni-dev git commit message check` to pass on every commit
(e.g., in a CI pipeline), understand the exit code semantics:

| Exit code | Meaning                        | When                       |
|-----------|--------------------------------|----------------------------|
| 0         | All commits pass               | No error or warning issues |
| 1         | At least one commit fails      | Error-level issues found   |
| 2         | Warnings present (strict mode) | `--strict` flag used       |

Because twiddle-generated commits are guaranteed to pass at error and
warning levels, a CI pipeline running check will not reject them. Info-level
scope appropriateness suggestions are reported but do not affect the exit
code.

For GitHub Actions, use
[omni-dev-commit-check](https://github.com/action-works/omni-dev-commit-check)
which wraps the check command with PR integration.

## Common Pitfalls

### 1. Overly broad file patterns

Scopes like `perf` with `file_patterns: ["src/**", "**/*.rs"]` match
virtually every file, making them useless for scope detection. Reserve broad
patterns for scopes that genuinely cover the entire codebase (e.g., `docs`
matching `**/*.md`).

### 2. Missing severity levels in guidelines

Without a severity levels table, the check command defaults everything to
`warning`. This means true format errors won't block CI, and minor style
issues will show as warnings instead of gentle suggestions.

### 3. Conflicting subject line limits

Having "50 characters" for the description but "72 characters" for the
subject line (type + scope + description combined) is a common source of
confusion. Define one clear rule for the total subject line length.

### 4. Stale global config

If you set up global config (at `$XDG_CONFIG_HOME/omni-dev/` or
`$HOME/.omni-dev/`) once and forget about it, it becomes a stale template
that doesn't reflect your current conventions. Periodically review your
global config, especially after updating project-level configurations.

### 5. Duplicate scope definitions between tiers

If your global config defines `core` with generic patterns and your project
config also defines `core` with specific patterns, only the project version
is used (higher priority file wins entirely). This is usually the desired
behaviour, but be aware that the global definition is completely ignored —
there is no merging within a single config file resolution.

### 6. Using scopes as commit types

Scopes like `perf`, `security`, or `cleanup` describe the **intent** of a
change, not the **area** of the codebase. These are better expressed as
commit types: `perf(core): ...`, `fix(auth): resolve security vulnerability`.

## Quality Checklist

Use this checklist to assess your configuration quality:

### Scope definitions (`scopes.yaml`)

- [ ] Each scope maps to a distinct area of the codebase
- [ ] File patterns don't significantly overlap between scopes
- [ ] No scope uses patterns broad enough to match the entire project
- [ ] Descriptions clearly explain what belongs in each scope
- [ ] Examples show realistic commit messages (2-3 per scope)
- [ ] Scopes represent code locations, not change intents

### Commit guidelines (`commit-guidelines.md`)

- [ ] Includes a severity levels table mapping sections to severities
- [ ] Subject line length limit is defined once and consistently
- [ ] Accuracy rules explicitly define what each commit type means
- [ ] Multi-scope format is documented (if supported)
- [ ] Good and bad examples are included
- [ ] Guidelines are specific enough to validate against

### Overall configuration

- [ ] `.omni-dev/local/` is in `.gitignore`
- [ ] Global config (`$XDG_CONFIG_HOME/omni-dev/` or `$HOME/.omni-dev/`) is reviewed periodically
- [ ] Project config is committed to version control
- [ ] Team members know where to find and how to update configuration
- [ ] Monorepo packages have `.omni-dev/` at the right directory level (if using walk-up)
