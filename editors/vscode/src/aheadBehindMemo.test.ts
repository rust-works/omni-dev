// Unit tests for the `ahead-behind` memo (#2120). Nothing here imports `vscode`,
// so it runs under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import { test } from "node:test";

import {
  AheadBehindBatchFetcher,
  AheadBehindMemo,
  AheadBehindTarget,
  aheadBehindKey,
  isMemoizable,
  stalePaths,
} from "./aheadBehindMemo";
import { AheadBehindMap, TreeWorktreePayload } from "./tree";

/**
 * A worktree with every key input present, overridable per test. `main_sha` is
 * `null` — not `undefined`, which would just select the default — for a repo whose
 * snapshot carries none (an older daemon, or no resolvable default branch).
 */
function target(
  path: string,
  wt: Partial<TreeWorktreePayload> = {},
  main_sha: string | null = "m1",
): AheadBehindTarget {
  return {
    wt: {
      path,
      branch: "feat",
      head_sha: "h1",
      upstream_sha: "u1",
      is_main: false,
      open: true,
      ...wt,
    },
    repo: { main_sha: main_sha ?? undefined },
  };
}

/** A fetcher that answers from `rows` and records every batch it was asked for. */
function fakeFetcher(rows: AheadBehindMap = {}) {
  const calls: string[][] = [];
  const fetch: AheadBehindBatchFetcher = async (paths) => {
    calls.push([...paths]);
    return Object.fromEntries(paths.filter((p) => p in rows).map((p) => [p, rows[p]]));
  };
  return { fetch, calls };
}

/** A promise settled by hand, for holding a fetch in flight. */
function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((res) => {
    resolve = res;
  });
  return { promise, resolve };
}

const ROWS: AheadBehindMap = {
  "/w/a": { ahead: 1, behind: 2, main_behind: 3 },
  "/w/b": { ahead: 0, behind: 0 },
  "/w/c": { ahead: 4, behind: 0 },
};

// --- aheadBehindKey ---------------------------------------------------------

test("aheadBehindKey is stable for equal inputs and ignores the path", () => {
  assert.equal(aheadBehindKey(target("/w/a")), aheadBehindKey(target("/w/a")));
  // The memo is keyed by path already, so two worktrees at the same refs share a key.
  assert.equal(aheadBehindKey(target("/w/a")), aheadBehindKey(target("/w/elsewhere")));
});

test("aheadBehindKey moves with each input the answer is computed from", () => {
  const base = aheadBehindKey(target("/w/a"));
  // A commit, a push or fetch of the upstream, a branch switch, and a fetch of the
  // default branch — one input each.
  assert.notEqual(aheadBehindKey(target("/w/a", { head_sha: "h2" })), base);
  assert.notEqual(aheadBehindKey(target("/w/a", { upstream_sha: "u2" })), base);
  assert.notEqual(aheadBehindKey(target("/w/a", { branch: "other" })), base);
  assert.notEqual(aheadBehindKey(target("/w/a", {}, "m2")), base);
});

test("aheadBehindKey cannot confuse an absent input with a present one", () => {
  // The same value in a different slot, and an absent slot, are all distinct keys.
  const keys = [
    target("/w/a", { head_sha: "x", upstream_sha: undefined }, null),
    target("/w/a", { head_sha: undefined, upstream_sha: "x" }, null),
    target("/w/a", { head_sha: undefined, upstream_sha: undefined }, "x"),
    target("/w/a", { head_sha: undefined, upstream_sha: undefined }, null),
    target("/w/a", { branch: "x", head_sha: undefined, upstream_sha: undefined }, null),
  ].map(aheadBehindKey);
  assert.equal(new Set(keys).size, keys.length);
});

// --- isMemoizable -----------------------------------------------------------

test("isMemoizable accepts a row whose key carries every input it evidences", () => {
  assert.equal(isMemoizable(target("/w/a"), { ahead: 1, behind: 2, main_behind: 3 }), true);
  assert.equal(isMemoizable(target("/w/a"), { ahead: 0, behind: 0 }), true);
});

