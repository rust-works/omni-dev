import assert from "node:assert/strict";
import { test } from "node:test";
import { sendWithDiagnostics } from "./replyDiagnostics";

test("request diagnostics preserve rejection and include window context once", async () => {
  const lines: string[] = [];
  const reply = { ok: false, error: "invalid window" };
  const received = await sendWithDiagnostics(
    { service: "sessions", op: "window", payload: {} },
    async () => reply, (line) => lines.push(line), "window=abc",
  );
  assert.equal(received, reply);
  assert.deepEqual(lines, ["sessions/window (window=abc) rejected: invalid window"]);
});

test("request diagnostics swallow transport throws and leave success silent", async () => {
  const lines: string[] = [];
  const env = { service: "sessions", op: "list", payload: {} };
  assert.equal(await sendWithDiagnostics(env, async () => { throw new Error("offline"); },
    (line) => lines.push(line)), undefined);
  assert.equal(lines.length, 1);
  const reply = { ok: true, payload: {} };
  assert.equal(await sendWithDiagnostics(env, async () => reply, (line) => lines.push(line)), reply);
  assert.equal(lines.length, 1);
});
