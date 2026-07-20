# Windows Daemon Port

**Status:** Aspirational — design accepted (ADR-0054); implementation not started, tracked in #1363
**ADRs:** [ADR-0039](../adrs/adr-0039.md) · [ADR-0054](../adrs/adr-0054.md)

## Overview

Run the omni-dev daemon — and the CLIs, MCP tools, Claude hook sink, and VS Code
companion that talk to it — natively on Windows, outside WSL (#1363). The
decisions (named-pipe transport, detached-spawn activation, phase-1 security
posture, path-normalization policy) live in [ADR-0054](../adrs/adr-0054.md);
this document carries the executable detail: the current-state inventory, the
transport seam, the naming spec and cross-language test vector, and the phased
PR roadmap with CI cut lines.

Out of scope (unchanged by this plan): WSL2 including VS Code Remote-WSL — the
daemon inside WSL2 is a Linux daemon and already works; the non-daemon CLI,
which already builds and installs on Windows.

## Current-state inventory

**Unix-coupled (must change):**

- `src/daemon/server.rs` — accept loop and the private framing helpers
  `handle_connection` / `run_stream` / `send_reply` are concretely typed
  `UnixStream` / `Framed<UnixStream, LinesCodec>`; `acquire_listener` returns
  `(UnixListener, bool)`.
- `src/daemon/client.rs` — `DaemonClient::request` calls `UnixStream::connect`.
- `src/daemon/single_instance.rs` — exclusive `UnixListener::bind` as the lock,
  `nix` umask guard for `0600`-from-birth, EADDRINUSE → ping-probe →
  `remove_file` stale reclaim.
- `src/cli/daemon/control.rs::spawn_detached` — `setsid` via `pre_exec`.
- The cfg gates: `src/daemon.rs` (all daemon modules except `paths`),
  `src/cli.rs` (`daemon`/`snowflake`/`worktrees`/`sessions` modules, `Commands`
  variants, dispatch arms), `src/mcp.rs` (`snowflake_tools`).
- `editors/vscode/src/socket.ts` (no `win32` data-dir branch; `sockaddr_un`
  length guard; Unix-path `net.createConnection`) and the second connect site
  `editors/vscode/src/subscription.ts`.

**Already portable (little or no change):**

- `src/daemon/protocol.rs` — NDJSON over `LinesCodec`; no Unix assumption.
- Registry/dispatch (`dispatch_envelope`, `handle_builtin`, `ServiceRegistry`).
- `src/daemon/paths.rs` — `dirs::data_dir()` resolution plus permission helpers
  that already carry `#[cfg(not(unix))]` no-op fallbacks.
- `src/daemon/lifecycle.rs` — already has the `#[cfg(not(unix))]` Ctrl-C arm
  (currently dead code under the gated module tree).
- The browser bridge planes (loopback TCP) and `BridgeService` token writing.
- The engines: `src/worktrees.rs`, `src/sessions.rs`, `src/snowflake/` compile
  on Windows today — only the adapters/CLI/MCP trees are gated.

## Transport seam (roadmap PR1–PR2)

New module `src/daemon/transport.rs`, compiled on all platforms: cfg type
aliases (`ClientStream` = `UnixStream` / `NamedPipeClient`, `ServerStream` =
`UnixStream` / `NamedPipeServer`), a cfg-split `Listener` with
`accept(&mut self)`, and `connect(socket_path)` / `bind(socket_path)`
functions. No enums, no trait objects: two platforms never coexist in one
binary, and generics monomorphize to byte-identical Unix code.

The Windows `accept` is tokio's instance-cycling shape, linearized so the
`server.rs` loop keeps its shape — the replacement instance is created
*before* the connected one is handed off, keeping the `ERROR_PIPE_BUSY` window
near zero:

```rust
// windows accept():
self.next.connect().await?;                        // wait for a client
let connected = std::mem::replace(
    &mut self.next,
    ServerOptions::new().create(&self.pipe_name)?, // NOT first_pipe_instance
);
Ok(connected)
```

Exact signature changes (everything not listed is untouched — notably
`DaemonClient { socket_path: PathBuf }` and every call site on it):

| File                            | Today                                                     | After                                                                   |
|---------------------------------|-----------------------------------------------------------|-------------------------------------------------------------------------|
| `src/daemon/server.rs`          | `acquire_listener(&Path) -> Result<(UnixListener, bool)>` | `-> Result<(transport::Listener, bool)>`; launchd/systemd arms wrap     |
| `src/daemon/server.rs`          | accept loop `Ok((stream, _addr))`                         | `Ok(stream)` via `Listener::accept`                                     |
| `src/daemon/server.rs`          | `handle_connection(stream: UnixStream, …)`                | generic `<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>`          |
| `src/daemon/server.rs`          | `run_stream` / `send_reply` on `Framed<UnixStream, _>`    | generic `<S: AsyncRead + AsyncWrite + Unpin>`                           |
| `src/daemon/client.rs`          | `UnixStream::connect(&self.socket_path)`                  | `transport::connect(&self.socket_path)`                                 |
| `src/daemon/single_instance.rs` | `bind_or_reclaim(&Path) -> Result<UnixListener>`          | `-> Result<transport::Listener>`; Unix body identical, Windows body new |
| `src/daemon/testutil.rs`        | `UnixListener::bind` + `tempdir_in("/tmp")`               | `transport::bind`; tempdir cfg-split (no `sockaddr_un` limit on pipes)  |

`--socket` keeps its name and type: the value is the **endpoint key**. On
Windows a normal path is hashed into a pipe name (below); a value beginning
with `\\.\pipe\` is used verbatim. For verbatim pipe keys,
`log_path_for_socket` / `token_path_for_socket` fall back to `runtime_dir()`
(a pipe namespace has no writable parent directory). `check_socket_path_len`
keeps its call site but gains a `#[cfg(not(unix))]` no-op arm (the 104-byte
limit is a `sockaddr_un` concept), matching the existing `paths.rs` pattern.

