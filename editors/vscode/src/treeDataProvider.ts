// The `vscode`-facing tree data provider. It is a thin adapter: all model and
// formatting logic lives in the `vscode`-free `tree.ts` (which is unit-tested);
// this file only maps a `Node` onto a `vscode.TreeItem` (icons, collapsible
// state, the per-click command) and re-fires the tree when a snapshot arrives.

import * as vscode from "vscode";

import {
  Node,
  TreeRepoPayload,
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
  private readonly emitter = new vscode.EventEmitter<Node | undefined | null | void>();
  readonly onDidChangeTreeData = this.emitter.event;

  /** Replaces the snapshot and refreshes the whole tree. */
  update(repos: TreeRepoPayload[]): void {
    this.repos = repos;
    this.emitter.fire(undefined);
  }

  getChildren(element?: Node): Node[] {
    if (!element) {
      return reposToNodes(this.repos);
    }
    return element.kind === "repo" ? worktreeNodes(element.repo) : [];
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
    item.tooltip = worktreeTooltip(node.wt, node.repo);
    item.contextValue = worktreeContextValue(node.wt);
    // The open badge: a green dot for a worktree with a live window, else the
    // plain branch glyph.
    item.iconPath = node.wt.open
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
