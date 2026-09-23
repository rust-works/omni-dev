import assert from "node:assert/strict";
import { test } from "node:test";
import { agentTerminalIdentity, agentTerminalOptions } from "./agentTerminal";
import { countClaudeTerminals } from "./claudeEmbeddings";

test("agent launch titles use OSC without pinning the VS Code name", () => {
  for (const agent of ["claude", "pi"] as const) {
    const options = agentTerminalOptions(agent, "Session 2");
    assert.equal("name" in options, false);
    assert.equal(options.message, "\x1b]0;Session 2\x07");
  }
});

test("initial titles cannot inject extra terminal control sequences", () => {
  assert.equal(
    agentTerminalOptions("pi", "A\x07\x1b\n\r\x9cB").message,
    "\x1b]0;AB\x07",
  );
});

test("session counts survive renames without counting pi as Claude", () => {
  const names = [
    agentTerminalIdentity("Blah", agentTerminalOptions("claude", "Claude Code")),
    agentTerminalIdentity("Other session", agentTerminalOptions("claude", "Claude Code 2")),
    agentTerminalIdentity("pi - Claude migration - repo", agentTerminalOptions("pi", "pi.dev")),
    agentTerminalIdentity("Claude Code", {}),
    agentTerminalIdentity("zsh", {}),
  ];
  assert.equal(countClaudeTerminals(names), 3);
});

test("unmanaged terminals retain legacy custom-title detection", () => {
  const identity = agentTerminalIdentity("My agent", { env: { OTHER: "value" } });
  assert.equal(countClaudeTerminals([identity], "My agent"), 1);
});
