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

- `src/worktrees.rs` — the `WorktreesRegistry` engine: the in-memory `HashMap`
  of open windows behind a `std::sync::Mutex` (never held across an `.await`),
  the TTL reaping and entry cap/eviction, and the
  register/heartbeat/unregister/list/first-folder operations. A standalone
  `crate::worktrees` module, matching the browser bridge (`src/browser/`) and
  Snowflake (`src/snowflake/`) engine/adapter split.
- `src/daemon/services/worktrees.rs` — `WorktreesService`, a thin `DaemonService`
  adapter over that engine: it routes control-socket ops to the registry,
  renders the tray menu/status, and drives the VS Code launcher. Cheap to
  construct; persists nothing.
- `src/cli/worktrees.rs` — the read-only `omni-dev worktrees list` client.
- The companion VS Code extension in [`editors/vscode/`](../editors/vscode/) —
  the **writer**: it `register`s on activation, `heartbeat`s every ~10 s, and
  `unregister`s on deactivation, talking to the daemon socket directly from each
  window.

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
omni-dev worktrees list -o json

# Against a non-default daemon socket.
omni-dev worktrees list --socket /path/to/daemon.sock
```

Each row shows the window's repo, its **current branch** and **ahead/behind sync
state** (`+ahead -behind`, or `-` when the branch tracks no upstream), the
primary folder, and how long ago the window was last seen. The branch and sync
columns are computed by the daemon from the worktree on disk — see
[Git enrichment](#git-enrichment) — so they reflect the live branch rather than
whatever the companion happened to report. `-o json` carries the same fields plus
the companion-reported `title`.

The companion extension feeds the registry; the CLI only reads it. If the daemon
is not running, `worktrees list` reports the connection failure (the companion, by
contrast, no-ops silently).

## Tray

On a macOS `menu-bar` build the service contributes a **"Worktrees" submenu**:
one line per open window, then a **"Focus …" action** per window. Each window's
line shows its name and, when the primary folder is a git worktree, the live
branch and ahead/behind state (`repo · branch (+2 -1)`); windows that are not a
repo fall back to their reported title. Clicking a "Focus" action spawns the VS
Code CLI on that window's folder; since VS Code reuses an already-open window,
this focuses the right window rather than opening a new one.

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

The `-o json` status detail carries the same enriched window entries as
`worktrees list -o json`.

## Git enrichment

The companion reports only raw folder paths; the **daemon** computes the richer
per-worktree git state — current branch and ahead/behind counts — with the
`git2` dependency it already carries (#1186). Keeping this in Rust preserves the
companion's thin-reporter contract ([ADR-0040](adrs/adr-0040.md)): the ~50-line
extension never runs git.

- **Computed on read.** Enrichment happens each time the registry is read
  (`list`, `status`, and the tray menu), from the worktree on disk, so the branch
  shown is always current — not a snapshot frozen at registration. `list` and
  `status` run it on a blocking thread (`git2` is synchronous disk I/O) and never
  under the registry lock.
- **Primary folder only.** Only the first workspace folder of each window is
  enriched — it is the one the table shows and the "Focus" action opens.
- **Best-effort and degrading.** Discovery tolerates a folder inside a
  subdirectory or a linked worktree. A folder that is not a git repo, a detached
  HEAD, or a branch with no upstream is still listed — just without the fields it
  cannot supply (no `branch`, or `branch` with no `ahead`/`behind`). The
  enrichment never fails a `list`.

## Security

**No new trust boundary** ([ADR-0040](adrs/adr-0040.md)). Requests ride the
daemon's existing `0600` Unix control socket in its `0700` directory; no secret is
persisted; everything is loopback/filesystem-local. The residual exposure is
bounded by socket ownership — reading the socket reveals your open repo *paths*,
and writing it (already requiring the owning local user) could inject entries or
trigger a focus. The focus action additionally requires the target to be an
existing absolute directory before spawning `code`. Registry strings
(`repo`/`folders`, and the companion `title`) are writer-influenced metadata, so
the `worktrees list` table strips control characters (C0, DEL, C1) from the
strings it renders before writing to the terminal — a registered entry cannot
inject ANSI escape sequences into the operator's TTY (#1137). The daemon-computed
`branch` is a git ref name (which cannot contain control bytes) but is sanitized
on the same path as defense-in-depth. Native tray menus do not interpret ANSI,
and the `-o json` output escapes control bytes via JSON encoding.

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
- A `list` `entry` is
  `{ key, folders[], repo?, title?, pid?, branch?, ahead?, behind?, last_seen }`
  with `last_seen` as an RFC 3339 timestamp; consumers compute age from it.
  Entries are sorted by `(repo, key)` for deterministic output. The
  companion-reported fields are stored and served verbatim on the wire (and in
  `-o json`); only the human-readable `worktrees list` table sanitizes them for
  terminal display (see Security).
- `branch`, `ahead`, `behind` are **daemon-computed, not companion-reported**:
  the daemon derives them from the primary folder's git state on every read (see
  [Git enrichment](#git-enrichment)). Each is optional and omitted when it does
  not apply — no `branch` for a non-repo or detached HEAD; no `ahead`/`behind`
  when the branch tracks no upstream. New optional fields like these follow the
  protocol's `#[serde(skip_serializing_if)]` convention, so an older client
  simply ignores them.

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
← {"ok":true,"payload":{"windows":[{"key":"3f1c…","folders":["/home/me/omni-dev"],"repo":"omni-dev","title":"omni-dev — main","pid":4321,"branch":"main","ahead":2,"behind":0,"last_seen":"2026-06-23T01:20:00Z"}]}}
```

The companion sends no `branch`/`ahead`/`behind` on `register`; the daemon adds
them to `list`/`status` replies.

## Scope and follow-ups

- The companion extension lives in [`editors/vscode/`](../editors/vscode/): a
  small TypeScript reporter that speaks the contract above, bundled with esbuild
  and packaged as a `.vsix` by its own path-filtered CI workflow. Publishing to
  the VS Code Marketplace / Open VSX is a follow-up (it needs a publisher account
  and CI secrets); until then, install the `.vsix` built by CI with
  `code --install-extension`.
- Git enrichment lives in Rust (#1186): the companion reports raw folder paths
  and the daemon computes per-worktree branch and ahead/behind state with `git2`
  (see [Git enrichment](#git-enrichment)), keeping the companion thin.
- The service and CLI are Unix-only (`#[cfg(unix)]`), like the rest of the daemon;
  Windows support is tracked with the broader daemon work (#1041).
