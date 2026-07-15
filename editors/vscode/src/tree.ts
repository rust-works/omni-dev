// The pure repo/worktree tree model and its label/description/tooltip formatters.
//
// This module is deliberately free of any `vscode` import so it stays pure and
// unit-testable under `node --test` (like `socket.ts`). The `vscode`-facing
// `TreeDataProvider` (`treeDataProvider.ts`) consumes the `Node` union and the
// formatters here and maps them onto `vscode.TreeItem`s. The types mirror the
// daemon's `tree` op payload (`src/daemon/services/worktrees.rs`): every field
// the daemon marks `skip_serializing_if` is optional here (absent, never null).

import * as path from "path";

/** A GitHub `owner/name` identity, present only for `github.com` origins. */
export interface TreeGithubIdentity {
  owner: string;
  name: string;
}

/**
 * The rolled-up CI state of a pull request's head commit, reduced from `gh`'s
 * `statusCheckRollup` to a single verdict (#1296): `success` (all checks passed),
 * `failure` (at least one failed/errored/cancelled), `pending` (some still
 * running, none failed), or `none` (no checks configured).
 */
export type PrCheckState = "success" | "failure" | "pending" | "none";

/**
 * The pull-request badge folded onto a worktree node (#1296). Derived
 * extension-side from `gh pr list` (see `github.ts`), it is the minimum needed to
 * render `#<number>` plus a draft marker and a checks glyph, with `url` kept for
 * the tooltip and any future open-on-click.
 */
export interface PrBadge {
  number: number;
  isDraft: boolean;
  checks: PrCheckState;
  url: string;
}

/** One worktree of a repository, as it appears in the `tree` payload. */
export interface TreeWorktreePayload {
  /** Absolute path to the worktree's working directory. */
  path: string;
  /** The checked-out branch, or absent when detached/unborn. */
  branch?: string;
  /**
   * The commit HEAD points at, or absent when unborn (present even on a detached
   * HEAD). Unlike {@link TreeWorktreePayload.ahead} this **is** carried by the
   * streamed snapshot: it is what makes a new commit a visible delta, so a push
   * re-renders instead of being dropped by the daemon's snapshot diff (#1337).
   * Absent from a pre-#1337 daemon.
   */
  head_sha?: string;
  /**
   * Commits ahead of upstream. **Not** carried by the streamed `tree`/`subscribe`
   * snapshot — it is fetched lazily via the `ahead-behind` op on expand and folded
   * in by {@link withAheadBehind} (#1306). Absent without an upstream, or until
   * fetched.
   */
  ahead?: number;
  /** Commits behind upstream. Lazily fetched like {@link TreeWorktreePayload.ahead}. */
  behind?: number;
  /** Whether this is the repo's main working tree (vs a linked worktree). */
  is_main: boolean;
  /** Whether a live VS Code window currently has this worktree open. */
  open: boolean;
  /** The open window's registry key, present only when `open`. */
  window_key?: string;
  /**
   * The open pull request whose head is this worktree's branch (#1296). Like
   * {@link TreeWorktreePayload.ahead}, it is **not** on the daemon wire — it is
   * resolved extension-side via `gh` on repo-expand and folded in by
   * {@link withPr}. Absent for a detached/non-GitHub worktree, or one with no
   * open PR (or until fetched).
   */
  pr?: PrBadge;
}

/**
 * Ahead/behind divergence of one worktree, as returned by the `ahead-behind` op
 * (#1306). Both counts are absent when the branch tracks no upstream.
 */
export interface AheadBehind {
  ahead?: number;
  behind?: number;
}

/** The `ahead-behind` op's `results`: divergence keyed by worktree path. */
export type AheadBehindMap = Record<string, AheadBehind>;

/**
 * Folds a lazily-fetched {@link AheadBehind} into a worktree payload, returning a
 * new payload with the counts applied (#1306). An absent entry — no upstream, or
 * not yet fetched — leaves the worktree unchanged, so it renders with no sync
 * indicator, exactly as an eager snapshot did for a branch with no upstream.
 */
