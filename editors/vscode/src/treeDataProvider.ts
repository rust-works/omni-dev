// The `vscode`-facing tree data provider. It is a thin adapter: all model and
// formatting logic lives in the `vscode`-free `tree.ts` (which is unit-tested);
// this file only maps a `Node` onto a `vscode.TreeItem` (icons, collapsible
// state, the per-click command) and re-fires the tree when a snapshot arrives.

import * as vscode from "vscode";

import {
  ModelFamilyMap,
  SessionTallyMap,
  formatModelMarker,
  sameModelFamilies,
  sameTallies,
  sessionDecoration,
  sessionGlyphs,
  sessionTooltipLine,
  unionModelFamilies,
} from "./sessionCounts";
import {
  AheadBehindMemo,
  AheadBehindBatchFetcher,
  AheadBehindTarget,
} from "./aheadBehindMemo";
import {
  CLOSED_GROUP_CONTEXT,
  CLOSED_GROUP_LABEL,
  closedContextValue,
  closedDescription,
  closedGroupDescription,
  closedIconId,
  closedLabel,
  closedTooltip,
  sameClosed,
} from "./recentlyClosed";
import { ClosedWorktreePayload } from "./socket";
import {
  AheadBehindMap,
  PrBadge,
  TreeElement,
  TreeGithubIdentity,
  TreeRepoPayload,
  closedChildNodes,
  elementId,
  needsPrFallback,
  repoContextValue,
  repoDescription,
  repoLabel,
  repoPollingEnabled,
  rootNodes,
  unbadgedBranches,
  visibleWorktreePaths,
  withAheadBehind,
  withPr,
  worktreeCheckDecoration,
  worktreeContextValue,
  worktreeDescription,
  worktreeLabel,
  worktreeNodes,
  worktreeTooltip,
} from "./tree";
import {
  RowColorMap,
  RowIcon,
  repoRowIcon,
  rowColorTag,
  sameRowColors,
  worktreeRowIcon,
} from "./icons";
import { worktreeResourceUri } from "./decorations";

/**
 * Fetches ahead/behind divergence for a batch of worktree paths on demand — the
 * `ahead-behind` op (#1306). Injected so the provider stays `vscode`-testable and
 * decoupled from the socket. Resolves to `undefined` when the daemon is
 * unreachable or has no such op, in which case the tree renders without sync — and
 * the failure is not cached, so the next refresh retries (#2120). The provider
 * only calls it for worktrees whose answer may have moved; see
 * {@link AheadBehindMemo}.
 */
export type AheadBehindFetcher = AheadBehindBatchFetcher;

/**
 * Resolves the open PR badge for each of a GitHub repo's branches on demand — one
 * `gh pr list` per repo-expand (#1296). Injected like {@link AheadBehindFetcher}
 * so the provider stays `vscode`-testable; the returned map is keyed by branch
 * name (only branches with an open PR appear). Invoked only for branches the
 * daemon left unresolved (`needsPrFallback`, #1370). Resolves to an empty map —
 * so the tree renders without PR badges — when `gh` is missing, the feature is
 * disabled, or the lookup fails.
 */
export type PrBadgeFetcher = (
  repo: TreeGithubIdentity,
  branches: string[],
) => Promise<Record<string, PrBadge>>;

/**
 * The command every worktree item fires on a (single) click. The TreeView API
 * has **no** double-click event, so this command is the hook the manual
 * double-click timer in `extension.ts` uses to distinguish select from open.
 */
export const ITEM_CLICKED_COMMAND = "omniDevWorktrees.itemClicked";

/** How often the Recently Closed group re-renders its relative ages (#2211). */
const CLOSED_AGE_REFRESH_MS = 60_000;

/**
 * Maps a pure {@link RowIcon} onto the editor's icon type — the whole of this file's
 * share of the row-icon logic (#1428).
 *
 * The one-argument form is used deliberately when there is no colour, rather than
 * passing an explicit `undefined`, so an uncoloured row constructs exactly the value it
 * did before the colour tags existed.
 */
