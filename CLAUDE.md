# Claude AI Assistant Guide

This document provides guidance for AI assistants (particularly Claude) working with the omni-dev project.

## Project Overview

omni-dev is a powerful Git commit message analysis and amendment toolkit written in Rust. It provides:

- Comprehensive commit analysis with YAML output
- Branch-aware commit analysis 
- Safe commit message amendment capabilities
- GitHub integration for PR and remote information
- Conventional commit detection and suggestions

## Key Files and Structure

### Core Source Files
- `src/main.rs` - CLI entry point
- `src/lib.rs` - Library exports
- `src/cli/` - Command-line interface implementation
- `src/cli/atlassian/` - Atlassian JIRA/Confluence CLI commands
- `src/atlassian/` - Atlassian API client, ADF/JFM conversion, document format
- `src/data/` - Data structures and YAML output formatting
- `src/core/` - Core application logic
- `src/utils/` - Utility functions

### Configuration
- `Cargo.toml` - Rust package configuration and dependencies
- `.github/` - GitHub Actions CI/CD workflows
- `.claude/skills/` - Claude skill definitions

### Documentation
- `README.md` - Main project documentation
- `CHANGELOG.md` - Version history and changes
- `CONTRIBUTING.md` - Contribution guidelines
- `docs/STYLE_GUIDE.md` - Project conventions for code, documentation, and other artifacts
- `docs/RELEASE.md` - Release process documentation
- `docs/plan/` - Project planning and specifications

## Development Workflow

### Code Quality Standards
- **Build Script**: Run `./scripts/build.sh` for complete validation (recommended)
- **Tests**: Run `cargo test` before commits
- **Linting**: Use `cargo clippy -- -D warnings` for code quality
- **Formatting**: Apply `cargo fmt` for consistent style
- **Documentation**: Maintain doc comments for public APIs

### Commit Message Format
Follow conventional commit format:
```
<type>(<scope>): <description>

<body>

<footer>
```

Common types: `feat`, `fix`, `docs`, `chore`, `refactor`, `test`

### Branch Strategy
- `main` - Production-ready code
- Feature branches - `feature/description` or `username/feature-description`
- Release branches - Tagged as `vX.Y.Z`

## AI Assistant Guidelines

### Code Changes
1. **Read Before Writing**: Always read existing files before making changes
2. **Follow the Style Guide**: Before writing or reviewing code, documentation, or other project artifacts, consult [docs/STYLE_GUIDE.md](docs/STYLE_GUIDE.md). Use the task-to-tag lookup table at the top of the guide to identify relevant tags, then search for those tags (e.g., `grep "Tags:.*code-style" docs/STYLE_GUIDE.md`). Read and follow the matched rules. Do not skip this step.
3. **Configuration Changes**: When modifying config loading or scope resolution, consult [docs/configuration-best-practices.md](docs/configuration-best-practices.md) and [docs/plan/config-internals.md](docs/plan/config-internals.md)
4. **Test Changes**: Run tests after modifications
5. **CLI Surface Changes**: After any change to `src/cli/**`, `src/main.rs`, or any `#[derive(Parser)]` / `#[derive(Subcommand)]` / `#[arg(...)]` site, invoke the [`update-snapshots`](.claude/skills/update-snapshots/SKILL.md) skill to review and update `insta` golden snapshots — most often [tests/snapshots/integration_test__help_all_output.snap](tests/snapshots/integration_test__help_all_output.snap). Do **not** assume `cargo test` passing in isolation surfaces drift before you've inspected the new snapshot: golden tests fail loudly, but only after the full suite has run, and the fix (`cargo insta accept`) must only be applied when the diff matches the *intended* CLI change. If the diff contains anything you did not intend, investigate the regression instead of accepting.
6. **Conventional Commits**: Use proper commit message format (see `.omni-dev/commit-guidelines.md`)
7. **Incremental Changes**: Make focused, reviewable changes

### Release Process
When preparing releases, follow the comprehensive guide in [docs/RELEASE.md](docs/RELEASE.md):

1. Update version in `Cargo.toml`
2. Update `CHANGELOG.md` with release notes
3. Run quality checks (`cargo test`, `cargo clippy`)
4. Commit changes with conventional commit format
5. Create annotated git tag
6. Push commits and tag
7. Create GitHub release
8. Publish to crates.io

