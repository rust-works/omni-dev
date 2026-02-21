# Config Internals

How omni-dev's configuration resolution system works, for contributors.

## Table of Contents

1. [Config Loading Architecture](#config-loading-architecture)
2. [Resolution Chain](#resolution-chain)
3. [Ecosystem Detection and Merge](#ecosystem-detection-and-merge)
4. [Pre-validated Checks Pattern](#pre-validated-checks-pattern)
5. [Prompt Architecture for Scope Checking](#prompt-architecture-for-scope-checking)
6. [Key Source Files](#key-source-files)

## Config Loading Architecture

Configuration loading has two entry points, both in
`src/claude/context/discovery.rs`:

### `ProjectDiscovery::discover()`

The full discovery pipeline used by the twiddle command. Runs four stages
in sequence:

1. `load_omni_dev_config()` — loads guidelines, PR guidelines, scopes, and
   feature contexts via `resolve_config_file()`.
2. `load_git_config()` — reserved for future git config integration.
3. `parse_documentation()` — extracts conventions from `CONTRIBUTING.md` and
   `README.md`.
4. `detect_ecosystem()` — sets `context.ecosystem` enum and calls
   `merge_ecosystem_scopes()` on the already-loaded scopes.

The result is a `ProjectContext` struct containing all configuration.

### `load_project_scopes()`

A standalone public function that returns just the scope list. Used by the
check and twiddle commands when they only need scopes (not full project
context). This function:

1. Resolves `scopes.yaml` via `resolve_config_file()`.
2. Parses the YAML into `Vec<ScopeDefinition>`.
3. Calls `merge_ecosystem_scopes()` to add ecosystem defaults.

This was extracted to unify three previously divergent code paths (see
issue #135). Both `CheckCommand::load_scopes()` and
`TwiddleCommand::load_check_scopes()` now delegate to this function.

Re-exported from `src/claude/context.rs`:
```rust
pub use discovery::{load_project_scopes, ProjectDiscovery};
```

## Config Directory Resolution

The `resolve_context_dir_with_source(override_dir)` function determines
which `.omni-dev/` directory to use. It returns both the resolved path and
a `ConfigDirSource` enum indicating how it was selected:

```
1. --context-dir CLI flag        — explicit override (ConfigDirSource::CliFlag)
2. OMNI_DEV_CONFIG_DIR env var   — environment override (ConfigDirSource::EnvVar)
3. Walk-up discovery             — nearest .omni-dev/ from CWD to repo root (ConfigDirSource::WalkUp)
4. .omni-dev (CWD-relative)      — default fallback (ConfigDirSource::Default)
```

The convenience wrapper `resolve_context_dir(override_dir)` returns just
the path, discarding the source.

### Walk-up discovery

The `walk_up_find_config_dir(start)` function walks from `start` upward
through parent directories. At each level it checks for a `.omni-dev/`
subdirectory. It stops at the repository root (identified by `.git`
directory or `.git` file for worktrees) and never escapes the repo. Returns
`None` if no `.omni-dev/` is found within the boundary.

Walk-up is disabled when `--context-dir` or `OMNI_DEV_CONFIG_DIR` is set,
ensuring explicit overrides have full control.

## Config File Resolution Chain

The `resolve_config_file(dir, filename)` function implements the four-tier
file priority chain:

```
1. {dir}/local/{filename}                    — local override (highest priority)
2. {dir}/{filename}                           — shared project config
3. $XDG_CONFIG_HOME/omni-dev/{filename}       — XDG global config
4. $HOME/.omni-dev/{filename}                 — legacy global defaults (lowest priority)
```

If no file exists at any tier, the function returns the project path
(`{dir}/{filename}`) as a default — callers check `.exists()` before reading.

### XDG compliance

The `xdg_config_dir()` helper returns the XDG config path for omni-dev.
It checks `$XDG_CONFIG_HOME/omni-dev/` first; if the variable is unset or
empty, it defaults to `$HOME/.config/omni-dev/`. It uses `std::env::var`
directly rather than `dirs::config_dir()`, which returns
`~/Library/Application Support/` on macOS — not the expected location for
a CLI tool.

Key design decisions:

- **File-level granularity**: Resolution is per-file, not per-directory.
  You can override just `scopes.yaml` locally while using the shared
  `commit-guidelines.md`.
- **No directory existence gate**: The resolution function runs regardless
  of whether the `.omni-dev/` directory exists (fixed in issue #134). Each
  file-loading operation checks existence individually.
- **Home directory fallback**: Uses the `dirs` crate for cross-platform
  home directory detection.
- **XDG before legacy**: The XDG path is checked before `$HOME/.omni-dev/`
  to encourage migration to the standard location.

## Ecosystem Detection and Merge

### Detection

`detect_ecosystem()` checks for marker files in priority order:

| Marker File | Ecosystem |
|-------------|-----------|
| `Cargo.toml` | Rust |
| `package.json` | Node |
| `pyproject.toml` or `requirements.txt` | Python |
| `go.mod` | Go |
| `pom.xml` or `build.gradle` | Java |
| (none found) | Generic |

Detection runs after config loading, so the ecosystem enum is set on the
`ProjectContext` for use by other systems.

### Merge semantics

`merge_ecosystem_scopes(scopes, repo_path)` adds default scopes for the
detected ecosystem. The critical guard is:

```rust
if !scopes.iter().any(|s| s.name == name) {
    scopes.push(/* ecosystem default */);
}
```

This means:
- User-defined scopes always win (matched by name).
- Ecosystem defaults only fill gaps.
- There is no partial merge — if you define `test` in your `scopes.yaml`,
  the entire ecosystem `test` definition is skipped, even if your version
  has different file patterns.

### Interaction with `ProjectDiscovery::discover()`

The discover pipeline calls `load_omni_dev_config()` first (which loads
YAML scopes into `context.valid_scopes`) and then `detect_ecosystem()`
(which calls `merge_ecosystem_scopes()` on those already-loaded scopes).
This sequence ensures user config takes precedence.

The standalone `load_project_scopes()` function replicates this same
sequence in a single call.

### Scope selection at generation time

There is no deterministic file-pattern-to-scope matching in the Rust code.
The full scope list (names, descriptions, file patterns) is serialized into
the AI prompt alongside the diff. The AI chooses which scope best matches
the changed files. This is a deliberate design choice — pattern matching
heuristics would be fragile and hard to maintain, while the AI can consider
context (description, examples, file semantics) that glob matching cannot.

The trade-off is that scope suggestions can vary between runs when multiple
scopes have overlapping file patterns.

## Pre-validated Checks Pattern

### Problem

AI models can hallucinate check results. If a project defines a custom scope
`claude`, the AI might flag it as invalid because `claude` isn't in its
training data as a conventional commit scope. This creates contradictions
where the generation command uses the scope correctly but the check command
flags it.

### Solution

Deterministic facts are computed **before** the AI runs and passed alongside
the commit data. The AI is instructed to treat these facts as authoritative.

### Implementation

**Data structure** (`src/git/commit.rs`):

```rust
pub struct CommitInfoForAI {
    // ... other fields ...
    /// Deterministic checks already performed; the LLM should treat these as authoritative.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_validated_checks: Vec<String>,
}
```

**Check execution** (`src/git/commit.rs:CommitInfoForAI::run_pre_validation_checks()`):

The method accepts `valid_scopes: &[ScopeDefinition]` and performs:

1. **Multi-scope format check**: If the scope contains commas without
   spaces, records "Scope format verified".
2. **Scope validity check**: If all scope parts are in the valid scopes list,
   records "Scope validity verified: '{scope}' is in the valid scopes list".

Convention: **Only passing checks are recorded.** If a scope is not valid,
nothing is added — the absence signals the AI is free to flag it.

**Call site** (`src/claude/client.rs`):

```rust
for commit in &mut ai_repo_view.commits {
    commit.run_pre_validation_checks(valid_scopes);
}
```

This runs just before generating the system prompt, so the pre-validated
checks are serialized into the YAML payload the AI receives.

## Prompt Architecture for Scope Checking

### `CHECK_SYSTEM_PROMPT`

The base constant (`src/claude/prompts.rs`) defines:

- **Severity levels**: How to interpret error/warning/info from the
  guidelines' severity table.
- **Accuracy checks**: Core value-add — compare message claims against diff
  content.
- **Response format**: Strict YAML structure with `passes`, `issues`,
  `suggestion`, and `summary` fields.
- **Pre-validated checks section**: Instructs the AI that
  `pre_validated_checks` values are authoritative and must not be
  contradicted.

### `generate_check_system_prompt_with_scopes()`

This function (`src/claude/prompts.rs`) builds the full prompt by combining:

1. The base `CHECK_SYSTEM_PROMPT`.
2. Project commit guidelines (or defaults).
3. The valid scopes list (if any).
4. **Scope Checking Rules** — the two-tier severity model.

### Two-tier scope checking

When valid scopes are provided, the function appends explicit rules:

1. **Scope Validity** (severity: `error`, section: "Accuracy"):
   The scope is NOT in the valid list. Only reported if `pre_validated_checks`
   does NOT confirm validity.

2. **Scope Appropriateness** (severity: `info`, section: "Scope
   Appropriateness"): The scope IS valid but a different scope might better
   match the changed files. This is a suggestion, never an error.

The key instruction: "If pre_validated_checks says the scope is valid, you
MUST NOT report it as an Accuracy error. You may suggest a more appropriate
scope at info level."

This separation was introduced in issue #136 to prevent the AI from
escalating appropriateness suggestions to error-level violations.

### Twiddle/check interaction model

Twiddle (generation) and check (validation) make independent AI calls
against the same scope list. The consistency guarantees are:

1. **Scope list**: Both load via `load_project_scopes()` (issue #135),
   so they always see the same valid scopes.
2. **Validity**: Deterministic pre-validation runs in both paths. If
   twiddle's AI picks a valid scope, check's `run_pre_validation_checks()`
   confirms it before the check AI runs — the check AI cannot contradict it.
3. **Appropriateness**: Independent AI judgment. Twiddle's AI might pick
   `core`; check's AI might suggest `cli`. This is by design — the check
   command surfaces info-level suggestions that the generation step may
   have missed. Since appropriateness is always info severity, it never
   causes `passes: false` on the commit.

The net effect: a twiddle-generated commit is guaranteed to pass check at
error and warning levels. It may receive info-level scope suggestions.

## Key Source Files

| Concept                  | File                              | Key Items                                                                                                          |
|--------------------------|-----------------------------------|--------------------------------------------------------------------------------------------------------------------|
| Config dir resolution    | `src/claude/context/discovery.rs` | `resolve_context_dir_with_source()`, `resolve_context_dir()`, `walk_up_find_config_dir()`, `ConfigDirSource`       |
| Config file resolution   | `src/claude/context/discovery.rs` | `resolve_config_file()`, `xdg_config_dir()`, `load_config_content()`, `ConfigSourceLabel`                          |
| Scope loading            | `src/claude/context/discovery.rs` | `load_project_scopes()`, `merge_ecosystem_scopes()`                                                                |
| Full discovery pipeline  | `src/claude/context/discovery.rs` | `ProjectDiscovery::discover()`, `load_omni_dev_config()`, `detect_ecosystem()`                                     |
| Module re-exports        | `src/claude/context.rs`           | `pub use discovery::{load_project_scopes, resolve_context_dir_with_source, ConfigDirSource, ConfigSourceLabel, …}` |
| Pre-validation           | `src/git/commit.rs`               | `CommitInfoForAI::run_pre_validation_checks()`, `pre_validated_checks` field                                       |
| Pre-validation call site | `src/claude/client.rs`            | Loop calling `run_pre_validation_checks(valid_scopes)`                                                             |
| Check prompt             | `src/claude/prompts.rs`           | `CHECK_SYSTEM_PROMPT`, `generate_check_system_prompt_with_scopes()`                                                |
| Scope data structure     | `src/data/context.rs`             | `ScopeDefinition`, `ProjectContext`                                                                                |
| Diagnostic display       | `src/cli/git/check.rs`            | `show_guidance_files_status()` — uses `resolve_context_dir_with_source()` and `config_source_label()`              |
| Diagnostic display       | `src/cli/git/twiddle.rs`          | `show_guidance_files_status()`, `show_check_guidance_files_status()`                                               |
| Diagnostic display       | `src/cli/git/create_pr.rs`        | `show_guidance_files_status()`                                                                                     |

## Related Issues

- **#134**: Removed directory existence gate that blocked global config fallback.
- **#135**: Unified three divergent scope-loading code paths into `load_project_scopes()`.
- **#136**: Added pre-validated scope checks and two-tier severity model.
- **#137**: This documentation (capturing learnings from #134–#136).
- **#173**: Consolidated duplicated config resolution into `discovery.rs`.
- **#174**: Added `OMNI_DEV_CONFIG_DIR` environment variable support.
- **#175**: Added XDG Base Directory compliance for global config tier.
- **#176**: Added walk-up discovery for `.omni-dev/` config directory.
- **#177**: Added `ConfigDirSource` diagnostic output showing how the config dir was selected.
