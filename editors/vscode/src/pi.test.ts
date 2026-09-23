import assert from "node:assert/strict";
import { test } from "node:test";
import { PI_TERMINAL_NAME, checkPiLaunch, nextPiTerminalName } from "./pi";

test("nextPiTerminalName uses and reuses the lowest available name", () => {
  assert.equal(nextPiTerminalName([]), PI_TERMINAL_NAME);
  assert.equal(nextPiTerminalName([PI_TERMINAL_NAME]), "pi.dev 2");
  assert.equal(nextPiTerminalName([PI_TERMINAL_NAME, "pi.dev 3"]), "pi.dev 2");
  assert.equal(nextPiTerminalName(["zsh", "pi.dev 2"]), PI_TERMINAL_NAME);
});

test("checkPiLaunch rejects a missing zsh without checking pi", async () => {
  let checkedPi = false;
  const result = await checkPiLaunch(
    async () => undefined,
    async () => {
      checkedPi = true;
      return true;
    },
  );

  assert.deepEqual(result, { kind: "missing-zsh" });
  assert.equal(checkedPi, false);
});

test("checkPiLaunch verifies pi in the resolved zsh environment", async () => {
  let receivedZsh: string | undefined;
  const result = await checkPiLaunch(
    async () => "/bin/zsh",
    async (zshPath) => {
      receivedZsh = zshPath;
      return false;
    },
  );

  assert.deepEqual(result, { kind: "missing-pi" });
  assert.equal(receivedZsh, "/bin/zsh");
});

test("checkPiLaunch returns the zsh path when pi is available", async () => {
  const result = await checkPiLaunch(async () => "/usr/local/bin/zsh", async () => true);
  assert.deepEqual(result, { kind: "ready", zshPath: "/usr/local/bin/zsh" });
});
