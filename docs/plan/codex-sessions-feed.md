# Codex Sessions Feed

**Status:** Aspirational — investigation complete (#1861); implementation not started, tracked in the follow-up issues listed at the end
**ADRs:** [ADR-0052](../adrs/adr-0052.md) · [ADR-0057](../adrs/adr-0057.md) · [ADR-0087](../adrs/adr-0087.md)

## Overview

The daemon's sessions service ([ADR-0052](../adrs/adr-0052.md)) tracks Claude Code
sessions across every terminal and VS Code window with a coarse live state. Codex
now ships lifecycle hooks with a payload shaped like Claude Code's, and #1861 asked
whether they can feed the same service. This document is the answer: a tested
event/state matrix per Codex surface, the accuracy limits each surface carries, the
sink and wire-contract changes needed, and the follow-up issues. The decisions
themselves are in [ADR-0087](../adrs/adr-0087.md) (Proposed).

The headline finding is that **Codex sessions are already reaching the daemon**:
wiring `omni-dev sessions hook` into `~/.codex/hooks.json` works today because the
payload field names match, but the sessions land **untagged** — the registry,
tray, CLI and both UIs present every entry as a Claude session — and the two
Codex-only events that carry the most useful signal (`PermissionRequest`,
`Interrupt`) are silently dropped by the sink
([`session_event_for`](../../src/cli/sessions.rs)).

## Test setup

| Surface             | Binary / version                                                                    | How exercised                                        |
|---------------------|-------------------------------------------------------------------------------------|------------------------------------------------------|
| CLI (`codex exec`)  | `codex-cli 0.155.1` (Homebrew)                                                      | Scripted, 5 runs                                     |
| CLI (interactive)   | `codex-cli 0.155.1`, `--no-alt-screen`                                              | PTY driver, 15 runs                                  |
| VS Code extension   | `openai.chatgpt-26.5908.31748` (bundled `codex 0.154.0-alpha.6.1`)                  | Driven by hand, 6 chats                              |
| Codex Desktop       | `ChatGPT.app` Codex Framework `152.0.7977.83` (bundled `codex 0.154.0-alpha.6.2`)   | Driven by hand, 2 chats                              |

Capture method: a field-only shell hook registered on all twelve events in
`~/.codex/hooks.json` (matcher-less, `timeout: 3`), appending one line per
invocation with `hook_event_name`, `session_id`, `cwd`, `model`, `turn_id`,
`permission_mode`, `source`, `reason`, `tool_name`, `tool_use_id`,
`stop_hook_active`, `agent_id`, `agent_type`, `trigger`, `transcript_path`, and the
payload's key *names*. It never recorded `prompt`, `tool_input`, `tool_response` or
`last_assistant_message`, and the log lived outside the repository. The existing
`omni-dev sessions hook` entries stayed installed throughout, so every run also
exercised the current sink.

Trust: an untrusted hook is **silently skipped** — `codex exec` ran it zero times
and printed no warning. One `/hooks` approval in the CLI covered the VS Code
extension without a second prompt (trust is stored per hook hash under
`~/.codex`, not per surface).

## Event/state matrix

Columns: whether the event fired on that surface; the state it should drive; and
whether that state is **confirmed** (the event *is* the state) or **inferred**
(the event merely bounds it). `SessionState` names are the existing enum in
[`src/sessions.rs`](../../src/sessions.rs).

| Codex event         | CLI exec | CLI TUI | VS Code  | Desktop  | Maps to                  | Kind      | Notes                                                                                                   |
|---------------------|----------|---------|----------|----------|--------------------------|-----------|---------------------------------------------------------------------------------------------------------|
| `SessionStart`      | ✅ `startup` | ✅ `startup`/`resume` | ✅ `startup` | ✅ `startup` | `starting`   | confirmed | `resume` reuses the **same** `session_id`; `/new` starts a **new** id with `source=startup` (not `clear`). Not observed on `/compact`. |
| `UserPromptSubmit`  | ✅       | ✅      | ✅       | ✅       | `working`                | confirmed | Carries `turn_id`. A message queued during a turn fires it after the turn's `Stop`.                    |
| `PreToolUse`        | ✅ `Bash` | ✅ `Bash`, `request_user_input` | ✅ `Bash`, `webrun` | ✅ `Bash`, `webrun`, `update_goal`, `request_user_input` | `working`; **`waiting_for_input`** when `tool_name == "request_user_input"` | confirmed | The clarifying-question UI is a tool call; in the TUI it auto-resolves after 60 s and needs the `default_mode_request_user_input` feature (under development, off by default); **Desktop offers it in default mode** (answered by hand in 9 s). |
| `PermissionRequest` | ✅ with `--approve-for-me`; ❌ with `never` | ✅       | ✅ human prompt and Approve-for-me | ✅ (auto-resolved in 8 s) | `waiting_for_permission` | confirmed | Fires **after** a sandbox-denied `PreToolUse`/`PostToolUse` pair, alongside a second `PreToolUse`. Also fires under automatic review (`--approve-for-me`) and resolves ~4 s later with the same `permission_mode=default` — indistinguishable from a human prompt. |
| `PostToolUse`       | ✅       | ✅      | ✅       | ✅       | `working`                | inferred  | The only signal that an approval was **granted** or an input answered: no resolved/denied event exists. |
| `Stop`              | ✅       | ✅      | ✅       | ✅       | `idle`                   | confirmed | `stop_hook_active=false` on a natural end. In the TUI it is **not** fired on an Esc-deny, a Ctrl-C, or an interrupted turn; VS Code's Deny button *does* end in a `Stop`. |
| `Interrupt`         | n/a      | ✅      | not tested (Deny fires `Stop`) | not tested | `idle`     | confirmed | Fired for Esc mid-turn, Ctrl-C mid-turn, **Esc on an approval prompt** ("No, and tell Codex what to do differently") and Ctrl-C on one. The rollout records `turn_aborted`. The IDE stop button was not exercised. |
| `SessionEnd`        | ✅ `other` | ✅ `other` | ✅ `other` (chat archived) | ✅ `other` (chat archived) | `ended` | confirmed | `/exit`, Ctrl-C×2 and `exec` completion fire it; `/exit` ends every thread the TUI opened (one per `/new`). **SIGHUP (terminal tab closed), SIGTERM and SIGKILL fire nothing.** No `model`/`permission_mode` in the payload. The documented 30-minute idle end did **not** fire in a TUI left idle for 32 minutes; its only `SessionEnd` came from the `/exit`. |
| `PreCompact`        | —        | ✅ `manual` | not tested | not tested | `working`       | inferred  | Bracketed by `Stop`s; no `SessionStart(compact)` observed despite the docs.                             |
| `PostCompact`       | —        | ✅ `manual` | not tested | not tested | `working`       | inferred  |                                                                                                         |
| `SubagentStart`     | —        | not tested | ✅ `agent_type=default` | not tested | `working` (parent) | inferred | Carries the **parent's** `session_id` plus `agent_id` (the child thread id). The child fires **no** `SessionStart`, and its own tool calls report the parent's `session_id` too — subagents never create registry entries. |
| `SubagentStop`      | —        | not tested | ✅       | not tested | `working` (parent)     | inferred  | Spawn/join are ordinary tool calls (`collaborationspawn_agent` / `collaborationwait_agent`).           |

Payload facts common to every event: `session_id` (UUIDv7), `cwd`, `model`,
`permission_mode` (`default` interactively; `bypassPermissions` for `codex exec`
even with a read-only sandbox), `transcript_path` = the rollout file
(`~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<id>.jsonl`), and `turn_id` from
`UserPromptSubmit` onwards. `SessionEnd` carries only `session_id`, `cwd`,
`reason`, `transcript_path`.

### Observed sequences

Each line is one captured session. Same-second events are in log order.

| Scenario                                   | Sequence                                                                                                 |
|--------------------------------------------|----------------------------------------------------------------------------------------------------------|
| VS Code, sandboxed command, chat archived  | `SessionStart · UserPromptSubmit · PreToolUse · PostToolUse · Stop · (11 s) · SessionEnd`; the rollout moves to `archived_sessions/` |
| VS Code, plugin tool calls                 | `PreToolUse`/`PostToolUse` pairs with `tool_name=webrun` — plugin and MCP tools are ordinary tool events  |
| VS Code, two subagents                     | `PreToolUse/PostToolUse(collaborationspawn_agent) ×2 · SubagentStart ×2 · child Bash pair · SubagentStop ×2 · PostToolUse(collaborationwait_agent) · Stop`, all under the parent id |
| VS Code, human prompt, **Deny**            | `… PreToolUse · PermissionRequest · (2 m 15 s) · Stop` — no `PostToolUse`, no `Interrupt`; the model answers and the turn ends |
| VS Code, home-dir write, sandbox-denied    | `PreToolUse · PostToolUse · Stop` — the model gave up without escalating, so no prompt and no file (three attempts) |
| VS Code, "Approve for me", home-dir write  | `… PreToolUse · PostToolUse · PreToolUse · PermissionRequest · (6 s) · PostToolUse · Stop` — identical to the CLI |
| Desktop, out-of-workspace write            | `… PreToolUse · PermissionRequest · (8 s) · PostToolUse · Stop` — same shape as the CLI and VS Code       |
| Desktop, `request_user_input` (default mode) | `UserPromptSubmit · PreToolUse(request_user_input) · (9 s, answered) · PostToolUse · Stop`             |
| Desktop, sandboxed command                 | `SessionStart · UserPromptSubmit · PreToolUse · PostToolUse · Stop`; `originator=Codex Desktop`, `cwd` under `~/Documents/Codex/<date>/<slug>` |
| `exec`, read-only, command allowed         | `SessionStart · UserPromptSubmit · PreToolUse · PostToolUse · Stop · SessionEnd`                         |
| `exec`, read-only, write blocked           | same — the failure is an ordinary `PostToolUse`; no `PermissionRequest` under `approval: never`          |
| `exec --approve-for-me`, out-of-tree write | `… PreToolUse · PermissionRequest · (4 s) · PostToolUse · Stop · SessionEnd`                             |
| TUI, approve (`y`)                         | `… PreToolUse · PostToolUse · PreToolUse · PermissionRequest · PostToolUse · Stop`                        |
| TUI, deny (Esc on prompt)                  | `… PreToolUse · PostToolUse · PreToolUse · PermissionRequest · Interrupt` — then idle, no `Stop`          |
| TUI, Ctrl-C on prompt                      | identical to deny                                                                                        |
| TUI, Esc / Ctrl-C mid-turn                 | `SessionStart · UserPromptSubmit · Interrupt`                                                            |
| TUI, `/exit` mid-turn                      | `… Interrupt · SessionEnd`                                                                               |
| TUI, SIGHUP or SIGTERM while idle          | `SessionStart · UserPromptSubmit · Stop` — no `SessionEnd`                                               |
| TUI, idle 32 min then `/exit`              | `… Stop · (32 min) · SessionEnd` — nothing at the 30-minute mark                                          |
| TUI, `codex resume <id>`                   | `SessionStart(resume) · UserPromptSubmit · Stop · SessionEnd` under the **original** id                  |
| TUI, `/compact`                            | `Stop · PreCompact(manual) · PostCompact(manual)`                                                        |
| TUI, `/new` then `/exit`                   | new-id `SessionStart(startup) … Stop`, then `SessionEnd` for **both** ids                                |
| TUI, `request_user_input` (feature on)     | `PreToolUse(request_user_input) · (60 s auto-resolve) · PostToolUse · Stop · UserPromptSubmit · Stop`    |

Cells marked *not tested* (the TUI and Desktop subagent rows) were not exercised;
every other cell cites a captured session. The IDE and Desktop runs matched the
CLI event for event wherever the same scenario was run, which is what the shared
`hooks.json` and the shared trust store predict.

## Accuracy limits

- **Approval resolution is inferred.** Granting an approval produces no event of
  its own; the next `PostToolUse` is the first sign. Denying it produces
  `Interrupt` in the TUI (Esc is "tell Codex what to do differently", which
  aborts the turn) but a plain `Stop` in VS Code (the Deny button lets the model
  answer first) — never a `PostToolUse` for the denied call. Both correctly read
  `idle`, and a denied session is indistinguishable from an interrupted one.
- **Automatic review flashes `waiting_for_permission`.** `--approve-for-me` fires
  `PermissionRequest` with `permission_mode=default`, exactly like a human prompt,
  and resolves it a few seconds later. The tree will show a short false "waiting"
  cue. Under `approval: never` (`codex exec` default) the event never fires, so
  headless runs are clean.
- **`waiting_for_input` is surface-dependent.** In the TUI `request_user_input`
  is only offered when `default_mode_request_user_input` is enabled (marked under
  development in 0.155.1) and auto-resolves after 60 s; Desktop offers it in
  default mode and waits for the answer. The signal is exact when present.
- **`Stop` is not always the end of activity.** On Desktop a turn resumed
  twice after a `Stop` with no `UserPromptSubmit` (a goal/follow-up feature);
  the next `PreToolUse` flips the row back to `working`, so the mapping holds
  but an `idle` cue can be brief.
- **A signalled process ends silently.** SIGHUP — what closing a terminal tab
  delivers — SIGTERM and SIGKILL all fire no `SessionEnd`; only `/exit`, a double
  Ctrl-C and `exec` completion do. The entry ages out on the registry's 5-minute
  idle TTL, the same accepted limitation as Claude's hook feed, and the reason a
  rollout watcher or process-liveness check is the only way to end such a session
  sooner.
- **`/new` leaves the old thread visible.** The previous thread stays `idle`
  until the TUI exits (its `SessionEnd` arrives with the new thread's), so a
  window that has cycled threads shows more sessions than it has tabs until the
  TTL reaps them.
- **`SessionEnd`/`Interrupt` hooks get 1 s** (max 3 s) — the sink's 2 s
  `HOOK_TIMEOUT` is longer than Codex's deadline for those two events. Because the
  POST is fire-and-forget the overrun is harmless (Codex kills the hook, nothing is
  lost that would have arrived anyway), but the install path should pass
  `timeout: 3` for them and the docs should say so.
- **Sandbox-denied attempts look like activity.** A write blocked by the sandbox
  is a normal `PreToolUse`/`PostToolUse` pair, so a session that is really stuck
  retrying reads as `working`; only the escalation prompt turns it `waiting`.
- **Surface is not in the payload.** `SessionStart.source` is the start reason,
  not the surface. The rollout's head-of-file `session_meta.originator`
  (`codex-tui` / `codex_vscode` / `Codex Desktop`) is the only place the surface
  is recorded, reachable via `transcript_path`.

## Sink design

Recommend **one sink with an explicit provider**, `omni-dev sessions hook
--provider codex`, rather than a separate `codex-hook` subcommand:

- The provider cannot be inferred from the payload safely — the field names are
  Claude's, and the one Codex-only field (`turn_id`) is absent on `SessionStart`
  and `SessionEnd`, exactly the two events that create and end an entry.
- `report`, `HookPayload` and the fire-and-forget POST are reused verbatim; only
  `session_event_for` grows a provider-aware arm.
- Required stdout: none. Exit 0 with empty stdout is a no-op for every event —
  verified for `PermissionRequest` (the prompt still appeared) and for
  `Stop`/`Interrupt` (no continuation was injected).

Mapping (all onto existing `SessionEvent`s, so the engine's state machine is
unchanged): `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`,
`Stop`, `SessionEnd` → the same-named events; `PermissionRequest` →
`Notification(PermissionPrompt)`; `PreToolUse` with `tool_name ==
"request_user_input"` → `Notification(AgentNeedsInput)`; `Interrupt` → `Stop`;
`SubagentStart`/`SubagentStop`/`PreCompact`/`PostCompact` → `PostToolUse`
(a working-state heartbeat). If reviewers prefer the event log to be truthful
over the diff being minimal, a `SessionEvent::Activity` variant is the honest
spelling of the last group.

Install/uninstall: `omni-dev sessions install-hooks --provider codex` (or
`install-codex-hooks`) targets `$CODEX_HOME/hooks.json`, defaulting to
`~/.codex/hooks.json`. The file has the **same shape** as Claude's `settings.json`
`hooks` object, so `merge_hooks`/`remove_hooks`/`read_settings`/`write_settings`
apply unchanged with a Codex event list: the seven Claude events minus
`Notification`, plus `PermissionRequest` and `Interrupt` (with `timeout: 3`) and
the four compaction/subagent events. `PreToolUse`/`PostToolUse` keep the `*`
matcher. After install the user must trust the hook once via `/hooks`; the
command must document that, since an untrusted hook is skipped without any
message.

## Data contract

Daemon (`src/sessions.rs`, additive on the wire):

- `provider: "claude" | "codex"` on `ObserveRequest`, `EndRequest` and
  `SessionEntry`; `#[serde(default)]` to `claude`, always serialised. An old sink
  or client keeps working; an old daemon ignores the field.
- Registry key becomes `(provider, session_id)`. Both providers use UUIDs, so a
  collision is improbable, but the key is the contract that makes a provider a
  first-class dimension rather than a label; it ripples into `focus_folder`,
  eviction and the tray action id (`focus:<provider>:<session_id>`).
- `Source` is unchanged: the `cwd` join already places an IDE Codex session on
  its VS Code window. A Desktop chat started from the app lives under
  `~/Documents/Codex/<date>/<slug>/`, which no window has open, so it falls to
  `terminal` — a wrong label, but a harmless one until a `desktop` variant is
  worth its own tray/UI treatment. If added, it should come from the
  `originator` enrichment below, never from the path shape.
- Optional enrichment: read `session_meta.originator` from the head of
  `transcript_path` at `observe` time (blocking thread, schema-tolerant, the
  `relocate::transcript_preview` precedent) to record the surface.

UI:

- Tray: title `Agent Sessions`; each row prefixed by a provider glyph.
- `omni-dev sessions list`: a `PROVIDER` column.
- VS Code (`sessionCounts.ts`): `classifyModel` gains a `g` family for `gpt-*`
  ids (today they fall to `*`); the tooltip prefix `Claude:` becomes the provider
  name; `decorations.ts` keeps its `claude` query key for wire compatibility (noted
  as debt). Glyphs and colours stay state-driven, so nothing changes there.
- TUI (`render.rs::sessions_summary`): `({provider} {model}, {source})`.

## Supplementary signals

- **Codex rollout watcher** (a Feed 2 twin over `$CODEX_HOME/sessions/**`,
  `archived_sessions/` excluded — archiving a chat *moves* the rollout there, so
  the move must read as an end, not a discovery, and a stored `transcript_path`
  goes stale at that moment). Better than Claude's: `session_meta` at the head
  of every file yields `cwd`, `originator` and `source`, and `source` being an
  object (`{"subagent": …}`) identifies subagent threads to skip — half of the
  rollouts on the test machine are guardian/`thread_spawn` subagents. Growth still
  parses only size/mtime. It covers sessions that predate the daemon and the
  killed-process gap, but **not approvals**: the rollout records the
  `custom_tool_call` and its output only, never the approval request or decision.
- **App Server** (`thread/list` polling; `activeFlags` `waitingOnApproval` /
  `waitingOnUserInput`). Rejected for now. The VS Code extension and Desktop each
  spawn a **private stdio** `codex app-server` child, so no outside observer can
  reach their threads; the shared daemon (`codex app-server daemon`,
  `~/.codex/app-server-control/app-server-control.sock`) is opt-in and was not
  running. Prior art (codex-agents) warns that subscribing duplicates actionable
  server requests to every client, so only `thread/list` polling is safe. It
  remains a candidate for sessions omni-dev itself launches (a `codex` tab in
  `worktrees ui` could connect via `--remote`), the ADR-0057 analogue.
- **Prior art.** Herdr treats the terminal screen as the authority because hooks
  "can miss permission approval results, escape interrupts" — this investigation
  confirms the first (resolution is inferred) and refutes the second on 0.155.1
  (`Interrupt` fires). Huginn installs only `SessionStart`/`UserPromptSubmit`/
  `Stop` and uses a 20-second pending-tool fallback for approvals; with
  `PermissionRequest` now available that fallback is unnecessary.

## Follow-ups

1. Provider tag on the wire, `PermissionRequest`/`Interrupt`/`request_user_input`
   mapping, `sessions hook --provider codex`, `install-hooks --provider codex`
   (docs/sessions-service.md, snapshots).
2. Provider labels in the tray, `sessions list`, VS Code (`classifyModel`,
   tooltip) and the TUI.
3. Codex rollout watcher with subagent filtering.
4. (Optional) App Server `thread/list` observer for sessions omni-dev launches.