test("isMemoizable keeps an empty row only when the key shows nothing to compute", () => {
  // "Nothing to show" is a real answer, and the daemon omits a row that resolves to
  // nothing: a detached HEAD, or a branch with no upstream and no default branch.
  assert.equal(isMemoizable(target("/w/a", { branch: undefined }), {}), true);
  assert.equal(isMemoizable(target("/w/a", { upstream_sha: undefined }, null), {}), true);
});

test("isMemoizable re-asks an empty row the key says should have had counts", () => {
  // The daemon sends the very same empty row when a computation fails (a repository
  // it could not open, a walk that errored). A branch with an upstream — or a repo
  // with a default branch — must have yielded counts, so this is a failure, and
  // keeping it would blank the indicator until a ref next moved.
  assert.equal(isMemoizable(target("/w/a"), {}), false);
  assert.equal(isMemoizable(target("/w/a", { upstream_sha: undefined }), {}), false);
  assert.equal(isMemoizable(target("/w/a", {}, null), {}), false);
  // A detached HEAD has no branch, so there is nothing it could have failed to compute
  // even when the repo does have an upstream elsewhere.
  assert.equal(isMemoizable(target("/w/a", { branch: undefined }), {}), true);
});

test("isMemoizable refuses what an older daemon's key could not have invalidated", () => {
  // A pre-#1337 daemon sends no head_sha: nothing to key on at all.
  assert.equal(isMemoizable(target("/w/a", { head_sha: undefined }), { ahead: 1, behind: 0 }), false);
  // A pre-#1344 daemon sends no upstream_sha, so a push would be invisible.
  assert.equal(
    isMemoizable(target("/w/a", { upstream_sha: undefined }), { ahead: 1, behind: 0 }),
    false,
  );
  // A pre-#2120 daemon sends no main_sha, so a default-branch fetch would be invisible.
  assert.equal(isMemoizable(target("/w/a", {}, null), { main_behind: 3 }), false);
});

test("isMemoizable still caches the rows an older daemon's key does cover", () => {
  // No upstream and no default branch: nothing in the row depends on the missing parts.
  assert.equal(isMemoizable(target("/w/a", { upstream_sha: undefined }, null), {}), true);
  // main_behind alone needs main_sha, not upstream_sha (it can exist without an upstream).
  assert.equal(isMemoizable(target("/w/a", { upstream_sha: undefined }), { main_behind: 3 }), true);
  // ahead/behind alone needs upstream_sha, not main_sha.
  assert.equal(isMemoizable(target("/w/a", {}, null), { ahead: 1, behind: 0 }), true);
});

test("isMemoizable never caches a shallow clone's row", () => {
  // Deepening changes the counts without moving any id the key can see.
  assert.equal(isMemoizable(target("/w/a"), { ahead: 1, behind: 0, shallow: true }), false);
});

// --- stalePaths -------------------------------------------------------------

test("stalePaths selects only the worktrees whose key moved", () => {
  const a = target("/w/a");
  const b = target("/w/b");
  const cached = new Map([
    ["/w/a", { key: aheadBehindKey(a) }],
    ["/w/b", { key: aheadBehindKey(b) }],
  ]);
  assert.deepEqual(stalePaths(cached, [a, b]), []);
  // A commit in /w/a only.
  assert.deepEqual(stalePaths(cached, [target("/w/a", { head_sha: "h2" }), b]), ["/w/a"]);
  // A worktree the memo has never seen fetches itself and nothing else.
  assert.deepEqual(stalePaths(cached, [a, b, target("/w/new")]), ["/w/new"]);
  // The default branch moved: every worktree of that repo, since they all share it.
  assert.deepEqual(stalePaths(cached, [target("/w/a", {}, "m2"), target("/w/b", {}, "m2")]), [
    "/w/a",
    "/w/b",
  ]);
});

// --- AheadBehindMemo.resolve ------------------------------------------------

test("a refresh that changes nothing issues no request", async () => {
  // A colour edit, a session poll, a CI verdict: `getChildren` re-runs with the very
  // same keys, and the daemon is not asked again.
  const { fetch, calls } = fakeFetcher(ROWS);
  const memo = new AheadBehindMemo(fetch);
  const targets = [target("/w/a"), target("/w/b")];

  const first = await memo.resolve(targets);
  assert.deepEqual(calls, [["/w/a", "/w/b"]]);
  assert.deepEqual(first, { "/w/a": ROWS["/w/a"], "/w/b": ROWS["/w/b"] });

  const second = await memo.resolve(targets);
  assert.equal(calls.length, 1, "an unchanged refresh re-asked the daemon");
  assert.deepEqual(second, first);
});

