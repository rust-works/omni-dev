// Unit tests for the pure row-icon layer (#1428). Nothing here imports `vscode`, so it
// runs under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as path from "node:path";
import { test } from "node:test";

import {
  ROW_COLORS,
  ROW_COLOR_IDS,
  repoRowIcon,
  rowColorTag,
  sameRowColors,
  worktreeRowIcon,
} from "./icons";
import { Node, TreeRepoPayload, TreeWorktreePayload } from "./tree";

const GITHUB_REPO: TreeRepoPayload = {
  main_repo: "omni-dev",
  github: { owner: "rust-works", name: "omni-dev" },
  root: "/Users/jky/wrk/rust-works/omni-dev",
  worktrees: [],
};

const PLAIN_REPO: TreeRepoPayload = {
  main_repo: "scratch",
  root: "/Users/jky/wrk/scratch",
  worktrees: [],
};

const CLOSED_WT: TreeWorktreePayload = {
  path: "/Users/jky/wrk/work-trees/omni-dev/issue-1428",
  branch: "issue-1428",
  is_main: false,
  open: false,
};

const OPEN_WT: TreeWorktreePayload = { ...CLOSED_WT, open: true, window_key: "w2" };
const SELF_WT: TreeWorktreePayload = { ...CLOSED_WT, open: true, window_key: "w1" };

const REPO_NODE: Node = { kind: "repo", repo: GITHUB_REPO };
const WT_NODE: Node = { kind: "worktree", repo: GITHUB_REPO, wt: CLOSED_WT };

// --- Untagged rows reproduce today's appearance exactly (#1428 acceptance) ---

test("an untagged repo row renders exactly as it does today", () => {
  // GitHub origin, polling on and the master switch on: green.
  assert.deepEqual(repoRowIcon({ ...GITHUB_REPO, polling_enabled: true }, true), {
    iconId: "github",
    colorId: "charts.green",
  });
  // The same repo with the master switch off greys, as does one not being polled.
  assert.deepEqual(repoRowIcon({ ...GITHUB_REPO, polling_enabled: true }, false), {
    iconId: "github",
    colorId: undefined,
  });
  assert.deepEqual(repoRowIcon(GITHUB_REPO, true), { iconId: "github", colorId: undefined });
  // A non-GitHub repo keeps the plain glyph and no colour.
  assert.deepEqual(repoRowIcon(PLAIN_REPO, true), { iconId: "repo", colorId: undefined });
});

test("an untagged worktree row renders exactly as it does today", () => {
  assert.deepEqual(worktreeRowIcon(SELF_WT, "w1"), { iconId: "check", colorId: "charts.blue" });
  assert.deepEqual(worktreeRowIcon(OPEN_WT, "w1"), {
    iconId: "circle-filled",
    colorId: "charts.green",
  });
  assert.deepEqual(worktreeRowIcon(CLOSED_WT, "w1"), { iconId: "git-branch", colorId: undefined });
  // No window key at all: this window owns nothing, so an open worktree is "elsewhere".
  assert.deepEqual(worktreeRowIcon(OPEN_WT, undefined), {
    iconId: "circle-filled",
    colorId: "charts.green",
  });
});

// --- A tag recolours every glyph, and never changes which glyph is shown ---

test("a tag applies to whichever glyph the worktree row is showing", () => {
  assert.deepEqual(worktreeRowIcon(SELF_WT, "w1", "charts.purple"), {
    iconId: "check",
    colorId: "charts.purple",
  });
  assert.deepEqual(worktreeRowIcon(OPEN_WT, "w1", "charts.purple"), {
    iconId: "circle-filled",
    colorId: "charts.purple",
  });
  // The closed row is one of the two that carry no colour today — it becomes taggable.
  assert.deepEqual(worktreeRowIcon(CLOSED_WT, "w1", "charts.purple"), {
    iconId: "git-branch",
    colorId: "charts.purple",
  });
});

test("a tag applies to both repo glyphs, overriding the PR-poll green", () => {
  assert.deepEqual(repoRowIcon({ ...GITHUB_REPO, polling_enabled: true }, true, "charts.orange"), {
    iconId: "github",
    colorId: "charts.orange",
  });
  assert.deepEqual(repoRowIcon(GITHUB_REPO, true, "charts.orange"), {
    iconId: "github",
    colorId: "charts.orange",
  });
  assert.deepEqual(repoRowIcon(PLAIN_REPO, true, "charts.orange"), {
    iconId: "repo",
    colorId: "charts.orange",
  });
});

// --- The rebase cue outranks a tag (#1415) ---