export function withAheadBehind(
  wt: TreeWorktreePayload,
  ab?: AheadBehind,
): TreeWorktreePayload {
  if (ab === undefined || (ab.ahead === undefined && ab.behind === undefined)) {
    return wt;
  }
  return { ...wt, ahead: ab.ahead, behind: ab.behind };
}

/**
 * Folds a lazily-resolved {@link PrBadge} into a worktree payload, returning a new
 * payload carrying it (#1296) — the PR counterpart of {@link withAheadBehind}. An
 * absent badge (no GitHub identity, no matching PR, or not yet fetched) leaves the
 * worktree unchanged, so it renders with no PR indicator.
 */
export function withPr(wt: TreeWorktreePayload, pr?: PrBadge): TreeWorktreePayload {
  if (pr === undefined) {
    return wt;
  }
  return { ...wt, pr };
}

/** One repository with **all** its worktrees, as it appears in the `tree` payload. */
export interface TreeRepoPayload {
  /** The main repository's directory name (daemon-computed). */
  main_repo: string;
  /** GitHub identity of `origin`, when it is a `github.com` remote. */
  github?: TreeGithubIdentity;
  /** Absolute path to the main working tree — the repo root. */
  root: string;
  /** Every worktree of the repo: main working tree first, then linked. */
  worktrees: TreeWorktreePayload[];
}

/** The full `tree` op / subscription snapshot payload. */
export interface TreeSnapshot {
  repos: TreeRepoPayload[];
  /**
   * The daemon-backed show/hide-closed toggle (#1301): whether the tree shows
   * worktrees with no open window. A single cross-window value the daemon
   * carries in every snapshot so all windows render — and live-sync — the same
   * state. Optional for forward-compatibility: an older daemon that omits it is
   * read as `true` (show all, the original behavior).
   */
  show_closed?: boolean;
}

/**
 * A node in the two-level tree: a repository, or one worktree of it. A worktree
 * node carries its parent `repo` so formatters (tooltip) and actions have the
 * full context without a second lookup.
 */
export type Node =
  | { kind: "repo"; repo: TreeRepoPayload }
  | { kind: "worktree"; repo: TreeRepoPayload; wt: TreeWorktreePayload };

/** The top-level repository nodes, in the daemon's (already deterministic) order. */
export function reposToNodes(repos: TreeRepoPayload[]): Node[] {
  return repos.map((repo) => ({ kind: "repo", repo }));
}

/**
 * The worktree child nodes of a repository, in the daemon's order (main first).
 *
 * When `showClosed` is false, worktrees with no open window (`open === false`)
 * are dropped so the view collapses to just what is actually open. The daemon
 * derives repos from open windows, so every repo keeps ≥1 open worktree — the
 * filter can never empty a repo or the tree. `showClosed` defaults to `true`
 * (the current, unfiltered behavior).
 */
export function worktreeNodes(repo: TreeRepoPayload, showClosed = true): Node[] {
  const worktrees = showClosed ? repo.worktrees : repo.worktrees.filter((wt) => wt.open);
  return worktrees.map((wt) => ({ kind: "worktree", repo, wt }));
}

/** A repo's display label: `owner/name` for GitHub repos, else its `main_repo`. */
export function repoLabel(repo: TreeRepoPayload): string {
  return repo.github ? `${repo.github.owner}/${repo.github.name}` : repo.main_repo;
}

/**
 * Whether this worktree is the one open in **the current** VS Code window — the
 * leaf whose registry `window_key` matches this window's own `windowKey`. Used to
 * mark it distinctly (a blue tick) from worktrees open in *other* windows. Stays
 * `vscode`-free so it is unit-testable. An absent `windowKey` (or a worktree with
 * no live window) never matches.
 */
export function isCurrentWindow(wt: TreeWorktreePayload, windowKey?: string): boolean {
  return wt.open && windowKey !== undefined && wt.window_key === windowKey;
}

