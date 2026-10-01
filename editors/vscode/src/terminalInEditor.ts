import type * as vscode from "vscode";

export const TERMINAL_IN_EDITOR_PROFILE_ID = "omniDevWorktrees.terminalInEditor";

/** The native API surface needed to register the shell profile. */
type TerminalProfileApi = {
  window: Pick<typeof vscode.window, "registerTerminalProfileProvider" | "onDidOpenTerminal">;
  TerminalProfile: typeof vscode.TerminalProfile;
  TerminalLocation: typeof vscode.TerminalLocation;
};

/** Register independently of the daemon and focus only terminals supplied by this profile. */
export function registerTerminalInEditor(api: TerminalProfileApi): vscode.Disposable {
  const pending = new WeakSet<vscode.TerminalOptions>();
  let disposed = false;
  const opened = api.window.onDidOpenTerminal((terminal) => {
    if (!pending.delete(terminal.creationOptions)) {
      return;
    }
    // The panel's profile menu refocuses its own active terminal after creation,
    // even when the provider requests Editor. Wait for our shell to start, then
    // reveal this exact terminal; never steal focus for unrelated profiles.
    void Promise.resolve(terminal.processId)
      .then(() => {
        if (!disposed) {
          terminal.show(false);
        }
      })
      .catch(() => {
        // A terminal closed during shell startup no longer needs revealing.
      });
  });
  const provider = api.window.registerTerminalProfileProvider(TERMINAL_IN_EDITOR_PROFILE_ID, {
    provideTerminalProfile(token) {
      if (token.isCancellationRequested || disposed) {
        return undefined;
      }
      // Omit shell/cwd overrides so the native profile flow resolves user settings.
      const options = { location: api.TerminalLocation.Editor };
      pending.add(options);
      return new api.TerminalProfile(options);
    },
  });
  return {
    dispose() {
      disposed = true;
      provider.dispose();
      opened.dispose();
    },
  };
}
