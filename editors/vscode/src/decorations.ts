// The `vscode`-facing file-decoration layer for the Worktrees tree (#1324): a
// `FileDecorationProvider` that paints a colored ✓/✗/● badge (and tints the row
// label) for a worktree's PR CI-check state. The color/glyph decision itself is the
// pure, unit-tested `checkStateDecoration` in `tree.ts`; this file only owns the
// custom `resourceUri` scheme and maps that decision onto a `vscode.FileDecoration`.
//
// A custom scheme (not `file:`) keeps these decorations from colliding with the
// built-in git SCM provider, which decorates real folder URIs. The check state is
// encoded in the URI query, so a state change yields a new URI that re-decorates on
// its own; `refresh()` additionally re-queries every visible row when a new snapshot
// or PR-badge fetch lands.

import * as vscode from "vscode";

import { PrCheckState, checkStateDecoration } from "./tree";

/**
 * The custom URI scheme carried by every worktree row that has a check badge. Kept
 * distinct from `file:` so the built-in git SCM decoration provider — which
 * decorates real folder URIs — never fights over these rows.
 */
export const WORKTREE_URI_SCHEME = "omnidev-worktree";

/**
 * Builds a worktree row's decoratable `resourceUri`: the custom scheme, the
 * worktree path, and the `checks` state in the query. Encoding the state means a
 * change (e.g. `pending` → `success`) produces a **new** URI, which VS Code
 * re-queries for a decoration on its own.
 */
export function worktreeResourceUri(path: string, checks: PrCheckState): vscode.Uri {
  return vscode.Uri.from({ scheme: WORKTREE_URI_SCHEME, path, query: `checks=${checks}` });
}

/**
 * Paints the colored ✓/✗/● PR-check badge on worktree rows (#1324). For an
 * `omnidev-worktree:` URI it reads the `checks` state back from the query and maps
 * it — via the pure {@link checkStateDecoration} — to a `vscode.FileDecoration`
 * (badge + `ThemeColor`); every other scheme, and the `none` state, yields no
 * decoration. `propagate = false` keeps the tint on the worktree row and off its
 * repo parent.
 */
export class WorktreeDecorationProvider implements vscode.FileDecorationProvider {
  private readonly emitter = new vscode.EventEmitter<vscode.Uri | vscode.Uri[] | undefined>();
  readonly onDidChangeFileDecorations = this.emitter.event;

  provideFileDecoration(uri: vscode.Uri): vscode.FileDecoration | undefined {
    if (uri.scheme !== WORKTREE_URI_SCHEME) {
      return undefined;
    }
    const checks = new URLSearchParams(uri.query).get("checks") as PrCheckState | null;
    const decoration = checks ? checkStateDecoration(checks) : undefined;
    if (!decoration) {
      return undefined;
    }
    const fileDecoration = new vscode.FileDecoration(
      decoration.badge,
      decoration.tooltip,
      new vscode.ThemeColor(decoration.colorId),
    );
    // Tint the worktree row only — never propagate up to (and colour) its repo row.
    fileDecoration.propagate = false;
    return fileDecoration;
  }

  /**
   * Re-evaluates the badge on every visible row. Fired when a new snapshot or a
   * lazy PR-badge fetch may have changed a worktree's check state, so colours
   * refresh even for a row whose `resourceUri` string is unchanged.
   */
  refresh(): void {
    this.emitter.fire(undefined);
  }

  dispose(): void {
    this.emitter.dispose();
  }
}
