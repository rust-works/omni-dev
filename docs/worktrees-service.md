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
`ahead`/`behind` merged back onto each worktree.

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
  **open badge** icon (#1274): a **blue tick** on the worktree open in *this*
  window, a **green dot** on one open in *another* window, and the plain branch
  glyph on a worktree with no live window. The current-window distinction comes
  from matching a worktree's `window_key` against this window's own key. The
  `↑ahead ↓behind` counts are **not** in the streamed snapshot — the extension
  fetches them **lazily when a repo is expanded** via the `ahead-behind` op (#1306),
  so only the worktrees you actually look at pay the divergence walk.
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

The tree view runs **alongside** the reporter lifecycle (register/heartbeat/
unregister): every window is both a reporter and, if it has the view open, a reader.

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

A right-click **"Open Pull Request…"** action on any GitHub worktree (or repo)
opens its PR **as a tab inside the editor** — never a browser. It discovers the
PR(s) via `gh` (a quick-pick when several match) and hands off to the **GitHub
Pull Requests** extension (`GitHub.vscode-pull-request-github`); if that extension
is absent it offers to install it or copy the PR URL.

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
  **asks GitHub** when the watched state moved or its backoff elapsed: ~10 s while a
  badge is pending (matching `gh pr checks --watch`), doubling to a 30-minute
  ceiling once everything is terminal, and nothing at all while no window is
  registered. The two cadences are separate because the triggers arrive by different
  routes: a window opening bumps the change-notify, but **nothing tells the daemon
  you committed** — so the loop looks often and cheaply, and pays only on a change.
  Override the fetch cadence with `OMNI_DEV_DAEMON_PR_POLL` (whole seconds). The
  daemon pushes to windows only when a verdict actually moves.
- **A commit invalidates the verdict immediately.** Each badge records the commit
  its verdict describes; when that differs from the worktree's `head_sha` the row
  renders ● rather than the previous commit's ✓ — no network call, on the very next
  snapshot. So committing or pushing never leaves a stale green standing, and the
  poller (which notices the moved head on its next wake) refreshes it for real
  shortly after. The same applies to unpushed work and to being behind the remote:
  the verdict simply is not about the commit in front of you.
- **Best-effort, silent.** A worktree with no branch, no matching PR, or on a
  non-GitHub repo shows nothing extra. If `gh` is absent or not authenticated,
  badges are simply omitted — no error dialog (the explicit "Open Pull Request…"
  action still surfaces the real `gh` error). A failing poll leaves the last known
  badges in place rather than blanking the rows.
- **Older daemons degrade.** Against a daemon predating #1337 — which omits `pr` —
  the extension falls back to its own `gh pr list` and shows the PR number and
  draft marker **without** a check glyph: nothing extension-side polls, so a verdict
  there could not refresh, and a stale `✓` is worse than none.
- **The check state is colored, not monochrome (#1324).** The `✓`/`✗`/`●` is a VS
  Code `FileDecoration` (the same mechanism git status uses to color `M`/`U`
  badges), painted from a custom `omnidev-worktree:` `resourceUri` the extension
  puts on each row — a custom scheme, so it never collides with git's own folder
  decorations. The color uses the theme `charts.{green,red,yellow}` palette, and the
  check state is encoded in the URI so a state change re-colors on its own. Purely
  extension-side: no daemon, wire, or trust-boundary change.
- **Opt-out.** The `omniDevWorktrees.showPullRequests` setting (default on) hides
  the badge. Since the daemon now supplies it on the snapshot, the setting strips
  it on the way in rather than skipping a lookup.

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
  snapshot computes only the *cheap* state (branch, `main_repo`, `is_worktree`) and
  **omits** `ahead`/`behind`. Divergence is instead fetched on demand through the
  **`ahead-behind`** op, batched by path, for just the worktrees a client is about
  to show (the extension does this when a repo is expanded; `worktrees tree` does it
  once for the whole tree). The bounded, non-streamed surfaces — `list`/`status`
  (the primary folder of each open window) and the tray `menu` (open windows only)
  — still compute `ahead`/`behind` inline, since the walk cost there is negligible.
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
  omits them). The `ahead-behind` op degrades the same way: a path with no upstream
  is simply omitted from its `results`. The enrichment never fails a `list`.

### Tuning the refresh cadence

Two periodic timers drive this git enrichment; both are whole-seconds
environment knobs (#1305), so a git-heavy tree can trade a little freshness for
lower idle CPU. A blank, non-numeric, or `0` value falls back to the default.

| Env var | Default | What it governs |
| --- | --- | --- |
| `OMNI_DEV_DAEMON_STREAM_TICK` | `10` | How often a `subscribe` stream re-samples on-disk git state absent a registry change. The coalescing snapshot cache (#1303) is sized to the same value, so the shared `build_tree` runs at most once per tick no matter how many windows subscribe. |
| `OMNI_DEV_DAEMON_MENU_REFRESH` | `10` | How often the background task recomputes the tray menu snapshot. This is an independent per-window git walk (it does **not** read the coalescing cache and still computes `ahead`/`behind` inline), so it dominates idle CPU on a large tree — relaxing it is the biggest single win. |
| `OMNI_DEV_DAEMON_PR_POLL` | `10` | How often the PR badge poller re-asks GitHub **while a badge is still pending** (#1337), and how often it wakes to look. It backs off ×2 to a 30-minute ceiling once every badge is terminal, and asks nothing while no window is registered, so this is the *fast* end of the range, not a sustained rate. A wake is only a cached-snapshot read; a moved HEAD (you committed) or a window opening makes it ask immediately, regardless of the backoff. Each poll is one `gh api graphql` costing **1 point** of GitHub's 5,000/hour budget regardless of how many repos, worktrees, or windows are open — so this knob is about battery and wakeups, not quota. |

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
nothing the owner could not read from those repos directly. The stream is bounded and self-limiting:
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

| op                | payload                                        | success payload                            |
|-------------------|------------------------------------------------|--------------------------------------------|
| `register`        | `{ key, folders[], repo?, title?, pid? }`      | `{ ok: true }`                             |
| `heartbeat`       | `{ key }`                                      | `{ known: <bool>, close?: true }`          |
| `unregister`      | `{ key }`                                      | `{ removed: <bool> }`                      |
| `list`            | `null`                                         | `{ windows: [entry, …] }`                  |
| `tree`            | `null`                                         | `{ repos: [repo, …], show_closed }`        |
| `ahead-behind`    | `{ paths: [path, …] }`                         | `{ results: { "<path>": { ahead, behind } } }` |
| `open`            | `{ path }`                                     | `{ ok: true }`                             |
| `close`           | `{ path, remove, requester_key?, confirmed? }` | *(safety report, or `{ removed/closed }`)* |
| `set-show-closed` | `{ show_closed }`                              | `{ ok: true }`                             |
| `subscribe`       | `null`                                         | *(stream — see below)*                     |

The first nine ops are strictly **request → one reply**. `subscribe` is the one
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
      `remove:false` ("Close Window", the main working tree) it only closes the
      window; the reply is `{ closed: true }`.
  Deletability keys **solely on `is_main`** (structural), never the branch name: a
  linked worktree on the default branch is fully deletable and its branch is kept.
  The daemon **refuses `remove:true` on a main working tree** regardless of the
  request — the defensive backstop behind the UI gating (see
  [Security](#security)). `requester_key` is the calling window's `key`, so a
  self-close (the requester owns the target) removes-then-replies and lets the
  extension close its own window, while a cross-window close signals the *other*
  window(s) and waits for them to `unregister` first.
- `heartbeat` may carry an additive `close: true` when the daemon needs a specific
  window to close itself (a cross-window `close`) — the only channel it has to a
  window it can reply to but never call. It rides the reply exactly like the
  `{ known:false } → re-register` precedent, is taken-and-cleared so it fires once,
  and is omitted (older windows read only `known`) when no close is pending. The
  companion runs `workbench.action.closeWindow` on seeing it.
- A `tree` `repo` is
  `{ main_repo, github?, root, worktrees: [worktree, …] }`, where `github` is
  `{ owner, name }` present only when `origin` (or the first `github.com` remote)
  is a GitHub URL, and `root` is the absolute path of the main working tree. Repos
  are **derived from the open windows** (each open folder → its git common dir →
  repo root) and deduped, so a repo appears only while at least one of its windows
  is open (the v1 model — [ADR-0048](adrs/adr-0048.md)).
- A `tree` `worktree` is
  `{ path, branch?, head_sha?, is_main, open, window_key?, pr? }`. The main working tree
  comes first (`is_main: true`), then linked worktrees sorted by path. `open` is
  `true` when a live window currently has that worktree open, and `window_key` (the
  open window's registry `key`, the handle a focus action resolves) is present only
  then. `branch` and `head_sha` are daemon-computed, independently-degrading git
  fields. `head_sha` is the commit HEAD points at — present even on a detached HEAD
  (which has a commit but no branch), absent on an unborn one. It rides the snapshot
  because it is a refs read rather than a walk, and because it is what makes a new
  commit a **visible delta**: the subscribe stream pushes only when a snapshot
  differs from the last, so without it a push serialised byte-identically, was
  dropped by the diff, and no window re-rendered (#1337). **`ahead`/`behind` are
  deliberately absent** from this snapshot — they are the expensive part and are
  fetched on demand via the `ahead-behind` op (#1306; see
  [Git enrichment](#git-enrichment)). `pr` is
  `{ number, isDraft, checks, url }` where `checks` is
  `success | failure | pending | none` — resolved by the daemon's background poller
  (#1337; see [Pull requests](#pull-requests)) and absent until the first poll
  lands, for a non-GitHub repo, or when no open PR heads the branch. Note `isDraft`
  is camelCase: it predates the move and every consumer already reads that key.
- `ahead-behind` (#1306) — the **lazy** per-worktree divergence op. Given
  `{ paths: [<worktree path>, …] }`, it returns
  `{ results: { "<path>": { ahead, behind } } }`, keyed by the requested path. A
  path that is not a repo, is on a detached/unborn HEAD, or tracks no upstream is
  **omitted** from `results` (the client renders it with no sync indicator). It
  exists so the streamed `tree`/`subscribe` snapshot can stay cheap: a client
  fetches divergence only for the worktrees it shows (the extension when a repo is
  expanded; `worktrees tree` once for the whole tree), rather than the daemon
  walking every worktree's commit graph on every tick.
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
- A `list` `entry` is
  `{ key, folders[], repo?, title?, pid?, branch?, ahead?, behind?, main_repo?,
  is_worktree?, last_seen }` with `last_seen` as an RFC 3339 timestamp; consumers
  compute age from it. Entries are sorted by `(repo, key)` for deterministic
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
heartbeat:     every ~10s → {op:"heartbeat", key}     // close self if {close:true}; else re-register if {known:false}
deactivate():  {op:"unregister", key}   + close the subscribe socket
```

Example exchange:

```text
→ {"service":"worktrees","op":"register","payload":{"key":"3f1c…","folders":["/home/me/omni-dev"],"repo":"omni-dev","title":"omni-dev — main","pid":4321}}
← {"ok":true,"payload":{"ok":true}}
→ {"service":"worktrees","op":"list"}
← {"ok":true,"payload":{"windows":[{"key":"3f1c…","folders":["/home/me/omni-dev"],"repo":"omni-dev","title":"omni-dev — main","pid":4321,"branch":"main","ahead":2,"behind":0,"main_repo":"omni-dev","last_seen":"2026-06-23T01:20:00Z"}]}}
→ {"service":"worktrees","op":"tree"}
← {"ok":true,"payload":{"repos":[{"main_repo":"omni-dev","github":{"owner":"rust-works","name":"omni-dev"},"root":"/home/me/omni-dev","worktrees":[{"path":"/home/me/omni-dev","branch":"main","head_sha":"64ca4a88…","is_main":true,"open":true,"window_key":"3f1c…"},{"path":"/home/me/wt/issue-1300","branch":"issue-1300","head_sha":"9b2e77a1…","is_main":false,"open":false,"pr":{"number":1300,"isDraft":false,"checks":"pending","url":"https://github.com/rust-works/omni-dev/pull/1300"}}]}],"show_closed":true}}
→ {"service":"worktrees","op":"ahead-behind","payload":{"paths":["/home/me/omni-dev","/home/me/wt/issue-1300"]}}
← {"ok":true,"payload":{"results":{"/home/me/omni-dev":{"ahead":2,"behind":0},"/home/me/wt/issue-1300":{"ahead":1,"behind":3}}}}
```

The `tree` snapshot carries `branch` but not `ahead`/`behind` — those are the
expensive part, fetched on demand via the `ahead-behind` op (#1306). The companion
sends no `branch`/`ahead`/`behind` on `register`; the daemon derives `branch` (and,
for `list`/`status`, `ahead`/`behind`) on read.

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
  Windows support is tracked with the broader daemon work (#1237).
- **Open-window-derived repos (v1):** a repository appears in the `tree` only while
  at least one of its windows is open. **Configured repo roots** — so a repo shows
  with zero windows open — are a deliberate follow-up ([ADR-0048](adrs/adr-0048.md),
  #1264); they would be the first state the service persists.
- **Push, not poll:** live updates use the change-notify plus a ~3 s safety tick
  for on-disk changes, so a branch move is reflected within the tick rather than
  instantly. A polling fallback and a filesystem watcher were both considered and
  deferred.
- The tree view is **view + focus only**; worktree *management* actions
  (add/remove/prune) are out of scope for this iteration (except the destructive
  **close** in [ADR-0049](adrs/adr-0049.md)).
- **PR badges are `gh`-based and GitHub-only (#1296):** the tree resolves open PRs
  via the GitHub CLI on repo-expand ([ADR-0050](adrs/adr-0050.md)). Other forges
  (GitLab/Bitbucket) and PR *creation/review/merge* are out of scope — the latter
  defers to the GitHub Pull Requests extension. A daemon-side, cross-window PR
  cache was considered and rejected: it would put a GitHub token in the daemon,
  breaking its no-secret posture.