/**
 * A worktree's primary label: its branch, or the folder basename when detached
 * or unborn (no branch reported).
 */
export function worktreeLabel(wt: TreeWorktreePayload): string {
  return wt.branch ?? path.basename(wt.path);
}

/**
 * The muted sync counts, e.g. `↑2 ↓0`. Each side is shown only when its count is
 * present (both are absent together when the branch has no upstream), so a
 * no-upstream worktree yields an empty string.
 */
function syncCounts(wt: TreeWorktreePayload): string {
  const parts: string[] = [];
  if (wt.ahead !== undefined) {
    parts.push(`↑${wt.ahead}`);
  }
  if (wt.behind !== undefined) {
    parts.push(`↓${wt.behind}`);
  }
  return parts.join(" ");
}

/**
 * The muted PR badge, e.g. `#65` or `#65 draft` (#1296). Empty when the worktree
 * carries no {@link PrBadge}, so a worktree with no open PR renders no PR
 * indicator. The CI-checks verdict is **not** in the badge text: since #1324 it is
 * rendered separately as a colored file decoration (see
 * {@link worktreeCheckDecoration}), so the glyph is never shown twice.
 */
export function worktreePrBadge(wt: TreeWorktreePayload): string {
  const pr = wt.pr;
  if (pr === undefined) {
    return "";
  }
  const parts = [`#${pr.number}`];
  if (pr.isDraft) {
    parts.push("draft");
  }
  return parts.join(" ");
}

/**
 * The muted row description: the sync counts and the PR badge, each shown only
 * when present, separated by a gap. A worktree with neither (no upstream, no PR)
 * yields an empty description — byte-for-byte the pre-#1296 behavior.
 */
export function worktreeDescription(wt: TreeWorktreePayload): string {
  return [syncCounts(wt), worktreePrBadge(wt)].filter(Boolean).join("  ");
}

/**
 * A worktree row's colored PR-check badge (#1324): the glyph, its theme color id,
 * and a hover tooltip. The `vscode`-facing `FileDecorationProvider`
 * (`decorations.ts`) maps these fields onto a `vscode.FileDecoration`, so
 * pass/fail/pending reads at a glance instead of from the monochrome glyph the
 * description used to carry.
 */
export interface CheckDecoration {
  /** The single-character badge glyph (`✓` / `✗` / `●`). */
  badge: string;
  /** A `ThemeColor` id from the `charts.*` palette, so the badge is theme-aware. */
  colorId: string;
  /** The decoration's hover tooltip, e.g. `checks passing`. */
  tooltip: string;
}

/**
 * Maps a rolled-up PR {@link PrCheckState} to its colored badge decoration (#1324):
 * a green `✓` (passing), red `✗` (failing), or yellow `●` (pending). A PR with no
 * checks configured (`none`) gets `undefined` — no badge. The `charts.{green,red,
 * yellow}` color ids match the palette the open-state icon already uses
 * (`treeDataProvider.ts`), so the badge adapts to light/dark themes. This is the
 * state-facing half, used by the decoration provider on the `checks=<state>` it
 * reads back from a row's `resourceUri`.
 */
export function checkStateDecoration(checks: PrCheckState): CheckDecoration | undefined {
  switch (checks) {
    case "success":
      return { badge: "✓", colorId: "charts.green", tooltip: "checks passing" };
    case "failure":
      return { badge: "✗", colorId: "charts.red", tooltip: "checks failing" };
    case "pending":
      return { badge: "●", colorId: "charts.yellow", tooltip: "checks pending" };
    case "none":
      return undefined;
  }
}

/**
 * The check decoration for a worktree row (#1324), or `undefined` when it has no
 * PR — or a PR with no checks — the cases that render no badge. The worktree-facing
 * half of {@link checkStateDecoration}: the tree provider uses it to decide whether
 * a row gets a decoratable `resourceUri`.
 */
