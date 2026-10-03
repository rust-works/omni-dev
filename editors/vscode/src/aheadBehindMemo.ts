// The per-window memo for the daemon's lazy `ahead-behind` op (#2120): skip the
// request when nothing a row shows can have moved.
//
// Every window used to re-ask the daemon for every worktree of every expanded repo
// on *every* tree refresh — including refreshes that cannot change a count (a CI
// verdict landing, a session transition, a colour edit, another window opening a
// worktree). #2111 made each request cheap on the daemon; this removes the
// requests.
//
// Nothing here imports `vscode`, so it runs under a plain Node process
// (`node --test out/`) like `sessionCounts.ts` and `tree.ts`. The `vscode`-facing
// half is `treeDataProvider.ts`, which owns one {@link AheadBehindMemo} and calls
// {@link AheadBehindMemo.resolve} where it used to call the fetcher directly.
//
// Exactness is the property to protect (it is what #2115 deliberately preserved), so
// there is **no TTL**: a cached answer is served only while every input the daemon
// computes it from is unchanged. A TTL could not help anyway — nothing re-runs
// `getChildren` without a snapshot delta, which is why the daemon publishes the
// default-branch tip as `main_sha` instead.

import { AheadBehind, AheadBehindMap, TreeRepoPayload, TreeWorktreePayload } from "./tree";

/**
 * Fetches divergence for a batch of worktree paths. Resolves `undefined` — not an
 * empty map — when the answer could not be obtained (daemon unreachable, an older
 * daemon without the op, an `ok: false` reply): the memo must be able to tell a
 * failure, which it never caches, from a successful reply whose rows were all
 * omitted, which it does.
 */
export type AheadBehindBatchFetcher = (paths: string[]) => Promise<AheadBehindMap | undefined>;

/** One worktree to resolve, with the repo whose default-branch tip it depends on. */
export interface AheadBehindTarget {
  wt: TreeWorktreePayload;
  repo: Pick<TreeRepoPayload, "main_sha">;
}

/**
 * Everything the daemon computes a worktree's `ahead`/`behind`/`main_behind` from,
 * as the snapshot reports it: the checked-out branch (whose configured upstream
 * defines the first answer), the commit HEAD is at, the commit that upstream is at,
 * and the tip of the remote default branch. Equal keys mean equal answers.
 *
 * The branch name is there because the oids only *proxy* "which upstream" — a
 * branch switch can land on the same commit, and the issue names it as a case that
 * must update. The key is a JSON array, so an absent part cannot collide with a
 * present one (`[a, null, b]` vs `[null, a, b]`). The worktree's path is not part of
 * it: the memo is keyed by path already.
 */
export function aheadBehindKey(target: AheadBehindTarget): string {
  const { wt, repo } = target;
  return JSON.stringify([
    wt.branch ?? null,
    wt.head_sha ?? null,
    wt.upstream_sha ?? null,
    repo.main_sha ?? null,
  ]);
}

/**
 * Whether `result` may be cached under {@link aheadBehindKey}: only when the key
 * carries every input the result evidences, so nothing is ever served that a key
 * change could not have invalidated.
 *
 * This is what makes the memo safe against an older daemon without a version
 * probe. A daemon that predates a field omits it, and an omitted key part reads the
 * same as "legitimately absent" — so instead of guessing, the result is held to what
 * the key can see:
 *
 * - `head_sha` is always required (a pre-#1337 daemon omits it; an unborn HEAD has
 *   none, and its answer is empty and cheap anyway);
 * - `ahead`/`behind` imply an upstream, so they need `upstream_sha` (a pre-#1344
 *   daemon omits it, which left a push invisible);
 * - `main_behind` implies a resolved default branch, so it needs `main_sha` (a
 *   pre-#2120 daemon omits it, which left a default-branch fetch invisible);
 * - a `shallow` row never qualifies, since deepening a clone changes the counts
 *   without moving any id.
 */
