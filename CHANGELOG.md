# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Snowflake query service on the daemon** ([#999](https://github.com/rust-works/omni-dev/issues/999)): A second daemon-hosted service (after the browser bridge) that authenticates Snowflake sessions via external-browser SSO and reuses them for **concurrent arbitrary SQL** across **any** account — solving the "SSO popup on every query" problem (a live Snowflake session has no serialization API, so a resident process is the only way to reuse one auth across invocations). New engine `src/snowflake/`: each `(account, user)` keeps a **bounded session pool** (`SNOWFLAKE_POOL_SIZE`, default 4) so queries run **concurrently** on one authentication identity while still honoring per-query `warehouse`/`role`/`database`/`schema` — applied as a `USE`-diff on checkout against each session's captured base context, with a `tokio::Semaphore` capping concurrency *and* the live-session/auth count, a shared auth gate serializing browser popups, lazy re-auth on session expiry, and arbitrary-schema → self-describing `{ columns, rows }` JSON conversion. Plus a thin `DaemonService` adapter `src/daemon/services/snowflake.rs` (routes `query`/`sessions`/`disconnect`, contributes a "Snowflake" tray submenu, surfaces in `daemon status`) and a thin client `omni-dev snowflake {query,sessions,disconnect}` — `query` takes SQL from an argument or stdin and supports per-query `--warehouse/--role/--database/--schema` plus `--format json|yaml`. Account/user/context default from `SNOWFLAKE_*` env vars then `~/.omni-dev/settings.json` (the Atlassian pattern); **no hardcoded accounts**. No new socket or persisted secret — it rides the daemon's existing `0600` Unix control socket and keeps sessions in memory only. The engine runs on an in-tree **clean-room Snowflake v1 REST client** (`src/snowflake/client/`, no third-party connector) that adds **session-token renewal** (so a session survives the ~1h token lifetime instead of re-authenticating), transparent renew-and-retry on expiry, **async-result polling** so heavy long-running queries complete (bounded by `SNOWFLAKE_QUERY_TIMEOUT`) instead of being killed by a per-request timeout, per-request HTTP timeouts (`SNOWFLAKE_HTTP_TIMEOUT`) and transient-error retries, an overall sign-in deadline (`SNOWFLAKE_AUTH_TIMEOUT`) so a hung auth can't block others, and external-chunk download for large results. No third-party Snowflake connector is used (an early `snowflake-connector-rs` dependency, which pulled `rsa`/RUSTSEC-2023-0071, was removed). See [docs/snowflake-service.md](docs/snowflake-service.md).