export function worktreeCheckDecoration(wt: TreeWorktreePayload): CheckDecoration | undefined {
  return wt.pr ? checkStateDecoration(wt.pr.checks) : undefined;
}

/** The word for a rolled-up checks state, or `""` when there are no checks. */
function prChecksWord(checks: PrCheckState): string {
  switch (checks) {
    case "success":
      return "checks passing";
    case "failure":
      return "checks failing";
    case "pending":
      return "checks pending";
    case "none":
      return "";
  }
}

/** The tooltip's PR line, e.g. `PR #65 · open · checks passing`, or `undefined`. */
function worktreePrTooltipLine(wt: TreeWorktreePayload): string | undefined {
  const pr = wt.pr;
  if (pr === undefined) {
    return undefined;
  }
  const parts = [`PR #${pr.number}`, pr.isDraft ? "draft" : "open"];
  const checks = prChecksWord(pr.checks);
  if (checks) {
    parts.push(checks);
  }
  return parts.join(" · ");
}

/**
 * A multi-line hover tooltip: path, main/linked, branch+sync, the PR (when one is
 * resolved), parent repo, open state. The open line distinguishes the current
 * window (`● this window`) from a worktree merely open elsewhere (`● window open`)
 * when `windowKey` is supplied.
 */
export function worktreeTooltip(
  wt: TreeWorktreePayload,
  repo: TreeRepoPayload,
  windowKey?: string,
): string {
  const kind = wt.is_main ? "main working tree" : "linked worktree";
  const branch = wt.branch ?? "(detached)";
  const sync = syncCounts(wt);
  const branchLine = sync ? `${branch}  ${sync}` : branch;
  const openLine = isCurrentWindow(wt, windowKey)
    ? "● this window"
    : wt.open
      ? "● window open"
      : "no window open";
  const lines = [wt.path, `${kind} of ${repoLabel(repo)}`, branchLine];
  const prLine = worktreePrTooltipLine(wt);
  if (prLine) {
    lines.push(prLine);
  }
  lines.push(openLine);
  return lines.join("\n");
}

/**
 * The `contextValue` used to gate context-menu items and mark the open badge.
 * Encodes three orthogonal facts as dotted segments:
 *
 *  - **open state** — `worktree.current` (this window), `worktree.open`
 *    (another window), or bare `worktree` (no window);
 *  - **structural role** — a `.main` (the repository's main working tree) or
 *    `.linked` (a linked worktree), which the daemon reports as `is_main` and
 *    which decides deletability (never the branch name);
 *  - **GitHub identity** — a trailing `.github` when the parent repo has a
 *    `github.com` origin (so the "Open Pull Request…" menu can gate on it),
 *    appended only when `hasGithub` is set.
 *
 * So every value starts with `worktree` — the existing `viewItem =~ /worktree/`
 * "open" menu still matches all variants — while the close menus gate on the
 * role: **Close Window** on `/worktree\.(current|open)\.main/` (a main tree with
 * a window) and **Close Worktree** on `/worktree\..*linked/` (any linked
 * worktree). A main tree with no window matches neither, so nothing is offered.
 * The trailing `.github` is appended last so those (unanchored) role regexes are
 * unaffected, and `hasGithub` defaults to `false` so a non-GitHub repo's values
 * stay byte-for-byte as before.
 */
export function worktreeContextValue(
  wt: TreeWorktreePayload,
  windowKey?: string,
  hasGithub = false,
): string {
  const role = wt.is_main ? "main" : "linked";
  const base = isCurrentWindow(wt, windowKey)
    ? `worktree.current.${role}`
    : wt.open
      ? `worktree.open.${role}`
      : `worktree.${role}`;
  return hasGithub ? `${base}.github` : base;
}

/**
 * A stable per-node identity: the repo root or the worktree path. Used both as
 * the `vscode.TreeItem.id` and as the key the double-click timer matches on.
 */
export function nodeId(node: Node): string {
  return node.kind === "repo" ? `repo:${node.repo.root}` : `wt:${node.wt.path}`;
}
