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
  let openListener: ((terminal: vscode.Terminal) => unknown) | undefined;
  let listenerDisposed = false;
  const api = {
    window: {
      onDidOpenTerminal(listener: (terminal: vscode.Terminal) => unknown) {
        openListener = listener;
        return { dispose() { listenerDisposed = true; } };
      },
      registerTerminalProfileProvider(id: string, value: vscode.TerminalProfileProvider) {
        registeredId = id;
        provider = value;
        return {
          dispose() { disposed = true; },
        };
      },
    },
    TerminalProfile: class {
      constructor(readonly options: vscode.TerminalOptions | vscode.ExtensionTerminalOptions) {}
    },
    TerminalLocation: { Panel: 1, Editor: 2 },
  };
  const disposable = registerTerminalInEditor(api);
  assert.ok(provider);
  return {
    provider, registeredId, disposable,
    isDisposed: () => disposed && listenerDisposed,
    open(terminal: Pick<vscode.Terminal, "creationOptions" | "processId" | "show">) {
      openListener?.(terminal as vscode.Terminal);
    },
  };
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


test("the menu-created terminal receives focus after its shell starts", async () => {
  const h = harness();
  const profile = await h.provider.provideTerminalProfile(token());
  assert.ok(profile);
  let ready!: (pid: number) => void;
  const processId = new Promise<number>((resolve) => { ready = resolve; });
  const shows: (boolean | undefined)[] = [];
  const terminal = {
    creationOptions: profile.options,
    processId,
    show: (preserveFocus?: boolean) => { shows.push(preserveFocus); },
  };
  h.open(terminal);
  assert.deepEqual(shows, []);
  ready(123);
  await processId;
  assert.deepEqual(shows, [false]);
  h.open(terminal);
  await Promise.resolve();
  assert.deepEqual(shows, [false]);
});

test("other editor profiles are never refocused", async () => {
  const h = harness();
  await h.provider.provideTerminalProfile(token());
  let shown = false;
  h.open({
    creationOptions: { location: 2 },
    processId: Promise.resolve(123),
    show() { shown = true; },
  });
  await Promise.resolve();
  assert.equal(shown, false);
});

test("disposal prevents delayed focus and further profile requests", async () => {
  const h = harness();
  const profile = await h.provider.provideTerminalProfile(token());
  assert.ok(profile);
  let ready!: (pid: number) => void;
  const processId = new Promise<number>((resolve) => { ready = resolve; });
  let shown = false;
  h.open({ creationOptions: profile.options, processId, show() { shown = true; } });
  h.disposable.dispose();
  ready(123);
  await processId;
  assert.equal(shown, false);
  assert.equal(await h.provider.provideTerminalProfile(token()), undefined);
  assert.equal(h.isDisposed(), true);
});


test("closing or failing a terminal during startup does not leave an unhandled rejection", async () => {
  for (const failsToStart of [false, true]) {
    const h = harness();
    const profile = await h.provider.provideTerminalProfile(token());
    assert.ok(profile);
    h.open({
      creationOptions: profile.options,
      processId: failsToStart ? Promise.reject(new Error("startup failed")) : Promise.resolve(123),
      show() { throw new Error("terminal disposed"); },
    });
    await new Promise<void>((resolve) => setImmediate(resolve));
  }
});
