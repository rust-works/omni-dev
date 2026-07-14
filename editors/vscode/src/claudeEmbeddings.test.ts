// Unit tests for the pure Claude-embedding detection. Run with `node --test
// out/` after `tsc` emits this to `out/claude.test.js`. Nothing here imports
// `vscode`, so it runs under a plain Node process.

import assert from "node:assert/strict";
import { test } from "node:test";
import {
  CLAUDE_WEBVIEW_MARKER,
  countClaudeTabs,
  countClaudeTerminals,
  isClaudeTerminalName,
} from "./claudeEmbeddings";

test("countClaudeTabs matches the mangled webview viewType by substring", () => {
  const viewTypes = [
    `mainThreadWebview-${CLAUDE_WEBVIEW_MARKER}`, // a real, prefixed Claude tab
    "claudeVSCodePanel", // a bare marker
    "some.other.extension.panel", // unrelated webview
    "markdown.preview",
  ];
  assert.equal(countClaudeTabs(viewTypes), 2);
  assert.equal(countClaudeTabs([]), 0);
});

test("isClaudeTerminalName matches default names case-insensitively", () => {
  assert.ok(isClaudeTerminalName("Claude Code"));
  assert.ok(isClaudeTerminalName("claude code"));
  assert.ok(isClaudeTerminalName("my CLAUDE session"));
  assert.ok(!isClaudeTerminalName("bash"));
  assert.ok(!isClaudeTerminalName("zsh"));
});

test("isClaudeTerminalName honours a custom title override", () => {
  // A renamed terminal that no longer contains "claude" still matches its
  // configured $CLAUDE_CODE_TERMINAL_TITLE.
  assert.ok(isClaudeTerminalName("Agent-1", "Agent-1"));
  assert.ok(!isClaudeTerminalName("Agent-2", "Agent-1"));
  // An empty/whitespace custom title never spuriously matches.
  assert.ok(!isClaudeTerminalName("bash", "  "));
});

test("countClaudeTerminals counts across names with an override", () => {
  const names = ["Claude Code", "bash", "Agent-1", "zsh"];
  assert.equal(countClaudeTerminals(names), 1);
  assert.equal(countClaudeTerminals(names, "Agent-1"), 2);
  assert.equal(countClaudeTerminals([]), 0);
});