## Endpoint naming spec + pinned test vector (PR2, mirrored in PR5)

`paths.rs::runtime_dir()` and `socket_path()` are unchanged: on Windows the
`daemon.sock` path is never created — it is the rendezvous key that derives
the pipe name and anchors `daemon.log` / `bridge.token` co-location.

Derivation (must byte-match in Rust and TypeScript):

1. Take the key string (the resolved socket path).
2. Replace every `/` with `\`.
3. ASCII-lowercase (`A–Z` → `a–z` only; non-ASCII bytes untouched).
4. SHA-256 of the UTF-8 bytes; take the first 16 lowercase hex chars.
5. Pipe name: `\\.\pipe\omni-dev.daemon.<hash16>`.

Rust: pure `pipe_name_for_key(&str)` in `paths.rs`, compiled on all platforms
so the Unix CI exercises the test vector; a `#[cfg(windows)]`
`pipe_name_for_socket(&Path)` wrapper adds the verbatim `\\.\pipe\`
passthrough (checked case-insensitively). `sha2` is already a dependency.
Node: `node:crypto`. Per-user uniqueness comes from the profile directory
inside the key; no username appears in the name (cross-language identity
risk). Logs and `daemon status` print the resolved pipe name alongside the
key.

Pinned cross-language test vector (committed to both test suites; verified):

- key: `C:\Users\Test\AppData\Roaming\omni-dev\daemon.sock`
- normalized: `c:\users\test\appdata\roaming\omni-dev\daemon.sock`
- SHA-256: `6067dae5cefddda1ce0d82efac20547665b4b8ddf899e68052ef7c1e8230c19e`
- pipe name: `\\.\pipe\omni-dev.daemon.6067dae5cefddda1`

## Companion changes (PR5)

- `socket.ts::defaultDataDir` gains a `win32` branch ahead of the XDG fallback:
  `env.APPDATA` when set to an absolute path, else
  `path.join(home, "AppData", "Roaming")` — mirroring `dirs::data_dir()`.
  (Today `win32` wrongly falls into `~/.local/share`.)
- New pure `pipeNameForKey(key)` implementing the spec, and
  `connectEndpoint(socketPath, platform)` returning the pipe name on `win32`
  (verbatim passthrough for `\\.\pipe\` inputs) or the path otherwise.
  `net.createConnection` accepts `\\.\pipe\...` natively.
- Both connect sites route through it: `sendEnvelope` (`socket.ts`) and
  `TreeSubscription.connect` (`subscription.ts`). `checkSocketPathLen` is
  skipped for pipe endpoints (both call sites).
- The `omniDevWorktrees.socketPath` setting keeps its semantics as the key;
  Rust and TS derive the same pipe name, equal modulo `/`-vs-`\` and ASCII
  case.
- `extension.ts` is unchanged (it already degrades gracefully). All new
  branches are unit-testable on any OS via the existing injected
  `env`/`platform`/`home` pattern in `socket.test.ts`; both suites pin the
  test vector.

## Single-instance and security (PR2 + follow-up)

- First instance created with
  `ServerOptions::new().first_pipe_instance(true).reject_remote_clients(true)`;
  exclusive creation **is** the lock. No umask guard, no `set_file_0600` —
  both meaningless for pipes.
- `ERROR_ACCESS_DENIED` (os error 5) at bind → ping-probe for diagnostics
  only: live daemon → today's "already running" error; non-answering holder →
  a distinct "held by another process that does not answer the omni-dev
  protocol" error, never displacement. `ERROR_PIPE_BUSY` (os error 231) on
  client connect → bounded retry loop (~50 ms sleep, ~2 s cap). Both matched
  via `io::Error::raw_os_error()` against local named consts — no
  `windows-sys` dependency in phase 1.
- Shutdown: `remove_socket` becomes Unix-only; dropping the `Listener` closes
  the instances and the name evaporates. `socket_activated` is always `false`
  on Windows.
- Phase-1 posture and residual exposure (cross-user opens; deterministic-name
  squatting/impersonation) per ADR-0054 Decision 5. The hardening follow-up
  issue — explicit owner-only DACL via `SECURITY_ATTRIBUTES` (where
  `windows-sys` becomes justified) plus client-side
  `GetNamedPipeServerProcessId` + owner-SID verification — is filed during
  PR2 and scheduled after the port.

## Path normalization (PR6)

New crate-root module `src/fspath.rs` (crate root because `src/sessions.rs` is
an engine and must not depend on daemon internals):

```rust
/// Comparison key: paths are EQUAL iff their keys are equal.
/// Windows: dunce::simplified() strips legality-checked verbatim prefixes
/// (\\?\C:\x → C:\x, \\?\UNC\s\sh → \\s\sh), '/' → '\', ASCII-lowercase.
/// Unix: identity — behavior unchanged.
pub fn compare_key(path: &Path) -> PathBuf;

