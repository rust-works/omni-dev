// The `vscode`-facing tree data provider. It is a thin adapter: all model and
// formatting logic lives in the `vscode`-free `tree.ts` (which is unit-tested);
// this file only maps a `Node` onto a `vscode.TreeItem` (icons, collapsible
// state, the per-click command) and re-fires the tree when a snapshot arrives.

import * as vscode from "vscode";

import {
  AheadBehindMap,
  Node,
  PrBadge,
  TreeGithubIdentity,
  TreeRepoPayload,
  isCurrentWindow,
  nodeId,
  repoLabel,
  reposToNodes,
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
 * name (only branches with an open PR appear). Resolves to an empty map — so the
 * tree renders without PR badges — when `gh` is missing, the feature is disabled,
 * or the lookup fails.
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
    // Lazily enrich this repo's worktrees on expand — the streamed snapshot no
    // longer carries per-worktree extras (#1306). Two independent, best-effort
    // lookups run in parallel: ahead/behind via the daemon `ahead-behind` op
    // (#1306), and — when the repo is on GitHub — PR badges via `gh` (#1296). A
    // re-render (a new snapshot) re-runs this and re-fetches, keeping both fresh;
    // a failure of either leaves just that indicator off.
    const paths = nodes.flatMap((n) => (n.kind === "worktree" ? [n.wt.path] : []));
    const branches = nodes.flatMap((n) =>
      n.kind === "worktree" && n.wt.branch ? [n.wt.branch] : [],
    );
    const abPromise: Promise<AheadBehindMap> = this.fetchAheadBehind
      ? this.fetchAheadBehind(paths).catch(() => ({}))
      : Promise.resolve({});
    const prPromise: Promise<Record<string, PrBadge>> =
      this.fetchPrBadges && element.repo.github && branches.length > 0
        ? this.fetchPrBadges(element.repo.github, branches).catch(() => ({}))
        : Promise.resolve({});
    const [ab, prBadges] = await Promise.all([abPromise, prPromise]);
    return nodes.map((n) => {
      if (n.kind !== "worktree") {
        return n;
      }
      const wt = withPr(
        withAheadBehind(n.wt, ab[n.wt.path]),
        n.wt.branch ? prBadges[n.wt.branch] : undefined,
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
      item.iconPath = new vscode.ThemeIcon(node.repo.github ? "github" : "repo");
      // `.github` gates the "Open Pull Request…" menu; the plain `repo` value is
      // unchanged for non-GitHub repos.
      item.contextValue = node.repo.github ? "repo.github" : "repo";
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
