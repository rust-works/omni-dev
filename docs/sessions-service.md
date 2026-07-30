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
                          (pushed live over `subscribe`)
```

The **engine** ([`src/sessions.rs`](../src/sessions.rs), `SessionsRegistry`) is
pure in-memory state behind `std::sync::Mutex`es never held across an `.await`;
the **adapter** ([`src/daemon/services/sessions.rs`](../src/daemon/services/sessions.rs))
routes ops, enriches `repo` from `cwd` with `git2`, renders the tray/status, and
owns the transcript-watcher task — the same engine/adapter split as the worktrees
service.

### Change-notify and the push stream

State reaches every open window over the **`subscribe`** op rather than a
per-window poll (#1414), mirroring the worktrees stream ([ADR-0048](adrs/adr-0048.md)):
the registry holds a `tokio::sync::watch` counter that a mutation bumps, and the
server's `run_stream` loop re-snapshots on each bump (plus its own
`OMNI_DEV_DAEMON_STREAM_TICK` re-sample, default 10 s) and pushes only a real
delta. Without it two windows showing the same worktree row could disagree about
its cue for a full poll period, with which one is stale set by whenever each
window happened to activate.

The stream needs **no coalescing snapshot cache** (the worktrees `TreeSnapshotCache`,
#1303): `repo` is enriched at `observe` time, so `list` is pure formatting and
`snapshot()` simply calls it — a one-shot `list` and a pushed frame are the same
bytes, which is what lets a client treat them interchangeably.

A registry mutation bumps **only when it changes something a consumer renders** —
a session appearing or ending, a `SessionState` transition, a best-effort field
taking a new value, or a window report that alters the `Source` join. It
deliberately does *not* bump on the `last_seen`/`last_event` churn every hook
event produces, nor on the unchanged ~10 s `window` refresh each open window
sends; those would push a fresh snapshot to every window several times a second
with the server's diff unable to suppress any of it. Their deltas ride the
periodic re-sample instead. TTL-driven transitions (an idle session ageing out)
fire no event at all and are likewise caught by that re-sample.

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
| transcript grew — while already `waiting_for_*` or `ended` | *unchanged* |
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

That difference sets the precedence, which is why growth is the one sighting with
exceptions: an inference never overwrites a state a hook reported directly. Claude
flushes the assistant `tool_use` line to the transcript **before** the prompt it is
asking about can be answered, and a session's last lines land around `SessionEnd`,
so in both cases growth is evidence the file grew, not that a turn is running.
Reading it as `working` would turn a waiting row green for the whole wait — exactly
when it should be shouting — and revive an exited session as a phantom `working` row
for the rest of the session TTL.

Neither state can strand, but the release has latency worth knowing: it is the
**next hook**, and no hook fires at the moment you answer a permission prompt (the
prompt comes after `PreToolUse`), so the next one is the `PostToolUse` that fires
when the approved tool *finishes*. On a hooks-only install a row therefore stays
amber for the duration of a long approved tool — a build, a test run — which is the
deliberate trade: a stale "blocked on you" is a nag you can see, a stale "working"
is the alert you never got. Feed 4 has no such gap, reporting `working` off the
`control_response` the moment the prompt is answered. Failing both,
`Stop` / `UserPromptSubmit` / `SessionEnd` or the TTL releases it.

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

**Terminal-tab titles carry the model, colour-coded** (issue #1445). VS Code's
terminal API only lets the *creating* extension set a tab's icon/colour, and only
at creation time, so there is no way to recolour an already-open tab — the wrapper
instead rewrites Claude's own OSC title sequence in flight, prepending a
colour-circle emoji and family name: 🟠 Fable, 🟡 Opus, 🟢 Sonnet, 🔵 Haiku, or
⚪ Claude for anything else. Claude asserts its title once, at startup, before its
model can possibly be known yet, so the tab briefly shows the undecorated title
and then corrects itself a moment later — no need to interact with the session
first. It updates live again if the model changes mid-session (`/model`). Set
`OMNI_DEV_CLAUDE_WRAP_NO_TITLE_REWRITE=1` to disable this and forward Claude's
title unchanged, if it ever misrenders in a given terminal or font. See the
[ADR-0057](adrs/adr-0057.md) amendment for how this stays fail-open.

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

- Ops ride the daemon's existing `0600` Unix socket in its `0700` directory. The
  `subscribe` stream (#1414) adds no capability: it is read-only and carries
  exactly what `list` already serves, just pushed rather than polled.
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

The companion speaks four additional ops to the same socket the worktrees service
uses (`DaemonEnvelope { service: "sessions", op, payload }`, newline-delimited
JSON):

| Op | Payload | Reply | Meaning |
|---|---|---|---|
| `window` | `{ key, folders[], tabs, terminals }` | `{ ok: true }` | Report this window's Claude embedding counts + folders; refreshes the report's liveness (a 30s TTL, so ride it every ~10s). |
| `window-unregister` | `{ key }` | `{ removed: bool }` | The window closed (fired on `deactivate()`). |
| `list` | *(none)* | `{ sessions: [...] }` | The live set, for the Worktrees tree's per-worktree cues. Read-only; the companion tallies sessions onto rows by `cwd`. |
| `subscribe` | *(none)* | `{ sessions: [...] }`, **repeatedly** | The same body as `list`, pushed on every real change (#1414). Read-only. |

`subscribe` takes over the connection for its lifetime: the daemon sends an
initial snapshot, then a fresh one on each change (and on its own periodic
re-sample) until the client writes any further line — which is read as a cancel —
or closes the socket. Prefer it over polling `list`: it is what keeps every open
window's cues in step instead of up to a poll period apart.

**Falling back.** The extension and daemon version independently, so handle a
daemon that predates the op: it replies `{ ok: false, error: "unknown sessions op:
subscribe" }` **and keeps the connection open**, so a client that only waits for
frames waits forever. Treat any non-`ok` reply on a subscription as "this daemon
cannot stream", stop reconnecting (a retry earns the same refusal), and poll
`list` instead. The companion re-attempts the subscription when its worktrees
stream reconnects, since a daemon upgrade lands as a restart.

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
