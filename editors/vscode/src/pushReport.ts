// The pure reporting model for the "Push (force-with-lease)" action (#1443):
// turning the daemon's two-phase `push` reply into the strings the modal and the
// summary toast show.
//
// Deliberately free of any `vscode` import, so it stays unit-testable under
// `node --test` (the `rebaseReport.ts` / `sessionCounts.ts` split). The
// `vscode`-facing half — the modal, the progress notification, the toasts — lives
// in `pushCommand.ts` and does nothing but call into here.

import { PushOutcome, PushReply } from "./socket";

/** Phase-1 statuses for a worktree that has something to publish. */
const FAST_FORWARD = "would-fast-forward";
const FORCE = "would-force";
const CREATE = "would-create";

/** Phase-2 statuses. */
const PUSHED = "pushed";
const CREATED = "created";
const REJECTED = "rejected";

/** The phase-1 statuses that mean "there is something to publish here". */
const PENDING = [FAST_FORWARD, FORCE, CREATE];

/** The worktrees a phase-1 reply says are actually worth pushing. */
export function pendingOutcomes(reply: PushReply): PushOutcome[] {
  return (reply.worktrees ?? []).filter((w) => PENDING.includes(w.status));
}

/**
 * The pending worktrees that need the **lease** — the ones whose history was
 * rewritten. Split out because they are the only reason the modal is a warning:
 * everything else is an ordinary publish.
 */
export function forcedOutcomes(reply: PushReply): PushOutcome[] {
  return (reply.worktrees ?? []).filter((w) => w.status === FORCE);
}

/**
 * The worktrees a reply did **not** plan to push, with the up-to-date ones
 * dropped: "already published" is the expected, uninteresting case, and listing
 * it would bury the reasons that matter (a refused default branch, a detached
 * HEAD, no remote).
 */
export function skippedOutcomes(reply: PushReply): PushOutcome[] {
  return (reply.worktrees ?? []).filter(
    (w) => !PENDING.includes(w.status) && w.status !== "up-to-date",
  );
}

/** The worktrees whose upstream already matched. */
export function upToDateCount(reply: PushReply): number {
  return (reply.worktrees ?? []).filter((w) => w.status === "up-to-date").length;
}

/** A worktree's display name: its branch, else the folder basename. */
export function outcomeLabel(outcome: PushOutcome): string {
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
    case "not-a-worktree":
      return "not a git worktree";
    case "no-remote":
      return "no remote to publish to";
    case "default-branch-force-push":
      return "refusing to force-push the default branch";
    default:
      return reason ?? "skipped";
  }
}

/** One line describing why a worktree was not pushed. */
function skipLine(outcome: PushOutcome): string {
  return `• ${outcomeLabel(outcome)} — ${skipReasonText(outcome.reason)}`;
}

/**
 * One line describing what will happen to a pending worktree.
 *
 * The whole point of listing these is that **force and fast-forward look
 * different**: a batch is one gesture over N branches, and the only place the user
 * sees which of them are about to have their published history rewritten is here.
 */
export function pendingLine(outcome: PushOutcome): string {
  const name = outcomeLabel(outcome);
  const to = outcome.remote ? ` → ${outcome.remote}/${outcome.remote_branch}` : "";
  switch (outcome.status) {
    case FORCE:
      return `• ${name}${to} — FORCE (${outcome.ahead ?? "?"} ahead, ${
        outcome.behind ?? "?"
      } behind)`;
    case CREATE:
      return `• ${name}${to} — new branch`;
    default:
      return `• ${name}${to} — fast-forward (${outcome.ahead ?? "?"} ahead)`;
  }
}

/**
 * The confirmation modal's detail body: every branch about to be published, the
 * forced ones first and labelled as such, then the skipped ones with reasons.
 *
 * Forced entries lead because they are the ones with a consequence for other
 * people; an ordinary fast-forward in the same batch is unremarkable and should
 * not be what the eye lands on.
 */
