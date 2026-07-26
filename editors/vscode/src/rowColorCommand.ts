// The `vscode`-facing "Set Colour…" / "Clear All Row Colours" commands (#1428): tag an
// individual repo or worktree row in the Worktrees tree with an icon colour. A thin
// adapter — the palette and the whole precedence rule live in the pure, unit-tested
// `icons.ts`, and this file only wires them onto the editor (the selection, the quick
// pick, the confirmation, the toasts).
//
// **Colours live in VS Code user settings, not in the daemon.** Unlike the show/hide
// closed toggle (#1301) and the per-repo PR-poll flag (#1376) — both of which the daemon
// holds and re-pushes on a snapshot — this is purely client-side presentation the tree
// snapshot has no other reason to carry. `workspace.onDidChangeConfiguration` fires in
// every open window when user-scope settings change, so settings buys the cross-window
// live sync that made those two daemon-backed, without a wire field or a persistence
// file, and adds hand-editability and Settings Sync for free.
//
// Like the other item commands, "Set Colour…" is a `view/item/context` command on a
// multi-select view (#1357), so VS Code invokes it as `(clicked, selected[])` — see
// `selectionTargets` for why the two arguments are resolved rather than concatenated.
// Repo rows are kept in the selection rather than filtered out: a repo row is exactly as
// taggable as a worktree row.

import * as vscode from "vscode";

import { ROW_COLORS, RowColorMap } from "./icons";
import { Node, nodeId, selectionTargets } from "./tree";

/** What the row-colour commands need from `extension.ts`, injected so this file stays thin. */
export interface RowColorDeps {
  /**
   * The **user-scope** map only. Deliberately not the merged `get()` view: writing that
   * back would bake any default or lower-scope value into the user's `settings.json`.
   */
  read: () => RowColorMap;
  /** Persists the map to user settings, clearing the key entirely when it is empty. */
  write: (colors: RowColorMap) => Thenable<void>;
}

/** The picker entry that removes a row's tag; `undefined` id means "delete the key". */
const DEFAULT_CHOICE = "Default (theme colour)";

interface ColorPick extends vscode.QuickPickItem {
  /** The chosen colour id, or `undefined` for the "Default (theme colour)" entry. */
  colorId?: string;
}

/**
 * The **Set Colour…** command: tags every selected row with one colour, or clears them.
 *
 * A colour attaches to a row, not to a glyph, so the tag survives the row changing state
 * — which is the whole point, since nearly every row in a large tree sits in the same
 * state and a state-keyed colour would differentiate nothing.
 */
export async function setRowColor(
  deps: RowColorDeps,
  clicked?: Node,
  selected?: Node[],
): Promise<void> {
  const targets = selectionTargets(clicked, selected);
  if (targets.length === 0) {
    return;
  }
  const current = deps.read();
  const keys = targets.map(nodeId);
  const pick = await pickColor(keys.length === 1 ? current[keys[0]] : undefined, keys.length);
  if (!pick) {
    return;
  }

  // Read-modify-write, so keys for rows outside this selection survive — including any
  // the user hand-wrote that this version does not recognise. Two windows writing
  // concurrently can still lose a key; user-initiated and rare enough to accept.
  const next = { ...current };
  for (const key of keys) {
    if (pick.colorId === undefined) {
      delete next[key];
    } else {
      next[key] = pick.colorId;
    }
  }
  await persist(deps, next, failureNoun(targets.length));
}

/**
 * The **Clear All Row Colours** command: drops every tag.
 *
 * The escape hatch for stale keys. Tags are keyed by path, so deleting a worktree leaves
 * its entry behind forever — and pruning automatically against the live tree would be
 * wrong, since a row is legitimately absent whenever the daemon is down, its repo is
 * open in no window, or the setting has synced to a machine with a different layout.
 */
export async function clearAllRowColors(deps: RowColorDeps): Promise<void> {
  const current = deps.read();
  const count = Object.keys(current).length;
  if (count === 0) {
    void vscode.window.showInformationMessage("omni-dev: no row colours are set.");
    return;
  }
  const confirm = await vscode.window.showWarningMessage(
    `Clear ${count === 1 ? "the row colour" : `all ${count} row colours`}?`,
    {
      modal: true,
      detail: "Every repository and worktree row returns to its default colour.",
    },
    "Clear",
  );
  if (confirm !== "Clear") {
    return;
  }
  await persist(deps, {}, "row colours");
}

/** Writes the map, reporting a failure rather than letting it reject into the void. */
async function persist(deps: RowColorDeps, colors: RowColorMap, noun: string): Promise<void> {
  try {
    await deps.write(colors);
  } catch (err) {
    const message = err instanceof Error ? err.message : String(err);
    void vscode.window.showErrorMessage(`omni-dev: could not save ${noun}: ${message}`);
  }
}

function failureNoun(count: number): string {
  return count === 1 ? "the row colour" : "the row colours";
}

/**
 * The colour quick pick, grouped by family with separators.
 *
 * `vscode.ThemeIcon` accepts only a `ThemeColor` — an id, never a hex — so this is a
 * fixed vocabulary. Each item carries its id as a payload field rather than being matched
 * back by label, since the families deliberately repeat labels ("Red" appears three
 * times); the raw id shows as the description to disambiguate.
 */
async function pickColor(
  currentId: string | undefined,
  count: number,
): Promise<ColorPick | undefined> {
  const items: ColorPick[] = [{ label: DEFAULT_CHOICE, description: "no colour override" }];
  if (currentId === undefined) {
    items[0].description = "no colour override (current)";
  }
  let group: string | undefined;
  for (const color of ROW_COLORS) {
    if (color.group !== group) {
      group = color.group;
      items.push({ label: group, kind: vscode.QuickPickItemKind.Separator });
    }
    items.push({
      label: color.label,
      description: color.id === currentId ? `${color.id} (current)` : color.id,
      colorId: color.id,
    });
  }
  return vscode.window.showQuickPick(items, {
    placeHolder:
      count === 1 ? "Pick a colour for this row" : `Pick a colour for ${count} selected rows`,
    matchOnDescription: true,
  });
}
