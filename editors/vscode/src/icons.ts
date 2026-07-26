// The pure row-icon layer for the Worktrees tree: which codicon a repo or worktree row
// renders, and in which colour. Nothing here imports `vscode`, so the precedence rule
// below is unit-tested under a plain Node process — the same split `tree.ts` (formatters)
// and `sessionCounts.ts` (session cues) already use, with `treeDataProvider.ts` reduced
// to mapping a {@link RowIcon} onto a `vscode.ThemeIcon`.

import {
  TreeRepoPayload,
  TreeWorktreePayload,
  isCurrentWindow,
  repoPollingEnabled,
  worktreeRebaseCue,
} from "./tree";

/**
 * A row's icon: the codicon id, and the workbench colour id to tint it with.
 *
 * An absent `colorId` means **pass no `ThemeColor` at all**, so the icon inherits the
 * theme — which is how the two uncoloured states (a non-GitHub repo, and a worktree with
 * no live window) render.
 */
export interface RowIcon {
  iconId: string;
  colorId?: string;
}

/**
 * A repository row's icon.
 *
 * The GitHub repo icon reflects PR-poll state (#1376): green when the daemon is polling
 * this repo *and* the global master is on, else the default gray. A non-GitHub repo
 * keeps the plain `repo` glyph.
 */
export function repoRowIcon(repo: TreeRepoPayload, showPr: boolean): RowIcon {
  if (!repo.github) {
    return { iconId: "repo" };
  }
  const colorId = showPr && repoPollingEnabled(repo) ? "charts.green" : undefined;
  return { iconId: "github", colorId };
}

/**
 * A worktree row's icon, in precedence order.
 *
 * 1. **The rebase cue** (#1415). A rebase in flight — or a conflict left in place —
 *    takes the icon over entirely, colour included: it is transient and actionable,
 *    where open-state is neither. The row does lose its open badge for the duration, an
 *    accepted trade, since the tooltip and `contextValue` still carry open state and the
 *    badge layer's two characters are already claimed by the PR-check and Claude-session
 *    providers.
 * 2. **The open badge**, three-way: a blue tick for the worktree open in *this* window,
 *    a green dot for one open in another window, else the plain branch glyph for a
 *    worktree with no live window.
 */
export function worktreeRowIcon(wt: TreeWorktreePayload, windowKey?: string): RowIcon {
  const rebase = worktreeRebaseCue(wt);
  if (rebase) {
    return { iconId: rebase.iconId, colorId: "charts.yellow" };
  }
  if (isCurrentWindow(wt, windowKey)) {
    return { iconId: "check", colorId: "charts.blue" };
  }
  if (wt.open) {
    return { iconId: "circle-filled", colorId: "charts.green" };
  }
  return { iconId: "git-branch" };
}
