// The pure Claude-session cue model for the Worktrees tree (#1406): tally the
// daemon's live sessions onto the worktrees they are running in, and decide the
// row's glyph summary and colored badge.
//
// Nothing here imports `vscode`, so it runs under a plain Node process
// (`node --test out/`) like `tree.ts` and `socket.ts`. The `vscode`-facing
// halves are `treeDataProvider.ts` (description/tooltip) and `decorations.ts`
// (the `FileDecoration`), exactly as they are for the PR CI-check badge.
//
// Session state itself comes from the daemon's `sessions` service; it is exact
// for Claude tabs launched through the `omni-dev claude-wrap` stream wrapper and
// inferred for everything else (see ADR-0052 and ADR-0057).

import { CheckDecoration } from "./tree";

/**
 * A session's live state, as the daemon serializes it (`snake_case`, mirroring
 * the Rust `SessionState`).
 */
export type SessionState =
  | "starting"
  | "working"
  | "idle"
  | "waiting_for_input"
  | "waiting_for_permission"
  | "ended";

/**
 * One entry of the sessions service's `list` reply. Only the two fields the cue
 * needs are declared; the daemon sends more.
 */
export interface SessionEntry {
  /** The Claude session id. */
  session_id: string;
  /** The session's working directory, when the daemon has learned one. */
  cwd?: string;
  /** The session's live state. */
  state: SessionState;
}

/** How many sessions of each coarse kind a worktree is running. */
export interface SessionTally {
  /** Actively processing a turn. */
  working: number;
  /** Blocked on the user (a permission prompt, or plain input). */
  waiting: number;
  /** Sitting at the prompt. */
  idle: number;
}

/** Per-worktree-path tallies, keyed by the path the daemon reported. */
export type SessionTallyMap = Record<string, SessionTally>;

/** The glyphs, matching the daemon tray's own vocabulary. */
const GLYPHS: Record<keyof SessionTally, string> = {
  waiting: "!",
  working: "⚙",
  idle: "◦",
};

/**
 * The order buckets are rendered and ranked in, most urgent first: a worktree
 * that is waiting on you should say so even while other sessions in it work.
 */
const BY_URGENCY: (keyof SessionTally)[] = ["waiting", "working", "idle"];

/** An empty tally, the identity every accumulation starts from. */
function emptyTally(): SessionTally {
  return { working: 0, waiting: 0, idle: 0 };
}

/**
 * The bucket a state counts towards, or `undefined` for states that should not
 * be shown at all.
 *
 * `ended` is dropped rather than bucketed: the daemon keeps an ended session
 * visible for ~10s so `sessions list` can show it, which is useful in a table
 * and misleading as a row badge.
 */
function bucketFor(state: SessionState): keyof SessionTally | undefined {
  switch (state) {
    case "starting":
    case "working":
      return "working";
    case "waiting_for_input":
    case "waiting_for_permission":
      return "waiting";
    case "idle":
      return "idle";
    case "ended":
      return undefined;
  }
}

/**
 * The longest of `paths` that contains `cwd`, or `undefined` when none does.
 *
 * Matching is on a path *boundary*, so `/w/foo` never claims a session running
 * in `/w/foo-bar`; longest wins, so a session inside a nested worktree is
 * attributed to that worktree rather than to its parent repository.
 */
function containingPath(cwd: string, paths: readonly string[]): string | undefined {
  let best: string | undefined;
  for (const candidate of paths) {
    // A trailing slash is not part of the identity: compare against the bare
    // path, but key the tally by whatever the daemon actually reported.
    const bare = candidate.endsWith("/") ? candidate.slice(0, -1) : candidate;
    if (cwd !== bare && !cwd.startsWith(`${bare}/`)) {
      continue;
    }
    if (best === undefined || candidate.length > best.length) {
      best = candidate;
    }
  }
  return best;
}

/**
 * Tallies live sessions onto the worktrees they are running in.
 *
 * Sessions with no `cwd` (the daemon has not learned one yet), sessions outside
 * every known worktree, and ended sessions are all skipped; a worktree with no
 * sessions gets no entry at all rather than a zeroed one.
 */
export function tallyByWorktree(
  sessions: readonly SessionEntry[],
  paths: readonly string[],
): SessionTallyMap {
  const tallies: SessionTallyMap = {};
  for (const session of sessions) {
    const bucket = bucketFor(session.state);
    if (bucket === undefined || !session.cwd) {
      continue;
    }
    const worktree = containingPath(session.cwd, paths);
    if (worktree === undefined) {
      continue;
    }
    const tally = tallies[worktree] ?? emptyTally();
    tally[bucket] += 1;
    tallies[worktree] = tally;
  }
  return tallies;
}

/** How many sessions a tally counts in total. */
export function sessionTotal(tally: SessionTally): number {
  return tally.working + tally.waiting + tally.idle;
}

/**
 * Counts live (non-`ended`) sessions whose `cwd` matches no known worktree
 * path — the population {@link tallyByWorktree} silently drops as "outside
 * every worktree" (#1447). Kept separate from that function's return so its
 * contract stays exactly `SessionTallyMap`; a caller that wants this count for
 * diagnostics runs it as its own pass.
 */
