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
  defaultSocketPath,
  heartbeatEnvelope,
  openEnvelope,
  registerEnvelope,
  sendEnvelope,
  treeEnvelope,
  unregisterEnvelope,
} from "./socket";
import { Node, TreeRepoPayload, nodeId } from "./tree";
import { TreeSubscription } from "./subscription";
import { ITEM_CLICKED_COMMAND, WorktreesTreeDataProvider } from "./treeDataProvider";

const CONFIG_SECTION = "omniDevWorktrees";

/** The tree view id, matching the `views` contribution in `package.json`. */
const TREE_VIEW_ID = "omniDevWorktrees.tree";

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
 * when the daemon was unreachable.
 */
async function send(envelope: Envelope): Promise<Reply | undefined> {
  try {
    return await sendEnvelope(socketPath(), envelope);
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
  // The registry is in-memory, so a restarted daemon has forgotten this
  // window; `known: false` is our signal to re-register.
  if (reply?.ok && reply.payload?.known === false) {
    await register();
  }
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  windowKey = randomUUID();
  output = vscode.window.createOutputChannel("omni-dev Worktrees");
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
  const treeProvider = new WorktreesTreeDataProvider();
  provider = treeProvider;
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
  );
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
