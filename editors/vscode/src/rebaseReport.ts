// The pure reporting model for the "Rebase on main" action (#1415): turning the
// daemon's two-phase `rebase` reply into the strings the modal and the summary
// toast show.
//
// Deliberately free of any `vscode` import, so it stays unit-testable under
// `node --test` (the `sessionCounts.ts` / `claudeEmbeddings.ts` split). The
// `vscode`-facing half — the modal, the progress notification, the toasts — lives
// in `rebaseCommand.ts` and does nothing but call into here.

import { RebaseOutcome, RebaseReply } from "./socket";

/** The statuses phase 1 assigns to a worktree it *would* rebase. */
const PENDING = "would-rebase";

/** Statuses phase 2 assigns to a worktree it rebased. */
const REBASED = "rebased";

/** The status assigned to a rebase that hit conflicts. */
const CONFLICT = "conflict";

/** The worktrees a phase-1 reply says are actually worth rebasing. */
export function pendingOutcomes(reply: RebaseReply): RebaseOutcome[] {
  return (reply.worktrees ?? []).filter((w) => w.status === PENDING);
}

/**
 * The worktrees a reply did **not** plan to rebase, with the up-to-date ones
 * dropped: "already on top of main" is the expected, uninteresting case, and
 * listing it would bury the reasons that matter (dirty, detached, mid-rebase).
 */
export function skippedOutcomes(reply: RebaseReply): RebaseOutcome[] {
  return (reply.worktrees ?? []).filter(
    (w) => w.status !== PENDING && w.status !== "up-to-date",
  );
}

/** The worktrees that were already on top of the target. */
export function upToDateCount(reply: RebaseReply): number {
  return (reply.worktrees ?? []).filter((w) => w.status === "up-to-date").length;
}

/** A repository fetch that failed, if any — the "you are offline" signal. */
export function failedFetches(reply: RebaseReply): string[] {
  return (reply.fetches ?? [])
    .filter((f) => !f.ok)
    .map((f) => `${f.onto} in ${f.repo_root}${f.detail ? `: ${f.detail}` : ""}`);
}

/** A worktree's display name: its branch, else the folder basename. */
export function outcomeLabel(outcome: RebaseOutcome): string {
  if (outcome.branch) {
    return outcome.branch;
  }
  const trimmed = outcome.path.replace(/[/\\]+$/, "");
  const cut = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  return cut >= 0 ? trimmed.slice(cut + 1) : trimmed;
}

/** The human phrase for a skip reason slug, falling back to the slug itself. */
export function skipReasonText(reason: string | undefined): string {
  switch (reason) {
    case "detached-head":
      return "detached HEAD";
    case "dirty":
      return "uncommitted changes";
    case "operation-in-progress":
      return "a rebase or merge already in progress";
    case "not-a-worktree":
      return "not a git worktree";
    case "no-onto-ref":
      return "no resolvable target branch";
    default:
      return reason ?? "skipped";
  }
}

/** One line describing why a worktree was not rebased. */
function skipLine(outcome: RebaseOutcome): string {
  const why =
    outcome.status === "fetch-failed"
      ? `the repository's fetch failed${outcome.detail ? ` (${outcome.detail})` : ""}`
      : skipReasonText(outcome.reason);
  return `• ${outcomeLabel(outcome)} — ${why}`;
}

/**
 * The confirmation modal's detail body: every branch about to be rewritten with
 * its **real** behind-count, then the skipped ones with reasons.
 *
 * The behind-counts are the payoff of checking before confirming. #1409's modal
 * deliberately showed none, because the only number it had was the tree's
 * `behind` — divergence from the branch's *upstream*, not from the rebase target,
 * which would have been quietly wrong. Phase 1 measures against the freshly
 * fetched target, so these are the true counts.
 */
export function confirmDetail(pending: RebaseOutcome[], skipped: RebaseOutcome[]): string {
  const lines = pending.map((p) => {
    const behind = p.behind === undefined ? "" : ` (${p.behind} behind)`;
    return `• ${outcomeLabel(p)}${behind}`;
  });
  if (skipped.length > 0) {
    lines.push("", "Skipped:", ...skipped.map(skipLine));
  }
  lines.push(
    "",
    "This rewrites branch history. A worktree that hits conflicts is left " +
      "mid-rebase so you can resolve it in place and run `git rebase --continue`.",
  );
  return lines.join("\n");
}

/** The modal's title line. */
export function confirmTitle(pending: RebaseOutcome[], total: number): string {
  if (pending.length === 1) {
    return `Rebase “${outcomeLabel(pending[0])}” onto the remote default branch?`;
  }
  const of = pending.length === total ? "" : ` of ${total}`;
  return `Rebase ${pending.length}${of} worktrees onto the remote default branch?`;
}

/**
 * The message shown when phase 1 found nothing to do — naming *why*, so the
 * action never looks like it silently failed.
 */
export function nothingToRebaseMessage(reply: RebaseReply, total: number): string {
  const failed = failedFetches(reply);
  if (failed.length > 0) {
    return `could not fetch — ${failed.join("; ")}`;
  }
  const skipped = skippedOutcomes(reply);
  const upToDate = upToDateCount(reply);
  if (skipped.length === 0) {
    return upToDate > 0
      ? `${upToDate === total ? "all" : upToDate} already up to date`
      : "nothing to rebase";
  }
  const reasons = [...new Set(skipped.map((s) => skipReasonText(s.reason)))].join(", ");
  const upToDateSuffix = upToDate > 0 ? `, ${upToDate} already up to date` : "";
  return `${skipped.length} of ${total} skipped (${reasons})${upToDateSuffix}`;
}

/** How a phase-2 result should be surfaced. */
export interface RebaseSummary {
  /** `error` for a failure, `warning` for conflicts to resolve, else `info`. */
  severity: "info" | "warning" | "error";
  message: string;
}

/**
 * Summarises a phase-2 reply into one toast.
 *
 * A left-in-place conflict is a **warning**, not an error and not an aside: the
 * rebase did what was asked, but a worktree is now sitting mid-rebase waiting for
 * the user — so it is named, and told what to do about it. Anything else that
 * stopped a worktree short (a failed fetch, a late-arriving skip) is reported too
 * rather than quietly folded into the count.
 */
export function summarize(reply: RebaseReply): RebaseSummary {
  const worktrees = reply.worktrees ?? [];
  const rebased = worktrees.filter((w) => w.status === REBASED);
  const conflicted = worktrees.filter((w) => w.status === CONFLICT);
  const failedFetch = worktrees.filter((w) => w.status === "fetch-failed");

  const parts: string[] = [];
  if (rebased.length > 0) {
    parts.push(`rebased ${rebased.length} ${rebased.length === 1 ? "worktree" : "worktrees"}`);
  }
  if (conflicted.length > 0) {
    const names = conflicted.map(outcomeLabel).join(", ");
    const kept = conflicted.some((c) => c.left_in_place);
    parts.push(
      kept
        ? `${conflicted.length} left mid-rebase to resolve: ${names} — fix the conflicts, then \`git rebase --continue\``
        : `${conflicted.length} conflicted and was rolled back: ${names}`,
    );
  }
  if (failedFetch.length > 0) {
    parts.push(`${failedFetch.length} skipped after a failed fetch`);
  }
  if (parts.length === 0) {
    return { severity: "info", message: "nothing was rebased" };
  }
  const severity: RebaseSummary["severity"] =
    failedFetch.length > 0 ? "error" : conflicted.length > 0 ? "warning" : "info";
  return { severity, message: parts.join("; ") };
}
