// Unit tests for the pure "Open Claude Code" button helpers. Nothing here imports
// `vscode`, so it runs under a plain Node process (`node --test out/`).

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  CLAUDE_TERMINAL_NAME,
  DEFAULT_CLAUDE_COMMAND,
  nextClaudeTerminalName,
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

test("nextClaudeTerminalName uses the base name for the first launch", () => {
  assert.equal(nextClaudeTerminalName([]), CLAUDE_TERMINAL_NAME);
  // Unrelated open terminals never take the base name.
  assert.equal(nextClaudeTerminalName(["zsh", "npm run watch"]), CLAUDE_TERMINAL_NAME);
});

test("nextClaudeTerminalName numbers past an open Claude terminal", () => {
  assert.equal(nextClaudeTerminalName([CLAUDE_TERMINAL_NAME]), "Claude Code 2");
  assert.equal(
    nextClaudeTerminalName([CLAUDE_TERMINAL_NAME, "Claude Code 2"]),
    "Claude Code 3",
  );
});

test("nextClaudeTerminalName reuses a number freed by a closed session", () => {
  // `Claude Code 2` was closed, leaving a gap between the base name and `3`; the
  // next launch fills the gap rather than climbing to `4`.
  assert.equal(
    nextClaudeTerminalName([CLAUDE_TERMINAL_NAME, "Claude Code 3"]),
    "Claude Code 2",
  );
});

test("nextClaudeTerminalName treats a non-Claude terminal named `Claude Code` as taken", () => {
  // The helper is fed *all* terminal names, so a user's own terminal that happens
  // to be named `Claude Code` still pushes ours to the next number.
  assert.equal(
    nextClaudeTerminalName(["Claude Code", "server"]),
    "Claude Code 2",
  );
});