function themeIcon(icon: RowIcon): vscode.ThemeIcon {
  return icon.colorId
    ? new vscode.ThemeIcon(icon.iconId, new vscode.ThemeColor(icon.colorId))
    : new vscode.ThemeIcon(icon.iconId);
}

/** Serves the repo→worktree tree from the latest daemon `tree` snapshot. */
export class WorktreesTreeDataProvider implements vscode.TreeDataProvider<TreeElement> {
  private repos: TreeRepoPayload[] = [];
  /** The Recently Closed entries (#2211), newest first; the group row shows when non-empty. */
  private recentlyClosed: ClosedWorktreePayload[] = [];
  /** Whether worktrees with no open window are shown; false hides them. */
  private showClosed = true;
  /**
   * Whether the global `showPullRequests` setting is on (#1376). When off it is
   * the master switch: the repo icon renders neutral/gray regardless of the
   * per-repo `polling_enabled` flag (badges are already stripped upstream by
   * `visibleRepos`). Defaults on.
   */
  private showPr = true;
  /**
   * Per-worktree Claude session tallies (#1406), keyed by worktree path. Unlike
   * ahead/behind and PR badges — which `getChildren` pulls lazily — session state
   * rides its own daemon op on a poll, so it is *pushed* in here and folded into
   * the item at render time.
   */
  private sessionTallies: SessionTallyMap = {};
  /**
   * Per-worktree Claude model-family sets (#1448), keyed like
   * {@link sessionTallies} and pushed in the same way, on the same cadence.
   */
  private modelFamilies: ModelFamilyMap = {};
  /**
   * Per-row icon colour tags (#1428), keyed by {@link nodeId} — the user's
   * `omniDevWorktrees.rowColors` setting. Like the session tallies this is *pushed* in
   * rather than pulled: it lives in VS Code settings, not in the daemon snapshot, so
   * `extension.ts` reads it and forwards it on every configuration change.
   */
  private rowColors: RowColorMap = {};
  /**
   * Remembers the `ahead-behind` answers, so a refresh that moves nothing a row
   * shows issues no request (#2120). `undefined` when no fetcher was injected.
   */
  private readonly aheadBehind?: AheadBehindMemo;
  /**
   * Re-renders the Recently Closed group so its relative ages ("5m ago") do not
   * freeze while the daemon has nothing to push. Running only while the list is
   * non-empty, and firing for the group alone: a whole-tree refresh would re-run
   * `getChildren` for every expanded repo and re-trigger its lazy fetches.
   */
  private ageTimer: ReturnType<typeof setInterval> | undefined;

  private readonly emitter = new vscode.EventEmitter<TreeElement | undefined | null | void>();
  readonly onDidChangeTreeData = this.emitter.event;

  /**
   * @param windowKey this window's own registry key, so the leaf whose
   * `window_key` matches can be marked distinctly from worktrees open elsewhere.
   * @param fetchAheadBehind fetches per-worktree divergence on demand (#1306); when
   * omitted (tests, or the daemon lacking the op) the tree renders without sync.
   * @param fetchPrBadges resolves per-branch PR badges on demand (#1296); when
   * omitted (tests, or the feature disabled) the tree renders without PR badges.
   */
  constructor(
    private readonly windowKey?: string,
    fetchAheadBehind?: AheadBehindFetcher,
    private readonly fetchPrBadges?: PrBadgeFetcher,
  ) {
    this.aheadBehind = fetchAheadBehind ? new AheadBehindMemo(fetchAheadBehind) : undefined;
  }

  /**
   * Replaces the snapshot and refreshes the whole tree. `closed` is the snapshot's
   * Recently Closed list (#2211); an older daemon sends none, which is `[]`.
   *
   * Takes the list *with* the repos rather than through a second setter so one
   * snapshot is one refresh: a separate setter would fire again for the same frame
   * whenever the closed list moved. {@link setRecentlyClosed} is the entry point for
   * a caller holding only the list.
   */
  update(repos: TreeRepoPayload[], closed: ClosedWorktreePayload[] = this.recentlyClosed): void {
    this.repos = repos;
    this.recentlyClosed = closed;
    this.syncAgeTimer();
    // Every worktree in the snapshot, not just the visible ones: toggling
    // show-closed must not throw away answers it will need again.
    this.aheadBehind?.prune(repos.flatMap((repo) => repo.worktrees.map((wt) => wt.path)));
    this.emitter.fire(undefined);
  }

