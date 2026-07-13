// Pure helpers for the "Open Claude Code" title-bar button. Nothing here imports
// `vscode`, so it runs under a plain Node process (`node --test out/`); the thin
// `vscode`-facing launch/reuse wiring lives in `extension.ts`. See issue #1322.

/** Fallback launch command when the `claudeCommand` setting is unset/blank. */
export const DEFAULT_CLAUDE_COMMAND = "claude";

/**
 * The name of the editor-area terminal the button opens. The title-bar button is
 * a single, window-level session (not per-worktree, unlike #1317), so one stable
 * name is all we need — and it keeps the reuse check to "is that terminal still
 * open?".
 */
export const CLAUDE_TERMINAL_NAME = "Claude Code";

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
