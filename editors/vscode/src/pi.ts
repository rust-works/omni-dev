// Pure helpers for the pi.dev launcher. Keeping the preflight policy out of the
// VS Code layer makes terminal naming and missing-tool handling testable under
// plain Node.

import * as path from "path";

/** The base name used for editor-area pi.dev terminals. */
export const PI_TERMINAL_NAME = "pi.dev";

/** Absolute zsh paths to try, including standard Unix locations outside GUI PATH. */
export function zshSearchPaths(platform: NodeJS.Platform, pathValue: string | undefined): string[] {
  const paths = platform === "win32" ? path.win32 : path.posix;
  const directories = (pathValue ?? "").split(paths.delimiter).filter(Boolean);
  if (platform !== "win32") {
    directories.push("/bin", "/usr/bin", "/usr/local/bin", "/opt/homebrew/bin");
  }
  const executable = platform === "win32" ? "zsh.exe" : "zsh";
  return [...new Set(directories.map((directory) => paths.resolve(directory, executable)))];
}

/**
 * Picks the lowest available pi.dev terminal name so concurrent editor tabs stay
 * distinguishable and closing one makes its number available again.
 */
export function nextPiTerminalName(existing: readonly string[]): string {
  const taken = new Set(existing);
  if (!taken.has(PI_TERMINAL_NAME)) {
    return PI_TERMINAL_NAME;
  }
  for (let n = 2; ; n += 1) {
    const candidate = `${PI_TERMINAL_NAME} ${n}`;
    if (!taken.has(candidate)) {
      return candidate;
    }
  }
}

export type PiLaunchCheck =
  | { readonly kind: "ready"; readonly zshPath: string }
  | { readonly kind: "missing-zsh" }
  | { readonly kind: "missing-pi" };

/** Resolves zsh, then verifies pi inside that shell's login environment. */
export async function checkPiLaunch(
  findZsh: () => Promise<string | undefined>,
  findPiInZsh: (zshPath: string) => Promise<boolean>,
): Promise<PiLaunchCheck> {
  const zshPath = await findZsh();
  if (!zshPath) {
    return { kind: "missing-zsh" };
  }
  if (!(await findPiInZsh(zshPath))) {
    return { kind: "missing-pi" };
  }
  return { kind: "ready", zshPath };
}
