// Pure detection of a window's embedded Claude Code sessions — editor webview
// tabs and integrated terminals — for the sessions service's `window` report
// (#1210). Deliberately free of any `vscode` import so it is unit-testable under
// a plain Node process (like `socket.ts`); `extension.ts` pulls the raw
// viewType/name strings from the VS Code API and calls these.
//
// The matching is by substring, on purpose: a Claude editor tab's webview
// `viewType` is mangled by VS Code (prefixed, e.g. `mainThreadWebview-…`), and a
// Claude terminal's name is user-configurable, so an exact-equals check would be
// brittle. See ADR-0050 and docs/sessions-service.md for why counts (not
// session ids) are all a companion can report.

/**
 * Substring identifying a Claude Code editor tab's webview `viewType`. The real
 * `viewType` is mangled (prefixed) by VS Code, so a `.includes` match — not an
 * equality check — is what reliably recognises it.
 */
export const CLAUDE_WEBVIEW_MARKER = "claudeVSCodePanel";

/**
 * Case-insensitive substring identifying a Claude Code integrated terminal by
 * its name. `"Claude Code"` is the default title; a user can override it via
 * `$CLAUDE_CODE_TERMINAL_TITLE`, which the caller passes as `customTitle`.
 */
export const CLAUDE_TERMINAL_MARKER = "claude";

/**
 * Counts the Claude Code editor tabs among a window's webview `viewType`s — the
 * ones whose (mangled) viewType contains {@link CLAUDE_WEBVIEW_MARKER}.
 */
export function countClaudeTabs(viewTypes: readonly string[]): number {
  return viewTypes.filter((v) => v.includes(CLAUDE_WEBVIEW_MARKER)).length;
}

/**
 * Whether a terminal `name` looks like a Claude Code terminal: it contains the
 * default {@link CLAUDE_TERMINAL_MARKER} (case-insensitive) or exactly matches a
 * non-empty `customTitle` (`$CLAUDE_CODE_TERMINAL_TITLE`).
 */
export function isClaudeTerminalName(name: string, customTitle?: string): boolean {
  if (name.toLowerCase().includes(CLAUDE_TERMINAL_MARKER)) {
    return true;
  }
  const title = customTitle?.trim();
  return !!title && name === title;
}

/**
 * Counts the Claude Code integrated terminals among a window's terminal `names`,
 * honouring an optional `$CLAUDE_CODE_TERMINAL_TITLE` override.
 */
export function countClaudeTerminals(
  names: readonly string[],
  customTitle?: string,
): number {
  return names.filter((n) => isClaudeTerminalName(n, customTitle)).length;
}
