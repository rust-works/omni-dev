// The pure "Recently Closed" model (#2211): how a closed worktree is labelled,
// aged, tooltipped and picked from, and the exact text of the confirmation shown
// before a *removed* worktree is recreated.
//
// Deliberately free of any `vscode` import so it stays unit-testable under
// `node --test` (like `rebaseReport.ts`). `treeDataProvider.ts` maps these strings
// onto tree items and `reopenCommand.ts` onto the quick-pick and modal; neither
// owns any wording of its own.

import * as path from "path";

import { ClosedWorktreePayload, ReopenPlan } from "./socket";

/** The Recently Closed group row's label. */
export const CLOSED_GROUP_LABEL = "Recently Closed";

/** The group row's `contextValue` — named so no worktree/repo menu regex matches it. */
export const CLOSED_GROUP_CONTEXT = "closedGroup";

/**
 * The caveat shown on **every** recreation confirmation, whatever the plan says.
 * A removed worktree's uncommitted changes went with its directory; recreating it
 * restores committed work only, and the user must never assume otherwise.
 */
export const UNCOMMITTED_CAVEAT =
  "Uncommitted changes the worktree had when it was removed are not recoverable. " +
  "Only committed work comes back.";

/** The commit abbreviation used in rows and confirmations. */
const SHORT_SHA_LEN = 7;

const MINUTE_MS = 60_000;
const HOUR_MS = 60 * MINUTE_MS;
const DAY_MS = 24 * HOUR_MS;

/**
 * A relative age such as `5m ago`, `3h ago`, `2d ago`. Under a minute — and a
 * `closed_at` in the future, which clock skew between processes can produce — reads
 * `just now`. An unparseable timestamp yields `""` so the caller omits the age
 * rather than showing `NaNd ago`.
 */
export function formatAge(closedAt: string, now: Date): string {
  const closed = Date.parse(closedAt);
  if (Number.isNaN(closed)) {
    return "";
  }
  const elapsed = now.getTime() - closed;
  if (elapsed < MINUTE_MS) {
    return "just now";
  }
  if (elapsed < HOUR_MS) {
    return `${Math.floor(elapsed / MINUTE_MS)}m ago`;
  }
  if (elapsed < DAY_MS) {
    return `${Math.floor(elapsed / HOUR_MS)}h ago`;
  }
  return `${Math.floor(elapsed / DAY_MS)}d ago`;
}

/** The entry's branch, or `(detached)` when HEAD was detached — the tooltip's phrasing too. */
export function closedBranchLabel(entry: ClosedWorktreePayload): string {
  return entry.branch ?? "(detached)";
}

/**
 * A closed row's label: `<main_repo> · <branch>`. The repo leads because, unlike a
 * live row nested under its repo, this group is flat across every repository, so
 * the branch alone (`main`, `issue-1`) would not say where it belongs.
 */
export function closedLabel(entry: ClosedWorktreePayload): string {
  return `${entry.main_repo} · ${closedBranchLabel(entry)}`;
}

/**
 * A closed row's muted description: its age, plus `· removed` when the worktree was
 * deleted (so reopening will recreate it) as opposed to merely having its window
 * closed. A main working tree is flagged too, since it is the one row whose label
 * does not otherwise differ from a linked worktree on the same branch.
 */
export function closedDescription(entry: ClosedWorktreePayload, now: Date): string {
  const parts: string[] = [];
  const age = formatAge(entry.closed_at, now);
  if (age) {
    parts.push(age);
  }
  if (entry.is_main) {
    parts.push("main tree");
  }
  if (entry.removed) {
    parts.push("removed");
  }
  return parts.join(" · ");
}

/** The codicon id for a row: a bin for a removed worktree, a clock for a closed window. */
export function closedIconId(entry: ClosedWorktreePayload): string {
  return entry.removed ? "trash" : "history";
}

/**
 * The row's `contextValue`: `closed.removed` (the folder is gone, reopening
 * recreates it) or `closed.window` (only the window was closed). Both start with
 * `closed.` so one menu regex covers them, and neither contains `worktree`, `repo`
 * or `main`, so no existing `viewItem =~ /.../` clause can match them.
 */
export function closedContextValue(entry: ClosedWorktreePayload): string {
  return entry.removed ? "closed.removed" : "closed.window";
}

/** The group row's description: how many entries it holds. */
export function closedGroupDescription(count: number): string {
  return String(count);
}

