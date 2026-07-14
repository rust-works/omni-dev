# Changelog

All notable changes to the **omni-dev** VS Code extension are
documented in this file. The extension versions independently of the omni-dev
Rust crate — the crate's own changelog lives in the
[repository root](https://github.com/rust-works/omni-dev/blob/main/CHANGELOG.md).

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **Open Claude Code** ([#1322](https://github.com/rust-works/omni-dev/issues/1322)): a **Claude-in-a-box** button in the editor title bar opens the Claude Code CLI in a terminal docked as an **editor tab** (not the bottom panel) and runs the configured command.
  - The working directory is the active window's workspace folder — the focused editor's folder when it sits in one, else the first folder. Clicking again while the terminal is still open **focuses** it instead of spawning a duplicate; after you close it, a fresh click opens a new one.
  - The launch command is the new `omniDevWorktrees.claudeCommand` setting (default `claude`); a shell prefix such as `proxy && claude` is allowed. The button is window-level and daemon-independent, so it works even when the omni-dev daemon is not running.

### Changed
- **Colored PR check badge** ([#1324](https://github.com/rust-works/omni-dev/issues/1324)): a worktree's CI-check state now renders as a **colored** badge on the row — a green `✓` (passing), red `✗` (failing), or yellow `●` (pending) — instead of a monochrome glyph tucked into the muted description, so a passing and a failing PR are distinguishable at a glance.
  - The badge is a theme-aware `FileDecoration` (using the `charts.{green,red,yellow}` palette) painted from a custom `omnidev-worktree:` `resourceUri`, a custom scheme so it never collides with git's own folder decorations; the color also tints the row's branch label, matching how git status colors its badges.
  - The check glyph is removed from the description text (no duplicate); the row now reads just `#1319 draft`, with the color badge carrying the check state. A PR with no checks, and a worktree with no PR, show no badge as before.
  - Extension-only — no daemon, protocol, or trust-boundary change.

## [0.4.0] - 2026-07-13

### Added
- **Open Pull Request…** ([#1299](https://github.com/rust-works/omni-dev/issues/1299)): a context-menu action on the **Worktrees** view opens a repo's or worktree's pull request(s) **as a tab inside VS Code**, never a browser.
  - On a **worktree** node it finds the PR(s) whose head branch is that worktree's checked-out branch; on a **repository** node it fans out to all the repo's open PRs. No PR shows a friendly info message, one PR opens directly, and several offer a multi-select quick-pick to open any or all at once.
  - PRs are discovered with the `gh` CLI (reusing its existing auth) and open through the **GitHub Pull Requests** extension's (`GitHub.vscode-pull-request-github`) documented URI handler. When that extension is not installed, a single warning offers **Install** or **Copy PR URL** — it never silently falls back to a browser.
  - Only repos with a `github.com` origin get the action (the daemon reports `owner/name` for those); it requires the `gh` CLI and the GitHub Pull Requests extension. `gh` is resolved from your `PATH`, the usual install locations (Homebrew, `~/.local/bin`, …), or the `OMNI_DEV_GH_BIN` override, so a GUI-launched editor with a minimal `PATH` still finds it.

### Changed
- **Show/hide-closed toggle now syncs across windows** ([#1301](https://github.com/rust-works/omni-dev/issues/1301)): the toggle's state moves from per-window `context.globalState` to the daemon, which carries `show_closed` on every pushed snapshot — so a flip in any window re-renders every other window live, and a freshly-opened window initializes from the daemon's value on its first frame instead of racing a read-once local default. The toggle commands now send a `set-show-closed` op rather than writing `globalState`; the toggle's behaviour and default (all worktrees shown) are otherwise unchanged. Requires daemon 0.35.0 or later.

## [0.3.0] - 2026-07-11

### Added
- **Hide worktrees without a window** ([#1290](https://github.com/rust-works/omni-dev/issues/1290)): a title-bar toggle in the **Worktrees** view collapses the list to just the worktrees a VS Code window currently has open, and back — one button that swaps between an *eye* icon (showing all) and an *eye-closed* icon (hiding).
  - The default is unchanged — **all** worktrees are shown until you toggle.
  - The filter is entirely client-side (the daemon already reports each worktree's open state); its state is persisted in `globalState`, so it reads the same in every window and survives a reload, and a live snapshot keeps it applied.
  - Repos are derived from open windows (each keeps ≥1 open worktree), so hiding only trims closed rows and never empties a repo or the tree.

## [0.2.0] - 2026-07-10

### Added
- **Worktrees tree view** ([#1268](https://github.com/rust-works/omni-dev/issues/1268)): an activity-bar **Worktrees** view lists every repository and git worktree open across all your VS Code windows — the cross-window picture the daemon aggregates and that no single per-window-sandboxed extension can build on its own.
  - Double-click a leaf (or the inline **Open in VS Code** action) to focus an already-open window or open a worktree's folder.
  - Each leaf shows the daemon-computed branch and ahead/behind state (e.g. `main (+2 -1)`), so the extension stays thin and runs no git itself.
  - The view is push-based and self-healing: it re-subscribes after a daemon restart and shows an empty tree — never an error — when the daemon is not running.
- **Close Worktree / Close Window context menus** ([#1277](https://github.com/rust-works/omni-dev/issues/1277)): right-click a leaf to close a worktree's window and, for a **linked** worktree, delete it.
  - A side-effect-free safety check runs first; a modal confirm appears **only** when a removal would lose data (modified or untracked files, an in-progress rebase/merge/cherry-pick, or commits reachable only from a detached HEAD) or would close a multi-root window's other folders. A clean worktree closes with no prompt.
  - Unpushed commits on a named branch never block removal — the branch survives.
  - The main working tree offers **Close Window** only and is never deleted.
- **Marketplace / Open VSX gallery icon** ([#1280](https://github.com/rust-works/omni-dev/issues/1280)): the extension listing now carries a 128×128 gallery icon (`media/icon.png`).

## [0.1.0] - 2026-07-07

### Added
- **Initial companion extension** ([#1111](https://github.com/rust-works/omni-dev/issues/1111)): a thin per-window reporter for the omni-dev daemon's worktrees service, the data producer the service needs to see across the per-window extension-host sandbox.
  - Over the daemon's local `0600` Unix control socket (newline-delimited JSON), it `register`s each window's open folders under a fresh per-activation UUID, `heartbeat`s every ~10s (re-registering when a restarted daemon replies `known: false`), re-registers on workspace-folder changes, and `unregister`s on deactivation.
  - No-ops silently when the daemon is not running — it never surfaces an error or blocks the window.
  - Reports only raw folder paths, leaving branch and ahead/behind enrichment to the daemon; macOS and Linux only.
  - Configurable daemon socket path (`omniDevWorktrees.socketPath`) and heartbeat interval (`omniDevWorktrees.heartbeatSeconds`).