### Understanding YAML Output
The project generates structured YAML output with field presence tracking:

- **Field Documentation**: Each output field is documented with presence indicators
- **AI Guidance**: Look for `present: true` fields in the explanation section
- **Dynamic Tracking**: The `update_field_presence()` method tracks which fields are available

### AI Response Parsing - CRITICAL UNDERSTANDING
**IMPORTANT**: When working with AI-generated responses in this project, understand the correct data structure:

- **AI responses are VALID YAML** with `title` and `description` fields
- **The `description` field VALUE contains markdown content**, including embedded code blocks
- **Embedded ```yaml blocks are CONTENT, not structure** - they're part of the description string
- **NEVER attempt to "unwrap" or extract content between markdown code fences**
- **Use simple `content.trim()` parsing** - complex extraction logic breaks the YAML structure

**Example of correct AI response structure**:
```yaml
title: "PR title here"
description: |
  # Section
  
  ```yaml
  - some: nested content
  ```
  
  This is all part of the description field value.
```

**Common Mistake**: Treating embedded ```yaml blocks as if they need extraction. They don't - they're just content within the description field.

**Correct Approach**: Parse the entire response as YAML directly. The markdown formatting (including code blocks) is the intended content of the description field.

### AI Model Configuration
The project includes a comprehensive model registry system:

- **Model Registry**: `src/claude/model_config.rs` manages AI model specifications
- **Model Templates**: `src/templates/models.yaml` defines supported Claude models with token limits
- **Fuzzy Matching**: Supports various identifier formats (Bedrock, AWS, regional)
- **Configuration Commands**: Use `omni-dev config models show` to view available models
- **Dynamic Limits**: Token limits are automatically applied based on model specifications

### AI Backend Dispatch
Backends are selected inside `src/claude/client.rs::create_default_claude_client` in this order:

1. `OMNI_DEV_AI_BACKEND=claude-cli` (or `--ai-backend claude-cli`) → `ClaudeCliAiClient` in `src/claude/ai/claude_cli.rs`.
2. `USE_OLLAMA=true` → `OpenAiAiClient::new_ollama` in `src/claude/ai/openai.rs`.
3. `USE_OPENAI=true` → `OpenAiAiClient::new_openai` in `src/claude/ai/openai.rs`.
4. `CLAUDE_CODE_USE_BEDROCK=true` → `BedrockAiClient` in `src/claude/ai/bedrock.rs`.
5. Default → `ClaudeAiClient` in `src/claude/ai/claude.rs` (direct Anthropic API).

Preflight (`src/utils/preflight.rs`) mirrors this switch and must change in lock-step when adding backends.

User-facing details — required env vars, model selection, Claude CLI sandbox semantics, the `--claude-cli-allow-tools` / `--claude-cli-allow-mcp` escape hatches, the `--claude-cli-max-budget-usd` spending cap, and per-backend troubleshooting — live in [docs/ai-backends.md](docs/ai-backends.md). Keep it in sync when changing any of those surfaces.

Architectural rationale for the sandboxed `claude-cli` subprocess backend — threat model, sandbox flag choices, escape-hatch design, budget-cap enforcement — lives in [ADR-0028](docs/adrs/adr-0028.md).

