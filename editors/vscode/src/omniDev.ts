// How to invoke the `omni-dev` CLI itself — the binary resolver plus the argv and
// shell-command-line builders for `worktrees rebase` (#1409).
//
// This is the `gh.ts` analogue for our own binary, minus the subprocess runner:
// "Rebase on main" runs the rebase in an **integrated terminal**, so the user's
// shell is the runner and this module only has to decide *what* to send it. That
// leaves the whole file pure and `vscode`-free, so every part of it is unit-tested
// in `omniDev.test.ts`.
//
// Why a terminal rather than `execFile`: the rebase is the one worktrees action
// that must run in the **user's own environment**. Its fetch needs their
// `ssh-agent` / `~/.ssh/config` / credential helper, which is exactly what the
// daemon (spawned by launchd/systemd with a minimal environment) does not have —
// see ADR-0055 and ADR-0003. A fetch can also prompt, and a rebase can hit
// conflicts, so live and user-drivable output is the feature, not a fallback.

import * as fs from "fs";
import * as os from "os";
import * as path from "path";

/**
 * Well-known absolute locations of the `omni-dev` binary, in probe order. A
 * GUI-launched VS Code (Dock/Finder) inherits a minimal `PATH` that omits Cargo's
 * and Homebrew's `bin` dirs, so a plain `omni-dev` lookup can fail even when it is
 * installed — the same tactic (and the same reason) as {@link wellKnownGhPaths} in
 * `gh.ts`. Cargo comes first: `cargo install omni-dev` is how the crate ships.
 * `home` is injectable for testing.
 */
export function wellKnownOmniDevPaths(home: string = os.homedir()): string[] {
  return [
    path.join(home, ".cargo", "bin", "omni-dev"), // cargo install (the usual case)
    "/opt/homebrew/bin/omni-dev", // macOS, Apple Silicon Homebrew
    "/usr/local/bin/omni-dev", // macOS Intel Homebrew, common manual installs
    "/home/linuxbrew/.linuxbrew/bin/omni-dev", // Linux Homebrew
    "/usr/bin/omni-dev", // Linux distro packages
    path.join(home, ".local", "bin", "omni-dev"), // user-local installs
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
 * Resolves the `omni-dev` executable to run. An explicit `OMNI_DEV_BIN` override
 * wins; otherwise the first existing {@link wellKnownOmniDevPaths} entry; else bare
 * `omni-dev` — a normal `PATH` lookup, which is what usually resolves it, since the
 * integrated terminal's shell loads the user's profile even when the editor's own
 * environment is minimal. `env`/`exists` are injectable for testing.
 */
export function resolveOmniDevBin(
  env: NodeJS.ProcessEnv = process.env,
  exists: (p: string) => boolean = isExecutableFile,
): string {
  const override = env.OMNI_DEV_BIN?.trim();
  if (override) {
    return override;
  }
  for (const candidate of wellKnownOmniDevPaths()) {
    if (exists(candidate)) {
      return candidate;
    }
  }
  return "omni-dev";
}

/**
 * The argv for `omni-dev worktrees rebase` over `paths`.
 *
 * The paths are passed positionally as a **single** invocation, which is what buys
 * the fetch-once-per-repo contract: the engine groups the selection by repository
 * and fetches each one's onto ref exactly once, however many worktrees of it were
 * selected. They are absolute (a tree row's `wt.path`), so the terminal's cwd never
 * affects resolution.
 *
 * `yes` skips the CLI's own `[y/N]` prompt — passed only because the extension has
 * already confirmed with a modal, which is the one place the user sees the whole
 * batch. `--autostash` is deliberately not offered: a worktree with uncommitted
 * changes is reported as `skipped` in the output the user is already reading.
 */
export function rebaseArgs(paths: string[], opts: { yes?: boolean } = {}): string[] {
  return ["worktrees", "rebase", ...paths, ...(opts.yes ? ["-y"] : [])];
}

/** Shell metacharacters absent from a word that can be passed through unquoted. */
const SHELL_SAFE = /^[A-Za-z0-9_@%+=:,./-]+$/;

/**
 * Quotes one word for a shell command line, since the command reaches the user's
 * shell as *text* (`Terminal.sendText`) rather than as an argv.
 *
 * Single-quoting with `'` → `'\''` is the POSIX rule, and PowerShell's own
 * single-quote rule (`''`) parses the same escape harmlessly as an empty string
 * concatenation. Only `cmd.exe` would misparse it — consistent with the rest of the
 * companion's Unix-shaped assumptions (the daemon socket it talks to is a Unix
 * domain socket).
 */
export function shellQuote(word: string): string {
  return SHELL_SAFE.test(word) ? word : `'${word.replaceAll("'", "'\\''")}'`;
}

/** One shell command line: the resolved binary and its args, each quoted as needed. */
export function commandLine(bin: string, args: string[]): string {
  return [bin, ...args].map(shellQuote).join(" ");
}
