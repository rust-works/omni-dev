// The real `gh` subprocess runner. Since #1389 (fix 7) the extension prefers the
// daemon's shared, counted `open-prs` op and only reaches for this against a daemon
// too old to serve it — the last-resort per-window `gh pr list`.
//
// Kept separate (and `vscode`-free) so `github.ts` stays a pure, unit-tested
// module: everything with logic worth asserting lives there behind injected
// fetchers, and this file only does the one thing that must touch the OS — spawn
// `gh` — and turn its failure modes into actionable errors.

import { execFile } from "child_process";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { promisify } from "util";

const execFileAsync = promisify(execFile);

/**
 * The stdout buffer cap for a `gh pr list` call. A repo's open-PR list (capped
 * at `--limit 100`, a handful of JSON fields each) is far under this; the bound
 * just guards against a pathological response rather than growing unboundedly.
 */
const MAX_BUFFER = 16 * 1024 * 1024;

/**
 * Well-known absolute locations of the `gh` binary, in probe order. A
 * GUI-launched VS Code (Dock/Finder) — or one spawned by the daemon under a
 * service manager — inherits a minimal `PATH` that omits Homebrew's and
 * user-local `bin` dirs, so a plain `gh` PATH lookup fails even when gh is
 * installed. Probing these first is the same tactic the daemon's VS Code
 * launcher uses to find `code`. `home` is injectable for testing.
 */
export function wellKnownGhPaths(home: string = os.homedir()): string[] {
  return [
    "/opt/homebrew/bin/gh", // macOS, Apple Silicon Homebrew
    "/usr/local/bin/gh", // macOS Intel Homebrew, common manual installs
    "/home/linuxbrew/.linuxbrew/bin/gh", // Linux Homebrew
    "/usr/bin/gh", // Linux distro packages
    path.join(home, ".local", "bin", "gh"), // user-local installs
  ];
}

/** Whether `p` exists and is executable — the well-known-path probe predicate. */
function isExecutableFile(p: string): boolean {
  try {
    fs.accessSync(p, fs.constants.X_OK);
    return true;
  } catch {
    return false;
  }
}

/**
 * Resolves the `gh` executable to run. An explicit `OMNI_DEV_GH_BIN` override
 * wins; otherwise the first existing {@link wellKnownGhPaths} entry; else bare
 * `gh` — a normal `PATH` lookup, which works when VS Code was launched from a
 * shell with a full environment. `env`/`exists` are injectable for testing.
 */
export function resolveGhBin(
  env: NodeJS.ProcessEnv = process.env,
  exists: (p: string) => boolean = isExecutableFile,
): string {
  const override = env.OMNI_DEV_GH_BIN?.trim();
  if (override) {
    return override;
  }
  for (const candidate of wellKnownGhPaths()) {
    if (exists(candidate)) {
      return candidate;
    }
  }
  return "gh";
}

/**
 * Runs `gh <args>` and resolves with its stdout. Rejects with an actionable
 * error for the two things that go wrong in practice:
 *  - `gh` not found (`ENOENT`) → tell the user to install it or point us at it;
 *  - a non-zero exit → surface `gh`'s own stderr (auth prompts, unknown repo, …).
 */
export async function runGh(args: string[]): Promise<string> {
  try {
    const { stdout } = await execFileAsync(resolveGhBin(), args, {
      maxBuffer: MAX_BUFFER,
      encoding: "utf8",
    });
    return stdout;
  } catch (err) {
    throw classifyGhError(err);
  }
}

/** Turns an `execFile` rejection into a user-facing error. */
function classifyGhError(err: unknown): Error {
  const e = err as NodeJS.ErrnoException & { stderr?: string | Buffer };
  if (e?.code === "ENOENT") {
    return new Error(
      "the GitHub CLI (`gh`) was not found on your PATH or in the usual install " +
        "locations. Install it from https://cli.github.com/ and run `gh auth login`, " +
        "or set OMNI_DEV_GH_BIN to its full path (a GUI-launched editor can inherit a " +
        "minimal PATH).",
    );
  }
  const stderr = e?.stderr ? String(e.stderr).trim() : "";
  if (stderr) {
    return new Error(`\`gh\` failed: ${stderr}`);
  }
  return e instanceof Error ? e : new Error(String(err));
}
