# `.omni-dev/` Directory Contract

This document is the canonical reference for the `.omni-dev/` directory: the
inventory of recognised files, their formats, the precedence rules that decide
which copy wins when more than one exists, and the validation behaviour
omni-dev exhibits when a file is missing or malformed.

If you came here from a passing mention of `.omni-dev/commit-guidelines.md` (or
any other `.omni-dev/<file>`), this is the spec.

## Overview

`.omni-dev/` is the configuration directory omni-dev uses to learn about a
project. It is normally placed at the repository root, but discovery walks up
from the current working directory so omni-dev keeps working from anywhere
inside a repository tree.

Two distinct precedence systems are at work, depending on which file is being
loaded:

- **Chain A** — for `commit-guidelines.md`, `pr-guidelines.md`, `scopes.yaml`,
  and feature contexts. Resolves through `local/` overrides, project scope,
  XDG, and a legacy `~/.omni-dev/` fallback. Discussed in
  [Chain A — hierarchical resolution](#chain-a--hierarchical-resolution).
- **Chain B** — for `models.yaml`. A layered merge with deep-merge semantics
  driven by ADR-0022. Discussed in
  [Chain B — layered model catalog](#chain-b--layered-model-catalog).

Credentials in `~/.omni-dev/settings.json` are home-only and use neither
chain — see [settings.json](#settingsjson).

## Recognised files

| File | Purpose | Format | Scope | Precedence | Source |
|---|---|---|---|---|---|
| `commit-guidelines.md` | Commit-message rules consumed by `git commit message check` / `twiddle` | Markdown | project / user / XDG / `~/.omni-dev/` | Chain A | [`src/claude/context/discovery.rs:456`](../src/claude/context/discovery.rs#L456) |
| `pr-guidelines.md` | PR title / body rules consumed by `git pr` flows | Markdown | same as above | Chain A | [`src/claude/context/discovery.rs:471`](../src/claude/context/discovery.rs#L471) |
| `scopes.yaml` | Commit/PR scope vocabulary; merged with ecosystem defaults | YAML | same as above | Chain A | [`src/claude/context/discovery.rs:486`](../src/claude/context/discovery.rs#L486) |
| `models.yaml` | AI model catalog overrides | YAML | project / user / embedded | Chain B | [`src/claude/model_config.rs:178`](../src/claude/model_config.rs#L178) |
| `context/feature-contexts/*.yaml` | Per-feature AI prompt context fragments | YAML | inside the active `.omni-dev/` (plus `local/` override) | Chain A (variant) | [`src/claude/context/discovery.rs:502`](../src/claude/context/discovery.rs#L502) |
| `local/<any>` | Gitignored personal overrides for any of the above | follows the underlying file | personal | top of Chain A | [`src/claude/context/discovery.rs:44`](../src/claude/context/discovery.rs#L44) |
| `~/.omni-dev/settings.json` | API credentials and env-var fallbacks (Atlassian / Datadog / etc.); written `0600` inside a `0700` dir | JSON | user (home) only | none — single path | [`src/utils/settings.rs:130`](../src/utils/settings.rs#L130) |

Missing files are not an error. Each loader falls through to a lower-precedence
tier (or to the embedded default, where one exists) and omni-dev continues.

## Precedence

### Chain A — hierarchical resolution

Used for `commit-guidelines.md`, `pr-guidelines.md`, `scopes.yaml`, and the
`context/feature-contexts/` files. Implemented by `resolve_config_file` in
[`src/claude/context/discovery.rs:43-72`](../src/claude/context/discovery.rs#L43-L72).
The first existing file wins:

| Priority | Location | Purpose |
|---|---|---|
| 1 | `{dir}/local/{filename}` | Gitignored personal override |
| 2 | `{dir}/{filename}` | Shared project config |
| 3 | `$XDG_CONFIG_HOME/omni-dev/{filename}` | XDG global config (defaults to `~/.config/omni-dev/`) |
| 4 | `$HOME/.omni-dev/{filename}` | Legacy global fallback |

`{dir}` is itself resolved by `resolve_context_dir_with_source` in
[`src/claude/context/discovery.rs:128-147`](../src/claude/context/discovery.rs#L128-L147):

| Priority | Source | Description |
|---|---|---|
| 1 | `--context-dir` CLI flag | Explicit override; disables walk-up |
| 2 | `OMNI_DEV_CONFIG_DIR` env var | Environment override; disables walk-up |
| 3 | Walk-up discovery | Nearest `.omni-dev/` from CWD up to the repo root (`.git` boundary) |
| 4 | `.omni-dev` | Default fallback relative to CWD |

Walk-up stops at the first directory containing a `.git` entry (file or
directory) — discovery does not escape the repository. See
[ADR-0005](adrs/adr-0005.md).

### Chain B — layered model catalog

Used **only** for `models.yaml`. Implemented by
`ModelRegistry::load_layered_from_paths` in
[`src/claude/model_config.rs:195-227`](../src/claude/model_config.rs#L195-L227).

Unlike Chain A, layers are **deep-merged** rather than first-match. The
embedded catalog is always present, and higher-precedence layers override
individual model entries (matched by `api_identifier`) and provider settings
without forcing the user to redeclare the whole file.

| Priority | Layer | Notes |
|---|---|---|
| 1 (highest) | `OMNI_DEV_MODELS_YAML` env override | Short-circuits both project and user layers. Missing file falls back to embedded with a warning. |
| 2 | `./.omni-dev/models.yaml` (project, **CWD-relative**) | No walk-up; resolved by `default_project_path` at [`src/claude/model_config.rs:494-498`](../src/claude/model_config.rs#L494-L498). |
| 3 | `~/.omni-dev/models.yaml` (user) | Resolved by `default_user_path` at [`src/claude/model_config.rs:501-503`](../src/claude/model_config.rs#L501-L503). |
| 4 (lowest) | Embedded [`src/templates/models.yaml`](../src/templates/models.yaml) | Compile-time include via `include_str!`; cannot be removed. |

> **Caveat — no walk-up for `models.yaml`.** Chain B resolves the project
> layer from the current working directory only. If you `cd` into a
> sub-directory that does not itself contain `.omni-dev/models.yaml`, the
> project-layer overrides will not apply, even though Chain A's walk-up would
> have found the same `.omni-dev/`. Run omni-dev from the project root, or
> use `OMNI_DEV_MODELS_YAML` to point at the file explicitly.

See [ADR-0022](adrs/adr-0022.md) for the rationale behind the layered-merge
design.

### Settings (`~/.omni-dev/settings.json`)

`Settings::get_settings_path` in
[`src/utils/settings.rs:130`](../src/utils/settings.rs#L130) returns a
single path: `$HOME/.omni-dev/settings.json`. There is no walk-up, no
project-scoped equivalent, and no XDG fallback. This is intentional:
credentials are personal and should never be checked into a project's
`.omni-dev/`.

All writes go through `Settings::upsert_env_vars_in` /
`Settings::remove_env_vars_in` (the Atlassian and Datadog `auth login` /
`logout` flows). When a profile is active they target that profile's `env`
map, mirroring read-side resolution (issue #1116); otherwise they target the
base `env` map. Because the `env` maps hold credentials, the file is created
`0600` inside a `0700`
`~/.omni-dev/` and re-tightened on every write
([`src/utils/settings.rs:330`](../src/utils/settings.rs#L330), issue #1128) —
the same owner-only posture as the daemon's runtime state
([`src/daemon/paths.rs`](../src/daemon/paths.rs)).

## File specs

### `commit-guidelines.md`

Markdown describing the commit-message conventions the project enforces. Read
verbatim into the AI prompt; omni-dev does not parse it for structure. The
spec-by-example lives at
[`src/templates/default-commit-guidelines.md`](../src/templates/default-commit-guidelines.md);
the default is used when no project, XDG, or home copy exists.

A minimally useful file declares severity levels and a list of accepted types:

```markdown
## Severity Levels

| Severity | Sections                              |
|----------|---------------------------------------|
| error    | Commit Format, Types, Subject Line    |
| warning  | Body Guidelines                       |
| info     | Subject Line Style                    |

## Commit Format

Use conventional commit format: `<type>(<scope>): <description>`

## Types

| Type    | Use for                |
|---------|------------------------|
| `feat`  | New features           |
| `fix`   | Bug fixes              |
| `docs`  | Documentation changes  |

## Subject Line

- Imperative mood ("add", not "added")
- No trailing period
```

The `## Severity Levels` table is the single source of truth for whether a
section is `error`, `warning`, or `info` — see
[ADR-0012](adrs/adr-0012.md).

### `pr-guidelines.md`

Markdown describing PR title and body conventions. Same loading semantics as
`commit-guidelines.md`. There is no embedded default — the file is optional;
when absent, omni-dev falls back to behaviour driven solely by
`commit-guidelines.md` and ecosystem defaults. See this repository's own
[`.omni-dev/pr-guidelines.md`](../.omni-dev/pr-guidelines.md) for a
worked example.

### `scopes.yaml`

YAML enumerating valid commit/PR scopes for the project. Loaded into
`ScopesConfig` in
[`src/claude/context/discovery.rs:245`](../src/claude/context/discovery.rs#L245),
then merged with ecosystem defaults via
`merge_ecosystem_scopes` (see [ADR-0019](adrs/adr-0019.md)).

Minimal valid example:

```yaml
scopes:
  - name: "auth"
    description: "Authentication and authorization"
    examples:
      - "auth: add OAuth2 login"
      - "auth: fix session timeout"
    file_patterns:
      - "src/auth/**"
      - "auth/**"
```

All four fields (`name`, `description`, `examples`, `file_patterns`) are
required per scope. Extra fields are ignored.

### `models.yaml`

YAML overriding the embedded model catalog. The schema version is currently
`"1"` (constant `MODELS_SCHEMA_VERSION` at
[`src/claude/model_config.rs:26`](../src/claude/model_config.rs#L26)). Two
top-level keys are treated specially during merge:

- `models:` — a sequence; entries are matched by `api_identifier` and
  deep-merged with the corresponding embedded entry, or appended if new.
- `providers:` — a mapping; merged per provider name, so a user file can
  override just `default_model` without redeclaring every tier.

All other top-level keys are last-writer-wins.

Minimal valid example — overrides Anthropic's default model and adds one
provider-specific entry:

```yaml
version: "1"

providers:
  claude:
    default_model: "claude-opus-4-7"

models:
  - provider: "claude"
    model: "Claude Opus 4.7"
    api_identifier: "claude-opus-4-7"
    max_output_tokens: 64000
    input_context: 200000
    generation: 4.7
    tier: "flagship"
```

`api_identifier` is the only strictly-required field per model entry — entries
without it are skipped (see [Validation behaviour](#validation-behaviour)).
The full schema, including provider defaults and tier descriptions, is
documented by example in
[`src/templates/models.yaml`](../src/templates/models.yaml).

See [ADR-0022](adrs/adr-0022.md) for the design rationale.

### `settings.json`

JSON file at `~/.omni-dev/settings.json` containing an `env` map. Each key
corresponds to an environment variable name that omni-dev (or one of its
sub-clients) consults; values are used **as a fallback** for the actual
environment — real environment variables always win. See
[`src/utils/settings.rs:63-74`](../src/utils/settings.rs#L63-L74).

Minimal valid example:

```json
{
  "env": {}
}
```

Recognised keys written by built-in flows:

| Key | Written by | Read by |
|---|---|---|
| `ATLASSIAN_INSTANCE_URL` | `omni-dev atlassian auth login` ([`src/atlassian/auth.rs:152`](../src/atlassian/auth.rs#L152)) | [`load_credentials`](../src/atlassian/auth.rs#L40) |
| `ATLASSIAN_EMAIL` | same | same |
| `ATLASSIAN_API_TOKEN` | same | same |
| `DATADOG_API_KEY` | `omni-dev datadog auth login` ([`src/datadog/auth.rs:195`](../src/datadog/auth.rs#L195)) | [`load_credentials`](../src/datadog/auth.rs#L85) |
| `DATADOG_APP_KEY` | same | same |
| `DATADOG_SITE` | same | same |

Any other environment variable consulted via `Settings::get_env_var` can also
be set under the same `env` map (including API keys for `CLAUDE_API_KEY`,
`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc.).

### `local/`

`{dir}/local/` is a gitignored sub-directory that mirrors the layout of
`{dir}/` itself: any file under `local/` (e.g.
`.omni-dev/local/commit-guidelines.md`,
`.omni-dev/local/scopes.yaml`,
`.omni-dev/local/context/feature-contexts/*.yaml`) shadows the file at the
same relative path one level up. The repo root
[`.gitignore`](../.gitignore) contains a `.omni-dev/local/` entry that you
should mirror in any project that adopts this convention.

This is the intended escape hatch for personal overrides that should never
ship to teammates — for example, a per-developer override of
`commit-guidelines.md` for experimentation, or a `models.yaml` that points
the project at a different model provider locally. (Note that `models.yaml`
uses Chain B, not Chain A, so `local/models.yaml` is **not** consulted —
use `OMNI_DEV_MODELS_YAML` for a personal model override instead.)

### `context/feature-contexts/*.yaml`

Per-feature prompt context fragments. Files are loaded from both
`{dir}/context/feature-contexts/` and `{dir}/local/context/feature-contexts/`;
local entries override standard entries with the same filename. Implemented
by `load_feature_contexts` at
[`src/claude/context/discovery.rs:570-610`](../src/claude/context/discovery.rs#L570-L610).
Only `.yaml` and `.yml` files are picked up; the filename (minus extension)
becomes the feature key.

## Validation behaviour

omni-dev favours silent fallback over hard failure: missing files are
expected (defaults exist); malformed files log a warning and fall through to
the next tier. The strings below are the actual messages emitted by the
current source — you can grep your logs against them verbatim.

### `models.yaml`

| File:line | Level | Trigger | Message |
|---|---|---|---|
| [`src/claude/model_config.rs:207-210`](../src/claude/model_config.rs#L207-L210) | `warn!` | `OMNI_DEV_MODELS_YAML` points at a missing or unreadable file | `{OMNI_DEV_MODELS_YAML_ENV} points at {} but the file is missing or unreadable; falling back to embedded catalog` |
| [`src/claude/model_config.rs:246-248`](../src/claude/model_config.rs#L246-L248) | hard error (`anyhow!`) | Embedded YAML is malformed at compile time (compile-time invariant — only triggers if a build ships a broken `src/templates/models.yaml`) | `Embedded models.yaml is malformed at compile time: {e}` |
| [`src/claude/model_config.rs:250-252`](../src/claude/model_config.rs#L250-L252) | `error!` | User or project `models.yaml` has invalid YAML — non-fatal, falls through | `Malformed {source} models.yaml: {e}. Falling through to lower-precedence layers.` |
| [`src/claude/model_config.rs:514-517`](../src/claude/model_config.rs#L514-L517) | `error!` | Read error (permissions, I/O) on user or project `models.yaml` | `Failed to read {}: {e}. Falling through to lower-precedence layers.` |
| [`src/claude/model_config.rs:600-602`](../src/claude/model_config.rs#L600-L602) | `warn!` | A model entry lacks the required `api_identifier` field | `` Skipping model entry without `api_identifier` from {source} models.yaml `` |
| [`src/claude/model_config.rs:685-687`](../src/claude/model_config.rs#L685-L687) | `warn!` | User or project `models.yaml` has no `version:` field | `` {source} models.yaml has no `version:` field; assuming compatibility with schema version {MODELS_SCHEMA_VERSION}. Add `version: "{MODELS_SCHEMA_VERSION}"` to silence this warning. `` |
| [`src/claude/model_config.rs:691-693`](../src/claude/model_config.rs#L691-L693) | `warn!` | Declared schema version differs from `MODELS_SCHEMA_VERSION` | `{source} models.yaml declares schema version {v}; this build understands {MODELS_SCHEMA_VERSION}. Continuing — unrecognised fields may be ignored.` |

### `scopes.yaml`

| File:line | Level | Trigger | Message |
|---|---|---|---|
| [`src/claude/context/discovery.rs:241`](../src/claude/context/discovery.rs#L241) | `warn!` | File exists but cannot be read (permissions, I/O) — `load_project_scopes` returns `vec![]` | `Cannot read scopes file {}: {e}` |
| [`src/claude/context/discovery.rs:248-251`](../src/claude/context/discovery.rs#L248-L251) | `warn!` | File exists but is malformed YAML — `load_project_scopes` returns `vec![]` | `Ignoring malformed scopes file {}: {e}` |
| [`src/claude/context/discovery.rs:494-497`](../src/claude/context/discovery.rs#L494-L497) | `warn!` | Same condition, but encountered while loading the wider `.omni-dev/` config — `load_omni_dev_config` skips the scopes update | `Ignoring malformed scopes file {}: {e}` |

### Feature contexts

| File:line | Level | Trigger | Message |
|---|---|---|---|
| [`src/claude/context/discovery.rs:578-581`](../src/claude/context/discovery.rs#L578-L581) | `warn!` | Feature contexts directory is unreadable — directory is skipped | `Cannot read feature contexts dir {}: {e}` |
| [`src/claude/context/discovery.rs:600-603`](../src/claude/context/discovery.rs#L600-L603) | `warn!` | A `.yaml` / `.yml` file in the directory fails to deserialise as `FeatureContext` — that one file is skipped | `Ignoring malformed feature context {}: {e}` |

### `settings.json`

| File:line | Level | Trigger | Message |
|---|---|---|---|
| [`src/utils/settings.rs:122-123`](../src/utils/settings.rs#L122-L123) | `Result::Err` (propagated via `anyhow::Context`) | File exists but cannot be read | `Failed to read settings file: {}` |
| [`src/utils/settings.rs:125-126`](../src/utils/settings.rs#L125-L126) | `Result::Err` (propagated via `anyhow::Context`) | File exists but is not valid JSON or does not match the `Settings` schema | `Failed to parse settings file: {}` |

Missing `~/.omni-dev/settings.json` is silent (see
[`src/utils/settings.rs:116-119`](../src/utils/settings.rs#L116-L119)) — `load`
returns an empty `Settings { env: {} }`.

### `commit-guidelines.md` / `pr-guidelines.md`

No file-specific validation. Read errors during
`fs::read_to_string` are propagated as `Result::Err` via the standard
`anyhow` chain. Missing files are silent; the context loader simply leaves
`context.commit_guidelines` / `context.pr_guidelines` as `None` and the AI
prompt falls back to
[`src/templates/default-commit-guidelines.md`](../src/templates/default-commit-guidelines.md)
(`commit-guidelines.md` only — `pr-guidelines.md` has no embedded default).

## See also

- [ADR-0005](adrs/adr-0005.md) — Hierarchical Configuration Resolution with Walk-Up Discovery (Chain A).
- [ADR-0018](adrs/adr-0018.md) — Automatic Context Detection for Adaptive AI Prompts.
- [ADR-0019](adrs/adr-0019.md) — Ecosystem-Aware Scope Auto-Detection (`scopes.yaml` merge).
- [ADR-0022](adrs/adr-0022.md) — Layered Model Catalog with User and Project Overrides (Chain B).
- [Configuration Guide](configuration.md) — narrative walkthrough with worked examples.
- [User Guide](user-guide.md) — end-to-end setup including `.omni-dev/` bootstrap.
- [Style Guide](STYLE_GUIDE.md) — commit-message-authoring conventions for this repository.
