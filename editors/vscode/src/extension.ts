// The worktrees companion: a thin per-window reporter. On activation it
// registers this window with the omni-dev daemon, heartbeats every ~10s, and
// unregisters on deactivation. When the daemon is not running, every call is a
// silent no-op. See docs/worktrees-service.md for the contract.

import { randomUUID } from "crypto";
import * as path from "path";
import * as vscode from "vscode";
import {
  Envelope,
  RegisterPayload,
  Reply,
  aheadBehindEnvelope,
  closeCheckEnvelope,
  closeEnvelope,
  defaultSocketPath,
  heartbeatEnvelope,
  openEnvelope,
  registerEnvelope,
  sendEnvelope,
  setShowClosedEnvelope,
  treeEnvelope,
  unregisterEnvelope,
} from "./socket";
import { runGh } from "./gh";
import { PullRequest, parsePrList, prBadgeForBranch, prBadgeListArgs } from "./github";
import { openPullRequest } from "./prCommands";
import { CLAUDE_TERMINAL_NAME, resolveClaudeCommand, resolveClaudeCwd } from "./claude";
import {
  AheadBehindMap,
  Node,
  PrBadge,
  TreeGithubIdentity,
  TreeRepoPayload,
  isCurrentWindow,
  nodeId,
  worktreeLabel,
} from "./tree";
import { TreeSubscription } from "./subscription";
import { ITEM_CLICKED_COMMAND, WorktreesTreeDataProvider } from "./treeDataProvider";
import { WorktreeDecorationProvider } from "./decorations";

const CONFIG_SECTION = "omniDevWorktrees";

/** The tree view id, matching the `views` contribution in `package.json`. */
const TREE_VIEW_ID = "omniDevWorktrees.tree";

/**
 * The `when`-clause context key (set via `setContext`) that swaps the title-bar
 * button between its Hide/Show forms. The toggle's **state** is no longer stored
 * per-window in `context.globalState` — that was read-once at activation, had no
 * cross-window change event, and raced a newly-opened window (#1301). It now
 * lives in the daemon and rides every pushed `tree` snapshot's `show_closed`, so
 * this key is driven from that snapshot (see {@link applyShowClosed}). Defaults
 * to `true` — show all worktrees — until the first snapshot lands.
 */
const SHOW_CLOSED_KEY = "omniDevWorktrees.showClosed";

/**
 * How close (ms) two clicks on the same item must be to count as a double-click.
 * The TreeView API has no native double-click event (single click only selects),
 * so `onItemClicked` implements this manually.
 */
const DOUBLE_CLICK_MS = 400;

/** Shown in the empty view while the daemon is unreachable — never an error dialog. */
const DAEMON_DOWN_MESSAGE =
  "omni-dev daemon not running. Start it with `omni-dev daemon start` to list your worktrees.";
/** Shown when the daemon is up but no window is reporting an open worktree. */
const EMPTY_MESSAGE = "No worktrees are open in any VS Code window yet.";

/**
 * The stable per-window identity the daemon keys this window by, generated
 * once per `activate()` (a UUID) — deliberately not `vscode.env.sessionId`,
 * whose per-window uniqueness is unverified.
 */
let windowKey: string;
let heartbeatTimer: ReturnType<typeof setInterval> | undefined;
let output: vscode.OutputChannel | undefined;

// --- Tree-view UI state ------------------------------------------------------
let treeView: vscode.TreeView<Node> | undefined;
let provider: WorktreesTreeDataProvider | undefined;
/** Paints each worktree row's colored PR-check badge (#1324); pulsed on snapshots. */
let decorationProvider: WorktreeDecorationProvider | undefined;
/** The last worktree click, for the manual double-click timer in `onItemClicked`. */
let lastClick: { id: string; at: number } | undefined;

/**
 * The single editor-area terminal opened by the "Open Claude Code" title-bar
 * button (#1322), tracked so a second click focuses it instead of spawning a
 * duplicate. Cleared when the user closes it — VS Code's terminal API is
 * write-only, so an open/closed terminal is all we can observe; we cannot tell
 * whether `claude` itself is still running inside it.
 */
