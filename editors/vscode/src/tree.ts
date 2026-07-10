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

/** A multi-line hover tooltip: path, main/linked, branch+sync, parent repo, open state. */
export function worktreeTooltip(wt: TreeWorktreePayload, repo: TreeRepoPayload): string {
  const kind = wt.is_main ? "main working tree" : "linked worktree";
  const branch = wt.branch ?? "(detached)";
  const sync = worktreeDescription(wt);
  const branchLine = sync ? `${branch}  ${sync}` : branch;
  const openLine = wt.open ? "● window open" : "no window open";
  return [wt.path, `${kind} of ${repoLabel(repo)}`, branchLine, openLine].join("\n");
}

/**
 * The `contextValue` used to gate context-menu items and mark the open badge.
 * Both start with `worktree` so a single `viewItem =~ /worktree/` menu `when`
 * matches open and closed alike.
 */
export function worktreeContextValue(wt: TreeWorktreePayload): string {
  return wt.open ? "worktree.open" : "worktree";
}

/**
 * A stable per-node identity: the repo root or the worktree path. Used both as
 * the `vscode.TreeItem.id` and as the key the double-click timer matches on.
 */
export function nodeId(node: Node): string {
  return node.kind === "repo" ? `repo:${node.repo.root}` : `wt:${node.wt.path}`;
}
