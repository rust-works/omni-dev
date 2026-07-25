// Unit tests for the `omni-dev` binary resolver and the `worktrees rebase`
// invocation builders. The whole module is pure — the terminal, not this code,
// spawns anything — so all of it is covered here.

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  commandLine,
  rebaseArgs,
  resolveOmniDevBin,
  shellQuote,
  wellKnownOmniDevPaths,
} from "./omniDev";

test("resolveOmniDevBin: an OMNI_DEV_BIN override wins over everything", () => {
  // The override is used even when a well-known path also exists.
  assert.equal(
    resolveOmniDevBin({ OMNI_DEV_BIN: "/custom/omni-dev" }, () => true),
    "/custom/omni-dev",
  );
});

test("resolveOmniDevBin: a blank override is ignored", () => {
  assert.equal(resolveOmniDevBin({ OMNI_DEV_BIN: "   " }, () => false), "omni-dev");
});

test("resolveOmniDevBin: returns the first well-known path that exists", () => {
  const second = wellKnownOmniDevPaths()[1];
  assert.equal(
    resolveOmniDevBin({}, (p) => p === second),
    second,
  );
});

test("resolveOmniDevBin: falls back to bare `omni-dev` (a PATH lookup) when none exist", () => {
  assert.equal(resolveOmniDevBin({}, () => false), "omni-dev");
});

test("wellKnownOmniDevPaths probes cargo first, then Homebrew and a user-local install", () => {
  const paths = wellKnownOmniDevPaths("/home/tester");
  // Cargo is how the crate installs, so it must be probed before anything else.
  assert.equal(paths[0], "/home/tester/.cargo/bin/omni-dev");
  assert.ok(paths.includes("/opt/homebrew/bin/omni-dev"));
  assert.ok(paths.includes("/usr/local/bin/omni-dev"));
  assert.ok(paths.includes("/home/linuxbrew/.linuxbrew/bin/omni-dev"));
  assert.ok(paths.includes("/home/tester/.local/bin/omni-dev"));
});

test("rebaseArgs passes every path positionally in one invocation", () => {
  // One invocation over all paths is what buys the fetch-once-per-repo contract.
  assert.deepEqual(rebaseArgs(["/wt/a", "/wt/b"]), ["worktrees", "rebase", "/wt/a", "/wt/b"]);
});

test("rebaseArgs appends -y only when asked", () => {
  assert.deepEqual(rebaseArgs(["/wt/a"], { yes: true }), ["worktrees", "rebase", "/wt/a", "-y"]);
  assert.deepEqual(rebaseArgs(["/wt/a"], { yes: false }), ["worktrees", "rebase", "/wt/a"]);
});

test("rebaseArgs never passes --autostash or --all", () => {
  const args = rebaseArgs(["/wt/a"], { yes: true });
  assert.ok(!args.includes("--autostash"));
  assert.ok(!args.includes("--all"));
});

test("shellQuote leaves an ordinary path alone", () => {
  const plain = "/Users/tester/wrk/work-trees/omni-dev/issue-1409";
  assert.equal(shellQuote(plain), plain);
});

test("shellQuote single-quotes a path with a space", () => {
  assert.equal(shellQuote("/Users/tester/my worktrees/a"), "'/Users/tester/my worktrees/a'");
});

test("shellQuote escapes an embedded single quote", () => {
  // The POSIX idiom: close the quote, emit an escaped quote, reopen.
  assert.equal(shellQuote("/tmp/it's here"), "'/tmp/it'\\''s here'");
});

test("shellQuote quotes shell metacharacters that would otherwise be interpreted", () => {
  for (const word of ["a;rm -rf /", "$(id)", "a`id`", "a|b", "a&b", "a>b", "*"]) {
    assert.equal(shellQuote(word), `'${word}'`, `expected ${word} to be quoted`);
  }
});

test("commandLine joins the binary and its args, quoting only what needs it", () => {
  assert.equal(
    commandLine("/home/tester/.cargo/bin/omni-dev", rebaseArgs(["/wt/a b"], { yes: true })),
    "/home/tester/.cargo/bin/omni-dev worktrees rebase '/wt/a b' -y",
  );
});