  /**
   * Replaces the Recently Closed list on its own, returning whether it changed and
   * refreshing only then — the same no-op-when-unchanged rule as
   * {@link setSessionState}, since a refresh re-runs {@link getChildren} for every
   * expanded repo. Compared by serialised value, so a daemon re-sending an equal
   * list costs nothing.
   */
  setRecentlyClosed(closed: ClosedWorktreePayload[]): boolean {
    if (sameClosed(this.recentlyClosed, closed)) {
      return false;
    }
    this.recentlyClosed = closed;
    this.emitter.fire(undefined);
    this.syncAgeTimer();
    return true;
  }

  /** Starts or stops the {@link ageTimer} to match whether any entry is listed. */
  private syncAgeTimer(): void {
    if (this.recentlyClosed.length === 0) {
      if (this.ageTimer !== undefined) {
        clearInterval(this.ageTimer);
        this.ageTimer = undefined;
      }
      return;
    }
    if (this.ageTimer === undefined) {
      this.ageTimer = setInterval(() => {
        this.emitter.fire({ kind: "closedGroup", closed: this.recentlyClosed });
      }, CLOSED_AGE_REFRESH_MS);
    }
  }

  /**
   * Sets whether worktrees with no open window are shown, then refreshes the
   * tree so the new filter applies. A no-op change still re-fires harmlessly.
   */
  setShowClosed(showClosed: boolean): void {
    this.showClosed = showClosed;
    this.emitter.fire(undefined);
  }

  /**
   * Sets whether the global `showPullRequests` master is on (#1376), then
   * refreshes so repo icons recolour: with it off, an enabled repo's icon greys
   * rather than showing green (badges are stripped separately by `visibleRepos`).
   */
  setShowPullRequests(showPr: boolean): void {
    this.showPr = showPr;
    this.emitter.fire(undefined);
  }

  /**
   * Replaces the per-row icon colour tags (#1428), returning whether anything actually
   * changed.
   *
   * Refreshing only on a real change matters more here than anywhere else: user-scope
   * settings changes fire `onDidChangeConfiguration` in **every** open window, and a
   * refresh re-runs {@link getChildren} and the whole tree rebuild with it (see
   * {@link setSessionState}). The ahead/behind requests that used to make this
   * N windows × one `ahead-behind` op per expanded repo are now absorbed by the
   * {@link AheadBehindMemo} (#2120), but the guard still saves every window the
   * rebuild and the PR-badge fallback.
   */
  setRowColors(colors: RowColorMap): boolean {
    if (sameRowColors(this.rowColors, colors)) {
      return false;
    }
    this.rowColors = colors;
    this.emitter.fire(undefined);
    return true;
  }

  /**
   * Replaces the per-worktree Claude session tallies (#1406) and their
   * model-family sets (#1448) together, returning whether anything actually
   * changed. Combined into one setter so the two never cause a double fire in
   * the same tick — a session's bucket and its newly-learned model commonly
   * change together, and an independent pair of setters would each re-fire
   * `onDidChangeTreeData` for that one poll.
   *
   * Refreshing only on a real change keeps an unchanged poll a complete no-op:
   * firing `onDidChangeTreeData` re-runs {@link getChildren}, which rebuilds every
   * expanded repo and re-evaluates the PR-badge fallback. The lazy ahead/behind fetch
   * no longer rides that (it is memoized by what it depends on, #2120), but a
   * ~10s cue poll should still not become a tree rebuild of its own.
   */
  setSessionState(tallies: SessionTallyMap, models: ModelFamilyMap): boolean {
    const changed =
      !sameTallies(this.sessionTallies, tallies) || !sameModelFamilies(this.modelFamilies, models);
    if (!changed) {
      return false;
    }
    this.sessionTallies = tallies;
    this.modelFamilies = models;
    this.emitter.fire(undefined);
    return true;
  }

