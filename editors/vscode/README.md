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
npm ci              # reproducible install from the committed package-lock.json
npm run typecheck   # tsc --noEmit
npm run build       # esbuild → dist/extension.js
npm test            # tsc → out/, then node --test
npm run package     # vsce package → omni-dev-worktrees-<version>.vsix
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
code --install-extension omni-dev-worktrees-*.vsix
```

## Releasing

The extension is published to the **VS Code Marketplace** (Microsoft VS Code)
and **Open VSX** (VSCodium / Cursor / Windsurf / Gitpod / code-server) by
[`.github/workflows/vscode-extension-release.yml`](../../.github/workflows/vscode-extension-release.yml).
Its `version` is independent of the Rust crate's `Cargo.toml`.

To cut a release:

1. Bump `version` in [`package.json`](package.json) and run `npm install` to
   refresh `package-lock.json`; commit both.
2. Tag the merge commit `vscode-v<version>` (e.g. `vscode-v0.2.1`) and push the
   tag. The release workflow verifies the tag matches `package.json`, re-runs
   typecheck/build/test/package, then publishes the same `.vsix` to both
   registries.

A one-time account + secrets setup is required before the first publish (an
Azure DevOps publisher whose id matches `"publisher": "rust-works"`, the
`rust-works` Open VSX namespace, and the repo secrets `VSCE_PAT` + `OVSX_PAT`) —
see [#1279](https://github.com/rust-works/omni-dev/issues/1279).