export function countUnmatchedSessions(
  sessions: readonly SessionEntry[],
  paths: readonly string[],
): number {
  let count = 0;
  for (const session of sessions) {
    if (session.state === "ended" || !session.cwd) {
      continue;
    }
    if (containingPath(session.cwd, paths) === undefined) {
      count += 1;
    }
  }
  return count;
}

/**
 * The muted row segment, e.g. `!1 ⚙2` — one glyph-and-count per non-empty
 * bucket, most urgent first. Empty for a worktree with no sessions, so it drops
 * out of the row description entirely.
 */
export function sessionGlyphs(tally: SessionTally | undefined): string {
  if (tally === undefined) {
    return "";
  }
  return BY_URGENCY.filter((bucket) => tally[bucket] > 0)
    .map((bucket) => `${GLYPHS[bucket]}${tally[bucket]}`)
    .join(" ");
}

/** The word for a bucket, used in the tooltip. */
function bucketWord(bucket: keyof SessionTally): string {
  return bucket === "waiting" ? "waiting on you" : bucket;
}

/**
 * The tooltip's Claude line, e.g. `Claude: 1 waiting on you, 2 working`, or
 * `undefined` when the worktree runs no sessions.
 */
export function sessionTooltipLine(tally: SessionTally | undefined): string | undefined {
  if (tally === undefined || sessionTotal(tally) === 0) {
    return undefined;
  }
  const parts = BY_URGENCY.filter((bucket) => tally[bucket] > 0).map(
    (bucket) => `${tally[bucket]} ${bucketWord(bucket)}`,
  );
  return `Claude: ${parts.join(", ")}`;
}

/**
 * A `FileDecoration` badge is capped at two characters, so a count of ten or
 * more degrades to a `+`. The exact number stays in the description and tooltip.
 */
function badge(glyph: string, count: number): string {
  return count > 9 ? `${glyph}+` : `${glyph}${count}`;
}

/**
 * The colored badge for a worktree row's sessions, or `undefined` when it runs
 * none.
 *
 * The dominant bucket wins by urgency — yellow `!` waiting outranks green `⚙`
 * working outranks muted `◦` idle — so one glance says whether a worktree needs
 * you. Mirrors {@link CheckDecoration}'s contract, which the decoration
 * provider consumes uniformly.
 */
export function sessionDecoration(tally: SessionTally | undefined): CheckDecoration | undefined {
  if (tally === undefined) {
    return undefined;
  }
  if (tally.waiting > 0) {
    return {
      badge: badge(GLYPHS.waiting, tally.waiting),
      colorId: "charts.yellow",
      tooltip: sessionTooltipLine(tally) ?? "",
    };
  }
  if (tally.working > 0) {
    return {
      badge: badge(GLYPHS.working, tally.working),
      colorId: "charts.green",
      tooltip: sessionTooltipLine(tally) ?? "",
    };
  }
  if (tally.idle > 0) {
    return {
      badge: badge(GLYPHS.idle, tally.idle),
      // Not a `charts.*` colour: an idle session is deliberately muted, the same
      // grey the row description already uses.
      colorId: "descriptionForeground",
      tooltip: sessionTooltipLine(tally) ?? "",
    };
  }
  return undefined;
}

/**
 * Encodes a tally into a worktree row's `resourceUri` query.
 *
 * The state has to ride the URI because that is what makes a change produce a
 * *new* URI, which VS Code re-queries for a decoration on its own — the same
 * trick the `checks=<state>` parameter plays.
 */
export function encodeSessionTally(tally: SessionTally): string {
  return `${tally.working}-${tally.waiting}-${tally.idle}`;
}

/** The exact shape {@link encodeSessionTally} writes; anything else is garbage. */
const ENCODED_TALLY = /^(\d+)-(\d+)-(\d+)$/;

/** Reads back what {@link encodeSessionTally} wrote, tolerating any garbage. */
export function decodeSessionTally(encoded: string | null | undefined): SessionTally | undefined {
  const match = encoded ? ENCODED_TALLY.exec(encoded) : null;
  if (!match) {
    return undefined;
  }
  return {
    working: Number(match[1]),
    waiting: Number(match[2]),
    idle: Number(match[3]),
  };
}

/**
 * Whether two tally maps are equivalent.
 *
 * The tree provider refreshes only on a real change: firing its
 * `onDidChangeTreeData` re-runs `getChildren`, which re-triggers the lazy
 * ahead/behind and PR-badge fetches, so an unchanged poll must be a no-op.
 */
export function sameTallies(left: SessionTallyMap, right: SessionTallyMap): boolean {
  const keys = Object.keys(left);
  if (keys.length !== Object.keys(right).length) {
    return false;
  }
  return keys.every((key) => {
    const a = left[key];
    const b = right[key];
    return (
      b !== undefined && a.working === b.working && a.waiting === b.waiting && a.idle === b.idle
    );
  });
}
