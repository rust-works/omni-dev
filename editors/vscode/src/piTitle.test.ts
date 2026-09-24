import assert from "node:assert/strict";
import { test } from "node:test";
import { PI_TITLE_FALLBACK_ENV, type PiTitleApi, type PiTitleContext, piTabTitle, registerPiTitle } from "./piTitle";

test("piTabTitle is exactly the session name when set", () => {
  assert.equal(piTabTitle("Blah", "pi.dev 2"), "Blah");
  assert.equal(piTabTitle("  Refactor auth  ", undefined), "Refactor auth");
});

test("piTabTitle falls back to the launcher title, then pi.dev", () => {
  assert.equal(piTabTitle(undefined, "pi.dev 2"), "pi.dev 2");
  assert.equal(piTabTitle("   ", "pi.dev 3"), "pi.dev 3");
  assert.equal(piTabTitle(undefined, undefined), "pi.dev");
  assert.equal(piTabTitle(undefined, ""), "pi.dev");
});

test("piTabTitle strips control characters that would end the OSC sequence", () => {
  assert.equal(piTabTitle("a\x07b\x1b]0;c", undefined), "ab]0;c");
});

/** A fake pi that records handlers, with a controllable session name. */
function fakePi(): { pi: PiTitleApi; fire(event: string, ctx: PiTitleContext): void; name?: string } {
  const handlers = new Map<string, Array<(event: unknown, ctx: PiTitleContext) => void>>();
  const fake = {
    name: undefined as string | undefined,
    pi: {
      on: (event: string, handler: (event: unknown, ctx: PiTitleContext) => void) => {
        handlers.set(event, [...(handlers.get(event) ?? []), handler]);
      },
      getSessionName: () => fake.name,
    },
    fire: (event: string, ctx: PiTitleContext) => {
      for (const handler of handlers.get(event) ?? []) handler({}, ctx);
    },
  };
  return fake;
}

function recordingContext(hasUI = true): { ctx: PiTitleContext; titles: string[] } {
  const titles: string[] = [];
  return { ctx: { hasUI, ui: { setTitle: (title) => titles.push(title) } }, titles };
}

test("registerPiTitle retitles on startup, /name, and agent start", () => {
  const fake = fakePi();
  const deferred: Array<() => void> = [];
  registerPiTitle(fake.pi, { [PI_TITLE_FALLBACK_ENV]: "pi.dev 2" }, (fn) => deferred.push(fn));
  const { ctx, titles } = recordingContext();

  fake.fire("session_start", ctx);
  fake.name = "Blah";
  fake.fire("session_info_changed", ctx);
  fake.fire("agent_start", ctx);
  fake.fire("turn_end", ctx);

  assert.deepEqual(titles, [], "writes are deferred past pi's own retitle");
  deferred.forEach((fn) => fn());
  assert.deepEqual(titles, ["Blah", "Blah", "Blah"]);
});

test("registerPiTitle reads the name when the deferred write runs", () => {
  const fake = fakePi();
  const deferred: Array<() => void> = [];
  registerPiTitle(fake.pi, {}, (fn) => deferred.push(fn));
  const { ctx, titles } = recordingContext();

  fake.fire("session_start", ctx);
  deferred.shift()?.();
  fake.name = "Later";
  fake.fire("session_info_changed", ctx);
  deferred.shift()?.();

  assert.deepEqual(titles, ["pi.dev", "Later"]);
});

test("registerPiTitle does nothing without a UI", () => {
  const fake = fakePi();
  const deferred: Array<() => void> = [];
  registerPiTitle(fake.pi, {}, (fn) => deferred.push(fn));
  const { ctx, titles } = recordingContext(false);

  fake.fire("session_start", ctx);
  assert.equal(deferred.length, 0);
  assert.deepEqual(titles, []);
});
