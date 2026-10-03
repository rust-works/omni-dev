# Sessions service

Track, for the logged-in user and across **every** terminal and VS Code window,
the Claude Code, [Codex](https://developers.openai.com/codex) and
[pi.dev](https://pi.dev) sessions running right now and each one's coarse live
state (working, idle, or waiting on you). It is the omni-dev daemon's **fourth
service** (after the browser bridge, Snowflake, and worktrees), fed by seven
independent sources that each degrade gracefully.

This guide is the operator-facing contract. The design rationale is
[ADR-0052](adrs/adr-0052.md) — plus [ADR-0057](adrs/adr-0057.md) for the stream
wrapper (Feed 4), [ADR-0087](adrs/adr-0087.md) for the Codex hooks (Feed 6) and
[ADR-0088](adrs/adr-0088.md) for the Codex wrapper (Feed 7);
the daemon framework is [ADR-0039](adrs/adr-0039.md) and the
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

- A **pi extension** runs inside one `pi` process and knows only that session.
- A **Codex hook** runs inside one Codex process (CLI, VS Code extension or
  Desktop) and knows only that session.

A single resident process — the daemon — is the rendezvous point that aggregates
all seven into one consistent view served back to the CLI, the tray, and the
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
  ├─ Feed 4: stream wrapper ────────────────────────────────────────┤
  │   omni-dev claude-wrap, launched by the Claude VS Code extension │
  │   in place of `claude`; tees its stream-json stdio and reports the│
  │   *exact* state (authoritative, unlike Feeds 1–3)               │
  │                                                                 │
  ├─ Feed 5: pi.dev extension ──────────────────────────────────────┤
  │   ~/.pi/agent/extensions/omni-dev-sessions.ts, loaded by every   │
  │   `pi`; maps pi's lifecycle events to the *exact* state (#1901)  │
  │                                                                 │
  └─ Feed 6: Codex hooks ──────►  omni-dev sessions hook --agent codex
      $CODEX_HOME/hooks.json; the Feed 1 sink with Codex's event
      mapping, tagging sessions `codex` (#1907)
  (Feed 7: omni-dev codex-wrap — polls a private Codex app-server
      for the *exact* state of the sessions omni-dev launches, #1910)

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
                 VS Code extension's per-tab key, so the feeds join on it.
                 Claude's ids are UUID v4; pi's and Codex's are UUID v7, whose
                 74 random bits make a collision improbable
agent            claude | pi | codex
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
| `PostToolUseFailure` / `PermissionDenied` / `ElicitationResult` / `SubagentStart` | `working` |
| transcript grew — while already `starting`, `waiting_for_*` or `ended` | *unchanged* |
| `Stop` / `StopFailure` | `idle` |
| `PermissionRequest` / `Notification` — permission prompt | `waiting_for_permission` |
| `Elicitation` / `Notification` — idle, input, or elicitation prompt | `waiting_for_input` |
| `Notification` — unclassified / `SubagentStop` / `PreCompact` / `PostCompact` / `SessionStart` with `source: "compact"` / transcript discovered | *unchanged* |
| `SessionEnd` | `ended` (reaped shortly after); ignored from a process a resume replaced — see [Liveness](#liveness) |
| **stream state** (Feed 4) | **exactly what was reported** |

`waiting_for_*` are **reliable**: a dedicated hook fires them directly.
`PermissionRequest` and `Elicitation` are events of their own. A `Notification`
is classified by Claude Code's `notification_type` field (`permission_prompt`,
`idle_prompt`, `elicitation_dialog`). Any other type, such as `auth_success`, is
unclassified. A substring match on the message is used only when there is no
type at all, as on older versions.
`StopFailure` covers a turn that ends on an API error, which fires no `Stop`.
Without it the row stayed `working` until the TTL expired.
`SubagentStop`, `PreCompact` and `PostCompact` refresh liveness without changing
the state. Each can fire while the session is idle: a background subagent can
finish after the turn's `Stop`, and a manual `/compact` can run from idle. No
event afterwards would release a `working`, so treating them as `working` would
leave an idle row showing `working`. A `SessionStart` with `source: "compact"` follows a compaction, which can run
mid-turn. It isn't a new session, so it doesn't reset the state to `starting`.
Claude subagent hooks carrying `agent_id` share the parent's `session_id`.
Their tool activity refreshes liveness without changing the parent's state
(#1926), so background tools after `Stop` leave the session idle. Permission
requests, classified notifications and elicitation still show waits. A tool
completion (including failure or permission denial), `ElicitationResult`, or
`SubagentStop` releases only that subagent's wait and restores the underlying
parent state once all subagent waits are resolved. Overlapping permission waits
have display priority over input waits. Parent hooks retain their existing wait
release behavior; passive sightings and transcript growth cannot clear waits.

`PostToolBatch` is not installed. Every tool
in a batch has already sent its own `PostToolUse` or `PostToolUseFailure`, so it
would only add a process spawn per batch.
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

`starting` is held for the same reason (#1946). A resumed session (a VS Code
window reload, `claude --resume`) keeps its session id, and the *old* process
appends a `cost-state` line to the shared transcript as it exits. The watcher
scans every 5s, so it usually sees that write after the new process's
`SessionStart`. No hook fires while an unprompted session sits idle, so reading
the write as `working` would keep the row busy until the next prompt. The first
prompt fires `UserPromptSubmit`, which releases the hold. A watcher-only session
never reaches `starting`, because `SessionStart` is a hook. Every consumer (the
tray summary, `worktrees ui`, the VS Code tree) counts `starting` as idle, because
a session that hasn't been prompted isn't busy.

Neither state can strand, but the release has latency worth knowing: it is the
**next hook**, and no hook fires at the moment you answer a permission prompt (the
prompt comes after `PreToolUse`), so the next one is the `PostToolUse` that fires
when the approved tool *finishes*. On a hooks-only install a row therefore stays
amber for the duration of a long approved tool — a build, a test run — which is the
deliberate trade: a stale "blocked on you" is a nag you can see, a stale "working"
is the alert you never got. Feed 4 has no such gap, reporting `working` off the
`control_response` the moment the prompt is answered. Failing both,
`Stop` / `UserPromptSubmit` / `SessionEnd` or the TTL releases it.

The newer events (#1915) **narrow** this gap without closing it. An approved tool
that *fails* now releases the wait at `PostToolUseFailure`. Before, it waited
for the next hook. The approval itself still fires no hook. Neither
does **denying** a prompt: the wait is released by whatever Claude does next
(`PreToolUse`, `Stop`, or your next `UserPromptSubmit`). `PermissionDenied` is not
that event. It fires only when **auto mode** refuses a call.

Feed 4 is the exception: it reads the state out of Claude's own stream rather
than guessing from a lifecycle event, so it wins outright over anything inferred
before it. See [the stream wrapper](#the-stream-wrapper-feed-4).

### Liveness

Like worktrees: a last-activity stamp + TTL, reaped **inline on every read** — no
background task. The maps are capped (512 sessions, 256 window reports); at the cap a new
entry evicts the longest-silent one, so ingest never fails.

Sessions differ from windows in one way: a session emits nothing while idle at the
prompt, so its only liveness signal used to be activity. The session TTL is
therefore generous (5 min), and **a session left idle longer than that ages out
and re-appears the moment it next does anything** — this remains the behavior for
a session the [pid liveness watcher](#pid-based-liveness-1916) below cannot vouch
for. A clean `SessionEnd` removes a session promptly regardless.

A resumed session is the exception to "`SessionEnd` removes it". Resuming in
place, as a VS Code window reload does, starts a new `claude` process on the same
`session_id` without waiting for the old one to exit. Each hook is its own
process, so nothing orders the old process's `SessionEnd` against the new one's
`SessionStart`, and it can arrive last (#1948). So every `observe` and `end` from
the hook sink carries its parent pid, which is the agent process, and so does
every report from [the stream wrapper](#the-stream-wrapper-feed-4), which sends
its child's pid. A sighting from a pid other than the session's current one takes
the session over and records the old pid as *replaced* (up to eight, oldest
forgotten first). An `end` from a replaced pid is ignored, so the resumed row
stays live. Any other `end` ends the session: one with no pid (an older sink, or
a feed that sends none), the owning pid, or a pid never seen. The last case
covers a hook command wrapped in a shell, where each hook's parent is a fresh
shell. That keeps the rule fail-open: it can only keep a session that has since
been taken over. A replaced pid never becomes the owner again, so a straggling
hook from the old process cannot take the session back.

The TTL is measured in **awake time** (#2108). While the machine sleeps nothing
refreshes a session or a window — no hooks, no Codex lock heartbeat, no pid
watcher — so measuring against the wall clock made the first read after a
wake-up find every entry stale and reap the lot. Each entry now carries, besides
the wall-clock `last_seen` shown in `list`, a stamp from a monotonic clock that
does not advance across system sleep (`Instant`: `CLOCK_UPTIME_RAW` on macOS,
`CLOCK_MONOTONIC` on Linux), and the TTLs compare against that. A session alive
at sleep survives until its feeds get a chance to report after the wake-up; a
dead one is still ended promptly by the lock probe or the pid watcher, or by the
TTL in awake time. The same applies to the 30 s window-report TTL, and to the
worktrees registry's own window TTL (#2126), which shares the clock
(`crate::utils::awake_clock`).

#### Pid-based liveness (#1916)

The 5-minute TTL exists only because a hook-fed session has no other liveness
signal — but the pid it already reports (above) *is* one. A dedicated
**pid liveness watcher**, an engine-owned background task alongside the
transcript and Codex rollout watchers, polls every pid-bearing session every
10 seconds and applies two decisions through the registry's ordinary `end` and
a new `confirm_pid_liveness`:

- **A pid this watcher once confirmed alive is now gone:** end the session
  immediately, through the same short ended-linger window a clean `SessionEnd`
  uses, instead of lingering for up to 5 minutes.
- **A pid is alive, its identity is confirmed, the session has had at least one
  `UserPromptSubmit`, it has never been reported by the
  [stream wrapper](#the-stream-wrapper-feed-4), and it is the most recently
  *active* `session_id` (by `last_seen`) among every candidate sharing that
  pid:** refresh `last_seen`,
  which is all it takes to keep the ordinary TTL reap from ever seeing the
  entry go stale.

The extra conditions on the second bullet close three ways a live pid could
still be the *wrong* reason to keep a row:

- **Prompted.** Otherwise a spare process VS Code keeps alive that is never
  prompted would pin a `starting` row forever just because its pid lives — the
  #1454-style pinning bug this feature must not reintroduce.
- **Not stream-wrapped (#1454).** `claude-wrap` is only attached to a VS Code/SDK
  `claude`, whose extension keeps a process per *chat* rather than per visible
  tab. A finished chat that was prompted keeps a live pid indefinitely, so on an
  install with both the hooks and the wrapper this watcher would pin it exactly as
  the wrapper's idle keep-alive used to. A session the wrapper has reported
  (`SessionEntry::streamed`, set by a Claude `StreamState` and cleared when a different
  process takes the session over) is
  therefore left to the TTL — the wrapper's own keep-alive holds its *busy* states
  — while a confirmed pid still *ending* it on exit is unchanged. Codex's wrapper
  and pi's extension are not affected. A terminal `claude` has no such
  per-chat-process behaviour and keeps the exemption.
- **Most recently active under the pid, by `last_seen` rather than creation
  order.** `/clear` (and possibly `/resume`) can start a new `session_id` in
  the *same* process without necessarily firing `SessionEnd` for the old one,
  so an abandoned `session_id` a newer one has since taken over falls back to
  ageing out normally, even though the process itself lives on. Keying this on
  `last_seen` rather than when each `session_id` was first created also means
  a later `/resume` of the older one correctly reclaims the exemption, since
  activity — not creation order — is what should decide which `session_id`
  under a pid is the one currently in use.

**Why a watcher, not an inline check.** An earlier version of this feature
checked pid liveness inline, in the same `reap_sessions` every `observe`/
`end`/`list` calls under the registry's lock. Review caught two problems with
that: the check can shell out (`ps` on macOS) and once a session is exempted it
never goes stale, so it would run *forever*, on every read, while holding the
lock — on a tokio worker thread and on the tray's macOS main thread. Polling
independently, off that lock, on its own schedule, fixes both: `reap_sessions`
stays pure TTL-only CPU work, and the daemon's other request handling is never
blocked by a pid check.

**Why the pid must be independently confirmed alive before its death is ever
trusted.** #1948 allows for a hook command wrapped in a shell, whose parent —
the pid a hook reports — is a fresh shell that exits within milliseconds of
the hook finishing. Trusting a bare "is `pid` gone right now" reading would end
such a session after every hook. Instead the watcher remembers, across polls,
which pids it has *itself* seen alive; a pid it never catches alive (near
guaranteed for a shell living only milliseconds, against a 10-second poll)
never triggers an end — it simply never enters pid-based liveness at all, and
the session ages out on the ordinary TTL exactly as before this feature
existed. A real `claude`/`codex` process, alive for the session's whole
lifetime, is caught alive on the watcher's very first poll.

**Why the identity token is never sent by the hook sink or `claude-wrap`.** The
token is an opaque, platform-specific string for a pid's start time
(`/proc/<pid>/stat`'s `starttime` field on Linux; `ps -o lstart=` on macOS —
never a parseable timestamp, only ever compared for equality), which
distinguishes a still-running process from the OS having recycled the same pid
number onto something unrelated. An earlier version had the hook sink and
`claude-wrap` read and send it. Review found two problems: reading it costs a
`ps` fork on macOS, adding latency to every hook against the sink's "never
blocks a turn" contract, and `ps -o lstart=` is locale/timezone dependent, so a
token read in the client's environment would almost never equal one read later
in the daemon's minimal service-manager environment — silently defeating the
whole feature. The watcher reads a pid's token itself instead, always in the
daemon's own environment, both when it first captures one and every time it
later compares one — so a client sends nothing but the bare `pid` it already
sent for #1948, and every comparison is apples to apples.

A session with no pid at all — an older sink, the transcript watcher, pi, or an
unsupported platform — gets none of this: pure pre-#1916 TTL aging, same as
before.

**Known limitation: a Linux subreaper can misattribute a pid.** `agent_pid`
(#1948) trusts the hook's parent pid outright, treating only pid 1 as an
orphan. Under a child subreaper (`systemd --user` is one) a `claude` that
dies mid-hook reparents the hook to the subreaper's own long-lived pid, not to
1, so the sink would report *that* pid instead. Since a subreaper is always
alive, the watcher would confirm it immediately and, if the session happens to
already be `prompted`, exempt it from the TTL indefinitely — the #1454-style
pinning this feature is otherwise built to avoid. This requires the exact
double fault of a subreaper *and* a crash mid-hook, and is not otherwise
mitigated today.

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
`Stop`, `SessionEnd`, `PermissionRequest`, `PermissionDenied`,
`PostToolUseFailure`, `StopFailure`, `Elicitation`, `ElicitationResult`,
`SubagentStart`, `SubagentStop`, `PreCompact`, and `PostCompact`. The tool events (`PreToolUse`, `PostToolUse`, `PostToolUseFailure`,
`PermissionRequest`, `PermissionDenied`) use the `*` matcher. It uses the absolute
path of the running binary so Claude Code invokes *this* omni-dev regardless of
its hook `PATH`. The portable manual form is `omni-dev sessions hook`.

Re-running `install-hooks` over an older install adds only the events it lacks,
and `uninstall-hooks` removes the sink from every event. Claude Code 2.1.280 drops
a hook event it doesn't know with a warning, not an error
(`Unknown hook event "…" was ignored`). How older versions treat an unknown event
hasn't been verified.

Because the command holds an absolute path, it goes stale when the binary moves (a
versioned install path, or a switch between cargo and Homebrew). Re-running
`install-hooks` from the new location rewrites every old-path sink **in place**
rather than adding a second one, and removes the duplicate that an install from
before #1927 may already have left beside it, so each event runs one sink.
`uninstall-hooks` from the new path likewise removes old-path sinks. Only an
**un-customised** sink counts as stale: an `omni-dev sessions hook` with no
arguments or only `--agent claude`. One with any other argument (a `--socket`, an
`--agent codex`) is a deliberate choice, so install and uninstall leave it alone
and print a note counting them; they keep running beside the canonical sink, so edit
them by hand if they point at a moved binary. A sink behind a wrapper (`FOO=1 …`,
`nice …`, `cd … &&`) is customised too.
This is narrower than Codex, where every `sessions hook` entry is rewritten (below).

The sink **never answers a `PermissionRequest`**. Claude Code reads that hook's
stdout as a decision, and a `"behavior": "allow"` would approve the tool call on
your behalf. The sink writes nothing to stdout and exits 0, so the prompt always
reaches you unchanged. `tests/sessions_hook_test.rs` pins this through the real
binary.

The `hook` subcommand is the **feed sink** — Claude Code runs it, not you. It
reads one hook event's JSON on stdin, maps it to an `observe`/`end` op, and
fire-and-forgets it to the socket, after appending it to the session's
[journal](#hook-journals-2108). It is **infallible by design**: a missing
daemon, a malformed payload, or any other error is swallowed and it **always
exits 0**, so it can never block or fail a Claude turn.

Once installed, restart is not required — the next Claude turn starts reporting.

### Hook journals (#2108)

The registry is in memory, and the hook sink is fail-open: it gives up after two
seconds when the daemon is unreachable. So an event fired while the daemon was
restarting, or before a cold socket-activated daemon answered, used to be lost,
and a restart forgot every session. The sink now **appends each event to a
per-session journal before it POSTs it**, and the daemon replays and tails those
journals.

**Hooks own the events; the daemon owns the state.** A hook never computes a
state. `SessionState::for_event` depends on the current state, subagent waits
roll up per session, a replaced process's stragglers are filtered (#1948), and
two hooks racing a read-modify-write would lose one. So a journal holds raw
events and the daemon runs them through the same state machine as a socket
event. The socket stays the fast path.

| Platform | Journal                                                                                    |
|----------|--------------------------------------------------------------------------------------------|
| macOS    | `~/Library/Application Support/omni-dev/sessions/<agent>/<session_id>.jsonl`               |
| Linux    | `$XDG_DATA_HOME/omni-dev/sessions/<agent>/<session_id>.jsonl` (default `~/.local/share/…`) |

The journals sit **beside the control socket**, so the hook and the daemon
resolve the same directory by construction, and a `--socket` override in the hook
command moves them with it. `<agent>` is `claude` or `codex`. Directories are
`0700` and files `0600`. `$XDG_STATE_HOME` and `$XDG_RUNTIME_DIR` are not used
(the first is often missing from the systemd user manager's environment, the
second has no macOS equivalent), nor are `~/Library/Caches` or `$TMPDIR`, which
the system may purge.

One record per line, appended with `O_APPEND` in a single write, so concurrent
hook processes need no lock:

```json
{"v":1,"ts":"2026-10-03T03:40:23.186123456Z","seq":"48213-1759462823186123456","agent":"codex","session_id":"…","op":"observe","event":"post_tool_use","pid":12345,"cwd":"…"}
{"v":1,"ts":"…","seq":"…","agent":"claude","session_id":"…","op":"end","reason":"clear","pid":12345}
```

A record holds only what the sink already sends over the socket: the ids, `cwd`,
the transcript *path*, the model id, the pid and the event kind. **Never** a
prompt, a tool input or any transcript content. The write is **fail-open** exactly
like the POST: an error is swallowed (logged at debug, never with the payload) and
never blocks a turn, changes the exit code or prints. `session_id` must be a
canonical hyphenated UUID and the agent a known hook agent before a path is built,
so a crafted payload cannot name a file outside the directory; an id that is not
a UUID is simply not journaled. `SessionEnd` is journaled too, so one that
arrives while the daemon is down is kept.

The sink cannot tell whether a daemon is reading, so it **bounds itself**: it does
not journal at all until the daemon's runtime directory exists (a machine that has
never run the daemon gets no journals), stops appending to a file past 512 KiB
(the daemon compacts long before that, so reaching it means nothing is tending
the journals), and, whenever it starts a new journal, deletes sibling journals
nobody has written to for 7 days. A hooks-only install with no sessions service
therefore cannot accumulate journals without limit.

The same `ts` and a per-event `seq` ride the POST as a `stamp`. The registry
remembers the last 32 `seq`s per session and drops the second copy of an event,
whichever route it arrived by. An event read from a journal is also dropped when
it is **older than one the session has already applied**, so a late read can
never undo a newer state, or end a session that has since been resumed. A
*socket* event is dropped as out of order only when it is behind the newest
applied event by less than 10 seconds: the adapter awaits a git lookup before it
applies an `observe`, so the journal can overtake it, but a wall clock stepped
back by more than that can never freeze a session.

**What the daemon does with them** (`src/sessions/journal_watcher.rs`, polling
every 5 s alongside the other watchers; a poll rather than a file watch, because
there is no notification dependency, it behaves the same on macOS and Linux, and
the POST remains the fast path):

- **First sight of a journal** (every file at startup, or one that appears later):
  replay it into a scratch registry to see what the state machine makes of it.
  A session that has **ended** is dropped, as is one whose last event is more
  than 7 days old. Otherwise it needs **proof of life**:
  - Codex: the thread's writer lock is **held**. A lock file nobody holds is proof
    of death; an *absent* one is not (a daemon started by a service manager may
    resolve a different `CODEX_HOME` from the shell's);
  - otherwise the owning pid is running **and started no later than its first
    journaled event** (a recycled pid started after it). A dead or unreadable pid
    is *not* proof of death, only the absence of proof, because it might have been
    a per-hook shell.
  
  A session with no proof is kept only if its last event is within the session
  TTL (5 min), which is what a hook-fed session with no pid always had.
- An **accepted** session is replayed in order with each event's own timestamp as
  its `last_seen`, so replayed history cannot look fresh. Its TTL clock starts
  now, in awake time, which holds it until the pid watcher or the Codex lock
  heartbeat confirms it.
- **Afterwards** only appended bytes are read (a partial last line waits for the
  next poll), so a dropped POST is caught within one interval.
- **Cleanup is the daemon's.** A rejected journal is deleted at once. An accepted
  one is deleted when the registry no longer holds its session live and the file
  has been quiet for 2 minutes, or when it is older than 7 days. A journal over
  128 KiB is compacted to its last ~64 KiB, cut at a line boundary, plus the
  newest prompt record the cut drops (so a long turn still replays as prompted),
  by writing a `0600` temp file and renaming it over; the rewrite is abandoned if the file
  grew since it was read, so the only events it can lose arrive in the instant
  between that check and the rename. Only regular `<uuid>.jsonl` files are read or
  removed; a symlink is never followed.

Known limits: a session with no usable pid and no Codex lock is restored only if
its last event is within the session TTL, so one left idle for longer than that
across a restart reappears on its next event. A hook appending in the instant
between a compaction's last check and its rename loses that one event (the POST has
normally delivered it). Feeds that are not hooks are **not**
journaled: the stream wrappers, the pi extension (its 30 s keep-alive already
re-reports after an outage), the transcript and rollout watchers, and the VS Code
window reports keep using the socket only.

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

It also re-reports a **busy** state (`working`, `waiting_for_*`) every 30s, so a
long silent turn or an unanswered permission prompt does not age out on the
5-minute TTL. An **idle** session is deliberately *not* re-reported (#1454): the
Claude VS Code extension keeps a `claude` process per chat in a window, not per
visible tab, so a never-prompted spare or a conversation finished hours ago stays
alive indefinitely, and pinning every such process would inflate a worktree's
session cue and `sessions list` — and only ever grow within a long-lived window.
Process lifetime is not tab visibility. The transition to idle is still reported
once, after which the session ages out on the TTL like a hook-fed one and
re-appears the moment it next does anything — which also means an idle chat
stays absent after a daemon restart (the registry is in-memory) until it is next
touched. The same reasoning excludes a session
the wrapper has reported from the [pid liveness watcher's](#pid-based-liveness-1916)
TTL exemption, though its death is still noticed promptly.

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

### The pi.dev extension (Feed 5)

[pi](https://pi.dev) has no hook-command block like Claude Code's
`settings.json`. Its extension point is a TypeScript module, auto-discovered from
`~/.pi/agent/extensions/` (or `$PI_CODING_AGENT_DIR/extensions/`) and loaded into
every `pi` process. `install-hooks` writes one there:

```bash
# Also writes ~/.pi/agent/extensions/omni-dev-sessions.ts, but only when pi is
# installed: its agent dir exists, or a `pi` executable is on PATH.
omni-dev sessions install-hooks

# Also removes that file (whether or not pi is still installed).
omni-dev sessions uninstall-hooks

# A non-default pi agent directory; passing it installs even if pi is not detected.
omni-dev sessions install-hooks --pi-agent-dir /path/to/agent
```

When pi is not detected, `install-hooks` says so and creates nothing under
`~/.pi`. The file carries a generated-by header. Install replaces only a file that
has that header, and refuses to overwrite a same-named one that lacks it.
Uninstall likewise removes only a file with the header. Other extensions in the
directory are never touched. The daemon socket path is baked into the file at
install time, just as the Claude hooks bake in the absolute binary path. Re-run
`install-hooks` if the socket moves. New `pi` sessions pick up the extension when
they start.

pi's events are first-class lifecycle events rather than side effects, so the
extension reports the **exact** state, as `{ "stream_state": … }` (the Feed 4
form), rather than having the daemon infer it:

| pi event | Reported |
|---|---|
| `session_start` | `idle` (pi's chat input is already live, so this is not `starting` — see below) |
| `before_agent_start`, `agent_start`, `turn_start`, `tool_execution_start` | `working` |
| `agent_settled` | `idle` (pi documents it as the event for status integrations) |
| `ui_prompt_start` | `waiting_for_input` |
| `ui_prompt_end` | `working` if an agent run is in flight, else `idle` |
| `session_shutdown` | the `end` op, with pi's `reason` (`quit`, `new`, `resume`, `fork`, `reload`) |

**No `waiting_for_permission`.** pi has no per-tool approval prompt, because its
approval model is project trust. The only blocking prompts are extension dialogs
(`ctx.ui.confirm`/`select`/…), and those carry nothing that separates a permission
question from any other. So every one reads as `waiting_for_input`.

`/new`, `/resume` and `/fork` end the old session and start a new one, so they
show up as an `end` followed by a fresh `idle`.

`starting` is never reported for pi — it exists only for Feeds 1–3's
hook-inferred Claude Code sessions, where it means "the process just launched,
before its chat surface exists yet". pi's extension only loads once pi's UI is
already interactive, so by the time `session_start` fires there is nothing left
to distinguish from `idle`; reporting `starting` there previously left every
pi session showing as busy from launch until the first prompt finished, since
`starting` then bucketed as working everywhere it rendered (the tray, `sessions
list`, and the VS Code tree). Since #1946 it buckets as idle.

The extension talks to the socket directly from pi's own Node process. It does not
spawn a sink per event, because `tool_execution_start` fires on every tool call.
It sends only on a state change. Reports go out one at a time, in order, and each
connection is bounded to 2s. At most 32 can be queued. It is **fail-open**: no
handler awaits the daemon and every error is swallowed, so a missing daemon costs
one refused connect and never delays a turn. On `session_shutdown` it waits at
most 1.5s for the `end` to leave. Like the wrapper, it re-reports its state every
30s, so an idle pi session does not age out on the 5-minute TTL. It sends **only**
state, `session_id`, `cwd`, the session-file path and the model id, never a
prompt, a message or a tool argument.

### Codex hooks (Feed 6)

[Codex](https://developers.openai.com/codex) — the CLI, its VS Code extension,
and Codex Desktop — runs lifecycle hooks from `$CODEX_HOME/hooks.json` (default
`~/.codex/hooks.json`), in the same `{ "hooks": { "<Event>": [ … ] } }` shape as
Claude Code's settings and with a payload that uses Claude Code's field names. So
the Feed 1 sink serves it too, with `--agent codex` selecting Codex's event
mapping and tagging the session `codex`. The investigation behind it is
[docs/plan/codex-sessions-feed.md](plan/codex-sessions-feed.md).

```bash
# Also installs into ~/.codex/hooks.json, but only when Codex is installed: its
# home directory exists, or a `codex` executable is on PATH. Honors $CODEX_HOME.
omni-dev sessions install-hooks

# Also removes our Codex hooks (whether or not Codex is still installed).
omni-dev sessions uninstall-hooks

# A non-default Codex home; passing it installs even if Codex is not detected.
omni-dev sessions install-hooks --codex-home /path/to/.codex
```

**Trust it once.** Codex **silently skips** a hook it has not been told to trust,
so after `install-hooks` open the Codex CLI, run `/hooks` and trust the omni-dev
entries. Trust lives in `~/.codex/config.toml`, so one approval covers the VS Code
extension and Desktop too. It is keyed by each hook's *position and content*, so
you must trust the entries again whenever the installed command changes (a moved
`omni-dev` binary, say). Both commands print this reminder.

The installed command is `<absolute omni-dev path> sessions hook --agent codex`
on these events:

| Codex event | Reported | Notes |
|---|---|---|
| `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `Stop` | the same-named event | `PreToolUse`/`PostToolUse` use the `*` matcher |
| `PermissionRequest` | `{ "notification": "permission_prompt" }` → `waiting_for_permission` | Codex's dedicated approval event |
| `PreToolUse` of `request_user_input` | `{ "notification": "agent_needs_input" }` → `waiting_for_input` | the clarifying-question UI is a tool call |
| `Interrupt` | `stop` → `idle` | Esc/Ctrl-C mid-turn, or a declined approval in the TUI; `timeout: 3` |
| `SubagentStart` | `post_tool_use` → `working` | spawned by the parent's tool call, so always inside a turn; subagents report the parent's `session_id` |
| `SubagentStop`, `PreCompact`, `PostCompact` | `transcript_discovered` → *unchanged* | each can fire on an idle session (`/compact` is `Stop · PreCompact · PostCompact`), and nothing afterwards would release a `working`; the thread lock keeps an idle session alive |
| `SessionEnd` | the `end` op, with Codex's `reason` | `timeout: 3` (Codex caps it at 3 s) |

**Position-stable install and uninstall.** Because Codex trusts a hook by its
`<event>:<group>:<hook>` index, removing a group would shift every later group in
that event down one index and silently untrust it. So for `hooks.json`:

- install only **appends** new groups, and rewrites any other
  `omni-dev sessions hook` entry (any path to an `omni-dev` binary, any
  arguments) to the current tagged command **where it sits**. That covers an
  untagged entry you added by hand — left beside the tagged one it would race it
  to be the session's first sighting, which fixes the agent tag, and could label a
  Codex session `claude` for good — and a tagged one left behind by an upgrade
  that moved the binary. Extra arguments such as `--socket` are not kept;
- uninstall removes every such entry and, where that empties a
  group that other groups follow, leaves an inert `{ "hooks": [] }` placeholder in
  its place rather than dropping it. Trailing empty groups, and then an empty
  event, are dropped, since nothing can shift into them. Placeholders accumulate
  across install/uninstall cycles; you can delete them by hand and re-trust what
  follows.

One case is not position-stable: if you put the sink in the same group as another
hook, ahead of it, removing it shifts that hook's index within the group. A hook
list has no placeholder form, so uninstall says when this happened and asks you to
trust that hook again.

**Accuracy limits.** Hooks remain an inference feed for Codex:

- Approving a prompt fires no event of its own; the next `PostToolUse` is the
  first sign. A declined prompt ends the turn (`Interrupt` in the TUI, `Stop` in
  VS Code), which reads `idle` either way.
- Automatic review ("Approve for me", `--approve-for-me`) fires
  `PermissionRequest` exactly like a human prompt and resolves it seconds later,
  so a brief false `waiting_for_permission` is unavoidable. `codex exec` under its
  default `approval: never` never fires it.
- SIGHUP (closing a terminal tab), SIGTERM and SIGKILL fire no `SessionEnd`, and
  Codex's documented 30-minute idle end did not fire in testing. The rollout
  watcher (below) ends such a session within seconds; without it, the session ages
  out on the 5-minute TTL.
- In the TUI, `request_user_input` needs Codex's still-under-development
  `default_mode_request_user_input` feature; Desktop offers it by default.
- A Desktop chat started from the app lives under `~/Documents/Codex/…`, which no
  VS Code window has open, so its `source` is `terminal`.

**The rollout watcher** (#1909) runs in the daemon beside the Claude transcript
watcher (Feed 2), with nothing to install. Every 5 seconds it scans
`$CODEX_HOME/sessions/YYYY/MM/DD/rollout-*.jsonl` (default `~/.codex`) and
supplements the hooks in three ways:

- **Discovery.** A rollout modified in the last 5 minutes is read once, and only
  its first line, Codex's `session_meta` record. That gives the session's id and
  `cwd`, so a session started before the daemon, or before the hooks were
  trusted, still lands on its worktree row. A subagent thread's `source` is an
  object (`{"subagent": …}`); those rollouts are skipped, since their hook events
  already carry the parent's id. Older rollouts are recorded by size and read only
  if they grow again (`codex resume` appends to the same file).
- **Ends.** Archiving a chat in the VS Code extension or Desktop *moves* its
  rollout to `archived_sessions/`, outside the watched tree, so a rollout that
  disappears ends its session. A killed process is caught through Codex's thread
  locks. While a thread is loaded, Codex holds an exclusive `flock` on
  `$CODEX_HOME/thread-writer-locks/<session-id>.lock`, and the kernel releases it
  when the process dies however it dies. A lock the watcher has seen held that is
  later free, or gone, ends the session. On first sight, a rollout whose lock is
  already free or gone is recorded but not listed, so a daemon restart does not
  show sessions that ended while it was down. Without a `thread-writer-locks`
  directory at all (a Codex without these locks) the locks say nothing and the
  watcher relies on the rollouts alone.
- **Idle liveness.** While the lock is held (or the rollout grows) the session is
  re-reported once a minute, keeping the state it has, so an idle Codex session no
  longer ages out on the TTL while its process lives. A thread Codex unloads
  releases its lock, and so ends too.

It never reports a state of its own: a session only the watcher knows about reads
`idle`, since rollout growth continues around a turn's end and cannot say
`working`, and the rollout records no approval requests. It reads no conversation
line and logs nothing. The lock probe is a non-blocking *shared* `flock`, taken
and released at once: on a session's first sight, then once per scan while the
session is tracked and not ended. Against a live holder it simply fails, so the
only possible collision is Codex acquiring that same thread's lock in the same
instant. A watcher heartbeat never refreshes a session that has already ended.

**Version skew.** A daemon from before the `agent` tag (#1901) ignores it and
lists Codex sessions as Claude's. A daemon with only `claude | pi` rejects
`"codex"` (`unknown variant`), and because the sink is fail-open that rejection is
silent: Codex sessions simply do not appear until you `omni-dev daemon restart`
onto the upgraded binary. `omni-dev daemon status` warns when the CLI and the
resident daemon versions differ.

### The Codex wrapper (Feed 7)

Hooks only *infer* a Codex session's state. Codex's app-server knows it exactly,
but the VS Code extension and Desktop each run a private one that nothing else
can reach. For the Codex sessions **omni-dev launches**, `codex-wrap` runs one it
owns ([ADR-0088](adrs/adr-0088.md)):

```bash
# Instead of `codex`: the same TUI, with exact state in the tree and tray.
omni-dev codex-wrap -- codex
omni-dev codex-wrap -- codex resume --last
```

`omni-dev worktrees ui` opens a Codex tab this way with `alt-⇧x` (or **New Codex
Tab** in the menus), as `alt-⇧t` opens a Claude tab through `claude-wrap`.

The wrapper starts `codex app-server --listen unix://<runtime-dir>/codex-wrap-<pid>.sock`
and runs `codex --remote unix://… <args>` on your terminal. Once a second, it
reads each loaded thread's status from that server and reports it in the
authoritative `{ "stream_state": … }` form, tagged `codex`:

| App-server status | Reported |
|---|---|
| `active`, no flags | `working` |
| `active` + `waitingOnApproval` | `waiting_for_permission` |
| `active` + `waitingOnUserInput` | `waiting_for_input` |
| `idle`, `systemError` | `idle` |
| unloaded, or `notLoaded` | the `end` op |

It re-asserts every session's state on every poll, so a hook's inferred report
is overridden within a second and an idle session never ages out. When the TUI exits it
ends the sessions, stops the server and removes the socket. Subagent threads and
the system's own side threads (`ephemeral`, such as the one that titles the chat)
are not reported.

It is an **observer only**. It calls `initialize`, `thread/loaded/list` and
`thread/read` and nothing else. It never starts, resumes or subscribes to a
thread, which is what routes approval requests to a client, and it drops any
server request unanswered. So it cannot see, answer or duplicate an approval. It
attaches only to the server it started.

**Fail-open.** An invocation the remote TUI does not serve is passed straight to
`codex`: a non-interactive subcommand (`exec`, `login`, `mcp`, …), no terminal, or
your own `--remote`. So is one whose server does not start within 10 s. If the
observer cannot connect, the TUI still runs, unobserved. On exit the TUI prints a
"Reconnect: codex --remote …" hint; the server it names is already stopped.

Hooks still fire for a wrapped session, from the private server that runs the
thread, so its hook reports arrive too. The wrapper's exact state corrects them
within a second.

## Tray

The macOS menu bar gains an **"Agent Sessions"** submenu (titled "Claude
Sessions" before #1908): one line per session, prefixed with its agent
(`<agent> · <name> <glyph> <state>`, e.g. `Codex · omni-dev ⚙ working`). A session embedded in a VS Code window is a clickable
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

When more than one live window covers a `cwd`, the winner is, in order: the
window whose matching folder is the **longest** (the most specific root — a window
on `/repo/sub` beats a later-opened one on `/repo` for a session in `/repo/sub`),
then the **most recently registered** key, then the lowest key (only so equal
stamps stay deterministic). Registration time is when a key first registered, and
a heartbeat refresh does not move it. A window reload registers a fresh key while
the old one lives on for up to the 30s window TTL, so the newest registration is
the live window ([#1451](https://github.com/rust-works/omni-dev/issues/1451)).
`last_seen` is deliberately not the criterion: every open window refreshes it on
its own heartbeat, so two live windows on one folder would alternate. After a
daemon restart the windows re-register in arbitrary order, which fixes the order
among them until a window next registers. The tray's `focus:<session_id>` action
resolves its window by the same rule.

## Security

**No new trust boundary** — the same posture as [ADR-0039](adrs/adr-0039.md) and
[ADR-0040](adrs/adr-0040.md):

- Ops ride the daemon's existing `0600` Unix socket in its `0700` directory. The
  `subscribe` stream (#1414) adds no capability: it is read-only and carries
  exactly what `list` already serves, just pushed rather than polled.
- **No secret is persisted.** The registry is in-memory only. The one thing the
  service keeps on disk is the [hook journals](#hook-journals-2108): session ids,
  `cwd`s, transcript paths, pids and event kinds, in a `0700` tree of `0600`
  files, inside the same owning-user boundary as the `0600` socket. They hold no
  prompt, tool or transcript content, and the daemon deletes them.
- Residual exposure, stated plainly: anything that can read the socket can
  enumerate your open session **cwds/repos and coarse state**; anything that can
  write it can inject fake sessions — but both already require being the owning
  local user.
- Hooks are **opt-in** user config; `sessions hook` writes nothing to stdout, and
  nothing to disk except its session's [journal](#hook-journals-2108), besides the
  fire-and-forget socket POST. That holds for the Codex hooks too, which also
  need Codex's own `/hooks` trust before they run.
- The pi extension is **opt-in** (written only by `install-hooks`, and only when
  pi is installed). It runs with pi's own permissions, as every pi extension
  does, and sends only state and identifiers, over the same fire-and-forget socket
  POST.
- The Codex rollout watcher reads only the first line of each recent rollout
  file (Codex's session metadata, never a conversation line) plus file sizes, and
  probes Codex's thread-lock files with a non-blocking shared `flock`. It writes
  nothing outside the daemon's memory.
- The Codex wrapper is **opt-in** (it runs only when you launch through it). It
  starts a Codex app-server of its own on a socket in the `0700` runtime
  directory and only ever polls it: it never subscribes to a thread or answers a
  server request, so it cannot act on an approval. It reads thread status, `cwd`
  and model id, never a conversation, and persists nothing.
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
| `observe` | `{ session_id, cwd?, transcript_path?, event, model?, agent? }` | `{ ok: true }` |
| `end` | `{ session_id, reason? }` | `{ ended: bool }` |

where `event` is one of `session_start`, `user_prompt_submit`, `pre_tool_use`,
`post_tool_use`, `stop`, `{ "notification": "permission_prompt" \| "idle_prompt" \|
"agent_needs_input" \| "other" }`, `transcript_grew`, `transcript_discovered`, or
`{ "stream_state": "<state>" }` — the authoritative Feed 4 form, applied verbatim
rather than inferred. `agent` is `claude` (the default, omitted by every Claude
feed), `pi` or `codex`. It is fixed by a session's first sighting, and `list`
always includes it.

## Scope and follow-ups

- **Idle-session liveness.** Without a dedicated event, idle sessions age out on
  the TTL — for the feeds that lack one. A wrapped session (Feed 4) heartbeats
  itself while busy only, deliberately not while idle (#1454); a future refinement
  could keep the rest alive off their window's heartbeat. Capping a window's
  sessions by its reported tab count, or hiding sessions whose transcript file does
  not exist, would trim the phantoms a long-lived idle chat sits among without
  this cost, and remain unbuilt.
- **Per-tab attribution** stays heuristic until (if ever) the Claude extension
  exposes a tab↔session API. Note the wrapper does *not* fix this: it knows its
  own session exactly, but still cannot say which tab is showing it.
- **Terminal Claude is unwrapped.** Feed 4 covers the VS Code extension's
  stream-json tabs only; a TUI `claude` keeps the inferred feeds. Wrapping it
  would mean interposing on an interactive terminal, which is a different and much
  riskier proposition. That includes the `worktrees ui` Claude tab: `claude-wrap`
  `exec`-replaces itself under a PTY and reports nothing there (#2152). A Claude
  Code mod can observe a terminal session from inside the process, which is the
  recommendation of the spike in
  [docs/plan/claude-mods-feed.md](plan/claude-mods-feed.md) (#2122, built in #2151).
- **Only new tabs are wrapped.** Coverage is prospective — sessions already
  running when the setting is applied keep the inferred feeds until they restart.
- **pi sessions are always `terminal`.** Tagging one `vscode` would need the
  companion to count pi terminals, but the pi.dev launcher renames those tabs to
  the session's `/name` (#1899), so a name match is unreliable and needs its own
  design. pi rows still get their worktree cues, since those are tallied by `cwd`.
- **No pi transcript watcher or RPC wrapper.** A Feed 2 analogue over
  `~/.pi/agent/sessions/` and a Feed 4 analogue over `pi --mode rpc` would each
  cover pi processes that do not load global extensions, but the extension
  already reports exact state for the ones that do.
- **Windows** support waits on the broader daemon Windows work (#1363); the hook
  sink and transcript scheme are already portable, only the socket transport is
  Unix-only.


## Troubleshooting

Session diagnostics let you follow a sighting through ingestion, the registry,
the daemon subscription, and the VS Code companion. They preserve existing
state and attribution rules; an attribution mismatch remains a separate bug
to investigate using the evidence.

### Enable and read daemon diagnostics

Set `daemon.log_level` in `$HOME/.omni-dev/settings.json`:

```json
{
  "daemon": {
    "log_level": "info,omni_dev::sessions=debug,omni_dev::daemon=debug"
  }
}
```

Restart the daemon, then follow its log:

```bash
omni-dev daemon restart
omni-dev daemon logs --follow
```

The log reader supports foreground/background file-log launches and launchd;
a foreground `daemon run` emits tracing to stderr. For a systemd user unit,
read the journal with `journalctl --user -u omni-dev.service -f` instead.
See [configuration](configuration.md#daemon-and-mcp-tracing-settings) for
precedence and fallback behavior. `RUST_LOG` in the daemon process overrides
the settings filter; shell exports are not inherited by socket activation.

Use `omni_dev::daemon::server=trace` to see each subscription sample's
`change_notification` or `periodic_tick` trigger and `pushed` versus
`suppressed_identical` result. Initial snapshots and terminal counts are
accounted for too. A subscription ends with a distinct `ClientCancel`,
`ClientEof`, `ReadDecodeError`, `DaemonShutdown`, or `WriteFailure` reason.
Cancellation and EOF are normal debug events; failed writes and service
rejections are warnings visible at the default `info` level.

Registry `session_observed` records include identity, agent/PID/event,
old/new states, an outcome, reap count, and the actual `bumped` decision.
`heartbeat_only` refreshes liveness without triggering a change notification;
`replaced_pid_ignored` explains a late event from the process an in-place resume
replaced. `ended_passive_ignored` prevents passive sightings reviving ended
sessions. `session_end` distinguishes unknown, already ended, ignored, and
newly ended sessions. Window reports and unregisters expose their bump decision.

Positive TTL reap counts and capacity evictions explain disappearing entries.
At trace level, individual reaps identify `session_ttl`, `ended_ttl`, or
`window_ttl`; `session_process_ended` identifies PID-based cleanup. Busy
stream-wrapper sessions keep alive, while idle ones can age out. A
`session_attribution_miss` reports cwd and the live-window count; trace-level
candidate folders help reveal lexical mismatches such as `/tmp` versus
`/private/tmp`. Matching still uses the existing folder specificity,
registration time, and key ordering, without canonicalization.

Feed 2 emits one scan summary containing candidate/sighting and skip/error
counts. A blocking scan failure warns that tracking state was reset. Repository
enrichment task failures warn separately from a normal nonrepository cwd.

### Hook diagnostics

Hook sinks are short-lived CLI processes; daemon settings do not set their
filter. To investigate hooks, run the configured hook command with
`RUST_LOG=omni_dev::cli::sessions=debug` in its own environment. Diagnostics go
to stderr, never stdout, and do not change exit 0 or the report timeout.
`session_hook_skipped` identifies read/JSON/identity/event/socket gates;
`session_hook_report` distinguishes delivery, timeout, transport failure, and
daemon rejection. Notification diagnostics contain the classification and
presence of type/message fields, never the raw notification text. Claude and
Codex sightings are tagged separately by the shared hook sink.

### Journal diagnostics

The sink logs `session_hook_journal` with outcome `journaled` or
`journal_failed` (with the error, never the payload) next to
`session_hook_report`. In the daemon, `session_journal_replayed` (with
`proven` and the record count), `session_journal_rejected` (with the reason:
`ended`, `too_old`, `dead` or `stale`), `session_journal_removed` (`rejected`,
`orphan`, `too_old`, `unreadable`, `stale_tmp`) and `session_journal_compacted`
follow a journal through its life, at debug level. `session_observed` and
`session_end` carry the route (`origin`) and the dedupe outcomes
`duplicate_ignored` and `journal_stale_ignored`.

### Opt-in wrapper metadata file

For the Claude stream wrapper, set `OMNI_DEV_CLAUDE_WRAP_LOG` to an absolute
file path in the environment of the process launching the wrapper. For example,
when launching a fresh VS Code process from a shell:

```bash
OMNI_DEV_CLAUDE_WRAP_LOG="$HOME/claude-wrap-diagnostics.jsonl" code .
```

An already running VS Code process may retain its earlier environment; fully
quit/relaunch it and start a new wrapped Claude process to apply the change.
The variable does not enable or install the wrapper by itself: use the Feed 4
installation instructions above first.

The file contains newline-delimited metadata: process start/exit, learned
session identity/cwd/model, state reports, report outcome codes, and periodic
plus final cumulative diagnostic summaries. `tee_full` identifies dropped
observer lines when a slow daemon backs up the bounded tee; `tee_closed`,
`tee_oversize`, and `tee_non_utf8` distinguish other drops. Tracker counters
identify parse failure, unknown control shapes, missing permission IDs, and
permission-cap drops. Known unrelated control requests are ignored normally.
The summary also counts drops from the independent diagnostic queue.

When unset or empty, no file, writer thread, tee-counter allocation, or
diagnostic formatting is created. The pure tracker still counts protocol drift.
When enabled, a separate thread appends records through a bounded nonblocking
queue; byte pumps only update counters. New files use `0600`; symlinks and
nonregular targets are refused, and existing file permissions are left intact.
Open/write failures silently disable writing without preventing Claude from
launching. Shutdown waits at most 200 ms for diagnostics; a killed process,
stuck disk, or full queue can lose records. Files append across processes and
include timestamps, wrapper PID, and session/child PID metadata where known;
there is no automatic rotation.

Conversation messages, tool inputs/results, raw stdio/hook payloads, and daemon
error text are never written to this wrapper file. Paths and identifiers are
still personal metadata: inspect logs before sharing them. Remove the variable
and launch new wrapper processes to disable logging; delete the file when the
investigation is finished. This is an explicit metadata-only persistence
exception to Feed 4's normal no-persistence rule.

### VS Code output and tracing a stale cue

Open **View → Output → omni-dev**. One-shot requests now report daemon
rejections as well as transport failures. A rejected/unreachable session-window
report includes the window key; invalid subscription frames report a reason
without dumping the frame, preserving the last valid snapshot. A rejected
subscription continues to use the existing unsupported/fallback behavior.

Compare evidence in this order:

1. Did the hook/watcher/wrapper deliver a sighting, or record a gate/drop?
2. Did `session_observed` accept it, change state/metadata, and bump?
3. Did the subscription wake and push, or suppress an identical snapshot?
4. Was the window report accepted and cwd matched to a live window?
5. Did the companion reject the frame or report a connection problem?
6. Did TTL/PID cleanup or capacity eviction remove the session afterward?

Sessions outside tracked worktrees legitimately contribute no row count.
The pure tally function remains unchanged and does not log every unmatched
session. Gather logs for the narrow reproduction window; use trace only when
debug cannot identify the failing hop.