let claudeTerminal: vscode.Terminal | undefined;

function config(): vscode.WorkspaceConfiguration {
  return vscode.workspace.getConfiguration(CONFIG_SECTION);
}

function socketPath(): string {
  const override = config().get<string>("socketPath")?.trim();
  return override ? override : defaultSocketPath();
}

function heartbeatMs(): number {
  const seconds = config().get<number>("heartbeatSeconds") ?? 10;
  return Math.max(1, seconds) * 1000;
}

/** Whether to resolve and show each worktree's GitHub PR badge via `gh` (#1296). */
function showPullRequests(): boolean {
  return config().get<boolean>("showPullRequests") ?? true;
}

/** Snapshots this window's open folders for a `register`. */
function registerPayload(): RegisterPayload {
  const folders = (vscode.workspace.workspaceFolders ?? []).map((f) => f.uri.fsPath);
  const payload: RegisterPayload = { key: windowKey, folders, pid: process.pid };
  if (folders.length > 0) {
    // The daemon enriches the primary folder with live git state; `repo` is
    // just a friendly fallback label.
    payload.repo = path.basename(folders[0]);
  }
  if (vscode.workspace.name) {
    payload.title = vscode.workspace.name;
  }
  return payload;
}

/**
 * Sends one envelope, swallowing every failure — a missing daemon must be a
 * silent no-op, never a user-facing error. Returns the reply, or `undefined`
 * when the daemon was unreachable. `timeoutMs` overrides the default for a
 * long-running op (the `close` execute waits on windows closing).
 */
async function send(envelope: Envelope, timeoutMs?: number): Promise<Reply | undefined> {
  try {
    return await sendEnvelope(socketPath(), envelope, timeoutMs);
  } catch (err) {
    output?.appendLine(
      `${envelope.op} skipped: ${err instanceof Error ? err.message : String(err)}`,
    );
    return undefined;
  }
}

/**
 * Fetches ahead/behind divergence for a batch of worktree paths via the daemon's
 * `ahead-behind` op (#1306) — the lazy replacement for the sync counts the tree
 * snapshot no longer carries. A missing daemon (or older one without the op)
 * resolves to an empty map, so the tree simply renders without sync indicators.
 */
async function fetchAheadBehind(paths: string[]): Promise<AheadBehindMap> {
  const reply = await send(aheadBehindEnvelope(paths));
  const results = reply?.ok ? (reply.payload?.results as AheadBehindMap | undefined) : undefined;
  return results ?? {};
}

/** How long a repo's open-PR list is reused before a fresh `gh` fetch (#1296). */
const PR_CACHE_TTL_MS = 60_000;

interface PrCacheEntry {
  /** When this repo's PRs were fetched (`Date.now()`). */
  at: number;
  /** The repo's open PRs, or `[]` (also cached on a `gh` failure — see below). */
  prs: PullRequest[];
}

/** Per-repo (`owner/name`) cache of the last `gh pr list`, TTL'd by {@link PR_CACHE_TTL_MS}. */
const prCache = new Map<string, PrCacheEntry>();

/**
 * The repo's open PRs, from cache when fresh, else one `gh pr list` (with the
 * checks rollup). A `gh` failure — missing binary, not authed, unknown repo — is
 * **cached as an empty list** for the TTL and logged once, so a missing `gh` is
 * not re-spawned on every pushed snapshot; the explicit "Open Pull Request…"
 * action still surfaces the real error.
 */
async function cachedRepoPrs(repo: TreeGithubIdentity): Promise<PullRequest[]> {
  const key = `${repo.owner}/${repo.name}`;
  const now = Date.now();
  const hit = prCache.get(key);
  if (hit && now - hit.at < PR_CACHE_TTL_MS) {
    return hit.prs;
  }
  try {
    const prs = parsePrList(await runGh(prBadgeListArgs(repo)));
    prCache.set(key, { at: now, prs });
    return prs;
  } catch (err) {
    prCache.set(key, { at: now, prs: [] });
    output?.appendLine(
      `pr badges skipped for ${key}: ${err instanceof Error ? err.message : String(err)}`,
    );
    return [];
  }
}