Dev-only notes:
- `ClaudeCliAiClient::run` is the warn site for both escape hatches, the INFO-level `total_cost_usd` log, and the post-response WARN when reported cost exceeds the configured cap. `ClaudeCliAiClient::max_budget_from_value` is the warn site for a set-but-invalid cap value (#1135).
- `--beta-header` is ignored for the `claude-cli` backend (`claude`'s `--betas` flag has different semantics).

### Browser Bridge
The `omni-dev browser bridge` command tree drives HTTP requests **through an authenticated browser tab** (Grafana/Loki, SSO-gated dashboards) without exfiltrating the browser's cookies/tokens — a *confused deputy by design*. It is a two-plane local server joined by an `id`-keyed correlator:

- `src/cli/browser.rs` + `src/cli/browser/` — the CLI surface: `bridge serve` (`bridge.rs`, the long-lived server), `bridge request` (`request.rs`, the thin client), and `bridge harvest <platform> <object>` (`harvest.rs`, best-effort scrapers). Both clients send a `ControlRequest` to `POST /__bridge/request` via the shared `src/browser/client.rs::BridgeClient` rather than opening their own socket.
- `src/browser/harvest/` — the harvest engines (`facebook.rs` = own-timeline pagination). These drive **reverse-engineered, undocumented** site internals: best-effort, re-harvest every volatile `doc_id`/token/provider flag per run (never hardcoded), fail with staged actionable errors on drift, and only ever use the connected tab's own session. The Facebook recipe is documented in [docs/browser-bridge.md](docs/browser-bridge.md) and issue #922.
- `src/browser/bridge.rs` — server core: the HTTP control plane (axum, default `127.0.0.1:9998`), the WebSocket plane the browser connects to (default `127.0.0.1:9999`), the `Correlator` (per-`id` channel), the transparent proxy, and `dispatch`/`start_stream`.
- `src/browser/protocol.rs` — the wire types (`ControlRequest`, `Command`, `BrowserReply`/`ResponseEnvelope`, the streaming `StreamItem`/`StreamLine`/`CancelCommand`, `StatusResponse`/`TabInfo`). New optional fields use `#[serde(default, skip_serializing_if = ...)]` to keep older clients byte-identical on the wire.
- `src/browser/auth.rs` — the **load-bearing** security primitives: token generation/resolution, `constant_time_eq`, the `X-Omni-Bridge` / `X-Omni-Bridge-Target` header constants, Host/Origin/Sec-Fetch-Site guards, and `validate_outbound_url` (server-side outbound scope; the in-page snippet is never trusted).
- `src/templates/browser-bridge.js` — the snippet pasted into the DevTools console (rendered by `src/browser/snippet.rs`); it reads `cmd.stream` / `cmd.credentials` and base64-encodes non-text bodies.

The security model is **core, not an add-on**: both planes are authenticated and default-closed. When touching the trust boundary (auth guards, outbound scope, token handling, the planes), keep [ADR-0036](docs/adrs/adr-0036.md) and the operator guide [docs/browser-bridge.md](docs/browser-bridge.md) in sync. Changes to the CLI surface require the [`update-snapshots`](.claude/skills/update-snapshots/SKILL.md) skill (see Code Changes §5).

The bridge can also be hosted by the **omni-dev daemon** (see the [Daemon](#daemon) section below) as its first migrated service. The daemon supervises lifecycle and adds a menu-bar surface; it does **not** touch this trust boundary — `BridgeService` (`src/daemon/services/bridge.rs`) wraps the same `BridgeServer` planes verbatim, and standalone `serve` keeps working.

### Daemon
The `omni-dev daemon` command tree hosts long-lived **services** inside one supervised process behind a Unix-domain control socket, with an optional macOS menu-bar shell. The browser bridge is the first service migrated onto it (a trivial `echo` service exercises the framework in tests). Architectural rationale — the service abstraction, the control socket, single-instance supervision, the `run`/launcher split, and the main-thread/runtime inversion for the tray — lives in [ADR-0039](docs/adrs/adr-0039.md).

- `src/daemon/service.rs` — the `DaemonService` trait (`name`/`handle`/`menu`/`menu_action`/`status`/`shutdown`) plus the menu/status types (`MenuSnapshot`, `MenuItem`, `MenuAction`, `ServiceStatus`).
- `src/daemon/registry.rs` — `ServiceRegistry`: routes a request envelope's `service` field to the matching service; iterates them for `status`/`menu`/`shutdown`.
- `src/daemon/protocol.rs` — the Unix-socket wire types `DaemonEnvelope { service, op, payload }` / `DaemonReply { ok, payload, error }`, newline-delimited JSON (`tokio_util` `LinesCodec`). A `service` of `None` or the reserved `"daemon"` targets the built-in ops `ping` / `status` / `shutdown`.
- `src/daemon/server.rs` + `single_instance.rs` + `lifecycle.rs` — the accept loop and dispatch; `acquire_listener` adopts the launchd-activated fd on macOS (else the exclusive socket bind, which **is** the single-instance lock: `ping`-probe + stale-socket reclaim); on the launchd path shutdown leaves the socket inode for launchd (the unlink is gated to the self-bound path); `SIGTERM`/`SIGINT` cancel a shared `CancellationToken` for graceful shutdown.
- `src/daemon/paths.rs` — runtime paths via `dirs::data_dir()` (`<data>/omni-dev/`, **not** `~/.omni-dev`): `daemon.sock` and `bridge.token`; directory `0700`, socket/token `0600`, 104-byte `sockaddr_un` guard.
- `src/daemon/launchd.rs` — macOS **socket-activated** LaunchAgent (`com.github.rust-works.omni-dev.daemon`): a `Sockets`→`Listener` dict (no `RunAtLoad`/`KeepAlive`) makes launchd own the control socket and demand-spawn the daemon on first connect; `daemon run` adopts the inherited fd via the `launch_activate_socket` FFI (`launchd_listener` — the crate's only `unsafe`, `#[allow(unsafe_code)]`-scoped), falling back to self-bind off the launchd path. `install_and_load` creates the `0700` runtime dir before bootstrap (launchd makes the socket before the process runs). Retires the #1078 `kickstart` workaround (#1081).
- `src/daemon/tray.rs` — the macOS menu bar, gated `#[cfg(all(target_os = "macos", feature = "menu-bar"))]` (off by default; pulls in `tray-icon`, `tao`, `arboard`). Compiled out elsewhere.
- `src/daemon/services/bridge.rs` — `BridgeService`, the first migrated service; wraps `BridgeServer`, persists the session token to the `0600` `bridge.token` file for thin-client discovery.
- `src/cli/daemon.rs` + `src/cli/daemon/{run,start,stop,restart,status}.rs` — the CLI surface. `run` becomes the daemon (foreground, what launchd execs); `start`/`stop`/`restart` are launchers/clients; `status` prints a per-service table (`--json` for machines).

Hosting the bridge in the daemon does **not** change its security model — keep [ADR-0036](docs/adrs/adr-0036.md), [ADR-0039](docs/adrs/adr-0039.md), and [docs/browser-bridge.md](docs/browser-bridge.md) in sync. Changes to the `daemon` CLI surface require the [`update-snapshots`](.claude/skills/update-snapshots/SKILL.md) skill (see Code Changes §5).

### Snowflake service
The daemon's **second** service (after the browser bridge) authenticates Snowflake sessions via external-browser SSO and reuses them for **concurrent arbitrary SQL** against **any** account — solving "SSO popup on every query" (the crate holds the session in memory with no serialization API, so a resident process is the only reuse path). It mirrors the bridge's engine/adapter split and is **account-agnostic** (no hardcoded accounts).

- `src/snowflake/` — the engine: `mod.rs` (`SnowflakeEngine`: lazy auth, per-query context as a `USE`-diff, base-context capture for resets, transparent token renew-and-retry, overall sign-in deadline; `SnowflakeEngineConfig::from_env_and_settings` over `SNOWFLAKE_*` env then `settings.json`), `session.rs` (the bounded session **pool** + `PoolRegistry`), `client/` (the clean-room v1 REST client that backs the engine — SSO login, query with **async-result polling** for long queries, **token renewal**, timeouts/retries, chunk download, row→JSON).
- **Concurrency model (the crux):** on v1, statement context (`warehouse`/`role`/`database`/`schema`) is session-global (`USE`, no statement-local form), so per-query context needs exclusive session access. To get concurrency *and* per-query context, each `(account, user)` keeps a **bounded `SessionPool`** (`SNOWFLAKE_POOL_SIZE`, default 4): a query checks out a session (LIFO reuse, else lazily authenticates one), applies only the `USE`s that differ from its current context, runs concurrently, and returns it. A `tokio::Semaphore(max)` caps concurrency *and* the live-session/auth count; sessions capture their base context (`SELECT CURRENT_*()`) so overrides can be reset on reuse. `menu()`/`status()` read pool bookkeeping behind a `std::Mutex` **never held across `.await`**; session creation (browser SSO) is serialized across pools by a shared `tokio` "auth gate" so only one browser opens at a time. (v2's per-request context would remove the per-session constraint — see issue #1003.)
- `src/daemon/services/snowflake.rs` — `SnowflakeService`, the thin `DaemonService` adapter: routes `query`/`sessions`/`disconnect`, renders a "Snowflake" tray submenu (one label per pool + `disconnect:<id>` / `disconnect-all` actions — **no clipboard actions, so `tray.rs` needs no change**), and reports `status()`.
- `src/cli/snowflake.rs` — `omni-dev snowflake {query,sessions,disconnect}`; `query` reads SQL from an arg or stdin, with per-query `--warehouse/--role/--database/--schema` and `--format json|yaml`.
- **Clean-room client (`src/snowflake/client/`)** is the **sole** Snowflake implementation — written from Snowflake's documented v1 REST protocol (no third-party connector), chosen precisely to gain **token renewal** (`session/token-request` via the master token), which `snowflake-connector-rs` can't do (it discards the master token). It also does async-result polling for long queries, per-request timeouts + transient retries, and external-chunk download. (`snowflake-connector-rs` was used briefly during bring-up but is gone — it pulled `rsa`/RUSTSEC-2023-0071 and an edition-2024 MSRV bump.) A background keep-alive heartbeat and threading a Chrome-profile browser command from settings are follow-ups; v2/PAT/key-pair is tracked in #1003.

**No new trust boundary:** requests ride the daemon's existing `0600` Unix socket; **no secret is persisted** (in-memory session only). Keep the operator guide + lumon contract [docs/snowflake-service.md](docs/snowflake-service.md) in sync, and run the [`update-snapshots`](.claude/skills/update-snapshots/SKILL.md) skill on any CLI-surface change (see Code Changes §5).

### Worktrees service
The daemon's **third** service tracks the repos/worktrees open across **every** VS Code window — something no extension can do alone, since the extension host is sandboxed per-window (each window sees only its own `workspace.workspaceFolders`). A single resident daemon is the rendezvous point that aggregates per-window registrations into one consistent view served back to the CLI, tray, and extension UI. Fed by a first-party companion VS Code extension (a separate, ~50-line deliverable); the registry is account-agnostic and **in-memory only**.

- `src/daemon/services/worktrees.rs` — `WorktreesService`, a thin `DaemonService` adapter (cheap `new()`, like Snowflake — no async setup, persists nothing). Holds a `HashMap` of open windows behind a `std::Mutex` **never held across `.await`**. Ops: `register` (idempotent upsert), `heartbeat` (returns `{known}` — `false` tells a window to re-register after a daemon restart, since state is in-memory), `unregister`, `list`. Keyed by a **companion-owned per-`activate()` UUID** (not `vscode.env.sessionId`, whose per-window uniqueness is unverified); `sessionId`/`pid` ride along as metadata.
- **Liveness (the crux):** entries carry `last_seen`; a 30s TTL (three missed ~10s heartbeats) ages out a window that crashed without `unregister`. Reaping is **inline on every read** (`list`/`status`/`menu`) — no background task, since the only consumers (tray ~1Hz poll, `worktrees list`, `daemon status`) trigger it naturally and staleness is only observable on read. This is the part a flat shared file could not do. The registry is also capped at 256 entries (#1140): a new-key `register` at the cap evicts the longest-silent entry rather than erroring (the evicted window re-registers off its next `known: false` heartbeat); upserts never evict.
- **Tray:** a "Worktrees" submenu lists open windows plus a best-effort `focus:<key>` action per window that shells out `code <folder>` (VS Code reuses the already-open window). The action routes through the daemon's **generic** `menu_action` dispatch (`tray.rs` needs no change). The launcher is resolved via `OMNI_DEV_VSCODE_BIN` → well-known absolute paths → bare `code` (handles launchd's minimal `PATH`); failure is logged, not fatal.
- `src/cli/worktrees.rs` — `omni-dev worktrees list [--json]`, a read-only client (`--socket` override). The register/heartbeat/unregister ops are **not** CLI subcommands — the companion speaks NDJSON to the socket directly.

**No new trust boundary:** requests ride the daemon's existing `0600` Unix socket; **no secret is persisted** (in-memory only); the companion is the daemon's first non-omni-dev client. Keep [ADR-0040](docs/adrs/adr-0040.md) and the operator guide + companion contract [docs/worktrees-service.md](docs/worktrees-service.md) in sync, and run the [`update-snapshots`](.claude/skills/update-snapshots/SKILL.md) skill on any CLI-surface change (see Code Changes §5).

### Request log
A local, append-only NDJSON log (`log.jsonl`) of every invocation **and** the HTTP requests it issues, plus an `omni-dev log` subcommand to search and pretty-print it. Best-effort and default-on; a write failure never changes the caller's exit code.

- `src/request_log.rs` — the schema (one forward-compatible `LogRecord` for both write and read, same `#[serde(default)]` + `skip_serializing_if` contract the daemon wire types use), the `record`/`record_http`/`record_invocation` writers, path resolution (`OMNI_DEV_LOG_FILE` → `state_dir`→`data_dir`/`omni-dev/log.jsonl`, dir `0700`/file `0600`), and the per-invocation `RequestLogContext` (process-global `OnceLock` + a `tokio` task-local override) that lets HTTP records inherit their parent `invocation_id`/`source`/`mcp_tool` without threading state through call sites.
- `src/cli/log.rs` + `src/cli/log/{format,query,stream}.rs` — the read-only `omni-dev log` command: the filter matrix, the `--query` mini-language (AND/OR/NOT, `field:value`, fuzzy tokens), `oneline`/`json`/`full` renderers (`json` is byte-identical to the on-disk lines), and the streaming reader (`--limit` ring buffer, `-f/--follow`).
- The invocation record is emitted by the [`main.rs`](src/main.rs) shell around `cli.execute()`; HTTP records are emitted from one hook per transport method (Atlassian, Datadog, Snowflake `send_once`, the Claude/AI backends + `claude-cli`, and the browser bridge `dispatch`/`start_stream`). The MCP server's hand-written `call_tool` scopes the task-local context to `source=mcp` per tool call.

**No secret is ever written:** auth headers are redacted centrally and request/response bodies are opt-in via `OMNI_DEV_LOG_BODIES` (headers via `OMNI_DEV_LOG_HEADERS`); `OMNI_DEV_LOG_DISABLE` is an absolute opt-out. **No new trust boundary** — the log is local-machine state with the same `0700`/`0600` posture as other runtime state. Keep the operator guide [docs/log.md](docs/log.md) in sync, and run the [`update-snapshots`](.claude/skills/update-snapshots/SKILL.md) skill on any CLI-surface change (see Code Changes §5).

### Skill Structure
Claude skills are organized in `.claude/skills/`, one subdirectory per skill with a `SKILL.md` file.

### Working with Git
Common git operations in this project:
- `git log --format=%H` - Get commit hashes
- `git show --stat <commit>` - Get diff summaries
- `git branch -r --contains <commit>` - Check remote branch containment
- `git status --porcelain` - Get working directory status

### Git Worktrees
New git worktrees should be created in the `.work/` directory of the current project (e.g., `git worktree add .work/<branch-name> <branch-name>`). The `.work/` directory is gitignored and keeps worktrees scoped to the project rather than scattered across sibling directories.

## Testing Approach

### Test Types
- **Unit Tests**: In `src/` files using `#[cfg(test)]`
- **Integration Tests**: In `tests/` directory
- **Golden Tests**: Using `insta` crate for snapshot testing

### Test Data
- Temporary git repositories for integration tests
- YAML fixtures for parsing tests
- Golden files for output validation

## Common Patterns

### Error Handling
```rust
use anyhow::{Context, Result};

fn operation() -> Result<()> {
    // Use .context() for error chain building
    some_operation()
        .context("Failed to perform operation")?;
    Ok(())
}
```

### YAML Serialization
```rust
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct Data {
    #[serde(skip_serializing_if = "Option::is_none")]
    optional_field: Option<String>,
}
```

### Git Operations
```rust
use git2::Repository;

let repo = Repository::open(".")?;
let head = repo.head()?;
let commit = head.peel_to_commit()?;
```

## Troubleshooting

### Common Issues
- **Clippy Warnings**: Use suggested fixes or add `#[allow(clippy::rule)]` with justification
- **Test Failures**: Check for timing issues with git operations
- **YAML Formatting**: Ensure proper serialization attributes

### Debug Commands
```bash
# Verbose test output
cargo test -- --nocapture

# Specific test
cargo test test_name

# Debug build
cargo build --verbose
```

## References

- [Rust Documentation](https://doc.rust-lang.org/)
- [git2 Crate Documentation](https://docs.rs/git2/)
- [Clap CLI Framework](https://docs.rs/clap/)
- [Serde Serialization](https://serde.rs/)
- [Release Process](docs/RELEASE.md) - Complete release workflow

## Best Practices

1. **Read the Full Context**: Understand the existing codebase before making changes
2. **Follow Rust Idioms**: Use idiomatic Rust patterns and conventions
3. **Maintain Safety**: Leverage Rust's safety features and error handling
4. **Document Changes**: Update documentation when adding features
5. **Test Thoroughly**: Ensure changes don't break existing functionality
6. **Follow Semver**: Use appropriate version bumps for changes