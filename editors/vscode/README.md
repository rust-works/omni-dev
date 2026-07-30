# omni-dev

A tiny VS Code companion extension for the [omni-dev](https://github.com/rust-works/omni-dev)
daemon's **worktrees service**. It reports each VS Code window's open worktrees
to the daemon so that `omni-dev worktrees list`, `omni-dev daemon status`, and
the macOS menu-bar "Worktrees" submenu can show the live set of repositories and
branches open across **every** window.

A VS Code extension host is sandboxed per window — each window sees only its own
`workspace.workspaceFolders` — so no extension can show the cross-window view on
its own. This companion is the **writer** for a single rendezvous point: the
resident daemon aggregates every window's report into one consistent view. See
[docs/worktrees-service.md](../../docs/worktrees-service.md) and
[ADR-0040](../../docs/adrs/adr-0040.md).

## What it does

Per window, over the daemon's local Unix control socket (newline-delimited JSON):

- **on activation** — `register` this window (its workspace folders, repo name,
  title, and pid) under a fresh per-activation UUID;
- **every ~10s** — `heartbeat`; if the daemon replies `known: false` (it was
  restarted and its in-memory registry forgot this window), re-`register`;
- **on folder change** — re-`register` the new folder set;
- **on deactivation** — `unregister`.

The daemon computes each worktree's live branch and ahead/behind state itself
(with `git2`), so this extension only reports raw folder paths and stays thin —
it never runs git.

If the daemon is **not running**, every call is a silent no-op: the extension
never surfaces an error or blocks the window.

## Worktrees view

The **Worktrees** activity-bar view lists every repository and git worktree open
across all your windows, fed live by the daemon. Each leaf shows the branch and
ahead/behind counts and an open badge; double-click to focus an already-open
window or open a worktree's folder. Two title-bar actions:

- **Refresh** — a one-shot re-fetch, a fallback for when the live subscription is
  momentarily down.
- **Hide / Show Worktrees Without a Window** — one toggle button (an *eye* icon
  when showing all, an *eye-closed* icon when hiding) that collapses the list to
  just the worktrees a window currently has open, and back. The default shows all
  worktrees. The setting is stored per-machine (`globalState`), so it reads the
  same in every window and survives a reload.

Right-click a leaf or repo for context-menu actions: **Open Worktree**, **Reload
Window**, **Close Window**, **Close Worktree**, **Rebase on main**, and — for a
`github.com` repo — **Open GitHub Repository**, **Open Pull Request…** and **Open
Pull Request in Browser…**.

The four worktree verbs are deliberately separate, because reloading a *window*,
closing a *window*, and deleting a *worktree* are different things:

- **Open Worktree** — opens (or focuses) a window for each selected worktree.
- **Reload Window** — reloads the window each selected worktree is open in (the
  batch form of `Developer: Reload Window`). Selected worktrees with no window are
  skipped. Nothing is confirmed and nothing is lost — VS Code's hot exit preserves
  dirty editors.
- **Close Window** — closes the window each selected worktree is open in, and
  **deletes nothing**. Available for any worktree that has a window, linked or
  main.
- **Close Worktree** — **deletes** each selected linked worktree and closes its
  window. Offered only on linked worktrees; a repository's main working tree is
  never deleted.

### Multi-select

Ctrl/cmd+click or shift+click to select several rows, then act on all of them at
once — the view lists every window's worktrees, so the useful verbs are plural
("open the PRs for these three branches", "close these five stale worktrees").

- **Open GitHub Repository** opens each selected row's repository page once. A
  worktree row counts as its parent repository, so a repo row selected with its
  own worktrees still opens one page. It never asks first — one page per selected
  row is a count you did pick.
- **Open Pull Request…** / **Open Pull Request in Browser…** open every selected
  row's PR. A repo node and one of its own worktrees both selected will not open
  the same PR twice. Above five PRs it asks first, since a repo node contributes
  *every* open PR of its repository — a count you did not pick.
- **Copy PR URL** copies one line per selected row — the PR's URL, or a
  `#`-commented placeholder naming a row that has none — so the block accounts
  for the whole selection rather than quietly shrinking to whatever had a PR.
- **Close Worktree** / **Close Window** run as one batch: a single confirmation
  listing exactly what will be deleted, then progress through the targets. A main
  working tree caught up in a **Close Worktree** selection is skipped and named,
  never deleted and never quietly downgraded to a window close.
- **Reload Window** runs as one batch with no confirmation, and reports what it
  did — including how many selections were skipped for having no window open.
  Windows other than this one reload on their next heartbeat, so a batch lands
  over the following ~10 seconds rather than all at once; this window reloads
  last, since doing so ends the extension host.
- **Move Claude Session Here** is hidden while more than one row is selected — its
  argument is a single *destination*, so it has no multi-target meaning.
- Selecting rows never opens them: double-click still opens a worktree, and
  ctrl/shift+click only changes the selection.

### Open GitHub Repository

Right-click a **repository** row with a `github.com` origin and choose **Open
GitHub Repository** to open `https://github.com/<owner>/<name>` in your default
browser — the Issues tab, Actions, Settings, the code browser, the `README`. Every
other GitHub action in the view is pull-request shaped, so reaching the repository
itself otherwise meant leaving the view and typing the URL, or going via a PR's
breadcrumb.

It is the one GitHub action that needs **nothing**: the URL is built from the
identity the daemon already put in the tree, so there is no `gh`, no network and no
daemon round-trip — and correspondingly no progress notification and no
confirmation. The action is offered on repository rows only, since a worktree's
repository page is its parent's and a second menu entry would add no information.
Repositories with no `github.com` origin do not show it at all.

### Open Pull Request…

Right-click a repository or worktree with a `github.com` origin and choose **Open
Pull Request…** to open its pull request(s) **as a tab inside VS Code**, or **Open
Pull Request in Browser…** to open them on `github.com` in your **default
browser**. The two sit together in the menu, in-editor first. Both find the PRs
the same way:

- a **worktree** node opens the PR(s) whose head branch matches its checked-out
  branch; a **repository** node fans out to all the repo's open PRs;
- **no PR** shows a friendly info message; **one** opens directly; **several**
  offer a multi-select quick-pick so you can open any of them or all at once.

PRs are discovered with the `gh` CLI (reusing its existing auth). **Open Pull
Request…** then hands off to the **GitHub Pull Requests** extension's URI handler;
if that extension is not installed, a single warning offers **Install** or **Copy
PR URL** — it never silently falls back to a browser. **Open Pull Request in
Browser…** is the explicit way to ask for one: it opens the PR's `github.com` page
with your OS default browser and needs no extension at all.

### Copy PR URL

Right-click any repository or worktree rows and choose **Copy PR URL** to put their
pull request links on the clipboard, ready to paste into a prompt, a stand-up note,
a `gh` one-liner or a review checklist. It writes **one line per selected row**, in
selection order:

```
https://github.com/rust-works/omni-dev/pull/1417
# No PR for issue-1428-configurable-row-icon-colour in /Users/me/wrk/work-trees/omni-dev/issue-1428
https://github.com/rust-works/omni-dev/pull/1415
```

The placeholders are the point as much as the links are: a list that silently
dropped the PR-less rows would leave you unable to tell which of the six worktrees
you selected are unrepresented. They are commented with `#` so the block stays
paste-safe into a shell, a YAML/TOML scratch file or a markdown list.

- A **repository** row contributes every open PR it has, one line each, and says
  `# No open PRs for <owner>/<name>` when it has none.
- A row with **no `github.com` origin**, or a **detached** worktree, is a
  placeholder rather than an error — so the command is offered on every row, not
  only GitHub ones.
- A lookup that **fails** says so distinctly (`# PR lookup failed for …`), never as
  a settled "no PR", and the rest of the batch still copies.
- URLs **de-duplicate** across the whole block, so selecting a repository together
  with one of its own worktrees lists that PR once; placeholders never
  de-duplicate, because they name different worktrees.
- For a repository with **PR polling** enabled the whole thing is answered from the
  daemon's snapshot — no `gh`, no network.

### Rebase on main

Right-click one or more worktrees — the repository's **main working tree**
included — and choose **Rebase on main** to rebase their branches onto the
repository's **remote default branch**. Select as many as you like, across as
many repositories as you like: each repository's default branch is **fetched
once** for the whole batch, so keeping a fan-out of feature branches, and your
own main checkout, current is one gesture instead of one `cd` and one
`git pull --rebase` each.

- It **always confirms first**, listing exactly which branches would be
  rewritten, each with its real behind-count measured against the freshly
  fetched target — a rebase rewrites history.
- Worktrees with uncommitted changes, a detached `HEAD`, or a rebase already in
  progress are **reported and skipped**, not touched.
- A rebase that hits **conflicts is left mid-rebase** rather than rolled back,
  so you can resolve it in place and finish with `git rebase --continue`; the
  row keeps cueing it until you do, even across a restart. The rest of the
  batch continues either way.
- It runs through the **omni-dev daemon**, so the result comes back as a
  structured per-worktree summary rather than a terminal you have to watch.
  Requires `omni-dev daemon start` (see [Requirements](#requirements)); with no
  daemon running it says so and points at `omni-dev worktrees rebase <path>`,
  which does the whole job locally using your own shell's SSH agent and
  credential helper.

## Open Claude Code

A **Claude-in-a-box** button in the **editor title bar** (the top-right icon
cluster, alongside the Claude Code extension's own icon) opens the **Claude Code
CLI** in a terminal docked as an **editor tab** — one click instead of opening a
terminal, re-docking it to the editor area, and typing `claude` by hand.

- The terminal's working directory is the active window's workspace folder — the
  folder of the focused editor when it sits in one, else the first folder.
- Clicking again while it is still open **focuses** that terminal instead of
  spawning a duplicate; once you close it, the next click starts a fresh one.
- The launch command is `omniDevWorktrees.claudeCommand` (default `claude`); a
  shell prefix such as `proxy && claude` is allowed.

The button is window-level and **daemon-independent** — a plain terminal, no
socket involved — so it works even when the omni-dev daemon is not running.

## Requirements

- The omni-dev daemon running locally (`omni-dev daemon start`).
- **macOS or Linux only** — like the daemon, the companion is Unix-only; on
  Windows there is no daemon socket to talk to (tracked in
  [#1363](https://github.com/rust-works/omni-dev/issues/1363)).
- For the pull-request actions only (**Open Pull Request…**, **Open Pull Request in
  Browser…**, **Copy PR URL**): the [`gh` CLI](https://cli.github.com/) installed
  and authenticated (`gh auth login`) — they all discover PRs with it, except where
  the daemon's PR polling has already resolved the answer. `gh` is found on your
  `PATH` or in the usual install locations (Homebrew, `~/.local/bin`, …); if a
  GUI-launched editor inherits a minimal `PATH` and can't find it, set
  `OMNI_DEV_GH_BIN` to its full path.
- For **Open Pull Request…** additionally: the [**GitHub Pull
  Requests**](https://marketplace.visualstudio.com/items?itemName=GitHub.vscode-pull-request-github)
  extension (`GitHub.vscode-pull-request-github`) to render the PR in a tab.
  **Open Pull Request in Browser…** does not need it.
- For **Rebase on main** only: the **`omni-dev` CLI** itself
  (`cargo install omni-dev`) — this is the one action that runs the CLI in a
  terminal rather than asking the daemon, so it needs the binary but **not** a
  running daemon. It is found via `OMNI_DEV_BIN`, then the usual install locations
  (`~/.cargo/bin`, Homebrew, `~/.local/bin`, …), then your `PATH`.

## Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `omniDevWorktrees.socketPath` | `""` | Override the daemon control-socket path (mirrors the daemon's `--socket`). Empty uses the computed default `<data_dir>/omni-dev/daemon.sock`. |
| `omniDevWorktrees.heartbeatSeconds` | `10` | Seconds between heartbeats. The daemon reaps a window after 30s of silence, so keep this well under 30. |
| `omniDevWorktrees.claudeCommand` | `"claude"` | Command run by the **Open Claude Code** title-bar button. A shell prefix such as `proxy && claude` is allowed. |

## Development

```bash
npm ci              # reproducible install from the committed package-lock.json
npm run typecheck   # tsc --noEmit
npm run build       # esbuild → dist/extension.js
npm test            # tsc → out/, then node --test
npm run package     # vsce package → omni-dev-<version>.vsix
```

The Marketplace / Open VSX gallery icon is the top-level `"icon"` in
`package.json` (`media/icon.png`) — a 128×128 raster, since the Marketplace
rejects SVG there. Its source is [`media/icon.svg`](media/icon.svg) (the
[`media/worktrees.svg`](media/worktrees.svg) glyph on a gradient tile);
regenerate the PNG after editing it with:

```bash
sips -s format png media/icon.svg --out media/icon.png   # macOS
# or: rsvg-convert -w 128 -h 128 media/icon.svg -o media/icon.png
```

The `.svg` source is excluded from the packaged `.vsix` (see `.vscodeignore`);
only the `.png` ships.

Install a local build with:

```bash
code --install-extension omni-dev-*.vsix
```

## Releasing

The extension is published to the **VS Code Marketplace** (Microsoft VS Code)
and **Open VSX** (VSCodium / Cursor / Windsurf / Gitpod / code-server) by
[`.github/workflows/vscode-extension-release.yml`](../../.github/workflows/vscode-extension-release.yml).
Its `version` and release notes are independent of the Rust crate: the version
lives in [`package.json`](package.json) (not `Cargo.toml`) and the notes in
[`CHANGELOG.md`](CHANGELOG.md) (not the [repository-root
changelog](../../CHANGELOG.md), which tracks the crate). Both registries render a
**Changelog** tab from that file in the packaged `.vsix`, so every published
version needs an entry.

To cut a release:

1. Bump `version` in [`package.json`](package.json) and run `npm install` to
   refresh `package-lock.json`; commit both.
2. In [`CHANGELOG.md`](CHANGELOG.md), move the `[Unreleased]` items into a new
   `## [X.Y.Z] - YYYY-MM-DD` section (add one if `[Unreleased]` is empty), grouped
   under Keep a Changelog headings (Added / Changed / Fixed / …). Add entries to
   `[Unreleased]` as changes land, not all at once here.
3. Tag the merge commit `vscode-v<version>` (e.g. `vscode-v0.2.1`) and push the
   tag. The release workflow verifies the tag matches `package.json`, re-runs
   typecheck/build/test/package, then publishes the same `.vsix` to both
   registries (Open VSX is skipped when `OVSX_PAT` is unset — see below).

A one-time account + secrets setup is required before the first publish:

- **VS Code Marketplace (required):** an Azure DevOps publisher whose id matches
  `"publisher": "rust-works"` and the repo secret `VSCE_PAT`.
- **Open VSX (optional):** the `rust-works` Open VSX namespace and the repo secret
  `OVSX_PAT`. If `OVSX_PAT` is unset the workflow publishes to the Marketplace only
  and skips Open VSX (rather than failing), so you can add it later.

See [#1279](https://github.com/rust-works/omni-dev/issues/1279).
