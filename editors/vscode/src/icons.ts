// The pure row-icon layer for the Worktrees tree (#1428): which codicon a repo or
// worktree row renders, and in which colour. Nothing here imports `vscode`, so the
// precedence rule below is unit-tested under a plain Node process — the same split
// `tree.ts` (formatters) and `sessionCounts.ts` (session cues) already use, with
// `treeDataProvider.ts` reduced to mapping a {@link RowIcon} onto a `vscode.ThemeIcon`.
//
// The colour of an individual row is user-configurable via `omniDevWorktrees.rowColors`,
// a map from {@link nodeId} to a workbench colour id. A colour attaches to a **row**,
// not to a row *state* and not to a glyph: once tagged, a row keeps its colour whichever
// icon it is currently rendering. That is the whole point — in a tree where nearly every
// row sits in the same "open in another window" state, colouring by state repaints
// almost everything identically and differentiates nothing. It is also what makes the
// tag stable: for the ~10s after a daemon restart, before every window has
// re-registered, every row transiently reports "closed", which a state-keyed colour
// would visibly follow and a row-keyed one does not.

import {
  Node,
  TreeRepoPayload,
  TreeWorktreePayload,
  isCurrentWindow,
  nodeId,
  repoPollingEnabled,
  worktreeRebaseCue,
} from "./tree";

/**
 * A row's icon: the codicon id, and the workbench colour id to tint it with.
 *
 * An absent `colorId` means **pass no `ThemeColor` at all**, so the icon inherits the
 * theme — which is how the two currently-uncoloured states (a non-GitHub repo, and a
 * worktree with no live window) render today.
 */
export interface RowIcon {
  iconId: string;
  colorId?: string;
}

/** The `omniDevWorktrees.rowColors` setting: {@link nodeId} → workbench colour id. */
export type RowColorMap = Record<string, string>;

/**
 * The colours a row can be tagged with, in picker order.
 *
 * `vscode.ThemeIcon` accepts only a `ThemeColor` — a colour *id*, never a hex value —
 * so this is a fixed vocabulary rather than a free-form colour. It is deliberately
 * extensible: adding an entry here plus the matching `enum`/`enumDescriptions` entry in
 * `package.json` is the whole change, and `icons.test.ts` fails if the two drift.
 *
 * `charts.*` is the family already in use for the built-in state colours and for the
 * badge layer's `COLOR_SEVERITY`; the `terminal.ansi*` ids are included because they are
 * the ones a user has usually already tuned to be legible in their theme.
 */
export const ROW_COLORS: readonly { id: string; label: string; group: string }[] = [
  { id: "charts.red", label: "Red", group: "Chart colours" },
  { id: "charts.orange", label: "Orange", group: "Chart colours" },
  { id: "charts.yellow", label: "Yellow", group: "Chart colours" },
  { id: "charts.green", label: "Green", group: "Chart colours" },
  { id: "charts.blue", label: "Blue", group: "Chart colours" },
  { id: "charts.purple", label: "Purple", group: "Chart colours" },
  { id: "charts.foreground", label: "Foreground", group: "Chart colours" },
  { id: "terminal.ansiRed", label: "Red", group: "Terminal colours" },
  { id: "terminal.ansiYellow", label: "Yellow", group: "Terminal colours" },
  { id: "terminal.ansiGreen", label: "Green", group: "Terminal colours" },
  { id: "terminal.ansiCyan", label: "Cyan", group: "Terminal colours" },
  { id: "terminal.ansiBlue", label: "Blue", group: "Terminal colours" },
  { id: "terminal.ansiMagenta", label: "Magenta", group: "Terminal colours" },
  { id: "terminal.ansiBrightRed", label: "Bright red", group: "Bright terminal colours" },
  { id: "terminal.ansiBrightYellow", label: "Bright yellow", group: "Bright terminal colours" },
  { id: "terminal.ansiBrightGreen", label: "Bright green", group: "Bright terminal colours" },
  { id: "terminal.ansiBrightCyan", label: "Bright cyan", group: "Bright terminal colours" },
  { id: "terminal.ansiBrightBlue", label: "Bright blue", group: "Bright terminal colours" },
  { id: "terminal.ansiBrightMagenta", label: "Bright magenta", group: "Bright terminal colours" },
  { id: "descriptionForeground", label: "Muted grey", group: "Other" },
];

