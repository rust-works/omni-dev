// The real `gh` subprocess runner behind `github.ts`'s injectable `GhRunner`.
//
// Kept separate (and `vscode`-free) so `github.ts` stays a pure, unit-tested
// module: everything with logic worth asserting lives there behind an injected
// runner, and this file only does the one thing that must touch the OS — spawn
// `gh` — and turn its two failure modes into actionable errors.

import { execFile } from "child_process";
import { promisify } from "util";

const execFileAsync = promisify(execFile);

/**
 * The stdout buffer cap for a `gh pr list` call. A repo's open-PR list (capped
 * at `--limit 100`, a handful of JSON fields each) is far under this; the bound
 * just guards against a pathological response rather than growing unboundedly.
 */
const MAX_BUFFER = 16 * 1024 * 1024;

/**
 * Runs `gh <args>` and resolves with its stdout. Rejects with an actionable
 * error for the two things that go wrong in practice:
 *  - `gh` not on `PATH` (`ENOENT`) → tell the user to install it;
 *  - a non-zero exit → surface `gh`'s own stderr (auth prompts, unknown repo, …).
 */
export async function runGh(args: string[]): Promise<string> {
  try {
    const { stdout } = await execFileAsync("gh", args, {
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
      "the GitHub CLI (`gh`) was not found on your PATH. Install it from " +
        "https://cli.github.com/ and run `gh auth login`.",
    );
  }
  const stderr = e?.stderr ? String(e.stderr).trim() : "";
  if (stderr) {
    return new Error(`\`gh\` failed: ${stderr}`);
  }
  return e instanceof Error ? e : new Error(String(err));
}
