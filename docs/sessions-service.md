# Sessions service

Track, for the logged-in user and across **every** terminal and VS Code window,
the Claude Code sessions running right now and each one's coarse live state
(working, idle, or waiting on you). It is the omni-dev daemon's **fourth
service** (after the browser bridge, Snowflake, and worktrees), fed by four
independent sources that each degrade gracefully.

This guide is the operator-facing contract. The design rationale is
[ADR-0052](adrs/adr-0052.md) — plus [ADR-0057](adrs/adr-0057.md) for the stream
wrapper (Feed 4); the daemon framework is [ADR-0039](adrs/adr-0039.md) and the
rendezvous pattern it reuses is [ADR-0040](adrs/adr-0040.md).

> **Distinct from history search (#876).** That searches your *past*
> conversations under `~/.claude/projects`; this tracks *currently-running*
> sessions and their live state. Both watch the same directory.

## Why a resident service

No single vantage point sees all your sessions:

- A **hook** runs inside one `claude` process and knows only that session.
- A **VS Code window** is sandboxed per extension host — it sees only its own
  tabs/terminals, never a sibling window's.
- The **transcript files** are machine-wide but carry no live state on their own.
- A **stream wrapper** sees one Claude process exactly, and only the ones it was
  configured to launch.

A single resident process — the daemon — is the rendezvous point that aggregates
all four into one consistent view served back to the CLI, the tray, and the
extension.

## Architecture

```
  ┌─ Feed 1: Claude Code hooks ─────────►  omni-dev sessions hook ─┐
  │   (SessionStart/Stop/Notification/…)    (reads hook JSON on     │
  │   installed in ~/.claude/settings.json   stdin, POSTs to socket)│
  │                                                                 ▼
  ├─ Feed 2: transcript watcher ────────►  daemon `sessions` service
  │   ~/.claude/projects/<enc-cwd>/           (in-memory SessionsRegistry,
  │   <session-id>.jsonl (growth/mtime)        TTL reap-on-read, like worktrees)
  │                                                                 ▲
  ├─ Feed 3: companion VS Code extension ───────────────────────────┤
  │   (editors/vscode, extended: reports its window's Claude        │
  │    tab/terminal counts so the daemon can tag a session's source)│
  │                                                                 │
  └─ Feed 4: stream wrapper ────────────────────────────────────────┘
      omni-dev claude-wrap, launched by the Claude VS Code extension
      in place of `claude`; tees its stream-json stdio and reports the
      *exact* state (authoritative, unlike Feeds 1–3)

              daemon ──► `omni-dev sessions list` / tray submenu
                     ──► the companion's Worktrees tree cues
```

The **engine** ([`src/sessions.rs`](../src/sessions.rs), `SessionsRegistry`) is
pure in-memory state behind `std::sync::Mutex`es never held across an `.await`;
the **adapter** ([`src/daemon/services/sessions.rs`](../src/daemon/services/sessions.rs))
routes ops, enriches `repo` from `cwd` with `git2`, renders the tray/status, and
owns the transcript-watcher task — the same engine/adapter split as the worktrees
service.

### Data model

Each live session is:

```
session_id       the Claude UUID — also the transcript filename stem and the
                 VS Code extension's per-tab key, so the feeds join on it
cwd, repo        working directory (from a hook) and its git repo name (git2)
transcript_path  the ~/.claude/projects/**/<id>.jsonl path
state            starting | working | idle | waiting_for_input |
                 waiting_for_permission | ended
source           terminal | vscode (with the window's key)
last_event       the most recent sighting
started_at, last_seen, model
```

### State inference

For Feeds 1–3 state is **inferred** — Claude Code ships no dedicated
session-state event (anthropics/claude-code#43058, *not planned*), so this is
best-effort:

| Sighting | State |
|---|---|
| `SessionStart` | `starting` |
| `UserPromptSubmit` / `PreToolUse` / `PostToolUse` / transcript grew | `working` |
| `Stop` | `idle` |
| `Notification` — permission prompt | `waiting_for_permission` |
| `Notification` — idle/input prompt | `waiting_for_input` |
| `Notification` — unclassified / transcript discovered | *unchanged* |
| `SessionEnd` | `ended` (reaped shortly after) |
| **stream state** (Feed 4) | **exactly what was reported** |

`waiting_for_*` are **reliable** (a `Notification` hook fires them directly).
`working` vs `idle` is best-effort, with the transcript-growth backstop covering
the ~5–15s "thinking window" between a prompt and the first tool call, where no
hook fires.

Feed 4 is the exception: it reads the state out of Claude's own stream rather
than guessing from a lifecycle event, so it wins outright over anything inferred
before it. See [the stream wrapper](#the-stream-wrapper-feed-4).

### Liveness

Like worktrees: `last_seen` + TTL, reaped **inline on every read** — no background
task. The maps are capped (512 sessions, 256 window reports); at the cap a new
entry evicts the longest-silent one, so ingest never fails.

Sessions differ from windows in one way: a session emits nothing while idle at the
prompt, so its only liveness signal is activity. The session TTL is therefore
generous (5 min). **A session left idle longer than that ages out and re-appears
the moment it next does anything** — the accepted limitation of a hook-based
tracker, since no liveness event exists. A clean `SessionEnd` removes it promptly.

## CLI

```bash
# The live set of running sessions, as a table.
omni-dev sessions list

# Machine-readable JSON (byte-identical to the on-socket payload).
omni-dev sessions list -o json

# Against a non-default daemon socket.
omni-dev sessions list --socket /path/to/daemon.sock
```

`list` is a read-only client, Unix-only (`#[cfg(unix)]`), like `worktrees list`.

### Window feed ops (companion parity)

The companion `window` / `window-unregister` feed ops — normally spoken by the
VS Code extension — are exposed as typed commands so scripted/headless companions
and integration tests can report a window's Claude embedding the way the extension
does (#1361). Each takes a caller-supplied window `--key`:

```bash
# Report this window's Claude editor-tab and terminal counts (mirrors WindowReport).
omni-dev sessions window --key <KEY> [--folder /abs/path]... [--tabs N] [--terminals N]

# Remove the window's embedding report (fired on the companion's deactivate()).
omni-dev sessions window-unregister --key <KEY>   # prints whether an entry was removed
```

Both accept `--socket`. The daemon joins a session to a window by `cwd`, so the
`--folder` paths are what tag a session's `source` as `vscode`. The underlying ops
are documented in the [companion contract](#companion-contract-for-the-extension-and-other-clients).

### Installing the hooks (Feed 1)

```bash
# Merge the sessions-tracker hooks into ~/.claude/settings.json (idempotent;
# preserves any hooks already there). Honors $CLAUDE_CONFIG_DIR.
omni-dev sessions install-hooks

# Remove them again (leaves your other hooks untouched).
omni-dev sessions uninstall-hooks

# Point at a specific settings file.
omni-dev sessions install-hooks --settings /path/to/settings.json
```

`install-hooks` writes a `command` hook running `omni-dev sessions hook` for
`SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Notification`,
`Stop`, and `SessionEnd`. It uses the absolute path of the running binary so
Claude Code invokes *this* omni-dev regardless of its hook `PATH`. The portable
manual form is `omni-dev sessions hook`.

The `hook` subcommand is the **feed sink** — Claude Code runs it, not you. It
reads one hook event's JSON on stdin, maps it to an `observe`/`end` op, and
fire-and-forgets it to the socket. It is **infallible by design**: a missing
daemon, a malformed payload, or any other error is swallowed and it **always
exits 0**, so it can never block or fail a Claude turn.

Once installed, restart is not required — the next Claude turn starts reporting.

### The stream wrapper (Feed 4)

Feeds 1–3 watch a session from the outside and *infer* what it is doing. Feed 4
sits **inside** the stream Claude's VS Code extension already reads, so the state
it reports is the real one — permission prompts included, with no hooks needed.

```bash
# Install the shim and point VS Code's Claude extension at it (idempotent).
omni-dev sessions install-wrapper

# Remove the shim and clear the setting again.
omni-dev sessions uninstall-wrapper

# Point at a specific settings file / shim location.
omni-dev sessions install-wrapper --settings /path/to/settings.json --shim /path/to/shim
```

`install-wrapper` writes a `0700` shim next to the daemon socket
(`<data-dir>/omni-dev/claude-wrap`) that `exec`s the absolute
`omni-dev claude-wrap`, then sets `claudeCode.claudeProcessWrapper` in your VS
Code **user** settings to that path. The shim exists because the extension spawns
the configured wrapper directly — no shell, no argument splitting — so the setting
has to name a single executable file.

**Reload VS Code afterwards.** Only Claude tabs started after the setting is
applied are wrapped; a window reload covers them all at once. Nothing about how
you launch tabs changes.

If your `settings.json` contains comments or trailing commas (both legal in VS
Code, neither safely rewritable), the command says so and prints the exact line to
paste — the shim is written first, so the hint is ready to use.

The `claude-wrap` subcommand is the **wrapper** — the extension runs it, not you.
It forwards the child's stdio byte-for-byte, tees complete lines to a parser, and
exits with the child's own status. It is **fail-open by construction**: the byte
forwarding never waits on the parser or the daemon, over-long or unparseable lines
are simply not parsed, and a missing daemon is a silent no-op. The worst case is
losing state visibility, never Claude failing to launch. It **never logs or
persists conversation content** — only the state, `session_id`, `cwd` and model
leave the process.

It also re-reports the current state every 30s, so a wrapped session idle at the
prompt does **not** age out on the 5-minute TTL the way a hook-fed one does.

Coverage is the VS Code extension's Claude tabs. Terminal Claude
(`claudeCode.useTerminal`, or `claude` in any shell) is not stream-json and is not
wrapped — `claude-wrap` detects a terminal and gets out of the way entirely — so
those sessions keep Feeds 1–3 and their limits.

## Tray

The macOS menu bar gains a **"Claude Sessions"** submenu: one line per session
(`<name> <glyph> <state>`). A session embedded in a VS Code window is a clickable
`focus:` action that opens/focuses that window (reusing the worktrees launcher);
a terminal session — with no window to focus — is a plain status line.

## Status

`omni-dev daemon status` includes a `sessions` row with a one-line summary
(`N session(s): X working, Y waiting, Z idle`) and, under `--json`, the full live
set.

## Source tagging (companion)

A session's `source` is resolved on read by joining its `cwd` against the live
window reports from the companion extension: a `cwd` under a window that reports
≥1 Claude tab/terminal is tagged **`vscode`** (with that window's key); everything
else is **`terminal`** — meaning "not matched to a reporting VS Code window" (a
bare terminal session, *or* a VS Code session whose companion is not installed).

The join is at the **session level** (by `cwd`), not the tab level: the Claude
extension exposes no API to bind a specific tab to a session, so one Claude tab in
a window/cwd is unambiguous, but several in the same cwd cannot be told apart.

## Security

**No new trust boundary** — the same posture as [ADR-0039](adrs/adr-0039.md) and
[ADR-0040](adrs/adr-0040.md):

- Ops ride the daemon's existing `0600` Unix socket in its `0700` directory.
- **No secret is persisted** — the registry is in-memory only.
- Residual exposure, stated plainly: anything that can read the socket can
  enumerate your open session **cwds/repos and coarse state**; anything that can
  write it can inject fake sessions — but both already require being the owning
  local user.
- Hooks are **opt-in** user config; `sessions hook` writes nothing except the
  fire-and-forget socket POST.
- The stream wrapper is **opt-in** too, and is the one component that *sees* your
  conversation as it streams. It extracts only the state, `session_id`, `cwd` and
  model, and logs and persists nothing — a design constraint, not a convention
  ([ADR-0057](adrs/adr-0057.md)). The only thing it writes is the same
  fire-and-forget socket POST.

This does not touch the browser-bridge ([ADR-0036](adrs/adr-0036.md)) or Snowflake
trust models.

## Companion contract (for the extension and other clients)

The companion speaks three additional ops to the same socket the worktrees service
uses (`DaemonEnvelope { service: "sessions", op, payload }`, newline-delimited
JSON):

| Op | Payload | Reply | Meaning |
|---|---|---|---|
| `window` | `{ key, folders[], tabs, terminals }` | `{ ok: true }` | Report this window's Claude embedding counts + folders; refreshes the report's liveness (a 30s TTL, so ride it every ~10s). |
| `window-unregister` | `{ key }` | `{ removed: bool }` | The window closed (fired on `deactivate()`). |
| `list` | *(none)* | `{ sessions: [...] }` | The live set, for the Worktrees tree's per-worktree cues. Read-only; the companion tallies sessions onto rows by `cwd`. |

`key` is the **same per-window UUID** the companion already uses for the worktrees
`register` op, so the two services agree on window identity. The companion reports
only *counts* of Claude tabs (webview `viewType` containing `claudeVSCodePanel`)
and terminals (named like Claude Code, honoring `$CLAUDE_CODE_TERMINAL_TITLE`) —
never a tab's `session_id`, which VS Code does not expose. New optional fields
follow the protocol's `#[serde(default, skip_serializing_if = …)]` convention, so
older and newer peers stay wire-compatible.

The hook `observe`/`end` ops (for reference; the sink builds these, not you):

| Op | Payload | Reply |
|---|---|---|
| `observe` | `{ session_id, cwd?, transcript_path?, event, model? }` | `{ ok: true }` |
| `end` | `{ session_id, reason? }` | `{ ended: bool }` |

where `event` is one of `session_start`, `user_prompt_submit`, `pre_tool_use`,
`post_tool_use`, `stop`, `{ "notification": "permission_prompt" \| "idle_prompt" \|
"agent_needs_input" \| "other" }`, `transcript_grew`, `transcript_discovered`, or
`{ "stream_state": "<state>" }` — the authoritative Feed 4 form, applied verbatim
rather than inferred.

## Scope and follow-ups

- **Idle-session liveness.** Without a dedicated event, idle sessions age out on
  the TTL — for the feeds that lack one. A wrapped session (Feed 4) heartbeats
  itself; a future refinement could keep the rest alive off their window's
  heartbeat.
- **Per-tab attribution** stays heuristic until (if ever) the Claude extension
  exposes a tab↔session API. Note the wrapper does *not* fix this: it knows its
  own session exactly, but still cannot say which tab is showing it.
- **Terminal Claude is unwrapped.** Feed 4 covers the VS Code extension's
  stream-json tabs only; a TUI `claude` keeps the inferred feeds. Wrapping it
  would mean interposing on an interactive terminal, which is a different and much
  riskier proposition.
- **Only new tabs are wrapped.** Coverage is prospective — sessions already
  running when the setting is applied keep the inferred feeds until they restart.
- **Windows** support waits on the broader daemon Windows work (#1363); the hook
  sink and transcript scheme are already portable, only the socket transport is
  Unix-only.
