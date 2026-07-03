# Worktrees service

The omni-dev daemon hosts a **worktrees service**: it maintains the live,
authoritative set of repositories and git worktrees open across **every** VS Code
window, fed by a small first-party companion extension that reports from each
window. It is the daemon's third service, after the browser bridge and Snowflake.

## Why a resident service

A VS Code extension host is **sandboxed per window**: each window's extension can
read only its own `workspace.workspaceFolders`, never a sibling window's. So no
extension can show, on its own, "which worktrees are open across all my windows".
Community cross-repo views (e.g. *Git Worktree Manager*) store a per-window
curated list that does not replicate between windows, so it has to be re-curated
by hand everywhere.

The only architecture that beats the sandbox is a **rendezvous point**: a single
resident process each window reports its own worktree to, which aggregates them
into one consistent view served back to every window, the CLI, and the tray. The
daemon already is that process, and — unlike a flat shared file — it can **age out
dead windows** (a window that crashed without unregistering), which is what makes
the view correct over time. See [ADR-0040](adrs/adr-0040.md).

## Architecture

- `src/daemon/services/worktrees.rs` — `WorktreesService`, a thin `DaemonService`
  adapter holding an in-memory `HashMap` of open windows behind a
  `std::sync::Mutex` (never held across an `.await`). Cheap to construct; persists
  nothing.
- `src/cli/worktrees.rs` — the read-only `omni-dev worktrees list` client.
- The companion VS Code extension (separate deliverable) — the **writer**: it
  `register`s on activation, `heartbeat`s every ~10 s, and `unregister`s on
  deactivation, talking to the daemon socket directly from each window.

### Data flow

```
VS Code window A ─┐
VS Code window B ─┼─►  omni-dev daemon (worktrees service)  ──►  CLI / tray / extension UI
VS Code window C ─┘     live registry, keyed by per-window key
```

### Liveness

Each entry carries a `last_seen` timestamp, refreshed by `register`/`heartbeat`.
An entry is evicted once it has been silent longer than the **30 s TTL** (three
missed ~10 s heartbeats). Reaping runs inline on every read — there is no
background task — so a window that crashed without a clean `unregister`
disappears the next time anything reads the registry.

Because the registry is in-memory, a window that was open *before* the daemon
started, or that survives a daemon restart, will heartbeat against an empty map.
The daemon answers `{ known: false }`, which is the companion's signal to
re-`register`. No state is persisted to make this work.