/** {@link ROW_COLORS} as a lookup, for validating a hand-edited settings value. */
export const ROW_COLOR_IDS: ReadonlySet<string> = new Set(ROW_COLORS.map((c) => c.id));

/**
 * The colour tagged onto `node`, or `undefined` when it has none.
 *
 * This is the **single** validation point for the setting, which is why it takes
 * `unknown` rather than a typed map: `settings.json` is hand-editable and VS Code's
 * schema validation produces squiggles, not sanitisation, so a non-object, an array, or
 * `{ "wt:/x": 42 }` all reach us intact. An unrecognised colour id is rejected here too
 * — `new vscode.ThemeColor("nonsense")` does not throw, it renders the icon *uncoloured*,
 * so a typo would silently look like a bug. Anything rejected falls through to the row's
 * state colour, leaving it exactly as it renders today.
 *
 * The empty string is treated as absent, so hand-writing `""` is a valid spelling of
 * "no tag" — the same thing the picker's **Default (theme colour)** entry achieves by
 * deleting the key.
 */
export function rowColorTag(colors: unknown, node: Node): string | undefined {
  if (typeof colors !== "object" || colors === null || Array.isArray(colors)) {
    return undefined;
  }
  const map = colors as Record<string, unknown>;
  const key = nodeId(node);
  if (!Object.prototype.hasOwnProperty.call(map, key)) {
    return undefined;
  }
  const value = map[key];
  return typeof value === "string" && ROW_COLOR_IDS.has(value) ? value : undefined;
}

/**
 * A repository row's icon.
 *
 * The GitHub repo icon reflects PR-poll state (#1376): green when the daemon is polling
 * this repo *and* the global master is on, else the default gray. A non-GitHub repo
 * keeps the plain `repo` glyph. A `tag` overrides the colour of either — the glyph still
 * distinguishes GitHub from non-GitHub, and the "Enable/Disable PR Polling" menu items
 * still report poll state.
 */
export function repoRowIcon(repo: TreeRepoPayload, showPr: boolean, tag?: string): RowIcon {
  if (!repo.github) {
    return { iconId: "repo", colorId: tag };
  }
  const colorId = tag ?? (showPr && repoPollingEnabled(repo) ? "charts.green" : undefined);
  return { iconId: "github", colorId };
}

/**
 * A worktree row's icon, in precedence order.
 *
 * 1. **The rebase cue** (#1415). A rebase in flight — or a conflict left in place —
 *    takes the icon over entirely, colour included: it is transient and actionable,
 *    where both open-state and a user's tag are neither. The row does lose its open
 *    badge for the duration, an accepted trade, since the tooltip and `contextValue`
 *    still carry open state and the badge layer's two characters are already claimed by
 *    the PR-check and Claude-session providers.
 * 2. **The row's tag**, when the user has set one.
 * 3. **The open badge**, three-way: a blue tick for the worktree open in *this* window,
 *    a green dot for one open in another window, else the plain branch glyph for a
 *    worktree with no live window.
 *
 * Note that a tag replaces the colour but never the glyph, so tagging a row does not
 * cost you the open-state distinction.
 */
export function worktreeRowIcon(
  wt: TreeWorktreePayload,
  windowKey?: string,
  tag?: string,
): RowIcon {
  const rebase = worktreeRebaseCue(wt);
  if (rebase) {
    return { iconId: rebase.iconId, colorId: "charts.yellow" };
  }
  if (isCurrentWindow(wt, windowKey)) {
    return { iconId: "check", colorId: tag ?? "charts.blue" };
  }
  if (wt.open) {
    return { iconId: "circle-filled", colorId: tag ?? "charts.green" };
  }
  return { iconId: "git-branch", colorId: tag };
}

/**
 * Whether two colour maps are equivalent.
 *
 * The tree provider refreshes only on a real change, for the reason `sameTallies`
 * documents: firing `onDidChangeTreeData` re-runs `getChildren`, which re-triggers the
 * lazy ahead/behind and PR-badge fetches. `onDidChangeConfiguration` fires in **every**
 * open window, so without this one colour edit would cost N windows × one `ahead-behind`
 * op per expanded repo.
 */
export function sameRowColors(left: RowColorMap, right: RowColorMap): boolean {
  const keys = Object.keys(left);
  return (
    keys.length === Object.keys(right).length && keys.every((key) => left[key] === right[key])
  );
}
