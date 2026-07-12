// Unit tests for the `gh` binary resolver. The runner itself (`runGh`) spawns a
// real subprocess and is not unit-tested; the resolution logic is pure (env +
// an injected existence predicate) and is what actually varies by machine.

import assert from "node:assert/strict";
import { test } from "node:test";
import { resolveGhBin, wellKnownGhPaths } from "./gh";

test("resolveGhBin: an OMNI_DEV_GH_BIN override wins over everything", () => {
  // The override is used even when a well-known path also exists.
  assert.equal(resolveGhBin({ OMNI_DEV_GH_BIN: "/custom/gh" }, () => true), "/custom/gh");
});

test("resolveGhBin: a blank override is ignored", () => {
  assert.equal(resolveGhBin({ OMNI_DEV_GH_BIN: "   " }, () => false), "gh");
});

test("resolveGhBin: returns the first well-known path that exists", () => {
  const second = wellKnownGhPaths()[1];
  assert.equal(
    resolveGhBin({}, (p) => p === second),
    second,
  );
});

test("resolveGhBin: falls back to bare `gh` (a PATH lookup) when none exist", () => {
  assert.equal(resolveGhBin({}, () => false), "gh");
});

test("wellKnownGhPaths covers Homebrew (both arches) and a user-local install", () => {
  const paths = wellKnownGhPaths("/home/tester");
  assert.ok(paths.includes("/opt/homebrew/bin/gh"));
  assert.ok(paths.includes("/usr/local/bin/gh"));
  assert.ok(paths.includes("/home/tester/.local/bin/gh"));
});