test("a commit re-asks for that worktree alone and keeps serving the rest", async () => {
  const { fetch, calls } = fakeFetcher(ROWS);
  const memo = new AheadBehindMemo(fetch);
  await memo.resolve([target("/w/a"), target("/w/b"), target("/w/c")]);

  const after = await memo.resolve([
    target("/w/a"),
    target("/w/b", { head_sha: "h2" }),
    target("/w/c"),
  ]);
  assert.deepEqual(calls[1], ["/w/b"]);
  assert.equal(calls.length, 2);
  assert.deepEqual(after, ROWS);
});

test("a push, a branch switch and a default-branch fetch each re-ask", async () => {
  for (const [name, moved] of [
    ["push", target("/w/a", { upstream_sha: "u2" })],
    ["branch switch", target("/w/a", { branch: "other" })],
    ["default-branch fetch", target("/w/a", {}, "m2")],
  ] as const) {
    const { fetch, calls } = fakeFetcher(ROWS);
    const memo = new AheadBehindMemo(fetch);
    await memo.resolve([target("/w/a")]);
    await memo.resolve([moved]);
    assert.deepEqual(calls, [["/w/a"], ["/w/a"]], name);
  }
});

test("a default-branch fetch re-asks every worktree of the repo in one batch", async () => {
  const { fetch, calls } = fakeFetcher(ROWS);
  const memo = new AheadBehindMemo(fetch);
  await memo.resolve([target("/w/a"), target("/w/b"), target("/w/c")]);
  await memo.resolve([target("/w/a", {}, "m2"), target("/w/b", {}, "m2"), target("/w/c", {}, "m2")]);
  assert.deepEqual(calls[1], ["/w/a", "/w/b", "/w/c"]);
  assert.equal(calls.length, 2);
});

test("a failed fetch is not remembered, so the next refresh retries", async () => {
  // One dropped connection must not blank a row until its refs next move.
  let fail = true;
  const calls: string[][] = [];
  const memo = new AheadBehindMemo(async (paths) => {
    calls.push(paths);
    return fail ? undefined : ROWS;
  });
  const targets = [target("/w/a")];

  assert.deepEqual(await memo.resolve(targets), {}, "a failure renders no sync indicator");
  assert.equal(memo.size, 0);

  fail = false;
  assert.deepEqual(await memo.resolve(targets), { "/w/a": ROWS["/w/a"] });
  assert.equal(calls.length, 2);
});

test("a fetcher that throws is a failure, not an unhandled rejection", async () => {
  let calls = 0;
  const memo = new AheadBehindMemo(async () => {
    calls += 1;
    throw new Error("socket closed");
  });
  assert.deepEqual(await memo.resolve([target("/w/a")]), {});
  assert.deepEqual(await memo.resolve([target("/w/a")]), {});
  assert.equal(calls, 2);
});

test("a successful reply with no row is remembered as nothing to show", async () => {
  // The daemon omits a row that resolves to nothing (no upstream, no default branch;
  // a detached HEAD); that is an answer, unlike the failure above, and re-asking for
  // it every refresh would be exactly the waste this removes.
  const { fetch, calls } = fakeFetcher({});
  const memo = new AheadBehindMemo(fetch);
  const targets = [
    target("/w/a", { upstream_sha: undefined }, null),
    target("/w/b", { branch: undefined }),
  ];

  assert.deepEqual(await memo.resolve(targets), { "/w/a": {}, "/w/b": {} });
  assert.deepEqual(await memo.resolve(targets), { "/w/a": {}, "/w/b": {} });
  assert.equal(calls.length, 1);
});

