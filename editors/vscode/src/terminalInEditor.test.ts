import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { test } from "node:test";
import type * as vscode from "vscode";
import { registerTerminalInEditor, TERMINAL_IN_EDITOR_PROFILE_ID } from "./terminalInEditor";

function harness() {
  let provider: vscode.TerminalProfileProvider | undefined;
  let registeredId: string | undefined;
  let disposed = false;
  const api = {
    window: {
      registerTerminalProfileProvider(id: string, value: vscode.TerminalProfileProvider) {
        registeredId = id;
        provider = value;
        return { dispose() { disposed = true; } };
      },
    },
    TerminalProfile: class {
      constructor(readonly options: vscode.TerminalOptions | vscode.ExtensionTerminalOptions) {}
    },
    TerminalLocation: { Panel: 1, Editor: 2 },
  };
  const disposable = registerTerminalInEditor(api);
  assert.ok(provider);
  return { provider, registeredId, disposable, isDisposed: () => disposed };
}

function token(cancelled = false): vscode.CancellationToken {
  return {
    isCancellationRequested: cancelled,
    onCancellationRequested: () => ({ dispose() {} }),
  };
}

test("the contributed menu entry activates the matching provider", () => {
  const manifest = JSON.parse(readFileSync(join(__dirname, "../package.json"), "utf8"));
  const { registeredId, disposable, isDisposed } = harness();
  assert.equal(registeredId, TERMINAL_IN_EDITOR_PROFILE_ID);
  const profiles = manifest.contributes.terminal.profiles.filter(
    (profile: { id: string }) => profile.id === registeredId,
  );
  assert.equal(profiles.length, 1);
  assert.equal(profiles[0].title, "Terminal in Editor");
  assert.ok(manifest.activationEvents.includes(`onTerminalProfile:${registeredId}`));
  assert.ok(manifest.activationEvents.includes("onStartupFinished"));
  disposable.dispose();
  assert.equal(isDisposed(), true);
});

test("each request supplies a fresh editor profile with native shell and cwd defaults", async () => {
  const { provider } = harness();
  const first = await provider.provideTerminalProfile(token());
  const second = await provider.provideTerminalProfile(token());
  assert.ok(first);
  assert.ok(second);
  assert.notEqual(first, second);
  assert.notEqual(first.options, second.options);
  assert.deepEqual(first.options, { location: 2 });
  assert.deepEqual(second.options, { location: 2 });
});

test("a cancelled request does not supply a terminal", async () => {
  const { provider } = harness();
  assert.equal(await provider.provideTerminalProfile(token(true)), undefined);
});
