// The `vscode`-facing "Open Pull Request…" commands. They are thin adapters: the
// discovery, `gh` arg building, parsing, URI building, and quick-pick formatting
// all live in the `vscode`-free, unit-tested `github.ts`; this file only wires
// those onto the editor — a progress notification, the empty/single/multi-select
// branches, and the open step.
//
// Both commands share one selection pipeline (`selectPullRequests`) and differ
// only in what they hand `openExternal`: the in-editor one opens each PR as a tab
// through the GitHub Pull Requests extension's URI handler, the browser one opens
// the PR's `github.com` page in the OS default browser.

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
 * Code** (never a browser) — see `openPullRequestInBrowser` for the sibling that
 * opens `github.com` instead.
 */
export async function openPullRequest(node?: Node): Promise<void> {
  const selected = await selectPullRequests(node);
  if (!selected) {
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

/**
 * Opens the pull request(s) for a repo or worktree node **in the OS default
 * browser** — the same selection flow as `openPullRequest`, handing `openExternal`
 * the PR's plain `github.com` web URL rather than the extension URI. Needs no
 * GitHub Pull Requests extension, so there is nothing to warn about or fall back
 * to.
 */
export async function openPullRequestInBrowser(node?: Node): Promise<void> {
  const selected = await selectPullRequests(node);
  if (!selected) {
    return;
  }

  for (const pr of selected) {
    await vscode.env.openExternal(vscode.Uri.parse(pr.url));
  }
}

/**
 * The pull request(s) to open for a node, or `undefined` when there is nothing to
 * open. Discovers via `gh` under a progress notification, then: no PR → a friendly
 * info message; one PR → it; several → a multi-select quick-pick. A node with no
 * GitHub identity is ignored (the menu is gated so it can only ever fire on a
 * `github` item), as is a cancelled or empty pick.
 */
async function selectPullRequests(node?: Node): Promise<PullRequest[] | undefined> {
  if (!node) {
    return undefined;
  }
  const scope = prScopeForNode(node);
  if (!scope) {
    return undefined;
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
    return undefined;
  }

  if (prs.length === 0) {
    void vscode.window.showInformationMessage(
      `No open pull request for ${scopeLabel(scope)}.`,
    );
    return undefined;
  }

  const selected = prs.length === 1 ? prs : await pickPullRequests(prs, scope);
  return selected && selected.length > 0 ? selected : undefined;
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
