// The `vscode`-facing "Move Claude Session Here" command (#1295). It is a thin
// adapter: the path encoding, relocation planner, preview label, and live-guard
// all live in the `vscode`-free, unit-tested `claudeSessions.ts`; this file wires
// them onto the editor — enumerating the current window's sessions, the
// quick-pick, the confirmation modal, and the actual `fs` move/copy under a
// progress notification. Both the source and destination project folders live
// under `~/.claude/projects/`, so the relocation is always same-filesystem and
// `fs.rename` is atomic (no cross-device fallback needed).

import * as fs from "fs";
import * as fsp from "fs/promises";
import * as path from "path";
import * as vscode from "vscode";

import {
  PREVIEW_SCAN_LINES,
  RelocationMode,
  isLikelyLive,
  parseSessionPreview,
  planRelocation,
  projectDirFor,
  relativeTime,
} from "./claudeSessions";
import { Node, worktreeLabel } from "./tree";

/** How many bytes off the head of a transcript to read for its preview label. */
const PREVIEW_BYTES = 64 * 1024;

/** A session discovered in the source project folder. */
interface SessionInfo {
  /** The session id (the `.jsonl` basename, and the sidecar dir name). */
  id: string;
  /** Absolute path to the `<id>.jsonl` transcript. */
  jsonlPath: string;
  /** Last-modified time (ms) of the transcript — for ordering and the live guard. */
  mtimeMs: number;
  /** Whether an `<id>/` sidecar dir accompanies the transcript. */
  hasSidecar: boolean;
  /** A human-readable preview label for the quick-pick. */
  preview: string;
}

/**
 * Relocates a Claude Code session's on-disk storage from **this** window's
 * project folder into the target worktree node's project scope, so the
 * conversation becomes resumable from a window opened on that worktree. Lists
 * this window's sessions newest-first, confirms, then moves (or copies) the
 * `<id>.jsonl` and its optional `<id>/` sidecar. Global state (`history.jsonl`,
 * `shell-snapshots/`) is never touched. A missing daemon is irrelevant — this is
 * pure per-user filesystem manipulation under `~/.claude/projects/`.
 */
export async function moveClaudeSessionHere(
  node: Node | undefined,
  output?: vscode.OutputChannel,
): Promise<void> {
  if (!node || node.kind !== "worktree") {
    return;
  }
  const destWorktree = node.wt.path;

  const srcFolder = currentWorkspaceFolder();
  if (!srcFolder) {
    void vscode.window.showInformationMessage(
      "omni-dev: this window has no folder open, so there is no Claude session storage to move from.",
    );
    return;
  }

  const srcDir = projectDirFor(srcFolder);
  const destDir = projectDirFor(destWorktree);
  if (srcDir === destDir) {
    void vscode.window.showInformationMessage(
      "omni-dev: that worktree is this window's own folder — its sessions are already in that project scope.",
    );
    return;
  }

  let sessions: SessionInfo[];
  try {
    sessions = await enumerateSessions(srcDir);
  } catch (err) {
    fail(output, "could not read Claude session storage", err);
    return;
  }
  if (sessions.length === 0) {
    void vscode.window.showInformationMessage(
      `omni-dev: no Claude sessions found for ${path.basename(srcFolder)} to move.`,
    );
    return;
  }

  const picked = await pickSession(sessions, srcFolder);
  if (!picked) {
    return;
  }

  // Re-stat for a fresh mtime (and to catch a delete during the pick): refuse a
  // transcript written moments ago — its window may be live, and moving it out
  // from under a running session corrupts the resume.
  let freshMtime: number;
  try {
    freshMtime = (await fsp.stat(picked.jsonlPath)).mtimeMs;
  } catch {
    void vscode.window.showErrorMessage(
      "omni-dev: the selected session is no longer present. Refresh and try again.",
    );
    return;
  }
  if (isLikelyLive(freshMtime, Date.now())) {
    void vscode.window.showWarningMessage(
      "omni-dev: that session was written moments ago and may still be live. " +
        "Close its VS Code window first, then try again.",
    );
    return;
  }

  // Never clobber an existing session of the same id in the destination.
  const collision = await destinationCollision(picked, destDir);
  if (collision) {
    void vscode.window.showErrorMessage(
      `omni-dev: a session with this id already exists in the target worktree (${collision}). Aborting.`,
    );
    return;
  }

  const mode = await confirmRelocation(picked, srcFolder, destWorktree);
  if (!mode) {
    return;
  }

  const plan = planRelocation({
    sessionId: picked.id,
    srcDir,
    destDir,
    hasSidecar: picked.hasSidecar,
    mode,
  });

  await vscode.window.withProgress(
    {
      location: vscode.ProgressLocation.Notification,
      title: `${mode === "move" ? "Moving" : "Copying"} Claude session to “${worktreeLabel(node.wt)}”…`,
    },
    async () => {
      try {
        await fsp.mkdir(destDir, { recursive: true });
        for (const op of plan.ops) {
          if (mode === "move") {
            await fsp.rename(op.from, op.to);
          } else {
            await fsp.cp(op.from, op.to, { recursive: op.kind === "dir" });
          }
        }
      } catch (err) {
        fail(output, `could not ${mode} the session`, err);
        return;
      }
      output?.appendLine(
        `moveClaudeSessionHere: ${mode} ${picked.id} → ${destDir} (${plan.ops.length} artifact(s))`,
      );
      void vscode.window.showInformationMessage(
        `omni-dev: Claude session ${mode === "move" ? "moved" : "copied"} to ` +
          `“${worktreeLabel(node.wt)}”. Use /resume in a window opened there.`,
      );
    },
  );
}

