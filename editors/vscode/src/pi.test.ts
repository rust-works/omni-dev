import assert from "node:assert/strict";
import { test } from "node:test";
import {
  PI_TERMINAL_NAME,
  checkPiLaunch,
  nextPiTerminalName,
  piLaunchCommand,
  quoteForZsh,
  resolvePiTitleMode,
  zshSearchPaths,
} from "./pi";

test("zshSearchPaths finds Windows zsh.exe on PATH", () => {
  assert.deepEqual(zshSearchPaths("win32", "C:\\msys64\\usr\\bin;D:\\tools"), [
    "C:\\msys64\\usr\\bin\\zsh.exe",
    "D:\\tools\\zsh.exe",
  ]);
});

test("zshSearchPaths uses absolute Unix paths and standard fallbacks", () => {
  const candidates = zshSearchPaths("darwin", "/custom/bin:/bin");
  assert.equal(candidates[0], "/custom/bin/zsh");
  assert.equal(candidates.filter((candidate) => candidate === "/bin/zsh").length, 1);
  assert.ok(candidates.includes("/opt/homebrew/bin/zsh"));
});

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

test("resolvePiTitleMode defaults anything but native to name", () => {
  assert.equal(resolvePiTitleMode("native"), "native");
  assert.equal(resolvePiTitleMode("name"), "name");
  assert.equal(resolvePiTitleMode(undefined), "name");
  assert.equal(resolvePiTitleMode("bogus"), "name");
});

test("piLaunchCommand runs plain pi in native mode", () => {
  assert.equal(piLaunchCommand("native", "/ext/dist/pi-title.mjs"), "pi");
});

test("piLaunchCommand loads the quoted title extension in name mode", () => {
  assert.equal(
    piLaunchCommand("name", "/Users/me/.vscode/extensions/rust-works.omni-dev-0.9.0/dist/pi-title.mjs"),
    "pi -e '/Users/me/.vscode/extensions/rust-works.omni-dev-0.9.0/dist/pi-title.mjs'",
  );
  assert.equal(
    piLaunchCommand("name", "C:\\Users\\Jo Bloggs\\ext\\dist\\pi-title.mjs"),
    "pi -e 'C:\\Users\\Jo Bloggs\\ext\\dist\\pi-title.mjs'",
  );
});

test("quoteForZsh keeps metacharacters literal and escapes single quotes", () => {
  assert.equal(quoteForZsh("a b$c`d"), "'a b$c`d'");
  assert.equal(quoteForZsh("it's"), "'it'\\''s'");
});