  async getChildren(element?: TreeElement): Promise<TreeElement[]> {
    if (!element) {
      return rootNodes(this.repos, this.recentlyClosed);
    }
    if (element.kind === "closedGroup") {
      return closedChildNodes(element.closed);
    }
    if (element.kind !== "repo") {
      return [];
    }
    const nodes = worktreeNodes(element.repo, this.showClosed);
    if (nodes.length === 0) {
      return nodes;
    }
    // Lazily enrich this repo's worktrees on expand — the streamed snapshot does
    // not carry ahead/behind (#1306), which is fetched via the daemon's
    // `ahead-behind` op. Best-effort: a failure leaves just that indicator off.
    //
    // This runs on *every* refresh, and most refreshes (a CI verdict, a session
    // transition, a colour edit, another window opening a worktree) cannot change a
    // count, so the answers are memoized by what they are computed from and only
    // the worktrees whose inputs moved are re-asked (#2120).
    //
    // PR badges are **not** in the same boat since #1337. The daemon resolves them
    // and pushes them on the snapshot, kept live by its own poller — which is the
    // whole point, because a re-render only happens when the *worktree* state
    // changes, and CI moves without it. A current daemon marks every checked
    // branch with either a badge (`pr`) or the explicit negative (`pr_none`,
    // #1370), so the fallback list is empty and no `gh` runs at all; only a
    // pre-#1370 daemon — or a branch it has not yet resolved — lands here.
    //
    // The fallback is now gated on `repoPollingEnabled` (#1389): a repo the daemon
    // is **not** polling deliberately resolves no badges, and the extension must
    // honour that opt-out rather than quietly shelling `gh pr list` per window for
    // it — the very per-window burn #1370/#1389 target. So a not-polled repo issues
    // zero `gh` from here too; only a *polled* repo's transient pre-first-poll
    // window still falls back (and that goes through the shared daemon op).
    const targets: AheadBehindTarget[] = nodes.flatMap((n) =>
      n.kind === "worktree" ? [{ wt: n.wt, repo: n.repo }] : [],
    );
    const unbadged = unbadgedBranches(nodes);
    const abPromise: Promise<AheadBehindMap> = this.aheadBehind
      ? this.aheadBehind.resolve(targets)
      : Promise.resolve({});
    const prPromise: Promise<Record<string, PrBadge>> =
      this.fetchPrBadges &&
      element.repo.github &&
      repoPollingEnabled(element.repo) &&
      unbadged.length > 0
        ? this.fetchPrBadges(element.repo.github, unbadged).catch(() => ({}))
        : Promise.resolve({});
    const [ab, prBadges] = await Promise.all([abPromise, prPromise]);
    return nodes.map((n) => {
      if (n.kind !== "worktree") {
        return n;
      }
      // `withPr(wt, undefined)` is a no-op, so a daemon-supplied badge — or its
      // explicit `pr_none` negative — is never overwritten by the (checks-less)
      // fallback. Guarded by the same predicate as the collection above.
      const wt = withPr(
        withAheadBehind(n.wt, ab[n.wt.path]),
        n.wt.branch && needsPrFallback(n.wt) ? prBadges[n.wt.branch] : undefined,
      );
      return { ...n, wt };
    });
  }