/**
 * Resolves the open PR badge for each of a GitHub repo's branches on repo-expand
 * (#1296) — the {@link PrBadgeFetcher} injected into the tree provider. One
 * `gh pr list` per repo (TTL-cached), matched to each branch by head. A no-op
 * (empty map, no `gh` call) when the `showPullRequests` setting is off.
 */
async function fetchPrBadges(
  repo: TreeGithubIdentity,
  branches: string[],
): Promise<Record<string, PrBadge>> {
  if (!showPullRequests()) {
    return {};
  }
  const prs = await cachedRepoPrs(repo);
  const badges: Record<string, PrBadge> = {};
  for (const branch of branches) {
    const badge = prBadgeForBranch(prs, branch);
    if (badge) {
      badges[branch] = badge;
    }
  }
  return badges;
}

async function register(): Promise<void> {
  await send(registerEnvelope(registerPayload()));
}

async function heartbeat(): Promise<void> {
  const reply = await send(heartbeatEnvelope(windowKey));
  if (!reply?.ok) {
    return;
  }
  // A cross-window "Close Worktree"/"Close Window" reaches this window as a
  // `close` directive on the heartbeat reply (the daemon can only reply to a
  // window, never call it). It takes priority over re-registration.
  if (reply.payload?.close === true) {
    await vscode.commands.executeCommand("workbench.action.closeWindow");
    return;
  }
  // The registry is in-memory, so a restarted daemon has forgotten this
  // window; `known: false` is our signal to re-register.
  if (reply.payload?.known === false) {
    await register();
  }
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  windowKey = randomUUID();
  output = vscode.window.createOutputChannel("omni-dev");
  context.subscriptions.push(output);

  await register();

  heartbeatTimer = setInterval(() => void heartbeat(), heartbeatMs());
  context.subscriptions.push({
    dispose: () => {
      if (heartbeatTimer) {
        clearInterval(heartbeatTimer);
        heartbeatTimer = undefined;
      }
    },
  });

  // Workspace folders can change without a reactivation; report the new set.
  context.subscriptions.push(
    vscode.workspace.onDidChangeWorkspaceFolders(() => void register()),
  );

  // The window-level "Open Claude Code" title-bar button (#1322) is independent of
  // the tree view below, so it is wired here and works regardless of tree state.
  context.subscriptions.push(
    vscode.commands.registerCommand("omniDevWorktrees.openClaude", () => openClaude()),
    vscode.window.onDidCloseTerminal((terminal) => {
      if (terminal === claudeTerminal) {
        claudeTerminal = undefined;
      }
    }),
  );

  // The reporter above runs regardless; the tree view is the new UI layer.
  setupTreeView(context);
}

/**
 * Stands up the repo/worktree tree view: the data provider, the live push
 * subscription that feeds it, and the refresh/open/click commands. All of it is
 * pushed onto `context.subscriptions` so it tears down cleanly on deactivate.
 */