test("an empty row for a worktree that has an upstream is a failed computation, re-asked", async () => {
  // The daemon degrades a computation it could not finish to "no divergence", which
  // omits the row — the same shape as a genuinely empty answer. The snapshot says this
  // one had an upstream, so it was a failure and must not be kept.
  const rows: AheadBehindMap = {};
  const calls: string[][] = [];
  const memo = new AheadBehindMemo(async (paths) => {
    calls.push(paths);
    return rows;
  });
  const targets = [target("/w/a")];

  assert.deepEqual(await memo.resolve(targets), { "/w/a": {} });
  assert.equal(memo.size, 0, "a failed computation was kept");

  rows["/w/a"] = { ahead: 1, behind: 0 };
  assert.deepEqual(await memo.resolve(targets), { "/w/a": { ahead: 1, behind: 0 } });
  assert.equal(calls.length, 2);
  assert.equal(memo.size, 1);
});

test("concurrent refreshes share one in-flight batch", async () => {
  const gate = deferred<AheadBehindMap | undefined>();
  const calls: string[][] = [];
  const memo = new AheadBehindMemo((paths) => {
    calls.push(paths);
    return gate.promise;
  });
  const targets = [target("/w/a"), target("/w/b")];

  const first = memo.resolve(targets);
  const second = memo.resolve(targets);
  gate.resolve(ROWS);

  assert.deepEqual(await first, await second);
  assert.equal(calls.length, 1, "the second refresh did not join the first");
});

test("a key that moves while a fetch is in flight is refetched and not clobbered", async () => {
  const gates = [deferred<AheadBehindMap | undefined>(), deferred<AheadBehindMap | undefined>()];
  const calls: string[][] = [];
  const memo = new AheadBehindMemo((paths) => {
    calls.push(paths);
    return gates[calls.length - 1].promise;
  });

  // A commit lands while the first answer is still on its way.
  const stale = memo.resolve([target("/w/a")]);
  const fresh = memo.resolve([target("/w/a", { head_sha: "h2" })]);
  assert.equal(calls.length, 2);

  // The older answer arriving last must not replace the newer entry.
  gates[1].resolve({ "/w/a": { ahead: 9, behind: 0 } });
  assert.deepEqual(await fresh, { "/w/a": { ahead: 9, behind: 0 } });
  gates[0].resolve({ "/w/a": { ahead: 1, behind: 0 } });
  await stale;

  assert.deepEqual(await memo.resolve([target("/w/a", { head_sha: "h2" })]), {
    "/w/a": { ahead: 9, behind: 0 },
  });
  assert.equal(calls.length, 2, "the newer answer was evicted by the older one");
});

test("prune forgets worktrees that left the snapshot", async () => {
  const { fetch, calls } = fakeFetcher(ROWS);
  const memo = new AheadBehindMemo(fetch);
  await memo.resolve([target("/w/a"), target("/w/b")]);
  assert.equal(memo.size, 2);

  memo.prune(["/w/a"]);
  assert.equal(memo.size, 1);

  // /w/a is still served; /w/b, were it to come back, is asked for afresh.
  await memo.resolve([target("/w/a"), target("/w/b")]);
  assert.deepEqual(calls[1], ["/w/b"]);
});

test("a row an older daemon's key cannot validate is re-asked every refresh", async () => {
  // No main_sha on the snapshot (a pre-#2120 daemon) but a main_behind in the reply:
  // a default-branch fetch would be invisible, so this must not be served from cache.
  const { fetch, calls } = fakeFetcher({ "/w/a": { main_behind: 3 }, "/w/b": { ahead: 1, behind: 0 } });
  const memo = new AheadBehindMemo(fetch);
  const targets = [target("/w/a", {}, null), target("/w/b", {}, null)];

  await memo.resolve(targets);
  await memo.resolve(targets);
  // /w/a (main_behind, no main_sha) is re-asked; /w/b (own upstream only) is not.
  assert.deepEqual(calls, [["/w/a", "/w/b"], ["/w/a"]]);
});

test("a shallow clone's row is re-asked every refresh", async () => {
  const { fetch, calls } = fakeFetcher({ "/w/a": { ahead: 1, behind: 0, shallow: true } });
  const memo = new AheadBehindMemo(fetch);
  const targets = [target("/w/a")];

  const first = await memo.resolve(targets);
  await memo.resolve(targets);
  assert.equal(calls.length, 2);
  // The row itself is still returned — the flag is a caveat, not a refusal.
  assert.equal(first["/w/a"].ahead, 1);
});
