// The `vscode`-facing "Reopen Closed Worktree…" command (#2211): brings back a
// worktree whose window was closed, or which was removed outright. A thin adapter —
// every string and the confirmation text come from the pure, unit-tested
// `recentlyClosed.ts`, and this file only wires them onto the editor (the
// quick-pick, the modal, the progress notification, the toasts).
//
// Two shapes, one flow. For a worktree whose folder still exists the daemon simply
// opens the window. For a *removed* one the first `reopen` is side-effect free and
// returns a plan, which the user confirms in a modal before a second, confirmed
// `reopen` recreates anything — the `close` op's two-phase pattern, reversed. The
// client never sends a branch or destination: only the recorded path, so a request
// cannot make the daemon create a worktree anywhere else.
//
// Invoked three ways: from the command palette (no argument — a quick-pick over a
// fresh `recent-closed` fetch), and from a Recently Closed row's inline button or
// double-click, as `(clicked, selected[])` like the other item commands.

import * as vscode from "vscode";

import {
  NOTHING_TO_REOPEN,
  closedLabel,
  closedPickItems,
  notRestorableMessage,
  reopenConfirmation,
  reopenedMessage,
} from "./recentlyClosed";
import {
  ClosedWorktreePayload,
  Envelope,
  RecentClosedReply,
  ReopenReply,
  Reply,
  recentClosedEnvelope,
  reopenEnvelope,
} from "./socket";
import { TreeElement, closedTargets } from "./tree";

/**
 * Timeout for the confirmed reopen. Recreating a worktree is a `git worktree add`,
 * which checks out a whole tree and can take a while on a large repository.
 */
const REOPEN_EXECUTE_TIMEOUT_MS = 120_000;

/** What `reopenClosedWorktree` needs from `extension.ts`, injected so this file stays thin. */
export interface ReopenDeps {
  /** Sends one envelope; resolves `undefined` when the daemon is unreachable. */
  send: (envelope: Envelope, timeoutMs?: number) => Promise<Reply | undefined>;
}

/**
 * The **Reopen Closed Worktree…** command. With closed rows in the arguments it
 * reopens those, in order; with none (the palette) it asks the daemon for the
 * current list and offers a quick-pick, newest first.
 */
export async function reopenClosedWorktree(
  deps: ReopenDeps,
  clicked?: TreeElement,
  selected?: TreeElement[],
): Promise<void> {
  const targets = closedTargets(clicked, selected);
  if (targets.length > 0) {
    // One at a time: each removed entry has its own confirmation modal.
    for (const target of targets) {
      await reopenEntry(deps, target.entry);
    }
    return;
  }
  // A palette invocation. A *tree* node that is not a closed row (the group, say)
  // has nothing to reopen itself, but offering the picker is the useful answer.
  const entry = await pickClosed(deps);
  if (entry) {
    await reopenEntry(deps, entry);
  }
}

/**
 * Fetches the fresh list and asks the user to pick one entry. A fresh fetch rather
 * than the last snapshot, so a picker never offers an entry another window already
 * reopened. Resolves `undefined` when there is nothing to pick or the user cancels.
 */
async function pickClosed(deps: ReopenDeps): Promise<ClosedWorktreePayload | undefined> {
  const reply = await deps.send(recentClosedEnvelope());
  if (!reply) {
    daemonDownError();
    return undefined;
  }
  if (!reply.ok) {
    void vscode.window.showErrorMessage(
      `omni-dev: could not list recently closed worktrees — ${reply.error ?? "unknown error"}. ` +
        "This needs a daemon of the same release as the extension.",
    );
    return undefined;
  }
  const closed = (reply.payload as RecentClosedReply | undefined)?.closed ?? [];
  if (closed.length === 0) {
    void vscode.window.showInformationMessage(NOTHING_TO_REOPEN);
    return undefined;
  }
  const items = closedPickItems(closed, new Date());
  const picked = await vscode.window.showQuickPick(items, {
    title: "Reopen Closed Worktree",
    placeHolder: "Select a recently closed worktree",
    matchOnDescription: true,
    matchOnDetail: true,
  });
  return picked ? closed.find((entry) => entry.path === picked.path) : undefined;
}

/** Reopens one entry, running the confirmation round-trip when it was removed. */
async function reopenEntry(deps: ReopenDeps, entry: ClosedWorktreePayload): Promise<void> {
  const first = await deps.send(reopenEnvelope(entry.path));
  if (!first) {
    daemonDownError();
    return;
  }
  if (!first.ok) {
    reportFailure(first);
    return;
  }
  const reply = first.payload as ReopenReply | undefined;
  if (reply?.reopened) {
    reportReopened(entry, reply.recreated === true);
    return;
  }
  const plan = reply?.plan;
  if (!plan) {
    void vscode.window.showErrorMessage(
      `omni-dev: could not reopen “${closedLabel(entry)}” — the daemon sent no plan.`,
    );
    return;
  }
  if (!plan.restorable) {
    void vscode.window.showErrorMessage(notRestorableMessage(entry, plan));
    return;
  }

  // Modal, and the only way past it is the explicit confirm button: cancelling (or
  // dismissing) returns before the second request, so nothing has been created.
  const { message, detail, confirmLabel } = reopenConfirmation(entry, plan);
  const choice = await vscode.window.showWarningMessage(
    message,
    { modal: true, detail },
    confirmLabel,
  );
  if (choice !== confirmLabel) {
    return;
  }

  await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: `Recreating “${closedLabel(entry)}”…`,
    },
    async () => {
      const done = await deps.send(reopenEnvelope(entry.path, true), REOPEN_EXECUTE_TIMEOUT_MS);
      if (!done) {
        daemonDownError();
        return;
      }
      if (!done.ok) {
        reportFailure(done);
        return;
      }
      const outcome = done.payload as ReopenReply | undefined;
      if (outcome?.reopened) {
        reportReopened(entry, outcome.recreated !== false);
        return;
      }
      void vscode.window.showErrorMessage(
        `omni-dev: could not recreate “${closedLabel(entry)}” — the daemon did not reopen it.`,
      );
    },
  );
}

/** Toasts a success. Information, not a modal: nothing is left for the user to decide. */
function reportReopened(entry: ClosedWorktreePayload, recreated: boolean): void {
  vscode.window.setStatusBarMessage(reopenedMessage(entry, recreated), 4000);
}

/** An `ok:false` reply, surfaced with the daemon's own explanation. */
function reportFailure(reply: Reply): void {
  void vscode.window.showErrorMessage(
    `omni-dev: could not reopen the worktree — ${reply.error ?? "unknown error"}`,
  );
}

/** The shared "the daemon isn't running" error, matching the other tree actions. */
function daemonDownError(): void {
  void vscode.window.showErrorMessage(
    "omni-dev daemon not running. Start it with `omni-dev daemon start`.",
  );
}