export function isMemoizable(target: AheadBehindTarget, result: AheadBehind): boolean {
  const { wt, repo } = target;
  if (wt.head_sha === undefined || result.shallow === true) {
    return false;
  }
  if ((result.ahead !== undefined || result.behind !== undefined) && wt.upstream_sha === undefined) {
    return false;
  }
  return result.main_behind === undefined || repo.main_sha !== undefined;
}

/**
 * The paths among `targets` that need fetching: those with no cached (or in-flight)
 * entry, or whose entry was recorded under a different key. The pure
 * changed-paths selection — everything else keeps being served from the memo.
 */
export function stalePaths(
  cached: ReadonlyMap<string, { readonly key: string }>,
  targets: readonly AheadBehindTarget[],
): string[] {
  return targets
    .filter((target) => cached.get(target.wt.path)?.key !== aheadBehindKey(target))
    .map((target) => target.wt.path);
}

/**
 * What a path's entry resolves to: the daemon's row for it (`{}` when it omitted
 * one — "nothing to show"), or `undefined` when the fetch failed.
 */
type Outcome = AheadBehind | undefined;

interface Entry {
  readonly key: string;
  readonly outcome: Promise<Outcome>;
}

/**
 * A per-window memo of `ahead-behind` answers, keyed by worktree path and
 * validated by {@link aheadBehindKey}.
 *
 * Holds promises rather than values, so a request already in flight for a path under
 * the same key is **joined** instead of repeated — two refreshes in quick succession
 * cost one batch.
 */
export class AheadBehindMemo {
  private readonly entries = new Map<string, Entry>();

  constructor(private readonly fetch: AheadBehindBatchFetcher) {}

  /**
   * Divergence for each of `targets`, fetching only the paths whose key moved (in
   * **one** batch) and serving the rest from the memo. A path whose fetch failed is
   * simply absent from the result — the row renders without a sync indicator, as
   * it always did — and is retried by the next call.
   */
  async resolve(targets: readonly AheadBehindTarget[]): Promise<AheadBehindMap> {
    const stale = new Set(stalePaths(this.entries, targets));
    const toFetch = targets.filter((target) => stale.has(target.wt.path));
    if (toFetch.length > 0) {
      this.fetchBatch(toFetch);
    }
    // Captured before awaiting, so a later call replacing an entry cannot swap the
    // answer out from under this one.
    const slots = targets.map((target) => ({
      path: target.wt.path,
      entry: this.entries.get(target.wt.path),
    }));
    const results: AheadBehindMap = {};
    await Promise.all(
      slots.map(async ({ path, entry }) => {
        const outcome = await entry?.outcome;
        if (outcome !== undefined) {
          results[path] = outcome;
        }
      }),
    );
    return results;
  }

  /** Forgets every path not in `livePaths`, so a removed worktree cannot leak. */
  prune(livePaths: Iterable<string>): void {
    const live = new Set(livePaths);
    for (const path of [...this.entries.keys()]) {
      if (!live.has(path)) {
        this.entries.delete(path);
      }
    }
  }

  /** How many paths are remembered (settled or in flight), for tests. */
  get size(): number {
    return this.entries.size;
  }

  /**
   * Issues one fetch for `targets` and registers an entry per path *synchronously*,
   * so a concurrent {@link resolve} sees them as in flight. Each entry evicts itself
   * once settled unless its outcome is a success {@link isMemoizable} accepts.
   */
  private fetchBatch(targets: readonly AheadBehindTarget[]): void {
    const batch = this.fetch(targets.map((target) => target.wt.path)).catch(
      (): AheadBehindMap | undefined => undefined,
    );
    for (const target of targets) {
      const path = target.wt.path;
      const entry: Entry = {
        key: aheadBehindKey(target),
        outcome: batch.then((map): Outcome => (map === undefined ? undefined : (map[path] ?? {}))),
      };
      this.entries.set(path, entry);
      void entry.outcome.then((outcome) => {
        // Superseded (the key moved while this was in flight) or pruned: not ours.
        if (this.entries.get(path) !== entry) {
          return;
        }
        if (outcome === undefined || !isMemoizable(target, outcome)) {
          this.entries.delete(path);
        }
      });
    }
  }
}
