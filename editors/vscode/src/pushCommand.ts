// The `vscode`-facing "Push (force-with-lease)" command (#1443): publishes the
// selected worktrees' branches, force-pushing with a lease where a rebase rewrote
// history. A thin adapter — every string it shows comes from the pure, unit-tested
// `pushReport.ts`, and this file only wires them onto the editor (the selection
// expansion, the confirmation, the progress notification, the toasts).
//
// This is the action that completes the batch-rebase workflow #1415 started: the
// tree could rewrite N branches with one gesture, then displayed the divergence it
// had just created with no way to resolve it.
//
// A socket client like every other tree action, so the daemon is required;
// `omni-dev worktrees push` keeps its own local engine for when there is none.
//
// Unlike "Rebase on main" this is offered on **repo rows too**, so the selection
// goes through `expandToWorktrees` rather than `worktreeTargets`: a selected repo
// means every worktree of it, expanded client-side from the snapshot the repo node
// already carries.

import * as vscode from "vscode";

import {
  confirmDetail,
  confirmTitle,
  forcedOutcomes,
  nothingToPushMessage,
  pendingOutcomes,
  skippedOutcomes,
  summarize,
} from "./pushReport";
import { Envelope, PushReply, Reply, pushCheckEnvelope, pushEnvelope } from "./socket";
import { Node, expandToWorktrees, selectionTargets } from "./tree";

/**
 * Generous timeout for the phase-2 execute: the daemon re-plans and then runs
 * `git push` per worktree, sequentially. A large batch over a slow remote is
 * minutes, not seconds.
 */
const PUSH_EXECUTE_TIMEOUT_MS = 300_000;

/**
 * Timeout for the phase-1 check. Much shorter than the execute, because unlike
 * `rebase`'s check this one contacts no remote at all — it reads local refs — so
 * anything approaching this is a stuck daemon rather than a slow network.
 */
const PUSH_CHECK_TIMEOUT_MS = 30_000;

/** What `pushForceWithLease` needs from `extension.ts`, injected so this file stays thin. */
export interface PushDeps {
  /** Sends one envelope; resolves `undefined` when the daemon is unreachable. */
  send: (envelope: Envelope, timeoutMs?: number) => Promise<Reply | undefined>;
  /** This window's registry key, carried on both phases for the daemon's audit log. */
  windowKey: string;
}

/**
 * The **Push (force-with-lease)** command: publishes every selected worktree's
 * branch in a single `push` op.
 *
 * Two-phase like "Rebase on main": phase 1 classifies (contacting no remote), the
 * modal confirms against *that* result, and phase 2 re-plans from scratch before
 * publishing anything.
 *
 * The daemon refuses to force-push a repository's remote default branch whatever
 * this sends, so a selection that includes `main` reports it as skipped rather
 * than being filtered out here — the same "classify in the daemon, not the client"
 * rule ADR-0060 applied to rebase.
 */
export async function pushForceWithLease(
  deps: PushDeps,
  clicked?: Node,
  selected?: Node[],
): Promise<void> {
  const targets = expandToWorktrees(selectionTargets(clicked, selected));
  if (targets.length === 0) {
    return;
  }
  const paths = targets.map((t) => t.wt.path);

  await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title:
        targets.length === 1
          ? `Checking “${targets[0].wt.branch ?? targets[0].wt.path}”…`
          : `Checking ${targets.length} worktrees…`,
    },
    async (progress) => {
      // Phase 1: classify. One op for the whole batch, so the reply is a single
      // per-worktree summary rather than N independent results.
      const checked = await deps.send(
        pushCheckEnvelope(paths, deps.windowKey),
        PUSH_CHECK_TIMEOUT_MS,
      );
      if (!checked) {
        daemonDownError();
        return;
      }
      if (!checked.ok) {
        void vscode.window.showErrorMessage(
          `omni-dev: could not plan the push — ${checked.error ?? "unknown error"}`,
        );
        return;
      }
      const report = checked.payload as PushReply;
      const pending = pendingOutcomes(report);
      if (pending.length === 0) {
        void vscode.window.showWarningMessage(
          `omni-dev: nothing to push — ${nothingToPushMessage(report, targets.length)}.`,
        );
        return;
      }
      if (!(await confirmPush(report, targets.length))) {
        return;
      }

      // Phase 2: execute. The daemon re-plans before publishing anything.
      progress.report({ message: `Pushing ${pending.length}…` });
      const exec = await deps.send(
        pushEnvelope(paths, deps.windowKey),
        PUSH_EXECUTE_TIMEOUT_MS,
      );
      if (!exec) {
        daemonDownError();
        return;
      }
      if (!exec.ok) {
        void vscode.window.showErrorMessage(
          `omni-dev: push failed — ${exec.error ?? "unknown error"}`,
        );
        return;
      }
      reportSummary(exec.payload as PushReply);
    },
  );
}

/**
 * The push confirmation. **Always** shown, even for a single row (ADR-0049 §1's
 * rule): a push publishes to everyone, and a batch is one gesture over N branches
 * — the modal is the only place the user sees the full set, and specifically which
 * of them are force pushes.
 *
 * The action button names the more consequential half of the batch, so a
 * force-push can never be confirmed by a button that says merely "Push".
 */
async function confirmPush(report: PushReply, total: number): Promise<boolean> {
  const pending = pendingOutcomes(report);
  const forced = forcedOutcomes(report);
  const action = forced.length > 0 ? "Force Push" : "Push";
  const choice = await vscode.window.showWarningMessage(
    confirmTitle(pending, total),
    { modal: true, detail: confirmDetail(pending, skippedOutcomes(report)) },
    action,
  );
  return choice === action;
}

/**
 * Toasts the phase-2 outcome. A refused lease raises this to an error and is
 * named: the branch is still unpublished and there is a specific thing to do about
 * it.
 */
function reportSummary(reply: PushReply): void {
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
      "or run `omni-dev worktrees push <path>` yourself.",
  );
}