function setupTreeView(context: vscode.ExtensionContext): void {
  // `windowKey` is assigned in `activate()` before this runs, so the provider can
  // mark this window's own worktree distinctly from those open in other windows.
  // The provider fetches ahead/behind (#1306) and PR badges (#1296) lazily on
  // expand via the injected `fetchAheadBehind` / `fetchPrBadges`.
  const treeProvider = new WorktreesTreeDataProvider(windowKey, fetchAheadBehind, fetchPrBadges);
  provider = treeProvider;

  // Seed the button/filter to the default (show all) before the first render;
  // the daemon's pushed `show_closed` is authoritative and updates both the
  // moment the first snapshot lands (#1301) — no per-window `globalState`.
  applyShowClosed(true);

  const view = vscode.window.createTreeView<Node>(TREE_VIEW_ID, {
    treeDataProvider: treeProvider,
    showCollapseAll: true,
  });
  treeView = view;
  // Start with the daemon-down hint; the first snapshot clears it.
  view.message = DAEMON_DOWN_MESSAGE;
  context.subscriptions.push(view, treeProvider);

  // The colored PR-check badge (#1324): a file-decoration provider paints each
  // worktree row off the custom-scheme `resourceUri` the tree items carry. It is
  // `refresh()`ed on every snapshot so colours track the lazily-fetched PR state.
  const decorations = new WorktreeDecorationProvider();
  decorationProvider = decorations;
  context.subscriptions.push(
    decorations,
    vscode.window.registerFileDecorationProvider(decorations),
  );

  const sub = new TreeSubscription(socketPath(), {
    onSnapshot: (snapshot) => {
      view.message = snapshot.repos.length === 0 ? EMPTY_MESSAGE : undefined;
      treeProvider.update(snapshot.repos);
      // The daemon-backed toggle rides every snapshot, so a flip in any window
      // re-renders this one and a fresh window initializes on its first frame.
      applyShowClosed(snapshot.show_closed);
      // A new snapshot re-runs the lazy PR-badge fetch, so re-evaluate every row's
      // check colour (state-keyed URIs already re-decorate; this covers the rest).
      decorations.refresh();
    },
    onStatus: (connected) => {
      // A drop re-shows the hint; a (re)connect's message is set by the snapshot.
      if (!connected) {
        view.message = DAEMON_DOWN_MESSAGE;
      }
    },
    onError: (message) => output?.appendLine(`subscription: ${message}`),
  });
  sub.start();
  context.subscriptions.push({ dispose: () => sub.close() });

  context.subscriptions.push(
    vscode.commands.registerCommand("omniDevWorktrees.refresh", () => void refreshTree()),
    vscode.commands.registerCommand(ITEM_CLICKED_COMMAND, (node?: Node) => onItemClicked(node)),
    vscode.commands.registerCommand(
      "omniDevWorktrees.closeWorktree",
      (node?: Node) => void closeWorktree(node),
    ),
    vscode.commands.registerCommand(
      "omniDevWorktrees.closeWindow",
      (node?: Node) => void closeWindow(node),
    ),
    vscode.commands.registerCommand(
      "omniDevWorktrees.openPullRequest",
      (node?: Node) => void openPullRequest(node),
    ),
    // The two halves of the one title-bar toggle: exactly one is contributed at a
    // time (gated on the context key), so clicking the visible button flips the
    // state to the other.
    vscode.commands.registerCommand(
      "omniDevWorktrees.hideClosedWorktrees",
      () => void setShowClosed(false),
    ),
    vscode.commands.registerCommand(
      "omniDevWorktrees.showClosedWorktrees",
      () => void setShowClosed(true),
    ),
  );
}

/**
 * Applies an authoritative show/hide-closed value — from a daemon snapshot or
 * the pre-snapshot default — to this window's UI: flips the `when`-clause
 * context key so the title-bar button shows the right form, and re-filters the
 * tree. It never persists or sends anything; the daemon owns the state (#1301).
 * A `show_closed` omitted by an older daemon degrades to `true` (show all).
 */
function applyShowClosed(showClosed = true): void {
  void vscode.commands.executeCommand("setContext", SHOW_CLOSED_KEY, showClosed);
  provider?.setShowClosed(showClosed);
}

/**
 * Flips the show/hide-closed toggle by sending the daemon `set-show-closed` op.
 * The daemon holds the single cross-window value and pushes a fresh `tree`
 * snapshot (carrying the new `show_closed`) to **every** window — including this
 * one, whose `onSnapshot` then drives the button and the tree via
 * {@link applyShowClosed}. So the UI reconciles from the snapshot, not from a
 * per-window write, giving live cross-window sync `context.globalState` could
 * not (#1301). A missing daemon is a silent no-op (the shared `send` logs it),
 * like the rest of the reporter.
 */
async function setShowClosed(showClosed: boolean): Promise<void> {
  await send(setShowClosedEnvelope(showClosed));
}

/**
 * The manual double-click handler. Every worktree item fires this on a single
 * click (the TreeView API has no double-click event); a second click on the
 * same item within {@link DOUBLE_CLICK_MS} opens it, otherwise the click is just
 * recorded and VS Code's native selection stands.
 */
