# Extracting Google Workspace functionality into gwi

**Status:** In Progress — phases 0-2 done, phase 3 (extract) not started
**ADRs:** [ADR-0095](../adrs/adr-0095.md) · [gwi ADR-0001](https://github.com/rust-works/gwi/blob/main/docs/adrs/adr-0001.md)

Tracking issue: [#2203](https://github.com/rust-works/omni-dev/issues/2203). New project:
[rust-works/gwi](https://github.com/rust-works/gwi) (*Google Workspace Interface*). This
file holds the inventory and procedures; the two ADRs hold the decisions.

## Decisions

| # | Question                | Outcome                                                                                              |
|---|-------------------------|------------------------------------------------------------------------------------------------------|
| 1 | Shared infrastructure   | **Fork** what gwi needs at a recorded commit. No shared crate, no dependency either way.             |
| 2 | Configuration and state | **Own `~/.gwi/`** plus an import command that copies from omni-dev. No read-fallback.                |
| 3 | Audit and request log   | **Separate**: gwi writes its own `log.jsonl` and fail-closed `audit.jsonl`. History is not merged.   |
| 4 | MCP                     | **gwi ships `gwi-mcp`**; omni-dev's MCP server drops the 23 tools.                                   |
| 5 | Compatibility window    | One omni-dev release with hidden `gmail`/`drive` stubs that point at gwi, then deletion. *(default)* |
| 6 | Naming                  | Crate `gwi`, binaries `gwi` and `gwi-mcp`. *(default)*                                               |
| 7 | Git history             | Preserve with `git filter-repo` for the moved paths. *(default)*                                     |
| 8 | Live testing            | Stays manual and local; gwi CI holds no Google credentials. *(default)*                              |

Decisions 1-4 were chosen explicitly; those marked *(default)* were proposed in phase 1 and
stand unless changed in review.

## Measured coupling

All figures are from the tree at `83085b7cb`.

**Shared modules gwi forks** (line counts include inline tests):

| Module                                                                                  | Lines | Notes                                                                     |
|-----------------------------------------------------------------------------------------|-------|---------------------------------------------------------------------------|
| `utils/secret.rs`, `utils/secret_env.rs` (+ dir)                                        | ~1.9k | STYLE-0030, ADR-0089/0090; the `SECRET_ENV_VARS` registry and grep guards |
| `utils/env.rs`                                                                          | 211   | `EnvSource` seam (STYLE-0028)                                             |
| `utils/settings.rs`                                                                     | 3.4k  | Fork only the loader, profile resolution and the raw-JSON account upsert  |
| `request_log.rs`                                                                        | 4.6k  | Generic invocation/HTTP records, the audit sink and its collision guards  |
| `daemon/paths.rs`                                                                       | 589   | Fork only `FileLock`, `*_0600`/`*_0700` helpers and the exists-error test |
| `utils/{http,rate_limit,multipart,terminal,path,browser_command}.rs`                    | ~1.1k | `multipart` and `browser_command` carry Google-specific code              |
| `test_support.rs`                                                                       | 799   | Env-injection harness                                                     |
| `browser::auth::generate_token`, `cli::format`, `cli::log::parse_since`, `cli::confirm` | small | PKCE state/verifier, CLI plumbing                                         |

The shared modules depend on almost nothing else in omni-dev; the one tangle is
`settings.rs` → `drive::write_gate`/`drive::auth`, which gwi resolves by owning those types.

**Inbound edges omni-dev must cut** (five files): `utils/settings.rs`, `request_log.rs`,
`cli.rs`, `utils/browser_command.rs`, `utils/multipart.rs`. The daemon, sessions and
worktrees code host no Gmail/Drive work.

**Dependencies that may become Google-only** (confirm with `cargo machete` after removal):
`mail-parser`, `mail-builder`, `htmd`, `csv`. Not Google-only: `mime_guess` (Atlassian),
`aws-lc-rs` (Snowflake, GitHub app auth), `sha2`, `base64`, `reqwest`.

## What moves

Code, tests and docs as listed in #2203 (`src/gmail*`, `src/drive*`, `src/cli/{gmail,drive}*`,
`src/mcp/{gmail,drive*}_tools.rs`, `tests/drive_lease_exit_code_test.rs`, `docs/gmail*.md`,
`docs/drive*.md`).

**ADRs.** A scan of every ADR for Gmail/Drive/Sheets/Docs/Slides content found 27 that are
Google-centred: **0063-0071, 0073-0086 and 0091-0094**. That is three more than #2203
listed (0074, 0079, 0083). Four of them have titled filenames (`adr-0077-…`, `adr-0082-…`,
`adr-0084-…`, `adr-0086-…`), which a glob for `adr-NNNN.md` misses. They are relocated and
the omni-dev copy becomes a stub linking to gwi.

ADRs that **stay**: 0023 (ADF schema) and 0028 and 0031 mention Google only in passing.
0089 (`<NAME>_FILE`) and 0090 (`<NAME>_COMMAND`) govern secret handling for the whole
project; gwi forks the practice, so its secret-handling ADR cites them rather than moving
them. ADR-0063 and ADR-0066 discuss profiles and secrets in general terms but are
Gmail-specific decisions, so they move.

## gwi layout and import

| Item                    | omni-dev today                                                                    | gwi                                                   |
|-------------------------|-----------------------------------------------------------------------------------|-------------------------------------------------------|
| Settings                | `~/.omni-dev/settings.json` blocks `gmail`, `drive`, `lease`                      | `~/.gwi/settings.json`, same three blocks             |
| Lease ledger            | `<state_dir>/omni-dev/lease-ledger.jsonl` (+ `.lock`)                             | `<state_dir>/gwi/lease-ledger.jsonl`                  |
| Request / audit log     | `<state_dir>/omni-dev/{log,audit}.jsonl`                                          | `<state_dir>/gwi/{log,audit}.jsonl`                   |
| Gmail insert/sync state | Per archive directory (`ledger_path(&archive_dir)`, manifests, `gmail-sync.yaml`) | Unchanged: lives with the archive, nothing to migrate |
| Credentials             | Secret values via `_FILE`/`_COMMAND` references, tokens in settings               | References carried over verbatim                      |

Environment variables: the 17 unprefixed `GMAIL_*`/`DRIVE_*` names keep their spelling. The
`OMNI_DEV_*` names read by the moved code (`_GMAIL_ACCOUNT`, `_DRIVE_ACCOUNT`, `_PROFILE`,
`_CONFIG_DIR`, `_LOG_FILE`, `_LOG_DISABLE`, `_DRIVE_LEASE_*`, `_LEASE_LOCK_WAIT_SECS`) become
`GWI_*` with no fallback. Phase 3 enumerates them from the fork rather than from this list.

**Import principles** (finalised in phase 3): copy, never move; idempotent; refuses to
overwrite existing gwi state without `--force`; takes the ledger's advisory lock while
copying; reports every `_file`/`_command` reference that points inside `~/.omni-dev/` so
the user can relocate it; copies no audit history (forensic records keep their provenance).

## Fork baseline

The shared modules are forked from omni-dev `main` at **`819907d14`** (the merge of
ADR-0095). Phase 3 updates this line if it forks from a later commit, and the gwi fork
records the same SHA in its commit message. Until gwi has a release, a fix to one of the
forked modules in omni-dev (`utils/secret*`, `utils/env.rs`, `utils/settings.rs`,
`request_log.rs`, `daemon/paths.rs`, `utils/{http,rate_limit,multipart,terminal,path}.rs`,
`test_support.rs`) must be carried into the fork by hand.

## Revised phases

0. **Reserve and scaffold.** Done except the crates.io publish, which needs a token.
1. **Design.** This plan and the two ADRs.
2. **Prerequisites in omni-dev (no refactor).** Done. With no shared crate there is nothing
   to invert, so this phase only pins what the removal must not break, with literal
   fixtures that survive the Google writers going away:
   `google_blocks_survive_every_non_google_settings_writer` and
   `settings_with_a_block_omni_dev_does_not_model_still_load` (`utils/settings.rs`) cover
   settings; `legacy_drive_mutation_line_still_decodes_with_its_kind_and_context`,
   `legacy_audit_line_still_decodes_with_its_kind_and_context` (`request_log.rs`) and
   `backlog_renders_legacy_drive_mutation_and_audit_lines` (`cli/log/stream.rs`) cover the
   log. The fixture lines were captured from the 0.46.0 builders. See *Fork baseline*.
3. **Extract.** Import history with `git filter-repo` (rewriting bare `#N` references to
   `rust-works/omni-dev#N`); fork the shared modules; add the import command and
   `gwi-mcp`; port docs, ADRs and the live-test notes; publish a real release.
4. **Remove from omni-dev.** Stub release, then deletion; `Removed` bullet under
   `[Unreleased]`; `update-snapshots`; relocate the ADRs; check crate size and `cargo deny`.

## Open items and risks

- `gwi` on crates.io is unreserved until the 0.0.1 placeholder is published.
- Importing gwi's history into a repository whose `main` already has scaffold commits needs
  either a one-time force-push (branch protection must be toggled) or an unrelated-history
  merge. Prefer the force-push while `main` is a single commit, and treat it as a
  user-authorised step.
- Live testing uses a personal Google account and a test folder. Neither is recorded in the
  public repository; gwi documents the procedure with placeholders.
- macOS-only code (Touch ID lease authentication, `authenticate`) and its target-specific
  dependencies move with the lease code and must keep their `unsafe` allowances scoped.
- Security-sensitive code (write gate, folder permission rules, leases, secret handling)
  must keep its grep-guard tests and invariants through the move.
