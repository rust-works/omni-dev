// The `vscode`-facing file-decoration layer for the Worktrees tree: the badges
// that carry a worktree's PR CI-check verdict (#1324) and its running Claude
// sessions (#1406). Every glyph/colour decision is pure and unit-tested — in
// `tree.ts` and `sessionCounts.ts`; this file owns only the custom `resourceUri`
// scheme and the mapping onto a `vscode.FileDecoration`.
//
// A custom scheme (not `file:`) keeps these decorations from colliding with the
// built-in git SCM provider, which decorates real folder URIs. Both states are
// encoded in the URI query, so a change yields a new URI that re-decorates on
// its own; `refresh()` additionally re-queries every visible row when a new
// snapshot, PR-badge fetch, or session poll lands.
//
// **Why two providers.** A single `FileDecoration.badge` is capped at two
// characters by the extension host, so one decoration cannot carry both a
// session cue and a check verdict. VS Code does merge across providers — it
// concatenates their badges (`⚙1, ✓`) and joins their tooltips — but it paints
// the merged result in exactly *one* colour, chosen by an internal ordering an
// extension cannot influence (`weight` is not on the API, and provider order is
// registration order reversed). So both providers here compute the **same**
// severity-ranked colour via `rowColorId`, which makes that choice moot: red
// (checks failing) outranks yellow (checks pending, or a session waiting on you)
// outranks green (checks passing, or a session working) outranks muted (idle).

import * as vscode from "vscode";

import {
  SessionTally,
  decodeSessionTally,
  encodeSessionTally,
  sessionDecoration,
} from "./sessionCounts";
import { CheckDecoration, PrCheckState, checkStateDecoration, rowColorId } from "./tree";

/**
 * The custom URI scheme carried by every worktree row that has a badge. Kept
 * distinct from `file:` so the built-in git SCM decoration provider — which
 * decorates real folder URIs — never fights over these rows.
 */
export const WORKTREE_URI_SCHEME = "omnidev-worktree";

/**
 * Which of the two badge dimensions a provider renders. Each gets its own
 * provider because a decoration's badge holds at most two characters.
 */
export type BadgeDimension = "sessions" | "checks";

/**
 * Builds a worktree row's decoratable `resourceUri`: the custom scheme, the
 * worktree path, and both decoratable states in the query — the PR `checks`
 * verdict and the row's Claude session tally. Encoding the state means a change
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
 * Paints one dimension of a worktree row's badge, in the colour of whichever
 * dimension is more severe.
 *
 * Two of these are registered, one per {@link BadgeDimension}; VS Code merges
 * them onto the row. `propagate = false` keeps the tint on the worktree row and
 * off its repo parent.
 */
export class WorktreeDecorationProvider implements vscode.FileDecorationProvider {
  private readonly emitter = new vscode.EventEmitter<vscode.Uri | vscode.Uri[] | undefined>();
  readonly onDidChangeFileDecorations = this.emitter.event;

  constructor(private readonly dimension: BadgeDimension) {}

  provideFileDecoration(uri: vscode.Uri): vscode.FileDecoration | undefined {
    if (uri.scheme !== WORKTREE_URI_SCHEME) {
      return undefined;
    }
    const { sessions, checks } = rowDecorations(uri.query);
    const mine = this.dimension === "sessions" ? sessions : checks;
    if (!mine) {
      return undefined;
    }
    // Deliberately not `mine.colorId`: the merged badge gets one colour, so both
    // providers agree on the severity winner rather than racing for it.
    const colorId = rowColorId(checks?.colorId, sessions?.colorId);
    const decoration = new vscode.FileDecoration(
      mine.badge,
      mine.tooltip,
      colorId ? new vscode.ThemeColor(colorId) : undefined,
    );
    decoration.propagate = false;
    return decoration;
  }

  /**
   * Re-evaluates this dimension's badge on every visible row. Fired when a new
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