  getTreeItem(node: TreeElement): vscode.TreeItem {
    if (node.kind === "closedGroup") {
      // Collapsed by default: it is a way back, not part of the live working set.
      const item = new vscode.TreeItem(
        CLOSED_GROUP_LABEL,
        vscode.TreeItemCollapsibleState.Collapsed,
      );
      item.id = elementId(node);
      item.iconPath = new vscode.ThemeIcon("history");
      item.contextValue = CLOSED_GROUP_CONTEXT;
      item.description = closedGroupDescription(node.closed.length);
      return item;
    }
    if (node.kind === "closed") {
      const item = new vscode.TreeItem(
        closedLabel(node.entry),
        vscode.TreeItemCollapsibleState.None,
      );
      item.id = elementId(node);
      item.iconPath = new vscode.ThemeIcon(closedIconId(node.entry));
      item.contextValue = closedContextValue(node.entry);
      item.description = closedDescription(node.entry, new Date());
      item.tooltip = closedTooltip(node.entry);
      // Routed through the same double-click timer as a live row, so a stray click
      // while selecting (or the first click of a multi-select) reopens nothing.
      item.command = {
        command: ITEM_CLICKED_COMMAND,
        title: "Reopen Closed Worktree",
        arguments: [node],
      };
      return item;
    }
    if (node.kind === "repo") {
      const item = new vscode.TreeItem(
        repoLabel(node.repo),
        vscode.TreeItemCollapsibleState.Expanded,
      );
      item.id = elementId(node);
      item.iconPath = themeIcon(
        repoRowIcon(node.repo, this.showPr, rowColorTag(this.rowColors, node)),
      );
      // Encodes GitHub identity (gates "Open Pull Request…") and poll state (gates
      // "Enable/Disable PR Polling"); the plain `repo` value is unchanged for
      // non-GitHub repos.
      item.contextValue = repoContextValue(node.repo);
      item.tooltip = node.repo.root;
      // The rolled-up Claude model-family marker (#1448): the union of every
      // *visible* child worktree's families — mirrors the `showClosed` filter
      // `getChildren` applies via `worktreeNodes` (both build on the same
      // `visibleWorktrees` predicate in tree.ts, so the two can't drift apart),
      // so the marker never names a family attributable only to a worktree
      // hidden by "hide closed worktrees". Omitted entirely when the repo runs
      // no sessions, matching the worktree row's own all-or-nothing description.
      const marker = formatModelMarker(
        unionModelFamilies(
          visibleWorktreePaths(node.repo, this.showClosed),
          this.modelFamilies,
        ),
      );
      const repoDesc = repoDescription(marker);
      if (repoDesc) {
        item.description = repoDesc;
      }
      return item;
    }

    const item = new vscode.TreeItem(
      worktreeLabel(node.wt),
      vscode.TreeItemCollapsibleState.None,
    );
    const sessions = this.sessionTallies[node.wt.path];
    item.id = elementId(node);
    const sessionsSegment = [
      sessionGlyphs(sessions),
      formatModelMarker(this.modelFamilies[node.wt.path]),
    ]
      .filter(Boolean)
      .join(" ");
    item.description = worktreeDescription(node.wt, sessionsSegment);
    item.tooltip = worktreeTooltip(
      node.wt,
      node.repo,
      this.windowKey,
      sessionTooltipLine(sessions),
    );
    item.contextValue = worktreeContextValue(node.wt, this.windowKey, !!node.repo.github);
    // A file decoration carries the PR-check badge (#1324) and combined
    // check/session colour (#1406). Rows with either state get a
    // custom-scheme `resourceUri` keyed by both, which the
    // `WorktreeDecorationProvider` paints (and which re-decorates on its own when
    // the state — and so the URI — changes). Rows with neither get none.
    // `item.id` still keys row identity.
    const pr = node.wt.pr;
    const checks = pr && worktreeCheckDecoration(node.wt) ? pr.checks : "none";
    if (checks !== "none" || sessionDecoration(sessions)) {
      item.resourceUri = worktreeResourceUri(node.wt.path, checks, sessions);
    }
    // The rebase cue, the user's colour tag, and the three-way open badge — see
    // `icons.ts` for the precedence between them.
    item.iconPath = themeIcon(
      worktreeRowIcon(node.wt, this.windowKey, rowColorTag(this.rowColors, node)),
    );
    item.command = {
      command: ITEM_CLICKED_COMMAND,
      title: "Open Worktree",
      arguments: [node],
    };
    return item;
  }

  dispose(): void {
    if (this.ageTimer !== undefined) {
      clearInterval(this.ageTimer);
    }
    this.emitter.dispose();
  }
}
