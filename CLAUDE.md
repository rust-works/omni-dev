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
`src/claude/backend.rs` is the single source of truth for backend, model, and beta-header resolution — `resolve_backend` / `resolve_model` / `resolve_beta_header` are shared by the client factory (`src/claude/client.rs::create_default_claude_client`) and preflight (`src/utils/preflight.rs`), so the two can no longer drift (#1118).

Backend selection: if `OMNI_DEV_AI_BACKEND` is set (directly or via the global `--ai-backend` flag, values `default|claude-cli|openai|ollama|bedrock`) it wins outright — `default` forces the direct API even when `USE_*` flags are set, and an unknown value is a hard error. When unset, legacy order: `USE_OLLAMA=true` → `USE_OPENAI=true` → `CLAUDE_CODE_USE_BEDROCK=true` → direct API. The resolved `AiBackend` enum dispatches to `ClaudeCliAiClient` (`src/claude/ai/claude_cli.rs`), `OpenAiAiClient::new_ollama`/`new_openai` (`src/claude/ai/openai.rs`), `BedrockAiClient` (`src/claude/ai/bedrock.rs`), or `ClaudeAiClient` (`src/claude/ai/claude.rs`).

Model selection stops at the first non-empty value: explicit param (MCP/`run_*` callers) → `OMNI_DEV_MODEL` (global `--model`) → per-family vars (Claude family: `CLAUDE_MODEL` → `CLAUDE_CODE_MODEL` → `ANTHROPIC_MODEL`; OpenAI: `OPENAI_MODEL`; Ollama: `OLLAMA_MODEL`) → registry default. `resolve_model` returns `String` (infallible); preflight validates the result via `preflight.rs::validate_model` → `ModelRegistry::is_known_model`, **gated to `Default | Bedrock`** — Ollama has no registry entries, OpenAI accepts unknown-but-well-shaped ids by design, and `claude-cli` resolves its own aliases, so widening that scope breaks them (#1333, pinned by tests in `preflight.rs`). `--model` and `--beta-header` are **global** flags (propagated as `OMNI_DEV_MODEL`/`OMNI_DEV_BETA_HEADER` in `Cli::propagate_global_flags`); the per-subcommand copies were removed in #1118. Adding a backend = new enum variant + resolver arms + one factory arm + one preflight arm.

Structured JSON-schema output (#1119): all four HTTP backends now advertise `supports_response_schema` and take the schema path. OpenAI/Ollama use `response_format: json_schema` (`openai.rs`); the direct Anthropic (`claude.rs`) and Bedrock (`bedrock.rs`) backends use the GA Messages-API `output_config.format` — but **model-gated** via the registry's `supports_structured_output` flag (`ModelRegistry::supports_structured_output`, populated from `src/templates/models.yaml`), since `output_config` `400`s on older models. Unflagged/unknown models fall back to YAML. `claude-cli` keeps its `--json-schema` path. The shared HTTP request timeout is 300s, overridable via `OMNI_DEV_AI_TIMEOUT_SECS` (parity with the subprocess backend's separate `OMNI_DEV_CLAUDE_CLI_TIMEOUT_SECS`).

User-facing details — required env vars, model selection, Claude CLI sandbox semantics, the `--claude-cli-allow-tools` / `--claude-cli-allow-mcp` escape hatches, the `--claude-cli-max-budget-usd` spending cap, and per-backend troubleshooting — live in [docs/ai-backends.md](docs/ai-backends.md). Keep it in sync when changing any of those surfaces.

Architectural rationale for the sandboxed `claude-cli` subprocess backend — threat model, sandbox flag choices, escape-hatch design, budget-cap enforcement — lives in [ADR-0028](docs/adrs/adr-0028.md).

Error classification (#1333): non-2xx HTTP responses from all four backends funnel through `ai.rs::check_error_response`, which preserves the status as `ClaudeError::ApiHttpError { status, body }` (`claude_cli.rs::map_api_error` maps its `api_error_status` onto the same variant). `ClaudeError::is_transient` / the `is_transient_ai_error(&anyhow::Error)` helper (both `src/claude/error.rs`) classify: a non-retryable 4xx is permanent, everything else — including anything not positively identified — is transient. Callers that degrade or retry (`create_pr.rs`'s template fallback, `client.rs`'s two retry loops) **must** gate on it, so a permanent failure fails loudly instead of being silently papered over. `ApiRequestFailed(String)` remains for the statusless cases only.

Dev-only notes:
- `ClaudeCliAiClient::run` is the warn site for both escape hatches, the INFO-level `total_cost_usd` log, and the post-response WARN when reported cost exceeds the configured cap. `ClaudeCliAiClient::max_budget_from_value` is the warn site for a set-but-invalid cap value (#1135).
- `--beta-header` is Anthropic-specific and only sent on the Anthropic backends (direct API + Bedrock, validated against the registry). The non-Anthropic backends warn-and-ignore it via `client.rs::warn_beta_header_ignored`: `claude-cli` (`claude`'s `--betas` flag has different semantics), plus `openai`/`ollama` (they never transmit it; #1119 stopped them validating a header they'd never send).

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
- `src/daemon/server.rs` + `single_instance.rs` + `lifecycle.rs` — the accept loop and dispatch; `acquire_listener` adopts the service-manager-activated fd when socket-activated (launchd on macOS, systemd on Linux; else the exclusive socket bind, which **is** the single-instance lock: `ping`-probe + stale-socket reclaim); on a socket-activated path shutdown leaves the socket inode for the manager (the unlink is gated to the self-bound path via the `socket_activated` flag); `SIGTERM`/`SIGINT`/`SIGHUP` cancel a shared `CancellationToken` for graceful shutdown.
- `src/daemon/paths.rs` — runtime paths via `dirs::data_dir()` (`<data>/omni-dev/`, **not** `~/.omni-dev`): `daemon.sock`, `bridge.token`, and (off-macOS) `daemon.log`; directory `0700`, socket/token/log `0600`, 104-byte `sockaddr_un` guard.
- `src/daemon/launchd.rs` — macOS **socket-activated** LaunchAgent (`com.github.rust-works.omni-dev.daemon`): a `Sockets`→`Listener` dict (no `RunAtLoad`/`KeepAlive`) makes launchd own the control socket and demand-spawn the daemon on first connect; `daemon run` adopts the inherited fd via the `launch_activate_socket` FFI (`launchd_listener` — scoped `#[allow(unsafe_code)]`, like the `setsid` `pre_exec` hook in `control.rs`), falling back to self-bind off the launchd path. `install_and_load` creates the `0700` runtime dir before bootstrap (launchd makes the socket before the process runs). Retires the #1078 `kickstart` workaround (#1081).
- `src/daemon/systemd.rs` — Linux **socket-activated** systemd **user** unit (`omni-dev-daemon.socket` + `.service` under `~/.config/systemd/user/`), gated `#[cfg(target_os = "linux")]`, mirroring `launchd.rs`: `enable --now` the socket (auto-start at login via `sockets.target`, `SocketMode=0600`); `daemon run` adopts the inherited fd via a hand-rolled `sd_listen_fds` (`systemd_listener`, `LISTEN_FDS`/`LISTEN_PID`, sets `FD_CLOEXEC`; no libsystemd dep). `unload` = `stop`+`disable` (the `bootout` analogue). `is_available()` gates on `/run/systemd/system` + the user-manager socket, with an `OMNI_DEV_DAEMON_DISABLE_SYSTEMD` escape hatch; falls back to the detached spawn when unavailable. `restart` skips the down-poll on Linux (an armed socket re-activates on any connect) (#1174).
- `src/daemon/tray.rs` — the macOS menu bar, gated `#[cfg(all(target_os = "macos", feature = "menu-bar"))]` (off by default; pulls in `tray-icon`, `tao`, `arboard`). Compiled out elsewhere.
- `src/daemon/services/bridge.rs` — `BridgeService`, the first migrated service; wraps `BridgeServer`, persists the session token to the `0600` `bridge.token` file for thin-client discovery.
- `src/cli/daemon.rs` + `src/cli/daemon/{run,start,stop,restart,status,logs,bridge,service}.rs` — the CLI surface. `run` becomes the daemon (foreground, what the service manager execs); `start`/`stop`/`restart` are launchers/clients (macOS installs a launchd LaunchAgent, Linux a systemd user unit for login auto-start; without a service manager `start` detaches the spawn: `setsid` + stdio to a `0600` `daemon.log`, no login auto-start — #1174); `status` prints a per-service table (`--json` for machines) and reports the resident daemon's version, warning on a CLI/daemon mismatch (#1113). Operability additions (#1113): `logs` reads/tails the daemon's own `daemon.log` (the `0600` file the launchd LaunchAgent / detached-spawn launcher sink the daemon's stdio to — #1316; systemd uses the journal instead); `bridge` is a typed client for the browser-bridge ops (`status`/`restart`/`disconnect-tab`/`snippet`/`token`/`request-command`) previously reachable only from the tray; `service <SVC> <OP> [--payload]` is a generic passthrough to any service op. Version rides the built-in `ping`/`status` payloads (additive; `DaemonClient::version()`). The shared thin-client `call_service`/`warn_version_mismatch` helpers live in `src/cli/daemon.rs`.

Hosting the bridge in the daemon does **not** change its security model — keep [ADR-0036](docs/adrs/adr-0036.md), [ADR-0039](docs/adrs/adr-0039.md), and [docs/browser-bridge.md](docs/browser-bridge.md) in sync. Changes to the `daemon` CLI surface require the [`update-snapshots`](.claude/skills/update-snapshots/SKILL.md) skill (see Code Changes §5).

### Snowflake service
The daemon's **second** service (after the browser bridge) authenticates Snowflake sessions via external-browser SSO and reuses them for **concurrent arbitrary SQL** against **any** account — solving "SSO popup on every query" (the crate holds the session in memory with no serialization API, so a resident process is the only reuse path). It mirrors the bridge's engine/adapter split and is **account-agnostic** (no hardcoded accounts).

- `src/snowflake/` — the engine: `snowflake.rs` (`SnowflakeEngine`: lazy auth, per-query context as a `USE`-diff, base-context capture for resets, transparent token renew-and-retry, overall sign-in deadline; `SnowflakeEngineConfig::from_env_and_settings` over `SNOWFLAKE_*` env then `settings.json`), `session.rs` (the bounded session **pool** + `PoolRegistry`), `client/` (the clean-room v1 REST client that backs the engine — SSO / PAT / key-pair-JWT login, query with **async-result polling** for long queries, **token renewal**, timeouts/retries, chunk download, row→JSON).
- **Concurrency model (the crux):** on v1, statement context (`warehouse`/`role`/`database`/`schema`) is session-global (`USE`, no statement-local form), so per-query context needs exclusive session access. To get concurrency *and* per-query context, each `(account, user)` keeps a **bounded `SessionPool`** (`SNOWFLAKE_POOL_SIZE`, default 4): a query checks out a session (LIFO reuse, else lazily authenticates one), applies only the `USE`s that differ from its current context, runs concurrently, and returns it. A `tokio::Semaphore(max)` caps concurrency *and* the live-session/auth count; sessions capture their base context (`SELECT CURRENT_*()`) so overrides can be reset on reuse. `menu()`/`status()` read pool bookkeeping behind a `std::Mutex` **never held across `.await`**; session creation (browser SSO) is serialized across pools by a shared `tokio` "auth gate" so only one browser opens at a time. (v2's per-request context would remove the per-session constraint — a v2 client was proposed in #1003 but closed as not planned.)
- `src/daemon/services/snowflake.rs` — `SnowflakeService`, the thin `DaemonService` adapter: routes `query`/`sessions`/`disconnect`, renders a "Snowflake" tray submenu (one label per pool + `disconnect:<id>` / `disconnect-all` actions — **no clipboard actions, so `tray.rs` needs no change**), and reports `status()`.
- `src/cli/snowflake.rs` — `omni-dev snowflake {query,sessions,disconnect}`; `query` reads SQL from an arg or stdin, with per-query `--warehouse/--role/--database/--schema` and `--format json|yaml`.
- **Clean-room client (`src/snowflake/client/`)** is the **sole** Snowflake implementation — written from Snowflake's documented v1 REST protocol (no third-party connector), chosen precisely to gain **token renewal** (`session/token-request` via the master token), which `snowflake-connector-rs` can't do (it discards the master token). It also does async-result polling for long queries, per-request timeouts + transient retries, and external-chunk download. (`snowflake-connector-rs` was used briefly during bring-up but is gone — it pulled `rsa`/RUSTSEC-2023-0071 and an edition-2024 MSRV bump.) A background keep-alive heartbeat (engine-owned task, `SNOWFLAKE_HEARTBEAT_INTERVAL`, default 900s) heartbeats idle pooled sessions so `CLIENT_SESSION_KEEP_ALIVE` actually extends the master token and an idle pool never re-prompts SSO (#1107). Threading a Chrome-profile browser command from settings is a follow-up. **Auth is method-pluggable** (`AuthMethod` in `client/config.rs`; `SNOWFLAKE_AUTHENTICATOR` selects it, resolved by `resolve_auth_method` in `snowflake.rs`): external-browser SSO (default), **PAT** (`SNOWFLAKE_TOKEN`), and **key-pair RS256 JWT** (`SNOWFLAKE_PRIVATE_KEY_PATH`/`SNOWFLAKE_PRIVATE_KEY`) — the two non-interactive methods added in #1108 let the daemon run headless. All three finish through the shared `auth::complete_login` and yield the same `LoginTokens`, so pooling/renewal/heartbeat are auth-agnostic. JWT signing lives in `client/jwt.rs` using **`aws-lc-rs`** (already in-tree via rustls; deliberately not the `rsa` crate — same RUSTSEC-2023-0071 reason `snowflake-connector-rs` was dropped); only **unencrypted PKCS#8** keys are supported (encrypted-key/passphrase support is a follow-up). The v2 client itself was declined in #1003.

**No new trust boundary:** requests ride the daemon's existing `0600` Unix socket; **no secret is persisted** (in-memory session only). Keep the operator guide + lumon contract [docs/snowflake-service.md](docs/snowflake-service.md) in sync, and run the [`update-snapshots`](.claude/skills/update-snapshots/SKILL.md) skill on any CLI-surface change (see Code Changes §5).

### Worktrees service
The daemon's **third** service tracks the repos/worktrees open across **every** VS Code window — something no extension can do alone, since the extension host is sandboxed per-window (each window sees only its own `workspace.workspaceFolders`). A single resident daemon is the rendezvous point that aggregates per-window registrations into one consistent view served back to the CLI, tray, and extension UI. Fed by a first-party companion VS Code extension shipped in `editors/vscode/`; the registry is account-agnostic and **in-memory only**.

- `src/worktrees.rs` — the engine: `WorktreesRegistry` mirrors the bridge/Snowflake engine split (a standalone `crate::worktrees` module, not fused into the adapter — #1154). Holds a `HashMap` of open windows (`WindowEntry`) behind a `std::Mutex` **never held across `.await`**, plus the `RegisterRequest` DTO. Ops: `register` (idempotent upsert), `heartbeat` (returns `known` — `false` tells a window to re-register after a daemon restart, since state is in-memory), `unregister`, `list`, and `first_folder` (for the tray focus action). It also owns the **close-pending directive** (`mark_close_pending`/`take_close_pending`, #1277): a per-key `HashSet` behind its **own** `Mutex` (never nested under the window map's), taken-and-cleared on the target's next `heartbeat` and cleared on `unregister`. Keyed by a **companion-owned per-`activate()` UUID** (not `vscode.env.sessionId`, whose per-window uniqueness is unverified); `sessionId`/`pid` ride along as metadata. Owns liveness/eviction (below).
- `src/daemon/services/worktrees.rs` — `WorktreesService`, the thin `DaemonService` adapter (cheap `new()`, like Snowflake — no async setup, persists nothing): routes `register`/`heartbeat`/`unregister`/`list`/`tree`/`ahead-behind`/`open`/`close` ops (plus the streaming `subscribe`) to the registry, validates the payload `key`, renders the menu/status, and drives the VS Code launcher. Per-worktree ahead/behind is the dominant per-worktree cost, so it is **lazy** (#1306): the streamed `tree`/`subscribe` snapshot uses the cheap `git_status_cheap` (branch + repo identity, **no** `graph_ahead_behind` walk) and divergence is fetched on demand via the `ahead-behind` op (batched by path) — the extension does it on repo-expand, `worktrees tree` folds it back in; `list`/`status` and the tray menu still enrich inline via the full `git_status`. The **`close`** op (#1277, [ADR-0049](docs/adrs/adr-0049.md)) is the one **destructive** op: a two-phase (side-effect-free safety check → confirmed execute) close of a worktree's window and, for a linked worktree, its `git2`-prune deletion (`valid`+`working_tree`, no shell-out, refuses a locked worktree). Deletability keys **solely on `is_main`** (structural, never the branch name — a linked worktree on the default branch is deletable and its branch survives); the daemon refuses to remove the main working tree defensively. A cross-window close signals the owning window via the additive `close` field on its `heartbeat` reply and waits (bounded ~20s) for it to `unregister`; a self-close removes-then-replies so the extension closes its own window. All git I/O is on a blocking thread, never under the registry lock.
- **Liveness (the crux):** entries carry `last_seen`; a 30s TTL (three missed ~10s heartbeats) ages out a window that crashed without `unregister`. Reaping is **inline on every read** (`list`/`status`/`menu`) — no background task, since the only consumers (tray ~1Hz poll, `worktrees list`, `daemon status`) trigger it naturally and staleness is only observable on read. This is the part a flat shared file could not do. The registry is also capped at 256 entries (#1140): a new-key `register` at the cap evicts the longest-silent entry rather than erroring (the evicted window re-registers off its next `known: false` heartbeat); upserts never evict.
- **Tray:** a "Worktrees" submenu lists open windows plus a best-effort `focus:<key>` action per window that shells out `code <folder>` (VS Code reuses the already-open window). The action routes through the daemon's **generic** `menu_action` dispatch (`tray.rs` needs no change). The launcher is resolved via `OMNI_DEV_VSCODE_BIN` → well-known absolute paths → bare `code` (handles launchd's minimal `PATH`); failure is logged, not fatal.
- `src/cli/worktrees.rs` — `omni-dev worktrees list [--json]` / `tree` read-only clients plus `focus <PATH>` (raises a worktree's VS Code window via the daemon's `open` op — the tray-only focus action made CLI-reachable, #1113); `--socket` override. The register/heartbeat/unregister ops are **not** CLI subcommands — the companion speaks NDJSON to the socket directly.
- `editors/vscode/` — the companion VS Code extension (TypeScript; the daemon's first non-omni-dev client, #1111): a thin per-window reporter plus the tree-view UI. `src/socket.ts` recomputes the socket path (`dirs::data_dir()`) and holds the NDJSON client + envelope builders (including `closeCheck`/`close`; no `vscode` import, so it is unit-tested with `node --test`); `src/extension.ts` is the activate→register / ~10s heartbeat (**close self on `close:true`**, else re-register on `known:false`) / deactivate→unregister lifecycle. The tree view adds two context-menu commands (#1277) routed by an enriched `contextValue` (`worktreeContextValue` encodes `is_main` as `.main`/`.linked`): **Close Worktree** (linked; phase-1 check → conditional modal confirm → phase-2 execute in `withProgress`) and **Close Window** (main tree; close-only, never a delete). Bundled with esbuild → `dist/extension.js`, packaged to `.vsix` by `.github/workflows/vscode-extension.yml` (path-filtered on `editors/vscode/**`). The Rust build never descends into `editors/` — it is single-crate and `editors/` is in `Cargo.toml`'s `exclude`, so `cargo publish` never ships it. Marketplace/Open VSX publishing is a deferred follow-up (needs a publisher account + CI secrets).

**One destructive op, no new trust boundary:** requests ride the daemon's existing `0600` Unix socket; **no secret is persisted** (in-memory only, the close directive included); the companion is the daemon's first non-omni-dev client. The `close` op deletes files and closes windows — a real threat-model escalation ([ADR-0049](docs/adrs/adr-0049.md)) but same-user-bounded and guarded in the daemon (`git2`-enforced real-linked-worktree-only, main-tree removal refused). Keep [ADR-0040](docs/adrs/adr-0040.md), [ADR-0048](docs/adrs/adr-0048.md), [ADR-0049](docs/adrs/adr-0049.md) and the operator guide + companion contract [docs/worktrees-service.md](docs/worktrees-service.md) in sync, and run the [`update-snapshots`](.claude/skills/update-snapshots/SKILL.md) skill on any CLI-surface change (see Code Changes §5).

### Sessions service
The daemon's **fourth** service tracks the Claude Code sessions running across **every** terminal and VS Code window for the logged-in user, with a coarse **inferred** live state (working/idle/waiting) — something no single vantage point sees (a hook knows only its own process; a VS Code window is sandboxed per host; the transcript files carry no live state alone). Mirrors the worktrees engine/adapter split and is **account-agnostic, in-memory only**. Fed by three independent, gracefully-degrading feeds. See [ADR-0052](docs/adrs/adr-0052.md).

- `src/sessions.rs` — the engine: `SessionsRegistry` (standalone `crate::sessions`, like `crate::worktrees`). Holds two `std::Mutex`-guarded maps **never held across `.await`** — live sessions keyed by Claude `session_id`, and companion window-embedding reports keyed by window key — plus the whole state machine ([`SessionState::for_event`]): `SessionStart→starting`; `UserPromptSubmit`/`PreToolUse`/`PostToolUse`/transcript-growth `→working`; `Stop→idle`; `Notification` classified `→waiting_for_permission`/`waiting_for_input`; `SessionEnd→ended`. `waiting_for_*` is reliable (direct `Notification`); `working`/`idle` is best-effort inference (Claude ships no state event — anthropics/claude-code#43058 *not planned*). Liveness = `last_seen`+TTL, **reap-on-read** (no background reaper), maps capped (512 sessions / 256 windows, evict-longest-silent). A session's `source` (`terminal`|`vscode{window_key}`) is resolved on read by a pure `cwd`-prefix join against the window reports. Idle sessions age out on the generous 5-min TTL (no liveness event) and re-appear on next activity — the accepted hook-based limitation.
- `src/sessions/watcher.rs` — Feed 2, the engine-owned transcript watcher (adapter starts it like `start_menu_refresh`): scans `~/.claude/projects/**/*.jsonl` every 5s for new/growing files to discover sessions predating the daemon and cover the hook-silent thinking window. Parses **only presence + size/mtime**, never the version-unstable line schema; only recently-touched files are surfaced (no flooding on first scan); it cannot decode `cwd` from the lossy encoded dir name, so watcher-only sessions have no `cwd`/`repo` until a hook enriches them (`observe` never clobbers known data with `None`).
- `src/daemon/services/sessions.rs` — `SessionsService`, the thin adapter (cheap `new()`, persists nothing): routes `observe`/`end`/`window`/`window-unregister`/`list`, enriches `repo` from `cwd` via `git2` on a blocking thread (the worktrees `git_status` precedent — engine does no disk I/O), renders the "Claude Sessions" tray submenu (a `focus:<session_id>` action for `vscode` sessions, reusing worktrees' `pub(crate) focus_window`) + `status()`. **No subscribe/menu-cache** (unlike worktrees): `repo` is enriched at observe time, so `menu`/`list` are pure formatting.
- `src/cli/sessions.rs` — `omni-dev sessions {list,hook,install-hooks,uninstall-hooks}` (`#[cfg(unix)]`). `list` is a read-only socket client; `hook` is the **feed sink** Claude Code runs (reads hook JSON on stdin → `observe`/`end` op → fire-and-forget POST; **infallible by design** — swallows every error, always exits 0, never blocks a turn); `install-hooks`/`uninstall-hooks` idempotently merge/remove the 7-event hook block in `~/.claude/settings.json` (absolute-current-exe command; preserves other hooks; prunes emptied scaffolding). Hook-JSON→op mapping and settings merge/remove are pure, unit-tested functions.
- `editors/vscode/` — Feed 3 extends the **existing** worktrees companion (not a new extension): `src/claudeEmbeddings.ts` (pure, no `vscode` import, `node --test`-tested) counts Claude editor tabs (`viewType.includes("claudeVSCodePanel")`) and terminals (name match, honoring `$CLAUDE_CODE_TERMINAL_TITLE`); `src/socket.ts` adds `sessionWindow`/`sessionWindowUnregister` envelopes; `src/extension.ts` reports its window's embedding **counts** (never a tab's `session_id` — the Claude extension exposes no API, ADR-0052) to the sessions service on activate / the same ~10s heartbeat / tab-&-terminal-change events, and unregisters on deactivate. The daemon joins sessions→window by `cwd`, tagging `source`.

**No new trust boundary:** ops ride the daemon's existing `0600` Unix socket; **no secret is persisted** (in-memory only). Residual exposure: a socket reader can enumerate session cwds/repos + coarse state, a writer can inject fake sessions — both already require the owning local user. Hooks are opt-in user config; the sink writes nothing but the socket POST. Distinct from #876 (history-search MCP; shares the transcript dir). Keep [ADR-0052](docs/adrs/adr-0052.md) and the operator guide + hook/companion contract [docs/sessions-service.md](docs/sessions-service.md) in sync, and run the [`update-snapshots`](.claude/skills/update-snapshots/SKILL.md) skill on any CLI-surface change (see Code Changes §5).

### Request log
A local, append-only NDJSON log (`log.jsonl`) of every invocation **and** the HTTP requests it issues, plus an `omni-dev log` subcommand to search and pretty-print it. Best-effort and default-on; a write failure never changes the caller's exit code.

- `src/request_log.rs` — the schema (one forward-compatible `LogRecord` for both write and read, same `#[serde(default)]` + `skip_serializing_if` contract the daemon wire types use), the `record`/`record_http`/`record_invocation` writers, path resolution (`OMNI_DEV_LOG_FILE` → `state_dir`→`data_dir`/`omni-dev/log.jsonl`, dir `0700`/file `0600`), the per-invocation `RequestLogContext` (process-global `OnceLock` + a `tokio` task-local override) that lets HTTP records inherit their parent `invocation_id`/`source`/`mcp_tool` without threading state through call sites, and the two opt-in growth bounds (#1121): env-gated unix-only rotation-on-write (`OMNI_DEV_LOG_MAX_SIZE`/`OMNI_DEV_LOG_KEEP_FILES`, serialized on a stable `log.jsonl.lock`) and the `prune` engine (age/size filter → atomic same-dir temp-file rewrite).
- `src/cli/log.rs` + `src/cli/log/{format,query,stream,prune}.rs` — the `omni-dev log` command: with no subcommand, the read-only search (the filter matrix, the `--query` mini-language — AND/OR/NOT, `field:value`, fuzzy tokens; `oneline`/`json`/`full` renderers where `json` is byte-identical to the on-disk lines; the streaming reader with a `--limit` ring buffer and `-f/--follow`); the `prune` subcommand (`--older-than`/`--max-size`/`--dry-run`) bounds on-disk growth via `request_log::prune`.
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