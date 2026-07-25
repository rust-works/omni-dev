// The `vscode`-facing "Rebase on main" command (#1409): rebases the selected
// worktree(s) onto their repository's remote default branch, fetching it once per
// repo. A thin adapter — the binary resolution and the invocation builders live in
// the pure, unit-tested `omniDev.ts`, and this file only wires them onto the editor
// (the selection filter, the confirmation, and the terminal).
//
// **This is the one tree-view action that does not use the daemon.** Every other
// one sends a socket envelope; the rebase deliberately shells out to
// `omni-dev worktrees rebase` in the user's own environment, because its fetch needs
// their `ssh-agent` / credential helper — which the daemon, spawned by
// launchd/systemd with a minimal environment, does not have (ADR-0055, ADR-0003,
// #903). Adding a `rebase` daemon op would reintroduce exactly the problem those
// ADRs avoid, so the entry point is a terminal, not a socket.
//
// Like the other item commands it is a `view/item/context` command on a multi-select
// view (#1357), so VS Code invokes it as `(clicked, selected[])` — see
// `selectionTargets` for why the two arguments are resolved rather than concatenated.

import * as vscode from "vscode";

import { commandLine, rebaseArgs, resolveOmniDevBin } from "./omniDev";
import {
  Node,
  WorktreeNode,
  partitionByRole,
  selectionTargets,
  worktreeLabel,
  worktreeTargets,
} from "./tree";

/** The terminal a rebase runs in. Fresh per invocation, so nothing queues behind a running one. */
const TERMINAL_NAME = "omni-dev rebase";

/**
 * The **Rebase on main** command: rebases every selected **linked** worktree onto
 * its repository's remote default branch, in a single `omni-dev worktrees rebase`
 * invocation so the engine fetches once per repository however many worktrees of it
 * were selected.
 *
 * Main working trees are filtered out here rather than trusted to the menu: a `when`
 * clause sees only the *clicked* row, so a mixed multi-selection reaches this handler
 * intact. They are **named** as skipped rather than silently dropped, the way
 * `closeWorktree` names them — quietly narrowing what the user asked for is worse
 * than telling them. (The CLI refuses them too, on the same structural `is_main`
 * guard, so this is convenience rather than the guard.)
 *
 * The rebase then runs in an integrated terminal rather than a captured subprocess:
 * the fetch may prompt, a rebase may hit conflicts, and the per-worktree result table
 * is the actual answer — so live, user-drivable output is the point. It is also the
 * error surface: a failed fetch, a conflict, or a `command not found` from an
 * unresolvable binary all land there visibly.
 */
export async function rebaseOnMain(clicked?: Node, selected?: Node[]): Promise<void> {
  const { linked, main } = partitionByRole(worktreeTargets(selectionTargets(clicked, selected)));
  if (linked.length === 0) {
    if (main.length > 0) {
      void vscode.window.showWarningMessage(
        `omni-dev: nothing to rebase — ${describeMainSkips(main)}.`,
      );
    }
    return;
  }

  if (!(await confirmRebase(linked, main))) {
    return;
  }

  const paths = linked.map((t) => t.wt.path);
  const line = commandLine(resolveOmniDevBin(), rebaseArgs(paths, { yes: true }));
  const terminal = vscode.window.createTerminal({
    name: TERMINAL_NAME,
    // Every path is absolute, so the cwd cannot affect which worktrees are
    // targeted; the first one just makes the terminal land somewhere relevant.
    cwd: paths[0],
    iconPath: new vscode.ThemeIcon("git-branch"),
  });
  terminal.show();
  terminal.sendText(line, true);
}

/**
 * The rebase confirmation. **Always** shown, even for a single row: a rebase
 * rewrites branch history, and a batch is one gesture over N branches — the modal is
 * the only place the user sees the full set (ADR-0049 §1's rule, as applied by
 * "Add to Merge Queue"). Confirming here is what lets the CLI run with `-y`.
 *
 * The list deliberately carries **no** behind-count: the tree's `behind` measures
 * divergence from the branch's *upstream*, not from the rebase target, so showing it
 * here would be quietly wrong. The real counts arrive in the terminal seconds later.
 */
async function confirmRebase(linked: WorktreeNode[], main: WorktreeNode[]): Promise<boolean> {
  const detail = [
    ...linked.map((t) => `• ${worktreeLabel(t.wt)}`),
    ...(main.length > 0 ? ["", `Skipped: ${describeMainSkips(main)}`] : []),
    "",
    "This rewrites branch history. Worktrees with uncommitted changes, a detached " +
      "HEAD, or a rebase already in progress are skipped and reported.",
  ].join("\n");
  const choice = await vscode.window.showWarningMessage(
    linked.length === 1
      ? `Rebase “${worktreeLabel(linked[0].wt)}” onto the remote default branch?`
      : `Rebase ${linked.length} worktrees onto the remote default branch?`,
    { modal: true, detail },
    "Rebase",
  );
  return choice === "Rebase";
}

/** Names the main working tree(s) a selection carried in, which are never rebased. */
function describeMainSkips(main: WorktreeNode[]): string {
  return main.length === 1
    ? `${worktreeLabel(main[0].wt)} is a main working tree`
    : `${main.length} main working trees`;
}
