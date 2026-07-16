// Pure helpers for the "Open Claude Code" title-bar button. Nothing here imports
// `vscode`, so it runs under a plain Node process (`node --test out/`); the thin
// `vscode`-facing launch/reuse wiring lives in `extension.ts`. See issue #1322.

/** Fallback launch command when the `claudeCommand` setting is unset/blank. */
export const DEFAULT_CLAUDE_COMMAND = "claude";

/**
 * The base name of the editor-area terminals the button opens (#1322, #1347). The
 * first session takes this name verbatim; concurrent sessions are numbered
 * `Claude Code 2`, `Claude Code 3`, … by {@link nextClaudeTerminalName}, so open
 * tabs stay distinguishable and a closed session frees its number for reuse.
 */
export const CLAUDE_TERMINAL_NAME = "Claude Code";

/**
 * Picks the name for a new Claude terminal: the lowest free name in the sequence
 * `Claude Code`, `Claude Code 2`, `Claude Code 3`, …, given the names already in
 * use. Every click of the button opens a fresh session (#1347), so distinct names
 * keep the editor tabs apart; drawing from the lowest free number means closing a
 * session frees its number for the next launch rather than letting a counter climb
 * forever.
 *
 * `existing` is fed **all** of the window's terminal names, not just Claude's, so
 * an unrelated terminal a user renamed `Claude Code` still pushes ours to the next
 * number.
 */
export function nextClaudeTerminalName(existing: readonly string[]): string {
  const taken = new Set(existing);
  if (!taken.has(CLAUDE_TERMINAL_NAME)) {
    return CLAUDE_TERMINAL_NAME;
  }
  for (let n = 2; ; n += 1) {
    const candidate = `${CLAUDE_TERMINAL_NAME} ${n}`;
    if (!taken.has(candidate)) {
      return candidate;
    }
  }
}

/**
 * Normalizes the configured launch command: trims surrounding whitespace and
 * falls back to {@link DEFAULT_CLAUDE_COMMAND} when unset or blank. Anything else
 * is passed through verbatim, including a shell prefix such as `proxy && claude`.
 */
export function resolveClaudeCommand(raw: string | undefined): string {
  const trimmed = raw?.trim();
  return trimmed ? trimmed : DEFAULT_CLAUDE_COMMAND;
}

/**
 * Picks the working directory for a new Claude terminal from the window's open
 * folders. Prefers `activeFolder` (the folder containing the focused editor) when
 * it is one of `folders`, else the first folder, else `undefined` — in which case
 * the caller lets VS Code fall back to the terminal's default cwd.
 */
export function resolveClaudeCwd(
  folders: readonly string[],
  activeFolder?: string,
): string | undefined {
  if (activeFolder && folders.includes(activeFolder)) {
    return activeFolder;
  }
  return folders[0];
}
