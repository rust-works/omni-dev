import type * as vscode from "vscode";

export const TERMINAL_IN_EDITOR_PROFILE_ID = "omniDevWorktrees.terminalInEditor";

/** The native API surface needed to register the shell profile. */
type TerminalProfileApi = {
  window: Pick<typeof vscode.window, "registerTerminalProfileProvider">;
  TerminalProfile: typeof vscode.TerminalProfile;
  TerminalLocation: typeof vscode.TerminalLocation;
};

/** Register independently of the daemon; VS Code creates and focuses each terminal. */
export function registerTerminalInEditor(api: TerminalProfileApi): vscode.Disposable {
  return api.window.registerTerminalProfileProvider(TERMINAL_IN_EDITOR_PROFILE_ID, {
    provideTerminalProfile(token) {
      if (token.isCancellationRequested) {
        return undefined;
      }
      // Omit shell/cwd overrides so the native profile flow resolves user settings.
      return new api.TerminalProfile({ location: api.TerminalLocation.Editor });
    },
  });
}
