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

/** One worktree of a repository, as it appears in the `tree` payload. */
export interface TreeWorktreePayload {
  /** Absolute path to the worktree's working directory. */
  path: string;
  /** The checked-out branch, or absent when detached/unborn. */
  branch?: string;
  /** Commits ahead of upstream (absent without an upstream). */
  ahead?: number;
  /** Commits behind upstream (absent without an upstream). */
  behind?: number;
  /** Whether this is the repo's main working tree (vs a linked worktree). */
  is_main: boolean;
  /** Whether a live VS Code window currently has this worktree open. */
  open: boolean;
  /** The open window's registry key, present only when `open`. */
  window_key?: string;
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

/** The worktree child nodes of a repository, in the daemon's order (main first). */
export function worktreeNodes(repo: TreeRepoPayload): Node[] {
  return repo.worktrees.map((wt) => ({ kind: "worktree", repo, wt }));
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
 * The muted sync description, e.g. `↑2 ↓0`. Each side is shown only when its
 * count is present (both are absent together when the branch has no upstream),
 * so a no-upstream worktree yields an empty description.
 */
export function worktreeDescription(wt: TreeWorktreePayload): string {
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
 * A multi-line hover tooltip: path, main/linked, branch+sync, parent repo, open
 * state. The open line distinguishes the current window (`● this window`) from a
 * worktree merely open elsewhere (`● window open`) when `windowKey` is supplied.
 */
export function worktreeTooltip(
  wt: TreeWorktreePayload,
  repo: TreeRepoPayload,
  windowKey?: string,
): string {
  const kind = wt.is_main ? "main working tree" : "linked worktree";
  const branch = wt.branch ?? "(detached)";
  const sync = worktreeDescription(wt);
  const branchLine = sync ? `${branch}  ${sync}` : branch;
  const openLine = isCurrentWindow(wt, windowKey)
    ? "● this window"
    : wt.open
      ? "● window open"
      : "no window open";
  return [wt.path, `${kind} of ${repoLabel(repo)}`, branchLine, openLine].join("\n");
}

/**
 * The `contextValue` used to gate context-menu items and mark the open badge.
 * Encodes two orthogonal facts as dotted segments:
 *
 *  - **open state** — `worktree.current` (this window), `worktree.open`
 *    (another window), or bare `worktree` (no window);
 *  - **structural role** — a trailing `.main` (the repository's main working
 *    tree) or `.linked` (a linked worktree), which the daemon reports as
 *    `is_main` and which decides deletability (never the branch name).
 *
 * So every value starts with `worktree` — the existing `viewItem =~ /worktree/`
 * "open" menu still matches all six variants — while the close menus gate on the
 * role: **Close Window** on `/worktree\.(current|open)\.main/` (a main tree with
 * a window) and **Close Worktree** on `/worktree\..*linked/` (any linked
 * worktree). A main tree with no window matches neither, so nothing is offered.
 */
export function worktreeContextValue(wt: TreeWorktreePayload, windowKey?: string): string {
  const role = wt.is_main ? "main" : "linked";
  if (isCurrentWindow(wt, windowKey)) {
    return `worktree.current.${role}`;
  }
  return wt.open ? `worktree.open.${role}` : `worktree.${role}`;
}

/**
 * A stable per-node identity: the repo root or the worktree path. Used both as
 * the `vscode.TreeItem.id` and as the key the double-click timer matches on.
 */
export function nodeId(node: Node): string {
  return node.kind === "repo" ? `repo:${node.repo.root}` : `wt:${node.wt.path}`;
}
