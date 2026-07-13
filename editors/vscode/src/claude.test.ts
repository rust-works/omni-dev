// Unit tests for the pure "Open Claude Code" button helpers. Nothing here imports
// `vscode`, so it runs under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  DEFAULT_CLAUDE_COMMAND,
  resolveClaudeCommand,
  resolveClaudeCwd,
} from "./claude";

test("resolveClaudeCommand defaults a blank/unset value to `claude`", () => {
  assert.equal(resolveClaudeCommand(undefined), DEFAULT_CLAUDE_COMMAND);
  assert.equal(resolveClaudeCommand(""), DEFAULT_CLAUDE_COMMAND);
  assert.equal(resolveClaudeCommand("   "), DEFAULT_CLAUDE_COMMAND);
});

test("resolveClaudeCommand trims but otherwise passes the command through", () => {
  assert.equal(resolveClaudeCommand("claude"), "claude");
  assert.equal(resolveClaudeCommand("  claude  "), "claude");
  // A shell prefix is a supported, verbatim value.
  assert.equal(resolveClaudeCommand("proxy && claude"), "proxy && claude");
});

test("resolveClaudeCwd prefers the active folder when it is an open folder", () => {
  const folders = ["/home/me/a", "/home/me/b"];
  assert.equal(resolveClaudeCwd(folders, "/home/me/b"), "/home/me/b");
});

test("resolveClaudeCwd falls back to the first folder when the active folder is not open", () => {
  const folders = ["/home/me/a", "/home/me/b"];
  // Active editor sits outside any workspace folder (e.g. an untitled/settings tab).
  assert.equal(resolveClaudeCwd(folders, "/somewhere/else"), "/home/me/a");
  // No active folder at all.
  assert.equal(resolveClaudeCwd(folders, undefined), "/home/me/a");
});

test("resolveClaudeCwd returns undefined when no folders are open", () => {
  assert.equal(resolveClaudeCwd([], undefined), undefined);
  assert.equal(resolveClaudeCwd([], "/home/me/a"), undefined);
});
