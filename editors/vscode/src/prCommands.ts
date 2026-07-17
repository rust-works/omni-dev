// The `vscode`-facing "Open Pull Request…" commands. They are thin adapters: the
// node→scope mapping, discovery, `gh` arg building, parsing, URI building,
// quick-pick formatting, and both dedupes all live in the `vscode`-free,
// unit-tested `github.ts`; this file only wires those onto the editor — a progress
// notification, the empty/single/multi-select branches, and the open step.
//
// Both commands share one selection pipeline (`selectPullRequests`) and differ
// only in what they hand `openExternal`: the in-editor one opens each PR as a tab
// through the GitHub Pull Requests extension's URI handler, the browser one opens
// the PR's `github.com` page in the OS default browser.
//
// Both are `view/item/context` commands on a multi-select view (#1357), so VS Code
// invokes them as `(clicked, selected[])` and they fan out over the whole
// selection — see `selectionTargets` for why the two arguments are resolved rather
// than concatenated.

import * as vscode from "vscode";

import { runGh } from "./gh";
import {
  PrScope,
  PullRequest,
  dedupePullRequests,
  discoverPullRequests,
  prOverviewUri,
  prQuickPickDescription,
  prQuickPickLabel,
  prScopesForNodes,
  scopeLabel,
} from "./github";
import { Node, selectionTargets } from "./tree";

/** The extension that renders a GitHub PR as a tab; without it the URI no-ops. */
const PR_EXTENSION_ID = "GitHub.vscode-pull-request-github";

/**
 * How many pull requests a multi-select may open before it asks. A **repo** node
 * contributes *every* open PR of its repository, so two of them can mean dozens of
 * tabs — a count the user never picked. Above this, confirm; a picker would
 * contradict "open everything I selected", a confirm honors it.
 */
const BULK_OPEN_CONFIRM_THRESHOLD = 5;

/**
 * Opens the pull request(s) for the selected repo/worktree nodes **as tabs inside
 * VS Code** (never a browser) — see `openPullRequestInBrowser` for the sibling
 * that opens `github.com` instead.
 */
export async function openPullRequest(clicked?: Node, selection?: Node[]): Promise<void> {
  const selected = await selectPullRequests(selectionTargets(clicked, selection));
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
 * Opens the pull request(s) for the selected repo/worktree nodes **in the OS
 * default browser** — the same selection flow as `openPullRequest`, handing
 * `openExternal` the PR's plain `github.com` web URL rather than the extension
 * URI. Needs no GitHub Pull Requests extension, so there is nothing to warn about
 * or fall back to.
 */
export async function openPullRequestInBrowser(
  clicked?: Node,
  selection?: Node[],
): Promise<void> {
  const selected = await selectPullRequests(selectionTargets(clicked, selection));
  if (!selected) {
    return;
  }

  for (const pr of selected) {
    await vscode.env.openExternal(vscode.Uri.parse(pr.url));
  }
}

/**
 * The pull request(s) to open for a selection, or `undefined` when there is
 * nothing to open. Nodes with no GitHub identity are dropped (the menu is gated so
 * it can only ever fire on a `github` item, but a *mixed* selection can hold
 * anything), and identical scopes collapse to one `gh` call.
 *
 * Discovery is parallel under one progress notification. Then:
 *
 * - **One node** behaves exactly as it always has: no PR → a friendly info
 *   message; one PR → it; several → a multi-select quick-pick.
 * - **Several nodes** open everything found, with no picker — the selection *is*
 *   the answer, so asking again would contradict it — subject only to the
 *   {@link BULK_OPEN_CONFIRM_THRESHOLD} blast-radius confirm.
 */
async function selectPullRequests(targets: Node[]): Promise<PullRequest[] | undefined> {
  const scopes = prScopesForNodes(targets);
  if (scopes.length === 0) {
    return undefined;
  }

  const found = await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: discoveryTitle(scopes),
    },
    () => Promise.allSettled(scopes.map((scope) => discoverPullRequests(scope, runGh))),
  );

  // A failing `gh` takes down only its own scope: report the failures once and
  // open what did resolve. Only a total failure is fatal — which for a single
  // scope is exactly today's behavior.
  const failures = found.flatMap((result, i) =>
    result.status === "rejected" ? [{ scope: scopes[i], reason: result.reason }] : [],
  );
  if (failures.length === scopes.length) {
    const { reason } = failures[0];
    void vscode.window.showErrorMessage(
      `omni-dev: ${reason instanceof Error ? reason.message : String(reason)}`,
    );
    return undefined;
  }
  if (failures.length > 0) {
    void vscode.window.showWarningMessage(
      `omni-dev: could not find pull requests for ${failures
        .map((f) => scopeLabel(f.scope))
        .join(", ")}.`,
    );
  }

  const prs = dedupePullRequests(
    found.flatMap((result) => (result.status === "fulfilled" ? result.value : [])),
  );
  if (prs.length === 0) {
    void vscode.window.showInformationMessage(
      `No open pull request for ${scopes.map(scopeLabel).join(", ")}.`,
    );
    return undefined;
  }

  // A single node keeps the quick-pick; a multi-select opens everything it found.
  if (scopes.length === 1) {
    const picked = prs.length === 1 ? prs : await pickPullRequests(prs, scopes[0]);
    return picked && picked.length > 0 ? picked : undefined;
  }
  return (await confirmBulkOpen(prs)) ? prs : undefined;
}

/** The progress title: the one scope's label, or how many selections are searching. */
function discoveryTitle(scopes: PrScope[]): string {
  return scopes.length === 1
    ? `Finding pull requests for ${scopeLabel(scopes[0])}…`
    : `Finding pull requests for ${scopes.length} selected items…`;
}

/**
 * Guards a multi-select against a blast radius it never picked: at or below
 * {@link BULK_OPEN_CONFIRM_THRESHOLD} this opens straight away, above it asks
 * once for the whole batch.
 */
async function confirmBulkOpen(prs: PullRequest[]): Promise<boolean> {
  if (prs.length <= BULK_OPEN_CONFIRM_THRESHOLD) {
    return true;
  }
  const choice = await vscode.window.showWarningMessage(
    `Open ${prs.length} pull requests?`,
    {
      modal: true,
      detail: prs.map((pr) => `• ${prQuickPickLabel(pr)}`).join("\n"),
    },
    "Open All",
  );
  return choice === "Open All";
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
