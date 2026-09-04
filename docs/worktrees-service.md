# Worktrees service

The omni-dev daemon hosts a **worktrees service**: it maintains the live,
authoritative set of repositories and git worktrees open across **every** VS Code
window, fed by a small first-party companion extension that reports from each
window. It is the daemon's third service, after the browser bridge and Snowflake.

Beyond the open-window `list`, the service exposes a **`tree`** view — every
repository and **all** of its git worktrees (open in a window or not), grouped by
repo and enriched with GitHub identity — and pushes it live to subscribers over
the control socket (the **`subscribe`** op). The companion extension renders that
tree in a Git Worktree Manager–style activity-bar view where **double-clicking** a
worktree focuses (or opens) its VS Code window via the **`open`** op. See
[ADR-0040](adrs/adr-0040.md) for the original service and
[ADR-0048](adrs/adr-0048.md) for the tree/subscription/UI expansion.

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
  register/heartbeat/unregister/list/first-folder operations. It also snapshots
  the distinct open **folders** (`open_folders`, the seed the `tree` op resolves
  to repos), holds the cross-window **`show_closed`** toggle (a lock-free
  `AtomicBool`, #1301), and carries a `tokio::sync::watch` **change-notify**
  counter (`subscribe_changes`) bumped whenever the visible set — or the toggle —
  changes, which drives the push subscription. A standalone `crate::worktrees`
  module, matching the browser bridge (`src/browser/`) and Snowflake
  (`src/snowflake/`) engine/adapter split.
- `src/daemon/services/worktrees.rs` — `WorktreesService`, a thin `DaemonService`
  adapter over that engine: it routes control-socket ops to the registry, renders
  the tray menu/status, drives the VS Code launcher, computes the git enrichment,
  builds the repo/worktree **`tree`** (enumerating each repo's worktrees with
  `git2` and tagging its GitHub identity), and backs the **`subscribe`** stream.
  Cheap to construct; persists nothing. All the git disk I/O runs on a blocking
  thread, never under the registry lock.
- `src/daemon/service.rs` + `src/daemon/server.rs` — the streaming machinery
  shared by all services: the `ServiceStream` trait capability (an optional
  `subscribe` on `DaemonService`) and the server's `run_stream` drive loop that
  pushes an initial snapshot, then a fresh one on each change-notify or periodic
  tick, diffing so identical frames are never re-sent.
- `src/cli/worktrees.rs` — the read-only `omni-dev worktrees list` and
  `omni-dev worktrees tree` clients.
- The companion VS Code extension in [`editors/vscode/`](../editors/vscode/) —
  both a **writer** and a **reader**. As a writer it `register`s on activation,
  `heartbeat`s every ~10 s, and `unregister`s on deactivation. As a reader it
  holds a long-lived `subscribe` connection and renders the pushed `tree`
  snapshots as an activity-bar tree view (see [Tree view](#tree-view-companion-ui)),
  talking to the daemon socket directly from each window.

### Data flow

The open-window registrations are the liveness source; the `tree` view is derived
from them (each open folder → its repo → all that repo's worktrees) and pushed live
to subscribers:

```
                         registrations                         list / tree (pull)
VS Code window A ─┐  (register/heartbeat/    ┌─────────────────────────►  CLI / tray
VS Code window B ─┼──── unregister) ────────►│ omni-dev daemon         │
VS Code window C ─┘                          │ (worktrees service)     ├── subscribe ──►  companion
                                             │ registry + git2 enrich  │   (push tree)    tree view
      ▲                                      └─────────────────────────┘                     │
      └──────────────────  open op: focus/open a worktree's window (code <path>)  ◄──────────┘
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
primary folder, and how long ago the window was last seen. The REPO column shows
the daemon-computed **main repository** (a linked worktree's parent repo, not its
worktree-folder basename) when available, falling back to the companion-reported
`repo`. The branch and sync columns are likewise computed by the daemon from the
worktree on disk — see [Git enrichment](#git-enrichment) — so they reflect the
live branch rather than whatever the companion happened to report. `-o json`
carries the same fields plus the companion-reported `title`.

The companion extension feeds the registry; the CLI only reads it. If the daemon
is not running, `worktrees list` reports the connection failure (the companion, by
contrast, no-ops silently).

`worktrees tree` shows the same live data inverted into the repo/worktree view —
every repository derived from the open windows, and **all** of each repo's
worktrees (open or not):

```bash
# Every repository and all its worktrees, grouped by repository.
omni-dev worktrees tree

# Machine-readable JSON (byte-identical to the on-socket `tree` payload).
omni-dev worktrees tree -o json
```

The table prints a header line per repository — its **main-repo name**, its GitHub
`owner/name` (when `origin` is a `github.com` remote), and its root path — then one
indented row per worktree: a `*` marks the **main working tree**, followed by the
branch, its `+ahead -behind` sync state, an `open` flag when a live window has that
worktree open, and the worktree path. A repo with no open window contributes no
rows, so the tree lists exactly the repos currently in play (the open-window-derived
v1 model — [ADR-0048](adrs/adr-0048.md)). The `+ahead -behind` state is **not** in
the (cheap) `tree` payload — `worktrees tree` fetches it on demand via the
`ahead-behind` op and folds it in (#1306), for both the table and `-o json`; so
`-o json` is the `tree` payload described under
[the contract](#companion-contract-for-the-extension-and-other-clients) with
`ahead`/`behind` merged back onto each worktree. When the branch is genuinely
behind the repository's remote default branch (#1457), the sync state gets a
trailing `main-N` fragment — e.g. `+1 -3 main-12` — the CLI counterpart of the
tree view's `⇊N` glyph, folded in from the same `ahead-behind` reply's
`main_behind` field and independent of `+ahead -behind`.

`worktrees focus` raises the VS Code window for a worktree folder from the CLI —
the same capability as the tray's per-window focus action, now reachable on
Linux/headless too (#1113):

```bash
# Focus the window for a worktree folder (a path from `worktrees tree`/`list`).
omni-dev worktrees focus /path/to/worktree
```

It resolves the path to an absolute directory client-side (a clear error if it
doesn't exist), then sends the daemon's **`open`** op, which runs `code <path>` via
the shared launcher resolution (`OMNI_DEV_VSCODE_BIN` → well-known paths → `code`);
VS Code reuses the already-open window rather than opening a second one.

`worktrees close` closes a worktree's window and, for a **linked** worktree,
deletes it — the daemon's two-phase `close` op driven from the CLI (#1361). It is
the one destructive command; all git logic stays in the daemon ([ADR-0049](adrs/adr-0049.md)):

```bash
# Safety check only — print the report, delete nothing.
omni-dev worktrees close /path/to/worktree --dry-run

# Delete a linked worktree (interactive y/N confirm unless --yes).
omni-dev worktrees close /path/to/worktree --yes

# Only close the owning window(s); never delete (the main-tree case).
omni-dev worktrees close /path/to/worktree --window-only
```

The path is resolved client-side. For a delete, the command first runs the
side-effect-free **phase-1** safety check and prints the report (whether the target
is `removable`, whether it is the main working tree, whether a window has it open,
and any `risks`/`info` notes); it then confirms interactively (unless `--yes`) and
sends the **phase-2** execute. A non-removable target (e.g. the main working tree,
which the daemon refuses to delete) prints the report and stops. A CLI process is
never a VS Code window, so it omits `requester_key`: the daemon treats the close as
cross-window, signals every owning window to close, waits (bounded ~20s) for them to
unregister, then prunes.

`worktrees rebase` rebases worktrees onto the repository's **remote default
branch**, fetching that branch **exactly once per repository** up front (#1400,
[ADR-0055](adrs/adr-0055.md)). Unlike every other subcommand it runs **entirely
locally and never contacts the daemon** — since #1415 that is a *choice*, not a
necessity: it means a batch rebase works with no daemon running at all, and it
keeps `--all` and `--onto <ref>` out of the wire protocol. (The daemon hosts the
same engine behind its `rebase` op, which is what the tree view drives; see
[ADR-0059](adrs/adr-0059.md).) Following [ADR-0003](adrs/adr-0003.md), the fetch
and rebase shell out to the user's `git` (git2 only reads); the batch logic lives
in `src/git/worktree_rebase.rs`.

```bash
# Preview: fetch once per repo, report what would be rebased, change nothing.
omni-dev worktrees rebase --all --dry-run

# Rebase every worktree of the current repo, main included (interactive y/N
# unless --yes).
omni-dev worktrees rebase --all

# Rebase specific worktrees onto an explicit ref (still fetched once up front).
omni-dev worktrees rebase /path/to/wt-a /path/to/wt-b --onto origin/release

# Stash uncommitted tracked changes around each rebase instead of skipping.
omni-dev worktrees rebase --all --autostash

# Leave a conflicting worktree mid-rebase to resolve in place, rather than
# aborting it back to where it started.
omni-dev worktrees rebase --all --keep-conflicts
```

Selection is explicit: pass `<PATH>...` **or** `--all` (every worktree of the
current repo, main working tree included since [ADR-0060](adrs/adr-0060.md)). A
bare invocation with neither is a usage error, not a silent mass-rebase. Each worktree is reported as `rebased` / `up-to-date` /
`skipped(<reason>)` / `conflict` / `fetch-failed` (`-o json` for the machine shape).
A dirty worktree is skipped with a reason (unless `--autostash`); a detached HEAD or
an in-progress rebase/merge is skipped; a rebase that hits conflicts is by default
`git rebase --abort`-ed so the worktree is **left exactly as it was** and reported,
and the batch continues for the rest. A rebase rewrites branch history, so the
command confirms by default ([ADR-0027](adrs/adr-0027.md)); `--dry-run` still fetches
(non-destructive) so the behind-count preview is accurate. This subcommand takes no
`--socket` (it never opens the control socket).

**`--keep-conflicts`** (#1415) flips the conflict handling: instead of aborting, the
worktree is left **mid-rebase**, with its conflict markers on disk, to be resolved
and finished with `git rebase --continue`. The batch continues either way. Aborting
stays the default so an unattended run never leaves a worktree needing attention by
surprise; the tree view's **Rebase on main** always opts in, since resolving in
place is the point there.

The tree view surfaces the same engine as **Rebase on main** — see
[Rebase onto main](#rebase-onto-main). Since #1415 it drives the daemon's `rebase`
op rather than spawning this CLI in a terminal.

`worktrees push` publishes worktrees' branches to their upstreams, force-pushing
**with a lease** where a rebase rewrote history (#1443,
[ADR-0061](adrs/adr-0061.md)) — the other half of the rebase workflow above. Like
`rebase` it runs **entirely locally and never contacts the daemon**, by choice: a
batch push works with no daemon running, and `--all` stays out of the wire
protocol. (The daemon hosts the same engine behind its `push` op, which is what
the tree view drives.) The batch logic lives in `src/git/worktree_push.rs`.

```bash
# Preview: classify every worktree, publish nothing. Contacts no remote at all.
omni-dev worktrees push --all --dry-run

# Publish every worktree of the current repo (interactive y/N unless --yes).
omni-dev worktrees push --all

# Publish specific worktrees.
omni-dev worktrees push /path/to/wt-a /path/to/wt-b
```

Selection is explicit: pass `<PATH>...` **or** `--all`. A bare invocation with
neither is a usage error, not a silent mass-push. Each worktree is reported as
`up-to-date` / `would-push` / `would-force` / `would-create` / `pushed` /
`created` / `rejected` / `skipped(<reason>)` (`-o json` for the machine shape). A
**dirty worktree is not skipped** — a push publishes commits, not the working
tree — while a detached HEAD (which includes one sitting mid-rebase), a
non-worktree path, and a branch with no remote to publish to are. A push publishes
history to everyone, so the command confirms by default
([ADR-0027](adrs/adr-0027.md)); the prompt names how many of the batch are force
pushes separately from the total. This subcommand takes no `--socket` (it never
opens the control socket).

**The lease, and why it is two flags.** Every non-fast-forward goes out as
`git push --force-with-lease --force-if-includes`. A bare `--force-with-lease`
leases against your *local* remote-tracking ref, so any background fetch that
refreshes it silently renews the lease — and VS Code's built-in Git extension does
exactly that if `git.autofetch` is on. `--force-if-includes` closes that hole by
additionally checking the remote tip was integrated locally, and per `git-push(1)`
it is **not** implied: it must be passed explicitly, and it is a no-op unless
`--force-with-lease` is given valueless (which is why this command never passes
the `=<refname>:<expect>` form). There is **no `--force`**: `omni-dev worktrees
push --force` is a parse error. When the lease refuses, the row says so and names
`git fetch` plus a rebase as the fix — the tool offers no way to push harder.

**The remote default branch is never force-pushed**, whichever worktree has it
checked out ([ADR-0061](adrs/adr-0061.md) §4). This deliberately inverts
[ADR-0060](adrs/adr-0060.md), which made the main working tree a valid *rebase*
target: a rebase rewrites only local history and is `git reflog`-recoverable, but
a force-push publishes that rewrite to everyone. The gate is on the **branch name**
rather than `is_main`, so a linked worktree with `main` checked out is refused too
— and a *fast-forward* onto the default branch stays allowed, being an ordinary
push. A branch with no upstream is published with `--set-upstream` rather than
refused. Pushing to a remote other than the branch's own upstream is not
supported.

`worktrees merge-queue` enqueues eligible worktrees' pull requests into the GitHub
merge queue — the daemon's two-phase `merge-queue` op driven from the CLI (#1401).
It takes **any number** of paths, since the daemon evaluates and enqueues the whole
selection in one op (see [Merge queue](#merge-queue) for the gates):

```bash
# Report eligibility only — enqueue nothing.
omni-dev worktrees merge-queue /path/to/worktree --check

# Enqueue several (interactive y/N confirm unless --yes).
omni-dev worktrees merge-queue /path/to/wt-a /path/to/wt-b --yes
```

Each path is resolved client-side. The command first runs the side-effect-free
**phase-1** check and prints the report — one line per eligible worktree (`PR #N
[branch] path`) and per skipped one (`[kind] path — detail`) — then confirms
interactively (unless `--yes`) and sends the **phase-2** execute, which re-validates
every gate before enqueuing. `--check` stops after the report. The final summary
counts queued/failed/skipped, marking any PR that was `(already queued)`.

`worktrees reposition` moves and resizes worktrees' open VS Code windows to match a
reference window, on macOS (#1407) — see
[Window repositioning](#window-repositioning-macos) for the permission setup and the
matching rules:

```bash
# Show which OS window each worktree resolves to; move nothing.
omni-dev worktrees reposition --reference /path/to/ref /path/to/wt-a --dry-run

# Move the named worktrees' windows onto the reference window's geometry.
omni-dev worktrees reposition --reference /path/to/ref /path/to/wt-a /path/to/wt-b

# Put back whatever the last reposition moved.
omni-dev worktrees reposition --undo
```

`--reference` is required unless `--undo` is given, and the reference window is never
itself moved. Paths are resolved client-side and then mapped to the keys of the
windows that have them open — the op addresses *windows*, since geometry belongs to
the OS window rather than to the worktree — so a worktree with no open window is an
error here rather than a silent skip. All Accessibility work happens in the **daemon**,
which is what holds the permission.

`worktrees reload` signals worktrees' open VS Code windows to reload themselves
(#1417) — the batch form of `Developer: Reload Window`; see
[Reloading windows](#reloading-windows):

```bash
# Reload the named worktrees' windows.
omni-dev worktrees reload /path/to/wt-a /path/to/wt-b

# Machine-readable: { requested, signalled, unknown[] }.
omni-dev worktrees reload /path/to/wt-a -o json
```

Paths are mapped to window keys exactly as `reposition` does, and a worktree with no
open window is an error rather than a silent skip. The daemon only *marks* a
directive per target — each window acts on it on its next heartbeat, up to ~10s later
— so the output reports how many windows were **signalled**, never how many reloaded.

`worktrees show-closed` reads or sets the cross-window "show closed worktrees"
toggle (`set-show-closed`; the value rides the `tree` snapshot's `show_closed`):

```bash
omni-dev worktrees show-closed          # read the current value
omni-dev worktrees show-closed false    # hide closed worktrees everywhere
```

`worktrees tree --follow` (`-f`) streams live snapshots via the daemon's
`subscribe` push op, re-rendering the tree on every change until interrupted
(Ctrl-C) — the terminal equivalent of the editor tree view. It honours `-o json`
(one compact frame per line, an NDJSON stream).

`worktrees ui` launches a full-screen terminal UI over the same live data
(issue #1585) — a `ratatui` app that supersedes `worktrees tree --follow` for
interactive use:

```bash
omni-dev worktrees ui

# Against a non-default daemon socket.
omni-dev worktrees ui --socket /path/to/daemon.sock
```

Phase 1 was a read-only, live-updating tree view: one row per
repository/worktree, redrawn on every `worktrees`/`sessions` push, with a
status bar reporting each feed's connection state (`live` / `reconnecting` /
`polling (daemon predates live updates)`). It reuses the same `subscribe`
reconnect-with-backoff-and-legacy-daemon-fallback contract `tree --follow`
does, and the same row-severity ranking `worktrees tree`'s glyph table follows
(the tree view's `rowColorId`: red > yellow > green > muted). `q`/`Esc`/Ctrl-C
quits and always restores the terminal, even on an error path.

Phase 2 (this release) adds row navigation/marking (`↑`/`↓`/`j`/`k`, `space`
to mark), an action menu (`a`) filtered to the current selection and grouped
in the tree view's own `0_open`/`1_pr`/`2_claude`/`3_copy`/`9_close` order, a
row-colour picker (`c` to set, `C` to clear), and five daemon-free parity
commands ported from the tree view: **Open GitHub Repository**, **Copy
Directory**, **Copy Pull Request URL(s)** (built entirely from the already-live
snapshot — no `gh` lookup, unlike the tree view), **Move/Copy Claude Session
Here** (pure `~/.claude/projects/` filesystem manipulation; the cursor row is
always the *source*, since a TUI has no "current window" to default a source
to the way the tree view's version does — `src/sessions/relocate.rs`), and
**Focus/Open Worktree** (wraps the daemon's `open` op, identical to `worktrees
focus`). **Close Worktree**/**Close Window** are the two-phase check→confirm→
execute flow, fanned out client-side across every marked target since
`close`'s wire payload isn't batched. Embedded terminals, tabs/splits, mouse
handling and the rest of the tree view's VS Code-parity surface land in later
phases.

Finally, the **companion feed ops** — normally spoken by the VS Code extension —
are exposed as typed commands so scripted/headless companions and integration tests
can drive the registry's full lifecycle without VS Code (#1361). Each takes a
caller-supplied window `--key`:

```bash
omni-dev worktrees register --key <KEY> --folder /abs/path [--repo-name R] [--title T] [--pid N]
omni-dev worktrees heartbeat --key <KEY>    # prints `known` and any pending `close`/`reload`
omni-dev worktrees unregister --key <KEY>   # prints whether an entry was removed
```

The repository is `--repo-name`, **not** `--repo`: the latter is the global
`-C/--repo` path flag, and because clap propagates a global arg by its *id*, a
subcommand-local `repo` displaced it and made every `register` invocation that
used it panic (#1420). It is a wire-shape no-op — the op's payload key is still
`repo`.

Every command accepts `--socket` to target a non-default control socket. The
underlying ops are documented in the [companion contract](#companion-contract-for-the-extension-and-other-clients).

## Tray

On a macOS `menu-bar` build the service contributes a **"Worktrees" submenu**:
**one clickable line per open window** — the live stats and the focus action are
the same item, not two separate rows. Each line shows the **main repository**
name and, when the primary folder is a git repo, the live branch and ahead/behind
state:

- a normal checkout reads `omni-dev · branch (+2 -1)` (a middle dot);
- a **linked worktree** reads `omni-dev ⑂ branch (+2 -1)` — the `⑂` fork glyph
  sets it off from the main checkout, and the name is the **parent** repository
  it belongs to (not the worktree-folder basename);
- a window that is not a git repo falls back to its reported title.

Clicking a line spawns the VS Code CLI on that window's folder; since VS Code
reuses an already-open window, this focuses the right window rather than opening
a new one. A window with no workspace folder has nothing to open, so it stays a
non-clickable status line.

Focusing is **best-effort**. The launcher is resolved in this order:

1. `OMNI_DEV_VSCODE_BIN` (set this if your daemon runs under launchd with a
   minimal `PATH` and cannot find `code`);
2. well-known absolute locations (`/usr/local/bin/code`, `/opt/homebrew/bin/code`,
   the in-app `.../Visual Studio Code.app/.../bin/code`, `/usr/bin/code`);
3. bare `code` resolved via `PATH`.

If none works, the failure is logged and the rest of the tray keeps working.

The daemon resolves **`git`** the same way for the `rebase` op (#1415):
`OMNI_DEV_GIT_BIN` → well-known absolute locations
(`/opt/homebrew/bin/git`, `/usr/local/bin/git`,
`/home/linuxbrew/.linuxbrew/bin/git`, `/usr/bin/git`) → bare `git`. Set the
override if the daemon must use a specific `git` — macOS's minimal launchd `PATH`
does contain `/usr/bin/git`, so the fallback works, but it may not be the same
`git` your shell uses. `gh` has its own `OMNI_DEV_GH_BIN` on the same pattern.

## Tree view (companion UI)

The companion extension contributes a **"Worktrees" activity-bar view** — a
*Git Worktree Manager*–style tree, but fed live by the daemon and consistent across
every window (no per-window curation). It renders the `tree` payload:

```
▸ omni-dev            (github: rust-works/omni-dev)
    ● main            ↑2 ↓0                     ← window open (badge), main working tree
      issue-1300      ↑1 ↓3  #1300         ✗   ← linked worktree; open PR, checks failing (red ✗)
    ● issue-1250      ↑0 ↓0  #1250 draft   ●   ← draft PR, checks running (yellow ●)
▸ some-other-repo
      main
```

The `✓`/`✗`/`●` at the right of each row is a **colored** check badge (a VS Code
file decoration, not description text), which also tints that row's branch label.

- **Top level: repositories.** A GitHub repo shows a GitHub icon and its
  `owner/repo`; a non-GitHub repo is still listed by its main-repo name.
- **Children: worktrees.** Every worktree of that repo (main + linked), labelled
  by its branch with `↑ahead ↓behind` as the row description, and a three-way
  **open badge** icon (#1274): a **green tick** on the worktree open in *this*
  window, a **green dot** on one open in *another* window, and the plain branch
  glyph on a worktree with no live window. The current-window distinction comes
  from matching a worktree's `window_key` against this window's own key, and is
  carried by the glyph alone (#1433) — the tick and the dot share one colour
  because they report the same thing. The tick also wins its glyph outright, so a
  worktree left mid-rebase still shows you which row is yours. Any of the
  three glyphs can be recoloured per row — see [Row colours](#row-colours) — which
  changes the colour only, never which glyph is shown. The
  `↑ahead ↓behind` counts are **not** in the streamed snapshot — the extension
  fetches them **lazily when a repo is expanded** via the `ahead-behind` op (#1306),
  so only the worktrees you actually look at pay the divergence walk. A third,
  independent glyph — `⇊N` — reports commits behind the repository's **remote
  default branch** (typically `origin/main`), distinct from `↓`'s divergence
  from the branch's *own* upstream (#1457): a feature branch can be fully in
  sync with its own upstream while sitting well behind `origin/main`, and
  `⇊N` is the only passive, at-rest signal of that. It rides the same
  `ahead-behind` op (no extra round trip), computed with no `git fetch` — the
  same staleness contract the `↑`/`↓` counts already have — and is omitted
  when the branch's own upstream *is* already the resolved default branch (the
  common checked-out-`main` case), since `↓` already reports that divergence.
- **Double-click to focus/open.** Because the VS Code TreeView API has **no** native
  double-click event (a single click only selects), the companion implements it with
  a manual click-timer: a second click on the same worktree within ~400 ms sends the
  daemon `open` op, which runs `code <path>` — focusing the already-open window or
  opening a new one. Uniform for open and not-open worktrees.
- **Live.** The view updates itself as windows open and close and as branches
  change, driven by the [push subscription](#push-subscription) — no manual
  refresh. `↑ahead ↓behind` is re-fetched each time a repo's children are rendered
  (on expand, and on any snapshot that re-renders it), so it tracks the visible
  worktrees; a commit that moves a tip without otherwise changing the snapshot
  refreshes the counts on the next expand/render rather than instantly (#1306). A
  **Refresh** title-bar action does a one-shot `tree` fetch as a fallback when the
  subscription is momentarily down.
- **Hide worktrees without a window.** A second title-bar action toggles the view
  between showing **all** worktrees (the default — an *eye* icon that hides) and
  showing only those a VS Code window currently has open (an *eye-closed* icon that
  reveals). The filter is entirely client-side — the `repos` payload is unchanged —
  but its **state is daemon-backed** (#1301): the toggle command sends
  `set-show-closed`, the daemon holds the single cross-window value and carries it
  as `show_closed` on every `tree`/`subscribe` snapshot, and each window drives its
  button and filter from that snapshot. So a flip in one window **live-syncs to all
  the others** and a newly-opened window initializes correctly on its first frame —
  neither of which the earlier per-window `globalState` (read-once, no cross-window
  change event) could do. Because the value is in-memory, a daemon restart resets
  it to *show all*. Because repos are derived from open windows (each has ≥1 open
  worktree), hiding only trims closed children and never empties a repo or the tree.
- **Daemon-down degrades gracefully.** When the daemon is not running, the view
  shows a hint ("start it with `omni-dev daemon start`") rather than an error dialog,
  and the subscription reconnects with exponential backoff (500 ms → 10 s) once the
  daemon returns. An empty-but-connected daemon shows "No worktrees are open…".
- **Multi-select (#1357).** The view is `canSelectMany`, so ctrl/cmd+click and
  shift+click select several rows and every context-menu action acts on the whole
  selection — the view is a *cross-window aggregate*, so its natural verbs are
  plural. Three properties of VS Code's `(clicked, selected[])` contract shape the
  client code: the selection argument is passed **only** when >1 row is selected
  *and* the clicked row is one of them (so handlers fall back to the clicked node,
  and never concatenate the two); `when` clauses are evaluated against the
  **clicked** row alone (so a mixed selection reaches any handler, and every
  handler re-validates each node rather than trusting `viewItem`); and repo nodes
  can appear in any selection (handlers filter to what they understand). The
  ordering hazard is **acting on self**: a batch containing *this* window's worktree
  must handle it last and alone, or `workbench.action.closeWindow` (or, for **Reload
  Window**, `workbench.action.reloadWindow`) kills the extension host mid-loop and
  the remaining targets are silently skipped. Selection survives the
  live snapshot churn for free, since it is keyed by `TreeItem.id` — the stable
  repo-root/worktree-path identity — and not by node object identity.
- **Claude model-family marker** (#1448): when the [sessions service](sessions-service.md)
  attributes one or more Claude sessions to a worktree, its row description gets a
  trailing `[hsof*]`-style marker — **h**aiku, **s**onnet, **o**pus, **f**able, `*`
  for anything else — naming which model families are running there, in that fixed
  order, with only the letters actually in use. A repository row shows the union of
  its worktrees' markers, so `[so]` there means Sonnet is running in one worktree and
  Opus in another, not necessarily both in the same one. Idle counts the same as
  working — the marker's scope matches the session-state glyphs it sits beside — and
  a row with no attributed session shows no marker at all. Purely a client-side
  reclassification of the `model` id the daemon already sends on every session; no
  wire change was needed.
- **Four worktree verbs.** Window management and worktree deletion are separate
  actions (#1357): **Open Worktree** opens/focuses a window per selected worktree;
  **Close Window** closes the window of any selected worktree — main *or* linked —
  and deletes nothing; **Reload Window** reloads it instead (#1417), skipping any
  selected worktree with no window open; **Close Worktree** is the only destructive
  one, deleting selected *linked* worktrees and skipping (never silently downgrading)
  any main working tree in the batch. **Move Claude Session Here** takes a single
  *destination* rather than a subject, so it is hidden while a multi-selection is
  active via the built-in `listMultiSelection` context key.

The tree view runs **alongside** the reporter lifecycle (register/heartbeat/
unregister): every window is both a reporter and, if it has the view open, a reader.

### Row colours

Any repository or worktree row can be tagged with an icon colour (#1428), so a tree
spanning many repositories can be read at a glance — and so a user whose theme renders
`charts.green` illegibly can pick something else. **Set Colour…** on the row's context
menu opens a colour picker; it acts on the whole [multi-selection](#tree-view-companion-ui)
and repo rows are as taggable as worktree rows. Its **Default (theme colour)** entry
clears the selected rows, and **omni-dev: Clear All Row Colours** in the command palette
clears every row at once.

- **A colour attaches to a row, not to a state or a glyph.** A tagged row keeps its
  colour whichever icon it is currently rendering — `check`, `circle-filled`, or
  `git-branch` — and the glyph itself is never changed, so the open-state distinction is
  not traded away. Colouring by *state* was rejected deliberately: in a tree where nearly
  every row sits in "open in another window", a per-state colour repaints almost
  everything identically and differentiates nothing. It would also be unstable — for the
  ~10s after a daemon restart, before every window has re-registered, every row
  transiently reports closed.
- **Precedence**, first match wins: the [mid-rebase cue](#rebase-onto-main) (yellow,
  transient and actionable) → the row's tag → the row's default state colour. So a tag
  overrides the open-state green and the PR-poll green, but a worktree left mid-rebase
  still shows yellow until it is resolved. An untagged row renders exactly as it did
  before the feature existed.
- **Colour decides only the colour** (#1433). The glyph is chosen by its own rule, in
  which the current window's tick outranks even the rebase cue, so a current-window row
  mid-rebase is a *yellow tick*: identity from the glyph, rebase state from the colour.
- **Badges are untouched.** The PR-check and Claude-session badges keep their own
  severity-ranked colour, so a tagged row's icon and badge can differ in colour — correct,
  since they report different things.
- **Stored in VS Code user settings**, as `omniDevWorktrees.rowColors`, and never sent
  over the socket — this is client-side presentation the snapshot has no reason to carry.
  Unlike the show/hide-closed toggle (#1301) and the per-repo poll flag (#1376), it
  therefore needs no daemon state: user-scope settings changes fire
  `workspace.onDidChangeConfiguration` in **every** open window, which gives the
  cross-window live sync those two needed the daemon for. Consequences: tags survive a
  daemon restart, a window reload and a reboot; they are hand-editable; and they ride
  Settings Sync. The setting is `"scope": "application"`, so it cannot be overridden per
  workspace — the tree spans every repo across every window, so a workspace-scoped map
  would apply only where that folder happened to be open.
- **Keys are `nodeId`**: `repo:<repo root>` for a repository row, `wt:<worktree path>`
  for a worktree row. The discrimination matters — a main worktree's path *is* its
  repo's root, so those two rows share a directory and a path-keyed map could not tell
  them apart. It follows that colouring a repository row does not colour its main
  worktree row; they are tagged separately.
- **Values are workbench colour ids**, not hex: `vscode.ThemeIcon` accepts only a
  `ThemeColor`. The vocabulary is the `charts.*` family, the `terminal.ansi*` and
  `terminal.ansiBright*` families, and `descriptionForeground`, validated by a
  JSON-schema `enum`. An empty string or an unrecognised id leaves the row its default
  colour rather than rendering it invisible.
- **Stale keys are never collected automatically.** Deleting a worktree leaves its entry
  behind, and recreating one at the same path inherits the old colour. Pruning against
  the live tree would be wrong — a row is legitimately absent whenever the daemon is
  down, its repo is open in no window, or the setting has synced to a machine with a
  different layout — so cleanup is the explicit **Clear All Row Colours** command, or a
  hand-edit.

### Pull requests

For a worktree on a **GitHub** repo whose branch has an **open pull request**, the
row shows a muted PR badge after the sync counts — `#<number>` and a `draft` marker
for drafts — and, when the PR has CI checks, a **colored check badge** at the right
of the row: a **green `✓`** (passing), **red `✗`** (failing), or **yellow `●`**
(still running); nothing when a PR has no checks (#1324). The check badge is a
theme-aware file decoration (it adapts to light/dark and also tints the branch
label), not a monochrome glyph in the description — so a passing and a failing PR
are distinguishable at a glance rather than by reading the glyph shape. The badge
appears on **every** worktree in the view — the one open in this window and those
open in others (and closed worktrees when shown) — and the hover tooltip adds a
`PR #<n> · open/draft · checks …` line.

Two right-click actions on any GitHub worktree (or repo) open its PR, sharing one
discovery flow — the daemon finds the PR(s) (a quick-pick when several match) — and
differing only in where they open it. **"Open Pull Request…"** opens it **as a tab
inside the editor**, never a browser: it hands off to the **GitHub Pull Requests**
extension (`GitHub.vscode-pull-request-github`), and if that extension is absent it
offers to install it or copy the PR URL. **"Open Pull Request in Browser…"** is the
explicit way to ask for a browser instead — it opens the PR's `github.com` page in
the OS default browser and needs no extension.

A third action **copies** rather than opens. **"Copy PR URL"** (#1430) puts the
selection's PR links on the clipboard as **one line per selected row**, in selection
order: the PR's URL when the row has one, and a `#`-commented placeholder naming the
row when it does not (`# No PR for <branch> in <absolute path>`, or
`# No open PRs for <owner>/<name>` for a repo row). The placeholders are the
feature as much as the URLs are — a block that silently dropped the PR-less rows
could no longer tell you which of the selected worktrees are unrepresented — and the
`#` keeps the block paste-safe into a shell, a YAML/TOML scratch file or a markdown
list. Three rules follow from being per-**row** rather than per-PR:

- A lookup that **failed** gets its own comment (`# PR lookup failed for …`) rather
  than an emptiness placeholder, so a transient `gh`/daemon failure can never be
  read as a settled negative. The rest of the batch still copies.
- URLs **de-duplicate across the whole block** (a repo row plus one of its own
  worktree rows lists that PR once) while placeholders never do — they name
  different worktrees. A row whose URLs were all already emitted therefore
  contributes nothing rather than a placeholder.
- It is gated like **"Copy Directory"** (`viewItem =~ /^(repo|worktree)/`), *not*
  like the two Open actions (`/github/`): a row with no GitHub identity, and a
  detached worktree, both have a truthful answer here, and the `/github/` gate would
  hide the command exactly where the placeholder earns its keep.

A fourth action opens the **repository** rather than a pull request. **"Open GitHub
Repository"** (#1442) opens `https://github.com/<owner>/<name>` in the OS default
browser, built straight from the `github` identity the `tree` payload already
carries. It is the one GitHub action in the view that costs **no `gh`, no network
and no daemon op at all** — the URL is a pure function of the snapshot — so it needs
no progress notification, no confirmation, and no particular daemon version. Two
details distinguish it from the PR actions:

- It is gated on **repo rows only** (`viewItem =~ /^repo\.github/` — the
  `repoContextValue` prefix), not the Open actions' `/github/` or Copy PR URL's
  `^(repo|worktree)`: a worktree's repository page is its parent's, so offering it on
  both doubles a menu entry for no new information. It sits in the `0_open` group
  beside "Open Worktree", not in `1_pr` — it is an *open* action, not a PR one.
- A multi-selection still **de-duplicates** by URL, and a worktree row caught up in
  one resolves to its parent repository. VS Code evaluates a `when` clause against
  the **clicked** row only, so the handler filters the selection itself rather than
  trusting the gate — the same reason every other action here does.

Discovery is **daemon-served** (#1389, fix 7): a worktree already resolved
daemon-side opens straight from the snapshot badge (**zero `gh`**), and a repo-wide
lookup goes through the daemon's shared, TTL-cached [`open-prs`](#companion-contract-for-the-extension-and-other-clients)
op — so N windows dedupe to **one** counted `gh pr list` per repo instead of each
window shelling its own (the per-window burn #1370/#1389 target). Only a daemon too
old to serve the op makes the extension fall back to its own `gh`.

- **Resolved by the daemon, and kept live (#1337).** PR state rides the `tree`
  payload as a per-worktree `pr` object, resolved by a background poller in the
  daemon and pushed over the existing subscribe stream. This is a **reversal of
  [ADR-0050](adrs/adr-0050.md)**, which resolved badges extension-side on
  repo-expand: badges were then only recomputed when a repo node's children were
  rebuilt, and since the snapshot carries worktree topology rather than CI, nothing
  ever re-asked GitHub — a badge went stale the moment CI moved and stayed stale,
  including a confident `✓` on a PR whose CI was still running. Every window also
  resolved its own, so cost scaled with window count. See
  [ADR-0053](adrs/adr-0053.md).
- **Still no credential in the daemon.** The daemon shells out to `gh api graphql`
  (per [ADR-0003](adrs/adr-0003.md)), so the token stays inside `gh` exactly as
  ADR-0050 wanted — the daemon never sees one, and ADR-0040's *"persists nothing /
  no secret"* posture is untouched. It needs `gh` installed and `gh auth login`.
- **One query for everything.** A single call resolves every (repo, branch) pair in
  the tree at **1 point** against GitHub's 5,000/hour GraphQL budget, regardless of
  how many repos, worktrees, or windows are open — `repository()`/`ref()` are not
  GraphQL connections, so aliasing them is free. The poller **wakes** every ~10 s —
  a read of the coalescing snapshot cache, no subprocess and no network — but only
  **asks GitHub** when there is reason to:
  - The watch set **grows** — a branch is added, or its **upstream moves** (a push,
    which is what starts the CI run a badge reports) — or the backoff elapses.
  - A pure **removal never asks** (#1389): a window closing, a VS Code or daemon
    shutdown, a TTL reap, or a lapsing 15-minute lease cannot change any *surviving*
    badge, so it spends nothing — it just prunes the dead entries locally.
  - A **local commit does not ask** either (#1389): GitHub has not seen it, so the
    answer would be the cached one; the badge stays correctly stale locally (below)
    until you push.

  Cadence while a badge is **pending** starts at ~10 s (matching `gh pr checks
  --watch`) for the first ~2 minutes after a push, then **escalates** toward a 60 s
  ceiling so a long CI run (or a zombie suite) stops pinning the fast rate; once
  everything is **terminal** it doubles to a 30-minute ceiling, and it polls nothing
  at all while no window is registered. A change-notify **storm** — the one a VS Code
  restart makes as it re-registers its windows one-by-one — is **debounced** into a
  single fetch on the final set (#1389). And because the daemon is the single `gh`
  choke point for every window, the cadence is **capped** when the shared GitHub
  budget crosses its warn threshold ([below](#github-rate-limit-monitor)), so no
  runaway in this class can exhaust it. Override the fetch cadence with
  `OMNI_DEV_DAEMON_PR_POLL` and the storm debounce with `OMNI_DEV_DAEMON_PR_DEBOUNCE`
  (whole seconds). The daemon pushes to windows only when a verdict actually moves.
- **Warm badges across a restart (#1389).** After each poll the resolved badges
  (number / URL / check-state / draft marker — no secret; the same data the tree
  wire already carries) are persisted to `worktrees-pr-cache.json` (`0600`, beside
  `worktrees-polling.json`). On restart the daemon serves them on the **first**
  snapshot and **skips its immediate re-poll** when every current target already has
  a verdict younger than the backoff — so a daemon restart costs ~0 calls rather than
  a fresh fetch. Best-effort: a missing or unreadable file simply re-polls, and the
  next poll rewrites a clean one.
- **Per-repository, off by default, and time-boxed (#1376).** Polling is opt-in
  **per repo**: the daemon polls a repo's badges only once you enable it
  (right-click the repo → **Enable PR Polling**), so a repo it is not polling
  issues **zero** `gh`. With many repos open but only a couple in active work, this
  is the difference between polling all of them and polling the two that matter. An
  enable is a **15-minute lease** — the repo **auto-disables** when it elapses (and
  re-enabling refreshes it), so an idle repo stops costing budget even if you never
  turn it off. The repo icon is **green** while polling and **gray** when off; the
  choice covers every worktree/branch of the repo and is **persisted** (with its
  expiry) in `worktrees-polling.json` (`0600`), so a restart within the window
  keeps the remaining lease. Disabling — or letting a lease expire — drops the
  repo's badges on the next snapshot. See the
  [`set-polling` / `polling_enabled`](#companion-contract-for-the-extension-and-other-clients)
  contract entry.
- **A commit invalidates the verdict immediately, locally.** Each badge records the
  commit its verdict describes; when that differs from the worktree's `head_sha` the
  row renders ● rather than the previous commit's ✓ — no network call, on the very
  next snapshot. So committing never leaves a stale green standing. The poller does
  **not** spend a call to refresh a *local* commit, though (#1389): GitHub has not
  seen it, so the verdict would be unchanged, and the badge stays correctly ● until
  you **push** — which moves the upstream and *does* trigger one fetch, resolving the
  CI run the push started. The same local-staleness applies to unpushed work and to
  being behind the remote: the verdict simply is not about the commit in front of you.
- **Best-effort, silent.** A worktree with no branch, no matching PR, or on a
  non-GitHub repo shows nothing extra. If `gh` is absent or not authenticated,
  badges are simply omitted — no error dialog (the explicit "Open Pull Request…"
  action still surfaces the real `gh` error). A failing poll leaves the last known
  state in place rather than blanking the rows — badges *and* negatives — and
  mints no new negatives.
- **"No open PR" is an explicit answer (#1370).** A branch the poller successfully
  checked and found PR-less rides the snapshot as `pr_none: true` (mutually
  exclusive with `pr`; a never-pushed branch counts as PR-less). Without it,
  absence carried two meanings — "checked, none" and "old daemon that never
  checked" — so every window's degraded `gh pr list` fallback re-fired per window
  × repo × 60 s forever for every PR-less branch, silently reintroducing the very
  windows × repos GitHub cost #1337 removed. Both fields absent still means "not
  resolved".
- **Older daemons degrade.** Against a daemon predating #1337 — which omits `pr` —
  the extension falls back to its own PR list and shows the PR number and draft
  marker **without** a check glyph: nothing extension-side polls, so a verdict there
  could not refresh, and a stale `✓` is worse than none. The fallback covers only
  branches with **neither** `pr` nor `pr_none` (#1370), so against a current daemon
  it runs no `gh` at all.
- **A not-polled repo issues zero `gh`, from the daemon *and* the extension
  (#1389).** The badge fallback is now gated on `polling_enabled`: a repo you have
  not enabled polling for (#1376) resolves no badges daemon-side **and** the
  extension no longer shells `gh pr list` per window for it — the opt-out is honoured
  end-to-end, closing the gap where disabling polling merely *moved* a repo's cost
  from one shared daemon call to one `gh` per window. The only fallback that survives
  — a *polled* repo's brief pre-first-poll transient — routes through the shared
  `open-prs` op, so even that is one `gh`, not one per window.
- **The check state is colored, not monochrome (#1324).** The `✓`/`✗`/`●` is a VS
  Code `FileDecoration` (the same mechanism git status uses to color `M`/`U`
  badges), painted from a custom `omnidev-worktree:` `resourceUri` the extension
  puts on each row — a custom scheme, so it never collides with git's own folder
  decorations. The color uses the theme `charts.{green,red,yellow}` palette, and the
  check state is encoded in the URI so a state change re-colors on its own. Purely
  extension-side: no daemon, wire, or trust-boundary change.
- **Opt-out.** The `omniDevWorktrees.showPullRequests` setting (default on) hides
  the badge. Since the daemon now supplies it on the snapshot, the setting strips
  it on the way in rather than skipping a lookup. It is the **master switch** above
  the per-repo toggle (#1376): when off, badges are stripped and repo icons render
  gray regardless of per-repo state, and the "Enable/Disable PR Polling" menu items
  are hidden.

### Merge queue

**Add to Merge Queue** (the `1_pr` context-menu group, on any GitHub-backed
worktree row) enqueues the selected worktrees' pull requests into the repo's
**GitHub merge queue** — but only the ones that are genuinely ready. Select any
number of rows, act once: eligible PRs are queued, and everything else comes back
**skipped with a reason** rather than being silently dropped or failing the batch.

**Precondition:** the merge queue must already be **enabled on the repo** (a GitHub
admin setting, on the target branch's ruleset). The daemon does not enable it; if
it is off, GitHub's rejection is surfaced per-PR in `failed[]`.

#### The gates

A worktree is enqueued only if **every** gate holds. Each failure carries a stable
machine `kind` plus a human-readable `detail`:

| # | Gate                                       | Skip `kind`                 |
| - | ------------------------------------------ | --------------------------- |
| 1 | Clean working tree                         | `dirty` / `untracked`       |
| 2 | Has a commit (non-unborn `HEAD`)           | `no-commits`                |
| 3 | Fully pushed (`head == upstream`, 0 ahead) | `unpushed` / `no-upstream`  |
| 4 | An open PR heads the branch                | `no-pr`                     |
| 5 | The PR is not a draft                      | `draft`                     |
| 6 | The PR is not conflicting                  | `conflicting`               |
| 7 | The PR's checks are green                  | `checks-failing`            |
| — | The PR head matches the local head         | `stale`                     |

Two more are refused locally: a detached `HEAD` (`detached`) and a repo with no
`github.com` remote (`no-github`).

Gates 1–3 are pure local git and run **first**, so an ineligible worktree is
reported with **zero** GitHub API calls. The survivors are then resolved in a
**single** `gh api graphql` query (the same aliasing that keeps the badge poll at 1
point), and gates 4–7 applied. Gates 5–7 are deliberately stricter than the merge
queue itself requires — the point of a batch action is that you do not inspect each
PR, so an enqueue that would predictably bounce is better reported as a skip.
`BLOCKED` and `UNKNOWN` merge states are **not** treated as conflicts (the queue
resolves those); only `CONFLICTING` and `DIRTY` are.

#### Two phases, re-validated

Like [`close`](#companion-contract-for-the-extension-and-other-clients), the op is
two-phase and the daemon **never trusts the client's phase-1 result**:

1. **Check** — `{ paths[], check: true }` (also any request without `confirmed`).
   Side-effect-free. Replies `{ eligible: [{ path, number, url, branch }], skipped:
   [{ path, kind, detail }] }`. The extension shows one modal for the whole
   selection ("Enqueue N of M worktrees? (K skipped)").
2. **Execute** — `{ paths[], confirmed: true }`. Re-runs the **entire** evaluation,
   then enqueues what still passes. Replies `{ queued: [{ path, number,
   already_queued? }], skipped: [...], failed: [{ path, number, error }] }`.

So a worktree that became dirty, was force-pushed, or had its PR closed between
render and click is skipped at execute time and reported — the UI gating is
convenience, never the guard. Both phases also accept an optional `requester_key`
(this window's key); unlike `close` it does not affect routing, and is only logged.

A PR **already in the queue** is idempotent success (`already_queued: true`), not a
failure. A per-PR rejection — queue disabled, PR unmergeable, insufficient
permissions — lands in `failed[]` and never sinks the rest of the batch. Enqueue
authenticates through the user's own `gh`, so **no credential enters the daemon**.

#### From the CLI

```bash
# Report eligibility and stop — no side effects.
omni-dev worktrees merge-queue ~/wrk/wt/issue-1401 --check

# Enqueue (prints the report, then asks to confirm; -y skips the prompt).
omni-dev worktrees merge-queue ~/wrk/wt/issue-1401 ~/wrk/wt/issue-1400 -y
```

Both phases are auditable in `omni-dev daemon logs` as `merge-queue check` /
`merge-queue enqueue` INFO lines. See [ADR-0056](adrs/adr-0056.md) for the
rationale — including why this uses a dedicated GraphQL query rather than widening
the badge poll, and why it is the service's first op to mutate remote state.

### Rebase onto main

**Rebase on main** (the `4_git` context-menu group, on every worktree row) rebases
the selected worktrees' branches onto their repository's remote default branch,
fetching each repository's ref **once** for the whole batch (#1409, reworked in
#1415). It drives the daemon's two-phase **`rebase`** op, which runs the same
engine as [`worktrees rebase`](#cli).

**It is a socket client like every other action** ([ADR-0059](adrs/adr-0059.md)).
#1409 shipped it as a shell-out to `omni-dev worktrees rebase` in an integrated
terminal, on [ADR-0055](adrs/adr-0055.md)'s premise that the daemon lacked
`SSH_AUTH_SOCK` and so could not authenticate a fetch. That premise was wrong —
launchd exports `SSH_AUTH_SOCK` into the per-user session, so the daemon inherits
the user's `ssh-agent`. What it lacked was a useful `PATH`, now closed by the same
well-known-paths probe used for `code` and `gh` (override with `OMNI_DEV_GIT_BIN`).

**Two phases, no confirmation gate** ([ADR-0062](adrs/adr-0062.md)). An earlier
version paused between the phases with a modal listing every branch about to be
rewritten; #1458 removed it, since every outcome a rebase can produce — a clean
fast-forward or one left mid-rebase with a conflict — is equally reflog-recoverable,
so a gate had nothing left to distinguish.

**Where the behind-count went.** #1409's since-removed modal deliberately showed no
numbers, because the only one it had was the tree's `behind` — divergence from the
branch's *upstream*, not from the rebase target — which would have been quietly
wrong. #1457 closes that gap at rest, with no modal needed to show it: the tree
row's `⇊N` glyph (see [Tree view](#tree-view-companion-ui)) passively surfaces the
real rebase-target divergence, with no rebase — or even a fetch — required.

1. **Check.** A progress notification while the daemon fetches each selected
   repository's target ref once and classifies every worktree. Nothing is
   rewritten. If nothing is pending, a warning toast says *why* — the skip reasons,
   the up-to-date count, or the fetch error — rather than the action appearing to
   do nothing.
2. **Execute.** The daemon **re-plans from scratch** (a worktree that went dirty
   between the phases is skipped, not rebased) and rebases each still-pending
   worktree sequentially, then a summary toast reports the outcome, naming every
   worktree left mid-rebase with a conflict.

**Conflicts are left in place.** This surface always sends `keep_conflicts`, so a
worktree that hits conflicts stays **mid-rebase** with its markers on disk instead
of being `git rebase --abort`-ed. The batch continues with the rest; the summary
toast is raised to a **warning** naming the worktrees to resolve; and the tree row
keeps cueing it (below) until you finish with `git rebase --continue`.

**Rebase state in the tree.** A row shows a spinning `$(sync~spin)` while the
daemon is rebasing it, and a `$(warning)` with `rebase in progress` for as long as
a rebase is left unfinished — the two coming from separate facts, so both a clean
in-flight rebase (which never touches disk state) and a left-in-place conflict
(which survives a daemon restart) are visible. While either is showing, the row's
icon turns **yellow** and — on every row but one — replaces its usual open-state
glyph; the tooltip still names the open state. The exception is the worktree open
in *this* window (#1433), which keeps its tick and takes only the colour, so a
**yellow tick** marks the row you are standing in while it is mid-rebase. That
matters most for the durable half: `--keep-conflicts` leaves this very row
unfinished for as long as the conflict takes to resolve, and its rebase state is
also the one you can see first-hand in VS Code's own SCM view. The cue is
deliberately *not* a badge: a badge holds two characters, and the PR check verdict
and Claude session cue already take one each.

**The daemon is required.** With none running, the action reports it and points at
`omni-dev daemon start` — or at running `omni-dev worktrees rebase <path>`
yourself, which still works entirely locally. Every run is auditable in
`omni-dev daemon logs` (`rebase check` / `rebase execute` lines carrying the
requesting window, the counts, and how many worktrees were left mid-rebase).

**Every worktree row, main included** ([ADR-0060](adrs/adr-0060.md)). The menu
`when` clause is the same unconditional `viewItem =~ /worktree/` gate **Open
Worktree** uses — no role restriction. A selected main working tree is sent to
the daemon like any other target and classified there (up-to-date, would-rebase,
dirty, etc.); there is no client-side pre-filter dropping it from the batch.

`--autostash` and `--onto <ref>` are **not** surfaced: a dirty worktree is instead
reported as a skip reason (step 1's toast, when it is the reason nothing in the
batch was pending), and a target other than the remote default branch is a CLI
concern (#1409).

### Push (force-with-lease)

**Push (force-with-lease)** (the `4_git` context-menu group, beside **Rebase on
main**) publishes the selected branches to their upstreams, force-pushing with a
lease where a rebase rewrote history (#1443,
[ADR-0061](adrs/adr-0061.md)). It drives the daemon's two-phase **`push`** op,
which runs the same engine as [`worktrees push`](#cli).

This is the action that completes the batch-rebase workflow. Before it, the view
could rewrite N branches with one gesture and then displayed the divergence it had
just created with no way to resolve it — you opened a terminal in each worktree
and force-pushed by hand.

**Offered on repo rows too**, unlike every other batch git action. A selected repo
means *every worktree of it*, expanded client-side from the snapshot the repo node
already carries (no extra round-trip) and deduplicated against any worktree rows
you selected alongside it. The `when` clause is therefore
`viewItem =~ /^(repo|worktree)/`.

**Two phases, one confirm.**

1. **Check.** A progress notification while the daemon classifies every selected
   worktree against its upstream. Nothing is published, and — unlike the rebase
   check — **no remote is contacted at all**: the comparison is against your local
   `refs/remotes/<remote>/<branch>`, which is exactly what the lease is checked
   against, so the plan you confirm and the lease git enforces agree by
   construction. If nothing is pending, a warning toast says *why*.
2. **Confirm.** A modal listing every branch about to be published, **force
   pushes first and labelled `FORCE`** with their ahead/behind counts, then the
   fast-forwards, then the skipped ones with reasons. Shown even for a single row
   ([ADR-0049](adrs/adr-0049.md) §1). The button reads **Force Push** whenever the
   batch contains one, so a rewrite is never confirmed by a button that says
   merely "Push".
3. **Execute.** The daemon **re-plans from scratch** (a branch that moved between
   the phases is re-classified, not pushed on stale information) and publishes
   each still-pending worktree sequentially, then a summary toast reports the
   outcome.

**The lease is not negotiable.** Every force goes out as `--force-with-lease
--force-if-includes` — see [the CLI section](#cli) for why both flags are needed —
and there is no `--force` anywhere: not a flag, not a modal escape hatch, not a
field a socket client could set. A refused lease is the feature working: the toast
is raised to an **error**, names the branches, and tells you to run `git fetch`,
rebase, and push again.

**The remote default branch is never force-pushed**, whichever worktree has it
checked out. The refusal lives in the daemon's engine, not in the menu gating, so
it holds however the op is reached; a fast-forward onto the default branch is an
ordinary push and stays allowed.

**Push state in the tree.** A row shows a spinning `$(sync~spin)` with `pushing…`
while the daemon is publishing it, and its icon turns yellow, exactly as for a
rebase. There is deliberately **no durable counterpart**: a push writes no on-disk
state a later snapshot could rediscover, so this cue is the whole of it — and a
*completed* push instead shows up as the row's `↑ahead` count clearing on the next
frame, since the push moves `upstream_sha` and that is what makes the frame a
delta (#1344). Like the rebase cue it is deliberately *not* a badge: a badge holds
two characters, and the PR check verdict and Claude session cue already take one
each.

**The daemon is required.** With none running, the action reports it and points at
`omni-dev daemon start` — or at running `omni-dev worktrees push <path>` yourself,
which still works entirely locally. Every run is auditable in `omni-dev daemon
logs` (`push check` / `push execute` lines carrying the requesting window, the
counts, how many needed the lease, and how many the lease refused).

### Window repositioning (macOS)

**Reposition Windows to Match** (the `4_layout` context-menu group) moves and
resizes every selected worktree's **already-open** VS Code window so it occupies
the same position and size as the window you invoked the command from.

- The **invoking window is the reference**: it supplies the geometry and is never
  itself moved, even if it is part of the selection.
- **Z-order does not change.** Nothing is raised, lowered, or activated — the
  windows land on top of each other in whatever front-to-back order they already
  had. This is a property of the mechanism, not a promise the daemon maintains:
  setting a window's Accessibility position/size attributes does not raise it.
- Worktrees with **no open window** are skipped and reported.
- Fullscreen and minimized windows are **skipped**, not mangled: a resize has no
  effect in native fullscreen, and geometry writes on a minimized window are
  undefined.

A VS Code extension cannot do any of this — there is no API for a window's OS
position or size, and the upstream request is explicitly parked
([microsoft/vscode#28150](https://github.com/microsoft/vscode/issues/28150),
[#83170](https://github.com/microsoft/vscode/issues/83170),
[#195406](https://github.com/microsoft/vscode/issues/195406)) — so the daemon does
it through the macOS Accessibility API. See [ADR-0058](adrs/adr-0058.md).

**macOS only.** Elsewhere the op reports the same "no permission" state as an
un-granted macOS daemon, and the companion hides the menu item.

#### Setup: the Accessibility permission

This is the one omni-dev feature that needs an OS permission grant. Without it
nothing is moved and the reply says so.

1. Open **System Settings → Privacy & Security → Accessibility**.
2. Click **+**. In the file picker press <kbd>⌘</kbd><kbd>⇧</kbd><kbd>G</kbd> and
   type the path of the installed binary — `~/.cargo/bin/omni-dev` for a
   `cargo install`, or the output of `which omni-dev`. (The picker hides dotted
   directories, hence the Go-to-Folder step.)
3. Ensure the new entry's toggle is **on**.
4. Restart the daemon: `omni-dev daemon restart`.

Step 4 is required, not belt-and-braces: macOS applies a grant when a process
starts, so the already-running daemon will not pick it up.

> **The grant does not survive an upgrade.** omni-dev installs unsigned, so its
> code signature changes on every rebuild and macOS silently stops honouring the
> grant — the entry stays listed but no longer works. After each
> `cargo install omni-dev` (or `brew upgrade`), toggle the entry off and on again
> (or remove and re-add it) and restart the daemon. A stable code-signing identity
> would fix this and is a follow-up.

The extension surfaces a missing grant with an **Open Accessibility Settings**
button rather than an opaque failure. The daemon never raises the system permission
prompt itself: a dialog from a background LaunchAgent is unreliable.

#### How a worktree is matched to its window

The registry stores no OS window id and no geometry, and its `pid` is the
**extension-host** process — in Electron every window's native `NSWindow` belongs
to the single **main** process, so the pid cannot locate a window. Resolution is
therefore two steps:

1. **Which application** — the extension host is a child of the VS Code main
   process, so one batched `ps -o pid=,ppid=` names the owning app. This also keeps
   Stable, Insiders, and VSCodium windows correctly separated.
2. **Which window within it** — the registry's `title` (that is
   `vscode.workspace.name`, normally the worktree's directory name) is matched
   against each window's `AXTitle` through progressively looser rules — exact
   equality, then suffix, then substring — where **each rule must isolate exactly
   one window** to be accepted.

If no rule isolates a single window the target is reported `ambiguous` and **left
alone**. Moving the wrong window is a worse failure than moving none, so the daemon
refuses to guess. Matching is reliable for VS Code's default window title; it
degrades — always into a *reported skip*, never a mis-target — if you set a custom
`window.title`, use a non-default profile, or have two worktrees with the same
directory name open at once.

Use `--dry-run` to see exactly what resolves to what before moving anything.

#### Per-target outcomes

| `outcome`     | Meaning                                                          |
| ------------- | ---------------------------------------------------------------- |
| `moved`       | The window now has the reference frame.                          |
| `would-move`  | Dry run: resolved to one window, and would have been moved.       |
| `unchanged`   | Already in that position; nothing was written.                    |
| `partial`     | Moved, but settled elsewhere (a minimum size, or a display that refused the position). `detail` names where it landed. |
| `reference`   | This is the invoking window, which never moves.                   |
| `no-window`   | No window is open on this worktree any more (a stale row).         |
| `no-title`    | The window reported no name to match on (a brand-new empty window). |
| `no-pid`      | The window reported no process id.                                |
| `not-found`   | No window of the owning application matched the name.             |
| `ambiguous`   | Several windows matched, or an earlier selection entry claimed the same one. |
| `minimized`   | The window is in the Dock.                                        |
| `fullscreen`  | The window is in native fullscreen, where a resize is a no-op.     |
| `failed`      | The window rejected the write; `detail` carries the `AXError`.      |

A whole batch can also be refused before any target is attempted, with a
`blocked.reason` of `reference-unidentifiable`, `reference-not-found`,
`reference-ambiguous`, `reference-fullscreen`, or `reference-minimized` — all
meaning there was no usable geometry to copy.

#### Undoing

Repositioning has no confirmation prompt: it creates, modifies and deletes nothing,
and it is a layout command you repeat. What it does lose is the previous layout, so
the daemon remembers where it moved each window from and offers **Undo** on the
summary notification, plus an `omni-dev: Undo Reposition Windows` command in the
palette.

The record is **one level deep and in memory**: the next reposition replaces it, an
undo consumes it (so it cannot be replayed onto a layout you have since redone by
hand), and a daemon restart drops it. Undo re-resolves every window from scratch, so
one that has closed or gone fullscreen in the meantime is reported rather than
forced.

#### From the CLI

```bash
# Show which OS window each worktree resolves to — moves nothing.
omni-dev worktrees reposition --reference ~/wrk/wt/issue-1407 ~/wrk/wt/issue-1406 --dry-run

# Move issue-1406's and issue-1401's windows onto issue-1407's geometry.
omni-dev worktrees reposition --reference ~/wrk/wt/issue-1407 \
  ~/wrk/wt/issue-1406 ~/wrk/wt/issue-1401

# Put them back.
omni-dev worktrees reposition --undo
```

Paths are the CLI's currency (as for `focus`/`close`); it maps each to the key of
the window that has it open, and errors if a named worktree has no open window.
Running it from a terminal needs no permission of its own — the CLI is a socket
client, so the Accessibility calls happen in the **daemon's** process under the
daemon's grant.

Every invocation is auditable in `omni-dev daemon logs` as a `reposition` INFO line
carrying `phase` (`check`/`apply`/`blocked`/`untrusted`), the counts, and the
per-target outcome slugs — so "why did that window not move?" is answerable from the
log alone.

**Known limitation — Spaces.** A window on another macOS Space *is* moved (the
coordinates apply) but stays on that Space, so you will not see the change until you
switch to it.

### Reloading windows

**Reload Window** reloads the VS Code window of every selected worktree — the batch
form of `Developer: Reload Window`, which otherwise has to be run by hand in each
window in turn. It is the standard remedy after an extension update, a settings
change that needs a restart, a wedged language server, or a git operation that left
the SCM view stale.

Multi-select aware like the other batch actions: select any number of worktree rows,
right-click, **Reload Window**. Selected worktrees with **no open window** are
silently skipped — there is nothing to reload, so they are simply not targets — and
the count still reaches the summary notification, so a batch is never *quietly*
narrowed. Selecting only closed worktrees reports "nothing to reload" and sends no
op at all.

The invoking window reloads **last and alone**. `workbench.action.reloadWindow` kills
the extension host, taking any in-flight request and the summary notification with
it, so the others are signalled and reported first. That is the one real hazard in
the feature, and it is why the summary appears a moment before your own window
reloads.

#### No confirmation, by design

Reloading has no confirmation prompt. It creates, modifies and destroys nothing —
VS Code's hot exit preserves dirty editors — so it follows the
[reposition](#undoing) precedent of firing and reporting, **not** the two-phase
confirm `close` needs ([ADR-0049](adrs/adr-0049.md)). The asymmetry is deliberate:
`close` deletes a worktree, and a modal on a routine, repeatable command costs more
than it protects. There is correspondingly nothing to undo — a reload has no previous
state to restore, which is what `reposition`'s Undo exists for.

#### Known limitation — the ~10s stagger

A cross-window reload rides the target's next **heartbeat**, so it lands **up to ~10s
later**, and reloads across a batch will visibly stagger as each window's tick comes
round. This is inherent to the companion contract — the daemon has no push channel to
a window outside the `subscribe` stream — and is the same latency `close` already
accepts. It is also why the notification says how many windows were *signalled*
rather than reloaded: when it appears, none of them have reloaded yet, and the daemon
could not observe it if they had (a reloaded window re-registers under the same key).

#### From the CLI

```bash
# Reload two worktrees' windows.
omni-dev worktrees reload ~/wrk/wt/issue-1417 ~/wrk/wt/issue-1415

# Machine-readable: { requested, signalled, unknown[] }.
omni-dev worktrees reload ~/wrk/wt/issue-1417 -o json
```

Paths are the CLI's currency (as for `focus`/`close`/`reposition`); it maps each to
the key of the window that has it open. Unlike the tree view, a named worktree with
**no** open window is an **error**, not a silent skip: there the selection is a sweep,
here you named each target explicitly.

## Workspace Trust (Restricted Mode)

When the daemon opens a worktree folder VS Code has never seen before — the tray
focus action, the tree view's double-click, or the `open` op, all of which run
`code <folder>` — the new window starts in **Restricted Mode** behind VS Code's
[Workspace Trust](https://code.visualstudio.com/docs/editing/workspaces/workspace-trust)
gate. The worktrees workflow (many short-lived per-branch worktrees) hits this
prompt constantly. Trust is decided by the VS Code workbench purely from the
folder path against a per-user trusted-folders list, **independent of how the
folder is opened**, so the launcher cannot influence it (#1297).

**Recommended: trust the worktree parent folder once.** Trigger the trust prompt
for your worktree root (e.g. `~/wrk/work-trees`) and choose to trust the **parent
folder**, or add it under *Manage Workspace Trust → Trusted Folders*. VS Code
trusts a folder together with all of its subfolders, so **every future worktree
window — including every daemon-spawned one — opens trusted** with no further
prompt. This is a one-time UI action and the only robust, supported route today.

Two approaches that look like a launcher-side fix are deliberately **not** taken,
with the reasons recorded so they are not re-proposed
([ADR-0051](adrs/adr-0051.md)):

- **`code --disable-workspace-trust <folder>`** — a real (but `--help`-hidden)
  flag. It is **session-only** ("only affects the current session") and, once a
  VS Code instance is already running, `code <folder>` **forwards the open to
  that resident instance**, which never received the switch — so the flag is a
  no-op in the daemon's normal "windows already open" state. Verified on VS Code
  1.128.0: a folder opened this way while other windows were open still landed in
  Restricted Mode. Shipping it behind an opt-in env would offer a false sense of a
  fix, so the launcher does not pass it.
- **Pre-seeding the global trust store** (`state.vscdb` → `ItemTable` key
  `content.trust.model.key`) genuinely trusts a path, but the write only sticks
  while VS Code is **fully quit** (the in-memory cache clobbers external writes on
  flush) and the internal schema is version-fragile, so the daemon does not touch
  it.

No supported declarative "default trusted folders" mechanism exists yet; upstream
[microsoft/vscode#291933](https://github.com/microsoft/vscode/issues/291933)
tracks one.

## Status

`omni-dev daemon status` includes the service:

```text
daemon: running
  worktrees        ok         3 window(s) across 2 repo(s)
```

The `-o json` status detail carries the same enriched window entries as
`worktrees list -o json`.

## GitHub rate-limit monitor

The PR-badge poller shells out to `gh`, spending the same GitHub API budget as
every other tool sharing the user's `gh` token. When that budget drains — a poll
storm, or heavy `gh` use elsewhere — GitHub rate-limits `gh` **machine-wide** with
no warning until commands start failing. A background monitor (#1375) polls
`gh api rate_limit` on a fixed cadence and surfaces the used-percentage as an
early-warning *trend*.

Polling `/rate_limit` is **exempt**: GitHub documents it as not counting against
any budget (confirmed empirically — consecutive reads leave `core.used` flat), so
the monitor costs nothing against the very budget it watches, and needs no adaptive
backoff (unlike the PR poller). It lives alongside the PR poller in the worktrees
service because that is the daemon's `gh` consumer, but the reading is machine-wide.

Two refinements ride #1389 (fix 8):

- **Every PR poll is a free budget reading (8a).** The badge query carries a
  `rateLimit { limit cost remaining used resetAt }` block — a meta-field GitHub
  answers *without* adding to the query's own `cost` — so each poll folds a fresh
  **graphql** reading into the shared cache (`observe_graphql`, preserving the
  standalone poller's `core`/`search`). The block's `cost` also reports the poll's
  **actual** point price, which is how the "1 point per poll" claim is verified at
  scale rather than assumed (measured `cost: 1` at 37 branches; it rises to 2 near
  100 branches, 4 near 200 — see [pr_status.rs](../src/pr_status.rs)). The poll logs
  it at debug: `PR poll cost 1 point(s); graphql 123/5000 used, 4877 remaining`.
- **The standalone poller idles when nothing is watched (8b).** Because the graphql
  figure now stays fresh from the PR polls, the `/rate_limit` loop only spends a
  subprocess while a window is registered **or** a polling lease is active; a
  fully-idle daemon leaves the last reading in place. The read is free against the
  budget, but the wakeups are not.

It surfaces three ways:

- **`omni-dev daemon status`** gains a top-level line above the per-service rows,
  with a `⚠` prefix when any resource is at or above ~80% used:

  ```text
  daemon: running (v0.36.0)
    github rate limit: graphql 82% (4100/5000, resets 06:50Z) · core 3% (27/1000)
    worktrees        ok         3 window(s) across 2 repo(s)
  ```

- **`-o json`** carries an additive optional `github_rate_limit` object on the
  status payload — `{ graphql, core, search }`, each `{ used, limit, remaining,
  percent, reset }` — omitted entirely until the monitor has polled successfully,
  so a pre-#1375 client stays byte-compatible.

- **The tray** prepends a compact `github: graphql 82% · core 3%` line (trailing
  `⚠` over threshold) to the Worktrees submenu.

The poller also logs a `WARN` (visible via `omni-dev daemon logs`) the moment a
resource *crosses* ~80% — once per crossing, not every poll. Cadence is
`OMNI_DEV_DAEMON_RATE_LIMIT_POLL` whole seconds (default 60).

Beyond surfacing the reading, the PR-badge poller now **consults** it (#1389): while
any resource sits at or above the ~80% warn threshold, the poller holds its cadence at
no less than 5 minutes and ignores even an immediate grown-watch fetch. Because the
daemon is the single `gh` choke point for every window, this is the one place a
machine-wide cap can be enforced — so no runaway in the PR-poll class can drain the
shared budget, structurally rather than by convention.

## Git enrichment

The companion reports only raw folder paths; the **daemon** computes the richer
per-worktree git state — current branch, ahead/behind counts, the **main
repository** name (from git's common dir, so a linked worktree resolves the
parent repo it belongs to), and an **`is_worktree`** flag — with the `git2`
dependency it already carries (#1186). Keeping this in Rust preserves the
companion's thin-reporter contract ([ADR-0040](adrs/adr-0040.md)): the ~50-line
extension never runs git.

- **Computed on read.** Enrichment happens each time the registry is read
  (`list`, `status`, the tray menu, and every `tree` / `subscribe` snapshot), from
  the worktree on disk, so the branch shown is always current — not a snapshot
  frozen at registration. Every path but the sync tray `menu` runs it on a blocking
  thread (`git2` is synchronous disk I/O) and never under the registry lock.
- **Ahead/behind is lazy for the tree (#1306).** The `graph_ahead_behind` upstream
  revwalk is the dominant per-worktree cost, and the `tree` / `subscribe` snapshot
  is rebuilt for **every** worktree of **every** repo on **every** tick — so that
  snapshot computes only the *cheap* state (branch, `main_repo`, `is_worktree`, and
  the `head_sha` / `upstream_sha` OIDs) and
  **omits** `ahead`/`behind`. Divergence is instead fetched on demand through the
  **`ahead-behind`** op, batched by path, for just the worktrees a client is about
  to show (the extension does this when a repo is expanded; `worktrees tree` does it
  once for the whole tree). The bounded, non-streamed surfaces — `list`/`status`
  (the primary folder of each open window) and the tray `menu` (open windows only)
  — still compute `ahead`/`behind` inline, since the walk cost there is negligible.
- **`main_behind` rides the same lazy op (#1457).** `folder_main_behind`
  resolves the repository's remote default branch the same **local-only,
  no-fetch** way `worktrees rebase`'s `--onto` default does
  (`RemoteInfo::detect_main_branch_local`), then reuses the same
  `graph_ahead_behind` walk against `origin/<default>` instead of the branch's
  own upstream. It folds into the `ahead-behind` op's per-path result as an
  independently-optional third field, and is omitted whenever the branch's own
  upstream *is* already that default branch (the common checked-out-`main`
  case) — reporting it there too would just duplicate `behind`. Unlike
  `ahead`/`behind`, it can be present for a branch with **no upstream at all**,
  since that is exactly the case it exists to surface.
- **`list` enriches the primary folder; `tree` enriches every worktree.** For a
  window entry (`list`/`status`), only the first workspace folder is enriched — it
  is the one the table shows and the "Focus" action opens. For the `tree` view the
  same building blocks (minus the lazy `ahead`/`behind`) are applied to **every**
  worktree the daemon enumerates for each repository, whether or not a window has it
  open.
- **Best-effort and degrading.** Discovery tolerates a folder inside a
  subdirectory or a linked worktree. A folder that is not a git repo, a detached
  HEAD, or a branch with no upstream is still listed — just without the fields it
  cannot supply (no `branch`, or `branch` with no `ahead`/`behind`). The
  `main_repo` name and `is_worktree` flag are resolved from the repository itself,
  so they are present even for an unborn or detached HEAD (only a non-repo folder
  omits them). The `ahead-behind` op degrades the same way, but since #1457 "no
  upstream" no longer implies "omitted from `results`": a path is included
  whenever *either* `ahead`/`behind` or `main_behind` resolves, so a branch with
  no upstream at all but a resolvable, genuinely-behind default branch still gets
  a row (`main_behind` alone). Only a path where **neither** resolves — not a
  repo, or a detached/unborn HEAD — is omitted entirely. A branch with no
  upstream likewise has no `upstream_sha`, so its own-upstream sync indicator
  stays absent even though `main_behind` may still show. The enrichment never
  fails a `list`.
- **The lazy counts still refresh themselves (#1344).** Fetching divergence on demand
  means it refreshes only when a client re-renders, and a client re-renders only on a
  snapshot delta — so the snapshot must carry a signal for every action that can
  change the counts. Committing moves `head_sha`; **pushing and fetching move only
  the remote-tracking ref**, which is why `upstream_sha` rides too. Both are refs
  reads, so they clear the same every-worktree-every-tick bar the revwalk failed.
  Without `upstream_sha` a fully-pushed worktree kept reporting `↑1 ↓0` indefinitely,
  correctable only by collapsing and re-expanding the repo.

### Tuning the refresh cadence

Two periodic timers drive this git enrichment; both are whole-seconds
environment knobs (#1305), so a git-heavy tree can trade a little freshness for
lower idle CPU. A blank, non-numeric, or `0` value falls back to the default.

| Env var | Default | What it governs |
| --- | --- | --- |
| `OMNI_DEV_DAEMON_STREAM_TICK` | `10` | How often a `subscribe` stream re-samples on-disk git state absent a registry change. The coalescing snapshot cache (#1303) is sized to the same value, so the shared `build_tree` runs at most once per tick no matter how many windows subscribe. |
| `OMNI_DEV_DAEMON_MENU_REFRESH` | `10` | How often the background task recomputes the tray menu snapshot. This is an independent per-window git walk (it does **not** read the coalescing cache and still computes `ahead`/`behind` inline), so it dominates idle CPU on a large tree — relaxing it is the biggest single win. |
| `OMNI_DEV_DAEMON_PR_POLL` | `10` | How often the PR badge poller re-asks GitHub **while a badge is pending and fresh** (#1337), and how often it wakes to look. While pending it holds this fast rate for ~2 minutes after a push, then escalates ×2 toward a 60 s ceiling (#1389); once every badge is terminal it backs off ×2 to a 30-minute ceiling, and it asks nothing while no window is registered — so this is the *fast* end of the range, not a sustained rate. A wake is only a cached-snapshot read; a **grown** watch (an added branch, or a moved upstream — you pushed, #1344) makes it ask immediately regardless of the backoff, but a **local commit** (#1389) and a pure **removal** (#1389) do not. Each poll is one `gh api graphql` costing **1 point** of GitHub's 5,000/hour budget regardless of how many repos, worktrees, or windows are open — so this knob is about battery and wakeups, not quota. |
| `OMNI_DEV_DAEMON_PR_DEBOUNCE` | `2` | How long the PR poller waits for the change-notify to go quiet before snapshotting (#1389), so a VS Code or daemon restart's one-window-at-a-time re-registration storm collapses into a single fetch on the final watch set instead of one per window. Bounded (~4× this) so a steady drip of changes cannot postpone a poll forever. |
| `OMNI_DEV_DAEMON_RATE_LIMIT_POLL` | `60` | How often the [GitHub rate-limit monitor](#github-rate-limit-monitor) (#1375) re-reads `gh api rate_limit`. A fixed cadence with no backoff (the read is exempt), but **gated on activity** (#1389, fix 8b): it only spends a subprocess while a window is registered or a polling lease is active — an idle daemon leaves the last reading in place, and the graphql figure stays fresh from each PR poll's folded-in budget (fix 8a). One free `gh` subprocess per interval while active. |
| `OMNI_DEV_DAEMON_OPEN_PR_TTL` | `60` | How long the daemon reuses a repo's `gh pr list` result before re-fetching, backing the shared [`open-prs`](#companion-contract-for-the-extension-and-other-clients) op (#1389, fix 7). Within the TTL, every window's "Open Pull Request…" lookup for that repo — and any transient badge fallback — is served from the one cached list, so N windows cost one counted `gh`, not N. |

Both were relaxed from their original 2–3 s (#1305). Neither affects the latency
of a user action: a window open/close or show-closed toggle still pushes
promptly via the change-notify, and the tray menu still opens instantly from its
cache. The only thing that grows staler is *passively observed* on-disk git state
(branch, `ahead`/`behind`, dirty status), which now surfaces within the interval
rather than every 2–3 s. Set a lower value if you want tighter freshness.

## Security

**No new trust boundary** ([ADR-0040](adrs/adr-0040.md)). Requests ride the
daemon's existing `0600` Unix control socket in its `0700` directory; no secret is
persisted; everything is loopback/filesystem-local. The residual exposure is
bounded by socket ownership — reading the socket reveals your open repo *paths*,
and writing it (already requiring the owning local user) could inject entries or,
via the **`open` op** (#1266), spawn `code` on a supplied path. That op is a
small, deliberate escalation: before it, only the human clicking the tray could
spawn `code`; now a socket *writer* can too — but still only as the owning local
user, and only on an **existing absolute directory** (which also blocks a
`-`-leading path from being parsed by `code` as a flag). The focus action and the
`open` op share that same guard before spawning `code`. Registry strings
(`repo`/`folders`, and the companion `title`) are writer-influenced metadata, so
the `worktrees list` and `worktrees tree` tables strip control characters (C0,
DEL, C1) from the strings they render before writing to the terminal — a
registered entry (or a worktree path / GitHub identity) cannot inject ANSI escape
sequences into the operator's TTY (#1137). The daemon-computed `branch` is a git
ref name (which cannot contain control bytes) but is sanitized on the same path as
defense-in-depth. Native tray menus do not interpret ANSI, and the `-o json`
output escapes control bytes via JSON encoding.

The **`tree`** op, the **`subscribe`** stream, and the **`ahead-behind`** op add
**no new exposure** beyond the existing reads. `tree` enumerates the worktrees of
repositories the socket owner already has windows open on, and reveals
repo/worktree *paths* and the GitHub `owner/name` of `origin` — all derivable by the
same owner from those open folders. `subscribe` streams exactly that same snapshot
on a schedule; a subscriber learns nothing a repeated `tree` poll would not.
`ahead-behind` only computes local commit-graph divergence for paths the caller
supplies (in practice the same worktrees `tree` already returned), so it reveals
nothing the owner could not read from those repos directly — `main_behind`
(#1457) is the same local commit-graph walk against a different target ref and
adds nothing new. The stream is bounded and self-limiting:
it lives on one `0600` connection, coalesces bursts, diffs to suppress duplicate
frames, and is torn down on client disconnect, an explicit cancel line, or daemon
shutdown.

The **`close`** op is the one **destructive** capability, and a real escalation of
this threat model ([ADR-0049](adrs/adr-0049.md)): a socket *writer* can delete a
linked worktree's files and close windows. It stays same-user-bounded — the `0600`
socket already requires the owning local user, so no new principal gains anything —
and deletion is confined by a **`git2`-enforced guard in the daemon** (not the UI):
the target must be a real **linked** worktree of a discoverable repository, and the
daemon **refuses to remove the main working tree** even if a malformed client asks.
It never shells out (`git2` prune, avoiding the launcher's `PATH` problem) and
refuses a locked worktree rather than forcing past it. No secret and no state is
persisted: the per-window close directive is in-memory and lost on a daemon
restart, so an in-flight close simply aborts and the user retries. So the two
capabilities the whole expansion adds to a socket *writer* are the `open` spawn and
the `close` deletion, both same-user-bounded and guarded; see
[ADR-0048](adrs/adr-0048.md) and [ADR-0049](adrs/adr-0049.md) for the full
threat-model notes.

The **`rebase`** op **rewrites git history** from the daemon
([ADR-0059](adrs/adr-0059.md)) — an escalation beyond `close`'s file deletion, and
the second op after `merge-queue` to spend the user's ambient credentials. It is
same-user-bounded for the same reason everything else here is (the `0600` socket
already requires the owning local user, so no new principal gains anything), and
guarded in the daemon rather than by the UI: it rewrites only branches the client
names — the main working tree included, since [ADR-0060](adrs/adr-0060.md); that
guard is `close`'s alone now, unrelated to rebase — skips a dirty tree / detached
HEAD / operation already in progress, and **re-validates all of it on execute**
rather than trusting a phase-1 result the client sends back. Concurrent executes are serialized, since linked worktrees share
one object database. No secret is read or persisted; the fetch relies entirely on
the user's ambient `git` configuration, reached through `ssh-agent` exactly as the
user's own shell would.

One genuinely new residual: with `keep_conflicts` a worktree can be **left
mid-rebase** by a socket request. That is the intent (#1415) rather than a lapse,
and it is recoverable with ordinary git (`--continue` or `--abort`) — but it is a
state a client can now leave behind, which is why it is cued durably in the tree,
named in the summary toast, and counted in the `rebase execute` audit line.

The **`push`** op is the first to **write to a remote with the user's ambient git
credentials** ([ADR-0061](adrs/adr-0061.md)) — `merge-queue` mutates the remote
through `gh` and its own token, and `rebase` only fetches — so it is the largest
escalation in this threat model to date: a socket writer can publish rewritten
history that other people then have to recover from. It is same-user-bounded like
everything else here, publishes only branches the client names, and re-validates
on execute.

The mitigation that actually carries the weight is **not ours**. Every
non-fast-forward goes out as `--force-with-lease --force-if-includes`, and the
lease is enforced by `git` against the remote's real tip: the daemon therefore
**cannot** overwrite a commit it has not seen, however wrong the request is. There
is no `--force` anywhere in the surface — no engine option, no CLI flag, no modal
escape hatch, and **no field in the wire payload** for a socket client to set — so
this is a property of the op rather than of the UI in front of it. A refused lease
is reported (distinctly from every other rejection, naming `git fetch` as the fix)
and counted in the `push execute` audit line as `stale_rejected`.

Two further guards live in the daemon's engine rather than the menu, so they hold
however the op is reached: the **remote default branch is never force-pushed**,
whichever worktree has it checked out — the gate is the branch name, not `is_main`
(this deliberately inverts [ADR-0060](adrs/adr-0060.md), which applies to rebase's
*local* rewrite and not to publishing one) — and a branch publishes only to its own
upstream's remote, never one the client picks. No secret is read or persisted; the
push relies entirely on the user's ambient `git` configuration, reached through
`ssh-agent`/credential helpers exactly as the user's own shell would. Nothing
durable is left behind: the transient `pushing` mark is in-memory and cleared on
every exit path, including a panicked task.

The **`worktrees rebase`** CLI command adds **no** socket capability: it never
opens the control socket ([ADR-0055](adrs/adr-0055.md)), running the same engine
in the invoking user's own shell with their `git` credentials. It applies the same
guards, confirms by default, and (absent `--keep-conflicts`) aborts-and-restores
rather than leaving a worktree dirty or half-rebased.

The tree view's **Rebase on main** action (#1409) changes none of that: it spawns
that same CLI in a VS Code integrated terminal — the user's own shell, their own
credentials — rather than sending an op, so it too adds **no new socket
capability**. What it does add is a one-click path to a history rewrite, which is
why it always confirms with a modal listing every branch it would rewrite, and why
it is offered on **linked** worktrees only.

The **`reposition`** op adds no socket capability but does introduce a new **OS**
trust surface, the first in omni-dev ([ADR-0058](adrs/adr-0058.md)): moving another
application's window requires the macOS **Accessibility** permission, which is
coarse — a process holding it could in principle drive any application's UI. So the
*capability the daemon holds* is far broader than the *capability it uses*, and the
narrowness is enforced in our own code rather than by the OS: the Accessibility
module reads only window titles, geometry and state flags, writes only position and
size, and only for processes the caller named by pid. It never synthesises input,
reads window contents, raises, activates, or closes a window. All of it is confined
to one `#[allow(unsafe_code)]` module behind a four-method trait, with every
Accessibility call bounded by an explicit messaging timeout so an unresponsive
application cannot wedge a daemon thread.

On the socket itself the addition is mild: a *writer* can rearrange the owning
user's windows, which is annoying rather than dangerous, and already requires being
that user. Nothing is persisted — the one-level undo record is in-memory like the
close directive, so a daemon restart simply drops it. The grant is **opt-in** and
must be added by hand; without it the op moves nothing and says so. Note that
because omni-dev installs unsigned, the grant is invalidated by every upgrade and
must be re-applied — see [Window repositioning](#window-repositioning-macos).

The **`reload`** op (#1417) adds **no capability at all**, and needs no ADR. It rides
the same `0600` socket, persists nothing (the directive is in-memory like `close`'s,
so a daemon restart drops it), and touches neither git nor the OS — the daemon only
sets a flag that the target window reads on its own next heartbeat and acts on
itself. A socket *writer* can make the owning user's windows restart their extension
hosts, which is annoying rather than dangerous: it is strictly weaker than the
`close` capability that writer already has, it destroys nothing (VS Code's hot exit
preserves dirty editors), and it already requires being that user. The op logs window
keys and counts only — it never sees a path.

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

| op                | payload                                                               | success payload                                                           |
|-------------------|------------------------------------------------------------------------|----------------------------------------------------------------------------|
| `register`        | `{ key, folders[], repo?, title?, pid? }`                             | `{ ok: true }`                                                            |
| `heartbeat`       | `{ key }`                                                             | `{ known: <bool>, close?: true, reload?: true }`                          |
| `unregister`      | `{ key }`                                                             | `{ removed: <bool> }`                                                     |
| `list`            | `null`                                                                | `{ windows: [entry, …] }`                                                 |
| `tree`            | `null`                                                                | `{ repos: [repo, …], show_closed }`                                       |
| `ahead-behind`    | `{ paths: [path, …] }`                                                | `{ results: { "<path>": { ahead?, behind?, main_behind? } } }`            |
| `open`            | `{ path }`                                                            | `{ ok: true }`                                                            |
| `open-prs`        | `{ owner, name }`                                                     | `{ pull_requests: [pr, …] }`                                              |
| `close`           | `{ path, remove, requester_key?, confirmed? }`                        | *(safety report, or `{ removed/closed }`)*                                |
| `reload`          | `{ target_keys[] }`                                                   | `{ requested, signalled, unknown[] }`                                     |
| `merge-queue`     | `{ paths[], check?, confirmed? }`                                     | *(eligibility report, or enqueue result)*                                 |
| `rebase`          | `{ paths[], check?, confirmed?, keep_conflicts?, autostash?, onto? }` | `{ fetches[], worktrees[] }` *(plan, or result)*                          |
| `push`            | `{ paths[], check?, confirmed?, requester_key? }`                     | `{ worktrees[] }` *(plan, or result)*                                     |
| `reposition`      | `{ reference_key, target_keys[], check? }`                            | `{ trusted, reference?, blocked?, results[], moved, skipped, undoable? }` |
| `reposition-undo` | `null`                                                                | *(same shape, without `reference`)*                                       |
| `set-show-closed` | `{ show_closed }`                                                     | `{ ok: true }`                                                            |
| `set-polling`     | `{ owner, name, enabled }`                                            | `{ ok: true }`                                                            |
| `subscribe`       | `null`                                                                | *(stream — see below)*                                                    |

`open-prs` (#1389, fix 7) serves "Open Pull Request…" from the daemon: it returns a
repo's open PRs (`number, title, url, headRefName, baseRefName, isDraft, state,
author`, forwarded from `gh pr list --json`) from a **shared, TTL-cached** result
(default 60 s, `OMNI_DEV_DAEMON_OPEN_PR_TTL`) so N windows dedupe to one counted
`gh` per repo. The client answers a branch already carrying a `pr` badge straight
from the snapshot without ever calling it.

Every op except `subscribe` is strictly **request → one reply**. `subscribe` is the one
**streaming** op (see [Push subscription](#push-subscription)): the reply is a
sequence of `{ ok: true, payload: { repos: …, show_closed } }` lines on the same
connection — an initial snapshot, then a fresh one each time the view changes —
not a single reply. It uses no new wire type, so a client that only ever sends the
other ops is wire-identical to the ADR-0040 contract.

Where:

- `key` — a stable per-window identifier the companion **generates once per
  `activate()`** (a UUID). The daemon does not derive identity from
  `vscode.env.sessionId`; report it (and `pid`) only as metadata.
- `register` never errors because of registry pressure: past the 256-entry cap
  it evicts the longest-silent entry rather than rejecting, so the companion
  needs no retry logic (an evicted window re-registers off its next heartbeat).
- `folders` — absolute workspace-folder paths.
- `open` — `path` must be an existing **absolute** directory; the daemon then
  spawns `code <path>`, which focuses the already-open window for that folder or
  opens a new one. It shares the tray focus action's launcher resolution and
  guard (#1266), so a client (the companion, on double-click) never duplicates
  that logic. A relative or non-existent `path` is rejected with a clear error
  before anything is spawned (see [Security](#security)).
- `close` — closes a worktree's window and, for a **linked** worktree, deletes it
  ([ADR-0049](adrs/adr-0049.md)). It is **two-phase**, keyed off `confirmed`:
    - **Phase 1** (`remove:true`, `confirmed` absent) is a side-effect-free
      **safety check**. The success payload is a report
      `{ removable, is_main, open, window_key?, window_folder_count, risks:[{kind,
      detail}], info:[…] }`. `risks` lists what removal would lose — modified
      tracked files, untracked files (ignoring `.gitignore`d), an in-progress
      rebase/merge/cherry-pick, and commits reachable only from a detached HEAD;
      unpushed commits on a **named** branch are `info` only (the branch survives).
      `removable && risks == []` → the client deletes with no prompt; any risk →
      confirm first.
    - **Phase 2** (`confirmed:true`, or any `remove:false`) executes. With
      `remove:true` it deletes the (linked) worktree via `git2` prune after
      closing the owning window(s); the reply is `{ removed: true }`. With
      `remove:false` ("Close Window") it only closes the window and never inspects
      git at all — so it applies to **any** worktree, main or linked (#1357); the
      reply is `{ closed: true }`, and a target with no open window is a no-op
      success.
  Deletability keys **solely on `is_main`** (structural), never the branch name: a
  linked worktree on the default branch is fully deletable and its branch is kept.
  The daemon **refuses `remove:true` on a main working tree** regardless of the
  request — the defensive backstop behind the UI gating (see
  [Security](#security)). `requester_key` is the calling window's `key`, so a
  self-close (the requester owns the target) removes-then-replies and lets the
  extension close its own window, while a cross-window close signals the *other*
  window(s) and waits for them to `unregister` first.
  Every close is **auditable** in `omni-dev daemon logs` (the journal on Linux,
  #1364): the handler emits INFO `tracing` lines for the phase-1 verdict (target,
  owning window key, open, `removable`/`is_main`, blocking risk kinds) and the
  phase-2 execute (requesting window key, self-close vs. cross-window routing, then
  the terminal outcome — a pruned worktree / closed window at INFO, a wait-timeout
  or prune failure at WARN), plus an ERROR line if the safety check or the removal
  task itself fails (a non-git-worktree target, a panicked task). Only the worktree
  path and window keys are logged, never a secret.
- `reload` — signals each listed window to reload itself (#1417), the batch form of
  `Developer: Reload Window`. Addressed by **window key**, like `reposition` and
  unlike `close`: a reload acts on a window, and one tree row is one window, whereas
  a path can be open in several. There is no `requester_key` — a client that wants to
  reload *itself* does so directly rather than waiting a heartbeat for its own
  directive, so the daemon never needs to know who asked. The reply is
  `{ requested, signalled, unknown[] }`, where `requested` counts **distinct** keys
  (a repeated key asks for one reload, not two) and `unknown` names the keys with no
  live window — a window that closed between the client listing its targets and the
  op arriving. That is reported, never an error: the batch is a sweep. An empty
  `target_keys` is a no-op success returning zeros.
  Unlike `close`, the op **does not wait**: a reload has no completion the daemon can
  observe, since the window re-registers under the same key. So the reply says what
  was *signalled*, never what reloaded — and, because the directive rides the ~10s
  heartbeat, a cross-window reload lands **up to ~10s later** and a batch will
  visibly stagger. That latency is inherent to the companion contract (the daemon has
  no push channel to a window outside the `subscribe` stream) and is the same one
  `close` already accepts.
  Each `reload` is auditable in `omni-dev daemon logs`: one INFO line with the
  requested/signalled counts and any unknown keys. Only window keys are logged — this
  op never sees a path.
- `heartbeat` may carry the additive directives `close: true` and `reload: true` when
  the daemon needs a specific window to act on itself (a cross-window `close` or
  `reload`) — the only channel it has to a window it can reply to but never call.
  They ride the reply exactly like the `{ known:false } → re-register` precedent, are
  each taken-and-cleared so each fires once, and are omitted (older windows read only
  `known`) when nothing is pending. The companion runs
  `workbench.action.closeWindow` / `workbench.action.reloadWindow` on seeing them.
  They are set and consumed **independently**, so both can ride one reply; the
  companion checks `close` first, since closing subsumes reloading. A daemon restart
  drops any pending directive — the registry is in-memory — after which the user
  simply retries.
- A `tree` `repo` is
  `{ main_repo, github?, root, worktrees: [worktree, …] }`, where `github` is
  `{ owner, name }` present only when `origin` (or the first `github.com` remote)
  is a GitHub URL, and `root` is the absolute path of the main working tree. Repos
  are **derived from the open windows** (each open folder → its git common dir →
  repo root) and deduped, so a repo appears only while at least one of its windows
  is open (the v1 model — [ADR-0048](adrs/adr-0048.md)).
- A `tree` `worktree` is
  `{ path, branch?, head_sha?, upstream_sha?, is_main, open, window_key?, pr?, pr_none?,
  operation?, rebasing?, pushing? }`. The
  main working tree
  comes first (`is_main: true`), then linked worktrees sorted by path. `open` is
  `true` when a live window currently has that worktree open, and `window_key` (the
  open window's registry `key`, the handle a focus action resolves) is present only
  then. `branch`, `head_sha`, and `upstream_sha` are daemon-computed,
  independently-degrading git fields. `head_sha` is the commit HEAD points at —
  present even on a detached HEAD (which has a commit but no branch), absent on an
  unborn one. `upstream_sha` is the commit the branch's upstream ref points at,
  absent when it tracks no upstream.
  Both ride the snapshot because each is a refs read rather than a walk, and because
  between them they are what make local git work a **visible delta**: the subscribe
  stream pushes only when a snapshot differs from the last, so a change invisible on
  the wire is dropped by the diff and no window re-renders. They are needed
  separately because they move on different actions — committing moves `head_sha`
  (#1337), while **pushing moves only `refs/remotes/<remote>/<branch>`**, so
  `upstream_sha` is the sole field that changes across a push (#1344). A `fetch` that
  leaves you behind is the same shape. **`ahead`/`behind` are
  deliberately absent** from this snapshot — they are the expensive part and are
  fetched on demand via the `ahead-behind` op (#1306; see
  [Git enrichment](#git-enrichment)); carrying the two OIDs they are *computed from*
  is what tells a client to re-ask for them. `pr` is
  `{ number, isDraft, checks, url }` where `checks` is
  `success | failure | pending | none` — resolved by the daemon's background poller
  (#1337; see [Pull requests](#pull-requests)) and absent until the first poll
  lands, for a non-GitHub repo, or when no open PR heads the branch. Note `isDraft`
  is camelCase: it predates the move and every consumer already reads that key.
  `pr_none` is the **explicit negative** (#1370): a skip-when-false boolean set
  when the poller checked the branch and found **no open PR** (a never-pushed
  branch included), mutually exclusive with `pr`. Both absent still means "not
  resolved" — before the first poll, after a failed one, or from a pre-#1370
  daemon — which is the only case a client's degraded `gh` fallback should cover.
  `operation` and `rebasing` are the two halves of the **rebase cue** (#1415), both
  skip-when-absent so a pre-#1415 client sees the identical bytes. `operation` is
  the worktree's own in-progress git operation read off disk —
  `rebase | rebase-interactive | merge | cherry-pick | revert | bisect |
  apply-mailbox`, absent when clean — and is **durable**: a conflict the `rebase`
  op left in place shows here across daemon restarts until it is resolved.
  `rebasing` is a skip-when-false boolean meaning the daemon is rebasing that
  worktree *right now*, from in-memory state. They are not redundant: a rebase that
  applies cleanly never writes an on-disk state for `operation` to report, and a
  conflicting one only writes it at the moment of collision. Treat an unrecognised
  `operation` value as "some operation in progress" rather than ignoring it.
  `pushing` is the `rebasing` twin for the **`push` op** (#1443), likewise
  skip-when-false and absent from a pre-#1443 daemon. It has **no durable
  counterpart**: a push writes no on-disk state, so unlike a rebase there is
  nothing for `operation` to report and this flag is the whole cue. A *completed*
  push is instead observable as `upstream_sha` moving, which is exactly why that
  field rides the snapshot.
- `ahead-behind` (#1306, extended #1457) — the **lazy** per-worktree divergence
  op. Given `{ paths: [<worktree path>, …] }`, it returns
  `{ results: { "<path>": { ahead?, behind?, main_behind? } } }`, keyed by the
  requested path. `ahead`/`behind` (a branch's divergence from its own upstream)
  fold in together or not at all; `main_behind` (its divergence from the
  repository's remote default branch, resolved the same local-only way
  `worktrees rebase`'s `--onto` default is) is **independently optional** — it
  can be present when `ahead`/`behind` are absent (no upstream at all, but
  behind the default branch) or absent when they are present (the branch's own
  upstream *is* already the default branch, so it would just duplicate
  `behind`). A path is **omitted** from `results` entirely only when *neither*
  side resolves (not a repo, or a detached/unborn HEAD) — the client renders it
  with no sync indicator. It exists so the streamed `tree`/`subscribe` snapshot
  can stay cheap: a client fetches divergence only for the worktrees it shows
  (the extension when a repo is expanded; `worktrees tree` once for the whole
  tree), rather than the daemon walking every worktree's commit graph on every
  tick.
- `subscribe` streams the `tree` payload live; see
  [Push subscription](#push-subscription) for the framing, coalescing, and
  teardown semantics.
- `show_closed` — the daemon-backed **show/hide-closed toggle** (#1301): a single
  cross-window boolean carried in every `tree`/`subscribe` snapshot, `true` = show
  worktrees with no open window (the default). `set-show-closed` (payload
  `{ show_closed }`) sets it; a real change re-pushes a snapshot to **every**
  subscriber, so all windows re-render together and a newly-opened window
  initializes from its first snapshot. It lives in the daemon precisely because
  the per-window `context.globalState` it replaced was read-once with no
  cross-window change event and raced a new window's first read. In-memory like
  the rest of the registry, so a **daemon restart resets it to the default**
  (`true`); the next snapshot propagates that reset to every window. The tree-view
  filter itself is client-side — `show_closed` only tells each window which way to
  filter, so the `repos` payload is unchanged (a repo, derived from open windows,
  always keeps ≥1 open worktree, so hiding never empties it).
- `set-polling` / `polling_enabled` — the **per-repository PR-poll toggle**
  (#1376). PR-badge polling defaults **off**: the daemon polls a repo's badges
  only once the user enables it, so a repo it is not polling issues **zero** `gh`
  (the poll's single `gh api graphql` never mentions it). `set-polling` (payload
  `{ owner, name, enabled }`) flips one GitHub repo — covering **all** its
  worktrees/branches, since it is keyed by `owner/name` — and, like
  `set-show-closed`, an effective off→on/on→off change re-pushes a snapshot to
  every subscriber. Each repo in the `tree`/`subscribe` snapshot carries
  `polling_enabled: true` while the daemon is polling it (omitted, i.e. false,
  otherwise — so a not-polled repo and a pre-#1376 daemon are byte-identical). The
  companion colours the repo icon **green** when set and **gray** when not, and
  gates its "Enable/Disable PR Polling" context-menu items on it. Disabling a repo
  also drops its badges immediately: the snapshot build skips folding badges onto a
  not-polled repo, so the next pushed frame strips them without waiting for a poll.
- **Enabling is a 15-minute lease, not a permanent flag.** Each `set-polling`
  enable leases the repo for **15 minutes**, after which it **auto-disables** — so
  an idle repo stops costing `gh` even if the user forgets to turn it off, which is
  the whole budget win. Re-enabling refreshes the lease; **Disable** ends it early.
  Expiry is enforced lazily, reaped on read the same way stale windows are: within
  one snapshot tick (~10 s) of a lease elapsing, the repo drops `polling_enabled`,
  its badges strip, and the poller stops watching it. The lease state is
  **persisted** under the daemon data dir (`<data_dir>/omni-dev/worktrees-polling.json`,
  `0600`, storing each enabled repo **with its expiry**), so a daemon restart
  within the window keeps the *remaining* lease rather than resetting the clock;
  an already-expired lease is dropped on load. The extension-only global
  `omniDevWorktrees.showPullRequests` setting is a master switch above all of this:
  when off, the companion strips badges and greys icons regardless. (The daemon
  cannot see that extension-only setting, so an explicitly-enabled repo would still
  be polled while the global is off — a benign known gap, since default-off already
  means nothing is polled until a repo is enabled, and the lease caps it at 15 min
  anyway.)
- A `list` `entry` is
  `{ key, folders[], repo?, title?, pid?, branch?, head_sha?, upstream_sha?, ahead?,
  behind?, main_repo?, is_worktree?, last_seen }` with `last_seen` as an RFC 3339
  timestamp; consumers
  compute age from it. The git fields carry the same meanings as on a `tree`
  `worktree` above — they come from the same daemon-side enrichment, flattened onto
  the entry — except that `list` is a bounded, non-streamed surface, so it computes
  `ahead`/`behind` inline rather than leaving them to the `ahead-behind` op. Entries are sorted by `(repo, key)` for deterministic
  output. The companion-reported fields are stored and served verbatim on the wire
  (and in `-o json`); only the human-readable `worktrees list` table sanitizes
  them for terminal display (see Security).
- `branch`, `ahead`, `behind`, `main_repo`, `is_worktree` are **daemon-computed,
  not companion-reported**: the daemon derives them from the primary folder's git
  state on every read (see [Git enrichment](#git-enrichment)). Each is optional
  and omitted when it does not apply — no `branch` for a non-repo or detached
  HEAD; no `ahead`/`behind` when the branch tracks no upstream; no `main_repo` for
  a non-repo folder; `is_worktree` omitted (false) for a normal checkout.
  `main_repo` is the parent repository's directory name (so a linked worktree
  names the repo it belongs to rather than its worktree-folder basename). New
  optional fields like these follow the protocol's `#[serde(skip_serializing_if)]`
  convention, so an older client simply ignores them.

Companion lifecycle, per window. The reporter half (register/heartbeat/
unregister) is unchanged from ADR-0040; the tree-view half opens one long-lived
`subscribe` connection alongside it:

```text
activate():    connect(socket) → {service:"worktrees", op:"register",
                                   payload:{key, folders, repo, title, pid}}
               connect(socket) → {service:"worktrees", op:"subscribe"}   // long-lived, reads pushed snapshots
heartbeat:     every ~10s → {op:"heartbeat", key}     // close self if {close:true}; else reload self if {reload:true}; else re-register if {known:false}
deactivate():  {op:"unregister", key}   + close the subscribe socket
```

Example exchange:

```text
→ {"service":"worktrees","op":"register","payload":{"key":"3f1c…","folders":["/home/me/omni-dev"],"repo":"omni-dev","title":"omni-dev — main","pid":4321}}
← {"ok":true,"payload":{"ok":true}}
→ {"service":"worktrees","op":"list"}
← {"ok":true,"payload":{"windows":[{"key":"3f1c…","folders":["/home/me/omni-dev"],"repo":"omni-dev","title":"omni-dev — main","pid":4321,"branch":"main","ahead":2,"behind":0,"main_repo":"omni-dev","last_seen":"2026-06-23T01:20:00Z"}]}}
→ {"service":"worktrees","op":"tree"}
← {"ok":true,"payload":{"repos":[{"main_repo":"omni-dev","github":{"owner":"rust-works","name":"omni-dev"},"root":"/home/me/omni-dev","worktrees":[{"path":"/home/me/omni-dev","branch":"main","head_sha":"64ca4a88…","upstream_sha":"64ca4a88…","is_main":true,"open":true,"window_key":"3f1c…","pr_none":true},{"path":"/home/me/wt/issue-1300","branch":"issue-1300","head_sha":"9b2e77a1…","upstream_sha":"c05d1f3b…","is_main":false,"open":false,"pr":{"number":1300,"isDraft":false,"checks":"pending","url":"https://github.com/rust-works/omni-dev/pull/1300"}}]}],"show_closed":true}}
→ {"service":"worktrees","op":"ahead-behind","payload":{"paths":["/home/me/omni-dev","/home/me/wt/issue-1300"]}}
← {"ok":true,"payload":{"results":{"/home/me/omni-dev":{"ahead":2,"behind":0},"/home/me/wt/issue-1300":{"ahead":1,"behind":3,"main_behind":12}}}}
```

The `tree` snapshot carries `branch` but not `ahead`/`behind` — those are the
expensive part, fetched on demand via the `ahead-behind` op (#1306). The companion
sends no `branch`/`ahead`/`behind` on `register`; the daemon derives `branch` (and,
for `list`/`status`, `ahead`/`behind`) on read. `issue-1300`'s own upstream
(`origin/issue-1300`) is distinct from the repo's resolved default branch, so it
also carries `main_behind` (#1457); `main`'s own upstream *is* `origin/main`
itself, so its entry omits the field — `behind: 0` already reports that
divergence.

### Push subscription

A client that sends `{ "service": "worktrees", "op": "subscribe" }` switches that
connection to **push mode**: the daemon replies with an initial `tree` snapshot,
then pushes a fresh `{ ok: true, payload: { repos: …, show_closed } }` line each
time the view changes — a window registers or unregisters, an entry ages out, the
[`show_closed` toggle](#companion-contract-for-the-extension-and-other-clients)
flips, or (via a periodic ~3 s re-sample) an on-disk branch/commit change that
fires no registry event. The semantics a client can rely on:

- **No new wire type.** Every pushed line is an ordinary `DaemonReply::ok(payload)`.
  A reader tells a subscription apart from a one-shot op only by continuing to read
  lines rather than stopping after the first.
- **Coalesced and de-duplicated.** A burst of changes collapses into one wake, and
  the daemon diffs each snapshot against the last one it sent, so **two identical
  frames are never pushed in a row**. Treat each line as the current full state, not
  a delta.
- **One shared computation across subscribers.** The `tree` snapshot every
  subscriber receives is byte-identical, so the daemon builds it **once per ~3 s
  tick (or change) and fans the same result out to all open windows**, rather than
  re-walking every repo per subscriber (#1303). Daemon CPU therefore scales with
  the worktree count, not `windows × worktrees`. (The one-shot `tree` op is a rare
  manual refresh and computes fresh, bypassing this cache.)
- **Teardown.** The stream ends when the client sends **any** further line (an
  explicit cancel), disconnects, or the daemon shuts down — all three close the
  connection cleanly. Use a **dedicated** connection for `subscribe`; do not
  multiplex other ops onto it.
- **Reconnect is the client's job.** The daemon does not persist subscriptions;
  after a daemon restart or a dropped socket the client reconnects and re-subscribes.
  The companion does this with exponential backoff + jitter (500 ms → 10 s) and
  treats an absent daemon as a silent no-op, never an error dialog.

Back-compat is total: a client that never sends `subscribe` only ever sees the
classic one-reply exchange. See [ADR-0048](adrs/adr-0048.md) for the design.

## Scope and follow-ups

- The companion extension lives in [`editors/vscode/`](../editors/vscode/): a
  TypeScript reporter **and** tree-view UI that speaks the contract above, bundled
  with esbuild and packaged as a `.vsix` by its own path-filtered CI workflow.
  A sibling
  [`vscode-extension-release.yml`](../.github/workflows/vscode-extension-release.yml)
  publishes it to the VS Code Marketplace and Open VSX on a `vscode-v*` tag
  (#1279); it needs a one-time publisher/namespace + `VSCE_PAT`/`OVSX_PAT`
  secrets setup before the first publish. Until published, install the `.vsix`
  built by CI with `code --install-extension`.
- Git enrichment lives in Rust (#1186): the companion reports raw folder paths
  and the daemon computes per-worktree branch and ahead/behind state with `git2`
  (see [Git enrichment](#git-enrichment)), keeping the companion thin.
- The service and CLI are Unix-only (`#[cfg(unix)]`), like the rest of the daemon;
  they run on Windows only under WSL2, and a native (non-WSL) Windows port is
  tracked with the broader daemon work (#1363).
- **Window repositioning is macOS-only (v1)** ([ADR-0058](adrs/adr-0058.md)). The
  platform sits behind a four-method trait, so Linux/X11 (EWMH title matching) and
  Windows (`SetWindowPos`) would each be one additional implementation with no
  change to any caller; Wayland is generally not possible. Two other follow-ups:
  **code signing**, so the Accessibility grant survives an upgrade instead of being
  invalidated on every rebuild, and — should title matching prove too weak in
  practice — embedding the window key in `window.title` for exact matching, at the
  cost of a visibly modified title bar. Arranging windows into a grid or cascade,
  rather than copying one window's frame, is a separate feature.
- **Open-window-derived repos (v1):** a repository appears in the `tree` only while
  at least one of its windows is open. **Configured repo roots** — so a repo shows
  with zero windows open — are a deliberate follow-up ([ADR-0048](adrs/adr-0048.md),
  #1264); they would be the first state the service persists.
- **Push, not poll:** live updates use the change-notify plus a ~3 s safety tick
  for on-disk changes, so a branch move is reflected within the tick rather than
  instantly. A polling fallback and a filesystem watcher were both considered and
  deferred.
- The tree view is **view + focus only**, apart from three management actions: the
  destructive **close** ([ADR-0049](adrs/adr-0049.md)), **Rebase on main**
  ([Rebase onto main](#rebase-onto-main), #1409/#1415), and
  **Push (force-with-lease)** ([Push](#push-force-with-lease), #1443). The rest
  (add/prune) remain out of scope for this iteration. Since #1415 rebase *is* in the daemon
  ([ADR-0059](adrs/adr-0059.md)), which unblocks a **tray** "rebase all" — no
  longer barred by credentials, just unbuilt. Two more follow-ups it opens up:
  **Abort Rebase / Continue Rebase** row actions, now that the tree can see a
  left-in-place conflict, and **parallel** rebases (they still run one at a time,
  within a batch and across concurrent requests, because linked worktrees share one
  object database).
- **PR badges are `gh`-based and GitHub-only (#1296):** the tree resolves open PRs
  via the GitHub CLI on repo-expand ([ADR-0050](adrs/adr-0050.md)). Other forges
  (GitLab/Bitbucket) and PR *creation/review/merge* are out of scope — the latter
  defers to the GitHub Pull Requests extension. A daemon-side, cross-window PR
  cache was considered and rejected: it would put a GitHub token in the daemon,
  breaking its no-secret posture.
- **`main_behind` is scoped to the lazy `ahead-behind` op (#1457):** both its
  consumers — the tree view's `⇊N` glyph and `worktrees tree`'s `main-N` — get
  it. Wiring it into `list`/`status`/the tray menu (which eagerly compute the
  full `GitStatus` today) is a deliberate follow-up: the walk is cheap and
  local, but those surfaces enrich only the primary folder of each *open*
  window, and keeping this iteration scoped to the tree/`tree` kept it
  reviewable.