/// Component-wise prefix test on compare keys.
pub fn starts_with_key(child: &Path, base: &Path) -> bool;

/// Display form for client-facing fields: canonicalized when possible,
/// verbatim prefix stripped, original case preserved.
pub fn display_path(path: &Path) -> PathBuf;
```

Exactly three application sites (engines keep storing verbatim paths; the wire
format and registries do not change):

1. `src/daemon/services/worktrees.rs::canonical()` splits into
   `canonical_key()` (`dunce::canonicalize` fallback-to-raw, then
   `compare_key`) for `open_window_index` / `worktree_entry` /
   `windows_with_path`, and `display_path()` for the client-facing
   `path`/`root` strings — stopping `\\?\C:\...` leaking into the tree view
   and round-tripping through `open`/`close`/`ahead-behind`.
2. `src/sessions.rs::resolve_source` — the `cwd.starts_with(f)` join becomes
   `starts_with_key(cwd, f)`.
3. `src/sessions.rs::focus_folder` — same substitution.

Documented tradeoffs: whole-path ASCII case-folding fixes the observed bug
class (`C:` vs `c:` drive-letter case between hook `cwd` and VS Code
`fsPath`); the cost — two paths differing only by case in a per-directory
case-sensitive NTFS tree falsely merge — is benign for grouping/joining where
today's false *split* is the bug. 8.3 short names resolve via
canonicalize-before-key where the path exists. `\\wsl$` / `\\wsl.localhost`
UNC keys never equal drive-letter keys — correct by design (different mount
views); WSL-side workflows keep using the daemon inside WSL. macOS
case-insensitivity is deliberately out of scope (Unix `compare_key` is the
identity). The normalization core operates on the path string with explicit
`\`-component handling so Windows-shaped fixtures
(`\\?\C:\Users\X`, `c:/users/x`, `\\wsl$\Ubuntu\home`) run as unit tests on
the ubuntu CI matrix; `dunce` is added for legality-checked de-verbatiming (a
naive prefix strip corrupts exactly the long/reserved-name paths for which
`canonicalize` produced the verbatim form).

## Phased PR roadmap

Sequencing constraint that sets the CI cut lines: `help_all_golden`
(`tests/integration_test.rs`) is not platform-gated and its committed snapshot
contains the four gated subcommands, so a full `cargo test` on Windows fails
until the CLI is un-gated. The Windows test job therefore starts scoped in PR2
and widens in PR3 — at which point the Unix-generated snapshot is unchanged
(the variants already exist on Unix) and the Windows help surface becomes
identical, so the golden test passes on both.

| PR  | Scope                                                              | Size | Depends on          |
|-----|--------------------------------------------------------------------|------|---------------------|
| PR1 | Transport seam, Unix-only, zero behavior change                    | M    | —                   |
| PR2 | Windows transport; un-gate daemon core; scoped Windows CI test job | L    | PR1                 |
| PR3 | CLI + MCP un-gating; widen Windows CI to the full test suite       | M    | PR2                 |
| PR4 | Windows launcher (detached spawn); full lifecycle on Windows CI    | S    | PR3                 |
| PR5 | Companion `win32` endpoint resolution                              | M    | PR2 (parallel PR3+) |
| PR6 | Path normalization (`src/fspath.rs` + `dunce`)                     | M    | PR2 (parallel PR3+) |
| PR7 | Docs/ADR sync; CI hardening notes                                  | S    | PR3–PR6             |

Critical path: PR1 → PR2 → PR3 → PR4 → PR7, with PR5 and PR6 in parallel after
PR2. Each PR's technical content is specified in the sections above (PR1–PR2:
transport seam; PR2/PR5: naming spec; PR2: single-instance and security; PR5:
companion; PR6: path normalization). Roadmap-only details:

- **PR1** touches no cfg gates and no snapshots: `transport.rs` (Unix halves),
  genericized framing helpers, `Listener` return types, `client.rs` /
  `testutil.rs` via `transport::*`.
- **PR2** un-gates the daemon core in `src/daemon.rs` (launchd/systemd/tray
  stay `target_os`-gated), adds `tests/daemon_pipe.rs` (`#![cfg(windows)]`,
  mirroring the `daemon_socket.rs` in-process harness), adds the scoped
  `windows-test` CI job (`cargo test --lib --features mcp` +
  `--test daemon_pipe`, stable toolchain only), audits `services/` for
  unix-isms, and files the DACL-hardening follow-up issue.
