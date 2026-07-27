// The pure clipboard model for the "Copy PR URL" action (#1430): turning a
// selection of tree rows plus the pull requests discovered for them into the
// block of text that lands on the clipboard.
//
// Deliberately free of any `vscode` import, so it stays unit-testable under
// `node --test` (the `rebaseReport.ts` / `sessionCounts.ts` split). The
// `vscode`-facing half — the progress notification, the clipboard write, the
// status bar message — lives in `prCommands.ts` and does nothing but call here.
//
// The one structural idea: discovery is per **scope** (`prScopesForNodes`
// collapses two worktrees on the same branch into one `gh` call), but this
// command reports per **row**. So the caller hands us the outcomes keyed by
// `prScopeKey` and we join them back onto the selected nodes, in selection
// order. That join is what lets a row which contributed no PR still get a line —
// the difference between a list you can trust and one that lies by omission.

import { PullRequest, dedupePullRequests, prScopeForNode, prScopeKey } from "./github";
import { Node, repoLabel, worktreeBranchLabel } from "./tree";

/**
 * What discovery came back with for one scope. A **failed** lookup is
 * deliberately distinct from an empty one: a transient `gh`/daemon failure must
 * never be pasted as a settled "this row has no PR".
 */
export type PrLookup = { status: "ok"; prs: PullRequest[] } | { status: "failed" };

/** Discovery outcomes keyed by {@link prScopeKey}, as the join expects them. */
export type PrLookupMap = ReadonlyMap<string, PrLookup>;

/**
 * Placeholder lines are commented with `#` so the block stays paste-safe into a
 * shell, a YAML/TOML scratch file, or a markdown list without a placeholder
 * being mistaken for a link.
 */
const COMMENT = "#";

/** What a placeholder names the row by: a repo's label, a worktree's branch and path. */
function rowSubject(node: Node): string {
  return node.kind === "repo"
    ? repoLabel(node.repo)
    : `${worktreeBranchLabel(node.wt)} in ${node.wt.path}`;
}

/**
 * The line for a row with no pull request. A worktree scope looks for the one PR
 * heading its branch; a repo scope lists the whole repository — so their empty
 * cases say different things.
 *
 * The path is **absolute**, matching "Copy Directory": the block is meant to be
 * pasted somewhere with no tree beside it, where a bare `issue-1428` names nothing.
 */
function emptyLine(node: Node): string {
  return node.kind === "repo"
    ? `${COMMENT} No open PRs for ${rowSubject(node)}`
    : `${COMMENT} No PR for ${rowSubject(node)}`;
}

/** The line for a row whose lookup failed outright. */
function failedLine(node: Node): string {
  return `${COMMENT} PR lookup failed for ${rowSubject(node)}`;
}

/**
 * One line per selected row, in selection order: the PR's URL when the row has
 * one, a `#`-commented placeholder naming the row when it does not.
 *
 * A row with no GitHub identity has no scope to look up at all; it is not an
 * error but an honest negative, so it gets the ordinary placeholder. (A scope
 * missing from `byScope` is treated the same, defensively — it cannot happen for
 * a caller that discovers every scope of the selection.)
 *
 * **URLs de-duplicate across the whole block; placeholders never do.** Selecting
 * a repo row together with one of its own worktree rows otherwise repeats that
 * PR, whereas two PR-less worktrees each deserve a line — they name different
 * worktrees. A row whose every URL was already emitted therefore contributes
 * *nothing*, not a placeholder: the PR is in the block already, so claiming the
 * row has none would be flatly false. An emptiness placeholder is only ever
 * minted for a lookup that genuinely found no open PR.
 */
export function prClipboardLines(nodes: Node[], byScope: PrLookupMap): string[] {
  const seen = new Set<string>();
  const lines: string[] = [];
  for (const node of nodes) {
    const scope = prScopeForNode(node);
    const lookup = scope ? byScope.get(prScopeKey(scope)) : undefined;
    if (lookup?.status === "failed") {
      lines.push(failedLine(node));
      continue;
    }
    const prs = lookup?.prs ?? [];
    if (prs.length === 0) {
      lines.push(emptyLine(node));
      continue;
    }
    for (const pr of prs) {
      if (seen.has(pr.url)) {
        continue;
      }
      seen.add(pr.url);
      lines.push(pr.url);
    }
  }
  return lines;
}

/**
 * The clipboard block for a set of lines. The single definition of the block's
 * shape — newline-separated, no trailing newline — shared by "Copy PR URL" and
 * the "Copy PR URL" action on the missing-extension warning, so the two cannot
 * drift.
 */
export function prClipboardText(lines: string[]): string {
  return lines.join("\n");
}

/**
 * The clipboard block for pull requests that are **already resolved** — the
 * missing-extension warning's action, which is handed the PRs a selection
 * discovered and so has no rows left to write a placeholder for.
 */
export function prUrlsText(prs: PullRequest[]): string {
  return prClipboardText(dedupePullRequests(prs).map((pr) => pr.url));
}

/** How many of the block's lines are actual URLs, for the status bar message. */
export function prUrlCount(lines: string[]): number {
  return lines.filter((line) => !line.startsWith(COMMENT)).length;
}

/**
 * The status bar summary for a copied block. The placeholders are counted out
 * loud rather than folded into one total: "Copied 6 PR URLs" over a block that
 * is half comments would misreport what was actually gathered, which is the very
 * omission the placeholders exist to prevent.
 */
export function prCopySummary(lines: string[]): string {
  const urls = prUrlCount(lines);
  const placeholders = lines.length - urls;
  const parts: string[] = [];
  if (urls > 0) {
    parts.push(`${urls} PR URL${urls === 1 ? "" : "s"}`);
  }
  if (placeholders > 0) {
    parts.push(`${placeholders} placeholder${placeholders === 1 ? "" : "s"}`);
  }
  return `Copied ${parts.join(" and ")}`;
}
