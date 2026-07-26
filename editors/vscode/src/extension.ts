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
  mergeQueueCheckEnvelope,
  mergeQueueEnvelope,
  openEnvelope,
  openPrsEnvelope,
  registerEnvelope,
  ReloadReply,
  reloadEnvelope,
  RepositionReply,
  repositionEnvelope,
  repositionUndoEnvelope,
  sendEnvelope,
  sessionWindowEnvelope,
  sessionWindowUnregisterEnvelope,
  sessionsListEnvelope,
  setPollingEnvelope,
  setShowClosedEnvelope,
  treeEnvelope,
  unregisterEnvelope,
} from "./socket";
import { runGh } from "./gh";
import { PullRequest, parsePrList, prFallbackBadge, prListArgsForRepo } from "./github";
import { countClaudeTabs, countClaudeTerminals } from "./claudeEmbeddings";
import { SessionEntry, tallyByWorktree } from "./sessionCounts";
import { openPullRequest, openPullRequestInBrowser } from "./prCommands";
import { nextClaudeTerminalName, resolveClaudeCommand, resolveClaudeCwd } from "./claude";
import { moveClaudeSessionHere } from "./moveSessionCommand";
import { rebaseOnMain } from "./rebaseCommand";
import { RowColorMap } from "./icons";
import { clearAllRowColors, setRowColor } from "./rowColorCommand";
import {
  AheadBehindMap,
  Node,
  PrBadge,
  TreeGithubIdentity,
  TreeRepoPayload,
  WorktreeNode,
  describeReload,
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
import { SessionsSubscription, TreeSubscription } from "./subscription";
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
 * The `when`-clause context key gating the "Reposition Windows to Match" menu item
 * on a platform where the daemon can actually move windows (#1407).
 *
 * Set once at activation from `process.platform`, which is sound because the daemon
 * always runs on the same machine as this extension host — it is reached over a
 * local Unix socket. Off macOS the op replies "untrusted" and moves nothing, so
 * hiding the item is a courtesy rather than the guard.
 */
const CAN_REPOSITION_KEY = "omniDevWorktrees.canReposition";

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
let decorationProviders: WorktreeDecorationProvider[] = [];

/** Re-queries every worktree row's badges, on both decoration dimensions. */
function refreshDecorations(): void {
  for (const provider of decorationProviders) {
    provider.refresh();
  }
}
/** The last worktree click, for the manual double-click timer in `onItemClicked`. */
let lastClick: { id: string; at: number } | undefined;
/**
 * The worktree paths from the latest snapshot, so the Claude session cues (#1406)
 * can attribute sessions to rows without re-reading the tree.
 */
let worktreePaths: string[] = [];
/**
 * The most recent set of live Claude sessions, from whichever feed supplied it.
 * Kept so a *tree* change can re-attribute the sessions it already has to the new
 * row set without another round-trip (#1414).
 */
let lastSessions: SessionEntry[] = [];
/**
 * Whether the daemon is pushing session state to this window (#1414). While true
 * the ~10s `list` poll stays off; a daemon too old to stream the op flips it back
 * to `false` and the poll resumes.
 */
let sessionPushActive = false;
/** The live sessions subscription, so a daemon restart can revive it. */
let sessionSubscription: SessionsSubscription | undefined;

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

/** Whether to show each worktree's Claude session cue (#1406). */
function showClaudeSessions(): boolean {
  return config().get<boolean>("showClaudeSessions") ?? true;
}

/**
 * The per-row icon colour tags (#1428), keyed by `nodeId`.
 *
 * Read from the **user scope** specifically, not the merged `get()` view: the same value
 * is written back on every edit, and writing the merged view would bake a default (or
 * some future lower-scope value) into the user's `settings.json`. The setting is
 * declared `"scope": "application"` so user scope is the only one it can occupy anyway
 * — this keeps that true rather than assuming it. Content is validated per-lookup by
 * `rowColorTag`, since a hand-edited settings file can hold anything.
 */
function rowColors(): RowColorMap {
  return config().inspect<RowColorMap>("rowColors")?.globalValue ?? {};
}

/**
 * Persists the row colour tags, or removes the key entirely when the map is empty so
 * clearing the last colour does not leave `"omniDevWorktrees.rowColors": {}` behind.
 *
 * `Global` is mandatory, not merely preferred: an `application`-scoped setting rejects a
 * workspace-target write. It is also the point — the tree spans every repo across every
 * window, so a workspace-scoped map would apply only in whichever window happened to
 * have that folder open.
 */
async function writeRowColors(colors: RowColorMap): Promise<void> {
  const value = Object.keys(colors).length === 0 ? undefined : colors;
  await config().update("rowColors", value, vscode.ConfigurationTarget.Global);
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

/**
 * Attributes {@link lastSessions} to the current worktree rows and pushes the
 * result into the tree (#1406). The single choke point both the push (#1414) and
 * the poll fall through to, and the one to call when only the *rows* changed.
 *
 * A complete no-op when nothing changed — `setSessionTallies` compares first,
 * and the provider's refresh re-runs the lazy ahead/behind and PR fetches, so it
 * must only fire on a real change.
 */
function retallySessions(): void {
  if (!provider || !showClaudeSessions()) {
    return;
  }
  if (provider.setSessionTallies(tallyByWorktree(lastSessions, worktreePaths))) {
    refreshDecorations();
  }
}

/**
 * Records a fresh set of live sessions — from the push or the poll — and repaints.
 */
function applySessionSnapshot(sessions: SessionEntry[]): void {
  lastSessions = sessions;
  retallySessions();
}

/**
 * Polls the daemon for every live Claude session (#1406).
 *
 * Since #1414 this is the **fallback** path only: it runs while
 * {@link sessionPushActive} is false, i.e. against a daemon too old to stream the
 * sessions `subscribe` op. Cheap and best-effort — one socket round-trip whose
 * failure leaves the tree exactly as it was, skipped when the view is hidden (the
 * reveal handler catches it up) or the cue is switched off.
 */
async function refreshSessionCues(): Promise<void> {
  if (!provider || !showClaudeSessions() || treeView?.visible === false) {
    return;
  }
  const reply = await send(sessionsListEnvelope());
  const sessions = reply?.ok ? (reply.payload?.sessions as SessionEntry[] | undefined) : undefined;
  if (!Array.isArray(sessions)) {
    return;
  }
  applySessionSnapshot(sessions);
}

/** Refreshes the session cues from whichever feed is currently live. */
function syncSessionCues(): void {
  if (sessionPushActive) {
    retallySessions();
  } else {
    void refreshSessionCues();
  }
}

/** Clears every session cue, e.g. when the setting is switched off. */
function clearSessionCues(): void {
  if (provider?.setSessionTallies({})) {
    refreshDecorations();
  }
}

/** How long a repo's open-PR list is reused before a fresh `gh` fetch (#1296). */
const PR_CACHE_TTL_MS = 60_000;

interface PrCacheEntry {
  /** When this repo's PRs were fetched (`Date.now()`). */
  at: number;
  /** The repo's open PRs, or `[]` (also cached on a `gh` failure — see below). */
  prs: PullRequest[];
}

/** Per-repo (`owner/name`) cache of the last open-PR fetch, TTL'd by {@link PR_CACHE_TTL_MS}. */
const prCache = new Map<string, PrCacheEntry>();

/**
 * A repo's open PRs from the daemon's shared, TTL-cached `open-prs` op (#1389,
 * fix 7), or `undefined` when the daemon is unreachable or too old to serve the op
 * (a non-`ok` reply, or a payload without a `pull_requests` array) — the signal to
 * fall back to this window's own `gh`.
 */
async function daemonRepoPrs(repo: TreeGithubIdentity): Promise<PullRequest[] | undefined> {
  const reply = await send(openPrsEnvelope(repo.owner, repo.name));
  if (!reply?.ok) {
    return undefined;
  }
  const raw = reply.payload?.pull_requests;
  return Array.isArray(raw) ? (raw as PullRequest[]) : undefined;
}

/**
 * Every open PR of a repo, preferring the daemon's shared op so N windows dedupe to
 * one counted `gh` (#1389, fix 7), and falling back to this window's own
 * `gh pr list` **only** against a daemon too old to serve it. This is the single
 * choke point both the "Open Pull Request…" command and the transient badge
 * fallback resolve a repo's PRs through.
 */
async function repoOpenPrs(repo: TreeGithubIdentity): Promise<PullRequest[]> {
  const viaDaemon = await daemonRepoPrs(repo);
  if (viaDaemon !== undefined) {
    return viaDaemon;
  }
  return parsePrList(await runGh(prListArgsForRepo(repo)));
}

/**
 * The repo's open PRs, from this window's cache when fresh, else {@link repoOpenPrs}
 * (the shared daemon op, or one `gh pr list` against an old daemon). A failure —
 * missing binary, not authed, unknown repo — is **cached as an empty list** for the
 * TTL and logged once, so it is not re-attempted on every pushed snapshot; the
 * explicit "Open Pull Request…" action still surfaces the real error.
 */
async function cachedRepoPrs(repo: TreeGithubIdentity): Promise<PullRequest[]> {
  const key = `${repo.owner}/${repo.name}`;
  const now = Date.now();
  const hit = prCache.get(key);
  if (hit && now - hit.at < PR_CACHE_TTL_MS) {
    return hit.prs;
  }
  try {
    const prs = await repoOpenPrs(repo);
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
 * Resolves a **degraded** PR badge for each branch the daemon left unresolved —
 * the {@link PrBadgeFetcher} injected into the tree provider.
 *
 * Badges are resolved daemon-side since #1337: one `gh api graphql` covers every
 * repo and branch, and a background poller keeps CI state live in every window.
 * A current daemon reports every checked branch as either a badge (`pr`) or the
 * explicit "no open PR" negative (`pr_none`, #1370), and the provider asks only
 * for branches carrying neither — so against a current daemon this runs no `gh`
 * at all. (Before #1370 a PR-less branch was indistinguishable from an unchecked
 * one, which kept this firing per window × repo × 60s forever.) Against a
 * pre-#1370 daemon the unresolved branches land here and it reports the PR
 * number without a checks glyph (see {@link prFallbackBadge}) — the intended
 * degraded path. A no-op when `showPullRequests` is off.
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
    const badge = prFallbackBadge(prs, branch);
    if (badge) {
      badges[branch] = badge;
    }
  }
  return badges;
}

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
  // A cross-window "Reload Window" (#1417) arrives on the same channel. Checked
  // *after* `close`, so if both are somehow pending the close wins — closing
  // subsumes reloading, and reloading first would only delay it a heartbeat.
  if (reply.payload?.reload === true) {
    await vscode.commands.executeCommand("workbench.action.reloadWindow");
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
    // Session state rides its own op, not the tree snapshot. The daemon pushes it
    // (#1414), so this poll is the fallback for a daemon too old to stream it —
    // and the reason two windows could disagree about a row's cue for up to a
    // full tick, since each window's phase is set by whenever it activated.
    if (!sessionPushActive) {
      void refreshSessionCues();
    }
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
  // The provider fetches ahead/behind (#1306) and PR badges (#1296) lazily on
  // expand via the injected `fetchAheadBehind` / `fetchPrBadges`.
  const treeProvider = new WorktreesTreeDataProvider(windowKey, fetchAheadBehind, fetchPrBadges);
  provider = treeProvider;

  // Seed the button/filter to the default (show all) before the first render;
  // the daemon's pushed `show_closed` is authoritative and updates both the
  // moment the first snapshot lands (#1301) — no per-window `globalState`.
  applyShowClosed(true);
  // Seed the PR-master context key + icon-colour flag from the current setting
  // (#1376), so the per-repo toggle menu and green icons reflect it from frame one.
  applyShowPullRequests();
  // Likewise the per-row colour tags (#1428), so tagged rows render tagged from the
  // first frame rather than only after the setting is next edited.
  applyRowColors();
  // Window repositioning is macOS-only for now (ADR-0058), so hide its menu item
  // elsewhere rather than offering an action that can only report "unsupported".
  void vscode.commands.executeCommand(
    "setContext",
    CAN_REPOSITION_KEY,
    process.platform === "darwin",
  );

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

  // The colored badges: the PR CI-check verdict (#1324) and the Claude session
  // cue (#1406), painted off the custom-scheme `resourceUri` the tree items
  // carry. Two providers because one decoration's badge holds only two
  // characters; VS Code concatenates them onto the row. They are `refresh()`ed
  // on every snapshot so colours track the lazily-fetched PR and session state.
  //
  // Registration order sets the order of the merged glyphs — the workbench
  // iterates providers most-recently-registered first — so the session cue is
  // registered last to lead. Both share one severity-ranked colour regardless.
  decorationProviders = [
    new WorktreeDecorationProvider("checks"),
    new WorktreeDecorationProvider("sessions"),
  ];
  for (const decorations of decorationProviders) {
    context.subscriptions.push(
      decorations,
      vscode.window.registerFileDecorationProvider(decorations),
    );
  }

  const sub = new TreeSubscription(socketPath(), {
    onSnapshot: (snapshot) => {
      view.message = snapshot.repos.length === 0 ? EMPTY_MESSAGE : undefined;
      treeProvider.update(visibleRepos(snapshot.repos));
      rememberWorktreePaths(snapshot.repos);
      // The daemon-backed toggle rides every snapshot, so a flip in any window
      // re-renders this one and a fresh window initializes on its first frame.
      applyShowClosed(snapshot.show_closed);
      // A new snapshot re-runs the lazy PR-badge fetch, so re-evaluate every row's
      // check colour (state-keyed URIs already re-decorate; this covers the rest).
      refreshDecorations();
      // The row set just changed, so re-attribute the live sessions to it (#1406).
      // Under the push (#1414) that is a pure re-tally — the sessions themselves
      // did not change, so there is nothing to re-fetch.
      syncSessionCues();
    },
    onStatus: (connected) => {
      // A drop re-shows the hint; a (re)connect's message is set by the snapshot.
      if (!connected) {
        view.message = DAEMON_DOWN_MESSAGE;
        return;
      }
      // The daemon is back. An upgrade lands as a restart, so a sessions
      // subscription that fell back against the *old* daemon gets one more
      // chance here rather than waiting for a window reload (#1414).
      if (!sessionPushActive) {
        sessionSubscription?.start();
      }
    },
    onError: (message) => output?.appendLine(`subscription: ${message}`),
  });
  sub.start();
  context.subscriptions.push({ dispose: () => sub.close() });

  // The parallel push of Claude session state (#1414). Worktree rows and session
  // cues are independent streams, so they get one connection each; this one
  // degrades on its own to the ~10s `list` poll without touching the tree.
  const sessions = new SessionsSubscription(socketPath(), {
    onSnapshot: (snapshot) => {
      sessionPushActive = true;
      applySessionSnapshot(snapshot.sessions);
    },
    onUnsupported: (message) => {
      // The daemon answered but will not stream: too old to know the op. Resume
      // polling and say so once, rather than silently showing stale cues.
      sessionPushActive = false;
      output?.appendLine(`sessions subscription unavailable, falling back to polling: ${message}`);
      void refreshSessionCues();
    },
    onStatus: (connected) => {
      // A dropped stream leaves the last tally rendered; the poll covers the gap
      // until the subscription's own backoff reconnects.
      if (!connected) {
        sessionPushActive = false;
      }
    },
    onError: (message) => output?.appendLine(`sessions subscription: ${message}`),
  });
  sessionSubscription = sessions;
  sessions.start();
  context.subscriptions.push({
    dispose: () => {
      sessions.close();
      sessionSubscription = undefined;
      sessionPushActive = false;
    },
  });

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
      "omniDevWorktrees.reloadWindow",
      (node?: Node, selected?: Node[]) => void reloadWindow(node, selected),
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
    // Row colour tags (#1428). `read`/`write` are injected so `ConfigurationTarget` and
    // the settings key stay in this file and the command module stays a thin adapter.
    // Nothing is applied here: the write echoes back through `onDidChangeConfiguration`,
    // which is the single place the provider is updated, in this window and every other.
    vscode.commands.registerCommand(
      "omniDevWorktrees.setRowColor",
      (node?: Node, selected?: Node[]) =>
        void setRowColor({ read: rowColors, write: writeRowColors }, node, selected),
    ),
    vscode.commands.registerCommand(
      "omniDevWorktrees.clearAllRowColors",
      () => void clearAllRowColors({ read: rowColors, write: writeRowColors }),
    ),
    vscode.commands.registerCommand(
      "omniDevWorktrees.openPullRequest",
      (node?: Node, selected?: Node[]) => void openPullRequest(node, selected, repoOpenPrs),
    ),
    vscode.commands.registerCommand(
      "omniDevWorktrees.openPullRequestInBrowser",
      (node?: Node, selected?: Node[]) =>
        void openPullRequestInBrowser(node, selected, repoOpenPrs),
    ),
    vscode.commands.registerCommand(
      "omniDevWorktrees.addToMergeQueue",
      (node?: Node, selected?: Node[]) => void addToMergeQueue(node, selected),
    ),
    // A daemon op since #1415: the daemon inherits the user's `ssh-agent` from
    // launchd, so it can fetch, and hosting the rebase there is what lets a
    // conflict be left in place. `send`/`windowKey` are injected so the handler
    // itself stays out of this file. See `rebaseCommand.ts` and ADR-0059.
    vscode.commands.registerCommand(
      "omniDevWorktrees.rebaseOnMain",
      (node?: Node, selected?: Node[]) =>
        void rebaseOnMain({ send, windowKey }, node, selected),
    ),
    // Geometry work happens in the daemon, which is the only process that can do
    // it (#1407) — a VS Code extension has no API for its own window's bounds.
    vscode.commands.registerCommand(
      "omniDevWorktrees.repositionWindowsToMatch",
      (node?: Node, selected?: Node[]) => void repositionWindowsToMatch(node, selected),
    ),
    // Deliberately not selection-scoped: the daemon holds the undo record, so this
    // is reachable from the summary toast's action and the command palette alike.
    vscode.commands.registerCommand(
      "omniDevWorktrees.undoReposition",
      () => void undoReposition(),
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
      // The per-row colour tags (#1428). This arm is the *only* place they are applied
      // — never optimistically at write time — because user-scope settings changes echo
      // back to the writing window too, so applying in both places would refresh it
      // twice. Deliberately no `refreshTree()`: a colour is client-side presentation and
      // needs no daemon round-trip, and the provider's own no-op guard keeps an
      // unrelated repaint from re-triggering the lazy ahead/behind and PR fetches.
      if (e.affectsConfiguration(`${CONFIG_SECTION}.rowColors`)) {
        applyRowColors();
      }
      // Flipping the Claude cue on repopulates it now; flipping it off has to
      // clear what is already rendered, since the cue feed simply stops (#1406).
      if (e.affectsConfiguration(`${CONFIG_SECTION}.showClaudeSessions`)) {
        if (showClaudeSessions()) {
          syncSessionCues();
        } else {
          clearSessionCues();
        }
      }
    }),
    // A hidden tree is not *polled*, so catch it up the moment it is revealed.
    // Under the push (#1414) the tally is already current and this is a no-op.
    view.onDidChangeVisibility((e) => {
      if (e.visible) {
        syncSessionCues();
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
 * Applies the per-row icon colour tags to this window's tree (#1428).
 *
 * Purely local: unlike the show/hide-closed toggle and the per-repo poll flag, the tags
 * never reach the daemon. They still sync across windows, because user-scope settings
 * changes fire `onDidChangeConfiguration` in every one of them — the cross-window event
 * `context.globalState` lacks, which is what #1301 needed the daemon for.
 */
function applyRowColors(): void {
  provider?.setRowColors(rowColors());
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
 * Generous timeout for a `merge-queue` execute call: the daemon re-validates every
 * target and then issues one `gh api graphql` mutation per eligible PR.
 */
const MERGE_QUEUE_EXECUTE_TIMEOUT_MS = 60_000;

/** One enqueue-eligible worktree (mirrors `PrRef` in Rust). */
interface MergeQueuePrRef {
  path: string;
  number: number;
  url: string;
  branch: string;
}

/** One skipped worktree and why (mirrors `Skip` in Rust). */
interface MergeQueueSkip {
  path: string;
  kind: string;
  detail: string;
}

/** The daemon's phase-1 report (mirrors `EligibilityReport` in Rust). */
interface EligibilityReport {
  eligible: MergeQueuePrRef[];
  skipped: MergeQueueSkip[];
}

/** The daemon's phase-2 result (mirrors `EnqueueResult` in Rust). */
interface EnqueueResult {
  queued: { path: string; number: number; already_queued?: boolean }[];
  skipped: MergeQueueSkip[];
  failed: { path: string; number: number; error: string }[];
}

/**
 * The **Add to Merge Queue** command: enqueues every selected worktree's PR into
 * the GitHub merge queue — but only the ones that pass every eligibility gate
 * (clean, committed, pushed, with an open non-draft, conflict-free, CI-green PR).
 * Ineligible worktrees are reported as skipped-with-reason, never enqueued.
 *
 * Two-phase like {@link closeWorktree} (ADR-0049's shape), but the daemon op is a
 * **single batched** call over all paths rather than a client-side fan-out, so
 * this sends one check envelope and one execute envelope. The batch confirms once
 * (ADR-0049 §1) — one gesture can enqueue N PRs, and the modal is the only place
 * the user sees the full set.
 *
 * Repo nodes are filtered out here rather than trusted to the menu: `when` clauses
 * see only the *clicked* row, so a mixed selection reaches this handler intact.
 * The daemon re-validates every gate on execute; this gating is convenience only.
 */
async function addToMergeQueue(clicked?: Node, selected?: Node[]): Promise<void> {
  const targets = worktreeTargets(selectionTargets(clicked, selected));
  if (targets.length === 0) {
    return;
  }
  const paths = targets.map((t) => t.wt.path);

  await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title:
        targets.length === 1
          ? `Checking “${worktreeLabel(targets[0].wt)}”…`
          : `Checking ${targets.length} worktrees…`,
    },
    async (progress) => {
      // Phase 1: eligibility. Side-effect-free, and one op for the whole batch.
      const checked = await send(mergeQueueCheckEnvelope(paths, windowKey));
      if (!checked) {
        daemonDownError();
        return;
      }
      if (!checked.ok) {
        void vscode.window.showErrorMessage(
          `omni-dev: could not check merge-queue eligibility — ${checked.error ?? "unknown error"}`,
        );
        return;
      }
      const report = checked.payload as EligibilityReport;
      const eligible = report.eligible ?? [];
      const skipped = report.skipped ?? [];

      if (eligible.length === 0) {
        void vscode.window.showWarningMessage(
          `omni-dev: nothing to enqueue — ${describeSkips(skipped, targets.length)}`,
        );
        return;
      }
      if (!(await confirmEnqueue(eligible, skipped, targets.length))) {
        return;
      }

      // Phase 2: execute. The daemon re-validates before enqueuing anything.
      progress.report({ message: `Enqueuing ${eligible.length}…` });
      const exec = await send(
        mergeQueueEnvelope(paths, windowKey),
        MERGE_QUEUE_EXECUTE_TIMEOUT_MS,
      );
      if (!exec) {
        daemonDownError();
        return;
      }
      if (!exec.ok) {
        void vscode.window.showErrorMessage(
          `omni-dev: merge-queue enqueue failed — ${exec.error ?? "unknown error"}`,
        );
        return;
      }
      reportEnqueueResult(exec.payload as EnqueueResult);
    },
  );
}

/** A compact "2 skipped (dirty, no-pr)" summary of why worktrees were skipped. */
function describeSkips(skipped: MergeQueueSkip[], total: number): string {
  if (skipped.length === 0) {
    return `no eligible worktrees in the ${total} selected`;
  }
  const kinds = [...new Set(skipped.map((s) => s.kind))].join(", ");
  return `${skipped.length} of ${total} skipped (${kinds})`;
}

/**
 * The enqueue confirmation. Always shown — enqueuing mutates remote state on
 * GitHub, and a batch is one gesture over N PRs — listing exactly which PRs will
 * be queued and which were skipped, with reasons.
 */
async function confirmEnqueue(
  eligible: MergeQueuePrRef[],
  skipped: MergeQueueSkip[],
  total: number,
): Promise<boolean> {
  const confirmLabel = eligible.length === 1 ? "Add to Merge Queue" : "Add All to Merge Queue";
  const detail = [
    ...eligible.map((e) => `• #${e.number} ${e.branch}`),
    ...(skipped.length > 0
      ? ["", "Skipped:", ...skipped.map((s) => `• ${s.detail} (${s.kind})`)]
      : []),
  ].join("\n");
  const choice = await vscode.window.showWarningMessage(
    skipped.length > 0
      ? `Enqueue ${eligible.length} of ${total} worktrees? (${skipped.length} skipped)`
      : `Enqueue ${eligible.length} ${eligible.length === 1 ? "pull request" : "pull requests"}?`,
    { modal: true, detail },
    confirmLabel,
  );
  return choice === confirmLabel;
}

/** Toasts the phase-2 outcome: how many queued, and any per-PR failures. */
function reportEnqueueResult(result: EnqueueResult): void {
  const queued = result.queued ?? [];
  const failed = result.failed ?? [];
  if (failed.length > 0) {
    void vscode.window.showErrorMessage(
      `omni-dev: ${failed.length} of ${queued.length + failed.length} failed — ${failed
        .map((f) => `#${f.number}: ${f.error}`)
        .join("; ")}`,
    );
    return;
  }
  const already = queued.filter((q) => q.already_queued).length;
  const suffix = already > 0 ? ` (${already} already queued)` : "";
  void vscode.window.showInformationMessage(
    `omni-dev: ${queued.length} ${
      queued.length === 1 ? "pull request" : "pull requests"
    } in the merge queue${suffix}.`,
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

// --- Reposition windows (#1407) ---------------------------------------------

/**
 * How long to wait for a `reposition`. Each target costs a handful of synchronous
 * Accessibility round-trips into VS Code's main process, and the daemon caps each at
 * ~2s, so a large selection against a busy app needs more than the default.
 */
const REPOSITION_TIMEOUT_MS = 30_000;

/** The deep link to the Accessibility pane of System Settings. */
const ACCESSIBILITY_SETTINGS_URL =
  "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/**
 * Moves and resizes every selected worktree's already-open window to match **this**
 * window's position and size (#1407).
 *
 * This window is the reference: it supplies the geometry and never moves, so it is
 * filtered out of the targets here as well as being skipped daemon-side. Worktrees
 * with no open window are dropped client-side and, if a row was stale, reported by
 * the daemon as `no-window`.
 *
 * Fires immediately with no confirmation — this is a routine layout command, and a
 * modal on every use would cost more than it protects. Reversibility comes from the
 * **Undo** action offered on the summary, which is why the toast is worth showing
 * even on complete success.
 *
 * Repo nodes are filtered out here rather than trusted to the menu: a `when` clause
 * sees only the *clicked* row, so a mixed selection reaches this handler intact.
 */
async function repositionWindowsToMatch(clicked?: Node, selected?: Node[]): Promise<void> {
  const { open } = partitionByWindow(worktreeTargets(selectionTargets(clicked, selected)));
  // Splitting self out is not cosmetic: the reference must not be in its own
  // target list, or the batch reports a confusing self-skip for the window the
  // user invoked from.
  const { others } = partitionSelfLast(open, windowKey);
  const targets = others.filter((node) => node.wt.window_key);
  if (targets.length === 0) {
    void vscode.window.showWarningMessage(
      "omni-dev: nothing to reposition — select worktrees other than this one that have a window open.",
    );
    return;
  }
  const keys = targets.map((node) => node.wt.window_key as string);

  const reply = await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title:
        targets.length === 1
          ? `Repositioning “${worktreeLabel(targets[0].wt)}”…`
          : `Repositioning ${targets.length} windows…`,
    },
    () => send(repositionEnvelope(windowKey, keys), REPOSITION_TIMEOUT_MS),
  );
  if (!reply) {
    daemonDownError();
    return;
  }
  if (!reply.ok) {
    void vscode.window.showErrorMessage(
      `omni-dev: could not reposition windows — ${reply.error ?? "unknown error"}`,
    );
    return;
  }
  reportReposition(reply.payload as RepositionReply, targets.length);
}

/**
 * Puts the windows the last reposition moved back where they were.
 *
 * The daemon holds the record, so this is deliberately reachable from anywhere —
 * the summary toast's action button and the command palette — rather than being
 * scoped to a selection. It is one level deep: once consumed, there is nothing to
 * replay onto a layout the user may have since redone by hand.
 */
async function undoReposition(): Promise<void> {
  const reply = await send(repositionUndoEnvelope(), REPOSITION_TIMEOUT_MS);
  if (!reply) {
    daemonDownError();
    return;
  }
  if (!reply.ok) {
    void vscode.window.showErrorMessage(
      `omni-dev: could not undo the reposition — ${reply.error ?? "unknown error"}`,
    );
    return;
  }
  const payload = reply.payload as RepositionReply;
  if (payload.trusted === false) {
    void showAccessibilityError();
    return;
  }
  const restored = payload.moved ?? 0;
  if (restored === 0) {
    void vscode.window.showInformationMessage("omni-dev: nothing to undo.");
    return;
  }
  void vscode.window.showInformationMessage(
    `omni-dev: restored ${restored} ${restored === 1 ? "window" : "windows"}.`,
  );
}

/**
 * Toasts a `reposition` outcome, offering **Undo** when the daemon recorded one.
 *
 * The missing-permission and blocked-reference cases get their own messages: both
 * mean *nothing* moved for a reason the user can act on, which a bare
 * "0 repositioned" would hide.
 */
function reportReposition(payload: RepositionReply, requested: number): void {
  if (payload.trusted === false) {
    void showAccessibilityError();
    return;
  }
  if (payload.blocked) {
    void vscode.window.showErrorMessage(
      `omni-dev: nothing was moved — ${payload.blocked.detail}.`,
    );
    return;
  }

  const moved = payload.moved ?? 0;
  const skipped = payload.skipped ?? 0;
  if (moved === 0) {
    void vscode.window.showWarningMessage(
      `omni-dev: no window was moved — ${describeSkippedReposition(payload)}`,
    );
    return;
  }
  const summary =
    skipped > 0
      ? `omni-dev: repositioned ${moved} of ${requested} windows (${describeSkippedReposition(payload)})`
      : `omni-dev: repositioned ${moved} ${moved === 1 ? "window" : "windows"}.`;
  if (payload.undoable) {
    void vscode.window.showInformationMessage(summary, "Undo").then((choice) => {
      if (choice === "Undo") {
        void undoReposition();
      }
    });
    return;
  }
  void vscode.window.showInformationMessage(summary);
}

/** A compact "2 skipped: ambiguous, fullscreen" summary of what was left alone. */
function describeSkippedReposition(payload: RepositionReply): string {
  const skipped = (payload.results ?? []).filter(
    (result) => result.outcome !== "moved" && result.outcome !== "partial",
  );
  if (skipped.length === 0) {
    return "nothing was skipped";
  }
  const kinds = [...new Set(skipped.map((result) => result.outcome))].join(", ");
  return `${skipped.length} skipped: ${kinds}`;
}

/**
 * Reports the missing Accessibility grant with a button that opens the pane the
 * user needs. A restart is part of the instruction because macOS only applies a new
 * grant to a freshly-spawned process, so the resident daemon will not pick it up.
 */
async function showAccessibilityError(): Promise<void> {
  const open = "Open Accessibility Settings";
  const choice = await vscode.window.showErrorMessage(
    "omni-dev: moving windows needs the macOS Accessibility permission. Add the omni-dev " +
      "binary under Privacy & Security → Accessibility, then run `omni-dev daemon restart`.",
    open,
  );
  if (choice === open) {
    await vscode.env.openExternal(vscode.Uri.parse(ACCESSIBILITY_SETTINGS_URL));
  }
}

// --- Reload windows (#1417) --------------------------------------------------

/**
 * How long to wait for a `reload`. The op only marks a directive per target — no
 * git, no OS calls, no waiting for the targets to act — so it needs nothing like
 * {@link CLOSE_EXECUTE_TIMEOUT_MS}.
 */
const RELOAD_TIMEOUT_MS = 10_000;

/**
 * Reloads the VS Code window of every selected worktree (#1417) — the batch form
 * of `Developer: Reload Window`, which otherwise has to be run by hand in each
 * window in turn.
 *
 * Selected worktrees with **no open window** are skipped rather than refused:
 * the selection is a sweep, so a row with nothing to reload is simply not a
 * target. The count still reaches the summary, so a narrowed batch is never
 * silently narrowed.
 *
 * Fires with no confirmation, following the `reposition` precedent rather than
 * `close`'s two-phase confirm: a reload creates, modifies and destroys nothing,
 * and VS Code's hot exit preserves dirty editors. There is correspondingly
 * nothing to undo.
 *
 * Repo nodes are filtered out here rather than trusted to the menu: a `when`
 * clause sees only the *clicked* row, so a mixed selection reaches this handler
 * intact.
 */
async function reloadWindow(clicked?: Node, selected?: Node[]): Promise<void> {
  const { open, closed } = partitionByWindow(
    worktreeTargets(selectionTargets(clicked, selected)),
  );
  // This window must reload alone and last — doing so kills this extension host,
  // taking any in-flight request and the summary notification with it.
  const { others, self } = partitionSelfLast(open, windowKey);
  const keys = others
    .map((node) => node.wt.window_key)
    .filter((key): key is string => !!key);

  if (keys.length === 0 && self.length === 0) {
    void vscode.window.showWarningMessage(
      "omni-dev: nothing to reload — no selected worktree has a window open.",
    );
    return;
  }

  let signalled = 0;
  let skipped = closed.length;
  if (keys.length > 0) {
    const reply = await vscode.window.withProgress(
      {
        location: vscode.ProgressLocation.Notification,
        title:
          keys.length === 1
            ? `Reloading “${worktreeLabel(others[0].wt)}”…`
            : `Reloading ${keys.length} windows…`,
      },
      () => send(reloadEnvelope(keys), RELOAD_TIMEOUT_MS),
    );
    if (!reply) {
      daemonDownError();
      return;
    }
    if (!reply.ok) {
      void vscode.window.showErrorMessage(
        `omni-dev: could not reload windows — ${reply.error ?? "unknown error"}`,
      );
      return;
    }
    const payload = reply.payload as ReloadReply;
    signalled = payload.signalled ?? 0;
    // A key the daemon had no live window for is a row that went stale between
    // render and send — the same "no window open" outcome from the user's side.
    skipped += payload.unknown?.length ?? 0;
  }

  // Report *before* touching this window: reloading it kills the host, and the
  // notification with it.
  void vscode.window.showInformationMessage(
    describeReload({ signalled, skipped, self: self.length }),
  );

  for (const _target of self) {
    await vscode.commands.executeCommand("workbench.action.reloadWindow");
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
    rememberWorktreePaths(repos);
    // The one-shot `tree` reply carries `show_closed` too, so a manual refresh
    // (subscription momentarily down) keeps the toggle applied (#1301).
    applyShowClosed(reply.payload.show_closed);
    // Re-evaluate the PR-check colours for the freshly-fetched rows (#1324).
    refreshDecorations();
    syncSessionCues();
    if (treeView) {
      treeView.message = repos.length === 0 ? EMPTY_MESSAGE : undefined;
    }
  }
}

/**
 * Records the worktree paths a fresh snapshot carries, which is what the Claude
 * session poll attributes sessions to (#1406).
 */
function rememberWorktreePaths(repos: TreeRepoPayload[]): void {
  worktreePaths = repos.flatMap((repo) => repo.worktrees.map((wt) => wt.path));
}

export async function deactivate(): Promise<void> {
  if (heartbeatTimer) {
    clearInterval(heartbeatTimer);
    heartbeatTimer = undefined;
  }
  // The tree view, provider, decoration provider, and subscription are torn down
  // via `context.subscriptions`; drop our references so a reactivation starts fresh.
  provider = undefined;
  decorationProviders = [];
  treeView = undefined;
  lastClick = undefined;
  await send(unregisterEnvelope(windowKey));
  await send(sessionWindowUnregisterEnvelope(windowKey));
}