- **PR3** un-gates `src/cli.rs` (modules, `Commands` variants, dispatch arms)
  and `src/mcp.rs::snowflake_tools`; the sessions hook install un-gates as-is.
  Ships a temporary `#[cfg(windows)]` `daemon start` bail — "run
  `omni-dev daemon run` in a terminal (#1363)" — removed in PR4. Test triage
  is per-test `#[cfg(unix)]`, never whole files when avoidable;
  `daemon_socket.rs` stays `#![cfg(unix)]` (umask/SIGHUP semantics).
- **PR4** adds the `spawn_detached` Windows arm via
  `creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)` — not
  `CREATE_NO_WINDOW`, whose console semantics are mutually exclusive with
  `DETACHED_PROCESS` — with stdio appended to `daemon.log`; no `setsid`
  analog is needed (Windows children survive parent exit). `stop` /
  `restart` / `status` work as-is (no re-arming socket exists). Enables
  `tests/daemon_test.rs` (real-binary spawn) on the Windows job.
- **PR5** ships through the extension's own release train: `package.json`
  bump, `editors/vscode/CHANGELOG.md` entry, `vscode-v*` tag. An optional
  `windows-latest` smoke leg in `vscode-extension.yml` is cheap but not
  required (tests are pure Node with injected platform).
- **PR7** amends ADR-0039's Status per its inline convention (plus a short
  Windows paragraph), repoints the ADR-0040/0048/0052 platform notes to
  ADR-0054, flips this document's Status, updates the operator docs
  (`docs/worktrees-service.md`, `docs/sessions-service.md`,
  `docs/snowflake-service.md`, READMEs), and keeps the Windows matrix
  stable-only to cap CI cost.