export function confirmDetail(pending: PushOutcome[], skipped: PushOutcome[]): string {
  const forced = pending.filter((p) => p.status === FORCE);
  const ordinary = pending.filter((p) => p.status !== FORCE);
  const lines = [...forced, ...ordinary].map(pendingLine);
  if (skipped.length > 0) {
    lines.push("", "Skipped:", ...skipped.map(skipLine));
  }
  if (forced.length > 0) {
    lines.push(
      "",
      "Force pushes use --force-with-lease --force-if-includes: if the remote " +
        "moved since you last fetched, the push is refused rather than " +
        "overwriting work you have not seen.",
    );
  }
  return lines.join("\n");
}

/** The modal's title line. */
export function confirmTitle(pending: PushOutcome[], total: number): string {
  const forced = pending.filter((p) => p.status === FORCE).length;
  if (pending.length === 1) {
    const name = outcomeLabel(pending[0]);
    return forced === 1
      ? `Force-push “${name}” with a lease?`
      : `Push “${name}”?`;
  }
  const of = pending.length === total ? "" : ` of ${total}`;
  const suffix = forced > 0 ? ` (${forced} force-pushed)` : "";
  return `Push ${pending.length}${of} branches${suffix}?`;
}

/**
 * The message shown when phase 1 found nothing to do — naming *why*, so the
 * action never looks like it silently failed.
 */
export function nothingToPushMessage(reply: PushReply, total: number): string {
  const skipped = skippedOutcomes(reply);
  const upToDate = upToDateCount(reply);
  if (skipped.length === 0) {
    return upToDate > 0
      ? `${upToDate === total ? "all" : upToDate} already up to date`
      : "nothing to push";
  }
  const reasons = [...new Set(skipped.map((s) => skipReasonText(s.reason)))].join(", ");
  const upToDateSuffix = upToDate > 0 ? `, ${upToDate} already up to date` : "";
  return `${skipped.length} of ${total} skipped (${reasons})${upToDateSuffix}`;
}

/** How a phase-2 result should be surfaced. */
export interface PushSummary {
  /** `error` for a lease refusal or other rejection, else `info`. */
  severity: "info" | "warning" | "error";
  message: string;
}

/**
 * Summarises a phase-2 reply into one toast.
 *
 * A **lease refusal** is the one outcome that gets its own sentence and names the
 * fix. It is not a bug and not a failure of the feature — it is the feature — but
 * it does mean the user's work is still unpublished and there is a specific thing
 * to do about it, so folding it into a bare "1 rejected" would be useless.
 */
export function summarize(reply: PushReply): PushSummary {
  const worktrees = reply.worktrees ?? [];
  const pushed = worktrees.filter((w) => w.status === PUSHED);
  const forced = pushed.filter((w) => w.forced);
  const created = worktrees.filter((w) => w.status === CREATED);
  const rejected = worktrees.filter((w) => w.status === REJECTED);
  const stale = rejected.filter((w) => w.stale);

  const parts: string[] = [];
  if (pushed.length > 0) {
    const forcedSuffix = forced.length > 0 ? ` (${forced.length} force-pushed)` : "";
    parts.push(
      `pushed ${pushed.length} ${pushed.length === 1 ? "branch" : "branches"}${forcedSuffix}`,
    );
  }
  if (created.length > 0) {
    parts.push(`published ${created.length} new`);
  }
  if (stale.length > 0) {
    const names = stale.map(outcomeLabel).join(", ");
    parts.push(
      `${stale.length} refused because the remote moved: ${names} — run \`git fetch\`, rebase, then push again`,
    );
  }
  const otherRejected = rejected.length - stale.length;
  if (otherRejected > 0) {
    const names = rejected
      .filter((w) => !w.stale)
      .map((w) => `${outcomeLabel(w)}${w.detail ? ` (${w.detail})` : ""}`)
      .join(", ");
    parts.push(`${otherRejected} rejected: ${names}`);
  }
  if (parts.length === 0) {
    return { severity: "info", message: "nothing was pushed" };
  }
  return {
    severity: rejected.length > 0 ? "error" : "info",
    message: parts.join("; "),
  };
}
