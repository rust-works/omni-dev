import type * as vscode from "vscode";

export const TERMINAL_IN_EDITOR_PROFILE_ID = "omniDevWorktrees.terminalInEditor";

/** The native API surface needed to register the shell profile. */
type TerminalProfileApi = {
  window: Pick<typeof vscode.window, "registerTerminalProfileProvider" | "onDidOpenTerminal">;
  env: Pick<typeof vscode.env, "shell">;
  workspace: {
    getConfiguration(section: string): Pick<vscode.WorkspaceConfiguration, "get">;
  };
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
      const config = api.workspace.getConfiguration("terminal.integrated");
      const platform = process.platform === "darwin" ? "osx"
        : process.platform === "win32" ? "windows" : "linux";
      const name = config.get<string>(`defaultProfile.${platform}`);
      const profiles = config.get<Record<string, {
        args?: string | string[];
        env?: Record<string, string | null>;
      } | null>>(`profiles.${platform}`);
      const profile = name ? profiles?.[name] : undefined;
      const options: vscode.TerminalOptions = { location: api.TerminalLocation.Editor };
      // Extension-owned terminals ignore terminal.integrated.cwd. Carry the
      // user's setting explicitly, leaving VS Code to resolve variables/folders.
      const cwd = config.get<string>("cwd");
      if (cwd?.trim()) {
        options.cwd = cwd;
      }
      // An explicitly configured default profile must retain its arguments and
      // environment. env.shell is VS Code's resolved default shell for this host,
      // including profiles defined with a source or multiple candidate paths.
      if (profile && api.env.shell) {
        options.shellPath = api.env.shell;
        options.shellArgs = profile.args;
        options.env = profile.env;
      }
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
