// The pi-side half of the name-only tab title (#1899). This module runs inside
// pi, not VS Code: esbuild bundles it (via piTitleExtension.ts) into
// dist/pi-title.mjs, which the launcher loads with `pi -e`. It must not import
// `vscode`, and it types pi's extension API structurally so the VS Code
// extension does not depend on pi's packages.

/** Carries the launcher's seeded title (e.g. `pi.dev 2`) for unnamed sessions. */
export const PI_TITLE_FALLBACK_ENV = "OMNI_DEV_PI_TITLE_FALLBACK";

const DEFAULT_FALLBACK = "pi.dev";

/** The slice of pi's `ExtensionContext` this extension uses. */
export interface PiTitleContext {
  readonly hasUI: boolean;
  readonly ui: { setTitle(title: string): void };
}

/** The slice of pi's `ExtensionAPI` this extension uses. */
export interface PiTitleApi {
  on(event: string, handler: (event: unknown, ctx: PiTitleContext) => void): void;
  getSessionName(): string | undefined;
}

/** The tab title: exactly the `/name` value, else the launcher's fallback. */
export function piTabTitle(sessionName: string | undefined, fallback: string | undefined): string {
  const clean = (value: string | undefined) =>
    (value ?? "").replace(/[\x00-\x1f\x7f-\x9f]/g, "").trim();
  return clean(sessionName) || clean(fallback) || DEFAULT_FALLBACK;
}

/**
 * pi titles the terminal `pi - <name> - <cwd basename>` from its own
 * `updateTerminalTitle()`, with no option to drop the basename, so this
 * overwrites it after each call site pi has:
 *
 * - `session_start` (startup, `--name`, `/new`, `/resume`, fork, `/reload`):
 *   pi retitles *after* binding extensions, so the write is deferred a tick.
 * - `session_info_changed` (`/name` at runtime): extensions already run after
 *   pi retitles; deferring anyway keeps one code path.
 * - `agent_start`: a self-heal for the retitle pi does on Windows once its
 *   startup package-update check settles, which no event announces.
 */
export function registerPiTitle(
  pi: PiTitleApi,
  env: Readonly<Record<string, string | undefined>>,
  defer: (fn: () => void) => void = (fn) => void setTimeout(fn, 0),
): void {
  const apply = (_event: unknown, ctx: PiTitleContext) => {
    if (!ctx.hasUI) {
      return;
    }
    defer(() => ctx.ui.setTitle(piTabTitle(pi.getSessionName(), env[PI_TITLE_FALLBACK_ENV])));
  };
  pi.on("session_start", apply);
  pi.on("session_info_changed", apply);
  pi.on("agent_start", apply);
}