test("the mid-rebase cue overrides a tag, glyph and colour", () => {
  const rebasing = { ...OPEN_WT, rebasing: true };
  assert.deepEqual(worktreeRowIcon(rebasing, "w1", "charts.purple"), {
    iconId: "sync~spin",
    colorId: "charts.yellow",
  });
  // The durable half — a conflict left in place — outranks a tag just the same, and on
  // the current-window row too.
  const conflicted = { ...SELF_WT, operation: "rebase-merge" };
  assert.deepEqual(worktreeRowIcon(conflicted, "w1", "charts.purple"), {
    iconId: "warning",
    colorId: "charts.yellow",
  });
});

// --- Tag lookup and validation (the single sanitisation point) ---

test("rowColorTag reads the tag for a repo and a worktree row independently", () => {
  // A main worktree's path equals its repo's root, so the two rows must not collide —
  // `nodeId` discriminates them and this is what proves it.
  const colors = {
    "repo:/Users/jky/wrk/rust-works/omni-dev": "charts.purple",
    "wt:/Users/jky/wrk/work-trees/omni-dev/issue-1428": "charts.orange",
  };
  assert.equal(rowColorTag(colors, REPO_NODE), "charts.purple");
  assert.equal(rowColorTag(colors, WT_NODE), "charts.orange");
  assert.equal(rowColorTag({}, REPO_NODE), undefined);
});

test("rowColorTag rejects anything that is not a known colour id", () => {
  const key = "repo:/Users/jky/wrk/rust-works/omni-dev";
  // A typo must fall through to the state colour rather than render uncoloured —
  // `new ThemeColor("nonsense")` does not throw, it silently paints nothing.
  assert.equal(rowColorTag({ [key]: "chart.greeen" }, REPO_NODE), undefined);
  // The empty string is a valid hand-edited spelling of "no tag".
  assert.equal(rowColorTag({ [key]: "" }, REPO_NODE), undefined);
  // Hand-edited settings can hold anything at all; none of it may throw.
  assert.equal(rowColorTag({ [key]: 42 }, REPO_NODE), undefined);
  assert.equal(rowColorTag({ [key]: null }, REPO_NODE), undefined);
  assert.equal(rowColorTag({ [key]: ["charts.red"] }, REPO_NODE), undefined);
  assert.equal(rowColorTag(undefined, REPO_NODE), undefined);
  assert.equal(rowColorTag(null, REPO_NODE), undefined);
  assert.equal(rowColorTag("charts.red", REPO_NODE), undefined);
  assert.equal(rowColorTag([], REPO_NODE), undefined);
});

test("rowColorTag reads own properties only", () => {
  // A key inherited from `Object.prototype` is not a tag anyone set.
  assert.equal(rowColorTag({}, { kind: "repo", repo: { ...PLAIN_REPO, root: "" } }), undefined);
  const proto = { "repo:/x": "charts.red" };
  const inherited = Object.create(proto) as Record<string, string>;
  const node: Node = { kind: "repo", repo: { ...PLAIN_REPO, root: "/x" } };
  assert.equal(rowColorTag(proto, node), "charts.red");
  assert.equal(rowColorTag(inherited, node), undefined);
});

// --- The refresh guard ---

test("sameRowColors compares maps by content", () => {
  assert.equal(sameRowColors({}, {}), true);
  assert.equal(sameRowColors({ a: "charts.red" }, { a: "charts.red" }), true);
  assert.equal(sameRowColors({ a: "charts.red" }, { a: "charts.blue" }), false);
  assert.equal(sameRowColors({ a: "charts.red" }, {}), false);
  assert.equal(sameRowColors({}, { a: "charts.red" }), false);
  // Same size, disjoint keys — the length check alone would pass this.
  assert.equal(sameRowColors({ a: "charts.red" }, { b: "charts.red" }), false);
});

// --- The palette and its `package.json` schema are one fact in two places ---

test("ROW_COLORS matches the enum contributed in package.json", () => {
  // Tests run from `out/`, so the package manifest is one level up.
  const manifest = JSON.parse(
    fs.readFileSync(path.join(__dirname, "..", "package.json"), "utf8"),
  ) as {
    contributes: {
      configuration: {
        properties: Record<
          string,
          { additionalProperties?: { enum?: string[]; enumDescriptions?: string[] } }
        >;
      };
    };
  };
  const schema =
    manifest.contributes.configuration.properties["omniDevWorktrees.rowColors"]
      ?.additionalProperties;
  assert.ok(schema, "omniDevWorktrees.rowColors must declare an additionalProperties schema");
  // The empty string leads: it is the hand-edited spelling of "no tag", which the
  // schema must accept even though it is never offered in the picker.
  assert.deepEqual(schema.enum, ["", ...ROW_COLORS.map((c) => c.id)]);
  assert.equal(schema.enumDescriptions?.length, schema.enum?.length);
  assert.equal(ROW_COLOR_IDS.size, ROW_COLORS.length, "colour ids must be unique");
});