function onItemClicked(node?: Node): void {
  if (!node || node.kind !== "worktree") {
    lastClick = undefined;
    return;
  }
  const id = nodeId(node);
  const now = Date.now();
  if (lastClick && lastClick.id === id && now - lastClick.at <= DOUBLE_CLICK_MS) {
    lastClick = undefined;
    void openNode(node);
    return;
  }
  lastClick = { id, at: now };
}

/**
 * Focuses (or opens) a worktree's window via the daemon `open` op. A missing
 * daemon is a silent no-op (like the reporter); a genuine rejection — the daemon
 * guards `path` to an absolute existing directory — is surfaced.
 */
async function openNode(node?: Node): Promise<void> {
  if (!node || node.kind !== "worktree") {
    return;
  }
  const reply = await send(openEnvelope(node.wt.path));
  if (reply && !reply.ok) {
    void vscode.window.showErrorMessage(
      `omni-dev: could not open worktree — ${reply.error ?? "unknown error"}`,
    );
  }
}

/**
 * The "Open Claude Code" title-bar button (#1322). Opens — or, if one is already
 * open, focuses — a terminal docked as an **editor tab** (not the bottom panel)
 * running the Claude Code CLI. The cwd is the active window's workspace folder
 * (falling back to the first folder); the launch command is
 * `omniDevWorktrees.claudeCommand` (default `claude`). This is window-level and
 * daemon-independent — a plain `createTerminal`, no socket involved.
 */
function openClaude(): void {
  if (claudeTerminal) {
    claudeTerminal.show();
    return;
  }
  const folders = (vscode.workspace.workspaceFolders ?? []).map((f) => f.uri.fsPath);
  const activeUri = vscode.window.activeTextEditor?.document.uri;
  const activeFolder =
    activeUri && activeUri.scheme === "file"
      ? vscode.workspace.getWorkspaceFolder(activeUri)?.uri.fsPath
      : undefined;
  const cwd = resolveClaudeCwd(folders, activeFolder);
  const command = resolveClaudeCommand(config().get<string>("claudeCommand"));

  const terminal = vscode.window.createTerminal({
    name: CLAUDE_TERMINAL_NAME,
    cwd,
    location: vscode.TerminalLocation.Editor,
    iconPath: new vscode.ThemeIcon("sparkle"),
  });
  claudeTerminal = terminal;
  terminal.show();
  terminal.sendText(command, true);
}

/**
 * Generous timeout for a `close` execute call: the daemon may wait ~20s for a
 * cross-window target to pick up the directive on its next heartbeat and close.
 */
const CLOSE_EXECUTE_TIMEOUT_MS = 30_000;

/** One entry of the daemon's phase-1 safety report. */
interface CloseNote {
  kind: string;
  detail: string;
}

/** The daemon's phase-1 `close` safety report (mirrors `SafetyReport` in Rust). */
interface CloseSafetyReport {
  removable: boolean;
  is_main: boolean;
  open: boolean;
  window_key?: string;
  window_folder_count: number;
  risks: CloseNote[];
  info: CloseNote[];
}

/** Shown when an explicit close action can't reach the daemon (unlike heartbeat, not silent). */
function daemonDownError(): void {
  void vscode.window.showErrorMessage(
    "omni-dev daemon not running. Start it with `omni-dev daemon start`.",
  );
}

/**
 * Closes a **linked** worktree: a phase-1 safety check, a modal confirm only
 * when something would be lost (or a multi-root window would close), then a
 * phase-2 execute that closes the owning window and deletes the worktree. If
 * this window is the one open on it, it closes itself once removal succeeds.
 */
