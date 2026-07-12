// The `vscode`-facing "Open Pull Request…" command. It is a thin adapter: the
// discovery, `gh` arg building, parsing, URI building, and quick-pick formatting
// all live in the `vscode`-free, unit-tested `github.ts`; this file only wires
// those onto the editor — a progress notification, the empty/single/multi-select
// branches, and opening each chosen PR as a tab through the GitHub Pull Requests
// extension's URI handler.

import * as vscode from "vscode";

import { runGh } from "./gh";
import {
  PrScope,
  PullRequest,
  discoverPullRequests,
  prOverviewUri,
  prQuickPickDescription,
  prQuickPickLabel,
} from "./github";
import { Node } from "./tree";

/** The extension that renders a GitHub PR as a tab; without it the URI no-ops. */
const PR_EXTENSION_ID = "GitHub.vscode-pull-request-github";

/**
 * Opens the pull request(s) for a repo or worktree node **as a tab inside VS
 * Code** (never a browser). Discovers via `gh` under a progress notification,
 * then: no PR → a friendly info message; one PR → open it; several → a
 * multi-select quick-pick to open any or all. A node with no GitHub identity is
 * ignored (the menu is gated so it can only ever fire on a `github` item).
 */
export async function openPullRequest(node?: Node): Promise<void> {
  if (!node) {
    return;
  }
  const scope = prScopeForNode(node);
  if (!scope) {
    return;
  }

  let prs: PullRequest[];
  try {
    prs = await vscode.window.withProgress(
      {
        location: vscode.ProgressLocation.Notification,
        title: `Finding pull requests for ${scopeLabel(scope)}…`,
      },
      () => discoverPullRequests(scope, runGh),
    );
  } catch (err) {
    void vscode.window.showErrorMessage(
      `omni-dev: ${err instanceof Error ? err.message : String(err)}`,
    );
    return;
  }

  if (prs.length === 0) {
    void vscode.window.showInformationMessage(
      `No open pull request for ${scopeLabel(scope)}.`,
    );
    return;
  }

  const selected = prs.length === 1 ? prs : await pickPullRequests(prs, scope);
  if (!selected || selected.length === 0) {
    return;
  }

  // The GitHub PR extension is what turns the URI into a tab; without it the
  // `openExternal` call silently no-ops, so warn once (never fall back to a
  // browser) before opening any of the selected PRs.
  if (!isPrExtensionInstalled()) {
    await warnMissingPrExtension(selected);
    return;
  }

  for (const pr of selected) {
    const uri = vscode.Uri.parse(prOverviewUri(vscode.env.uriScheme, pr.url));
    await vscode.env.openExternal(uri);
  }
}

/** The scope to search for a node, or `undefined` when it has no GitHub identity. */
function prScopeForNode(node: Node): PrScope | undefined {
  const github = node.repo.github;
  if (!github) {
    return undefined;
  }
  if (node.kind === "repo") {
    return { kind: "repo", repo: github };
  }
  return { kind: "worktree", repo: github, branch: node.wt.branch };
}

/** A human label for the scope, used in progress and info messages. */
function scopeLabel(scope: PrScope): string {
  const repo = `${scope.repo.owner}/${scope.repo.name}`;
  return scope.kind === "worktree" && scope.branch ? `${repo}@${scope.branch}` : repo;
}

/** A multi-select quick-pick over the PRs; returns the chosen ones (or `undefined`). */
async function pickPullRequests(
  prs: PullRequest[],
  scope: PrScope,
): Promise<PullRequest[] | undefined> {
  const items = prs.map((pr) => ({
    label: prQuickPickLabel(pr),
    description: prQuickPickDescription(pr),
    pr,
  }));
  const picks = await vscode.window.showQuickPick(items, {
    canPickMany: true,
    placeHolder: `Select pull request(s) to open for ${scopeLabel(scope)}`,
  });
  return picks?.map((p) => p.pr);
}

/** Whether the GitHub Pull Requests extension is installed in this editor. */
function isPrExtensionInstalled(): boolean {
  return vscode.extensions.getExtension(PR_EXTENSION_ID) !== undefined;
}

/**
 * Warns once that the GitHub Pull Requests extension is required, offering to
 * **Install** it or **Copy PR URL** — the latter puts the selected PR URL(s) on
 * the clipboard so the user can open them however they like. We never silently
 * fall back to a browser.
 */
async function warnMissingPrExtension(prs: PullRequest[]): Promise<void> {
  const choice = await vscode.window.showWarningMessage(
    "The GitHub Pull Requests extension (GitHub.vscode-pull-request-github) is required " +
      "to open a pull request inside VS Code. Install it, or copy the PR URL.",
    "Install",
    "Copy PR URL",
  );
  if (choice === "Install") {
    await vscode.commands.executeCommand(
      "workbench.extensions.installExtension",
      PR_EXTENSION_ID,
    );
  } else if (choice === "Copy PR URL") {
    await vscode.env.clipboard.writeText(prs.map((pr) => pr.url).join("\n"));
    void vscode.window.showInformationMessage(
      prs.length === 1
        ? "PR URL copied to the clipboard."
        : `${prs.length} PR URLs copied to the clipboard.`,
    );
  }
}
