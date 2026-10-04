# Claude Code Mods Feed

**Status:** Aspirational — spike complete (#2122); the recommendation is a mod feed for terminal sessions only (#2151), and nothing is built
**ADRs:** [ADR-0052](../adrs/adr-0052.md) · [ADR-0057](../adrs/adr-0057.md) · [ADR-0028](../adrs/adr-0028.md) · [ADR-0072](../adrs/adr-0072.md)

## Overview

[ADR-0057](../adrs/adr-0057.md)'s `claude-wrap` is the only authoritative Claude
session-state feed, and it costs a dependence on an undocumented stream-json protocol, a
place in Claude's launch path, and a process that sees conversation content. Claude Code
v2.1.287 shipped **mods**: JavaScript handlers, loaded as a plugin, that run inside the
Claude process through a documented API. #2122 asked whether a mod can replace the Claude
half of Feed 4, supplement it, or neither. This document is the answer: a tested
event/state matrix, the coverage and failure-mode comparison, a recommendation, a proposed
contract and install design, an ADR statement and the follow-ups. No production code
lands from the spike; the throwaway mod and harness are attached to the issue.

**Recommendation: supplement, for terminal sessions only. Do not replace the wrapper.**

- **A mod cannot replace the wrapper in the VS Code panel.** It detects a permission
  prompt as fast as the wrapper does, but nothing in the mod API fires when the user
  *allows* one, and the panel draws no UI to observe. An approved 25 s tool read
  `waiting_for_permission` until it finished, 26 to 34 s later, where the wrapper released
  it in 4 ms. Denials and `AskUserQuestion` release at once; allowed long tools do not.
- **A mod is the first authoritative feed the terminal has.** The wrapper `exec`-replaces
  itself when stdout is a TTY, so it reports nothing for a terminal session, and that
  includes the `worktrees ui` Claude tab (measured: zero reports, #2152). In a terminal
  the mod sees the answer (`ToolUse` render with `isRunning`, +72 ms against Feed 1's
  +28.5 s), the interrupt (`turn.complete{isAborted}`, +153 ms, where Feed 1 stayed stuck
  until the process exited), and a refused prompt (+85 ms, where Feed 1 never released).
- **The recommended deployment works.** With the mod beside the Feed 1 hooks on one
  registry, the answer, interrupt and refusal gains all held (+54 to +218 ms). One race
  was found: Feed 1 overwrote the mod's `waiting_for_input` for `AskUserQuestion` 20 ms
  later, and a one-line Feed 1 mapping fix removes it with or without a mod.
- **Delivery is cheap and safe.** Spawning the existing sink costs a median 18-115 ms per
  event, runs off the turn's path, stays silent and exits 0 on every failure tried.
- **Reliability is different, not worse.** `--safe-mode`, `--bare`, `disableAllHooks` and
  an old `claude` (npm `stable` was 2.1.285) silence the mod, and the first three also
  silence the Feed 1 hooks; the wrapper survives them. A spinning hook stalls Claude for
  about 10 s once and is then unloaded. ADR-0028's `--setting-sources ""` sandbox keeps an
  installed mod out of the `claude-cli` backend (measured).
- **A new ADR is needed** if this proceeds: code that runs inside Claude's process is a
  new trust surface and a new install mechanism, in the manner of ADR-0057.

The spike also found two defects in the existing wrapper, filed separately: #2152 (the
TTY blind spot above, which contradicts ADR-0072 and the code comments) and #2153 (a stale
`working` after an SDK `set_model`).

## Test setup

| Surface                  | How it was exercised                                                                                                                                         | Runs |
|--------------------------|--------------------------------------------------------------------------------------------------------------------------------------------------------------|------|
| Panel stand-in           | A driver speaking the extension's transport (`--input-format/--output-format stream-json`, `--permission-prompt-tool stdio`), through `omni-dev claude-wrap` | 24   |
| Terminal TUI             | A Python `pty` driver (trust dialog, prompts, `Esc`, `/model`, `/compact`, `/clear`)                                                                         | 20   |
| `claude -p`              | Loaded the mod, `--setting-sources project`                                                                                                                  | 1    |
| Installed-plugin sandbox | `claude --init-only` with a user-scope installed mod in an isolated `CLAUDE_CONFIG_DIR`, no API call                                                         | 15   |