async function closeWorktree(node?: Node): Promise<void> {
  if (!node || node.kind !== "worktree") {
    return;
  }
  const wt = node.wt;
  await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: `Closing worktree “${worktreeLabel(wt)}”…`,
    },
    async () => {
      // Phase 1: what would removal lose?
      const check = await send(closeCheckEnvelope(wt.path, windowKey));
      if (!check) {
        daemonDownError();
        return;
      }
      if (!check.ok) {
        void vscode.window.showErrorMessage(
          `omni-dev: could not check worktree — ${check.error ?? "unknown error"}`,
        );
        return;
      }
      const report = check.payload as CloseSafetyReport;
      // Defensive: the daemon refuses to delete a main working tree; the UI
      // should never route one here, but never delete if it somehow does.
      if (report.is_main || !report.removable) {
        void vscode.window.showErrorMessage(
          "omni-dev: this is the repository's main working tree and is never deleted. Use Close Window.",
        );
        return;
      }

      // Confirm only when there is something to warn about: data at risk, or a
      // multi-root window whose other folders would also close.
      const warnings = (report.risks ?? []).map((r) => r.detail);
      if (report.window_folder_count > 1) {
        warnings.push(
          `This window has ${report.window_folder_count} folders open; all will close.`,
        );
      }
      if (warnings.length > 0) {
        const choice = await vscode.window.showWarningMessage(
          `Delete worktree “${worktreeLabel(wt)}”? This cannot be undone.`,
          { modal: true, detail: warnings.map((w) => `• ${w}`).join("\n") },
          "Delete Worktree",
        );
        if (choice !== "Delete Worktree") {
          return;
        }
      }

      // Phase 2: execute (a long wait if a cross-window target must close first).
      const exec = await send(
        closeEnvelope(wt.path, { remove: true, requesterKey: windowKey, confirmed: true }),
        CLOSE_EXECUTE_TIMEOUT_MS,
      );
      if (!exec) {
        daemonDownError();
        return;
      }
      if (!exec.ok) {
        void vscode.window.showErrorMessage(
          `omni-dev: could not close worktree — ${exec.error ?? "unknown error"}`,
        );
        return;
      }
      // Self-close: if *this* window has the worktree open, close it now that
      // the removal has succeeded (the daemon replied first to dodge the
      // ext-host-dies-mid-op race).
      if (isCurrentWindow(wt, windowKey)) {
        await vscode.commands.executeCommand("workbench.action.closeWindow");
      }
    },
  );
}

/**
 * Closes the **window** a main working tree is open in, without ever deleting
 * the repository. If it is *this* window, close it directly; otherwise ask the
 * daemon to signal the owning window (it closes on the directive's heartbeat).
 */
async function closeWindow(node?: Node): Promise<void> {
  if (!node || node.kind !== "worktree") {
    return;
  }
  const wt = node.wt;
  if (isCurrentWindow(wt, windowKey)) {
    await vscode.commands.executeCommand("workbench.action.closeWindow");
    return;
  }
  const reply = await send(
    closeEnvelope(wt.path, { remove: false, requesterKey: windowKey, confirmed: true }),
    CLOSE_EXECUTE_TIMEOUT_MS,
  );
  if (!reply) {
    daemonDownError();
    return;
  }
  if (!reply.ok) {
    void vscode.window.showErrorMessage(
      `omni-dev: could not close window — ${reply.error ?? "unknown error"}`,
    );
  }
}

/**
 * The manual refresh command: a one-shot `tree` fetch, a fallback for when the
 * subscription is momentarily down. The live view normally updates itself.
 */
async function refreshTree(): Promise<void> {
  const reply = await send(treeEnvelope());
  if (reply?.ok && Array.isArray(reply.payload?.repos)) {
    const repos = reply.payload.repos as TreeRepoPayload[];
    provider?.update(repos);
    // The one-shot `tree` reply carries `show_closed` too, so a manual refresh
    // (subscription momentarily down) keeps the toggle applied (#1301).
    applyShowClosed(reply.payload.show_closed);
    // Re-evaluate the PR-check colours for the freshly-fetched rows (#1324).
    decorationProvider?.refresh();
    if (treeView) {
      treeView.message = repos.length === 0 ? EMPTY_MESSAGE : undefined;
    }
  }
}

export async function deactivate(): Promise<void> {
  if (heartbeatTimer) {
    clearInterval(heartbeatTimer);
    heartbeatTimer = undefined;
  }
  // The tree view, provider, decoration provider, and subscription are torn down
  // via `context.subscriptions`; drop our references so a reactivation starts fresh.
  provider = undefined;
  decorationProvider = undefined;
  treeView = undefined;
  lastClick = undefined;
  claudeTerminal = undefined;
  await send(unregisterEnvelope(windowKey));
}