/** The current window's primary workspace folder, or `undefined` when none is open. */
function currentWorkspaceFolder(): string | undefined {
  const folders = vscode.workspace.workspaceFolders;
  return folders && folders.length > 0 ? folders[0].uri.fsPath : undefined;
}

/**
 * Lists the sessions in a source project folder, newest first. A `.jsonl` file
 * is a session; a same-named directory is its sidecar. A missing folder (this
 * window never ran Claude here) yields an empty list, not an error.
 */
async function enumerateSessions(srcDir: string): Promise<SessionInfo[]> {
  let entries: fs.Dirent[];
  try {
    entries = await fsp.readdir(srcDir, { withFileTypes: true });
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === "ENOENT") {
      return [];
    }
    throw err;
  }
  const sidecarDirs = new Set(entries.filter((e) => e.isDirectory()).map((e) => e.name));
  const sessions: SessionInfo[] = [];
  for (const entry of entries) {
    if (!entry.isFile() || !entry.name.endsWith(".jsonl")) {
      continue;
    }
    const id = entry.name.slice(0, -".jsonl".length);
    const jsonlPath = path.join(srcDir, entry.name);
    let stat: fs.Stats;
    try {
      stat = await fsp.stat(jsonlPath);
    } catch {
      continue;
    }
    sessions.push({
      id,
      jsonlPath,
      mtimeMs: stat.mtimeMs,
      hasSidecar: sidecarDirs.has(id),
      preview: await readPreview(jsonlPath, id),
    });
  }
  sessions.sort((a, b) => b.mtimeMs - a.mtimeMs);
  return sessions;
}

/** Reads the head of a transcript and derives its preview label (id on failure). */
async function readPreview(jsonlPath: string, id: string): Promise<string> {
  try {
    const handle = await fsp.open(jsonlPath, "r");
    try {
      const chunk = Buffer.alloc(PREVIEW_BYTES);
      const { bytesRead } = await handle.read(chunk, 0, PREVIEW_BYTES, 0);
      const head = chunk
        .toString("utf8", 0, bytesRead)
        .split("\n")
        .slice(0, PREVIEW_SCAN_LINES);
      return parseSessionPreview(head, id);
    } finally {
      await handle.close();
    }
  } catch {
    return id;
  }
}

/** A single-select quick-pick over the sessions; returns the chosen one. */
async function pickSession(
  sessions: SessionInfo[],
  srcFolder: string,
): Promise<SessionInfo | undefined> {
  const now = Date.now();
  const items = sessions.map((s) => ({
    label: s.preview,
    description: relativeTime(s.mtimeMs, now),
    detail: s.hasSidecar ? `${s.id}  ·  + subagent/tool-result sidecar` : s.id,
    session: s,
  }));
  const pick = await vscode.window.showQuickPick(items, {
    placeHolder: `Move which Claude session from ${path.basename(srcFolder)}? (newest first)`,
    matchOnDescription: true,
    matchOnDetail: true,
  });
  return pick?.session;
}

/** The destination artifact that would be clobbered, or `undefined` when clear. */
async function destinationCollision(
  session: SessionInfo,
  destDir: string,
): Promise<string | undefined> {
  if (await exists(path.join(destDir, `${session.id}.jsonl`))) {
    return `${session.id}.jsonl`;
  }
  if (session.hasSidecar && (await exists(path.join(destDir, session.id)))) {
    return `${session.id}/`;
  }
  return undefined;
}

/** Whether a path exists (any type). */
async function exists(p: string): Promise<boolean> {
  try {
    await fsp.access(p);
    return true;
  } catch {
    return false;
  }
}

/**
 * The confirmation modal: shows source → destination and the exact artifacts,
 * and doubles as the move-vs-copy choice via its two buttons. Returns the chosen
 * {@link RelocationMode}, or `undefined` when dismissed.
 */
async function confirmRelocation(
  session: SessionInfo,
  srcFolder: string,
  destWorktree: string,
): Promise<RelocationMode | undefined> {
  const artifacts = session.hasSidecar
    ? `${session.id}.jsonl and its ${session.id}/ sidecar (subagents, tool-results)`
    : `${session.id}.jsonl`;
  const detail = [
    `From:  ${srcFolder}`,
    `To:    ${destWorktree}`,
    "",
    `Artifacts: ${artifacts}`,
    "",
    "Move relocates the session (removed from the source). " +
      "Copy leaves the original in place and forks a duplicate (same id).",
  ].join("\n");
  const choice = await vscode.window.showWarningMessage(
    `Move this Claude session into “${path.basename(destWorktree)}”?`,
    { modal: true, detail },
    "Move Session",
    "Copy (Fork)",
  );
  if (choice === "Move Session") {
    return "move";
  }
  if (choice === "Copy (Fork)") {
    return "copy";
  }
  return undefined;
}

/** Logs a failure to the output channel and surfaces it as an error toast. */
function fail(output: vscode.OutputChannel | undefined, what: string, err: unknown): void {
  const message = err instanceof Error ? err.message : String(err);
  output?.appendLine(`moveClaudeSessionHere: ${what}: ${message}`);
  void vscode.window.showErrorMessage(`omni-dev: ${what} — ${message}`);
}
