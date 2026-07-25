// The `vscode`-facing file-decoration layer for the Worktrees tree: a
// `FileDecorationProvider` that paints a colored badge (and tints the row label)
// for a worktree's PR CI-check state (#1324) or its running Claude sessions
// (#1406). Both color/glyph decisions are pure and unit-tested — in `tree.ts`
// and `sessionCounts.ts` respectively; this file only owns the custom
// `resourceUri` scheme, the priority between the two, and the mapping onto a
// `vscode.FileDecoration`.
//
// A custom scheme (not `file:`) keeps these decorations from colliding with the
// built-in git SCM provider, which decorates real folder URIs. Both states are
// encoded in the URI query, so a change yields a new URI that re-decorates on
// its own; `refresh()` additionally re-queries every visible row when a new
// snapshot, PR-badge fetch, or session poll lands.

import * as vscode from "vscode";

import {
  SessionTally,
  decodeSessionTally,
  encodeSessionTally,
  sessionDecoration,
} from "./sessionCounts";
import { CheckDecoration, PrCheckState, checkStateDecoration } from "./tree";

/**
 * The custom URI scheme carried by every worktree row that has a check badge. Kept
 * distinct from `file:` so the built-in git SCM decoration provider — which
 * decorates real folder URIs — never fights over these rows.
 */
export const WORKTREE_URI_SCHEME = "omnidev-worktree";

/**
 * Builds a worktree row's decoratable `resourceUri`: the custom scheme, the
 * worktree path, and the decoratable state in the query — the PR `checks` state
 * and, since #1406, the row's Claude session tally. Encoding the state means a
 * change (e.g. `pending` → `success`, or a session starting to wait) produces a
 * **new** URI, which VS Code re-queries for a decoration on its own.
 */
export function worktreeResourceUri(
  path: string,
  checks: PrCheckState,
  sessions?: SessionTally,
): vscode.Uri {
  const query = new URLSearchParams({ checks });
  if (sessions) {
    query.set("claude", encodeSessionTally(sessions));
  }
  return vscode.Uri.from({ scheme: WORKTREE_URI_SCHEME, path, query: query.toString() });
}

/**
 * The single decoration a worktree row gets, from the two dimensions its query
 * can carry.
 *
 * VS Code allows one `FileDecoration` per URI per provider, so when a row both
 * runs Claude sessions and has a PR, the sessions win: they are the live,
 * changing thing, and the PR's check state is still one hover away in the
 * tooltip. A row with neither gets no decoration at all.
 */
function rowDecoration(query: string): CheckDecoration | undefined {
  const params = new URLSearchParams(query);
  const sessions = sessionDecoration(decodeSessionTally(params.get("claude")));
  if (sessions) {
    return sessions;
  }
  const checks = params.get("checks") as PrCheckState | null;
  return checks ? checkStateDecoration(checks) : undefined;
}

/**
 * Paints a worktree row's colored badge: its Claude session cue (#1406) or, when
 * it runs none, its PR CI-check verdict (#1324). For an `omnidev-worktree:` URI
 * it reads both states back from the query and maps the winner — via the pure
 * {@link sessionDecoration} / {@link checkStateDecoration} — to a
 * `vscode.FileDecoration` (badge + `ThemeColor`); every other scheme, and a row
 * with nothing to show, yields no decoration. `propagate = false` keeps the tint
 * on the worktree row and off its repo parent.
 */
export class WorktreeDecorationProvider implements vscode.FileDecorationProvider {
  private readonly emitter = new vscode.EventEmitter<vscode.Uri | vscode.Uri[] | undefined>();
  readonly onDidChangeFileDecorations = this.emitter.event;

  provideFileDecoration(uri: vscode.Uri): vscode.FileDecoration | undefined {
    if (uri.scheme !== WORKTREE_URI_SCHEME) {
      return undefined;
    }
    const decoration = rowDecoration(uri.query);
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
