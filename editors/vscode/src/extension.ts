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
  sessionWindowEnvelope,
  sessionWindowUnregisterEnvelope,
  setPollingEnvelope,
  setPrSourceEnvelope,
  setShowClosedEnvelope,
  treeEnvelope,
  unregisterEnvelope,
} from "./socket";
import { countClaudeTabs, countClaudeTerminals } from "./claudeEmbeddings";
import { openPullRequest, openPullRequestInBrowser } from "./prCommands";
import { nextClaudeTerminalName, resolveClaudeCommand, resolveClaudeCwd } from "./claude";
import { moveClaudeSessionHere } from "./moveSessionCommand";
import {
  AheadBehindMap,
  Node,
  TreeRepoPayload,
  WorktreeNode,
  isCurrentWindow,
  nodeDirectories,
  nodeId,
  partitionByRole,
  partitionByWindow,
  partitionSelfLast,
  repoLabel,
  selectionTargets,
  withoutPrBadges,
  worktreeLabel,
  worktreeTargets,
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
 * The `when`-clause context key mirroring the global `showPullRequests` setting,
 * so the per-repo "Enable/Disable PR Polling" menu items are hidden while PR
 * badges are globally off — the master switch (#1376). Set from the setting on
 * activation and on every configuration change (see {@link applyShowPullRequests}).
 */
const SHOW_PR_KEY = "omniDevWorktrees.showPullRequests";

/**
 * The `when`-clause context key mirroring the daemon's active PR-status source
 * (#1384). Driven from every pushed `tree` snapshot's `pr_source` (see
 * {@link applyPrSource}), so the poll-volume UI — the #1376 Enable/Disable PR
 * Polling items — is hidden in `webhook` mode, where there is no poll volume to
 * manage. Defaults to `"poll"` until the first snapshot lands.
 */
const PR_SOURCE_KEY = "omniDevWorktrees.prStatusSource";

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

/** Whether to show each worktree's GitHub PR badge (#1296). */
function showPullRequests(): boolean {
  return config().get<boolean>("showPullRequests") ?? true;
}

/**
 * The live PR-status source this user selects (#1384): `"poll"` (default) or
 * `"webhook"`. Forwarded to the daemon (the authority) via `set-pr-source`; the
 * daemon then echoes the active mode on every snapshot's `pr_source`.
 */
function prStatusSource(): "poll" | "webhook" {
  return config().get<string>("prStatusSource") === "webhook" ? "webhook" : "poll";
}

/**
 * The repos to render, with daemon-supplied PR badges stripped when the
 * `showPullRequests` setting is off (#1337). Since the daemon pushes badges on the
 * snapshot, not gating here would leave the setting with nothing to switch off.
 */
function visibleRepos(repos: TreeRepoPayload[]): TreeRepoPayload[] {
  return showPullRequests() ? repos : withoutPrBadges(repos);
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

// PR badges are resolved **only** by the daemon (#1337/#1384): one shared
// `gh api graphql` (or the webhook buffer) covers every repo/branch for every
// window, pushed on the tree snapshot as `pr`/`pr_none`. The extension no longer
// runs its own `gh pr list` fallback — a per-window duplicate that isn't shared
// and burst on every daemon (re)start. A branch the daemon has not resolved yet
// simply shows no badge until the daemon resolves it and pushes an update; the
// window waits for the daemon rather than self-serving.

async function register(): Promise<void> {
  await send(registerEnvelope(registerPayload()));
}

/**
 * Counts this window's embedded Claude Code sessions: editor webview tabs (their
 * mangled `viewType` contains the Claude marker) and integrated terminals (named
 * like a Claude terminal, honouring `$CLAUDE_CODE_TERMINAL_TITLE`). The
 * extension host is sandboxed per window, so this only ever sees *this* window's
 * tabs/terminals — which is exactly the per-window fact the daemon aggregates.
 */
function claudeEmbeddings(): { tabs: number; terminals: number } {
  const viewTypes: string[] = [];
  for (const group of vscode.window.tabGroups.all) {
    for (const tab of group.tabs) {
      const input = tab.input;
      if (input instanceof vscode.TabInputWebview) {
        viewTypes.push(input.viewType);
      }
    }
  }
  const terminalNames = vscode.window.terminals.map((t) => t.name);
  return {
    tabs: countClaudeTabs(viewTypes),
    terminals: countClaudeTerminals(terminalNames, process.env.CLAUDE_CODE_TERMINAL_TITLE),
  };
}

/**
 * Reports this window's Claude embeddings to the daemon's sessions service so it
 * can tag a session's source as VS Code (by joining a session `cwd` against this
 * window's folders). Refreshes the report's liveness on the same cadence as the
 * worktrees heartbeat; a missing daemon is a silent no-op like everything else.
 */
async function reportSessionWindow(): Promise<void> {
  const folders = (vscode.workspace.workspaceFolders ?? []).map((f) => f.uri.fsPath);
  const { tabs, terminals } = claudeEmbeddings();
  await send(sessionWindowEnvelope({ key: windowKey, folders, tabs, terminals }));
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
  await reportSessionWindow();

  // One tick refreshes both the worktrees window registration and the sessions
  // Claude-embedding report, so a single ~10s cadence keeps both live.
  heartbeatTimer = setInterval(() => {
    void heartbeat();
    void reportSessionWindow();
  }, heartbeatMs());
  context.subscriptions.push({
    dispose: () => {
      if (heartbeatTimer) {
        clearInterval(heartbeatTimer);
        heartbeatTimer = undefined;
      }
    },
  });

  // Workspace folders can change without a reactivation; report the new set to
  // both services.
  context.subscriptions.push(
    vscode.workspace.onDidChangeWorkspaceFolders(() => {
      void register();
      void reportSessionWindow();
    }),
  );

  // A Claude tab or terminal opening/closing changes this window's embedding
  // count; push a fresh report immediately rather than waiting for the next tick.
  context.subscriptions.push(
    vscode.window.tabGroups.onDidChangeTabGroups(() => void reportSessionWindow()),
    vscode.window.onDidOpenTerminal(() => void reportSessionWindow()),
    vscode.window.onDidCloseTerminal(() => void reportSessionWindow()),
  );

  // The window-level "Open Claude Code" title-bar button (#1322) is independent of
  // the tree view below, so it is wired here and works regardless of tree state.
  context.subscriptions.push(
    vscode.commands.registerCommand("omniDevWorktrees.openClaude", () => openClaude()),
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
  // The provider fetches ahead/behind (#1306) lazily on expand via the injected
  // `fetchAheadBehind`; PR badges come entirely from the daemon snapshot.
  const treeProvider = new WorktreesTreeDataProvider(windowKey, fetchAheadBehind);
  provider = treeProvider;

  // Seed the button/filter to the default (show all) before the first render;
  // the daemon's pushed `show_closed` is authoritative and updates both the
  // moment the first snapshot lands (#1301) — no per-window `globalState`.
  applyShowClosed(true);
  // Seed the PR-master context key + icon-colour flag from the current setting
  // (#1376), so the per-repo toggle menu and green icons reflect it from frame one.
  applyShowPullRequests();
  // Seed the PR-source context key from the setting so the poll-volume UI is
  // gated from frame one, and forward the setting to the daemon (the authority),
  // which echoes the active mode back on every snapshot (#1384).
  applyPrSource(prStatusSource());
  void setPrSource();

  // `canSelectMany` makes every item command plural: VS Code then invokes them as
  // `(clicked, selected[])`, and each handler resolves its targets through
  // `selectionTargets` and re-validates them (the `when` clause only ever saw the
  // *clicked* row, so a mixed selection can reach any handler).
  const view = vscode.window.createTreeView<Node>(TREE_VIEW_ID, {
    treeDataProvider: treeProvider,
    showCollapseAll: true,
    canSelectMany: true,
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
      treeProvider.update(visibleRepos(snapshot.repos));
      // The daemon-backed toggle rides every snapshot, so a flip in any window
      // re-renders this one and a fresh window initializes on its first frame.
      applyShowClosed(snapshot.show_closed);
      // The active PR-source mode rides along too (#1384), so the poll-volume UI
      // hides/shows in lock-step across every window.
      applyPrSource(snapshot.pr_source);
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
    // Fires from `TreeItem.command`, which passes only its own declared
    // `arguments` — never the `(clicked, selected[])` pair a `view/item/context`
    // command gets — so this one stays single-node.
    vscode.commands.registerCommand(ITEM_CLICKED_COMMAND, (node?: Node) => onItemClicked(node)),
    vscode.commands.registerCommand(
      "omniDevWorktrees.openWorktree",
      (node?: Node, selected?: Node[]) => void openWorktrees(node, selected),
    ),
    vscode.commands.registerCommand(
      "omniDevWorktrees.closeWorktree",
      (node?: Node, selected?: Node[]) => void closeWorktree(node, selected),
    ),
    vscode.commands.registerCommand(
      "omniDevWorktrees.closeWindow",
      (node?: Node, selected?: Node[]) => void closeWindow(node, selected),
    ),
    vscode.commands.registerCommand(
      "omniDevWorktrees.copyDirectory",
      (node?: Node, selected?: Node[]) => {
        const dirs = nodeDirectories(selectionTargets(node, selected));
        if (!dirs.length) {
          return;
        }
        void vscode.env.clipboard.writeText(dirs.join("\n"));
        vscode.window.setStatusBarMessage(
          dirs.length === 1 ? `Copied ${dirs[0]}` : `Copied ${dirs.length} directories`,
          3000,
        );
      },
    ),
    vscode.commands.registerCommand(
      "omniDevWorktrees.openPullRequest",
      (node?: Node, selected?: Node[]) => void openPullRequest(node, selected),
    ),
    vscode.commands.registerCommand(
      "omniDevWorktrees.openPullRequestInBrowser",
      (node?: Node, selected?: Node[]) => void openPullRequestInBrowser(node, selected),
    ),
    // A destination, not a subject — the menu hides it while a multi-selection is
    // active (`!listMultiSelection`), so it stays single-node here too.
    vscode.commands.registerCommand(
      "omniDevWorktrees.moveClaudeSessionHere",
      (node?: Node) => void moveClaudeSessionHere(node, output),
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
    // The per-repo PR-poll toggle (#1376): each acts on the GitHub repo node(s)
    // right-clicked, sending the daemon a `set-polling` op. The daemon holds the
    // (persisted) state and re-pushes a snapshot, so the icon/badges reconcile
    // from the snapshot — the `set-show-closed` pattern, no local write.
    vscode.commands.registerCommand(
      "omniDevWorktrees.enablePolling",
      (node?: Node, selected?: Node[]) => void setPolling(node, selected, true),
    ),
    vscode.commands.registerCommand(
      "omniDevWorktrees.disablePolling",
      (node?: Node, selected?: Node[]) => void setPolling(node, selected, false),
    ),
    // Keep the PR-master context key + icon colour in sync when the user flips
    // `showPullRequests`, so the toggle menu and green icons respond immediately
    // rather than at the next ~10s snapshot.
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration(`${CONFIG_SECTION}.showPullRequests`)) {
        applyShowPullRequests();
        void refreshTree();
      }
      // Selecting a new PR source (#1384) forwards it to the daemon; the pushed
      // snapshot's `pr_source` then drives the context key everywhere. The local
      // seed keeps this window's poll-volume UI in step without waiting a tick.
      if (e.affectsConfiguration(`${CONFIG_SECTION}.prStatusSource`)) {
        applyPrSource(prStatusSource());
        void setPrSource();
      }
    }),
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
 * Applies the global `showPullRequests` setting to this window's UI (#1376):
 * flips the `when`-clause context key that hides the per-repo "Enable/Disable PR
 * Polling" menu while PR badges are globally off (the master switch), and tells
 * the provider to grey repo icons accordingly. Pure local UI — the per-repo poll
 * state lives in the daemon.
 */
function applyShowPullRequests(): void {
  const on = showPullRequests();
  void vscode.commands.executeCommand("setContext", SHOW_PR_KEY, on);
  provider?.setShowPullRequests(on);
}

/**
 * Applies an authoritative PR-source mode — from a daemon snapshot or the
 * pre-snapshot default — to this window's UI (#1384): flips the `when`-clause
 * context key that hides the poll-volume UI (the #1376 Enable/Disable PR Polling
 * items) while the source is `webhook`. Never persists or sends; the daemon owns
 * the state. A `pr_source` omitted by an older daemon degrades to `"poll"`.
 */
function applyPrSource(source: "poll" | "webhook" = "poll"): void {
  void vscode.commands.executeCommand("setContext", PR_SOURCE_KEY, source);
  provider?.setPrSource(source);
}

/**
 * Forwards this window's `omniDevWorktrees.prStatusSource` setting to the daemon
 * via `set-pr-source` (#1384). The daemon is the authority and pushes a fresh
 * `tree` snapshot carrying the new `pr_source` to every window, whose `onSnapshot`
 * then drives {@link applyPrSource} — the `set-show-closed` reconcile pattern.
 * Sent on activation and on every configuration change; a missing daemon is a
 * silent no-op (the shared `send` logs it).
 */
async function setPrSource(): Promise<void> {
  await send(setPrSourceEnvelope(prStatusSource()));
}

/**
 * Enables or disables PR polling for the GitHub repo node(s) among the command
 * targets (#1376), sending the daemon one `set-polling` op per distinct repo. It
 * covers every worktree of the repo — the daemon keys by `owner/name`. The daemon
 * holds the (persisted) state and re-pushes a snapshot, so the icon recolours and
 * badges drop/appear from that snapshot; this handler writes nothing locally. A
 * missing daemon is a silent no-op (the shared `send` logs it).
 */
async function setPolling(
  clicked: Node | undefined,
  selected: Node[] | undefined,
  enabled: boolean,
): Promise<void> {
  const seen = new Set<string>();
  const labels: string[] = [];
  for (const node of selectionTargets(clicked, selected)) {
    if (node.kind !== "repo" || !node.repo.github) {
      continue;
    }
    const gh = node.repo.github;
    const key = `${gh.owner}/${gh.name}`;
    if (seen.has(key)) {
      continue;
    }
    seen.add(key);
    await send(setPollingEnvelope(gh, enabled));
    labels.push(repoLabel(node.repo));
  }
  if (labels.length > 0) {
    const what = labels.length === 1 ? labels[0] : `${labels.length} repositories`;
    // Enabling is a time-boxed lease (#1376): the daemon auto-disables the repo
    // after ~15 minutes, so say so rather than implying it stays on forever.
    const message = enabled
      ? `Enabled PR polling for ${what} — auto-disables after 15 min`
      : `Disabled PR polling for ${what}`;
    vscode.window.setStatusBarMessage(message, 4000);
  }
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
 * The **Open Worktree** command: opens (or focuses) a window for every selected
 * worktree — the multi-select answer to "restore my working set", which
 * double-click, being inherently single, cannot express. Repo nodes in the
 * selection are ignored.
 */
async function openWorktrees(clicked?: Node, selected?: Node[]): Promise<void> {
  const targets = worktreeTargets(selectionTargets(clicked, selected));
  if (targets.length === 0) {
    return;
  }
  if (targets.length === 1) {
    await openNode(targets[0]);
    return;
  }
  await runBatch(targets, `Opening ${targets.length} worktrees…`, async (target) => {
    const reply = await send(openEnvelope(target.wt.path));
    if (reply && !reply.ok) {
      throw new Error(reply.error ?? "unknown error");
    }
  });
}

/** One target's failure, collected so a batch reports once rather than N times. */
interface BatchFailure {
  label: string;
  message: string;
}

/**
 * The daemon is unreachable. Distinct from a per-target failure because it is
 * never per-target: one socket serves every target, so a batch aborts rather than
 * failing N times over, and the user gets the actionable start-it message once.
 */
class DaemonDownError extends Error {
  constructor() {
    super("daemon not running");
  }
}

/**
 * Reports a batch's failures in **one** message rather than N toasts. A batch of
 * one is not a batch — it gets the bare message, which is what these commands have
 * always shown for a single target.
 */
function reportFailures(failures: BatchFailure[], total: number): void {
  if (failures.length === 0) {
    return;
  }
  if (total === 1) {
    void vscode.window.showErrorMessage(`omni-dev: ${failures[0].message}`);
    return;
  }
  void vscode.window.showErrorMessage(
    `omni-dev: ${failures.length} of ${total} failed — ${failures
      .map((f) => `${f.label}: ${f.message}`)
      .join("; ")}`,
  );
}

/**
 * Runs `action` over the targets **concurrently**, reporting completions into an
 * existing progress and continuing past a failure, which it collects rather than
 * throws.
 *
 * Fanning out is the point (#1359): the daemon's per-target cost in a close is
 * almost entirely a *wait* on the target window's next heartbeat (~10s), and waits
 * on N **independent** windows are exactly the thing that overlaps — marked
 * together they all fire within one shared interval rather than N stacked ones.
 * The transport carries it: `sendEnvelope` opens a connection per request and the
 * daemon spawns a task per connection. The one genuinely shared resource, `git2`'s
 * prune against a repo's `.git/worktrees`, is serialized daemon-side rather than
 * here, so safety does not depend on every caller staying sequential.
 *
 * This window's own worktree is the exception, and runs alone after the rest: see
 * {@link partitionSelfLast}.
 */
async function runConcurrent(
  targets: WorktreeNode[],
  progress: vscode.Progress<{ message?: string }>,
  action: (target: WorktreeNode) => Promise<void>,
): Promise<BatchFailure[]> {
  const { others, self } = partitionSelfLast(targets, windowKey);
  const failures: BatchFailure[] = [];
  let done = 0;
  let daemonDown = false;

  const run = async (target: WorktreeNode) => {
    try {
      await action(target);
    } catch (err) {
      if (err instanceof DaemonDownError) {
        daemonDown = true;
      } else {
        failures.push({
          label: worktreeLabel(target.wt),
          message: err instanceof Error ? err.message : String(err),
        });
      }
    }
    done += 1;
    // Completions, not a current target: a fan-out has no single "current" one.
    progress.report({ message: `${done}/${targets.length}` });
  };

  await Promise.all(others.map(run));
  // One socket serves every target, so a down daemon fails them all identically.
  // Report it once and skip the self-close, exactly as the sequential abort this
  // replaces did — rather than listing N copies of the same message.
  if (daemonDown) {
    daemonDownError();
    return failures;
  }
  for (const target of self) {
    await run(target);
  }
  if (daemonDown) {
    daemonDownError();
  }
  return failures;
}

/** {@link runConcurrent} plus its own progress notification and failure summary. */
async function runBatch(
  targets: WorktreeNode[],
  title: string,
  action: (target: WorktreeNode) => Promise<void>,
): Promise<void> {
  const failures = await vscode.window.withProgress(
    { location: vscode.ProgressLocation.Notification, title },
    (progress) => runConcurrent(targets, progress, action),
  );
  reportFailures(failures, targets.length);
}

/** One target, with the same error reporting a batch of one would give it. */
async function runOne(
  target: WorktreeNode,
  action: (target: WorktreeNode) => Promise<void>,
): Promise<void> {
  try {
    await action(target);
  } catch (err) {
    if (err instanceof DaemonDownError) {
      daemonDownError();
      return;
    }
    reportFailures(
      [
        {
          label: worktreeLabel(target.wt),
          message: err instanceof Error ? err.message : String(err),
        },
      ],
      1,
    );
  }
}

/**
 * The "Open Claude Code" title-bar button (#1322, #1347). On **every** click opens
 * a new terminal docked as an **editor tab** (not the bottom panel) running the
 * Claude Code CLI — concurrent sessions in one window are a normal way to work, so
 * the button never caps at one or focuses an existing tab. Each terminal gets a
 * distinguishable name (`Claude Code`, `Claude Code 2`, …) via
 * {@link nextClaudeTerminalName}. The cwd is the active window's workspace folder
 * (falling back to the first folder); the launch command is
 * `omniDevWorktrees.claudeCommand` (default `claude`). This is window-level and
 * daemon-independent — a plain `createTerminal`, no socket involved.
 */
function openClaude(): void {
  const folders = (vscode.workspace.workspaceFolders ?? []).map((f) => f.uri.fsPath);
  const activeUri = vscode.window.activeTextEditor?.document.uri;
  const activeFolder =
    activeUri && activeUri.scheme === "file"
      ? vscode.workspace.getWorkspaceFolder(activeUri)?.uri.fsPath
      : undefined;
  const cwd = resolveClaudeCwd(folders, activeFolder);
  const command = resolveClaudeCommand(config().get<string>("claudeCommand"));
  const name = nextClaudeTerminalName(vscode.window.terminals.map((t) => t.name));

  const terminal = vscode.window.createTerminal({
    name,
    cwd,
    location: vscode.TerminalLocation.Editor,
    iconPath: new vscode.ThemeIcon("sparkle"),
  });
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

/** A phase-1 safety check's result for one target. */
type CheckOutcome =
  | { kind: "ok"; target: WorktreeNode; report: CloseSafetyReport }
  | { kind: "error"; target: WorktreeNode; message: string; daemonDown?: true };

/** Runs the side-effect-free phase-1 `close-check` for one target (ADR-0049). */
async function closeCheck(target: WorktreeNode): Promise<CheckOutcome> {
  const reply = await send(closeCheckEnvelope(target.wt.path, windowKey));
  if (!reply) {
    return { kind: "error", target, message: "daemon not running", daemonDown: true };
  }
  if (!reply.ok) {
    return {
      kind: "error",
      target,
      message: `could not check worktree — ${reply.error ?? "unknown error"}`,
    };
  }
  return { kind: "ok", target, report: reply.payload as CloseSafetyReport };
}

/** What a removal would cost: the daemon's risks, plus a multi-root window note. */
function closeWarnings(report: CloseSafetyReport): string[] {
  const warnings = (report.risks ?? []).map((r) => r.detail);
  if (report.window_folder_count > 1) {
    warnings.push(`This window has ${report.window_folder_count} folders open; all will close.`);
  }
  return warnings;
}

/**
 * The delete confirmation.
 *
 * A **single** target confirms only when something would actually be lost — data
 * at risk, or a multi-root window whose other folders would also close. A
 * **batch** always confirms and lists exactly what dies: a mis-aimed multi-select
 * is far easier to make than a mis-aimed right-click, and the modal is the only
 * place the user sees the full set. Main working trees carried in by a mixed
 * selection are named as skipped rather than silently downgraded to a window
 * close — quietly turning a requested delete into something else is worse than
 * refusing it.
 */
async function confirmDelete(
  deletable: { target: WorktreeNode; report: CloseSafetyReport }[],
  skippedMain: WorktreeNode[],
  selectedCount: number,
): Promise<boolean> {
  // Batch-ness is a property of the *gesture*, not of what survived phase 1: a
  // two-row selection whose first check failed is still a batch, and must still
  // confirm. Keying this off `deletable.length` would let a partly-failed batch
  // delete silently.
  const single = selectedCount === 1 && skippedMain.length === 0;
  const warnings = deletable.flatMap(({ report }) => closeWarnings(report));
  if (single && warnings.length === 0) {
    return true;
  }

  const confirmLabel = deletable.length === 1 ? "Delete Worktree" : "Delete Worktrees";
  const detail = single
    ? warnings.map((w) => `• ${w}`).join("\n")
    : [
        ...deletable.map(({ target, report }) => {
          const warns = closeWarnings(report);
          const label = worktreeLabel(target.wt);
          return warns.length > 0 ? `• ${label} — ${warns.join("; ")}` : `• ${label}`;
        }),
        ...(skippedMain.length > 0
          ? [
              "",
              `${skippedMain.length} main working ${
                skippedMain.length === 1 ? "tree" : "trees"
              } will be skipped (never deleted): ${skippedMain
                .map((n) => worktreeLabel(n.wt))
                .join(", ")}`,
            ]
          : []),
      ].join("\n");

  const choice = await vscode.window.showWarningMessage(
    deletable.length === 1
      ? `Delete worktree “${worktreeLabel(deletable[0].target.wt)}”? This cannot be undone.`
      : `Delete ${deletable.length} worktrees? This cannot be undone.`,
    { modal: true, detail },
    confirmLabel,
  );
  return choice === confirmLabel;
}

/**
 * The **Close Worktree** command: deletes every selected **linked** worktree and
 * closes the window each is open in. Phase-1 safety checks run in parallel (they
 * are side-effect-free by design — ADR-0049), aggregate into one confirmation,
 * then phase 2 fans out too, so N closes share one heartbeat wait instead of
 * stacking N of them (#1359). A selection of one behaves exactly as it always has,
 * down to the messages.
 *
 * Repo nodes and main working trees are filtered out here rather than trusted to
 * the menu: `when` clauses see only the *clicked* row, so a mixed selection
 * reaches this handler intact.
 */
async function closeWorktree(clicked?: Node, selected?: Node[]): Promise<void> {
  const { linked, main } = partitionByRole(worktreeTargets(selectionTargets(clicked, selected)));
  if (linked.length === 0) {
    // Defensive: the daemon refuses to delete a main working tree; the UI should
    // never route one here, but never delete if it somehow does.
    if (main.length > 0) {
      void vscode.window.showErrorMessage(
        "omni-dev: this is the repository's main working tree and is never deleted. Use Close Window.",
      );
    }
    return;
  }

  await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title:
        linked.length === 1
          ? `Closing worktree “${worktreeLabel(linked[0].wt)}”…`
          : `Closing ${linked.length} worktrees…`,
    },
    async (progress) => {
      // Phase 1: what would removal lose? Side-effect-free, so fan out.
      const outcomes = await Promise.all(linked.map(closeCheck));
      if (outcomes.every((o) => o.kind === "error" && o.daemonDown)) {
        daemonDownError();
        return;
      }

      const failures: BatchFailure[] = [];
      const deletable: { target: WorktreeNode; report: CloseSafetyReport }[] = [];
      for (const outcome of outcomes) {
        if (outcome.kind === "error") {
          failures.push({ label: worktreeLabel(outcome.target.wt), message: outcome.message });
        } else if (outcome.report.is_main || !outcome.report.removable) {
          // A stale tree: the row said linked, the daemon says otherwise. Never delete.
          failures.push({
            label: worktreeLabel(outcome.target.wt),
            message:
              "this is the repository's main working tree and is never deleted. Use Close Window.",
          });
        } else {
          deletable.push(outcome);
        }
      }

      if (deletable.length === 0 || !(await confirmDelete(deletable, main, linked.length))) {
        reportFailures(failures, linked.length);
        return;
      }

      // Phase 2: execute. Each target is mostly a *wait* on its window's next
      // heartbeat, so they fan out and share one interval rather than stacking N
      // of them; `runConcurrent` keeps this window's own worktree until last and
      // alone, since closing it kills the extension host.
      failures.push(
        ...(await runConcurrent(
          deletable.map((d) => d.target),
          progress,
          async (target) => {
            const exec = await send(
              closeEnvelope(target.wt.path, {
                remove: true,
                requesterKey: windowKey,
                confirmed: true,
              }),
              CLOSE_EXECUTE_TIMEOUT_MS,
            );
            if (!exec) {
              throw new DaemonDownError();
            }
            if (!exec.ok) {
              throw new Error(`could not close worktree — ${exec.error ?? "unknown error"}`);
            }
            // Self-close: if *this* window has the worktree open, close it now that
            // the removal has succeeded (the daemon replied first to dodge the
            // ext-host-dies-mid-op race). `partitionSelfLast` held us back until
            // every other target had finished, so nothing is lost when the host
            // dies — and if the user cancels an unsaved-file prompt, the window
            // survives and the summary below still reports, exactly once.
            if (isCurrentWindow(target.wt, windowKey)) {
              await vscode.commands.executeCommand("workbench.action.closeWindow");
            }
          },
        )),
      );
      reportFailures(failures, linked.length);
    },
  );
}

/**
 * The **Close Window** command: closes the window every selected worktree is open
 * in, **without ever deleting anything** — the non-destructive counterpart to
 * {@link closeWorktree}, and the only way to close a *linked* worktree's window
 * while keeping the worktree. Selected worktrees with no window are skipped;
 * nothing is confirmed, since VS Code prompts for unsaved editors itself.
 */
async function closeWindow(clicked?: Node, selected?: Node[]): Promise<void> {
  const { open } = partitionByWindow(worktreeTargets(selectionTargets(clicked, selected)));
  if (open.length === 0) {
    return;
  }
  if (open.length === 1) {
    await runOne(open[0], closeOneWindow);
    return;
  }
  // `runBatch` fans these out and keeps this window's own worktree until last and
  // alone — closing it kills the extension host.
  await runBatch(open, `Closing ${open.length} windows…`, closeOneWindow);
}

/**
 * Closes the window holding one worktree. This window closes itself directly;
 * any other is signalled through the daemon, which delivers the directive on the
 * target's next heartbeat — the only channel it has to a window it can reply to
 * but never call — and waits for it to unregister.
 */
async function closeOneWindow(target: WorktreeNode): Promise<void> {
  if (isCurrentWindow(target.wt, windowKey)) {
    await vscode.commands.executeCommand("workbench.action.closeWindow");
    return;
  }
  const reply = await send(
    closeEnvelope(target.wt.path, {
      remove: false,
      requesterKey: windowKey,
      confirmed: true,
    }),
    CLOSE_EXECUTE_TIMEOUT_MS,
  );
  if (!reply) {
    throw new DaemonDownError();
  }
  if (!reply.ok) {
    throw new Error(`could not close window — ${reply.error ?? "unknown error"}`);
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
    provider?.update(visibleRepos(repos));
    // The one-shot `tree` reply carries `show_closed` too, so a manual refresh
    // (subscription momentarily down) keeps the toggle applied (#1301).
    applyShowClosed(reply.payload.show_closed);
    // …and `pr_source` (#1384), so the poll-volume UI stays correctly gated.
    applyPrSource(reply.payload.pr_source);
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
  await send(unregisterEnvelope(windowKey));
  await send(sessionWindowUnregisterEnvelope(windowKey));
}
