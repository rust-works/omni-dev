// The `vscode`-facing "Rebase on main" command (#1409, reworked in #1415): rebases
// the selected worktree(s) onto their repository's remote default branch, fetching
// it once per repo. A thin adapter — every string it shows comes from the pure,
// unit-tested `rebaseReport.ts`, and this file only wires them onto the editor (the
// selection filter, the confirmation, the progress notification, the toasts).
//
// **It is a socket client like every other tree action.** #1409 shipped it as a
// shell-out to `omni-dev worktrees rebase` in an integrated terminal, because
// ADR-0055 held that the daemon could not authenticate a fetch — its launchd
// environment supposedly lacking `SSH_AUTH_SOCK`. That premise was wrong: launchd
// exports `SSH_AUTH_SOCK` into the per-user session, so the daemon inherits the
// user's `ssh-agent`. ADR-0059 moves the rebase into the daemon's two-phase
// `rebase` op, which removes the terminal, gives the modal real behind-counts, and
// — the actual motivation — lets a conflicted worktree be left mid-rebase instead
// of thrown away by `git rebase --abort`.
//
// The daemon is therefore required, as it is for close / merge-queue / reposition.
// `omni-dev worktrees rebase` keeps its own local engine for when there is none.
//
// Like the other item commands it is a `view/item/context` command on a multi-select
// view (#1357), so VS Code invokes it as `(clicked, selected[])` — see
// `selectionTargets` for why the two arguments are resolved rather than concatenated.

import * as vscode from "vscode";

import {
  confirmDetail,
  confirmTitle,
  nothingToRebaseMessage,
  pendingOutcomes,
  skippedOutcomes,
  summarize,
} from "./rebaseReport";
import { Envelope, RebaseReply, Reply, rebaseCheckEnvelope, rebaseEnvelope } from "./socket";
import { Node, selectionTargets, worktreeLabel, worktreeTargets } from "./tree";

/**
 * Generous timeout for the phase-2 execute: the daemon re-plans (which re-fetches
 * every selected repository) and then runs `git rebase` per worktree, sequentially.
 * A large batch over a slow remote is minutes, not seconds.
 */
const REBASE_EXECUTE_TIMEOUT_MS = 300_000;

/**
 * Timeout for the phase-1 check. Shorter than the execute — it fetches but never
 * rebases — yet still well past a normal fetch, since an unreachable remote hangs
 * until git's own timeout.
 */
const REBASE_CHECK_TIMEOUT_MS = 120_000;

/** What `rebaseOnMain` needs from `extension.ts`, injected so this file stays thin. */
export interface RebaseDeps {
  /** Sends one envelope; resolves `undefined` when the daemon is unreachable. */
  send: (envelope: Envelope, timeoutMs?: number) => Promise<Reply | undefined>;
  /** This window's registry key, carried on both phases for the daemon's audit log. */
  windowKey: string;
}

/**
 * The **Rebase on main** command: rebases every selected worktree onto its
 * repository's remote default branch, in a single `rebase` op so the daemon
 * fetches once per repository however many worktrees were selected. The main
 * working tree is a valid target like any other (#1438, ADR-0060): it is sent to
 * the daemon and classified there — up-to-date, would-rebase, dirty, etc. — same
 * as a linked worktree, with no client-side pre-filter dropping it from the batch.
 *
 * Two-phase like "Add to Merge Queue": phase 1 fetches and classifies (side-effect
 * free apart from advancing a remote-tracking ref), the modal confirms against
 * *that* result, and phase 2 re-plans from scratch before rewriting anything.
 */
export async function rebaseOnMain(
  deps: RebaseDeps,
  clicked?: Node,
  selected?: Node[],
): Promise<void> {
  const targets = worktreeTargets(selectionTargets(clicked, selected));
  if (targets.length === 0) {
    return;
  }
  const paths = targets.map((t) => t.wt.path);

  await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title:
        targets.length === 1
          ? `Checking “${worktreeLabel(targets[0].wt)}”…`
          : `Checking ${targets.length} worktrees…`,
    },
    async (progress) => {
      // Phase 1: fetch once per repo and classify. One op for the whole batch —
      // that is what lets the daemon group the selection by repository.
      const checked = await deps.send(
        rebaseCheckEnvelope(paths, deps.windowKey),
        REBASE_CHECK_TIMEOUT_MS,
      );
      if (!checked) {
        daemonDownError();
        return;
      }
      if (!checked.ok) {
        void vscode.window.showErrorMessage(
          `omni-dev: could not plan the rebase — ${checked.error ?? "unknown error"}`,
        );
        return;
      }
      const report = checked.payload as RebaseReply;
      const pending = pendingOutcomes(report);
      if (pending.length === 0) {
        void vscode.window.showWarningMessage(
          `omni-dev: nothing to rebase — ${nothingToRebaseMessage(report, targets.length)}.`,
        );
        return;
      }
      if (!(await confirmRebase(pending, skippedOutcomes(report), targets.length))) {
        return;
      }

      // Phase 2: execute. The daemon re-plans before rewriting anything.
      progress.report({ message: `Rebasing ${pending.length}…` });
      const exec = await deps.send(
        rebaseEnvelope(paths, deps.windowKey),
        REBASE_EXECUTE_TIMEOUT_MS,
      );
      if (!exec) {
        daemonDownError();
        return;
      }
      if (!exec.ok) {
        void vscode.window.showErrorMessage(
          `omni-dev: rebase failed — ${exec.error ?? "unknown error"}`,
        );
        return;
      }
      reportSummary(exec.payload as RebaseReply);
    },
  );
}

/**
 * The rebase confirmation. **Always** shown, even for a single row: a rebase
 * rewrites branch history, and a batch is one gesture over N branches — the modal is
 * the only place the user sees the full set (ADR-0049 §1's rule, as applied by
 * "Add to Merge Queue").
 *
 * Unlike #1409's version this lists each branch's **behind-count**, because phase 1
 * has just measured it against the freshly fetched rebase target. The old modal
 * omitted counts on purpose: the only number it had was the tree's `behind`, which
 * measures divergence from the branch's *upstream* and would have been wrong here.
 */
async function confirmRebase(
  pending: ReturnType<typeof pendingOutcomes>,
  skipped: ReturnType<typeof skippedOutcomes>,
  total: number,
): Promise<boolean> {
  const detail = confirmDetail(pending, skipped);
  const choice = await vscode.window.showWarningMessage(
    confirmTitle(pending, total),
    { modal: true, detail },
    "Rebase",
  );
  return choice === "Rebase";
}

/**
 * Toasts the phase-2 outcome. A worktree left mid-rebase raises this to a warning
 * and is named — it is the one case that still needs the user, and the tree row
 * keeps cueing it until the conflict is resolved.
 */
function reportSummary(reply: RebaseReply): void {
  const { severity, message } = summarize(reply);
  const text = `omni-dev: ${message}.`;
  if (severity === "error") {
    void vscode.window.showErrorMessage(text);
  } else if (severity === "warning") {
    void vscode.window.showWarningMessage(text);
  } else {
    void vscode.window.showInformationMessage(text);
  }
}

/** The shared "the daemon isn't running" error, matching the other tree actions. */
function daemonDownError(): void {
  void vscode.window.showErrorMessage(
    "omni-dev daemon not running. Start it with `omni-dev daemon start`, " +
      "or run `omni-dev worktrees rebase <path>` yourself.",
  );
}
