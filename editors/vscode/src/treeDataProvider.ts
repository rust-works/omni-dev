// The `vscode`-facing tree data provider. It is a thin adapter: all model and
// formatting logic lives in the `vscode`-free `tree.ts` (which is unit-tested);
// this file only maps a `Node` onto a `vscode.TreeItem` (icons, collapsible
// state, the per-click command) and re-fires the tree when a snapshot arrives.

import * as vscode from "vscode";

import {
  AheadBehindMap,
  Node,
  TreeRepoPayload,
  isCurrentWindow,
  nodeId,
  repoContextValue,
  repoLabel,
  repoPollingEnabled,
  reposToNodes,
  withAheadBehind,
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
   * The daemon's active PR-status source (#1384). Recolours the GitHub repo icon:
   * in `webhook` mode every owned repo is backed the same way, so the green/gray
   * poll distinction is meaningless and the icon goes **blue** instead. Defaults
   * `poll` until the first snapshot drives {@link setPrSource}.
   */
  private prSource: "poll" | "webhook" = "poll";
  private readonly emitter = new vscode.EventEmitter<Node | undefined | null | void>();
  readonly onDidChangeTreeData = this.emitter.event;

  /**
   * @param windowKey this window's own registry key, so the leaf whose
   * `window_key` matches can be marked distinctly from worktrees open elsewhere.
   * @param fetchAheadBehind fetches per-worktree divergence on demand (#1306); when
   * omitted (tests, or the daemon lacking the op) the tree renders without sync.
   */
  constructor(
    private readonly windowKey?: string,
    private readonly fetchAheadBehind?: AheadBehindFetcher,
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
   * Sets the active PR-status source (#1384), then refreshes so GitHub repo icons
   * recolour — blue in `webhook` mode, else the poll-state green/gray.
   */
  setPrSource(source: "poll" | "webhook"): void {
    this.prSource = source;
    this.emitter.fire(undefined);
  }

  /**
   * The icon for a GitHub repo node. Master switch off → neutral. `webhook`
   * mode → blue (every owned repo is backed the same way, so the poll distinction
   * does not apply). `poll` mode → green while the daemon is polling this repo,
   * else neutral.
   */
  private githubRepoIcon(repo: TreeRepoPayload): vscode.ThemeIcon {
    if (!this.showPr) return new vscode.ThemeIcon("github");
    if (this.prSource === "webhook") {
      return new vscode.ThemeIcon("github", new vscode.ThemeColor("charts.blue"));
    }
    return repoPollingEnabled(repo)
      ? new vscode.ThemeIcon("github", new vscode.ThemeColor("charts.green"))
      : new vscode.ThemeIcon("github");
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
    // PR badges are **not** fetched here: the daemon is the sole resolver
    // (#1337/#1384) and pushes them on the snapshot as `pr`/`pr_none`, shared by
    // every window. The extension no longer runs its own `gh pr list` — a branch
    // the daemon has not resolved yet just shows no badge until the daemon does.
    const paths = nodes.flatMap((n) => (n.kind === "worktree" ? [n.wt.path] : []));
    const ab: AheadBehindMap = this.fetchAheadBehind
      ? await this.fetchAheadBehind(paths).catch(() => ({}))
      : {};
    return nodes.map((n) => {
      if (n.kind !== "worktree") {
        return n;
      }
      return { ...n, wt: withAheadBehind(n.wt, ab[n.wt.path]) };
    });
  }

  getTreeItem(node: Node): vscode.TreeItem {
    if (node.kind === "repo") {
      const item = new vscode.TreeItem(
        repoLabel(node.repo),
        vscode.TreeItemCollapsibleState.Expanded,
      );
      item.id = nodeId(node);
      // The GitHub repo icon reflects the live PR source: blue in `webhook` mode
      // (#1384), else the #1376 poll state — green when the daemon is polling this
      // repo and the master is on, otherwise gray. A non-GitHub repo keeps `repo`.
      item.iconPath = node.repo.github
        ? this.githubRepoIcon(node.repo)
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
    item.id = nodeId(node);
    item.description = worktreeDescription(node.wt);
    item.tooltip = worktreeTooltip(node.wt, node.repo, this.windowKey);
    item.contextValue = worktreeContextValue(node.wt, this.windowKey, !!node.repo.github);
    // A colored ✓/✗/● file decoration carries the PR CI-check state (#1324). Rows
    // whose PR has a pass/fail/pending verdict get a custom-scheme `resourceUri`
    // keyed by that state, which the `WorktreeDecorationProvider` paints (and which
    // re-decorates on its own when the state — and so the URI — changes). Rows with
    // no PR, or a PR with no checks, get none. `item.id` still keys row identity.
    const pr = node.wt.pr;
    if (pr && worktreeCheckDecoration(node.wt)) {
      item.resourceUri = worktreeResourceUri(node.wt.path, pr.checks);
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
