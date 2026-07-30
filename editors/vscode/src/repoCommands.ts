// The `vscode`-facing repository commands — currently just "Open GitHub
// Repository" (#1442). A thin adapter, like `prCommands.ts`: the interesting
// half (the URL, the selection→pages mapping, the dedupe) lives in the
// `vscode`-free, unit-tested `github.ts`, and this file does nothing but hand
// the result to `openExternal`.
//
// It is the cheapest action in the view. The URL is a pure function of the
// `github` identity already in the daemon's snapshot, so unlike every other
// GitHub action here there is no `gh`, no subprocess, no network, no daemon op —
// and so nothing to show progress for.

import * as vscode from "vscode";

import { repoWebUrlsForNodes } from "./github";
import { Node, selectionTargets } from "./tree";

/**
 * Opens the selected rows' repositories on `github.com` in the OS default
 * browser.
 *
 * A `view/item/context` command on a multi-select view (#1357), so VS Code
 * invokes it as `(clicked, selected[])` and {@link selectionTargets} resolves
 * which of the two to act on. The menu is gated to **repo** rows, but a
 * multi-selection can hold anything — `repoWebUrlsForNodes` maps a worktree row
 * onto its parent repository and dedupes, so a repo row selected with its own
 * worktrees still opens one page.
 *
 * Nothing is confirmed: unlike "Open Pull Request…", where a single repo node
 * contributes *every* open PR of its repository, the blast radius here is at
 * most one page per selected row — a count the user picked by selecting them.
 */
export async function openGithubRepository(
  clicked: Node | undefined,
  selection: Node[] | undefined,
): Promise<void> {
  for (const url of repoWebUrlsForNodes(selectionTargets(clicked, selection))) {
    await vscode.env.openExternal(vscode.Uri.parse(url));
  }
}
