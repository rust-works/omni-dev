import type { ExtensionTerminalOptions, TerminalOptions } from "vscode";

const AGENT_ENV = "OMNI_DEV_TERMINAL_AGENT";

/**
 * Seed an application-controlled title without TerminalOptions.name: VS Code
 * treats that option as a static title and stops listening for OSC updates.
 * Keep the launcher identity separate from the title for session reporting.
 */
export function agentTerminalOptions(
  agent: "claude" | "pi",
  initialTitle: string,
): Pick<TerminalOptions, "message" | "env"> {
  const title = initialTitle.replace(/[\x00-\x1f\x7f-\x9f]/g, "");
  return {
    message: `\x1b]0;${title}\x07`,
    env: { [AGENT_ENV]: agent },
  };
}

/** Stable counting identity, even when a session is renamed to e.g. "Blah". */
export function agentTerminalIdentity(
  name: string,
  options: Readonly<TerminalOptions | ExtensionTerminalOptions>,
): string {
  const agent = "env" in options ? options.env?.[AGENT_ENV] : undefined;
  switch (agent) {
    case "claude": return "Claude Code";
    case "pi": return "pi.dev";
    default: return name;
  }
}
