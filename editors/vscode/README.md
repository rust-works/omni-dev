# omni-dev Worktrees Reporter

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

## Requirements

- The omni-dev daemon running locally (`omni-dev daemon start`).
- **macOS or Linux only** — like the daemon, the companion is Unix-only; on
  Windows there is no daemon socket to talk to (tracked in
  [#1237](https://github.com/rust-works/omni-dev/issues/1237)).

## Settings

| Setting | Default | Description |
|---------|---------|-------------|
| `omniDevWorktrees.socketPath` | `""` | Override the daemon control-socket path (mirrors the daemon's `--socket`). Empty uses the computed default `<data_dir>/omni-dev/daemon.sock`. |
| `omniDevWorktrees.heartbeatSeconds` | `10` | Seconds between heartbeats. The daemon reaps a window after 30s of silence, so keep this well under 30. |

## Development

```bash
npm install         # no committed lockfile yet — see note below
npm run typecheck   # tsc --noEmit
npm run build       # esbuild → dist/extension.js
npm test            # tsc → out/, then node --test
npm run package     # vsce package → omni-dev-worktrees-<version>.vsix
```

> No `package-lock.json` is committed yet, so CI and local builds use
> `npm install`. Run `npm install` once and commit the generated lockfile to
> switch to reproducible `npm ci` builds (and enable the npm cache in CI).

Install a local build with:

```bash
code --install-extension omni-dev-worktrees-*.vsix
```

Publishing to the VS Code Marketplace / Open VSX is not yet wired up (it needs a
publisher account and CI secrets) — see the tracking issue for that follow-up.
