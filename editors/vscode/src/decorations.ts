// The `vscode`-facing file-decoration layer for the Worktrees tree: the badges
// that carry a worktree's PR CI-check verdict (#1324) and colours that also
// reflect its running agent sessions (#1406). Every glyph/colour decision is
// pure and unit-tested in `tree.ts` and `sessionCounts.ts`; this file owns only
// the custom `resourceUri` scheme and the mapping onto a `vscode.FileDecoration`.
//
// A custom scheme (not `file:`) keeps these decorations from colliding with the
// built-in git SCM provider, which decorates real folder URIs. Both states are
// encoded in the URI query, so a change yields a new URI that re-decorates on
// its own; `refresh()` additionally re-queries every visible row when a new
// snapshot, PR-badge fetch, or session poll lands.
//
// One provider owns the row decoration. Only PR checks contribute a badge;
// sessions already show their full breakdown in the description and tooltip.
// Sessions still contribute colour through `rowColorId`: red (checks failing)
// outranks yellow (checks pending, or a session waiting on you) outranks green
// (checks passing, or a session working) outranks muted (idle). Session-only
// rows receive a colour and tooltip without a badge.

import * as vscode from "vscode";

import {
  SessionTally,
  decodeSessionTally,
  encodeSessionTally,
  sessionDecoration,
} from "./sessionCounts";
import { CheckDecoration, PrCheckState, checkStateDecoration, rowColorId } from "./tree";

/**
 * The custom URI scheme carried by every worktree row that has a decoration. Kept
 * distinct from `file:` so the built-in git SCM decoration provider — which
 * decorates real folder URIs — never fights over these rows.
 */
export const WORKTREE_URI_SCHEME = "omnidev-worktree";

/**
 * Builds a worktree row's decoratable `resourceUri`: the custom scheme, the
 * worktree path, and both decoratable states in the query — the PR `checks`
 * verdict and the row's agent session tally. Encoding the state means a change
 * (e.g. `pending` → `success`, or a session starting to wait) produces a **new**
 * URI, which VS Code re-queries for a decoration on its own.
 */
export function worktreeResourceUri(
  path: string,
  checks: PrCheckState,
  sessions?: SessionTally,
): vscode.Uri {
  const query = new URLSearchParams({ checks });
  if (sessions) {
    // Keyed `claude` for wire compatibility with URIs minted before sessions
    // gained an agent tag (#1908); it now carries every agent's tally. Debt: a
    // rename would need both keys read for one release.
    query.set("claude", encodeSessionTally(sessions));
  }
  return vscode.Uri.from({ scheme: WORKTREE_URI_SCHEME, path, query: query.toString() });
}

/** Both dimensions, decoded back out of a row's `resourceUri` query. */
function rowDecorations(query: string): {
  sessions?: CheckDecoration;
  checks?: CheckDecoration;
} {
  const params = new URLSearchParams(query);
  const checks = params.get("checks") as PrCheckState | null;
  return {
    sessions: sessionDecoration(decodeSessionTally(params.get("claude"))),
    checks: checks ? checkStateDecoration(checks) : undefined,
  };
}

/**
 * Paints the PR-check badge in the combined check/session severity colour.
 * `propagate = false` keeps the tint on the worktree row and off its repo parent.
 */
export class WorktreeDecorationProvider implements vscode.FileDecorationProvider {
  private readonly emitter = new vscode.EventEmitter<vscode.Uri | vscode.Uri[] | undefined>();
  readonly onDidChangeFileDecorations = this.emitter.event;

  provideFileDecoration(uri: vscode.Uri): vscode.FileDecoration | undefined {
    if (uri.scheme !== WORKTREE_URI_SCHEME) {
      return undefined;
    }
    const { sessions, checks } = rowDecorations(uri.query);
    if (!checks && !sessions) {
      return undefined;
    }
    // Sessions retain their colour contribution even though checks own the badge.
    const colorId = rowColorId(checks?.colorId, sessions?.colorId);
    const decoration = new vscode.FileDecoration(
      checks?.badge,
      checks?.tooltip ?? sessions?.tooltip,
      colorId ? new vscode.ThemeColor(colorId) : undefined,
    );
    decoration.propagate = false;
    return decoration;
  }

  /**
   * Re-evaluates the badge and colour on every visible row. Fired when a new
   * snapshot, a lazy PR-badge fetch, or a session poll may have changed a
   * worktree's state, so colours refresh even for a row whose `resourceUri`
   * string is unchanged.
   */
  refresh(): void {
    this.emitter.fire(undefined);
  }

  dispose(): void {
    this.emitter.dispose();
  }
}