Claude Code **2.1.288** (macOS, arm64, `claude-haiku-4-5` for every turn), `omni-dev`
0.45.0. The first three rows are 45 timed session runs; the fourth is 15 load-path checks
that make no model call, 60 in all. Of the session runs, 13 carried the forwarder
prototype alone, 5 carried it beside the Feed 1 hooks on one registry, one carried it
beside the wrapper, and the rest carried a discovery logger. Three early runs were discarded: this Claude Code
blocks a bare `sleep N` in the Bash tool, so the "long tool" never ran (a leading
`mkdir -p … &&` avoids the block).

Three scratch daemons (`daemon run --socket … --services sessions --no-menu`) kept the
feeds apart: the wrapper reported to one, a Feed 1 stand-in (the 17-event hook block
pointed at its own socket through `--settings`) to a second, and the forwarder to a third.
A subscriber timestamped every state change. The driver is the ground-truth clock: the
instant it saw `can_use_tool`, sent the answer or the `interrupt`, or pressed the key. The
user's own daemon, `settings.json`, plugin installs and marketplaces were never written.
Only event names, ids, states and timestamps were kept; no prompt, tool input or output.

Limits, stated plainly:

- The panel is exercised at the **protocol level**, not in the VS Code chat panel itself.
  A one-time confirmation in the real panel is listed under [Not tested](#not-tested).
- The host was busy (load average about 79 at the end of the session), so absolute
  latencies are pessimistic: a `sleep 25` took 26 to 34 s of wall time across runs, and
  one run's later reports took about 1 s each.
- The TUI runs shared one trusted working directory (a fresh one needs the trust dialog
  answered, which records an entry in Claude's own config). The subscriber therefore also
  saw the previous run's `ended` session expire on the registry's 10 s ended TTL and
  logged it as a drop: 16 such rows in 14 of the 20 TUI runs, every one an earlier
  session's and none the run's own. No figure here uses one.

## Event/state matrix

A mod registers handlers with `on(event, handler)`. Besides its own events, every settings
hook event is also a mod event named `classic.<Event>` with the same stdin JSON, so Feed
1's whole signal set is available in-process. What a mod adds is below.

| State                    | What the mod sees                                                                                                                | Authoritative?                         |
|--------------------------|----------------------------------------------------------------------------------------------------------------------------------|----------------------------------------|
| `starting`               | `classic.SessionStart` (`source`: `startup`, `resume`, `clear`, `compact`)                                                       | Yes                                    |
| `working`                | `turn.start` (a subagent's turn carries `agentId`)                                                                               | Yes                                    |
| `waiting_for_permission` | `classic.PermissionRequest` (`tool.check` resolves to `ask` about 20 ms earlier)                                                 | Yes                                    |
| `waiting_for_input`      | `classic.PermissionRequest` with `tool_name: AskUserQuestion`; `classic.Notification{idle_prompt}` after about 60 s              | Yes                                    |
| release of a wait        | Terminal: `ui.render{component=ToolUse}` with `isRunning: true`. Both: the `tool.call` chain returning (`isError` for a refusal) | Terminal yes; panel only for a refusal |
| `idle`                   | `turn.complete`, including `isAborted: true`; a manual `/compact` ends at `classic.PostCompact`                                  | Yes                                    |
| `ended`                  | `session.end` (`reason`: `clear`, `resume`, `prompt_input_exit`, `other`)                                                        | Yes, but absent on `kill -9`           |

Answers to the questions the issue asked:

- **Is there an event for the prompt itself?** Yes: `classic.PermissionRequest`, and it
  fires in both the terminal and the panel path (the settings hook of the same name is
  what Feed 1 already installs).
- **Does `ui.render{component=AskUserQuestion}` fire in the panel?** No. It fires in the
  terminal. In the panel, `AskUserQuestion` is a `can_use_tool`-style permission, so the
  signal there is `classic.PermissionRequest` with the tool's name.
- **Is there an event for "the user answered"?** Only in the terminal, as a side effect of
  rendering: `ToolUse` flips `isRunning` to `true` 30 ms after the keypress. The panel
  renders nothing, so its only release for an allow is the tool finishing.

### Measured against the user's action

Milliseconds from the ground-truth event (the driver's `can_use_tool` or screen dialog for
a start, its answer or keypress for a release).

**Panel (stream-json, the extension's transport)**

| Scenario                    | Wrapper                              | Mod forwarder                          | Feed 1 hooks                           |
|-----------------------------|--------------------------------------|----------------------------------------|----------------------------------------|
| Permission prompt shown     | +0 to +26                            | +22 to +219                            | +14 to +325                            |
| Allowed, 25 s tool          | released +4                          | released **+33,592**                   | released +32,511                       |
| Allowed, 46 s tool          | released +3                          | released **+49,618**                   | released +49,554                       |
| Denied                      | released +0                          | released +18                           | released +1,704 (to `idle`)            |
| `AskUserQuestion`           | `waiting_for_permission` +1, rel. +1 | `waiting_for_input` +22, released +79  | `waiting_for_permission` +27, rel. +53 |
| Interrupt (control request) | `idle` +78                           | `idle` +404 (queued behind one report) | never; `ended` at exit (+4,177)        |
| `kill -9`                   | `ended` +10                          | `ended` +3.5 s (pid watcher)           | `ended` +3.3 to +4.9 s (pid watcher)   |

A 25 s `sleep` took 26 to 34 s of wall time on the busy host (an earlier panel run measured
Feed 1's release at +26,157 ms), which is the spread in the 25 s rows.

**Terminal (TUI)**

| Scenario                  | Wrapper                         | Mod forwarder                         | Feed 1 hooks                            |
|---------------------------|---------------------------------|---------------------------------------|-----------------------------------------|
| Permission prompt shown   | none: zero reports (TTY `exec`) | +3 to +28                             | +12 to +42                              |
| Allowed, 25 s tool        | none                            | released **+72**                      | released **+28,539**                    |
| Allowed, 46 s tool        | none                            | released +40                          | released at the tool's end (+45.6 s)    |
| Refused (`Esc` at dialog) | none                            | released +85                          | never; `ended` at exit (+6,536)         |
| `Esc` interrupt in a tool | none                            | `idle` +153                           | stuck 16.6 s; `ended` at exit (+8,554)  |
| `AskUserQuestion`         | none                            | `waiting_for_input` +3, released +101 | `waiting_for_permission` +12, rel. +107 |
| `kill -9`                 | none                            | entry gone +2.9 s (pid watcher)       | same path as the mod                    |

Where the mod is and is not enough:

- **Start of a wait.** The mod ties both other feeds. It also names `AskUserQuestion` a
  `waiting_for_input`; the wrapper and Feed 1 both call it `waiting_for_permission`.
- **Release after an allow.** Terminal: solved, by the render site. Panel: unsolved. It
  cannot be solved from the mod API; the wrapper reads the answer off stdin.
- **Interrupt.** `Stop` does not fire on `Esc`, which is why Feed 1 sticks. `turn.complete`
  does, with `isAborted: true`, in the terminal (about 80 ms after the key in the mod's own
  log, +153 ms for the state to reach the daemon) and in the panel.
- **Turn start.** `turn.start` led the wrapper's `working` by 1.3 to 2.0 s when this was
  measured, because the wrapper reported `idle` at `init` and only flipped on the first
  assistant line. #2173 fixed that: a prompt the editor wrote now holds `working` through
  the `init`, so the wrapper reports `working` from the prompt (from its turn's `init` for
  a process's first prompt) and the lead above no longer applies.

### Observed sequences

- **Normal turn.** `classic.SessionStart` → `prompt.submit` → `classic.UserPromptSubmit` →
  `turn.start` → `turn.step` (once per model request) → `tool.call` → `classic.PostToolUse` →
  `tool.call` returns → `turn.step` → `classic.Stop` → `turn.complete` → `session.end`.
- **Allowed Bash.** `tool.call` → `tool.check` (`ask`) → `classic.PermissionRequest` →
  (terminal) `ToolUse isRunning:true` → `classic.PostToolUse` → `tool.call` returns. In the
  panel the middle step is missing.
- **Refused or interrupted.** `classic.PostToolUseFailure` → `tool.call` returns with
  `isError: true` → `turn.complete{isAborted}`. No `Stop`.
- **Subagent.** `tool.call{Agent}` → `agent.spawn` → `classic.SubagentStart`; the subagent's
  own `tool.call` and `turn.complete` carry `agentId`. A forwarder must ignore every
  event with an `agentId`, or a subagent's `turn.complete` marks the parent `idle`. In the
  terminal the `Agent` tool returned at once and the parent's `Stop` came before the
  subagent's own `turn.complete`.
- **`/compact`.** Not a turn: `session.compact` → `classic.PreCompact{trigger}` →
  `classic.SessionStart{source: compact}` → `classic.PostCompact`, 10 to 12 s apart. The
  wrapper reports `working` for the whole compaction; Feed 1 reports `idle`. The mod can
  bracket it, though the forwarder prototype did not map it.
- **`/model`.** `classic.PreModelSwitch` / `PostModelSwitch` with `source` `command`
  (terminal) or `sdk` (`set_model`); `turn.step.model` carries the model of each request.
  The wrapper, by contrast, stayed `working` after an SDK `set_model` when this was
  measured (#2153, since fixed: a `user` line the CLI writes no longer counts as work).
- **`/clear`.** `session.end{reason: clear}`, then `classic.SessionStart{source: clear}`
  with a new `session_id`. `session.start` does not fire again.
- **Window reload (kill and `--resume`).** `classic.SessionEnd`, then
  `classic.SessionStart{source: resume}` under the same id, as the wrapper's `ended` then
  `working`.

## Delivery to the daemon

A mod has no socket, so it spawns. `$.process.run(argv, { stdin, timeoutMs })` resolves
when the child exits; `$.process.spawn` (2.1.288) streams its output. Neither returns a
pid.

| Measure                                                       | Result                                                         |
|---------------------------------------------------------------|----------------------------------------------------------------|
| Spawn of `true`                                               | median 7 ms                                                    |
| Spawn of `omni-dev sessions hook`, daemon up (n=30)           | median 34 ms, p90 61 ms, max 81 ms                             |
| Same, no daemon                                               | median 27 ms                                                   |
| Inside the mod, per run (13 runs, busy host)                  | medians 18 to 115 ms; individual reports 15 to 430 ms          |
| Inside the mod, one run (the busiest)                         | first three reports 28 to 36 ms, the next three about 1 s each |
| Stdout, stderr and exit code, on success, no daemon, bad JSON | empty, empty, 0 (all three)                                    |

- **It never delays a turn.** The prototype's handler enqueues and returns at once; a
  single drainer awaits `$.process.run`. A promise started and not awaited keeps running
  after the handler returns (every report landed). The queue never exceeded one entry.
- **An abort cancels an in-flight spawn.** In 2 of 13 runs a report in flight when the
  turn was interrupted or refused came back with no exit code (`rc` -1); the `Stop` that
  followed landed and the final state was right. Reports must therefore carry the state,
  not a delta, and the last one must be enough on its own. The drainer should also retry
  a failed spawn once, and coalesce queued reports down to the latest state, so that a
  cancelled `idle` (nothing later repairs it, since `Esc` fires no `Stop`) is never the
  last word. Neither was tried; in both observed cancellations the report that failed was
  the *release*, with the `idle` queued behind it.
- **Ordering.** A single FIFO drainer preserved order in every run. The 10 s per-handler
  budget is not reached because the handler does not wait; the spawn's own
  `timeoutMs` (5 s) bounds the drainer.
- **The Unix-socket route exists but is accidental.** `$.http.fetch(url, { socketPath })`
  goes over a Unix socket, but the daemon speaks newline-delimited JSON, not HTTP.
  Sent an HTTP request, the daemon answers one `invalid envelope` line per header line,
  and, if the body ends in a newline, *also accepts the envelope in it*. A raw-socket
  client delivered an `observe` that way. It was not run from inside a mod, and it leans
  on the daemon tolerating garbage, so it is not recommended; if the spawn cost ever
  matters, give the daemon an explicit HTTP endpoint instead.

## Identity and liveness

- **Pid.** A process the mod spawns has the Claude process as its parent. In the three
  terminal runs checked, `$PPID` equalled the pid the driver started; in the two panel runs
  it was the wrapper's child (the driver's pid plus one). So `omni-dev sessions hook` run
  by a mod already reports the right pid through `parent_id()`, and the settings-hook sink
  reported the same pid as the mod's `$PPID` in all 19 runs that carried both the logger
  and the Feed 1 stand-in.
- **`kill -9`.** No `session.end` fires. The existing pid watcher (#1916) ends the entry
  in 3 to 5 s, the same path Feed 1 uses; the wrapper notices in 10 ms because the child's
  stdout closes.
- **Keep-alive.** `$.clock.every(30_000, …)` fired during a pending approval and a running
  tool (the timer is not blocked by the turn), so the wrapper's busy-only re-report (#1454)
  is reproducible. It is also the hazard in [Coverage](#coverage).
- **Model.** `$.session.model()` and `turn.step.model`; a `/model` shows within the next
  request.
- **After a worker respawn.** `session.start` fires again, `classic.SessionStart` does not,
  and module state is lost; the forwarder re-reads `$.session.id()` and `cwd()` there. The
  `isInteractive` gate must be re-derived from that `session.start` too (its event carries
  it every time), and the mod must stay silent until it is known: a default of "report"
  would double-feed a panel session, a default of "silent" would lose the terminal feed.

## Coverage

`session.start` tells the mod where it runs: `e.isInteractive` is `true` and
`surfaces` is `["terminal"]` in the TUI; `false` and `[]` for the stream-json panel and
`claude -p`.

| Surface                          | Mod loads | Verdict                                                                     |
|----------------------------------|-----------|-----------------------------------------------------------------------------|
| VS Code chat panel (stream-json) | yes       | Tested at the protocol level. Wrapper stays: no release signal for an allow |
| Terminal TUI                     | yes       | **The gain**: the first authoritative feed                                  |
| `worktrees ui` Claude tab        | yes       | A TUI under a PTY; the wrapper `exec`-replaces and reports nothing (#2152)  |
| `claude -p`, Agent SDK           | yes       | Loads, `isInteractive: false`; gate it out                                  |
| Desktop Code tab                 | per docs  | Untested                                                                    |
| Remote Control, cloud sessions   | per docs  | Untested                                                                    |
| WSL (Desktop)                    | no        | Plugins are not available there (documented)                                |

**Two feeds on one session conflict.** With the wrapper and the forwarder both reporting
into one daemon, the forwarder's 30 s keep-alive re-asserted `waiting_for_permission`
over the wrapper's correct `working` and held it for 8 s, until the tool ended. The fix
is the gate above: report only when `isInteractive`, so the panel and `-p` stay with the
wrapper and Feed 1, and a terminal session, where the wrapper is silent, has only the mod.

**The recommended terminal deployment: the mod beside the Feed 1 hooks, on one registry.**
Both feeds report every terminal session, so the question is whether Feed 1's inferred
events overwrite the mod's. Five terminal runs sent both into one daemon (`StreamState`
reports win outright in the registry, and the later report wins between two):

| Scenario                  | Result on the one registry                                                                       |
|---------------------------|--------------------------------------------------------------------------------------------------|
| Allowed, 25 s tool        | released +54 ms                                                                                  |
| Allowed, 46 s tool        | released +54 ms                                                                                  |
| `Esc` interrupt in a tool | released +55 ms, then `idle` +218 ms                                                             |
| Refused (`Esc` at dialog) | released +121 ms                                                                                 |
| `AskUserQuestion`         | `waiting_for_input` landed first, then Feed 1's `permission_prompt` **overwrote it** 20 ms later |

The gains survive, because Feed 1 has nothing to say at an answer or an interrupt. The one
loss is a race on `AskUserQuestion`: Feed 1's `PermissionRequest` carries the tool's name
but its sink ignores it. A one-line change to Feed 1's mapping (a `PermissionRequest` for
`AskUserQuestion` becomes `AgentNeedsInput`, as the Codex mapping already does for
`request_user_input`) fixes that classification for the hook feed on its own, with no
mod at all, and removes the race. It is included in #2151.

**`claude -p` from omni-dev's own `claude-cli` backend.** The backend passes
`--setting-sources ""`, no `--plugin-dir`, and scrubs `CLAUDE_CODE_*` from the child's
environment. Measured with the probe installed at user scope in an isolated config
(`claude --init-only`, no API call):

| Flags                                   | Installed mod loads | `--plugin-dir` mod loads | `CLAUDE_CODE_PLUGIN_DIRS` loads |
|-----------------------------------------|---------------------|--------------------------|---------------------------------|
| (none)                                  | yes                 | yes                      | yes                             |
| `--setting-sources user`                | yes                 | n/a                      | n/a                             |
| `--setting-sources ""`                  | **no**              | yes                      | yes                             |
| `--setting-sources project`             | no                  | n/a                      | n/a                             |
| `--safe-mode`                           | no                  | no                       | n/a                             |
| `--bare`                                | no                  | no                       | n/a                             |
| `--restricted`                          | no                  | yes                      | n/a                             |
| `--settings '{"disableAllHooks":true}'` | no                  | n/a                      | n/a                             |

ADR-0028's assumption holds for an installed mod. The other two load paths survive
`--setting-sources ""`, so the sandbox rests on the backend neither passing `--plugin-dir`
nor inheriting `CLAUDE_CODE_*`; a test should pin the latter (#2151).

## Silent-off conditions and failure modes

| Condition                        | Mod                                        | Feed 1 hooks                     | Wrapper | Evidence                                                          |
|----------------------------------|--------------------------------------------|----------------------------------|---------|-------------------------------------------------------------------|
| `claude` older than 2.1.287      | off                                        | on                               | on      | npm `stable` was 2.1.285, `latest` 2.1.288                        |
| `--safe-mode`                    | off                                        | off                              | **on**  | measured, panel and TUI                                           |
| `--bare`                         | off                                        | off                              | on      | measured (mod), documented (hooks)                                |
| `disableAllHooks`                | off                                        | off                              | on      | measured                                                          |
| `allowManagedModsOnly`           | off                                        | on                               | on      | documented, not tested                                            |
| `allowManagedHooksOnly`          | off (not an organisation mod)              | on (managed only)                | on      | documented, not tested                                            |
| Anthropic's remote switch        | off                                        | on                               | on      | `claude plugin test` reports it; it was not engaged on 2026-10-04 |
| A hook that spins                | one mod unloaded                           | on                               | on      | measured, below                                                   |
| Three untraceable worker crashes | every mod unloaded until `/reload-plugins` | on                               | on      | documented, not forced                                            |
| Baked `omni-dev` path gone       | loads, reports nothing                     | on (if its sink path is current) | on      | measured, below                                                   |
| Mod not installed                | off                                        | on                               | on      | by construction                                                   |

- **A spinning hook.** `claude --init-only` with a mod looping in `classic.SessionStart`
  took 10 s instead of about 1 s: the engine's heartbeat got no answer for 5 s, respawned
  the worker, and logged `spike-crasher was unloaded: it crashed the hooks worker`. A
  second mod loaded alongside it kept working, but lost one in-flight event and all its
  module state. The cost of a misbehaving mod is therefore a bounded stall plus a state
  reset; the cost of a misbehaving wrapper is a Claude that does not start.
- **A stale binary path.** The install bakes the absolute `omni-dev` path. With the path
  gone (`/nonexistent/bin/omni-dev`), `claude --init-only` ran normally in 4 s, and the
  one report failed in 15 ms with no exit code and no log line outside the mod's own: the
  mod fails open and silent. A terminal session has no wrapper to fall back on, so it drops
  to inference with no visible sign. Feed 1 handles the same case by rewriting a stale
  sink in place (#1927); the install must do likewise (re-running `install-mod` rewrites
  the path and bumps `version`), and `daemon status` or the install's own check should
  warn when the baked path no longer exists.
- **Stale rows.** A silenced or unloaded mod simply stops reporting. The pid watcher
  ends a row whose process dies; a live but unreported row falls back to the Feed 1
  inference already present for the same session, and ages out on the session TTL. The
  design keeps the 30 s busy-only keep-alive so a busy row does not age out, and must
  not flip the registry's `streamed` latch (see [Proposed design](#proposed-design)).

## Trust boundary

A mod is unsandboxed and in-process. It can read every prompt, tool call and
environment variable it chooses to, and a process it spawns is outside any sandbox. That
is a larger *capability* than the wrapper, whose reach is the stdio of one process, but
in both cases the property the repo relies on is the code's narrowness, not the host's
isolation.

`claude plugin validate` lists exactly what a mod hooks and calls, and for the forwarder
prototype it printed:

```text
hooks: classic.SessionStart, session.start, turn.start, classic.PermissionRequest,
       tool.call, ui.render{component=ToolUse}, turn.complete, session.end
calls: $.clock.every, $.clock.now, $.process.run (via drain), $.session.cwd,
       $.session.id, $.ui.log
```

No `$.fs`, `$.http`, `$.env` or `$.settings` call, and no `env reads:` line. `$.clock.now`
and `$.ui.log` were instrumentation; the production module needs neither. What
`validate` cannot show is the argument of `$.process.run`, and a `tool.call` hook receives
every tool call's input even if it never reads it, so the audit is the source, which is
small enough to review and to pin: a CI test should run `claude plugin validate --json`
against the generated plugin and fail on any other `calls:` entry.

Compared with ADR-0057's "extracts state and identity only": the mod's own code forwards
`session_id`, `cwd` and a state, and reads no prompt, no tool input and no result; the
one tool-derived field it reads is a tool's name, to tell `AskUserQuestion` apart. That is
the same constraint as the wrapper's, enforceable by a smaller artefact, but a design rule
rather than a sandbox in both cases.

**An ADR is needed if this proceeds.** The change adds code running inside Claude's
process, a second authoritative feed with precedence rules against the wrapper, and an
install mechanism that touches Claude's plugin state. Those are decisions of the size
ADR-0057 recorded. It should extend ADR-0052 and ADR-0057 and be written with the
implementation (#2151), not in the spike.

## Install story

| Route                                           | Verdict                                                                                             |
|-------------------------------------------------|-----------------------------------------------------------------------------------------------------|
| Directory marketplace, `claude plugin install`  | **Recommended.** Idempotent, no settings rewrite by omni-dev, and blocked by `--setting-sources ""` |
| `CLAUDE_CODE_PLUGIN_DIRS` in the settings `env` | Loads even under `--setting-sources ""`; needs an edit of a shared settings file                    |
| `--plugin-dir`                                  | Per session only; not an install                                                                    |

Measured against an isolated config directory:

- `claude plugin marketplace add <dir>` and `claude plugin install <plugin>@<marketplace>
  --scope user` are idempotent: the second run says `already on disk` / `already
  installed`.
- An installed directory-marketplace plugin is reported as loading in place, but Claude
  also keeps a cached copy by version. Editing the source at the same version left the
  cached copy unchanged; bumping `version` and running `claude plugin marketplace update`
  then `claude plugin update` moved 0.0.1 to 0.0.2 ("Restart to apply changes").
- `claude plugin validate` and `claude plugin test` work without a session or an API call.

The design follows from that, mirroring the pi extension and the wrapper shim: generate a
plugin directory beside the daemon socket (`paths::runtime_dir()`) from a template in
`src/templates/`, bake the absolute `omni-dev` path (so a launchd-started VS Code with a
minimal `PATH` still works), put a marker on its first line, and **bump `version`
whenever the baked path or template changes**, which also repairs a moved binary. Install
runs the two `claude plugin` commands; uninstall runs `claude plugin uninstall` and
`marketplace remove`, acting only on a plugin carrying the marker. A session that is
already open needs `/reload-plugins` (documented; not exercised here). The source lives
in the repo as a template, not an `editors/` project: the Rust build does not descend
into `editors/`, and a template is what `install-hooks` already ships for pi.

## What the wrapper does that a mod does not

The terminal-title rewrite with a colour-coded model identity (#1445). The 2.1.288 type
declarations have no site for a terminal or tab title (the only `title` is a pane's), and
the render-site table has none. A mod could in principle write an OSC sequence through a
spawned process's controlling terminal, but that is unsupported, was not tried, and
would race Claude's own title. **The rewrite stays on the wrapper**, or is dropped where
the wrapper is not in the launch path; it is no reason to keep the wrapper for state.

## Proposed design

Gate and mapping (only when `session.start` reports `isInteractive`; ignore every event
with an `agentId`):

| Mod signal                                                                | State reported                                                         |
|---------------------------------------------------------------------------|------------------------------------------------------------------------|
| `classic.SessionStart`                                                    | `starting` (`compact` source keeps the state)                          |
| `turn.start`                                                              | `working`                                                              |
| `classic.PermissionRequest`                                               | `waiting_for_permission`, or `waiting_for_input` for `AskUserQuestion` |
| `ui.render{ToolUse}` with `isRunning`, or the `tool.call` chain returning | `working`                                                              |
| `session.compact`; manual `classic.PostCompact`                           | `working`; `idle`                                                      |
| `turn.complete` (aborted or not)                                          | `idle`                                                                 |
| `session.end`                                                             | `ended`                                                                |

Wire contract: a new `omni-dev sessions report` reads one JSON object on stdin,
`{ "session_id": …, "cwd": …, "model": …, "state": … }`, takes the pid from its parent as
`sessions hook` does, and sends `observe` with a `StreamState`-style event. It prints
nothing and always exits 0. The mod never sends tool names, prompts or results.

Registry rules the design must add:

- A mod report must not latch `SessionEntry::streamed`. That flag (set for a Claude
  `StreamState` report) removes a session from the pid-liveness TTL exemption, so an idle
  terminal session would age out after 5 minutes. Use a distinct marker.
- A session reported by the wrapper is never also reported by the mod (the gate), and a
  state older than the last report from the same feed is dropped.
- Reports carry the state, so a lost or aborted one is repaired by the next; the mod's
  drainer retries once and coalesces to the latest.
- Feed 1 stays installed beside the mod (a terminal session's liveness, start and end still
  come from it) and its `PermissionRequest` mapping learns `AskUserQuestion`, so the two
  never disagree about a classification.
- The install detects a stale baked path and rewrites it, as `install-hooks` does for a
  moved sink (#1927).

## Not tested

- **The real VS Code chat panel**, Desktop's Code tab, Remote Control and cloud sessions.
  A recipe for the panel: put `CLAUDE_CODE_PLUGIN_DIRS` in the settings `env` and compare
  the tree badge against the window while answering a permission prompt late.
- `allowManagedModsOnly` and `allowManagedHooksOnly` (they need root-owned managed
  settings), and the three-crash mass unload (it needs untraceable crashes).
- `/reload-plugins` in a live session (it needs an authenticated TUI with the plugin
  installed), and the Desktop-specific render behaviour of `ToolUse`.
- `$.http.fetch` over the daemon socket from inside a mod (only a raw-socket client).

## Follow-ups

1. **#2151**: the mod feed for interactive sessions: the gate, the mapping, `sessions
   report`, the registry marker, `install-mod`/`uninstall-mod`, the ADR, the docs and a
   `plugin validate` CI check.
2. **#2152**: `claude-wrap` observes nothing under a PTY, so `worktrees ui` Claude tabs are
   not an authoritative feed. This change added a caveat to CLAUDE.md and the sessions guide;
   ADR-0072 and the two code comments (`worktrees/ui/mod.rs`, `terminal/mod.rs`) said
   otherwise and were left to #2152, since an ADR amendment and a code edit are outside
   a docs-only spike. #2152 has since corrected them.
3. **#2153**: `claude-wrap` reports a stale `working` after an SDK `set_model`. Fixed
   since: a `user` line the CLI writes is never read as work.
