# Sessions service

Track, for the logged-in user and across **every** terminal and VS Code window,
the Claude Code sessions running right now and each one's coarse live state
(working, idle, or waiting on you). It is the omni-dev daemon's **fourth
service** (after the browser bridge, Snowflake, and worktrees), fed by three
independent sources that each degrade gracefully.

This guide is the operator-facing contract. The design rationale is
[ADR-0052](adrs/adr-0052.md); the daemon framework is [ADR-0039](adrs/adr-0039.md)
and the rendezvous pattern it reuses is [ADR-0040](adrs/adr-0040.md).

> **Distinct from history search (#876).** That searches your *past*
> conversations under `~/.claude/projects`; this tracks *currently-running*
> sessions and their live state. Both watch the same directory.

## Why a resident service

No single vantage point sees all your sessions:

- A **hook** runs inside one `claude` process and knows only that session.
- A **VS Code window** is sandboxed per extension host — it sees only its own
  tabs/terminals, never a sibling window's.
- The **transcript files** are machine-wide but carry no live state on their own.

A single resident process — the daemon — is the rendezvous point that aggregates
all three into one consistent view served back to the CLI, the tray, and the
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
  └─ Feed 3: companion VS Code extension ───────────────────────────┘
      (editors/vscode, extended: reports its window's Claude
       tab/terminal counts so the daemon can tag a session's source)

              daemon ──► `omni-dev sessions list` / tray submenu
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

State is **inferred** — Claude Code ships no dedicated session-state event
(anthropics/claude-code#43058, *not planned*), so this is best-effort:

| Sighting | State |
|---|---|
| `SessionStart` | `starting` |
| `UserPromptSubmit` / `PreToolUse` / `PostToolUse` / transcript grew | `working` |
| `Stop` | `idle` |
| `Notification` — permission prompt | `waiting_for_permission` |
| `Notification` — idle/input prompt | `waiting_for_input` |
| `Notification` — unclassified / transcript discovered | *unchanged* |
| `SessionEnd` | `ended` (reaped shortly after) |

`waiting_for_*` are **reliable** (a `Notification` hook fires them directly).
`working` vs `idle` is best-effort, with the transcript-growth backstop covering
the ~5–15s "thinking window" between a prompt and the first tool call, where no
hook fires.

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

This does not touch the browser-bridge ([ADR-0036](adrs/adr-0036.md)) or Snowflake
trust models.

## Companion contract (for the extension and other clients)

The companion speaks two additional ops to the same socket the worktrees service
uses (`DaemonEnvelope { service: "sessions", op, payload }`, newline-delimited
JSON):

| Op | Payload | Reply | Meaning |
|---|---|---|---|
| `window` | `{ key, folders[], tabs, terminals }` | `{ ok: true }` | Report this window's Claude embedding counts + folders; refreshes the report's liveness (a 30s TTL, so ride it every ~10s). |
| `window-unregister` | `{ key }` | `{ removed: bool }` | The window closed (fired on `deactivate()`). |

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
"agent_needs_input" \| "other" }`, `transcript_grew`, or `transcript_discovered`.

## Scope and follow-ups

- **Idle-session liveness.** Without a dedicated event, idle sessions age out on
  the TTL. A future refinement could keep a VS Code-embedded session alive off its
  window's heartbeat.
- **Per-tab attribution** stays heuristic until (if ever) the Claude extension
  exposes a tab↔session API.
- **Windows** support waits on the broader daemon Windows work (#1237); the hook
  sink and transcript scheme are already portable, only the socket transport is
  Unix-only.
