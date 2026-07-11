// The `vscode`-facing tree data provider. It is a thin adapter: all model and
// formatting logic lives in the `vscode`-free `tree.ts` (which is unit-tested);
// this file only maps a `Node` onto a `vscode.TreeItem` (icons, collapsible
// state, the per-click command) and re-fires the tree when a snapshot arrives.

import * as vscode from "vscode";

import {
  Node,
  TreeRepoPayload,
  isCurrentWindow,
  nodeId,
  repoLabel,
  reposToNodes,
  worktreeContextValue,
  worktreeDescription,
  worktreeLabel,
  worktreeNodes,
  worktreeTooltip,
} from "./tree";

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
   */
  constructor(private readonly windowKey?: string) {}

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

  getChildren(element?: Node): Node[] {
    if (!element) {
      return reposToNodes(this.repos);
    }
    return element.kind === "repo" ? worktreeNodes(element.repo, this.showClosed) : [];
  }

  getTreeItem(node: Node): vscode.TreeItem {
    if (node.kind === "repo") {
      const item = new vscode.TreeItem(
        repoLabel(node.repo),
        vscode.TreeItemCollapsibleState.Expanded,
      );
      item.id = nodeId(node);
      item.iconPath = new vscode.ThemeIcon(node.repo.github ? "github" : "repo");
      item.contextValue = "repo";
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
    item.contextValue = worktreeContextValue(node.wt, this.windowKey);
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
