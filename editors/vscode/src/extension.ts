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
  closeCheckEnvelope,
  closeEnvelope,
  defaultSocketPath,
  heartbeatEnvelope,
  openEnvelope,
  registerEnvelope,
  sendEnvelope,
  treeEnvelope,
  unregisterEnvelope,
} from "./socket";
import { Node, TreeRepoPayload, isCurrentWindow, nodeId, worktreeLabel } from "./tree";
import { TreeSubscription } from "./subscription";
import { ITEM_CLICKED_COMMAND, WorktreesTreeDataProvider } from "./treeDataProvider";

const CONFIG_SECTION = "omniDevWorktrees";

/** The tree view id, matching the `views` contribution in `package.json`. */
const TREE_VIEW_ID = "omniDevWorktrees.tree";

/**
 * The key under which the show/hide-closed toggle is both persisted (in
 * `context.globalState`, so it reads the same in every window and survives a
 * reload) and exposed as a `when`-clause context key (via `setContext`, so the
 * title-bar button swaps between its Hide/Show forms). Defaults to `true` —
 * show all worktrees, the original behavior.
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
  const treeProvider = new WorktreesTreeDataProvider(windowKey);
  provider = treeProvider;

  // Seed the show/hide-closed toggle from cross-window persisted state before the
  // first render: prime the `when`-clause context key so the correct title-bar
  // button shows, and the provider's filter so the initial tree already reflects it.
  const showClosed = context.globalState.get<boolean>(SHOW_CLOSED_KEY, true);
  void vscode.commands.executeCommand("setContext", SHOW_CLOSED_KEY, showClosed);
  treeProvider.setShowClosed(showClosed);

  const view = vscode.window.createTreeView<Node>(TREE_VIEW_ID, {
    treeDataProvider: treeProvider,
    showCollapseAll: true,
  });
  treeView = view;
  // Start with the daemon-down hint; the first snapshot clears it.
  view.message = DAEMON_DOWN_MESSAGE;
  context.subscriptions.push(view, treeProvider);

  const sub = new TreeSubscription(socketPath(), {
    onSnapshot: (repos) => {
      view.message = repos.length === 0 ? EMPTY_MESSAGE : undefined;
      treeProvider.update(repos);
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
    vscode.commands.registerCommand("omniDevWorktrees.open", (node?: Node) => void openNode(node)),
    vscode.commands.registerCommand(ITEM_CLICKED_COMMAND, (node?: Node) => onItemClicked(node)),
    vscode.commands.registerCommand(
      "omniDevWorktrees.closeWorktree",
      (node?: Node) => void closeWorktree(node),
    ),
    vscode.commands.registerCommand(
      "omniDevWorktrees.closeWindow",
      (node?: Node) => void closeWindow(node),
    ),
    // The two halves of the one title-bar toggle: exactly one is contributed at a
    // time (gated on the context key), so clicking the visible button flips the
    // state to the other.
    vscode.commands.registerCommand(
      "omniDevWorktrees.hideClosedWorktrees",
      () => void setShowClosed(context, false),
    ),
    vscode.commands.registerCommand(
      "omniDevWorktrees.showClosedWorktrees",
      () => void setShowClosed(context, true),
    ),
  );
}

/**
 * Flips the show/hide-closed toggle. Persists it to `context.globalState` (so it
 * reads the same in every window and survives a reload), updates the
 * `when`-clause context key so the title-bar button swaps to its other form, and
 * re-filters the live tree — a fresh daemon snapshot then keeps this filter.
 */
async function setShowClosed(
  context: vscode.ExtensionContext,
  showClosed: boolean,
): Promise<void> {
  await context.globalState.update(SHOW_CLOSED_KEY, showClosed);
  await vscode.commands.executeCommand("setContext", SHOW_CLOSED_KEY, showClosed);
  provider?.setShowClosed(showClosed);
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
  // The tree view, provider, and subscription are torn down via
  // `context.subscriptions`; drop our references so a reactivation starts fresh.
  provider = undefined;
  treeView = undefined;
  lastClick = undefined;
  await send(unregisterEnvelope(windowKey));
}