The registry is also **capped at 256 windows** (#1140): where the TTL bounds how
*stale* an entry can get, the cap bounds how *many* can exist, so a misbehaving
client flooding `register` with distinct keys cannot grow daemon memory faster
than the TTL reaps it. A `register` that would exceed the cap evicts the
longest-silent entry instead of failing; an evicted live window comes back
through the normal `{ known: false }` heartbeat path within ~10 s.

## CLI

```bash
# The live cross-window set of open worktrees/repos, as a table.
omni-dev worktrees list

# Machine-readable JSON (byte-identical to the on-socket payload).
omni-dev worktrees list --json

# Against a non-default daemon socket.
omni-dev worktrees list --socket /path/to/daemon.sock
```

The companion extension feeds the registry; the CLI only reads it. If the daemon
is not running, `worktrees list` reports the connection failure (the companion, by
contrast, no-ops silently).

## Tray

On a macOS `menu-bar` build the service contributes a **"Worktrees" submenu**:
one line per open window, then a **"Focus …" action** per window. Clicking it
spawns the VS Code CLI on that window's folder; since VS Code reuses an
already-open window, this focuses the right window rather than opening a new one.

Focusing is **best-effort**. The launcher is resolved in this order:

1. `OMNI_DEV_VSCODE_BIN` (set this if your daemon runs under launchd with a
   minimal `PATH` and cannot find `code`);
2. well-known absolute locations (`/usr/local/bin/code`, `/opt/homebrew/bin/code`,
   the in-app `.../Visual Studio Code.app/.../bin/code`, `/usr/bin/code`);
3. bare `code` resolved via `PATH`.

If none works, the failure is logged and the rest of the tray keeps working.

## Status

`omni-dev daemon status` includes the service:

```text
daemon: running
  worktrees        ok         3 window(s) across 2 repo(s)
```

## Security

**No new trust boundary** ([ADR-0040](adrs/adr-0040.md)). Requests ride the
daemon's existing `0600` Unix control socket in its `0700` directory; no secret is
persisted; everything is loopback/filesystem-local. The residual exposure is
bounded by socket ownership — reading the socket reveals your open repo *paths*,
and writing it (already requiring the owning local user) could inject entries or
trigger a focus. The focus action additionally requires the target to be an
existing absolute directory before spawning `code`. Registry strings
(`title`/`repo`/`folders`) are writer-influenced metadata, so the `worktrees
list` table strips control characters (C0, DEL, C1) before rendering to the
terminal — a registered entry cannot inject ANSI escape sequences into the
operator's TTY (#1137). Native tray menus do not interpret ANSI, and the
`--json` output escapes control bytes via JSON encoding.

## Companion contract (for the extension and other clients)

The service is reachable directly over the daemon's Unix control socket
(newline-delimited JSON), which is how the companion talks to it.

- **Socket:** `<data_dir>/omni-dev/daemon.sock` (`dirs::data_dir()`; on macOS
  `~/Library/Application Support/omni-dev/daemon.sock`, on Linux
  `${XDG_DATA_HOME:-~/.local/share}/omni-dev/daemon.sock`), mode `0600` in a
  `0700` directory. The companion computes this path the same way and **no-ops
  gracefully when the socket is absent** (daemon not running).
- **Request envelope:** one JSON line —
  `{ "service": "worktrees", "op": "<op>", "payload": <object> }`.
- **Reply:** one JSON line — `{ "ok": true, "payload": <object> }` or
  `{ "ok": false, "error": "<message>" }`.

Ops:

| op           | payload                                          | success payload                |
|--------------|--------------------------------------------------|--------------------------------|
| `register`   | `{ key, folders[], repo?, title?, pid? }`        | `{ ok: true }`                 |
| `heartbeat`  | `{ key }`                                         | `{ known: <bool> }`            |
| `unregister` | `{ key }`                                         | `{ removed: <bool> }`          |
| `list`       | `null`                                            | `{ windows: [entry, …] }`      |

Where:

- `key` — a stable per-window identifier the companion **generates once per
  `activate()`** (a UUID). The daemon does not derive identity from
  `vscode.env.sessionId`; report it (and `pid`) only as metadata.
- `register` never errors because of registry pressure: past the 256-entry cap
  it evicts the longest-silent entry rather than rejecting, so the companion
  needs no retry logic (an evicted window re-registers off its next heartbeat).
- `folders` — absolute workspace-folder paths.
- A `list` `entry` is `{ key, folders[], repo?, title?, pid?, last_seen }` with
  `last_seen` as an RFC 3339 timestamp; consumers compute age from it. Entries are
  sorted by `(repo, key)` for deterministic output. Fields are stored and served
  verbatim on the wire (and in `--json`); only the human-readable `worktrees list`
  table sanitizes them for terminal display (see Security).

Companion lifecycle, per window:

```text
activate():    connect(socket) → {service:"worktrees", op:"register",
                                   payload:{key, folders, repo, title, pid}}
heartbeat:     every ~10s → {op:"heartbeat", key}     // re-register if {known:false}
deactivate():  {op:"unregister", key}
```

Example exchange:

```text
→ {"service":"worktrees","op":"register","payload":{"key":"3f1c…","folders":["/home/me/omni-dev"],"repo":"omni-dev","title":"omni-dev — main","pid":4321}}
← {"ok":true,"payload":{"ok":true}}
→ {"service":"worktrees","op":"list"}
← {"ok":true,"payload":{"windows":[{"key":"3f1c…","folders":["/home/me/omni-dev"],"repo":"omni-dev","title":"omni-dev — main","pid":4321,"last_seen":"2026-06-23T01:20:00Z"}]}}
```

## Scope and follow-ups

- The companion extension is a separate deliverable (~50 lines): a minimal
  reporter that speaks the contract above.
- Git enrichment lives in Rust: the companion reports raw folder paths; richer
  per-worktree data (branch, ahead/behind) is a follow-up the daemon can compute
  with `git2`, tracked in #1186.
- The service and CLI are Unix-only (`#[cfg(unix)]`), like the rest of the daemon;
  Windows support is tracked with the broader daemon work (#1041).