### Changed
- **`jira dev --summary` provider output** ([#924](https://github.com/rust-works/omni-dev/issues/924)): each entry in a category's `providers` list now carries both the `instance_type` identifier (the `applicationType` round-trip key, e.g. `stash`) and the display `name` (e.g. `Bitbucket Server`). The human table shows `Bitbucket Server (stash)` when the two differ and a plain `GitHub` when they match. **Output shape change**: under `--output json`/`yaml`, `providers` is now a list of `{instance_type, name}` objects instead of a list of strings.

### Removed
- **BREAKING — `omni-dev voice …` command tree removed** ([#980](https://github.com/rust-works/omni-dev/issues/980), [#977](https://github.com/rust-works/omni-dev/issues/977)): All voice / audio / speech-to-text functionality has been extracted to the dedicated [rust-works/omni-voice](https://github.com/rust-works/omni-voice) repository and removed from omni-dev. The entire `omni-dev voice …` command tree (`capture`, `transcribe`, `reflect`, `review`, `enroll`, `install-model`) no longer exists — users who need it should use omni-voice. This drops the `src/voice/` and `src/cli/voice/` modules, their tests, snapshots, and ~10 MB of audio fixtures, plus the voice-only dependency stack (`cpal`, `rubato`, `hound`, `signal-hook`, `ringbuf`, `ulid`, `candle-core`, `candle-nn`, `candle-transformers`, `hf-hub`, `tokenizers`, `byteorder`, `tract-onnx`, `rustfft`) and the `libasound2-dev`/ALSA CI steps that existed solely for `cpal`. The result is a measurably smaller dependency tree and release binary. The voice ADRs (0031–0035, 0037) are marked superseded by the new [ADR-0038](docs/adrs/adr-0038.md), which records the extraction; they remain as historical context.

### Fixed
- **Browser-bridge restart reliably rebinds its fixed ports** ([#990](https://github.com/rust-works/omni-dev/issues/990)): a daemon- or tray-initiated `BridgeService::restart` tears the old planes down and rebinds the *same* fixed ports (default `9998`/`9999`), but a just-closed connection could leave a port in `TIME_WAIT`, so `BridgeServer::start` failed with `EADDRINUSE` — leaving the bridge wedged not-running with no automatic recovery (`status`/`menu` reported not-running until another restart). The bridge now binds its loopback control and WebSocket listeners with `SO_REUSEADDR`, so the rebind succeeds immediately despite a lingering `TIME_WAIT`; standalone `omni-dev browser bridge serve` restarts benefit too. Fail-closed against a *live* listener on the port is unchanged (`SO_REUSEADDR` ≠ `SO_REUSEPORT`), and the loopback-only, token-authenticated trust boundary (ADR-0036) is untouched.
- **`daemon restart` no longer risks leaving the daemon stopped *and* unregistered from launchd** ([#994](https://github.com/rust-works/omni-dev/issues/994)): on macOS, `restart` booted the launchd agent out (`launchd::unload`) *before* re-bootstrapping it via `install_and_load` — but `install_and_load` already boots out any prior agent first, so the explicit pre-bootout was redundant and merely widened a failure window: if the process died or `launchctl bootstrap` failed between that bootout and a successful re-bootstrap, the daemon was left both stopped and unregistered from auto-start (worse than before `restart` ran). The redundant pre-bootout is removed, so the unavoidable bootout→bootstrap gap is now the tight, retrying sequence inside `install_and_load`. This also fixes a second instance of the same bug, where a slow shutdown could make `wait_until_down`'s timeout bail *after* the agent had already been booted out. `stop` still boots out via `unload` to disable auto-start.
- **Confluence attachment upload returned HTTP 405** ([#1005](https://github.com/rust-works/omni-dev/issues/1005)): `confluence attachment upload` (and the `confluence_attachment_upload` MCP tool) POSTed to the Confluence Cloud v2 path `/wiki/api/v2/pages/{id}/attachments`, which has no attachment-creation handler and returns `405 METHOD_NOT_ALLOWED`. Attachment creation is v1-only; uploads now use `POST /wiki/rest/api/content/{id}/child/attachment` and parse the v1 response shape, so the returned attachment metadata (media type, file size, download URL, page id, file id, version) is populated. Listing and deletion (already on supported v2 endpoints) are unchanged.
- **Large (multi-chunk) Snowflake results decode correctly** ([#999](https://github.com/rust-works/omni-dev/issues/999)): the clean-room client parsed each downloaded result chunk as a self-contained JSON array, but Snowflake serves chunks as **bare comma-separated row arrays** (`[r1],[r2],…`) meant to be concatenated — so any query whose result spanned a multi-row chunk failed with `invalid result chunk JSON: trailing characters`. Chunk bytes are now framed with `[` … `]` before parsing (matching the reference connector), so heavy queries returning large result sets decode correctly. (Found via the #999 field test once the inner error was surfaced.)
- **Daemon errors surface the underlying cause** ([#999](https://github.com/rust-works/omni-dev/issues/999)): a control-socket reply collapsed any handler error to its top-level wrapper (e.g. just `Snowflake query failed`), hiding the real reason. Replies now carry the full error chain (`{:#}`), so the actual Snowflake server error code/message — or a client-side polling-deadline timeout (`query did not finish within …`) — reaches the caller, and the engine also logs the inner cause server-side. (`SNOWFLAKE_QUERY_TIMEOUT`, default 3600s, bounds the async-result polling for long-running queries.)
- **Menu-bar tray no longer closes on refresh** ([#999](https://github.com/rust-works/omni-dev/issues/999)): the daemon tray rebuilt its whole menu (`set_menu`) whenever any item's text changed — and on macOS that closes an open menu, so a Snowflake submenu showing live per-session activity would snap shut roughly once a second. The tray now updates item text/enabled **in place** and only rebuilds when the menu *structure* changes (a session added or removed), so an open menu stays open while its live state refreshes. Each authenticated session now also shows **what it's doing** — the running query (a truncated preview) and how long it's been running, or its idle time and query count — across the tray submenu, `daemon status`, and `omni-dev snowflake sessions`.
- **`jira dev` auto-discovery for Bitbucket Server** ([#924](https://github.com/rust-works/omni-dev/issues/924)): provider auto-discovery now queries the DevStatus detail endpoint with the instance-type identifier (e.g. `applicationType=stash`) reported by the summary's `byInstanceType` map, rather than the human-readable display name (`Bitbucket Server`). Previously, any provider whose display name differed from its identifier — Bitbucket Server being the canonical case — was missed, returning empty results.
- **Inline `code` in JFM headings no longer fails the ADF mark validator** ([#1005](https://github.com/rust-works/omni-dev/issues/1005)): A heading authored with backticks — e.g. `` ### `GET /api/services/example` `` — previously produced a `heading` node carrying a `code` mark, which ADF's heading content model forbids, so the document was rejected at write time with an opaque JSONPath-only error. The JFM→ADF converter now strips the `code` mark from heading content (keeping the text as plain — the safe lossy direction, since Atlassian renders no inline-code styling on headings) and emits a warning naming the affected heading. The validator remains the safety net for any other path that produces a code-marked heading.

## [0.29.0] - 2026-06-12

### Added
- **`transcript youtube sync`** ([#931](https://github.com/rust-works/omni-dev/pull/931), [#928](https://github.com/rust-works/omni-dev/issues/928)): A new `omni-dev transcript youtube sync` subcommand for incrementally syncing transcripts from one or more YouTube channels to the filesystem. Builds on the existing per-video fetcher with channel enumeration, filesystem-as-state deduplication, and concurrent fetching (`futures::stream::buffer_unordered`). Flags: `--out`, `--lang`, `--format`, `--auto`, `--full`, `--since` (accepts `YYYY-MM-DD` midnight-UTC or RFC 3339), `--concurrency`, and `--dry-run`. Incremental mode stops at the first already-present file (newest-first ordering); `--full` examines every ID to fill gaps. Each transcript is written atomically via a sibling `.tmp` file + rename, and a `SyncReport` (synced / already present / no transcript / failed / would fetch / channel errors) is recorded and printed. Videos without a usable transcript (age-gated, region-locked, no captions) are skipped rather than aborting the run. New module `src/cli/transcript/youtube/sync.rs`.
- **`git branch create pr --from-commits`** ([#926](https://github.com/rust-works/omni-dev/pull/926), [#899](https://github.com/rust-works/omni-dev/issues/899)): A commit-message-driven PR generation path that sends only commit metadata to the AI instead of full diff content — faster, and it avoids large diff payloads when the branch's commits already convey intent. The new `--from-commits` flag on `CreatePrCommand` selects the path at runtime; `ClaudeClient::generate_pr_content_with_context_from_commits` builds a `RepositoryViewForAiFromCommits` payload (commit messages + detected type/scope only, no diff files read from disk), falls back to per-commit split dispatch when the combined payload exceeds the token budget, and bails with a clear error if even the single-commit slim view overflows. `try_full_diff_budget` is made generic over the view type so the diff-driven and commit-driven paths share one budget check.
- **`coverage diff` — diff/patch coverage analysis** ([#952](https://github.com/rust-works/omni-dev/pull/952), [#964](https://github.com/rust-works/omni-dev/pull/964), [#949](https://github.com/rust-works/omni-dev/issues/949)): A complete coverage-attribution pipeline that ingests per-line coverage reports (lcov, llvm-cov JSON, Cobertura XML — line coverage only, branch data ignored) plus a git diff and produces PR-attributable metrics: patch coverage (fraction of added lines covered), uncovered new lines (actionable `file:line` list), per-file project deltas, and indirect coverage flips on unchanged code. The `omni-dev coverage diff` command supports report-format auto-detection, base/head refs, baseline reports, output formats (markdown/YAML/JSON), coverage gates, and rendering options; the markdown renderer is a drop-in replacement for the legacy shell renderer used in PR comments. New CLI surface plus core analysis in `src/lib`.

- **`browser bridge harvest facebook posts`** ([#923](https://github.com/rust-works/omni-dev/issues/923)): A new `omni-dev browser bridge harvest <platform> <object>` command tree, with `harvest facebook posts` as the first target: it downloads the signed-in user's **own** Facebook timeline through the bridge, encapsulating the previously manual three-step recipe (issue [#922](https://github.com/rust-works/omni-dev/issues/922)) — harvest session tokens (`fb_dtsg`/`lsd`/`USER_ID` + the initial query's variables) from the `/me` shell, discover the pagination persisted-query `doc_id` from a cross-origin `static.xx.fbcdn.net` script bundle, then replay the refetch GraphQL query feeding `end_cursor` back until the timeline is exhausted. It reuses the existing `bridge request` dispatch/auth/cross-origin path via a shared `BridgeClient` (`src/browser/client.rs`) rather than opening its own socket — the cross-origin discovery step relies on `--allow-origin` ([#918](https://github.com/rust-works/omni-dev/issues/918)) and `--credentials omit` ([#920](https://github.com/rust-works/omni-dev/issues/920)). Each post is `{ id, creation_time, text, url, shared_link }`. Options: `--output` (file or stdout), `--format <jsonl|json>` (default `jsonl`; streamed and append-friendly), `--since <unix-ts|ISO8601>` (incremental archive), `--limit <N>` (sampling), `--resume <PATH>` (continue a run interrupted by a 504 / token rotation from its last saved cursor), plus the `--target` / `--token-file` / `--control-port` auth surface shared with `bridge request`. **Best-effort contract**: it drives reverse-engineered, undocumented Facebook internals, re-harvests every volatile `doc_id`/token/provider flag on each run (nothing hardcoded), fails with staged actionable errors naming the step that drifted (not panics), and only ever uses the connected tab's own session (your own account only). The stable alternative — Facebook's official "Download Your Information" export — is documented alongside. New modules `src/browser/harvest/` and `src/cli/browser/harvest.rs`; see [docs/browser-bridge.md](docs/browser-bridge.md).

### Changed
- **`coverage diff` scopes deltas to changed files by default** ([#973](https://github.com/rust-works/omni-dev/pull/973)): Coverage is measured by two independent instrumented runs (baseline vs head), so lines in files the PR never touched can flip covered↔uncovered purely from run-to-run variance, surfacing as phantom per-file deltas and "indirect coverage changes" for unrelated files. The project-delta and indirect-change sections now default to `DiffScope::DiffOnly` (report only files the diff touches). Real cross-file effects (e.g. a PR that removes a test, dropping a whole module's coverage) still surface via a magnitude-gated "notable unchanged" note (≥ 10 net covered lines), so only the small measurement-noise flips are hidden. Patch coverage and the total are unchanged. The new `--all-files` flag restores the unscoped (noisier) report.

### Removed
- **`git://repo/commits/{range}` MCP resource** ([#972](https://github.com/rust-works/omni-dev/pull/972), [#971](https://github.com/rust-works/omni-dev/issues/971)): Removes the `git://repo/commits/{range}` resource URI and all associated infrastructure from the MCP server and CLI, consolidating the resource layer on Atlassian and omni-dev resource types. The `ResourceUri::GitCommits` variant and its parsing/matching logic are gone, `read_resource()` loses its now-unused `repo_root` parameter, git-specific error messaging is dropped from `UriParseError::UnknownScheme`, and the server boundary in `src/mcp/server.rs` no longer resolves the CWD for git. The advertised resource count drops from 6 to 5.

### Fixed
- **Repo-relative operations anchored to the injected repo path** ([#970](https://github.com/rust-works/omni-dev/pull/970), [#967](https://github.com/rust-works/omni-dev/issues/967)): Resolves a category of bugs where commands resolved configuration, scratch directories, and git operations relative to the process CWD instead of the user-provided (or inferred) repository root — fixing inconsistencies when the MCP server or CLI injects an alternate repo path via `--repo`. Relative `.` defaults are converted to absolute paths via `current_dir()` in the twiddle and staged entry points; AI scratch-directory resolution is anchored to the opened repository's workdir in `CommitAnalysis`; new `_at` variants of the config-dir resolution functions anchor to an explicit repo path; and the preflight/remote checks (`check_ai_command_prerequisites`, `check_pr_command_prerequisites`, `check_github_cli`, `remote.rs::get_github_default_branch`) verify the injected repo via `check_git_repository_at` and `.current_dir(repo_root)` rather than the ambient CWD. The obsolete `CwdGuard` and no-arg repo opens are retired.
- **Voice model-installation test HOME race** ([#979](https://github.com/rust-works/omni-dev/pull/979), [#978](https://github.com/rust-works/omni-dev/issues/978)): Eliminates a `HOME` environment-variable mutation race that could make model-installation tests flaky when run in parallel.
- **Drift-report connection-test port race** ([#943](https://github.com/rust-works/omni-dev/pull/943), [#861](https://github.com/rust-works/omni-dev/issues/861)): Resolves a port race condition in the Atlassian drift-report connection test, and unifies the test environment mutex ([#953](https://github.com/rust-works/omni-dev/pull/953), [#950](https://github.com/rust-works/omni-dev/issues/950)) to fix flaky credential races.
- **Cobertura XML end-tag pattern for clippy 1.96**: Resolves a Cobertura XML end-tag match pattern flagged by clippy 1.96.
- **Exclude large assets from published crate**: The published crate no longer bundles large assets, keeping the crates.io package within size limits.

### Documentation
- **ADR-0037 — pure-C native ASR backends via Rust FFI** ([#938](https://github.com/rust-works/omni-dev/pull/938), [#933](https://github.com/rust-works/omni-dev/issues/933)): Records a coupled four-part decision enabling the native Voxtral backend without reopening the C++/cmake door ADR-0033 closed: a scoped relaxation permitting pure-C deps compiled with `cc`/`make` behind a Rust FFI boundary on `cfg(not(target_os = "windows"))` (C++ and cmake remain forbidden on all platforms), a third ADR-0035 integration shape (in-process native-C FFI behind a target-cfg Cargo dependency), and Windows graceful fallback (the Voxtral backend is cfg-excluded on Windows; the auto-upgrade hierarchy falls through to candle).
- **README leads with the demo** ([#945](https://github.com/rust-works/omni-dev/pull/945), [#833](https://github.com/rust-works/omni-dev/issues/833)): Moves the "See It In Action" demo and the 30-second command examples to the top of the README, immediately after the introduction, so new users see concrete value before the feature lists and installation instructions.
- **Browser bridge surfaced across docs and website** ([#925](https://github.com/rust-works/omni-dev/pull/925), [#927](https://github.com/rust-works/omni-dev/pull/927), [#929](https://github.com/rust-works/omni-dev/pull/929)): Surfaces the browser bridge in the README, `CLAUDE.md`, and ADR-0036, and adds browser-bridge, YouTube-transcript, and Claude-history demos plus a tutorial to the project website.

### CI/CD
- **PR coverage reporting pipeline** ([#948](https://github.com/rust-works/omni-dev/pull/948), [#946](https://github.com/rust-works/omni-dev/pull/946), [#944](https://github.com/rust-works/omni-dev/pull/944)): Renders the PR coverage comment via `omni-dev coverage diff` with a merge-base baseline strategy, per-file tracking, commit references, and artifact links; rebuilds the lcov baseline when the merge-base lacks a coverage file, and shares/reuses the coverage build env so the tool build is cached. Coverage tests were made deterministic ([#966](https://github.com/rust-works/omni-dev/pull/966)).
- **Haiku model added to commit-check workflow** ([#940](https://github.com/rust-works/omni-dev/pull/940), [#939](https://github.com/rust-works/omni-dev/issues/939)): Adds `claude-haiku-4-5` to the commit-check workflow.
- **Dependency bumps**: `tokio-tungstenite` 0.24→0.29 ([#959](https://github.com/rust-works/omni-dev/pull/959)), `cpal` 0.17→0.18 ([#960](https://github.com/rust-works/omni-dev/pull/960)), `rand` 0.8→0.9 ([#961](https://github.com/rust-works/omni-dev/pull/961)), `tract-onnx` 0.22.1→0.23.0 ([#962](https://github.com/rust-works/omni-dev/pull/962)); GitHub Actions `actions/upload-artifact` 4→7 ([#955](https://github.com/rust-works/omni-dev/pull/955)), `codecov/codecov-action` 6→7 ([#956](https://github.com/rust-works/omni-dev/pull/956)), `dawidd6/action-download-artifact` v6→v21 ([#954](https://github.com/rust-works/omni-dev/pull/954)), `marocchino/sticky-pull-request-comment` v2→v3 ([#957](https://github.com/rust-works/omni-dev/pull/957)).

## [0.28.0] - 2026-06-02

### Added
- **Browser WebSocket bridge** ([#902](https://github.com/rust-works/omni-dev/issues/902)): A new `omni-dev browser` command tree drives HTTP requests **through an authenticated browser tab**, borrowing the browser's SSO/OAuth/cookie session without exfiltrating credentials (a confused-deputy by design). `omni-dev browser bridge serve` runs a long-lived local process exposing two planes — a WebSocket plane the browser connects to via a pasted DevTools snippet (default `127.0.0.1:9999`), and an HTTP control plane the operator drives (default `127.0.0.1:9998`) — joined by an `id`-keyed correlator. Requests reach the page origin via a transparent proxy (any non-`/__bridge/` path), `GET /__bridge/status`, or `POST /__bridge/request`; `omni-dev browser bridge request --url … --method …` is a thin client that injects auth headers itself. The security model is core (see [ADR-0036](docs/adrs/adr-0036.md)): both planes are authenticated and default-closed, with a startup-generated session token never read from argv, control-plane `Host` allowlist + `Origin`/`Sec-Fetch-Site` rejection + mandatory `X-Omni-Bridge: 1` header + no CORS (anti-CSRF / anti-DNS-rebinding), WS token subprotocol, server-side relative-URL-only outbound scope (`--allow-origin` to widen), per-request `504` timeout, and bounded body size/concurrency. Both ports accept `0` to bind an OS-assigned random free port. New modules under `src/browser/` and `src/cli/browser/`; new dependencies `axum`, `tokio-tungstenite`, `rand`. Documented in [docs/browser-bridge.md](docs/browser-bridge.md).

- **Binary Response Bodies in the Browser Bridge** ([#906](https://github.com/rust-works/omni-dev/issues/906)): The browser bridge now round-trips **non-text** response bodies (images, gzipped/encoded blobs, file downloads) intact, closing the v1 gap where the snippet read every response via `resp.text()` and corrupted anything that wasn't UTF-8 text. The pasted snippet now inspects the response `Content-Type`: bodies that aren't `text/*`, JSON, XML, or JavaScript (and bodies with no type) are read via `arrayBuffer()`, base64-encoded, and tagged with `"encoding": "base64"` on the WebSocket frame; text/JSON bodies are unchanged and omit `encoding` (back-compat). The wire protocol gains an optional `encoding` field on `BrowserReply` / `ResponseEnvelope` (`src/browser/protocol.rs`) defaulting to plain text. The transparent proxy (`src/browser/bridge.rs`) base64-decodes before writing the HTTP response body so `curl` receives the raw bytes; `POST /__bridge/request` returns the envelope as-is for the caller to decode. `--max-body-bytes` is now accounted against the **decoded** size, and a malformed or unsupported encoding fails closed with `502`. Covered by new protocol unit tests, a snippet-content assertion, and three integration tests (proxy decodes to raw bytes, request endpoint returns the base64 envelope, oversized decoded body rejected). See [docs/browser-bridge.md](docs/browser-bridge.md) and [ADR-0036](docs/adrs/adr-0036.md).

- **Streaming / Chunked Response Bodies in the Browser Bridge** ([#907](https://github.com/rust-works/omni-dev/issues/907)): The browser bridge can now consume **streaming / long-lived** endpoints (Grafana Live, Server-Sent Events, chunked APIs) that never deliver a final buffered body, closing the v1 gap where every response was buffered under `--request-timeout` (so streaming endpoints timed out with `504` or returned nothing). Streaming is **caller opt-in** — `--stream` on `omni-dev browser bridge request`, `"stream": true` on `POST /__bridge/request`, or `?__stream=1` on the transparent proxy — and buffered (text + binary) behaviour is the unchanged default. The wire protocol (`src/browser/protocol.rs`) gains a `stream` flag on the `Command`, a server→browser `CancelCommand`, a `BrowserFrame` superset that classifies into head / `{seq,chunk}` / `done` / error `StreamItem`s, and an untagged `StreamLine` for NDJSON output. The pasted snippet reads `response.body.getReader()`, emits a head frame then one base64 chunk frame per read then a `done` frame, and cancels its reader on a `cancel` frame. The correlator (`src/browser/bridge.rs`) replaces the single `oneshot` per id with a per-id channel; the transparent proxy streams the decoded bytes as a native chunked HTTP body (curl-friendly) while `POST /__bridge/request` streams an `application/x-ndjson` body (head line, `{seq,chunk}` lines, `{done}` line). For streams, `--request-timeout` is reinterpreted as an **inter-chunk idle timeout** and `--max-body-bytes` as a **cumulative** ceiling; exceeding either limit — or the control-plane consumer disconnecting mid-stream — sends the browser a `cancel` frame so it stops its in-page reader. There is no socket-level backpressure to the browser's reader (memory is bounded by the cumulative cap, an accepted limitation). Covered by new protocol/correlator unit tests, a snippet-content assertion, and integration tests spanning the happy paths (proxy reassembles chunks to a raw body, request endpoint returns ordered NDJSON, `--stream` thin-client round-trip), the limit paths (cumulative cap aborts and cancels, idle timeout ends the stream), and the error/edge paths (no browser → 503, cross-origin → 403, invalid header → 400, error head / chunk-before-head → 502, start idle timeout → 504, undecodable chunk truncates, stray head ignored, unparseable frame survived). See [docs/browser-bridge.md](docs/browser-bridge.md) and [ADR-0036](docs/adrs/adr-0036.md).
- **Per-request `--allow-origin` on the browser bridge `request`** ([#918](https://github.com/rust-works/omni-dev/issues/918)): `omni-dev browser bridge request` gains a `--allow-origin <URL>` flag carrying a **request-scoped** outbound-origin override, so a single request can target a cross-origin URL **without** restarting `serve` and **without** breaking the extension's WebSocket connection. Previously cross-origin was gated only by the `serve --allow-origin` global, which feeds two checks at different times — `ws_origin_allowed` at WebSocket **upgrade** (the extension's `Origin`) and `validate_outbound_url` **per request** (the target URL's origin) — so widening it to permit a cross-origin target simultaneously caused the connection-time check to reject the page's own tab. The new override reaches **only** `validate_outbound_url`, never `ws_origin_allowed`: it takes precedence over the `serve` global for that request's outbound-URL check and leaves the WS upgrade gate untouched. The wire protocol (`src/browser/protocol.rs`) gains an optional `allow_origin` field on `ControlRequest` (`#[serde(default, skip_serializing_if = "Option::is_none")]`, so pre-feature clients are byte-identical on the wire); `dispatch` and `start_stream` (`src/browser/bridge.rs`) resolve the per-request value over the global and emit a WARN at the dispatch site whenever the override is exercised. The override is **per-request and explicit — never a default**: blast radius is bounded by the browser's own CORS (response body readable only if the target sends permissive CORS), and a token holder can already issue authenticated same-origin requests. Covered by protocol round-trip/back-compat tests and bridge unit tests (override wins over and falls back to the global; the WS gate is unaffected). See [docs/browser-bridge.md](docs/browser-bridge.md) and [ADR-0036](docs/adrs/adr-0036.md).
- **Multi-tab routing on the browser bridge via `X-Omni-Bridge-Target`** ([#908](https://github.com/rust-works/omni-dev/issues/908)): The bridge now accepts **multiple authenticated browser tabs concurrently**, replacing the single `Option<WsConn>` connection slot with a `conn_id`-keyed `HashMap<u64, WsConn>` registry (`src/browser/bridge.rs`); each tab authenticates independently via the token subprotocol and a new connection never evicts an existing one. A request selects its target tab by connection id or by unique `Origin` through the new `X-Omni-Bridge-Target` header (or the `target` body field on `POST /__bridge/request`, with the header taking precedence and being stripped before forwarding), surfaced on the thin client as `omni-dev browser bridge request --target <ID|ORIGIN>`. `resolve_target()` routes the request, rejecting ambiguous matches with `409` and unknown targets with `404`; with a single tab connected the target is optional (v1 back-compat) and only becomes required once more than one tab is connected. The protocol (`src/browser/protocol.rs`) gains a `TabInfo { id, origin }` struct and `StatusResponse` grows a `tabs: Vec<TabInfo>` listing every connected tab (`browser_origin` retained for back-compat). See [docs/browser-bridge.md](docs/browser-bridge.md) and [ADR-0036](docs/adrs/adr-0036.md).
- **Per-request `--credentials` on the browser bridge `request`** ([#920](https://github.com/rust-works/omni-dev/issues/920)): `omni-dev browser bridge request` gains a `--credentials <include|omit|same-origin>` flag controlling the browser `fetch()` credentials mode per request. A new `Credentials` enum (`ValueEnum` for clap, with an `as_fetch_value()` wire helper) threads an optional `credentials` field through `ControlRequest` and `Command` (`#[serde(skip_serializing_if)]`, so v1 clients stay byte-identical on the wire) into `dispatch` and `start_stream`; the `browser-bridge.js` snippet reads `cmd.credentials || 'include'` instead of the hard-coded `'include'`. The default stays `include` (send cookies/auth) so existing callers are unaffected. The primary use case is reading wildcard-CORS (`Access-Control-Allow-Origin: *`) cross-origin assets the browser refuses to expose to a credentialed request — pair `--credentials omit` with `--allow-origin`. Covered by unit tests for defaults, all three modes, invalid-value rejection, wire mapping, serialization round-trips, and field omission. See [docs/browser-bridge.md](docs/browser-bridge.md).
- **Project website** ([#875](https://github.com/rust-works/omni-dev/issues/875)): A complete [Zola](https://www.getzola.org/) static site for omni-dev, deployed to GitHub Pages at [omni-dev.john-ky.io](https://omni-dev.john-ky.io) via a new `website.yml` workflow that builds and publishes on pushes to `main` touching `website/**`. The landing page carries a four-tab demo (Jira, Confluence, Create PR, Twiddle commits) pairing reproducible asciicasts with screenshots of the rendered Atlassian artefacts, an expanded install section distinguishing the CLI-only and `--features mcp` paths, and a link to a self-contained [tutorial page](https://omni-dev.john-ky.io/tutorial.html) walking through the four core workflows and doubling as a JFM reference (nested expanders, layout sections, decision/task lists, directive tables, status pills, mentions, inline cards, and the `adf-unsupported` escape hatch). A new `website` project scope is registered for the promo site. Cast/demo sources live at [rust-works/omni-dev-demo](https://github.com/rust-works/omni-dev-demo).

### Changed
- **Browser bridge commands nested under `bridge` (breaking)** ([#916](https://github.com/rust-works/omni-dev/issues/916)): The `omni-dev browser` command tree now groups both the long-lived server and the thin client under a `bridge` parent: `omni-dev browser bridge` becomes `omni-dev browser bridge serve`, and `omni-dev browser request` becomes `omni-dev browser bridge request`. `bridge` was previously doing double duty as both the noun (the server) and the verb (run it) while `request` — which only makes sense against a running bridge — sat as its sibling; nesting `serve` and `request` under `bridge` makes the grouping read correctly and leaves room for future bridge-scoped subcommands (e.g. `status`, `stop`). In `src/cli/browser/bridge.rs` the former server struct is renamed `ServeCommand` and a new parent `BridgeCommand` holds a `Serve` / `Request` subcommand split; `src/cli/browser.rs` drops the top-level `Request` variant. This is a **breaking CLI change** — scripts invoking `omni-dev browser bridge` or `omni-dev browser request` must move to the nested paths. No deprecated aliases are kept: the feature shipped only in this `[Unreleased]` cycle, so a hard break is cleanest. Docs ([docs/browser-bridge.md](docs/browser-bridge.md), [ADR-0036](docs/adrs/adr-0036.md)), the startup banner, and the help snapshot are updated to match.
- **Actionable Over-`--max-body-bytes` Error + Canonical Pagination Loop** ([#909](https://github.com/rust-works/omni-dev/issues/909)): When a browser response exceeds `--max-body-bytes` (default 8 MiB), the bridge's `502` now names the observed decoded size and the configured limit and steers the operator toward both remedies — page the request to fetch less per call (narrow the time range or lower a `limit`/page size) or raise `--max-body-bytes` — instead of the bare `"browser response body exceeds --max-body-bytes"`. The message is produced in `dispatch()` (`src/browser/bridge.rs`) where the decoded length and configured cap are already in scope, and reaches the `omni-dev browser bridge request` user verbatim via its `bridge returned {status}: {text}` relay. The body cap and its `502` status are unchanged — this is ergonomics, not a loosening of the bound. [docs/browser-bridge.md](docs/browser-bridge.md) gains a runnable canonical drain loop (the Grafana/Loki `direction=backward` worked example), using the `request` thin client + `jq` to take the oldest returned timestamp as the next `end` and repeat until a page is empty, with a note tying a tripped over-limit error back to lowering the page size or raising the cap. No new CLI surface — pagination conventions are per-API, so the bridge stays a thin generic proxy and orchestration is left to the caller. Covered by extending the existing `oversized_decoded_binary_body_is_rejected` integration test to assert the new guidance text.

### Fixed
- **SSH Remotes Work for `git branch create pr`** ([#903](https://github.com/rust-works/omni-dev/issues/903)): Remote git operations no longer depend on libgit2's network transport, which the vendored libgit2 inside released binaries lacks a reliable SSH implementation for — causing `omni-dev git branch create pr` (and any remote operation) to fail on SSH remotes (`git@github.com:…`) with `unsupported URL protocol; class=Net (12)` even though the user's `git`, SSH keys, and network were all fine. Both affected operations in `src/git/repository.rs` now shell out to the user's `git` CLI via a new private `GitRepository::run_git` helper: `push_branch` runs `git push --set-upstream <remote> <branch>` (recording the tracking branch in the same step) and `branch_exists_on_remote` runs `git ls-remote --heads <remote> <branch>`, comparing the returned ref column exactly against `refs/heads/<branch>` to avoid the tail-glob false positives `git ls-remote`'s pattern matching would otherwise produce (e.g. `team/<branch>` matching `<branch>`). This delegates all URL-scheme and authentication handling (ssh-agent, `~/.ssh/config`, credential helpers) to `git`, matching the project's existing "favour the git CLI" guidance and the user's manual workflow. The now-obsolete libgit2 credential-callback machinery (`make_auth_callbacks`, `get_ssh_identity_for_host`, `format_auth_error`, `extract_hostname_from_git_url`, the `MAX_AUTH_ATTEMPTS` constant) and the `ssh2-config` dependency are removed. Covered by four new unit tests exercising push + presence/absence detection and the exact-ref-match guard against a local bare remote.

## [0.27.0] - 2026-05-23

### Added
- **JFM→ADF Auto-Conversion for Rich-Text Custom Fields** ([#866](https://github.com/rust-works/omni-dev/issues/866)): String values supplied for rich-text/textarea JIRA custom fields are now auto-converted from JFM markdown to ADF — both via the MCP `jira_write` `fields` escape hatch and via the CLI's `--set-field NAME=VALUE`. Callers no longer need to hand-craft `{"version":1,"type":"doc",…}` payloads for fields like Acceptance Criteria (`customfield_19300`); a plain `"- bullet\n- bullet"` is converted using the same pipeline that already serves `content`/description and body sections. An empty string (or YAML null on the CLI side) clears the field. JSON object values are passed through unchanged, preserving the raw-ADF path for callers who need it. Implemented as a new `convert_textarea_string_values` helper in `src/atlassian/custom_fields.rs` (shared with a relaxed `resolve_custom_fields` that now accepts string scalars for rich-text fields) and wired into `run_jira_write` in `src/mcp/jira_core_tools.rs` with a lazy prefilter — editmeta is only fetched when at least one `fields` value is a string. Per-issue editmeta is cached via a new `editmeta` slot on `CatalogueCache` (`src/mcp/catalogue_cache.rs`) with a 60-second TTL (separate `EDITMETA_TTL` constant) and the same single-flight semantics as the other catalogue slots. Metadata-lookup failures fall back to passthrough — the API surfaces its own error rather than failing the write. Covered by 7 unit tests on the conversion helper, 7 wiremock integration tests on `run_jira_write` (string→ADF, object passthrough, empty→null, non-textarea passthrough, editmeta-failure passthrough, invalid-nesting short-circuit, unknown-field passthrough), 3 CLI tests on `--set-field` for textareas, and 5 editmeta-cache unit tests.
- **Actionable Error for JIRA ADF-Required Fields** ([#867](https://github.com/rust-works/omni-dev/issues/867)): When `jira_write` submits a plain string to a JIRA rich-text custom field, JIRA returns HTTP 400 with `errors.<field_id>: "Operation value must be an Atlassian Document (see the Atlassian Document Format)"`. The error is now enriched with the offending field ID(s), a `To fix:` hint pointing at JFM markdown and the `omni-dev://specs/jfm` resource, and the verbatim original API message — instead of being relayed as an opaque body. Implemented via a new `jira_write_error` helper in `src/atlassian/client.rs` (paralleling `confluence_write_error` in `src/atlassian/confluence_api.rs`) and a new `AtlassianError::JiraAdfFieldRequired { fields, original_message, body }` variant in `src/atlassian/error.rs`. Multi-field 400s are joined into a single header line. Non-ADF 400s and all other status codes fall back to the existing `ApiRequestFailed` format. Scoped to the JIRA update path (`update_issue_with_custom_fields`); the create path is intentionally out of scope. Covered by three Display unit tests in `error.rs` and two integration tests (`run_jira_write_enriches_adf_required_error`, `run_jira_write_falls_back_for_non_adf_400`) in `src/mcp/jira_core_tools.rs`.
- **Confluence Inline (Anchored) Comments** ([#830](https://github.com/rust-works/omni-dev/issues/830)): omni-dev's Confluence comment surface now distinguishes footer comments (page-level) from inline comments (anchored to a text selection), closing the gap with the official Atlassian MCP server. Three additions: (1) new `omni-dev atlassian confluence comment add-inline <PAGE-ID> [FILE] --anchor-text "..." [--match-index N]` CLI subcommand and matching `confluence_comment_add_inline` MCP tool create inline comments by posting to `/wiki/api/v2/inline-comments` with the `inlineCommentProperties` payload Confluence requires (`textSelection`, `textSelectionMatchCount`, `textSelectionMatchIndex`); the index/count are resolved automatically by fetching the page via the existing `get_content` and counting occurrences of `--anchor-text` in the body (flattened to plain text via the new `adf_to_plain_text` helper in `src/atlassian/convert.rs`), with explicit errors for "not found", "ambiguous (specify --match-index 1..=N)", and out-of-range overrides. (2) `confluence comment list` (CLI) and `confluence_comment_list` (MCP) gain a `--kind {footer,inline,all}` filter defaulting to `all`, which issues both `/footer-comments` and `/inline-comments` v2 GETs and merges the results sorted by creation time. (3) New `omni-dev atlassian confluence comment replies <COMMENT-ID> --kind {footer,inline}` CLI subcommand and `confluence_comment_replies` MCP tool fetch a comment's reply thread via `GET /wiki/api/v2/{footer|inline}-comments/{id}/children` — the caller must commit to a kind because Confluence stores replies on kind-specific endpoints. `ConfluenceComment` gains a `kind: CommentKind` field (serialised as lower-case `"footer"` / `"inline"`) so a merged listing identifies each row; the table printer adds the kind to each comment's header line. Footer behaviour is unchanged — `confluence comment add` still posts a footer comment, the v2 footer endpoints still back it, and existing scripts that never pass `--kind` to `list` will see inline comments included by default (the intentional behaviour change). Implemented across `src/atlassian/confluence_api.rs` (new `CommentKind` enum, `InlineAnchor` struct, `get_page_inline_comments`, `get_comment_replies`, `add_inline_page_comment`, `resolve_anchor`, refactored pagination into a shared `fetch_comments_paginated` helper), `src/atlassian/convert.rs` (`adf_to_plain_text`), `src/cli/atlassian/confluence/comment.rs` (new `AddInlineCommand`, `RepliesCommand`, shared `parse_comment_input` helper), and `src/mcp/confluence_tools.rs` (extended list params, new add-inline / replies tool handlers, registration). Covered by anchor-resolution unit tests (0/1/N-match branches, `--match-index` bounds), wiremock integration tests for each new HTTP path (`POST /inline-comments`, `GET /{kind}-comments/{id}/children`, `list --kind all` issuing both GETs), and the registration test in `tool_router_registers_all_confluence_tools`.
- **`confluence space pages` Command + `confluence_space_pages` MCP Tool** ([#829](https://github.com/rust-works/omni-dev/issues/829)): New `omni-dev atlassian confluence space pages <KEY>` CLI subcommand and matching `confluence_space_pages` MCP tool enumerate pages within a Confluence space against `GET /wiki/api/v2/spaces/{id}/pages`. The space key is resolved to a space ID via the existing `resolve_space_id` helper, so callers pass the human-readable key (e.g. `ENG`) rather than an opaque numeric ID. Optional `--status` (common values: `current`, `archived`, `draft`, `trashed`) and `--sort` (common values: `id`, `-id`, `title`, `-title`, `created-date`, `-created-date`, `modified-date`, `-modified-date`) are passed through to the v2 API verbatim — no client-side allow-list, matching the `confluence_space_list` philosophy. Output records are summary-only — `id`, `title`, `status`, `parentId`, `authorId`, `createdAt` — with no page body, so listings of large spaces stay bounded in size. Pagination is explicitly cursor-driven (`--cursor` / `--limit`, default 25): the MCP tool returns a `next_cursor` callers thread back to fetch the next page, mirroring `list_spaces` rather than the auto-drained pattern in `get_space_root_pages`, because spaces with thousands of pages must not buffer entirely in memory on the MCP path. The `body-format` filter suggested in the issue is intentionally omitted — we never emit a body, so accepting it would only waste server bytes. Pairs with #828: agents can now walk from "what spaces exist?" → "what pages are in this space?" without prior knowledge of keys or IDs.
- **`confluence space list` Command + `confluence_space_list` MCP Tool** ([#828](https://github.com/rust-works/omni-dev/issues/828)): New `omni-dev atlassian confluence space list` CLI subcommand and matching `confluence_space_list` MCP tool list Confluence spaces against `GET /wiki/api/v2/spaces`. Filters `--keys <K1,K2>` (comma-separated), `--type <TYPE>`, and `--status <STATUS>` combine as AND on the wire; `--cursor` + `--limit` (default 25) page explicitly — the MCP tool returns a `next_cursor` callers pass back to fetch the next page so large space inventories never have to buffer in memory. Output fields are `id`, `key`, `name`, `type`, `status`, `homepageId`. `--type` / `--status` are passed through verbatim rather than restricted to a clap enum; Atlassian's `SpaceTypeEnum` includes template-derived values (`onboarding`, `xflow_sample_space`, `system`, `app`, …) that extend the four documented types, so a strict client-side allow-list would silently drift from reality. Invalid values surface as a clear `400 INVALID_REQUEST_PARAMETER` from Atlassian listing the current accepted set. Internally, the existing `resolve_space_id` helper was refactored to delegate to the new `list_spaces` method against the same endpoint, removing the duplicated query-building path.
- **`voice enroll` Subcommand** ([#805](https://github.com/rust-works/omni-dev/issues/805)): New CLI command at `src/cli/voice/enroll.rs` captures a microphone sample, computes a wespeaker speaker embedding, and persists it to `~/.omni-dev/voice/speakers/<name>.json`. Reuses `voice::capture::run_capture` in-process exactly as `voice capture` does — no shell-out, no duplicated pipeline. `--name <name>` chooses the JSON filename stem (default `default`); `--idle-after <secs>` stops on trailing silence (default 2 s); `--max-secs <secs>` enforces a hard upper bound (default 30 s, `0` disables); `--device <name>` selects the audio input; `--speaker-model <path>` overrides the wespeaker ONNX location; `--force` overwrites an existing enrolment. The deadline cap is enforced by a background watchdog thread that flips the same stop signal Ctrl-C uses, so capture terminates on the first of: idle silence, deadline reached, or user interrupt. The captured WAV is staged at `~/.omni-dev/voice/captures/.enroll-<UTC-timestamp>.wav` and deleted on success. The persisted `EnrolledSpeaker` JSON shape (`name`, `model`, `dim`, `vector`, `samples_used`, `enrolled_at`) matches the original #805 spec and is forward-compatible with extra fields via serde defaults. Covered by 9 unit tests for clap parsing, the install-hint error path, and WAV-format validation; the end-to-end embedding path is validated by the `#[ignore]`-gated `voice_enroll_speaker_test`.
- **`voice transcribe --speaker <name>` Filter** ([#805](https://github.com/rust-works/omni-dev/issues/805)): `omni-dev voice transcribe` now accepts `--speaker <name>` plus `--threshold <f32>` (default `0.5` — calibrated against `tests/fixtures/voice/two_speakers.wav` where within-speaker mean cosine is ≈ 0.91 and cross-speaker mean ≈ 0.07, leaving ~0.4 margin on both sides) and `--speaker-model <path>`. When `--speaker` is set, the transcribe command loads the enrolled embedding from `~/.omni-dev/voice/speakers/<name>.json`, re-reads the source WAV in parallel with the transcriber, and for each `Final` event embeds the corresponding PCM window via `WespeakerEmbedder` and drops the event when cosine similarity to the enrolled vector falls below the threshold. Surviving events are tagged with `speaker: Some(name)`. Windows shorter than 0.5 s (the minimum for stable cepstral-mean normalisation) are dropped conservatively rather than risk false positives. `Partial` and `Endpoint` events pass through untouched; the `Transcriber` trait is **not** extended — the filter is a separate composable layer over the existing event stream so future backends with native diarisation can bypass it. Implementation in `src/cli/voice/transcribe.rs::SpeakerFilter` (~140 LOC).
- **`voice install-model --variant <variant>` Flag** ([#805](https://github.com/rust-works/omni-dev/issues/805)): The install-model subcommand now accepts `--variant {whisper-tiny.en, speaker-wespeaker-en}` (default `whisper-tiny.en` for backwards compatibility — bare `voice install-model` continues to install the ASR model). Selecting `speaker-wespeaker-en` downloads [`wespeaker_en_voxceleb_resnet34_LM.onnx`](https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/wespeaker_en_voxceleb_resnet34_LM.onnx) (26.5 MB, SHA-256 `e9848563da86f263117134dfd7ad63c92355b37de492b55e325400c9d9c39012`) from the sherpa-onnx `speaker-recongition-models` release into `~/.omni-dev/voice/models/wespeaker-en-voxceleb-resnet34-LM/` via a direct `ureq` GET with SHA-256 verification and the same `.part` + atomic rename install pattern as the Whisper path. Both variants share the same `--dest`, `--force`, and idempotency semantics; the download transport bifurcates on the `ModelSource` carried by each variant's `ModelSpec`.
- **`WespeakerEmbedder` Speaker-Embedding Runtime** ([#805](https://github.com/rust-works/omni-dev/issues/805) / [ADR-0034](docs/adrs/adr-0034.md)): New `src/voice/speaker.rs` wraps `tract-onnx` v0.21 loading the wespeaker `voxceleb_resnet34_LM` ONNX model. `WespeakerEmbedder::new(path)` loads + optimises the ONNX graph and builds the FBANK mel filterbank + FFT plan once; `WespeakerEmbedder::embed(&[i16]) -> Vec<f32>` takes 16 kHz mono signed-PCM samples and returns a 256-dimensional L2-normalised embedding suitable for cosine similarity. Refuses windows shorter than 0.5 s (`MIN_EMBED_SAMPLES`) for which cepstral-mean normalisation would be unstable. `Send + Sync` because `SimplePlan::run` takes `&self`, allowing one embedder to be shared across worker threads without internal mutexing. Companion `EnrolledSpeaker` struct in the same module handles atomic load/save against the JSON storage convention, with `dim`/`vector.len()` parity verified on load. Free functions `cosine(&[f32], &[f32]) -> f32` and `l2_normalise(Vec<f32>) -> Vec<f32>` round out the public surface. Runtime choice documented in [SPIKE.md on `issue-805-spike-tract-speaker`] and [ADR-0034](docs/adrs/adr-0034.md). The `tract-linalg` build-dep on `cc` (compiling platform-conditional SIMD assembly stubs, not C++) is the same C-toolchain footprint already required by [ADR-0033](docs/adrs/adr-0033.md)'s `onig_sys`/`ring`.
- **`voice features` Kaldi-Style FBANK Module** ([#805](https://github.com/rust-works/omni-dev/issues/805)): New `src/voice/features.rs` implements 80-dim Kaldi-style FBANK feature extraction matching sherpa-onnx's `kaldi-native-fbank` defaults for the wespeaker recipe — 16 kHz mono, 25 ms / 10 ms framing, pre-emphasis 0.97, Hamming window, mel scale `1127·ln(1 + f/700)` (Slaney/Kaldi), low-freq 20 Hz / high-freq 8000 Hz, 512-point FFT (smallest power of two ≥ 400-sample window), log applied to mel energies with `1e-10` numerical floor, cepstral mean normalisation across the supplied window's frames. Pure-Rust DSP — only non-`std` dep is `rustfft` for the FFT. Public surface: `compute_fbank(pcm: &[f32], filters: &[Vec<f32>]) -> Result<Vec<Vec<f32>>>` and `build_mel_filterbank(num_bins, fft_size, sample_rate) -> Result<Vec<Vec<f32>>>`, plus the parameter constants (`SAMPLE_RATE`, `FRAME_LENGTH_MS`, `FRAME_SHIFT_MS`, `NUM_MEL_BINS`, `FFT_SIZE`, `LOW_FREQ_HZ`, `HIGH_FREQ_HZ`, `PREEMPHASIS`). Covered by 8 unit tests including filterbank dimensionality, non-negativity, monotonic centre-frequency ordering, frame-count formula correctness, CMN-zeros-mean-per-bin invariant, and `hz<->mel` round-trip stability.
- **`omni_dev::voice::paths` Helpers** ([#805](https://github.com/rust-works/omni-dev/issues/805)): New `src/voice/paths.rs` centralises `~/.omni-dev/voice/...` layout so capture, transcribe, install-model, and enroll all derive paths from one source of truth. Public functions: `omni_dev_voice_root() -> Result<PathBuf>`, `captures_dir() -> Result<PathBuf>`, `speakers_dir() -> Result<PathBuf>`, `speaker_file(name: &str) -> Result<PathBuf>`. `speaker_file` rejects empty names, path separators (`/` or `\`), null bytes, and `.`/`..` as a defence-in-depth guard against accidental directory traversal. `src/cli/voice/capture.rs::default_output_path` now routes through `captures_dir()` rather than open-coding the home-dir join.
- **`#[ignore]`-Gated `voice_enroll_speaker_test` Integration Test** ([#805](https://github.com/rust-works/omni-dev/issues/805)): New `tests/voice_enroll_speaker_test.rs` exercises `WespeakerEmbedder` end-to-end against the committed `tests/fixtures/voice/two_speakers.wav` fixture (two LibriVox readers, 24.5 s, public-domain). Asserts the same spike-aligned separability gates that ADR-0034 was accepted on: within-speaker cosine ≥ 0.70 (spike measured ≈ 0.91) and all six cross-speaker cosines ≤ 0.40 (spike measured ≈ 0.07). A second test verifies the default 0.5 threshold correctly accepts a same-speaker query window and rejects both other-speaker windows — the runtime contract `voice transcribe --speaker` depends on. Both tests are `#[ignore]`-by-default because they need the wespeaker ONNX file on disk; run locally with `omni-dev voice install-model --variant speaker-wespeaker-en` followed by `cargo test --test voice_enroll_speaker_test -- --ignored`, or point at a pre-staged install via `OMNI_DEV_VOICE_SPEAKER_MODEL`.
- **`whisper-candle` ASR Backend** ([#802](https://github.com/rust-works/omni-dev/issues/802)): New pure-Rust Whisper backend at `src/voice/backends/candle.rs` selected via `--backend whisper-candle` (or `OMNI_DEV_VOICE_BACKEND=whisper-candle`) on `omni-dev voice transcribe`. Built on `candle-core` / `candle-nn` / `candle-transformers` 0.10.x with `openai/whisper-tiny.en` (revision `refs/pr/15`), greedy decode (temperature 0), English-only, no timestamps; RTF ≈ 0.10 on CPU per the spike in #813. Emits one `TranscriptEvent::Final` per decoded mel segment (`event_id = Ulid::new()`, `confidence` from per-token average log-probability mapped via `avg_logprob.exp().clamp(0.0, 1.0)`, `words = None`, `speaker = None`, `revisable = false`) followed by a terminal `Endpoint { kind: StreamEnd }`. The `m::model::Whisper` handle is held behind a `Mutex` because its encoder and decoder methods take `&mut self` and the decoder owns a per-segment KV cache that concurrent calls would corrupt. Mel-filter coefficients (80-bin, 64 KB) are vendored as `src/voice/backends/candle_melfilters.bytes` and embedded via `include_bytes!`. Model files are loaded from a directory resolved with priority `--model <path>` → `OMNI_DEV_VOICE_WHISPER_MODEL` → `~/.omni-dev/voice/models/whisper-tiny.en/`; missing-model errors carry an actionable install hint pointing at `voice install-model`. Default backend remains `mock` until the model storage convention has been through at least one release cycle. Recorded in [ADR-0033](docs/adrs/adr-0033.md). Covered by unit tests for `Send + Sync`, mel-filter shape, the missing-model error path, factory dispatch, and a `#[ignore]`-by-default end-to-end test at `tests/voice_transcribe_candle_test.rs` that exercises the backend against the committed `short_en.wav` fixture with case-insensitive substring matching on the seven content words from the #813 baseline.
- **`omni-dev voice install-model` Subcommand** ([#802](https://github.com/rust-works/omni-dev/issues/802)): New one-time CLI command at `src/cli/voice/install_model.rs` downloads the Whisper tiny.en `config.json`, `tokenizer.json`, and `model.safetensors` from `openai/whisper-tiny.en` (revision `refs/pr/15`) into `~/.omni-dev/voice/models/whisper-tiny.en/` via `hf-hub`. `--dest <path>` overrides the install directory; `--force` re-downloads when files are already present. Idempotent — skips when all three required files exist and are non-empty. Atomic — each file is staged as a `.<name>.part` sibling and renamed into place, so partial downloads never leave half-written model artifacts at the destination. Progress lines (`fetching <file>... done (N bytes in M.Ms)`) are emitted to stderr so stdout stays free for future machine-readable additions. Bumps the model-download cost to install time rather than transcribe time so network failures surface explicitly when the user opts in to installing rather than silently on first `voice transcribe`.
- **`--model <path>` Flag on `voice transcribe`** ([#802](https://github.com/rust-works/omni-dev/issues/802)): `omni-dev voice transcribe` now accepts `--model <path>` to override the backend's model directory (for `whisper-candle`, this overrides `OMNI_DEV_VOICE_WHISPER_MODEL` and the default at `~/.omni-dev/voice/models/whisper-tiny.en/`). The flag is plumbed through to `VoiceOpts.model` (previously hardcoded `None`) and is ignored by the `mock` backend. The shared `src/voice/models.rs` helper centralises three-tier path resolution so the backend's load path and `install-model`'s download path can't diverge.

### Changed
- **`jira_field_list` Exposes `schema.custom` and Maps Rich-Text Fields** ([#865](https://github.com/rust-works/omni-dev/issues/865)): The `omni-dev atlassian jira field list` CLI and matching `jira_field_list` MCP tool now report `schema_type: "richtext"` for ADF-required custom fields (e.g. *Acceptance Criteria*) and expose the raw plugin URI from `schema.custom` as a new `schema_custom` field. Callers — including AI agents — can detect rich-text fields up front instead of discovering them via a `400 "Operation value must be an Atlassian Document"` on the first write attempt. Previously the JIRA API's `schema.custom` value (e.g. `com.atlassian.jira.plugin.system.customfieldtypes:textarea`) was deserialised and discarded; `schema_type` reported `"string"` indistinguishably from plain string fields. Implementation extends `JiraFieldSchema` to deserialise `custom`, adds `JiraField.schema_custom: Option<String>` (`#[serde(skip_serializing_if = "Option::is_none")]` so non-custom fields stay shape-compatible), and routes `(schema.type, schema.custom)` through a new file-private `map_schema_type` helper in `src/atlassian/client.rs::get_fields` that returns `"richtext"` when `schema.custom == "...customfieldtypes:textarea"` and passes everything else through unchanged. The textarea URI is consolidated into a `pub(crate) const TEXTAREA_CUSTOM_TYPE` shared with the existing `EditMetaField::is_adf_rich_text` discriminator. The CLI table's TYPE column picks up the new value transparently; the MCP tool description is extended to document the new contract for agent enumeration. Behaviour change for consumers that compared `schema_type` against the literal `"string"` for textarea custom fields — they now see `"richtext"`, which is more accurate.
- **`src/voice/models.rs` Generalised to `ModelSpec` Shape** ([#805](https://github.com/rust-works/omni-dev/issues/805)): The Whisper-only constants from #802 are now exposed through a `ModelSpec` struct (with a sibling `ModelSource` enum carrying either `HfHub { repo_id, revision }` or `HttpReleaseAsset { url, sha256, bytes }`) so the same registry hosts both ASR (Whisper, HfHub-sourced) and speaker (wespeaker, GitHub-release-asset-sourced) models. Two canonical instances are registered: `WHISPER_TINY_EN` and `SPEAKER_WESPEAKER_EN`. `ModelSpec` exposes `default_dir()`, `resolve_dir(override)`, `required_files_in(dir)`, and `ensure_present(dir)` as methods; the last carries the variant-specific install hint baked into the spec (`run \`omni-dev voice install-model\` or pass --model <path>` for Whisper, `run \`omni-dev voice install-model --variant speaker-wespeaker-en\` or pass --speaker-model <path>` for wespeaker). All pre-existing Whisper helpers (`MODEL_ID`, `REVISION`, `REQUIRED_FILES`, `DEFAULT_VARIANT_DIR`, `default_whisper_model_dir`, `resolve_whisper_model_dir`, `ensure_model_present`, `required_files_in`) are retained as thin shims over `WHISPER_TINY_EN`, so existing callers in `src/voice/backends/candle.rs`, `src/voice/factory.rs`, and `src/cli/voice/install_model.rs` keep compiling with no signature change.

### CI/CD
- **Release Workflow Ships `omni-dev-mcp` Binary** ([#874](https://github.com/rust-works/omni-dev/pull/874)): The `release.yml` workflow now builds with `--features mcp --bin omni-dev --bin omni-dev-mcp` and includes both binaries in the per-platform release archives (Linux/macOS/Windows). The MCP server binary was previously buildable from source but missing from GitHub release assets. Both binaries are stripped on linux and macos.

## [0.26.0] - 2026-05-11

### Added
- **ADF Validator: Per-Context Mark Allow-Lists & Per-Mark Attribute Schemas** ([#733](https://github.com/rust-works/omni-dev/issues/733)): `validate_document` now flags two more violation classes — `DisallowedMark { mark_type, parent_type, inline_index, path }` (e.g. a `code` mark on text inside a `heading`, a `border` block mark on a `paragraph`) and `InvalidMarkAttr { mark_type, attr_name, problem, inline_index, path }` (e.g. `link.href: "not a url"`, `subsup.type: "side"`, `border.size: 5`). Inline-mark allow-lists are keyed by the *parent* container (since `code` is fine on `paragraph` text but not on `heading` text); block-mark allow-lists are keyed by the node's *own* type (since `border`/`backgroundColor` are valid on `tableCell` and `tableHeader` only, while `alignment`/`indentation` are valid on `paragraph` and `heading` only). Per-mark attribute schemas reuse the `AttrSchema` / `AttrType` / `AttrProblem` machinery from the attribute slice — `link` requires an absolute-URL `href`, `subsup` requires `type ∈ {sub, sup}`, `alignment` requires `align ∈ {start, end, center, right, left}`, `indentation` requires `level ∈ 1..=6`, `breakout` requires `mode ∈ {wide, full-width}`, `border` requires hex `color` and `size ∈ 1..=3`, `annotation` requires `id` and `annotationType`. `unsupportedMark` and `unsupportedNodeAttribute` are accepted under any context as round-trip preservation wrappers (matching `unsupportedBlock`/`unsupportedInline`'s treatment in #717). Per-mark and per-context hints are added to `adf_hints` (`headings cannot carry the code mark — use a paragraph if you need code styling`, `link.href must be an absolute URL`, etc.). New module: `src/atlassian/adf_mark_schema.rs`. Modified: `src/atlassian/adf_schema.rs` (new `DisallowedMark` / `InvalidMarkAttr` enum variants + walker calls `validate_marks` for every visited child), `src/atlassian/adf_attr_schema.rs` (`check_value` made `pub` so the mark module can reuse type-checking), `src/atlassian/error.rs` (Display arms for the new variants), `src/atlassian/adf_hints.rs` (per-mark hint tables). Covered by 22 unit tests in `adf_mark_schema` (allow-list per parent context, block-mark contexts, mark-attr happy/sad path per mark type), 2 new integration tests in `tests/adf_schema_test.rs` exercising `code`-on-heading and malformed `link.href` through `validate_document`, and a new `print_dry_run_fails_on_disallowed_mark` end-to-end test in the CLI helper.
- **ADF Validator: Per-Node Attribute Schemas** ([#733](https://github.com/rust-works/omni-dev/issues/733)): `validate_document` now validates each node's `attrs` against a per-node-type schema and reports two new violation variants — `MissingAttr { node_type, attr_name, path }` and `InvalidAttr { node_type, attr_name, problem, path }`. The `problem` field carries an `AttrProblem` enum (`NotInEnum { allowed, actual }`, `OutOfRange { lo, hi, actual }`, `OutOfRangeF { … }`, `WrongType { expected }`, `BadFormat { reason }`) so display wording is centralised. Schemas are encoded in a new `src/atlassian/adf_attr_schema.rs` module for the user-visible mistakes called out in the issue (`panel.panelType ∈ {info, note, warning, success, error, custom}`, `heading.level ∈ 1..=6`) and for the rest of the typed attrs across the snapshot: `media.type` (file/link/external), `mediaSingle.layout`, `taskItem`/`decisionItem` `state` and `localId`, `taskList`/`decisionList.localId`, `status.color` and `text`, `extension.extensionType`/`extensionKey`, `mention.id`, `date.timestamp`, `emoji.shortName`, `embedCard.url` (URL-parseable), `expand.title`, `nestedExpand.title`, `orderedList.order` (positive integer), `layoutColumn.width` (0–100), `codeBlock.language`, plus more. Forward-compatibility preserved per ADR-0023's permissive philosophy: unknown node types and unknown attribute names trigger no violation; explicit `null` is treated as absent. Per-attribute hints are added to the `adf_hints` module (`set panelType to one of: info, note, warning, success, error, custom`, `heading.level must be an integer in 1..=6`, etc.). Reuses the shared `AttrSchema` / `AttrType` machinery — designed to be reused by mark-attribute validation in the third sub-PR. Modified: `src/atlassian/adf_schema.rs` (new `MissingAttr` / `InvalidAttr` enum variants + walker calls `validate_attrs` for every visited child), `src/atlassian/error.rs` (Display arms for the new variants), `src/atlassian/adf_hints.rs` (per-attr hint table), `src/atlassian/adf_attr_schema.rs` (new module). Covered by 21 unit tests in `adf_attr_schema`, 9 integration tests in `tests/adf_schema_test.rs`, and a new `print_dry_run_fails_on_invalid_attribute` end-to-end test exercising `panelType: "purple"` through the CLI helper.
- **ADF Validator: Quantifier / Arity Checks** ([#733](https://github.com/rust-works/omni-dev/issues/733)): `validate_document` now flags arity violations alongside the existing disallowed-child checks. `mediaSingle` with two `media` children, an empty `bulletList`, an empty `mediaGroup`, an empty `tableRow`, and a `layoutSection` with the wrong number of columns (must be 2 or 3) are all reported as `AdfSchemaViolation::Arity { parent_type, atoms, expected, actual, path }`. The `expected` field carries a new `Quantifier` enum (`ZeroOrOne`, `ZeroOrMore`, `OneOrMore`, `Exactly(n)`, `Range(min, max)`); `path` points at the parent whose count is wrong (whereas `DisallowedChild`'s `path` points at the child). Disallowed children do **not** count toward parent arity — an empty panel containing only an `expand` produces both a `DisallowedChild` (for the expand) and an `Arity` (for the panel needing ≥1 valid child). `unsupportedBlock` / `unsupportedInline` continue to be accepted under any known parent **and** count toward the parent's current term arity, so round-tripped documents carrying preservation wrappers still satisfy `+` / `Exactly(n)` requirements. Two upstream rules are intentionally relaxed for compatibility with real-world inputs: `doc` is `block*` instead of `block+` (so `AdfDocument::new()` — the canonical empty value for missing JIRA descriptions — does not trigger an arity violation) and `tableCell` / `tableHeader` are `block*` instead of `block+` (visibly-empty cells in real Confluence tables are common and accepted by the renderer). The `confluence write --dry-run` output and the HTTP-500 diagnosis in `confluence_write_error` both pick up the new variant via the existing `Display` impl, with arity-specific wording (`'bulletList' must contain at least one 'listItem' (found 0)`). Per-arity hints (empty list, two-media mediaSingle, layoutSection out-of-range, …) are added to the new `adf_hints` module. New module: `src/atlassian/adf_hints.rs`. Modified: `src/atlassian/adf_schema.rs` (full content-model rewrite + new `Quantifier` / `ContentTerm` types + new walker), `src/atlassian/error.rs` (Display switches per variant), `src/atlassian/confluence_api.rs` (uses `adf_hints::hint_for(&violation)`). Covered by 41 unit tests in `adf_schema` plus 6 unit tests in `adf_hints` plus 6 integration tests in `tests/adf_schema_test.rs`.

### Changed
- **`AdfSchemaViolation` Struct → Enum (Breaking)** ([#733](https://github.com/rust-works/omni-dev/issues/733)): The single concrete `AdfSchemaViolation` struct from #717 is now an enum so each new class of violation lands as its own variant (callers can opt in to strictness). The first variant `DisallowedChild { child_type, parent_type, path }` carries the existing fields verbatim — same `Display` wording, same path semantics — so the only migration is updating constructions and pattern-matches in callers. The second variant `Arity { … }` is added in the same release for the quantifier slice. Public `validate_document(&AdfDocument) -> Vec<AdfSchemaViolation>` signature is unchanged. The `hint_for` helper formerly inside `confluence_api.rs` moves to the new dedicated `src/atlassian/adf_hints.rs` module and now takes `&AdfSchemaViolation` so it can dispatch per variant. New `pub fn content_model(parent: &str) -> Option<&'static [ContentTerm]>` exposes the full per-parent term sequence (with quantifiers) for callers that want richer schema introspection than `allowed_children` provides.
- **OpenAI/Ollama Backends Enforce JSON Schema Response Format** ([#702](https://github.com/rust-works/omni-dev/issues/702)): `OpenAiAiClient` (used by `USE_OLLAMA=true`, `USE_OPENAI=true`, and OpenAI-compatible servers like LM Studio) now honours `RequestOptions::response_schema` by populating `response_format: {type: "json_schema", json_schema: {name, strict, schema}}` on `/v1/chat/completions`. Previously the option was silently dropped and the backend reported `supports_response_schema = false`, so structured calls (twiddle, check, PR generation) received hard-validated JSON only on `claude-cli` and prose-instructed YAML everywhere else — the most visible symptom being invented commit scopes that were not in `.omni-dev/scopes.yaml`. Capability flips to `true` on both OpenAI and Ollama paths; the existing capability-driven dispatch in `client.rs::send_with_optional_schema` and the `ResponseFormat::JsonSchema` system-prompt swap pick up the change with no further wiring. Honoured by OpenAI ≥2024-08-06 (gpt-4o, gpt-5, o-series), LM Studio (decoder-constrained), and Ollama ≥0.5; older servers either 400 with a clear "model does not support structured output" error (a surface change for those users — schema enforcement was never available there before, and the error names the cause) or silently ignore the field and emit prose (matching today's behaviour). The wire body is byte-identical to today's when no schema is attached, guarded by `#[serde(skip_serializing_if = "Option::is_none")]` on the new `OpenAiRequest::response_format` field. The schema registry's existing types (`AmendmentFile`, `PrContent`, `AiCheckResponse`) were tightened to satisfy OpenAI's strict-subset rule that every property in `properties` must also appear in `required`: `Amendment.summary` becomes a non-optional `String` (defaulting to empty for graceful YAML loading), `AiCommitCheck` and `AiIssue` use `#[schemars(extend("required" = [...]))]` to force `Option<T>` and serde-defaulted fields into `required` while preserving nullability via `type: ["...", "null"]`. A new CI test (`schemas_satisfy_openai_strict_subset` in `src/claude/response_schema.rs`) walks each schema and asserts the invariant, catching drift the moment any new optional field is added without an `extend(...)` override. Implementation localised to `src/claude/ai/openai.rs` (~150 LOC including tests) plus the schema-shape adjustments; covered by serde-level body-shape tests, wiremock round-trip tests for both schema-bearing and schemaless paths, and the new strict-subset CI check.

### Added
- **TTL-Bounded In-Memory Cache for JIRA Catalogue MCP Tools** ([#719](https://github.com/rust-works/omni-dev/issues/719)): New `src/mcp/catalogue_cache.rs` module wraps the four near-static JIRA catalogue endpoints (`jira_link_types`, `jira_field_list`, `jira_project_list`, `jira_board_list`) so repeated MCP invocations within a server-process lifetime collapse to a single HTTP request per catalogue per hour, instead of rebuilding an `AtlassianClient` and hitting JIRA every time. `CatalogueCache` lives as `Arc<CatalogueCache>` on `OmniDevServer`; each catalogue has its own `tokio::sync::RwLock<Option<CacheEntry<T>>>` slot, with single-flight via the read-fast-path / write-on-miss / double-check pattern (no extra mutex). Entries are tagged with the issuing `instance_url` and refetched on drift. Failed fetches do not poison the cache — they propagate the error and leave the slot untouched. Default TTL is 1 hour (`DEFAULT_TTL`), constructible with arbitrary `Duration` for tests. Parameterised methods (`get_projects(limit)`, `get_boards(project, board_type, limit)`) cache the *unfiltered, unbounded* result; the YAML helpers (`project_list_yaml`, `board_list_yaml`, `field_list_yaml`) apply limit/filter to the cached `Arc<…>` at serve time so the hit rate is independent of caller arguments. CLI commands remain uncached (one-shot per process; nothing to amortise). Recorded in [ADR-0024](docs/adrs/adr-0024.md). Covered by 7 wiremock tests in `catalogue_cache.rs` (per-catalogue cache hit, TTL expiry, instance-url drift, error propagation) plus updated `*_yaml` helper tests for the four affected helpers.
- **ADF Schema Validation in `confluence write --dry-run`** ([#718](https://github.com/rust-works/omni-dev/issues/718)): `omni-dev atlassian confluence write --dry-run` now runs the ADF schema validator from [#717](https://github.com/rust-works/omni-dev/issues/717) over the converted ADF after printing the JSON output, prints a `Validation:` section that reports either `OK` or one line per violation (using the `AdfSchemaViolation` `Display` impl, e.g. `✗ ADF schema violation at /0/0: 'expand' is not permitted inside 'panel'`), and exits non-zero when any violation is found. Acts as a CI pre-flight check that surfaces what Confluence would otherwise reject silently or with HTTP 500. First wiring point for the soft-launched validator; the converter fix and non-dry-run / MCP wiring are tracked in [#730](https://github.com/rust-works/omni-dev/issues/730). Implemented as a ~20-line addition to `print_dry_run` in `src/cli/atlassian/helpers.rs`; covered by a new `print_dry_run_fails_on_schema_violation` unit test that exercises the `panel`-containing-`expand` case.
- **Data-Driven ADF Content-Model Schema and Validator** ([#717](https://github.com/rust-works/omni-dev/issues/717)): New `src/atlassian/adf_schema.rs` module encodes the full allowed-children content model from the upstream `@atlaskit/adf-schema` npm package, transcribed faithfully from `json-schema/v1/full.json` of the pinned tarball (version `52.9.5`, SHA-256 `90b9b26f5cdf6f0850cebe5cf2df7662601b249322d6bcbeead712ca018e0b56`, recorded in `SCHEMA_VERSION` and `UPSTREAM_TARBALL_SHA256` constants). Storage is a static `LazyLock<HashMap<&str, &[&str]>>` lookup; public helpers are `allowed_children(parent)`, `permits_child(parent, child)`, and a depth-first walker `validate_document(&AdfDocument) -> Vec<AdfSchemaViolation>` that reports nesting violations in document order with `parent_type`, `child_type`, and an index `path` from the document root. Covers every container node type in the upstream schema: `doc`, `paragraph`, `heading`, `panel`, `expand`, `nestedExpand`, `blockquote`, `bulletList`/`orderedList`/`listItem`, `tableCell`/`tableHeader`/`tableRow`/`table`, `layoutSection`/`layoutColumn`, `taskList`/`taskItem`/`blockTaskItem`, `decisionList`/`decisionItem`, `mediaSingle`/`mediaGroup`/`caption`, `codeBlock`, `bodiedExtension`, `bodiedSyncBlock`. Forward-compat preserved per [ADR-0020](docs/adrs/adr-0020.md) via two complementary mechanisms: unknown parent node types are treated permissively (their subtrees are not walked, so future Atlassian schema additions don't fail validation), and `unsupportedBlock`/`unsupportedInline` are accepted under any known parent via a walker-level short-circuit (the upstream JSON schema does not list these wrappers in any parent's allowed-children set, so faithfulness to upstream and round-trip survival can only coexist via the escape hatch). Unknown *children* under known parents are still flagged. Soft launch — the validator is a public library function, not wired into `markdown_to_adf` or any write path; the existing converter still emits structurally invalid ADF (the `nested_expand_inside_panel` and `nested_expand_inside_table_cell` tests document this) and fixing the converter to produce only schema-valid output is tracked as a separate effort. Recorded in [ADR-0023](docs/adrs/adr-0023.md). Covered by 20 unit tests (issue-#717 examples, sortedness invariants, walker depth-first ordering, permissiveness on unknown parents *and* unknown root doc_type, `unsupportedBlock`/`unsupportedInline` walker escape hatch with positive *and* negative assertions on the underlying allowed-children sets) plus 4 integration tests exercising the public API through `AdfDocument::from_json_str`; 100% line and region coverage on the new module. Deferred to follow-ups: converter fix + write-path enforcement ([#730](https://github.com/rust-works/omni-dev/issues/730)), CI drift detection against upstream ([#731](https://github.com/rust-works/omni-dev/issues/731)), code-generation from npm ([#732](https://github.com/rust-works/omni-dev/issues/732)), validator coverage beyond nesting (quantifiers, marks, attribute schemas — [#733](https://github.com/rust-works/omni-dev/issues/733)).
- **ADF Nesting Validation Wired Into API Send Path** ([#714](https://github.com/rust-works/omni-dev/issues/714), follow-up to [#717](https://github.com/rust-works/omni-dev/issues/717)): The schema validator from [#717](https://github.com/rust-works/omni-dev/issues/717) / [ADR-0023](docs/adrs/adr-0023.md) is now enforced at every API send site, so JFM → ADF conversions that produce structurally invalid ADF (e.g., `expand` inside `panel`) abort locally with an actionable message rather than surfacing as opaque HTTP 500s from Confluence. A new `src/atlassian/adf_validated.rs` module exposes a `ValidatedAdfDocument` newtype whose only fallible constructor (`try_new`) runs `adf_schema::validate_document`; the type system enforces "validated once before send" by giving every API send signature (`AtlassianApi::update_content`, `AtlassianClient::{update_issue, update_issue_with_custom_fields, create_issue, create_issue_with_custom_fields, add_comment, update_comment}`, `ConfluenceApi::{create_page, add_page_comment, update_content}`, plus per-section validation in `custom_fields.rs` for rich-text JIRA custom fields) a `&ValidatedAdfDocument` parameter rather than `&AdfDocument`. Errors are surfaced via a new `AtlassianError::InvalidAdfNesting` variant wrapping `AdfValidationError`, formatted as ``invalid ADF nesting — `expand` cannot be a child of `panel` at /0/0. hint: invert the nesting (put the panel inside the expand) or use siblings.`` — the path comes from the upstream walker, the per-(parent, child) hint table covers the high-traffic combinations called out in #714 (panel↔expand, table-cell-or-header↔expand, layout-section nesting, blockquote / list-item containment) with a generic ``"restructure the document so X is not a direct child of Y."`` fallback for any other forbidden pair. All violations are collected so the user can fix multiple issues in one editor pass. The `omni-dev atlassian convert to-adf` CLI also validates by default and accepts a new `--no-validate` flag for users inspecting invalid ADF for debugging. The existing `nested_expand_inside_panel()` and `nested_expand_inside_table_cell()` regression tests now assert that validation rejects the converted document, replacing assertions that the converter still emits these constructs (the converter fix is tracked in [#730](https://github.com/rust-works/omni-dev/issues/730)); `nested_expand_inside_layout_column()` is preserved as a positive case (`expand` inside `layoutColumn` is legitimate per the schema). Architecture decision recorded in [ADR-0025](docs/adrs/adr-0025.md).
- **Discrete `jira_link_list` / `jira_link_types` / `jira_link_remove` MCP Tools** ([#711](https://github.com/rust-works/omni-dev/issues/711)): Three new MCP tools mirror the existing `omni-dev atlassian jira link {list,types,remove}` CLI subcommands by re-exposing the existing `AtlassianClient::{get_issue_links,get_link_types,remove_issue_link}` methods individually, alongside the existing dispatch-style `jira_link` tool (which stays for back-compat). `jira_link_list(key)` returns inward + outward links with link type and target issue summary; `jira_link_types()` returns the configured link-type catalogue (id, name, inward, outward) — global per JIRA instance, not the types used in any particular issue; `jira_link_remove(link_id)` returns YAML `{status: ok}`. Pattern mirrors the existing `jira_watcher_{list,add,remove}` triplet. No new client methods. Implemented in `src/mcp/jira_tools.rs` (params structs, `*_yaml` helpers, three `#[tool]` handlers) with six wiremock unit tests covering each helper's happy path plus a 404-on-remove acceptance test, and registration tests updated in `src/mcp/server.rs` and `tests/mcp_integration_test.rs`. Caching of the link-types catalogue (which is near-static) is deferred to [#719](https://github.com/rust-works/omni-dev/issues/719); confirmation-flag conventions for destructive CLI operations are deferred to [#720](https://github.com/rust-works/omni-dev/issues/720).
- **Confluence Page Version History** ([#708](https://github.com/rust-works/omni-dev/issues/708)): New `omni-dev atlassian confluence history <id>` CLI subcommand and matching `confluence_history` MCP tool list version metadata for a Confluence page (number, ISO timestamp, author account ID, edit message, minor-edit flag), newest-first. Pure metadata — no body fetch — so the structural-diff feature in [#706](https://github.com/rust-works/omni-dev/issues/706) can resolve `from`/`to` references without inlining version-list logic. `--since` accepts either a numeric version (`5`) or an ISO 8601 date (`2026-01-01T00:00:00Z`); newest-first ordering means a date or version cutoff terminates pagination early. `--limit` defaults to 20 with `0 = unlimited`, matching the `comment list` / `label list` convention. Output supports `-o table|json|yaml|yamls|jsonl` and emits `{ page: { id, title, current_version }, versions: [...], truncated }`. Backed by a new `ConfluenceApi::list_page_versions` (auto-paginates `/wiki/api/v2/pages/{id}/versions?limit=&cursor=`, applies the `since` filter client-side, returns a `truncated` flag when `limit` cuts the listing short) plus a lightweight `get_page_metadata` that skips the body and space-key lookup. Tolerates null `authorId`/`createdAt`/`message` per the issue's risk #1. Implemented in `src/atlassian/confluence_api.rs`, `src/cli/atlassian/confluence/history.rs`, and `src/mcp/confluence_tools.rs`; covered by wiremock tests for pagination, numeric/ISO `since` filters, early termination at the cutoff, missing-field tolerance, `limit` truncation, empty results, and API-error propagation across the API, CLI, and MCP layers.
- **`jira_transition_list` MCP Tool & `transition list` CLI Subcommand** ([#710](https://github.com/rust-works/omni-dev/issues/710)): New single-purpose MCP tool `jira_transition_list(key)` and CLI subcommand `omni-dev atlassian jira transition list <KEY>` expose JIRA workflow transitions without forcing a full `jira_read` round-trip. Returns YAML with `{id, name, to_status: {id, name, category}, has_screen}` for each transition — the richer shape mirrors what sooperset/mcp-atlassian publishes, so agent prompts trained against that surface work unchanged. Equivalent to `jira_transition` with `list = true` (which is preserved for backward compatibility), but the dedicated tool is easier for LLM agents to discover. Implemented in `src/mcp/jira_core_tools.rs` (new `JiraTransitionListParams`, `run_jira_transition_list`, `jira_transition_list` tool method) and `src/cli/atlassian/jira/transition.rs`, with `JiraTransition` extended in `src/atlassian/client.rs` to expose the `to_status` and `has_screen` fields previously discarded by the deserialiser. Covered by new wiremock tests for happy path, 404 issue-not-found, empty transitions array, and a richly-populated response body.
- **Destructive CLI Commands: `--dry-run` Flag** ([#720](https://github.com/rust-works/omni-dev/issues/720)): All five destructive Atlassian subcommands (`omni-dev atlassian jira delete`, `omni-dev atlassian confluence delete`, `omni-dev atlassian jira watcher remove`, `omni-dev atlassian jira link remove`, `omni-dev atlassian confluence label remove`) now accept `--dry-run`, which prints `Would <verb> <target>.` and returns without making any API calls. `--dry-run` takes precedence over `--force`, so a script author can add `--dry-run` to an existing scripted `--force` invocation as a temporary safety check without removing the force flag. The `confluence label remove --dry-run` preview is purely client-side: labels are echoed back verbatim without verifying they exist on the page (documented trade-off in [ADR-0027](docs/adrs/adr-0027.md)). A new shared helper `src/cli/atlassian/confirm.rs` exposes `guard_destructive(opts) -> GuardOutcome { Proceed | Cancelled | DryRun }` and replaces the two duplicated `confirm_with_reader` copies that previously lived inline in `jira/delete.rs` and `confluence/delete.rs`. The decision is recorded in [ADR-0027](docs/adrs/adr-0027.md).
- **CI Drift Detection vs Upstream `@atlaskit/adf-schema`** ([#731](https://github.com/rust-works/omni-dev/issues/731)): A new `adf-schema-drift` binary plus a weekly scheduled GitHub Actions workflow (`.github/workflows/adf-schema-drift.yml`, also manual-dispatch) detect drift between the locally-encoded schema snapshot and the upstream npm package, opening or updating an issue labelled `adf-schema-drift` when drift is detected. The Rust implementation lives in a new `src/atlassian/adf_schema/drift.rs` module exposing `DriftReport` / `ParentDrift` value types, an HTTP-backed upstream tarball fetcher with `reqwest` (overridable per `OMNI_DEV_ADF_SCHEMA_LATEST_URL` for test injection) that verifies the response SHA-256 via `sha2`, a `.tgz` extraction routine using `flate2` + `tar`, and a JSON-schema parser tolerant of `anyOf` / `allOf` alias chains and marks-overlay definitions (refs into marks subtrees are filtered out so they do not leak into content children). The `adf-schema-drift` CLI binary (`src/bin/adf_schema_drift.rs`) takes `--format markdown|json|both` and `--output-dir`, writes `drift-report.md` / `drift-report.json`, and emits `drift=<bool>` / `version_changed=<bool>` to stdout and `$GITHUB_OUTPUT`. Adds `flate2`, `tar`, and `sha2` to `Cargo.toml`. Covered by wiremock-backed integration tests that spawn the compiled binary against synthesised upstream responses (success, 4xx, 5xx, connection refused, output-path failure modes).
- **`adf-schema-codegen` Binary and Vendored Upstream Schema Assets** ([#732](https://github.com/rust-works/omni-dev/issues/732)): The hand-transcribed `CONTENT_ENTRIES` allowed-children table is no longer the sole source of truth for the ADF nesting model. A new `adf-schema-codegen` binary (registered in `Cargo.toml`, sourced from `src/bin/adf_schema_codegen.rs`) reads `assets/adf-schema/full.json` plus its `provenance.json` sidecar (npm package, version, tarball URL, tarball SHA-256, `full.json` SHA-256), verifies the input via SHA-256 before generating, parses the upstream JSON schema via the existing `parse_upstream_full_json` function from `drift.rs` (now `pub` for reuse), and emits a deterministic `src/atlassian/adf_schema/generated.rs` formatted through `rustfmt`. A `--check` mode exits non-zero if the committed `generated.rs` is stale, suitable as a CI staleness gate. The vendored tarball assets (`assets/adf-schema/full.json`, `provenance.json`, and a refresh-workflow `README.md`) are checked into the repository. Two new tests assert (a) the generated `UPSTREAM_ENTRIES` slice agrees with the hand-maintained `CONTENT_ENTRIES` modulo a documented leniency allowlist, and (b) the vendored provenance SHA and npm version match the runtime `SCHEMA_VERSION` / `UPSTREAM_TARBALL_SHA256` constants.
- **Confluence HTTP 500 ADF Diagnosis** ([#715](https://github.com/rust-works/omni-dev/issues/715)): When a Confluence write returns HTTP 500, the error now includes a structured diagnosis pointing at the offending ADF nesting and an actionable hint, rather than surfacing as opaque `ApiRequestFailed`. A new helper `confluence_write_error` runs `adf_schema::validate_document` over the submitted payload at error time, returning a new `AtlassianError::ApiRequestFailedWithDiagnosis` variant whose `Display` impl produces a multi-line message — header, `Diagnosis:` line naming the offending parent/child, and an optional `Hint:` line — and intentionally omits the raw response body (which is logged at `debug!` by the call site). A `hint_for(parent, child)` table maps known bad pairs (`expand` inside `panel`, nested `expand` inside `expand`, etc.) to fix suggestions. Wired into `ConfluenceApi::{update_content, create_page, add_page_comment}`. Covered by integration tests for both the 500-with-violation and 500-without-violation paths across all three write operations.
- **Confluence Page Version Comparison** ([#706](https://github.com/rust-works/omni-dev/issues/706)): New `omni-dev atlassian confluence compare run|section` CLI surface and matching `confluence_compare` / `confluence_compare_section` MCP tools perform a structural diff between two versions of a Confluence page using an ADF-tree-aware engine rather than raw text diffing. The engine in `src/atlassian/diff.rs` splits documents into heading-delimited sections identified by stable slug paths (e.g. `/h2#background`), pairs nodes via a three-tier matcher (natural-key via `localId` / `url` / `id` attrs → content-hash → positional), then dispatches per-block: paragraphs and code blocks get word-level inline deltas via the `similar` crate, tables get cell-level pairing, lists get item-level pairing, and everything else falls back to opaque tree-equality. Three output detail levels — `summary` (counts only), `outline` (per-section change kind + drill-in cursors), `full` (embedded deltas, byte-budget-truncated via binary search to fit ~16 KiB / ≈4000 tokens) — let agents request progressively more detail; the `compare_section` drill-in tool decodes a stateless base64url cursor encoding `{page_id, from_v, to_v, section_path}` to re-fetch a single section in `unified`, `side-by-side`, or `markdown-inline` text format. Section-level filters (path, change kind, `min_change_chars`) are honoured by both surfaces. `ConfluenceApi` gains `get_page_at_version` and `resolve_version` (accepting `latest`, `previous`, `v-N`, numeric, or ISO 8601 date references). Adds `similar = "2.7"` to dependencies.
- **Confluence Page Move** ([#707](https://github.com/rust-works/omni-dev/issues/707)): New `omni-dev atlassian confluence move <id> --position append|before|after [--target <id>]` CLI subcommand and matching `confluence_move` MCP tool reparent or reorder a page within its current space via the v1 `PUT /wiki/rest/api/content/{id}/move/{position}/{target}` endpoint. Cross-space moves are not supported by the v2 API and are out of scope. `ConfluenceApi::move_page` returns a `MovedPage` whose ancestor chain is freshly fetched via `fetch_page_with_ancestors` (`GET /wiki/api/v2/pages/{id}?include-ancestors=true`) so callers see the post-move parent. Case-insensitive `parse_move_position` allows agents to pass the position as a string. Explicit 403 / 404 error messages distinguish permission-denied from missing-target.
- **Confluence Attachment Management** ([#709](https://github.com/rust-works/omni-dev/issues/709)): New `omni-dev atlassian confluence attachment upload|list|delete` CLI subcommands and matching `confluence_attachment_upload` / `confluence_attachment_list` / `confluence_attachment_delete` MCP tools cover end-to-end management of page attachments. Uploads are streamed via a new `AtlassianClient::post_multipart` helper with `X-Atlassian-Token: no-check` and no auto-429-retry (streamed bodies are non-replayable); the file is never fully buffered in memory. List is explicitly cursor-paginated — callers control page traversal, unlike the auto-draining label helpers — and `delete` honours an optional `purge: bool` flag that hard-deletes the attachment row rather than tombstoning it. The CLI `delete` subcommand prompts for confirmation by default (`--force` skips). Adds `mime_guess` to deps and enables `multipart` + `stream` features on `reqwest`; `tokio-util` becomes unconditional with `codec` + `io` features.
- **JIRA Project Version Management** ([#712](https://github.com/rust-works/omni-dev/issues/712)): New `omni-dev atlassian jira version list|create` CLI subcommands and matching `jira_version_list` / `jira_version_create` MCP tools manage JIRA project release versions. `list` returns a `JiraProjectVersionList` filterable by `--released true|false` and `--archived true|false` (tri-state — flag absent means "any"). `create` validates the `YYYY-MM-DD` release date up-front via a new `validate_iso_date` helper so callers see a clear error rather than JIRA's opaque 400. The slice also extracts `EnvGuard` and `AUTH_ENV_MUTEX` from the existing `atlassian::auth::tests` into a new `pub(crate) test_util` module so every Atlassian-touching test shares a single process-wide env-serialisation mutex, eliminating cross-test flakiness when several tests mutate `ATLASSIAN_TOKEN` / `ATLASSIAN_USER` / `ATLASSIAN_DOMAIN` concurrently; pre-existing inline guards in `confluence_tools`, `jira user`, and `confluence download` were migrated to the shared mutex.
- **JIRA Comment Edit** ([#713](https://github.com/rust-works/omni-dev/issues/713)): New `omni-dev atlassian jira comment edit <ISSUE> <COMMENT-ID> [--from-file|--message] [--visibility-type group|role] [--visibility-value <name>]` CLI subcommand and matching `jira_comment_edit` MCP tool send `PUT /rest/api/3/issue/{key}/comment/{id}` with the supplied JFM rendered to ADF plus optional visibility restriction. New `JiraVisibility` / `JiraVisibilityType` types serialise to JIRA's `{type, identifier}` wire format; `JiraComment` gains an `updated` timestamp field. A shared `parse_comment_input` helper is extracted from the existing `add` subcommand so the two share argument parsing. Covered by wiremock tests for success, 403 forbidden, 404 not-found, visibility payload round-trip, and rejection of unknown visibility types.
- **Resolve Duplicate Amendments in `commit twiddle`** ([#697](https://github.com/rust-works/omni-dev/issues/697)): The model occasionally emits duplicate amendments (same commit hash listed twice), which previously caused the apply path to fail because the first amendment rewrites the commit, leaving subsequent duplicates pointing at a now-nonexistent hash. A new `resolve_duplicate_amendments` step runs immediately after scope refinement in both the single-commit and batch-commit flows: when stdin is a TTY it prompts interactively (showing each candidate message, looping until a valid choice is made), otherwise it silently keeps the first occurrence and emits a warning to stderr. `--auto-apply` always takes the silent path. Order of non-duplicate amendments is preserved. The slice also tightens the system-prompt instructions for amendment cardinality so the duplicates show up less frequently in the first place.
- **Probe Local Server Context Length at Startup** ([#696](https://github.com/rust-works/omni-dev/issues/696)): The `OpenAiAiClient` (used by `USE_OLLAMA=true`, `USE_OPENAI=true`, and LM Studio) now queries the running server's actual loaded context length at client construction and feeds it into `AiClientMetadata`, so token-budget calculations reflect the real-time limits of the model that is actually loaded rather than the static registry estimate. LM Studio is probed via `/api/v0/models`; Ollama is probed via `/api/show`, scanning the response for an architecture-specific `*.context_length` key. `create_default_claude_client` becomes async to accommodate the probe; failures (network error, missing endpoint, unparseable response) fall back to the registry default with a warning. Covered by wiremock tests for both server shapes plus fallback / error paths.
- **YouTube Transcript Subcommand and Source-Agnostic Library Module** ([#687](https://github.com/rust-works/omni-dev/issues/687)): New top-level `omni-dev transcript` command tree fetches captions and metadata from media platforms, launching with full YouTube support. The CLI surface is `transcript youtube fetch|info|list-langs` with `--format srt|vtt|txt|json`, language selection, auto-generated caption opt-in, and translation. The underlying `src/transcript/` library is clap-free and structured around an async `TranscriptSource` trait (`fetch`, `list_languages`, `info`) so future platforms can be added without touching format converters; first-class `Cue`, `Transcript`, `LanguageInfo`, `MediaInfo`, `TrackKind`, `Format` (SRT / WebVTT / plain / JSON), and a `TranscriptError` enum covering invalid locator / parse failure / missing language / auto-caption-opt-in-required / playability refusal / HTTP error live under that module. The YouTube source uses the ANDROID_VR InnerTube client to bypass bot-detection gating that refuses WEB-context sessions on most videos: a per-session `visitorData` token is scraped from the watch-page HTML on first call and cached in `tokio::sync::OnceCell` (concurrent first-callers serialise on a single in-flight fetch rather than double-fetching), then forwarded under `context.client` on every InnerTube `/player` POST. The InnerTube envelope uses ANDROID_VR device-fingerprint fields (`deviceMake`, `deviceModel`, `osName`, `osVersion`, `androidSdkVersion`) and moves the API key from `?key=` to `X-Goog-Api-Key`. URL normalisation accepts watch / share / shorts / embed forms across www / m subdomains plus bare 11-character video IDs; the timedtext URL builder replaces (not appends) `fmt=` and `tlang=` params so YouTube serves `json3` rather than the legacy SRV3 format. Adds `async-trait` to deps. Documentation lives in `docs/transcript.md` (CLI reference, library architecture, error variants, recipe for adding a new source) with README and `docs/README.md` index entries.
- **Layered Model Catalog with User and Project Overrides** ([#684](https://github.com/rust-works/omni-dev/issues/684)): The previously single-source embedded model registry is replaced by a three-layer YAML loader so model specifications and provider configurations can be customised without a rebuild. Precedence (highest wins): explicit `--models-yaml <PATH>` / `OMNI_DEV_MODELS_YAML` env var → `./.omni-dev/models.yaml` (project) → `~/.omni-dev/models.yaml` (user) → `src/templates/models.yaml` (embedded — always present, hard error if malformed). Deep-merge happens at YAML value level: the `models` sequence is keyed by `api_identifier`, the `providers` mapping is deep-merged per provider, other keys use last-writer-wins. A new `ModelSource` enum (`Embedded` / `User` / `Project` / `Override`) is populated by the loader (never read from YAML) and tracked on each merged `ModelSpec` / `ProviderConfig`. `version: "1"` is added to the embedded catalog with advisory mismatch warnings for user / project files declaring a different version. Missing user / project files fall through silently; malformed files log an error and are skipped; an explicit override path that doesn't exist emits a warning. `config models show` renders the merged catalog with per-entry `source:` annotations and a layer-count header; `--embedded-only` falls back to a verbatim embedded dump. Recorded in [ADR-0022](docs/adrs/adr-0022.md), which supersedes [ADR-0011](docs/adrs/adr-0011.md). Covered by recovery / edge-case tests for scalar-replacement of top-level / `models` / `providers` and for empty-env-var filtering.
- **Breaking-Change Detection in Commit-Twiddle System Prompt** ([#744](https://github.com/rust-works/omni-dev/issues/744)): The AI system prompt now contains a mandatory `BREAKING CHANGE DETECTION` section that lists the diff signals which require breaking-change markers (new mandatory parameters, removed or renamed public APIs, CLI flag changes, MCP schema changes, serialisation format changes, default-behaviour changes) and instructs the model to emit **both** the `!` suffix on type/scope and a `BREAKING CHANGE:` footer with concrete migration instructions whenever any signal is detected. An "Additive-Looking-But-Breaking Trap" subsection calls out the case that originally regressed (adding a mandatory `confirm: bool` to an existing MCP params struct without `#[serde(default)]` — addition-of-required-field is breaking even though the type signature looks superset-compatible) with the `WatcherRemoveParams` confirm-field example. A `BREAKING_CHANGE_FINAL_PASS` constant appended to both the basic and contextual user-prompt variants exploits recency bias as a hedge against the system-prompt-only rule being ignored on long inputs. Covered by prompt-assembly tests asserting the new block is present, lists the expected signals, requires both markers, and is inherited into the contextual prompt.

### Changed
- **`omni-dev atlassian jira transition` CLI Restructure (Breaking)** ([#710](https://github.com/rust-works/omni-dev/issues/710)): The flat `transition <KEY> <TRANSITION>` form has been replaced with subcommands. Migrate `omni-dev atlassian jira transition PROJ-1 Done` → `omni-dev atlassian jira transition execute PROJ-1 Done`, and `omni-dev atlassian jira transition PROJ-1 --list` → `omni-dev atlassian jira transition list PROJ-1`. Mirrors the `comment list`/`comment add` shape used elsewhere in the JIRA CLI tree. The MCP `jira_transition` tool is unchanged.
- **Destructive CLI Commands: Confirmation by Default (Breaking)** ([#720](https://github.com/rust-works/omni-dev/issues/720)): `omni-dev atlassian jira watcher remove`, `omni-dev atlassian jira link remove`, and `omni-dev atlassian confluence label remove` now prompt for confirmation by default and accept `--force` to skip the prompt — bringing them into line with the existing behaviour of `jira delete` and `confluence delete`. Previously these three commands executed the API mutation immediately with no guard. Non-interactive callers (CI scripts, automation) must add `--force` to keep current behaviour, or feed `y\n` on stdin.
- **MCP Destructive Tools: Mandatory `confirm: true` (Breaking)** ([#720](https://github.com/rust-works/omni-dev/issues/720)): The `jira_watcher_remove`, `jira_link_remove`, and `confluence_label_remove` MCP tools now require a mandatory `confirm: bool` parameter and refuse with an explanatory error when called without `confirm: true`. Brings them into line with the existing `jira_delete` and `confluence_delete` tools. The shared `WatcherMutateParams` is split: the `add` tool keeps the original two-field struct, the `remove` tool gets a new `WatcherRemoveParams` with the `confirm` field. `LinkRemoveParams` and `ConfluenceLabelRemoveParams` gain `confirm` in place. There is no `dry_run` parameter on the MCP surface — assistants can preview by reading the relevant resource (`jira_read`, `confluence_read`, `jira_watcher_list`, `jira_link_list`) before mutating.
- **`sha2` 0.10 → 0.11** ([#757](https://github.com/rust-works/omni-dev/pull/757)): Dependabot bump. `sha2` 0.11 changes the digest output type from `GenericArray` (which implements `LowerHex`) to `hybrid_array::Array` (which does not), breaking the previous `format!("{:x}", Sha256::digest(...))` idiom at three call sites in `src/atlassian/adf_schema/drift.rs` and `src/bin/adf_schema_codegen.rs`. A new `pub fn hex_encode(bytes: &[u8]) -> String` helper in `drift.rs` folds bytes into a lowercase hex string and replaces all three sites. Transitive bumps: `block-buffer` 0.10→0.12, `cpufeatures` 0.2→0.3, `crypto-common` 0.1→0.2, `digest` 0.10→0.11; adds `hybrid-array` 0.4 and `const-oid` 0.10; removes `generic-array` 0.14.
- **`similar` 2.7 → 3.1** ([#758](https://github.com/rust-works/omni-dev/pull/758)): Dependabot bump. No call-site changes required.
- **`tokio` 1.52.1 → 1.52.3** ([#756](https://github.com/rust-works/omni-dev/pull/756)): Dependabot bump. No call-site changes required.
- **Subject-Length Rule Replaced With Imperative-Mood in Commit Examples**: The 72-character subject-line limit was documented in `commit-guidelines.md` and `default-commit-guidelines.md` but never enforced by the checker, while the `imperative-mood` rule was checked but absent from the human-readable docs. The mismatch is removed: the length limit is dropped from both guideline files and the imperative-mood rule replaces it in the prompt example output and test fixtures (`prompts.rs`, `client.rs`, `check.rs`).

### Fixed
- **`decisionItem` ADF Inline Content** ([#753](https://github.com/rust-works/omni-dev/issues/753)): The ADF schema requires `decisionItem` to contain inline nodes (`text`, etc.) directly, not wrapped in a `paragraph`. `parse_decision_items` was inserting a paragraph wrapper before passing inline content to `AdfNode::decision_item`, producing schema-invalid output. The wrapper is removed and a regression test asserts the first child of a constructed `decisionItem` is a `text` node, not a `paragraph`.
- **`claude-cli` `--json-schema` Inline Argument**: `claude -p --json-schema` silently returns empty output when given a filesystem path; the value must be an inline JSON string on argv. `build_command_with_schema` is changed from `Option<&Path>` to `Option<&str>` and `run_with_options` now serialises the schema with `serde_json::to_string` and passes it verbatim as the argument instead of writing a temp file and passing the path. A new `build_command_inline_schema_is_passed_verbatim_not_as_path` test asserts the argv value starts with `{` and is not an absolute path; an integration test using a new argv-capture shim asserts the inline JSON appears verbatim in the subprocess argv.
- **Multi-Scope Subject Allows Single Trailing Space After Comma**: Pre-validation previously rejected `feat(a, b): …` (one space after the comma) and only accepted `feat(a,b): …`. The check is relaxed to permit a single trailing space (`!scope.contains(",  ") && !scope.contains(" ,")`) while still rejecting two or more spaces after the comma and any whitespace before the comma. `commit-guidelines.md` now shows both `scope1,scope2` and `scope1, scope2` as accepted forms and explicitly calls out the rejected variants.
- **Breaking-Change Detection: Additive-Looking-But-Breaking Trap** ([#744](https://github.com/rust-works/omni-dev/issues/744)): Companion to the [#744](https://github.com/rust-works/omni-dev/issues/744) feature entry above — adds the `WatcherRemoveParams` example and the diff-signal expansion that catches new mandatory MCP params, new mandatory struct fields without `#[serde(default)]`, and new confirmation prompts on previously-unattended operations as breaking even when the type signature looks superset-compatible.

### Documentation
- **Comprehensive Documentation Coverage Refresh**: Audited the user-facing docs against the current CLI/MCP surface and closed the major gaps that had accumulated through the v0.23–v0.25 cycle. New `docs/mcp.md` is the canonical MCP tool/resource reference (~70 tools across git, JIRA, Confluence, Atlassian shared, Datadog, AI/config domains plus the `omni-dev://specs/{name}` resource scheme), and the README MCP section is rewritten to defer detail there. The README Datadog section drops "subsequent slices" wording in favour of present-tense subcommand examples, and a new `## Datadog Integration` section in `docs/user-guide.md` covers auth, metrics, monitors, dashboards, logs, events, SLOs, hosts, and downtimes. The user guide also gains backfills for JIRA `read --fields`/`--all-fields`, `write --parent`/`--assignee`/`--reporter`/`--no-content`/`--set-field`, `create --set-field`, `sprint create`/`update`, watcher, worklog, user search, and dev-info subcommands; Confluence comments, labels, children, user search, and bulk download (with `--space`/`--title-filter`/`--resume`/`--on-conflict`/`--concurrency`); `git branch create pr` flag table (adds `--ready`/`--draft`/`--base`/`--model`/`--context-dir`/`--no-push`); a full `git commit message check` reference; and new `ai chat` / `ai claude history sync` / `ai claude skills` / `ai claude cli model resolve` / `commands generate` sections. `docs/configuration.md` gains an "AI Backend Selection" matrix covering the five-way dispatch (`OMNI_DEV_AI_BACKEND` claude-cli → Ollama → OpenAI → Bedrock → Anthropic), model resolution precedence, and the claude-cli sandbox/escape-hatch/budget-cap envelope. `docs/troubleshooting.md` grows new sections for Atlassian auth (404, trailing slashes, MCP env inheritance), Datadog auth (region/site mismatch, 429), MCP server (`failed to open git repository`, missing `mcp` feature), and `claude-cli` backend failures (tools blocked, MCP blocked, budget cap). `ARCHITECTURE.md` module map adds `mcp/`, `atlassian/`, `datadog/`, `claude/ai/claude_cli.rs`, the `omni-dev-mcp` second binary (per ADR-0021), and an "AI backend dispatch" subsection. Drift fixes: removed the non-existent `--edit` flag from twiddle's option tables in `README.md` and `docs/user-guide.md`; bumped the Rust MSRV claim from 1.70+ to 1.80+ to match `Cargo.toml`. `docs/README.md` Key Features table adds rows for Datadog and the MCP server; `CONTRIBUTING.md` gains the `.work/` worktree convention.
- **ADF Content Model Section in JFM Specification** ([#716](https://github.com/rust-works/omni-dev/issues/716)): A new "Content Model Constraints" section in `docs/specs/jfm.md` documents the public ADF validator helpers (`allowed_children`, `permits_child`, `validate_document`, `content_model`), the common per-container pitfalls and recommended workarounds (expand↔panel, nested-expand in table cells, decision / task items, blockquote / list-item containment, layoutSection nesting), forward-compatibility behaviour on unknown and `unsupported*` node types, and the write-path enforcement path via `ValidatedAdfDocument` plus HTTP-500 re-diagnosis via `AtlassianError::ApiRequestFailedWithDiagnosis`. The "Coverage and limits" subsection lists what the validator currently covers (allowed-children sets, per-term quantifiers) and what remains out of scope (mark whitelists and attribute-value schemas — the [#733](https://github.com/rust-works/omni-dev/issues/733) follow-ups).
- **Architecture Decision Records**:
  - [ADR-0022](docs/adrs/adr-0022.md) accepted — layered model catalog with user and project overrides, supersedes [ADR-0011](docs/adrs/adr-0011.md).
  - [ADR-0023](docs/adrs/adr-0023.md) accepted — data-driven ADF content-model schema and validator transcribed from the upstream JSON schema.
  - [ADR-0025](docs/adrs/adr-0025.md) accepted — wire ADF schema validator into the API send path via `ValidatedAdfDocument` newtype enforcement.
  - [ADR-0026](docs/adrs/adr-0026.md) accepted — ADF validator extensions covering quantifier / arity checks, per-node attribute schemas, and per-context mark allow-lists.

### CI/CD
- **`cargo clippy --all-targets` in Lint Workflow** ([#690](https://github.com/rust-works/omni-dev/issues/690)): The clippy step in `.github/workflows/ci.yml` now passes `--all-targets`, surfacing lints on test / bench / example targets that were previously checked only for the default target set. The slice also includes batch lint fixes across `src/atlassian/` produced when `--all-targets` first surfaced them, plus `#[allow(clippy::unwrap_used, clippy::expect_used)]` opt-ins in test modules where assertion-style panics are idiomatic.
- **`rustfmt` Component Pinned in Rust Toolchain Setup**: The Rust toolchain setup action in CI now explicitly installs the `rustfmt` component so `cargo fmt --check` runs reliably in environments where the default profile does not include it.

## [0.25.0] - 2026-05-05

### Added
- **Set JIRA Parent Field via `jira write` and `jira_link`** ([#670](https://github.com/rust-works/omni-dev/issues/670)): Closes the gap that previously forced callers to fall back to raw `curl` for setting the JIRA `parent` field. `omni-dev jira write` accepts a new `--parent <KEY>` flag (independent of `--set-field`, which only handles custom fields); when supplied alone it performs a parent-only update without overwriting the existing description, when supplied with a body it sends both in one PUT, and the parent key is validated up-front via `validate_issue_key`. The MCP `jira_write` tool gains a matching `parent` parameter and makes `content` optional — at least one of `content`/`parent` is required. The MCP `jira_link` tool gains a `parent` action (`key` = child issue, `target` = parent issue) for callers that prefer the link-management surface; this is distinct from `create`, which produces relationship links (Blocks, Composition, etc.) rather than the system parent field. Internally, `AtlassianClient::link_to_epic(epic_key, issue_key)` was renamed to `set_issue_parent(issue_key, parent_key)` (argument order flipped to match other update helpers); the CLI `jira link epic` subcommand is unchanged for back-compat. `update_issue_with_custom_fields` now accepts `description_adf: Option<&AdfDocument>` and a new `parent: Option<&str>` so a parent-only PUT does not overwrite the description, with a guard rejecting empty payloads. Implemented across `src/atlassian/client.rs`, `src/cli/atlassian/helpers.rs`, `src/cli/atlassian/jira/{write.rs,link.rs,mod.rs}`, and `src/mcp/jira_core_tools.rs`. Covered by new wiremock-backed tests for `set_issue_parent`, parent-only updates, content+parent updates, the `--parent` CLI flag (dry-run, parent-only, body+parent, invalid-key rejection), the `jira_link` `parent` action (success, missing-key, missing-target, API error), and the `jira_write` MCP tool's content/parent variants.
- **`jira_write` Field Updates** ([#669](https://github.com/rust-works/omni-dev/issues/669)): The `jira_write` MCP tool and `omni-dev atlassian jira write` CLI now accept typed `assignee` and `reporter` parameters (taking an Atlassian `accountId`) plus an escape-hatch `fields` map for arbitrary canonical JIRA fields. The empty string `""` clears assignee/reporter (Atlassian's `null` payload); `"-1"` triggers automatic assignment. The description body is now optional — `content`/`--no-content` lets a caller flip an assignee or set a priority without re-posting the description, removing the previous `jira_read` → `jira_write` round-trip. A typed `assignee`/`reporter` parameter that collides with the same key inside `fields` (or `--set-field`) is rejected with a hard error rather than silently overriding, so the caller's intent is unambiguous. Implemented in `src/mcp/jira_core_tools.rs`, `src/cli/atlassian/jira/write.rs`, and a shared `user_field_value` / `apply_user_field_overrides` helper in `src/atlassian/custom_fields.rs` (used by both MCP and CLI paths).
- **`jira_user_search` MCP Tool & CLI** ([#669](https://github.com/rust-works/omni-dev/issues/669)): New `jira_user_search` MCP tool and `omni-dev atlassian jira user search` CLI subcommand resolve a display name or email substring to an Atlassian `accountId` so callers can populate `jira_write`'s new `assignee`/`reporter` parameters. Returns YAML/table/json/yaml/jsonl output with `account_id`, `display_name`, `email_address` (often GDPR-redacted), `active`, and `account_type` for each match. The MCP tool description explicitly tells the model to call this tool first when only a name or email is available, keeping disambiguation visible to the agent rather than hidden inside `jira_write`. Backed by a new `AtlassianClient::search_jira_users` method that paginates `GET /rest/api/3/user/search` until a short page is returned (the endpoint has no `isLast`/`next` envelope).
- **`omni-dev://specs/{name}` MCP Resource** ([#672](https://github.com/rust-works/omni-dev/issues/672)): New MCP resource URI scheme serves reference specifications embedded into the binary at compile time. The initial spec (`omni-dev://specs/jfm`) is the JIRA-Flavoured Markdown reference, embedded from `docs/specs/jfm.md` via `include_str!` so installed builds can serve it without reading from disk and the resource cannot drift from the human-readable docs. The resource template is advertised through the server's `resources` capability with a description guiding AI clients to fetch it before writing JIRA or Confluence content; server instructions and the `jira_write` tool description point at the same resource. `content` field doc comments on `ConfluenceCreateParams`, `ConfluenceWriteParams`, `JiraCreateParams`, `JiraWriteParams`, and `JiraCommentParams` reference the spec resource so the model surfaces it during schema introspection. New `ResourceUri::Specs` variant adds `EmptyIdentifier` and `Malformed` parsing errors. Implemented in `src/mcp/specs.rs`, `src/mcp/resources.rs`, and `src/mcp/server.rs`; covered by new unit tests for spec lookup, URI parsing, and `read_resource` dispatch, plus integration tests for listing and reading the resource.

### Changed
- **`jira_transition` MCP Tool Discoverability** ([#671](https://github.com/rust-works/omni-dev/issues/671)): The `jira_transition` tool description now leads with the most common usage (executing by name, e.g. `transition: "In Progress"`), shows the numeric-id form (`transition: "31"`), and explicitly tells the assistant to call with `list = true` first when the available transitions are not yet known. The `transition` field doc carries the same examples into the JSON schema. When the underlying `POST /rest/api/3/issue/{key}/transitions` call fails, the error chain now includes a hint that the workflow may require additional fields (assignee, resolution, screen-driven field) or that the transition may not be valid from the current status, suggesting `list = true` to confirm — the original `AtlassianError::ApiRequestFailed { status, body }` stays in the chain so the HTTP status and response body remain visible. No API surface change.
- **rmcp 1.5.0 → 1.6.0**: Bumped the `rmcp` dependency (and `rmcp-macros`) to the upstream 1.6.0 release.

### Documentation
- **JFM Markdown Description in Confluence Tool Annotations**: Clarified the JFM markdown wording in the `confluence_*` MCP tool descriptions so the model surfaces the correct format expectations.
- **ADR-0021 Accepted**: Marked the "MCP server via second binary" architectural decision as accepted.

### CI/CD
- **Stop Hook Enforces Snapshot Updates** ([#667](https://github.com/rust-works/omni-dev/issues/667)): Added a Claude Code stop hook (`.claude/hooks/check-snapshots.sh`) that blocks session completion when files under `src/cli/` or `src/main.rs` have changed but `tests/snapshots/` has not been updated, directing the AI to run the `update-snapshots` skill before stopping. Guards against infinite loops via the `stop_hook_active` flag, registered with a 30-second timeout in `.claude/settings.json`. Permissions also broadened to allow all `omni_dev` debug/release binaries instead of a single hardcoded hash-suffixed path.

## [0.24.0] - 2026-05-03

### Added
- **Claude History Sync — Markdown Output** ([#665](https://github.com/rust-works/omni-dev/issues/665)): `omni-dev ai claude history sync` accepts `--output-format <jsonl|markdown>` (comma-separated, e.g. `--output-format jsonl,markdown` writes both side-by-side). Markdown output produces an LLM-friendly `<target>/<slug>/<uuid>.md` alongside (or instead of) the lossless `.jsonl`, with YAML frontmatter (session metadata) followed by `## User` / `## Assistant` turns, `### Tool call: <name>` blocks for tool invocations, `<details>` blocks for thinking, and 4-backtick fences when tool output contains embedded triple-backtick blocks. Agent-to-user interactions are surfaced as first-class structured events: `AskUserQuestion` calls render as `### Agent question: <header>` with the question text and option list (with descriptions and a `(multi-select)` marker where applicable), and the paired answer renders as `## User response`; tool denials are detected via the "The user doesn't want to proceed" sentinel and labelled `denied by user` rather than a generic `error`; user interrupts (escape mid-execution) are labelled `interrupted by user`. This means the rendered transcript distinguishes "the agent asked, the user picked B" and "the agent tried X, the user denied" from plain tool runs — the signal a coaching LLM needs. Sub-agent (`Agent`) calls render only the `prompt` argument; sub-agent internal turns are not captured (matches the v1 sync invariant from #658). The existing `<persisted-output>` "(persisted, NN KB)" envelope from large tool outputs is preserved verbatim. System-side events (system reminders inside user text, attachments, permission-mode changes, summary events, generic system events) are **included by default** — pass `--exclude-system` to drop them; `--exclude-system` only affects markdown output, the jsonl is byte-identical regardless. Markdown idempotency uses source mtime alone (the rendered length differs from the source length, so size cannot participate in the freshness key); the source jsonl is append-only, making mtime alone sufficient. `--prune` scopes deletion to whichever extensions are listed in `--output-format`, so artifacts the run was not asked to manage survive. Implemented in `src/cli/ai/claude/history/markdown.rs` (renderer) with new clap surface in `src/cli/ai/claude/history/sync.rs` and a `FileFormat` enum in `src/cli/ai/claude/history/common.rs`. Covered by 130+ unit tests in the history module including `insta` snapshots pinning the rendered shape of one of each event type both with and without `--exclude-system`, plus end-to-end tests for both-formats-side-by-side, prune scoping per format, partial-jsonl-prefix tolerance, and markdown idempotency on re-run.
- **Claude History Sync** ([#658](https://github.com/rust-works/omni-dev/issues/658)): New `omni-dev ai claude history sync --target <DIR>` exports Claude Code conversation history to a target directory as one `.jsonl` per chat, grouped by encoded project slug (`<target>/<slug>/<uuid>.jsonl`). Re-running is idempotent — sessions whose source `(size, mtime)` match the target are skipped, modified sessions are overwritten via a sibling tempfile + rename, and source `mtime` is preserved on the target so downstream tooling can sort sessions chronologically. In-progress chats produce a valid jsonl prefix via a snapshot-EOF read (the source size is captured up front and exactly that many bytes are copied — never more). Flags: `--source` (defaults to `~/.claude/projects`), `--project NAME_OR_PATH` (matches encoded slug or decoded cwd path), `--since DURATION_OR_DATE` (`30s`, `5m`, `2h`, `7d`, `4w`, or RFC 3339), `--prune` (deletes target files for sessions removed upstream — only files matching `<slug>/<uuid>.jsonl` are eligible; anything else inside the target survives regardless), `--dry-run`, and `--format text|yaml`. Refuses to run when the target is the source root or a descendant. The export is a **behavioural transcript** — prompts, responses, thinking, tool calls, and tool-result metadata sufficient for analyst use cases (behavioural coaching, work-log generation). Sub-agent internal turns, tool-result `*.txt` sidecars, PDF rasters, and auto-memory are deliberately excluded; see the issue for the rationale and planned follow-ups (memory sync, full-fidelity archive mode, restore, redact, MCP wrapper). Implemented in `src/cli/ai/claude/history/{mod.rs, common.rs, sync.rs}` and wired through `src/cli/ai/claude/mod.rs`. Covered by 48 unit tests (line coverage 95–100% across the new module).
- **`--claude-cli-allow-mcp` Escape Hatch** ([#634](https://github.com/rust-works/omni-dev/issues/634)): New global flag and `OMNI_DEV_CLAUDE_CLI_ALLOW_MCP` environment variable opt the `claude-cli` backend out of `--strict-mcp-config`, letting the nested `claude -p` session load MCP servers from `~/.claude/settings.json`. Independent of `--claude-cli-allow-tools` so MCP access can be granted without enabling built-in tools, and vice-versa. A WARN log fires on every invocation when the flag is active, mirroring the existing tool escape hatch. Wired through `src/cli.rs` (flag → env propagation) and `src/claude/ai/claude_cli.rs` (`ClaudeCliAiClient::with_allow_mcp` builder, `allow_mcp_from_env`, conditional argv assembly). Honours the README forward reference promised by the [#608](https://github.com/rust-works/omni-dev/issues/608) lockdown work.
- **MCP `output_file` for Read Tools** ([#631](https://github.com/rust-works/omni-dev/issues/631)): `confluence_read` and `jira_read` accept an optional `output_file` parameter. When set, the rendered content is written to that path and the tool returns a short YAML summary (`path`, `bytes`, `format`) instead of the inline body. This avoids blowing past the assistant's context window on large pages — the assistant can then page through the file with offset/limit using its filesystem read tool. Mirrors the on-disk pattern already established by `confluence_download` and `jira_attachment_download`. Implemented in `src/mcp/output_file.rs` and wired through `confluence_tools::run_confluence_read` and `jira_core_tools::run_jira_read`.
- **Datadog Phase 2 Endpoints** ([#639](https://github.com/rust-works/omni-dev/issues/639)): New `omni-dev datadog events`, `slo`, `hosts`, `downtime`, and `metrics catalog` subcommands wrap the read-only Phase 2 Datadog endpoints called out in the original [#619](https://github.com/rust-works/omni-dev/issues/619) design. `events list` calls `GET /api/v2/events` (single-page; cursor pagination preserved on `meta.page.after` for callers that need to iterate manually); `slo list` and `slo get <id>` call `GET /api/v1/slo` (auto-paginating, capped at 10000 SLOs) and `GET /api/v1/slo/{id}`; `hosts list` calls `GET /api/v1/hosts` with `start`/`count` pagination; `downtime list [--active-only]` calls `GET /api/v1/downtime` (the API returns the full set in one response — no pagination); `metrics catalog list` calls `GET /api/v1/metrics` (distinct from the Phase 1 metrics *query* endpoint at `/api/v1/query`). Built on five new façades — `EventsApi`, `SloApi`, `HostsApi`, `DowntimesApi`, `MetricsCatalogApi` — under `src/datadog/` plus matching wire types (`Event`, `EventsResponse`, `Slo`, `SloListResponse`, `SloGetResponse`, `Host`, `HostsResponse`, `Downtime`, `MetricCatalogResponse`) in `src/datadog/types.rs`. Output supports `-o json|yaml|yamls|jsonl|table`; bespoke tables render `TIMESTAMP | TITLE | SOURCE | HOST | TAGS` (events), `ID | NAME | TYPE | TAGS` (SLOs), `NAME | UP | LAST REPORTED | APPS` (hosts), `ID | SCOPE | START | END | MONITOR | MESSAGE` (downtimes), and `METRIC` (metrics catalog). Six new MCP tools (`datadog_events_list`, `datadog_slo_list`, `datadog_slo_get`, `datadog_hosts_list`, `datadog_downtime_list`, `datadog_metrics_catalog_list`) mirror the CLI surface so AI assistants can issue the same queries over the MCP transport. No new crate dependencies. Phase 2 follow-ons (logs cursor pagination, 429 retry tuning) remain deferred.
- **Datadog MCP Tools** ([#640](https://github.com/rust-works/omni-dev/issues/640)): The Phase 1 Datadog CLI surface is now also exposed over MCP so AI assistants (Claude Desktop, Claude Code) can invoke Datadog read-only queries over stdio. New tools registered on `omni-dev-mcp`: `datadog_auth_status` (boolean credential-presence flags only — never emits `DATADOG_API_KEY` or `DATADOG_APP_KEY` values), `datadog_metrics_query`, `datadog_monitor_list`, `datadog_monitor_get`, `datadog_monitor_search`, `datadog_dashboard_list`, `datadog_dashboard_get`, and `datadog_logs_search`. Each tool builds a fresh `DatadogClient` per invocation via the existing `cli::datadog::helpers::create_client` (per-call construction mirrors the Atlassian tools and lets credential changes take effect without restarting the MCP server) and serialises the typed response struct as YAML, matching the CLI `-o yaml` output. Implemented in `src/mcp/datadog_tools.rs` and registered through `Self::datadog_tool_router()` in `OmniDevServer::new()`. Gated behind the existing `mcp` Cargo feature; non-MCP builds are unaffected. No new crate dependencies.
- **Datadog Logs Search** ([#638](https://github.com/rust-works/omni-dev/issues/638)): New `omni-dev datadog logs search` subcommand wraps Datadog's read-only logs search endpoint via `POST /api/v2/logs/events/search` (the only Phase 1 endpoint that uses POST). Accepts `--filter <q>`, `--from` (relative shorthand like `15m`/`1h`, `now`, RFC 3339, or Unix epoch seconds — converted to RFC 3339 before sending), `--to` (default `now`), `--limit` (per-page; default 100, max 1000 — rejected client-side above the cap), and `--sort timestamp-asc|timestamp-desc` (serialised as `timestamp` / `-timestamp` on the wire). Built on a new `LogsApi` façade in `src/datadog/logs_api.rs` plus `LogEvent`, `LogEventAttributes`, `LogSearchResult`, `LogSearchMeta`, `LogSearchPage`, and `SortOrder` types in `src/datadog/types.rs`. Datadog v2 logs search uses cursor pagination (`meta.page.after`), not offset; Phase 1 ships single-page only and the cursor token is preserved on the response so a Phase 2 follow-up can iterate without changing the wire types. Output supports `-o json|yaml|yamls|jsonl|table`; the bespoke table renders `TIMESTAMP | SERVICE | STATUS | MESSAGE`. No new crate dependencies. Closes [#619](https://github.com/rust-works/omni-dev/issues/619).
- **Datadog Dashboard List/Get** ([#637](https://github.com/rust-works/omni-dev/issues/637)): New `omni-dev datadog dashboard` subcommands wrap Datadog's read-only dashboard endpoints. `dashboard list [--filter-shared]` calls `GET /api/v1/dashboard` (which returns every dashboard in a single response — no server-side pagination); `dashboard get <id>` calls `GET /api/v1/dashboard/{id}` and renders the full definition. Built on a new `DashboardsApi` façade in `src/datadog/dashboards_api.rs` plus `DashboardSummary`, `DashboardListResponse`, and `Dashboard` types in `src/datadog/types.rs`. Per-widget schemas are deeply heterogeneous, so `Dashboard.widgets` is preserved as raw `serde_json::Value` (mirroring how ADF is handled in the Atlassian integration). Output supports `-o json|yaml|yamls|jsonl|table`; the bespoke table renders `ID | TITLE | AUTHOR | URL`. No new crate dependencies.
- **Datadog Monitor List/Get/Search** ([#636](https://github.com/rust-works/omni-dev/issues/636)): New `omni-dev datadog monitor` subcommands wrap Datadog's read-only monitor endpoints. `monitor list [--name <substr>] [--tags k:v,...] [--monitor-tags k:v,...] [--limit N]` filters via `GET /api/v1/monitor`; `monitor get <id>` fetches a single monitor; `monitor search --query <str> [--limit N]` runs the faceted `GET /api/v1/monitor/search`. `--limit 0` auto-paginates and is hard-capped at 10,000 monitors per invocation (per the decisions in #619). Built on a new `MonitorsApi` façade in `src/datadog/monitors_api.rs` plus `Monitor`, `MonitorSearchResult`, `MonitorSearchItem`, and `MonitorSearchMetadata` types in `src/datadog/types.rs`. Output supports `-o json|yaml|yamls|jsonl|table`; the bespoke table renders `ID | NAME | STATUS | TAGS`. No new crate dependencies.
- **Datadog Metrics Query** ([#635](https://github.com/rust-works/omni-dev/issues/635)): New `omni-dev datadog metrics query` subcommand runs point-in-time Datadog timeseries queries via `GET /api/v1/query`. Accepts `--query`, `--from`, and optional `--to` (relative shorthand like `15m`/`1h`, the literal `now`, RFC 3339 timestamps, or Unix epoch seconds via the shared `src/datadog/time.rs` parser), plus `-o json|yaml|yamls|jsonl|table`. Table output is bespoke: a `TIMESTAMP` column followed by one column per series, with the union of per-series timestamps sorted ascending and `-` rendered for gaps and missing samples. Built on a new `MetricsApi` façade in `src/datadog/metrics_api.rs` that percent-encodes the query string via the existing `url` crate. No new crate dependencies.
- **Datadog Integration — Scaffold & Auth** ([#619](https://github.com/rust-works/omni-dev/issues/619)): New `omni-dev datadog` command tree exposing read-only Datadog API access. This first slice ships the authentication surface: `datadog auth login` (interactive credential prompt), `datadog auth logout` (removes credentials from `~/.omni-dev/settings.json`), and `datadog auth status` (verifies credentials via `/api/v1/validate`). Credentials live in the shared `env` map under `DATADOG_API_KEY`, `DATADOG_APP_KEY`, and `DATADOG_SITE`; environment variables override stored settings. The underlying `DatadogClient` in `src/datadog/client.rs` handles 429 responses with `Retry-After` and Datadog-specific `X-RateLimit-Reset` awareness, and surfaces `X-RateLimit-*` headers in error output when retries are exhausted. No new crate dependencies.

## [0.23.1] - 2026-04-23

### Fixed
- **AI Issue Output Field Order** ([#627](https://github.com/rust-works/omni-dev/issues/627)): Require the `reasoning` field to appear before `severity` in AI-generated issue output. The prompt now instructs the model to write `reasoning` before `severity`, and the deserializer rejects payloads whose field order is reversed so that callers see a clear parse error rather than silently accepting malformed output
- **Multi-Scope Whitespace Trimming** ([#626](https://github.com/rust-works/omni-dev/issues/626)): `feat(a, b): …` style multi-scope subjects are now valid. Scope validation trims whitespace around each comma-separated part before checking allowed scopes, so spaces after commas no longer cause spurious validation failures

### Security
- **RUSTSEC-2026-0104** ([#623](https://github.com/rust-works/omni-dev/pull/623)): Bump `rustls-webpki` to 0.103.13 to address RUSTSEC-2026-0104

### Documentation
- **Worktree Convention** ([#622](https://github.com/rust-works/omni-dev/pull/622)): Document the project convention that new git worktrees live under `.work/` (gitignored) so worktrees stay scoped to the project rather than scattered across sibling directories

## [0.23.0] - 2026-04-22

### Added
- **MCP Server (Foundation)** ([#575](https://github.com/rust-works/omni-dev/issues/575)): New `omni-dev-mcp` binary exposes omni-dev's git analysis as MCP tools for AI assistants (Claude Desktop, Claude Code). Gated behind the `mcp` Cargo feature so default builds are unaffected. Initial tool: `git_view_commits`. See [ADR-0021](docs/adrs/adr-0021.md) for the architectural rationale.
- **MCP Resource System** ([#606](https://github.com/rust-works/omni-dev/issues/606)): Content is now addressable over MCP via three URI schemes — `git://repo/commits/{range}`, `jira://issue/{key}[.adf]`, and `confluence://page/{id}[.adf]` — so AI clients can fetch commits, JIRA issues, and Confluence pages as MCP resources without issuing tool calls. Resource templates, listings, and reads are advertised through the server's `resources` capability. JIRA and Confluence resources return JFM markdown by default or raw ADF JSON when the URI ends in `.adf`. See `STYLE-0026` in [docs/STYLE_GUIDE.md](docs/STYLE_GUIDE.md) for MCP tool/resource authoring conventions.
- **JIRA Custom Field Read** ([#594](https://github.com/rust-works/omni-dev/issues/594)): `jira read` accepts `--fields <name>` (repeatable) and `--all-fields` to request custom fields alongside the default set. Scalar custom fields are rendered into the frontmatter `custom_fields:` map; ADF rich-text custom fields are rendered as tagged body sections (`<!-- field: Name (id) -->`) in the JFM document
- **JIRA Custom Field Write** ([#594](https://github.com/rust-works/omni-dev/issues/594)): `jira write` and `jira create` now support writing custom fields. Frontmatter `custom_fields:` entries and body sections delimited by `<!-- field: Name (id) -->` are resolved through `/editmeta` (write) and `/createmeta` (create), dispatching by schema type (option, radio, array, textfield, number, date, and rich-text ADF). A new `--set-field NAME=VALUE` CLI flag allows inline overrides; CLI values take precedence over frontmatter scalars for the same name. Rejected when combined with `--format adf`
- **MCP Tool: `git_branch_info`** ([#600](https://github.com/rust-works/omni-dev/issues/600)): Mirrors `omni-dev git branch info`. Returns repository information as YAML for commits between a base branch and `HEAD`.
- **MCP Tool: `git_check_commits`** ([#600](https://github.com/rust-works/omni-dev/issues/600)): Mirrors `omni-dev git commit message check`. Returns the full check report plus pass/fail summary and exit code (honouring `strict`) as a structured payload so the assistant can act on validation failures without parsing exit codes.
- **MCP Tool: `git_twiddle_commits`** ([#600](https://github.com/rust-works/omni-dev/issues/600)): Mirrors `omni-dev git commit message twiddle --auto-apply`. Forces non-interactive semantics — never starts an editor. Accepts a `dry_run` flag that returns the proposed amendments as YAML without applying them.
- **MCP Tool: `git_create_pr`** ([#600](https://github.com/rust-works/omni-dev/issues/600)): Mirrors the content-generation phase of `omni-dev git branch create pr`. Returns the AI-drafted PR title and description as YAML; does not push the branch or invoke `gh pr create`.
- **MCP JIRA Sprint Tools** ([#602](https://github.com/rust-works/omni-dev/issues/602)): `jira_sprint_list`, `jira_sprint_issues`, `jira_sprint_add`, `jira_sprint_create`, and `jira_sprint_update` — list, inspect, populate, create, and transition agile sprints from MCP-aware AI assistants
- **MCP JIRA Watcher Tools** ([#602](https://github.com/rust-works/omni-dev/issues/602)): `jira_watcher_list`, `jira_watcher_add`, `jira_watcher_remove` — manage watchers on a JIRA issue by Atlassian account ID
- **MCP JIRA Worklog Tools** ([#602](https://github.com/rust-works/omni-dev/issues/602)): `jira_worklog_list` and `jira_worklog_add` — list worklog entries and log time on an issue (accepts JIRA's `1h 30m` duration format)
- **MCP JIRA Field Tools** ([#602](https://github.com/rust-works/omni-dev/issues/602)): `jira_field_list` (with optional substring filter) and `jira_field_options` (auto-discovers context when not specified) — discover custom fields and their permitted option values
- **MCP JIRA Board Tools** ([#602](https://github.com/rust-works/omni-dev/issues/602)): `jira_board_list` and `jira_board_issues` — enumerate agile boards and their issues, with optional project/type/JQL filters
- **MCP JIRA Attachment Tools** ([#602](https://github.com/rust-works/omni-dev/issues/602)): `jira_attachment_download` (filename substring filter) and `jira_attachment_images` (PNG/JPEG/GIF/SVG/WebP only) — download attachments to disk and return YAML metadata including the on-disk path; defaults to a fresh temp dir so the assistant can read files via its filesystem tool
- **MCP JIRA Project & Changelog Tools** ([#602](https://github.com/rust-works/omni-dev/issues/602)): `jira_project_list` and `jira_changelog` — enumerate projects and inspect issue change history
- **MCP JIRA Delete Tool (Destructive)** ([#602](https://github.com/rust-works/omni-dev/issues/602)): `jira_delete` deletes an issue irreversibly. The tool requires the assistant to explicitly pass `confirm: true`; without it, the call is rejected before any API contact. The required-confirmation guard was preferred over a Cargo feature flag to keep the MCP build surface uniform while still preventing accidental deletions through MCP.
- **MCP JIRA Tools — Read/Search** ([#601](https://github.com/rust-works/omni-dev/issues/601)): `jira_read` fetches an issue as JFM markdown (default) or raw ADF JSON; `jira_search` runs a JQL query and returns matching issues as YAML.
- **MCP JIRA Tools — Create/Write** ([#601](https://github.com/rust-works/omni-dev/issues/601)): `jira_create` creates a new JIRA issue from `project`/`summary`/`description`/`issue_type`; `jira_write` updates an existing issue's description from JFM markdown or raw ADF.
- **MCP JIRA Tools — Transition/Comment** ([#601](https://github.com/rust-works/omni-dev/issues/601)): `jira_transition` lists workflow transitions (or executes one by name/id with an optional comment); `jira_comment` lists or posts comments on a JIRA issue.
- **MCP JIRA Tools — Link/Dev** ([#601](https://github.com/rust-works/omni-dev/issues/601)): `jira_link` lists issue links, lists available link types, creates, or removes links; `jira_dev` returns linked pull requests, branches, and repositories as YAML.
- **MCP Confluence Tools** ([#603](https://github.com/rust-works/omni-dev/issues/603)): Six Confluence tools wired through the MCP server — `confluence_read`, `confluence_search`, `confluence_create`, `confluence_write`, `confluence_delete`, and `confluence_download`. Each mirrors its CLI counterpart and reuses the same `AtlassianClient` / `ConfluenceApi` implementation. `confluence_delete` requires an explicit `confirm: true` parameter to prevent accidental deletion; `confluence_download` writes page trees to a caller-provided directory (or a tempdir) and returns a YAML manifest summary.
- **MCP `atlassian_convert` Tool** ([#603](https://github.com/rust-works/omni-dev/issues/603)): Stateless JFM ↔ ADF conversion tool exposed over MCP. Takes `content` and `direction` (`"to-adf"` or `"from-adf"`) plus optional `compact` / `strip_local_ids` flags. No API client required.
- **MCP Confluence Children Tool** ([#604](https://github.com/rust-works/omni-dev/issues/604)): `confluence_children` MCP tool lists child pages of a Confluence page or top-level pages in a space, with optional `recursive` and `max_depth` parameters mirroring the `atlassian confluence children` CLI command
- **MCP Confluence Comment Tools** ([#604](https://github.com/rust-works/omni-dev/issues/604)): `confluence_comment_list` (auto-paginated, with a `limit` parameter where `0` means unlimited) and `confluence_comment_add` (markdown body, converted to ADF before posting) MCP tools mirror the `atlassian confluence comment` CLI subcommands
- **MCP Confluence Label Tools** ([#604](https://github.com/rust-works/omni-dev/issues/604)): `confluence_label_list`, `confluence_label_add`, and `confluence_label_remove` MCP tools mirror the `atlassian confluence label` CLI subcommands
- **MCP Confluence User Search Tool** ([#604](https://github.com/rust-works/omni-dev/issues/604)): `confluence_user_search` MCP tool searches Confluence users by display name or email, with a `limit` parameter (`0` = unlimited, default 25)
- **MCP AI Tools** ([#605](https://github.com/rust-works/omni-dev/issues/605)): Adds `ai_chat` (single-turn chat with the configured AI model) and `claude_skills_sync` / `claude_skills_clean` / `claude_skills_status` (worktree-aware skill distribution) as MCP tools. `ai_chat` is single-response (no streaming); skills tools accept a `worktrees` flag and a `format` selector (`"text"` or `"yaml"`). The skill-mutation tools operate relative to the MCP server process's current working directory.
- **MCP Config & Auth Tools** ([#605](https://github.com/rust-works/omni-dev/issues/605)): Adds `config_models_show` (returns the embedded `models.yaml` listing every AI model the CLI knows about) and `atlassian_auth_status` (reports Atlassian credential presence per scope as boolean flags, never leaking secret values). A new `atlassian::auth::status()` helper provides the read-only check.
- **MCP Server Hardening** ([#607](https://github.com/rust-works/omni-dev/issues/607)): Production hardening for the MCP server — input validators at the tool boundary (`range`, `repo_path`, JIRA key, Confluence id) return clear `invalid_params` errors on malformed input; a 100 KB default response cap with a `truncated`/`original_bytes`/`limit_bytes` marker for oversized payloads; cancellation support via the rmcp `CancellationToken` that returns early from blocking git work when a client cancels; per-tool `debug!` logs on invocation and a startup `info!` event listing compiled-in feature flags; an integration test that drives the binary and asserts every stdout frame is a valid JSON-RPC message. CI now runs `cargo build`/`test`/`clippy` with and without `--features mcp`.
- **Claude Skills Status** ([#558](https://github.com/rust-works/omni-dev/issues/558)): New `ai claude skills status` subcommand reports the symlinks and managed exclude-block entries left behind by prior `sync` runs, with `--worktrees` to aggregate across every worktree and `--format {text,yaml}` for machine-readable output
- **Claude Skills YAML Output** ([#558](https://github.com/rust-works/omni-dev/issues/558)): `ai claude skills sync` and `ai claude skills clean` now accept `--format {text,yaml}` (default `text`), producing a structured YAML report when invoked with `--format yaml`

### Changed
- **Claude Skills Exclude File Format** ([#558](https://github.com/rust-works/omni-dev/issues/558)): Entries in `.git/info/exclude` written by `sync` are now wrapped in a managed `# BEGIN omni-dev-skills (managed — do not edit)` / `# END omni-dev-skills` block so that `clean` can reverse the operation precisely. Clean break: bare `.claude/skills/*/` lines written by earlier builds are not recognised by the new `clean` and must be removed manually

## [0.22.0] - 2026-04-19

### Added
- **Confluence Bulk Space Download** ([#556](https://github.com/rust-works/omni-dev/issues/556)): `confluence download` now accepts `--space <KEY>` to recursively download every page in a Confluence space, plus `--title-filter` to download only pages whose title contains a substring (case-insensitive)
- **Confluence Children Command** ([#557](https://github.com/rust-works/omni-dev/issues/557)): New `confluence children` command for traversing page hierarchies, with `--space` and `--recursive` options for whole-space and recursive subtree listing
- **JIRA Comment Pagination** ([#543](https://github.com/rust-works/omni-dev/issues/543)): `jira comment list` now auto-paginates the JIRA comments REST API (previously only the first page was returned) and accepts a `--limit` flag to cap the total number of comments (use `0` for unlimited)
- **Confluence Comment Pagination** ([#544](https://github.com/rust-works/omni-dev/issues/544)): `confluence comment list` now auto-paginates the underlying API, returning every comment rather than just the first page
- **YAML Stream Output** ([#546](https://github.com/rust-works/omni-dev/issues/546)): New `-o yamls` output format emits results as `---`-separated YAML documents (YAML multi-document streams). For sequence results, each item becomes its own document; other values emit as a single `---`-prefixed document. Streams parse with standard YAML tooling (`yaml.safe_load_all()`, `yq`) and enable future per-page streaming for auto-paginated commands.
- **JSONL Output Format** ([#545](https://github.com/rust-works/omni-dev/issues/545)): New `-o jsonl` output format emits results as newline-delimited JSON (one JSON value per line), suitable for streaming consumption by `jq`, `fx`, and other line-oriented JSON tools
- **Claude Skills Sync/Clean** ([#558](https://github.com/rust-works/omni-dev/issues/558)): New `claude skills sync` and `claude skills clean` subcommands for worktree-aware skill distribution, allowing skills to be propagated consistently across multiple git worktrees of the same project

### Changed
- **CI Coverage**: Switched coverage tooling from `cargo-tarpaulin` to `cargo-llvm-cov` ([#589](https://github.com/rust-works/omni-dev/issues/589)) to eliminate `.await?` false negatives and improve accuracy for async code, macros, and generics
- **Atlassian Command Refactor** ([#281](https://github.com/rust-works/omni-dev/issues/281)): Extract `run_*` handler functions from the `execute` methods across Atlassian CLI commands, consolidating parameter structs (`DownloadParams`) and expanding wiremock coverage for every handler

### Fixed
- **Null ADF Input** ([#591](https://github.com/rust-works/omni-dev/issues/591)): `convert from-adf` no longer errors on `null` JSON input (empty Jira descriptions); null is now interpreted as an empty ADF document
- **CommonMark Code Span Handling** ([#578](https://github.com/rust-works/omni-dev/issues/578)): `convert to-adf` no longer splits code-marked text containing backtick characters. Code-span rendering now picks the minimum backtick delimiter length that avoids collisions and applies the CommonMark space-padding rule; the parser matches multi-backtick delimiters with exact-length matching and strips padding spaces symmetrically
- **Integer vs Float Table Width** ([#577](https://github.com/rust-works/omni-dev/issues/577)): ADF→JFM→ADF round-trips no longer coerce integer table width values to floats. A shared `parse_numeric_attr`/`fmt_numeric_attr` helper pair preserves integer-vs-float JSON type based on whether the source had a decimal point
- **Integer/Float in Numeric Attributes** ([#555](https://github.com/rust-works/omni-dev/issues/555)): `layoutColumn`, `mediaSingle`, and file-attachment `media` width/height/`mediaWidth` attributes now preserve the original integer-vs-float JSON number type on round-trip, preventing byte-level JSON inequalities and potential schema validation failures
- **Combined Emoji ShortNames** ([#576](https://github.com/rust-works/omni-dev/issues/576)): Emoji nodes with combined shortNames like `:slightly_smiling_face::bow:` are no longer split into multiple nodes during ADF→JFM→ADF round-trips. The inline parser now extends the match through adjacent `:name:` shortcodes before checking for a trailing attribute directive
- **Pipe Characters in Table Cells** ([#579](https://github.com/rust-works/omni-dev/issues/579)): Literal `|` characters inside pipe-table cell content (including inside inline code spans) are now escaped as `\|` on render and parsed as literal pipes on input, preventing table cells from being split at non-separator pipes
- **Code-Block & Unicode Emoji Shortcodes** ([#552](https://github.com/rust-works/omni-dev/issues/552)): Two ADF↔Markdown round-trip bugs fixed: hardBreak continuation no longer consumes 2-space-indented fenced code blocks or `:::` container directives as continuation lines, and emoji shortcode escaping now uses Unicode `is_alphanumeric` (matching the parser) rather than ASCII-only, so patterns like `:Café:` and `:配置:` are escaped and survive round-trips intact
- **Card URL Brackets** ([#553](https://github.com/rust-works/omni-dev/issues/553)): `::card[...]` / `:card[...]` directives now round-trip correctly even when the URL contains unbalanced `]` or newline characters. URLs that cannot be safely embedded in bracket content are emitted using the quoted `url` attribute form. Additionally, bare URLs inside emphasis, strikethrough, link labels, bracketed spans, or span directives are no longer incorrectly promoted to `inlineCard` nodes (which was silently dropping the enclosing marks)
- **Spurious order=1 on orderedList** ([#547](https://github.com/rust-works/omni-dev/issues/547)): Converting an ADF `orderedList` with no attrs through JFM and back no longer adds a spurious `{"order": 1}` attrs field to the node
- **ADF Mark Ordering** ([#549](https://github.com/rust-works/omni-dev/issues/549)): Inline mark ordering is now preserved across ADF→JFM→ADF round-trips. Markdown wrappers are emitted in mark-array order (outermost first) rather than a fixed priority, so every permutation of `em`, `strong`, `strike`, `underline`, `link`, `annotation`, `textColor`, `backgroundColor`, and `subsup` survives with its original ordering intact
- **Media `occurrenceKey` & Attribute Quoting** ([#550](https://github.com/rust-works/omni-dev/issues/550)): The `occurrenceKey` attribute on `mediaSingle` and `mediaInline` nodes is now preserved on round-trip. Attribute values containing spaces, quotes, closing braces, or backslashes are now quoted and escaped correctly, so arbitrary `id`, `collection`, `url`, `alt`, `localId`, and `occurrenceKey` values round-trip losslessly
- **Annotation-Link Round-Trip Test URL** ([#574](https://github.com/rust-works/omni-dev/pull/574)): Correct the URL used in the annotation-link round-trip test (typo fix)
- **Confluence User Search** ([#542](https://github.com/rust-works/omni-dev/issues/542)): `confluence user search` no longer fails with `missing field 'accountId'` deserialization errors. The response parser now reads the nested `user` object returned by `/wiki/rest/api/search/user` and tolerates user records (such as app users or deactivated users) that omit `accountId`.
- **Empty Task Checkboxes** ([#548](https://github.com/rust-works/omni-dev/issues/548)): Recognise `- [ ]` and `- [x]` markdown task markers even when the checkbox is not followed by a trailing space. Fixes ADF round-trip drift where empty `taskItem` nodes were parsed as `listItem` nodes containing literal `[ ]` text
- **Literal Checkbox Text in Bullet Lists** ([#548](https://github.com/rust-works/omni-dev/issues/548)): Escape the leading `[` when rendering a `bulletList` item whose literal text begins with a sequence that looks like a task checkbox marker (`[ ]`, `[x]`, or `[X]` followed by space, newline, or end), preventing `to-adf` from falsely promoting these bullet items to `taskList`/`taskItem` on round-trip
- **ADF Round-Trip URL Brackets** ([#551](https://github.com/rust-works/omni-dev/issues/551)): Preserve square brackets in URLs embedded in link-marked text so ADF→JFM→ADF round-trips no longer leak `\[`/`\]` escapes or split the text into corrupted `inlineCard` nodes
- **ADF Mark Combinations** ([#554](https://github.com/rust-works/omni-dev/issues/554)): `textColor`, `backgroundColor`, `subsup`, `underline`, and `annotation` marks were silently dropped when combined with a `code` mark or with each other. Marks are now preserved by nesting `:span[…]{…}` and `[…]{…}` wrappers based on the original mark order
- **Underscore boundary in span directives** ([#554](https://github.com/rust-works/omni-dev/issues/554)): `from-adf` now escapes underscores at text-node boundaries (per the CommonMark intraword rule). Previously, a plain text node ending with `_ ` followed by a textColor span whose text started with `_` produced JFM that the parser saw as italic, destroying the span directive and losing the textColor mark

## [0.21.0] - 2026-04-17

### Added
- **Confluence Label Commands**: Label management commands for adding, removing, and listing page labels
- **Confluence User Search**: Search for Confluence users by query
- **Confluence Page Comments**: List and add comments on Confluence pages
- **JIRA Watcher Commands**: List, add, and remove watchers on JIRA issues
- **JIRA Worklog Commands**: Time tracking with worklog list and add commands
- **JIRA Sprint Management**: Create and update sprint commands
- **JIRA Dev Status**: Dev status command for viewing development information on issues
- **PR Auto-Push**: `--no-push` flag to skip branch push on PR creation
- **ADF Node Support**: Added `mediaInline`, `placeholder`, `table caption`, `mediaSingle caption`, and `expand localId/parameters` node support
- **ADF Annotation Marks**: Full annotation mark support for ADF/markdown conversion
- **ADF localId Round-Trip**: Comprehensive `localId` round-trip support with `--strip-local-ids` option
- **ADF Border Mark**: Border mark support for media and table cell/header nodes
- **ADF Breakout Width**: Optional width parameter for breakout marks

### Fixed
- **ADF Round-Trip Fidelity** (50+ fixes): Extensive improvements to ADF/markdown round-trip conversion:
  - Preserve mark ordering (link, annotation, strong/em/strike, code) across conversions
  - Prevent consecutive paragraphs from merging in blockquotes, list items, and task items
  - Preserve `localId` on caption, listItem/mediaSingle, layout columns, media, table, and paragraph nodes
  - Handle nested taskList, taskItem, and ordered list nodes correctly
  - Preserve hardBreak nodes in paragraphs, headings, list items, and table cells
  - Preserve trailing/leading whitespace in headings, list items, table cells, and text nodes
  - Escape backticks, backslashes, asterisks, and underscores in plain text to prevent misinterpretation
  - Preserve NBSP content in list item paragraphs and NBSP-only paragraphs
  - Preserve empty language attrs in codeBlock, empty attrs on tableCell, and integer colwidth values
  - Handle parentheses in link/image URLs and bracket-link ambiguity
  - Prevent bare URLs and URL link text from becoming inlineCard nodes
  - Preserve emoji shortName with/without colons, date timestamps, and mention localId attribution
  - Preserve parameters block in bodiedExtension and extension layout/localId attrs
  - Preserve multiple annotation marks, table cell attrs, embedCard attrs, and mediaSingle mode
  - Preserve content after directive table blocks and whitespace-only text after hardBreak
  - Reject pipe syntax for tableCell-only first rows and intraword underscores as emphasis
  - Escape trailing double-spaces to prevent hardBreak misinterpretation
  - Ensure blank lines between consecutive block nodes in table cells
- **Deterministic AI Scopes**: Make AI-generated commit scopes deterministic by post-processing with file-pattern logic
- **CI Scope Guidance**: Improve CI scope guidance in type selection rules

### Changed
- **Function Naming**: Rename `execute_*` helper functions to `run_*` across Atlassian CLI commands
- **Function Extraction**: Extract `run_download`, `run_images`, `run_transition` into standalone functions

### Documentation
- **ADR-0020**: Add ADR for JFM markdown dialect for ADF interchange
- **JFM Spec**: Expand JFM spec with table, media, localId, escaping, and annotation mark sections
- **Style Guide**: Update style guide to clarify `run_*` function extraction rules and add testing style rules

### CI/CD
- Bump `EmbarkStudios/cargo-deny-action` to 2.0.17
- Bump `codecov/codecov-action` from 5 to 6
- Bump `cachix/cachix-action` from 16 to 17
- Bump Rust minor patch dependencies

### Security
- Update `rustls-webpki` to 0.103.12 for RUSTSEC-2026-0098

## [0.20.0] - 2026-04-12

### Added
- **Confluence Download**: Recursive page tree download with concurrent workers
  - BFS tree traversal via Confluence children API
  - Bounded parallel downloads with configurable concurrency (`--concurrency`)
  - Directory tree mirroring with `{id}-{slug}/index.{md,json}` structure
  - Manifest-based resume (`--resume`) with ID-aware page tracking
  - Per-page `meta.json` with untruncated titles and parent IDs
  - Backup-before-clobber with `--on-conflict backup|skip|overwrite`
  - Append-mode `download.log` recording all actions per run
  - Configurable max depth (`--max-depth`)
- **Structured Output**: `--output json|yaml|table` flag on all list/table commands
  - Added `Serialize` derives to all public Atlassian data types
  - `OutputFormat` enum with `output_as()` helper
- **HTTP 429 Rate Limiting**: Automatic retry with `Retry-After` header support
  - All transport methods (`get_json`, `post_json`, `put_json`, `delete`, `get_bytes`) retry on 429
  - Exponential backoff fallback when no `Retry-After` header is present
  - Configurable max retries (default: 3)

### Fixed
- **Nested Container Directives**: Container directives (`:::expand`, `:::panel`) inside table cells, layout columns, and other containers are now correctly parsed with depth tracking
- **hardBreak in Table Cells**: Tables containing `hardBreak` nodes now fall back to directive form instead of pipe tables, preventing row corruption on round-trip
- **Multi-Paragraph Containers**: Panels, expands, layout columns, and extensions with multiple paragraphs now render with blank-line separators, preserving paragraph boundaries on round-trip
- **Commit Message Generator**: Added type selection rules to align generator with checker expectations — prevents incorrect type selection (e.g., `docs` for source code changes)

## [0.19.0] - 2026-04-10

### Added
- **Atlassian Integration**: Comprehensive JIRA and Confluence CLI commands via JFM (JIRA-Flavored Markdown) format
  - Read and write JIRA and Confluence content as JFM markdown
  - JIRA issue create, delete, search (JQL), and transition commands
  - JIRA comment list and add commands
  - JIRA issue link management and link list commands
  - JIRA issue attachment download commands
  - JIRA issue changelog command
  - JIRA agile board and sprint management commands
  - JIRA project list and field listing/options commands
  - Auto-discover field context when fetching JIRA field options
  - Confluence search (CQL), create, and delete commands with purge flag
  - `post_json` and `delete` methods on `AtlassianClient`
- **Auto-Pagination**: Automatic pagination for all Atlassian API methods
- **Claude CLI Model Resolve**: `claude cli model resolve` command for model resolution diagnostics

### Changed
- **CLI Help Text**: Improved help text and converted key arguments to positional for Atlassian commands

### Fixed
- **Confluence Delete Error**: Improved 404 error message for confluence delete command
- **JIRA Search Endpoint**: Updated search endpoint and handle missing `total` field in response

### Security
- **CI Hardening**: Pin `cargo-deny-action` version and update vulnerable dependencies

### Testing
- **Field Context Error Handling**: Added test for `get_field_contexts` 404 error response

### Documentation
- **Atlassian User Guide**: Comprehensive command reference for all Atlassian CLI commands
- **JFM Specification**: Moved JFM spec from plan to specs directory with revised content
- **Atlassian Scope**: Added `atlassian` scope to scope list
- **v0.18.0 Retrospective**: Added release retrospective document

## [0.18.0] - 2026-02-26

### Added
- **Split Dispatch for Large Diffs**: Intelligent per-file diff splitting when commits exceed token budgets
  - Per-file and per-hunk unified diff parser for granular diff handling
  - Per-file diff storage with `FileDiffRef` struct tracking byte lengths
  - Greedy file-packing algorithm for token-budget-constrained splitting
  - Split dispatch across amendment, check, and multi-commit operations
  - Per-hunk diff override support for partial commit views
  - Placeholder substitution for oversized diffs instead of hard failures
- **File-Level Context Analysis**: Hook-based file analyzer adds semantic context to commit pipelines
- **Walk-Up Config Directory Discovery**: Config resolution walks up the directory tree to find `.omni-dev/` directories
- **XDG Base Directory Compliance**: Config resolution follows XDG standards for fallback locations
- **`OMNI_DEV_CONFIG_DIR` Environment Variable**: Explicit config directory override for all commands
- **Config Source Tracking**: Diagnostic output shows where each config file was loaded from
- **`--quiet` Flag**: Suppress interactive retry prompts in twiddle
- **`--context-dir` Option**: Explicit context directory for create-pr command
- **`--refine` Flag**: Opt-in to refine mode (fresh mode is now the default for twiddle)
- **Amendment Parse Retry Logic**: Automatic retry on amendment parse and AI request failures
- **Claude Sonnet 4.6 Model**: Added to model registry and set as default
- **Preflight Checks Expanded**: Applied consistently to amend, view, and info commands
- **Configurable Mock AI Client**: Shared test utility for integration testing

### Changed
- **Fresh Mode Default**: Twiddle now generates messages from scratch by default; use `--refine` to amend existing messages
- **Default AI Model**: Claude Sonnet 4.6 replaces previous default
- **Registry Default Model**: Uses model registry default instead of hardcoded model strings

### Removed
- **`--batch-size` Flag**: Removed deprecated flag from check and twiddle commands (use `--concurrency` instead)
- **Progressive Diff Reduction**: Removed fallback strategy in favor of split dispatch
- **Dead Utils Module**: Removed unused `utils/general` module and its re-exports

### Fixed
- **Batch Processing Failure Tracking**: Track failed commit indices when batch processing errors occur
- **Progress Counter**: Increment only on success path to avoid inflated counts
- **Split Dispatch Overhead**: Correctly subtract prompt overhead from chunk capacity
- **Token Estimation**: Use conservative estimation for code diffs
- **Error Chain Display**: Print full error chain for failed commits in twiddle
- **Tracing Output**: Write tracing output to stderr instead of stdout
- **Stdin EOF Loop**: Prevent infinite loop on EOF in interactive prompts
- **Provider String Matching**: Use exact string matching in prompt_style selection
- **Field Presence Tracking**: Add missing `branch_prs[].base` field
- **Batch-Size Deprecation**: Proper deprecation warning for `--batch-size` flag

### Security
- **Dependency Advisories**: Update `bytes` and `git2` to resolve security advisories
- **CI Hardening**: Add security audit, dependency policy, and secret scanning workflows

### Refactored
- **Generic Repository View**: Make `RepositoryView` and `CommitInfo` generic over inner types
- **Single Async Runtime**: Migrate command execution to single tokio runtime
- **Consolidated Config Resolution**: Unified config resolution into discovery module
- **Panic-Free Operations**: Replace panicking operations with proper error handling across all modules
- **Pure Logic Extraction**: Extract and test pure logic from twiddle, create-pr, and check commands
- **Interactive Retry Extraction**: Extract interactive retry loop and `read_interactive_line` helper into testable methods
- **Shared Formatting Utilities**: Extract formatting module for reuse across commands
- **AI Client Helpers**: Extract shared helpers for AI client implementations
- **Deduplicated Models Embed**: Shared constant for `models.yaml` embedding

### Testing
- **Property-Based Tests**: Added proptest-based tests across 7 modules
- **Comprehensive Unit Tests**: Added tests for 6+ previously untested modules
- **Split Dispatch Integration Tests**: Integration tests with prompt recording
- **Test Directory Isolation**: Relocated temp directories to project-local `tmp/` folder
- **Parallel Test Safety**: Fix process-wide CWD mutation causing parallel test failures

### Documentation
- **Architecture Decision Records**: Added ADRs 0004–0019 covering embedded templates, hierarchical config resolution, two-view data model, preflight validation, deterministic pre-validation, token-budget batch planning, multi-layer retry, model registry, severity levels, self-describing YAML, provider-specific prompts, dual error handling, hierarchical CLI, per-file diff splitting, context detection, and ecosystem scope auto-detection
- **Architecture Documentation**: Comprehensive architecture docs for the codebase
- **Style Guide**: Broadened scope, added STYLE-0022 (ADR format) and STYLE-0023 (commit validation)
- **Config Resolution Docs**: Four-tier config resolution with walk-up discovery and XDG support

### CI/CD
- **Code Coverage Enforcement**: Enforce minimum coverage threshold and fail on codecov errors
- **GitHub Actions Updates**: Bump actions/checkout to v6, codecov-action to v5, cachix/install-nix-action to v31, cachix/cachix-action to v16
- **Clippy Pedantic Lints**: Enable pedantic and nursery lint groups

### Dependencies
- `crossterm` 0.28 → 0.29
- `dirs` 5.0 → 6.0
- `ssh2-config` 0.6 → 0.7
- `thiserror` 1.x → 2.x
- `reqwest` 0.12 → 0.13
- Rust minor-patch group with 13 updates

## [0.17.0] - 2026-02-13

### Added
- **Ecosystem Default Scopes**: Automatic scope detection based on project ecosystem
  - Detects Rust, Node.js, Python, Go, and Java projects from marker files (Cargo.toml, package.json, etc.)
  - Merges ecosystem-specific default scopes (e.g., `cargo`, `lib`, `core`, `test` for Rust)
  - Skips defaults that conflict with existing custom scopes in `scopes.yaml`
  - Works consistently across twiddle, check, and PR creation commands
- **Scope Pre-Validation**: Deterministic scope checks before AI processing
  - Validates scope format (e.g., multi-scope comma separation without spaces)
  - Verifies scope validity against the merged scope list before sending to AI
  - Passing checks recorded in `pre_validated_checks` field so the AI skips re-checking them
  - Prevents AI from contradicting deterministic validations

### Fixed
- **Config Loading**: Always load `.omni-dev/` configuration regardless of directory existence
  - Previously skipped config loading when the context directory didn't exist as a directory
  - Now correctly resolves individual config files even when the parent directory is absent
  - Fixes scope and guideline loading in projects without an explicit `.omni-dev/` directory

### Refactored
- **Scope Loading Consolidation**: Unified scope loading across all commands
  - Extracted `load_project_scopes()` as a single entry point for scope resolution
  - Consistent config file priority (local override → project → home fallback) everywhere
  - Eliminated duplicated scope loading logic between twiddle and check commands

### Documentation
- **Configuration Best Practices**: New guide for `.omni-dev/` configuration
  - Scope definition patterns, file pattern matching, and local override workflows
  - Troubleshooting guide for common configuration issues
- **Configuration Internals**: New technical reference for configuration resolution
  - Detailed explanation of config file priority, ecosystem detection, and scope merging
  - Architecture diagrams for the discovery pipeline

## [0.16.0] - 2026-02-12

### Added
- **Parallel Map-Reduce Processing**: Replaced sequential batch processing with concurrent commit processing
  - Each commit processed individually in parallel using semaphore-based concurrency control
  - New `--concurrency` flag (default: 4) replaces deprecated `--batch-size`
  - Real-time progress feedback with atomic completion counters
  - Graceful failure handling continues processing remaining commits
- **Cross-Commit Coherence Pass**: Optional AI refinement for consistency across commit messages
  - Ensures consistent scope usage, terminology, and message quality across a commit set
  - New `--no-coherence` flag to skip the coherence pass when not needed
  - Automatically skipped when all commits fit in a single batch
- **Token-Budget-Aware Commit Batching**: Intelligent grouping using first-fit-decreasing bin-packing
  - Groups commits into batches that fit within the AI model's token budget
  - Estimates tokens from file metadata without reading full content
  - Split-and-retry fallback for oversized batches with progressive diff reduction
  - Reduces API calls from O(n) to O(batches) while maintaining quality
- **Progressive Diff Reduction**: Four-level fallback for token budget optimization
  - Automatically reduces diff detail when prompts exceed model limits: Full → Truncated → StatOnly → FileListOnly
  - Precise truncation calculations with tokens-to-chars conversion
  - Maximizes context sent to AI while respecting model constraints
- **Token Budget Validation**: Pre-flight token estimation and budget check before all AI requests
  - Estimates prompt token count using a character-based heuristic with 10% safety margin
  - Validates prompts fit within the model's input context window minus reserved output tokens
  - Returns a clear `PromptTooLarge` error instead of letting the API reject oversized requests
  - Covers all AI call paths: twiddle, check, PR creation, and raw message sending
- **HTTP Request Timeout Configuration**: Configurable timeout for AI client HTTP requests
- **Enhanced YAML Formatting**: Improved multi-line commit message formatting in YAML output

### Changed
- **Deprecated `--batch-size`**: Replaced by `--concurrency` flag with clearer semantics; `--batch-size` remains as a hidden backward-compatible alias

### Refactored
- **Module Structure Flattening**: Converted `mod.rs` files to direct module files across claude, cli, data, and git modules
- **Git CLI Split**: Split monolithic git module into focused subcommand modules
- **YAML Payload Reduction**: Reduced per-commit YAML payload size for more efficient AI analysis
- **Dead Code Removal**: Removed unused core module scaffolding

### Fixed
- **Error Handling**: Improved error handling and configuration parsing in AI client

### Documentation
- **Architecture Decision Records**: Introduced ADR framework with ADR-0001 (YAML as primary data exchange format)
- **Style Guide Enhancements**: Added tag-based categorization system, task-to-tag lookup table, and STYLE-0020 single-purpose commit guidelines
- **Commit Guidelines**: Enhanced with multi-scope support and practical examples
- **Module Layout Guidance**: Refined examples and guidance for module organization
- **Documentation Updates**: Updated all docs to reflect `--concurrency` replacing `--batch-size`

## [0.15.0] - 2026-02-08

### Added
- **Beta Header Support**: New `--beta-header` flag for twiddle and check commands
  - Enables enhanced model capabilities like 1M context window and 128K output tokens
  - Format: `--beta-header key:value` (e.g., `--beta-header anthropic-beta:context-1m-2025-08-07`)
  - Validates beta headers against the model registry with helpful error messages
  - Beta-aware token limits automatically applied to API requests and display
  - Debug logging for active beta headers sent with API requests
- **Interactive Chat Command**: New `omni-dev chat` command for conversational AI interaction
  - Interactive Claude AI chat session with streaming-style responses
  - Configurable system prompts and model selection
  - Multi-line input support and conversation history
- **Interactive Twiddle Mode for Check**: New `--twiddle` flag on check command
  - Automatically runs twiddle to fix failing commit messages after check identifies issues
  - Streamlined workflow for validating and correcting commits in one step
- **Intelligent Retry Mechanism**: Smart retry for twiddle commit validation
  - Automatically retries failed commit message generation with refined prompts
  - Configurable retry limits with exponential backoff
  - Improved success rates for challenging commit messages
- **Deterministic Scope Pre-Validation**: Rule-based validation before AI processing
  - Catches common scope formatting issues (e.g., extra spaces) without API calls
  - Reduces unnecessary AI requests for deterministic formatting rules

### Changed
- **Model Catalog Update**: Updated AI model registry to February 2026
  - Added Claude Opus 4.6 as current flagship model
  - Added beta header definitions for models supporting extended context and output
  - Updated model specifications and tier classifications

### CI/CD
- **Enhanced Commit-Check Workflow**: Improved CI validation pipeline
  - Added concurrency control to prevent redundant workflow runs
  - Updated GitHub Actions to latest versions

### Documentation
- **Context Window Documentation**: Added documentation for context window limitations and fallback behavior

## [0.14.0] - 2026-02-08

### Added
- **Scope Refinement via File Patterns**: Intelligent scope detection that matches changed file paths against configured scope patterns from `.omni-dev/scopes.yaml`
  - Pattern matching using globset for project-specific scope rules
  - Specificity-based matching prioritizes more specific patterns
  - Support for negation patterns and multi-scope matching
  - Fallback to original detection when no patterns match
  - Applied across twiddle, check, and validation commands
- **Preflight Validation System**: Comprehensive early failure detection for AI and GitHub commands
  - AI provider detection and credential validation for Claude, Bedrock, OpenAI, and Ollama
  - GitHub CLI availability and authentication checks
  - Clear, actionable error messages with resolution guidance
  - Integrated into twiddle, create-pr, and check commands
- **Working Directory Validation**: Early cleanliness check before expensive twiddle operations
  - Detects staged changes, unstaged modifications, and untracked files
  - Provides detailed error messages showing specific uncommitted files
  - Prevents wasted AI processing time on dirty working directories
- **Model Parameter for create-pr**: Added `--model` flag to create-pr command for model selection

### Changed
- **Scope Definitions Loading**: Simplified and consolidated scope definitions loading logic in twiddle command
  - Scope refinement now works consistently with or without contextual intelligence
  - Same logic pattern applied to both full and batch processing modes

## [0.13.1] - 2025-01-07

### Fixed
- **Bedrock Client Selection Logic**: Fixed inverted conditional that prevented Bedrock from being used
  - Setting `CLAUDE_CODE_USE_BEDROCK=true` now correctly uses Bedrock client
  - Removed confusing `CLAUDE_CODE_SKIP_BEDROCK_AUTH` requirement
  - Users only need `CLAUDE_CODE_USE_BEDROCK=true`, `ANTHROPIC_AUTH_TOKEN`, and `ANTHROPIC_BEDROCK_BASE_URL`
- **CI Publish Ordering**: Publish to crates.io only after all platform builds succeed

### Added
- **Scope Definitions**: Added `release` and `workflows` scopes for better commit categorization
  - `release`: Version bumps, changelog updates, release preparation
  - `workflows`: GitHub Actions and CI/CD pipeline changes

### Changed
- **CI Commit Check**: Trigger commit validation on push to main branch
- **CI Workflow**: Removed version pinning from commit-check workflow

## [0.13.0] - 2025-12-27

### Added
- **Post-Twiddle Validation**: New `--check` flag for twiddle command
  - Automatically validates commit messages after applying amendments
  - Runs full AI-powered analysis against project guidelines
  - Supports batched processing for large commit ranges
  - Single-step workflow: improve and validate in one command
- **Guidance File Diagnostics**: Enhanced diagnostic output for loaded configuration
  - Shows status of commit guidelines, scopes, and other guidance files
  - Clear visibility into which configuration files are being used
  - Helps troubleshoot configuration issues
- **Scope Validation in Check**: Enhanced commit message checking with scope awareness
  - Validates commit scopes against project-defined scope list
  - Reports invalid or missing scopes as warnings

### Changed
- **CI Workflow Enhancement**: Added commit message validation for pull requests
  - New GitHub Actions workflow validates PR commit messages
  - Automatic quality enforcement on all pull requests

### Documentation
- **Release Process Restructure**: Comprehensive overhaul of release documentation
  - Reorganized for automated CI/CD workflow with clear manual vs automated steps
  - Added documentation review phase before version updates
  - Enhanced with CI monitoring commands and verification steps
  - Improved release skill with complete automation guidance
- **README Updates**: Added documentation for check command and new twiddle options
  - New section for commit message validation command
  - Updated options table with `--fresh` and `--check` flags

## [0.12.0] - 2025-12-25

### Added
- **Commit Message Validation Command**: New `check` command for validating commit messages against project guidelines
  - AI-powered analysis with configurable severity levels (error, warning, info)
  - Multiple output formats (text, JSON, YAML) for CI/CD integration
  - Batch processing support for large commit ranges
  - Smart exit codes for pipeline integration (0=pass, 1=errors, 2=warnings in strict mode)
  - Optional suggestion generation for improved commit messages
  - Color-coded severity indicators in text output
- **Fresh Mode for Twiddle**: Generate commit messages from scratch ignoring existing messages
  - New `--fresh` flag for twiddle command
  - Forces AI to analyze only diff content for completely fresh suggestions
  - Useful for poorly-written or misleading original messages
- **Base Branch Support**: Explicit base branch selection for PR creation and updates
  - New `--base` flag for `create pr` command
  - Intelligent base branch resolution with fallback logic
  - Interactive confirmation when changing base branch on updates
  - Better visibility of target branches in PR operations
- **Comprehensive Gemini Model Support**: Full Google Gemini model catalog
  - Gemini 3.0 Pro and Flash (preview models)
  - Gemini 2.5 series (Pro, Flash, Flash-Lite)
  - Legacy support for Gemini 2.0 and 1.5 series
  - Three-tier system (flagship, balanced, fast) for model selection

### Changed
- **AI Model Registry Update**: Updated to latest model releases (December 2025)
  - Added Claude 4.5 series (Opus, Sonnet, Haiku) as current generation
  - Updated default Claude model to claude-sonnet-4-5-20250929
  - Added OpenAI GPT-5.2, o3/o4 reasoning models, and GPT-4.1 series
  - Marked legacy models appropriately for deprecation visibility

### Refactored
- **Commit Guidelines Template**: Extracted default guidelines to shared template file
  - Single source of truth in `src/templates/default-commit-guidelines.md`
  - Consistent guidelines between twiddle and check commands
  - Easier maintenance and editing as markdown

### Documentation
- **Enhanced Commit Guidelines**: Comprehensive guidelines with severity levels
  - Detailed type and scope tables with clear use cases
  - Subject line rules with imperative mood requirements
  - Accuracy requirements section for truthful descriptions
  - Severity level mapping for CI/CD integration
- **Commit Message Check Plan**: Detailed implementation specification
  - Design philosophy for guideline-driven validation
  - Command structure and output format examples
  - CI integration patterns and exit code behavior

## [0.11.0] - 2025-12-10

### Added
- **Draft PR Support**: New draft PR functionality with configurable defaults
  - Added `--draft` flag to PR creation command for creating draft pull requests
  - Configurable default draft status via `.omni-dev/pr-config.yaml`
  - Enhanced PR workflow with draft mode for work-in-progress changes
- **No-AI Mode for Twiddle**: Direct YAML output without AI processing
  - Added `--no-ai` flag to twiddle command for direct YAML generation
  - Enables manual editing workflows without AI-powered amendment
  - Better integration with custom automation pipelines

### Documentation
- **AI-Generated PR Guidelines**: Comprehensive documentation for PR description generation
  - Detailed guidelines for AI-powered PR description creation
  - Best practices and examples for effective PR generation
  - Enhanced documentation for team collaboration

### Fixed
- **PR Creation Branch Handling**: Improved head branch parameter handling
  - Fixed explicit head branch parameter in `gh pr create` command
  - Better handling of upstream branch configuration
  - More reliable PR creation workflow

## [0.10.0] - 2025-09-30

### Added
- **Branch Information in Twiddle**: Enhanced twiddle repository view with branch information
  - Branch context now included in commit analysis and AI-powered amendments
  - Better understanding of current branch status for more targeted suggestions
  - Improved repository view completeness for AI assistants

### Enhanced
- **AI Model Configuration**: Updated default models to Claude Opus 4.1
  - Latest AI model specifications for improved performance
  - Enhanced model registry with updated token limits and capabilities
  - Better AI response quality and accuracy
- **PR Command User Experience**: Improved PR command UX by showing context early
  - Faster feedback for users during PR creation process
  - Better progress indicators and context display
  - Enhanced user interface clarity
- **PR Template Integration**: Enhanced PR template location exposure in repository views
  - PR template location now visible in repository analysis
  - Better integration between template system and PR creation workflow
  - Improved AI understanding of project PR standards

### Documentation
- **Comprehensive Scope Documentation**: Added detailed scope documentation and usage examples
  - Complete guide for scope usage patterns and best practices
  - Real-world examples and configuration scenarios
  - Enhanced developer documentation for project customization

## [0.9.0] - 2025-09-18

### Added
- **AI-Powered Pull Request Creation**: New `git create pr` command with intelligent PR generation
  - Automatically generates PR titles and descriptions using AI analysis of commits and diffs
  - Supports both interactive creation and save-only modes for review
  - Integrates with GitHub CLI for seamless PR creation and updates
  - Context-aware analysis using project-specific guidelines and branch information
- **PR Guidelines System**: Project-specific PR description guidelines support
  - New `.omni-dev/pr-guidelines.md` configuration file for PR generation guidance
  - Separate from commit guidelines to allow different standards for PRs vs commits
  - Local override support with priority: local > project > global
  - Integration with AI prompts for project-consistent PR descriptions
- **Enhanced PR Template**: Significantly improved `.github/pull_request_template.md`
  - Added comprehensive sections for testing, performance, security, and deployment
  - Better structure and guidance for thorough PR descriptions
  - Includes examples and best practices for different types of changes
- **YAML Output Format**: New structured output format for PR details
  - `pr-details.yaml` replaces `pr_description.md` for better structured data
  - Complete PR content serialization including title and description
  - Better integration with automation and tooling workflows

### Enhanced
- **Context-Aware AI Generation**: PR creation now uses full project context
  - Leverages branch analysis, work patterns, and architectural understanding
  - Project-specific scope validation and suggestions for PR organization
  - Enhanced prompts that incorporate both commit analysis and PR best practices
- **Command-Specific Guidance Display**: Improved user interface clarity
  - Twiddle command shows only commit guidelines (focused on commit messages)
  - PR creation command shows only PR guidelines (focused on PR descriptions)
  - Eliminates confusion about which guidelines are being used for each operation
- **Comprehensive Documentation**: Updated user guides and README
  - Added complete workflow documentation for PR creation feature
  - Enhanced examples and usage patterns for both commit and PR workflows
  - Better organization of feature documentation and command references

### Fixed
- **YAML Parsing Robustness**: Improved Claude API response processing
  - Better handling of markdown-wrapped YAML responses from AI
  - Consistent parsing logic across commit amendments and PR generation
  - Enhanced error diagnostics for malformed AI responses

## [0.8.0] - 2025-09-17

### Added
- **AI Model Configuration System**: New `config models show` command to view available AI models
  - Complete model registry with token limits and specifications
  - Support for both standard Claude and AWS Bedrock identifier formats
  - Model information display in twiddle command output
- **Interactive Amendment Editing**: `--edit` option for twiddle command
  - Integration with `OMNI_DEV_EDITOR` and `EDITOR` environment variables
  - Manual review and editing of AI-generated amendments before applying
- **Build Automation Script**: New `scripts/build.sh` for standardized builds
  - Combines cargo build, format checking, and clippy analysis
  - Comprehensive error handling and progress indicators

### Enhanced
- **Contextual Intelligence System**: Significantly improved commit message generation
  - Home directory fallback support for all `.omni-dev` configuration files
  - Literal template reproduction ensures AI follows project formats exactly
  - Enhanced diagnostic output showing guidance file status and sources
- **AI Client Logging**: Improved debugging and observability
  - Enhanced logging for API requests and responses
  - Better error handling and diagnostics for troubleshooting

### Removed
- **Commit Template System**: Removed template functionality to simplify configuration
  - Projects should use commit guidelines instead of templates
  - Eliminates conflicts between templates and guidelines
  - **BREAKING**: `.gitmessage` and commit template files are no longer loaded

## [0.7.0] - 2025-09-14

### Added
- **AWS Bedrock AI Client**: Complete integration with AWS Bedrock for Claude AI model access
  - Implemented `BedrockAiClient` with full AWS API support
  - Added comprehensive logging and diagnostics for troubleshooting
  - Support for AWS credentials and region configuration
  - Integration with existing `AiClient` trait architecture
- **AI Client Architecture**: Extensible AI provider system
  - New `AiClient` trait for pluggable AI providers
  - Provider selection and configuration management
  - Support for multiple AI service backends
- **Settings Management System**: Enhanced configuration handling
  - New settings management utilities for AI provider configuration
  - Environment-based configuration support
  - Structured settings validation and loading

### Improved
- **Code Quality**: Resolved clippy warnings for better maintainability
  - Fixed `vec_init_then_push` patterns with `vec![]` macro usage
  - Improved code consistency and performance

## [0.6.0] - 2025-09-09

### Added
- **File-based Amendment Workflow**: Complete overhaul of the twiddle command user experience
  - Save amendments to temporary YAML files instead of printing to stdout
  - Interactive menu system with [A]pply/[S]how/[Q]uit options 
  - File content preview functionality for reviewing changes before applying
  - Better user feedback and more granular control over amendment process
  - Preserved backward compatibility with `--auto-apply` and `--save-only` options

- **Local Configuration Overrides**: Personal workflow customization system
  - Support for `.omni-dev/local/` directory to override shared project settings
  - Local override capability for all configuration files (scopes, guidelines, templates)
  - Priority system: local overrides take precedence over shared project configuration
  - Automatic `.gitignore` exclusion to keep personal settings private
  - Comprehensive documentation for setup and usage patterns

- **Structured Debug Logging**: Professional logging system using `RUST_LOG`
  - Integration with `tracing` and `tracing-subscriber` for structured logging
  - Module-specific debug control (e.g., `RUST_LOG=omni_dev::claude=debug`)
  - Detailed diagnostic information for troubleshooting configuration and API issues
  - Comprehensive documentation in troubleshooting guide
  - Replaced custom verbose flag with industry-standard logging approach

### Improved
- **YAML Output Formatting**: Enhanced readability of amendment files
  - Automatic conversion of multiline commit messages to YAML literal block scalars (`|`)
  - Proper formatting instead of escaped newlines in quoted strings
  - Better preserved indentation and structure in generated files
  - Improved user experience when reviewing amendment content

### Removed
- **Verbose Flag**: Removed `--verbose`/`-v` CLI option in favor of `RUST_LOG` environment variable
  - More flexible and powerful debugging control through standard Rust logging
  - Better performance with zero overhead when logging is disabled
  - Industry-standard approach familiar to Rust developers

### Documentation
- **Comprehensive RUST_LOG Documentation**: Added detailed logging guides
  - Basic usage examples and log level explanations
  - Module-specific targeting for focused debugging
  - Common troubleshooting scenarios with specific commands
  - Updated README.md with debugging section
- **Local Override Documentation**: Complete guide for personal configuration management
  - Setup instructions and best practices
  - Real-world usage examples and patterns
  - Integration with team workflows

## [0.5.0] - 2025-09-01

### Changed
- **Diff Output Format**: Modified YAML output to write diff content to external files instead of embedding in YAML
  - Changed `diff_content` field to `diff_file` in `CommitAnalysis` struct for improved memory usage
  - Diff content now written to temporary files in AI scratch directory
  - Enables AI assistants to access detailed diff information through file reads
  - Maintains backward compatibility with similar data structure
  - Updated field documentation for AI assistant guidance

## [0.4.1] - 2025-08-29

### Fixed
- **Rebase Operations**: Fixed short commit hash ambiguity in interactive rebase operations
  - Modified rebase sequence generation to use full commit hashes instead of 7-character truncated hashes
  - Eliminates "short object ID is ambiguous" errors when multiple git objects share the same hash prefix
  - Ensures reliable commit amendment operations regardless of repository size

## [0.4.0] - 2025-08-29

### Added
- **Command Template Management**: New command template system for enhanced CLI experience
  - Added `pr-update` command template generation for pull request workflow automation
  - Implemented comprehensive command template management system
  - Enhanced Claude slash command integration with structured templates
- **AI Scratch Directory Support**: Added AI scratch directory configuration support
  - Integrated AI_SCRATCH environment variable support for enhanced AI assistant workflows
  - Added scratch directory path handling in command templates
- **Version Information Enhancement**: Added version information to command outputs
  - Commands now include version context for better debugging and support
  - Enhanced output format with version tracking
- **Documentation Improvements**: Enhanced slash command documentation structure
  - Improved Claude command file organization and documentation
  - Added comprehensive AI assistant guide and release documentation
  - Better structured troubleshooting information in slash commands

## [0.3.0] - 2025-08-26

### Added
- **Field Presence Tracking**: Enhanced YAML output with explicit field presence indicators
  - Added `present: bool` field to `FieldDocumentation` struct for AI assistant guidance
  - Implemented `update_field_presence()` method on `RepositoryView` to dynamically track available fields
  - Added comprehensive AI assistant guidance in field explanation text
  - Included git command mappings for better field documentation
- **Enhanced Command Structure**: Reorganized Claude command files with improved analysis instructions
  - Added commit-twiddle commands for debug, release, and standard modes
  - Added pr-create commands with enhanced PR workflow decision guidance
  - Standardized command structure across all variants with detailed field checking instructions

### Changed
- **Data Structure Improvements**: Reordered RepositoryView fields to place commits last for better readability
  - Summary fields (explanation, working_directory, remotes, branch_info, pr_template, branch_prs) now appear before detailed commit analysis
  - Improved YAML output organization and user experience

### Fixed
- **Code Quality**: Resolved clippy warnings for better code quality
  - Replaced deprecated `map_or(false, |prs| !prs.is_empty())` patterns with `is_some_and(|prs| !prs.is_empty())`
  - Maintained proper borrowing semantics with `.as_ref()` calls

## [0.2.0] - 2025-08-26

### Added
- **Git Branch Analysis**: New `omni-dev git branch info` command for comprehensive branch analysis
  - Branch-aware commit analysis with automatic range calculation
  - Current branch detection and validation
  - Base branch comparison (defaults to main/master)
  - Enhanced YAML output including branch context
- **GitHub Integration**: GitHub CLI integration for enhanced functionality
  - Accurate main branch detection using GitHub API
  - Pull request information retrieval and display
  - PR template support with conditional YAML output
  - GitHub repository URI parsing and validation
- **Git Commit Analysis**: Comprehensive commit analysis with YAML output
  - Commit metadata extraction (hash, author, date)
  - File change analysis and diff statistics
  - Conventional commit type detection
  - Remote branch tracking and main branch detection
  - Working directory status reporting
- **Commit Message Amendment**: Safe and reliable commit message modification
  - HEAD commit amendment using `git commit --amend`
  - Multi-commit amendment via individual interactive rebases
  - Shell-script-inspired strategy for reliable rebase operations
  - YAML-based amendment file format with validation
- **Safety Features**: Comprehensive safety checks and error handling
  - Working directory cleanliness validation (ignoring build artifacts)
  - Commit existence and accessibility validation
  - Automatic rebase abort and error recovery
  - Prevention of amendments to potentially problematic commits
- **CLI Interface**: Full-featured command-line interface
  - `omni-dev git commit message view [range]` - Analyze and view commits
  - `omni-dev git commit message amend <yaml-file>` - Amend commit messages
  - `omni-dev git branch info [base-branch]` - Analyze branch commits
  - Rich help system and error reporting
- **Testing Infrastructure**: Comprehensive test suite
  - Integration tests with temporary git repositories
  - Amendment functionality validation
  - YAML parsing and validation tests
  - Error handling and edge case testing

### Changed
- Complete rewrite of core functionality to focus on git commit operations
- Updated CLI interface to provide git-specific commands
- Enhanced error handling with detailed context and recovery options
- Remote information now uses `uri` field instead of `url` for consistency

### Fixed
- Working directory safety checks now properly ignore git-ignored files
- Multi-commit amendment reliability improved with individual rebase strategy
- Clippy linting warnings resolved (needless_borrows_for_generic_args)
- Compilation warnings eliminated through dead code cleanup

## [0.1.0] - 2024-08-24

### Added
- Initial release of omni-dev
- Basic project structure and configuration
- CLI application with version and help commands
- Core application framework with configuration support
- Utility functions for input validation and byte formatting
- Comprehensive test suite
- GitHub Actions CI/CD pipeline
- Documentation and community files (README, CONTRIBUTING, CODE_OF_CONDUCT)
- BSD 3-Clause license

[Unreleased]: https://github.com/rust-works/omni-dev/compare/v0.29.0...HEAD
[0.29.0]: https://github.com/rust-works/omni-dev/compare/v0.28.0...v0.29.0
[0.28.0]: https://github.com/rust-works/omni-dev/compare/v0.27.0...v0.28.0
[0.27.0]: https://github.com/rust-works/omni-dev/compare/v0.26.0...v0.27.0
[0.26.0]: https://github.com/rust-works/omni-dev/compare/v0.25.0...v0.26.0
[0.25.0]: https://github.com/rust-works/omni-dev/compare/v0.24.0...v0.25.0
[0.24.0]: https://github.com/rust-works/omni-dev/compare/v0.23.1...v0.24.0
[0.23.1]: https://github.com/rust-works/omni-dev/compare/v0.23.0...v0.23.1
[0.23.0]: https://github.com/rust-works/omni-dev/compare/v0.22.0...v0.23.0
[0.22.0]: https://github.com/rust-works/omni-dev/compare/v0.21.0...v0.22.0
[0.21.0]: https://github.com/rust-works/omni-dev/compare/v0.20.0...v0.21.0
[0.20.0]: https://github.com/rust-works/omni-dev/compare/v0.19.0...v0.20.0
[0.19.0]: https://github.com/rust-works/omni-dev/compare/v0.18.0...v0.19.0
[0.18.0]: https://github.com/rust-works/omni-dev/compare/v0.17.0...v0.18.0
[0.17.0]: https://github.com/rust-works/omni-dev/compare/v0.16.0...v0.17.0
[0.16.0]: https://github.com/rust-works/omni-dev/compare/v0.15.0...v0.16.0
[0.15.0]: https://github.com/rust-works/omni-dev/compare/v0.14.0...v0.15.0
[0.14.0]: https://github.com/rust-works/omni-dev/compare/v0.13.1...v0.14.0
[0.13.1]: https://github.com/rust-works/omni-dev/compare/v0.13.0...v0.13.1
[0.13.0]: https://github.com/rust-works/omni-dev/compare/v0.12.0...v0.13.0
[0.12.0]: https://github.com/rust-works/omni-dev/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/rust-works/omni-dev/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/rust-works/omni-dev/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/rust-works/omni-dev/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/rust-works/omni-dev/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/rust-works/omni-dev/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/rust-works/omni-dev/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/rust-works/omni-dev/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/rust-works/omni-dev/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/rust-works/omni-dev/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/rust-works/omni-dev/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/rust-works/omni-dev/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/rust-works/omni-dev/releases/tag/v0.1.0