/** The hover text for a closed row: where it was, when it closed, and what reopening does. */
export function closedTooltip(entry: ClosedWorktreePayload): string {
  const lines = [entry.path, `Branch: ${closedBranchLabel(entry)}`];
  if (entry.head_sha) {
    lines.push(`Commit: ${shortSha(entry.head_sha)}`);
  }
  lines.push(`Closed: ${entry.closed_at}`);
  lines.push(
    entry.removed
      ? "The worktree was deleted. Reopening recreates it after you confirm."
      : "Only the window was closed. Reopening opens the folder again.",
  );
  return lines.join("\n");
}

/** Abbreviates a commit id for display. */
export function shortSha(sha: string): string {
  return sha.slice(0, SHORT_SHA_LEN);
}

/**
 * Whether two lists are the same — compared by serialised value, like the other
 * "push in a value, fire only on change" setters. Both orderings matter: the list is
 * newest-first, so a reorder is a real change.
 */
export function sameClosed(a: ClosedWorktreePayload[], b: ClosedWorktreePayload[]): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

/** One quick-pick entry: the row text plus the recorded path the pick reopens. */
export interface ClosedPickItem {
  label: string;
  description: string;
  detail: string;
  path: string;
}

/**
 * The quick-pick entries for `closed`, in the order received (newest first). The
 * detail line carries the full path, which the label deliberately omits.
 */
export function closedPickItems(closed: ClosedWorktreePayload[], now: Date): ClosedPickItem[] {
  return closed.map((entry) => ({
    label: closedLabel(entry),
    description: closedDescription(entry, now),
    detail: entry.path,
    path: entry.path,
  }));
}

/** The entry recorded for `path`, if any. */
export function findClosed(
  closed: ClosedWorktreePayload[],
  entryPath: string,
): ClosedWorktreePayload | undefined {
  return closed.find((entry) => entry.path === entryPath);
}

/** A short name for `entry` in messages: its branch, else its folder name. */
export function closedShortName(entry: ClosedWorktreePayload): string {
  return entry.branch ?? path.basename(entry.path);
}

/** The text of the "nothing to reopen" notice. */
export const NOTHING_TO_REOPEN = "omni-dev: no recently closed worktrees to reopen.";

/** The error shown when the daemon says a removed worktree cannot be recreated. */
export function notRestorableMessage(entry: ClosedWorktreePayload, plan: ReopenPlan): string {
  const why = plan.reason?.trim() || "it can no longer be recreated";
  return `omni-dev: cannot reopen “${closedLabel(entry)}” — ${why}.`;
}

/** The modal confirmation for recreating a removed worktree. */
export interface ReopenConfirmation {
  /** The modal's headline. */
  message: string;
  /** The modal's body: branch, destination, source, warnings, then the caveat. */
  detail: string;
  /** The confirming button — the cancel path is the modal's own. */
  confirmLabel: string;
}

/**
 * The confirmation shown before a removed worktree is recreated (#2211).
 *
 * Names what the user is agreeing to — the branch, **where** it will be recreated
 * (the recorded path, never a choice), and whether it comes from the branch itself
 * or from the commit the worktree was last at because the branch has since been
 * deleted — then any warnings the daemon raised, then {@link UNCOMMITTED_CAVEAT},
 * which is unconditional. The plan's own `branch`/`head_sha` win over the recorded
 * ones, since the daemon re-derived them just now.
 */
export function reopenConfirmation(
  entry: ClosedWorktreePayload,
  plan: ReopenPlan,
): ReopenConfirmation {
  const branch = plan.branch ?? entry.branch;
  const sha = plan.head_sha ?? entry.head_sha;
  const lines: string[] = [];

  lines.push(branch ? `Branch: ${branch}` : "Branch: (detached HEAD)");
  lines.push(`Recreated at: ${entry.path}`);
  if (plan.source === "head-sha") {
    const commit = sha ? `the recorded commit ${shortSha(sha)}` : "the recorded commit";
    const gone = branch ? `; the branch “${branch}” no longer exists` : "";
    lines.push(`Source: ${commit}${gone}`);
  } else if (plan.source === "branch") {
    lines.push(`Source: the existing branch${branch ? ` “${branch}”` : ""}`);
  }
  const warnings = plan.warnings ?? [];
  if (warnings.length > 0) {
    lines.push("", ...warnings.map((w) => `• ${w}`));
  }
  lines.push("", UNCOMMITTED_CAVEAT);

  return {
    message: `Recreate worktree “${closedLabel(entry)}”?`,
    detail: lines.join("\n"),
    confirmLabel: "Recreate Worktree",
  };
}

/** The toast text after a successful reopen. */
export function reopenedMessage(entry: ClosedWorktreePayload, recreated: boolean): string {
  return recreated
    ? `omni-dev: recreated and reopened “${closedLabel(entry)}”.`
    : `omni-dev: reopened “${closedLabel(entry)}”.`;
}
