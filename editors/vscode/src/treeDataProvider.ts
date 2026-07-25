// The `vscode`-facing tree data provider. It is a thin adapter: all model and
// formatting logic lives in the `vscode`-free `tree.ts` (which is unit-tested);
// this file only maps a `Node` onto a `vscode.TreeItem` (icons, collapsible
// state, the per-click command) and re-fires the tree when a snapshot arrives.

import * as vscode from "vscode";

import {
  SessionTallyMap,
  sameTallies,
  sessionDecoration,
  sessionGlyphs,
  sessionTooltipLine,
} from "./sessionCounts";
import {
  AheadBehindMap,
  Node,
  PrBadge,
  TreeGithubIdentity,
  TreeRepoPayload,
  isCurrentWindow,
  needsPrFallback,
  nodeId,
  repoContextValue,
  repoLabel,
  repoPollingEnabled,
  reposToNodes,
  unbadgedBranches,
  withAheadBehind,
  withPr,
  worktreeCheckDecoration,
  worktreeContextValue,
  worktreeDescription,
  worktreeLabel,
  worktreeNodes,
  worktreeTooltip,
} from "./tree";
import { worktreeResourceUri } from "./decorations";

/**
 * Fetches ahead/behind divergence for a batch of worktree paths on demand — the
 * `ahead-behind` op (#1306). Injected so the provider stays `vscode`-testable and
 * decoupled from the socket. Resolves to an empty map when the daemon is
 * unreachable or has no such op, in which case the tree renders without sync.
 */
export type AheadBehindFetcher = (paths: string[]) => Promise<AheadBehindMap>;

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

/** Serves the repo→worktree tree from the latest daemon `tree` snapshot. */
export class WorktreesTreeDataProvider implements vscode.TreeDataProvider<Node> {
  private repos: TreeRepoPayload[] = [];
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
  private readonly emitter = new vscode.EventEmitter<Node | undefined | null | void>();
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
    private readonly fetchAheadBehind?: AheadBehindFetcher,
    private readonly fetchPrBadges?: PrBadgeFetcher,
  ) {}

  /** Replaces the snapshot and refreshes the whole tree. */
  update(repos: TreeRepoPayload[]): void {
    this.repos = repos;
    this.emitter.fire(undefined);
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
   * Replaces the per-worktree Claude session tallies (#1406), returning whether
   * anything actually changed.
   *
   * Refreshing only on a real change is load-bearing, not an optimization:
   * firing `onDidChangeTreeData` re-runs {@link getChildren}, which re-triggers
   * the lazy ahead/behind and PR-badge fetches. An unchanged poll must therefore
   * be a complete no-op, or a ~10s cue poll would turn those into a poll of
   * their own.
   */
  setSessionTallies(tallies: SessionTallyMap): boolean {
    if (sameTallies(this.sessionTallies, tallies)) {
      return false;
    }
    this.sessionTallies = tallies;
    this.emitter.fire(undefined);
    return true;
  }

  async getChildren(element?: Node): Promise<Node[]> {
    if (!element) {
      return reposToNodes(this.repos);
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
    const paths = nodes.flatMap((n) => (n.kind === "worktree" ? [n.wt.path] : []));
    const unbadged = unbadgedBranches(nodes);
    const abPromise: Promise<AheadBehindMap> = this.fetchAheadBehind
      ? this.fetchAheadBehind(paths).catch(() => ({}))
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

  getTreeItem(node: Node): vscode.TreeItem {
    if (node.kind === "repo") {
      const item = new vscode.TreeItem(
        repoLabel(node.repo),
        vscode.TreeItemCollapsibleState.Expanded,
      );
      item.id = nodeId(node);
      // The GitHub repo icon reflects PR-poll state (#1376): green when the
      // daemon is polling this repo *and* the global master is on, else the
      // default gray. A non-GitHub repo keeps the plain `repo` glyph.
      item.iconPath = node.repo.github
        ? this.showPr && repoPollingEnabled(node.repo)
          ? new vscode.ThemeIcon("github", new vscode.ThemeColor("charts.green"))
          : new vscode.ThemeIcon("github")
        : new vscode.ThemeIcon("repo");
      // Encodes GitHub identity (gates "Open Pull Request…") and poll state (gates
      // "Enable/Disable PR Polling"); the plain `repo` value is unchanged for
      // non-GitHub repos.
      item.contextValue = repoContextValue(node.repo);
      item.tooltip = node.repo.root;
      return item;
    }

    const item = new vscode.TreeItem(
      worktreeLabel(node.wt),
      vscode.TreeItemCollapsibleState.None,
    );
    const sessions = this.sessionTallies[node.wt.path];
    item.id = nodeId(node);
    item.description = worktreeDescription(node.wt, sessionGlyphs(sessions));
    item.tooltip = worktreeTooltip(
      node.wt,
      node.repo,
      this.windowKey,
      sessionTooltipLine(sessions),
    );
    item.contextValue = worktreeContextValue(node.wt, this.windowKey, !!node.repo.github);
    // A colored file decoration carries the row's Claude session cue (#1406) or,
    // failing that, its PR CI-check state (#1324). Rows with either get a
    // custom-scheme `resourceUri` keyed by both, which the
    // `WorktreeDecorationProvider` paints (and which re-decorates on its own when
    // the state — and so the URI — changes). Rows with neither get none.
    // `item.id` still keys row identity.
    const pr = node.wt.pr;
    const checks = pr && worktreeCheckDecoration(node.wt) ? pr.checks : "none";
    if (checks !== "none" || sessionDecoration(sessions)) {
      item.resourceUri = worktreeResourceUri(node.wt.path, checks, sessions);
    }
    // The open badge, three-way: a blue tick for the worktree open in *this*
    // window, a green dot for one open in another window, else the plain branch
    // glyph for a worktree with no live window.
    item.iconPath = isCurrentWindow(node.wt, this.windowKey)
      ? new vscode.ThemeIcon("check", new vscode.ThemeColor("charts.blue"))
      : node.wt.open
        ? new vscode.ThemeIcon("circle-filled", new vscode.ThemeColor("charts.green"))
        : new vscode.ThemeIcon("git-branch");
    item.command = {
      command: ITEM_CLICKED_COMMAND,
      title: "Open Worktree",
      arguments: [node],
    };
    return item;
  }

  dispose(): void {
    this.emitter.dispose();
  }
}
