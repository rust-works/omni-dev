// Pure, `vscode`-free helpers for relocating a Claude Code session's on-disk
// storage from one worktree's project scope into another (#1295). All filesystem
// I/O lives in the `vscode`-facing `moveSessionCommand.ts`; this module holds the
// unit-tested logic — path encoding, the relocation planner, the quick-pick
// preview label, and the live-session guard — so it runs under `node --test`
// like `tree.ts` / `github.ts`.

import * as os from "os";
import * as path from "path";

/**
 * How recently (ms) a transcript must have been written to be treated as
 * possibly-live and refused. Best-effort: an idle-but-open session may not be
 * written for minutes, so this only catches an actively-streaming one — the
 * confirmation modal is the real safety net. See {@link isLikelyLive}.
 */
export const LIVE_THRESHOLD_MS = 10_000;

/** How many leading transcript lines to scan for a human-readable preview. */
export const PREVIEW_SCAN_LINES = 40;

/** Max characters of a preview label before it is truncated with an ellipsis. */
const PREVIEW_MAX_CHARS = 80;

/**
 * Encodes an absolute directory path the way Claude Code names its per-project
 * storage folder under `~/.claude/projects/`: every `/` **and** `.` becomes `-`
 * (verified on disk — `/Users/x/wrk/omni-dev` → `-Users-x-wrk-omni-dev`,
 * `/Users/x/Downloads/Dot.dot` → `-Users-x-Downloads-Dot-dot`, and a hidden
 * `/a/.work` → `-a--work`). Lossy and one-way; we only ever encode.
 */
export function encodeProjectPath(absPath: string): string {
  return absPath.replace(/[/.]/g, "-");
}

/** The base Claude config dir: `CLAUDE_CONFIG_DIR` if set, else `~/.claude`. */
export function claudeConfigDir(): string {
  const override = process.env.CLAUDE_CONFIG_DIR?.trim();
  return override ? override : path.join(os.homedir(), ".claude");
}

/** The `projects/` root holding every per-cwd session folder. */
export function claudeProjectsDir(): string {
  return path.join(claudeConfigDir(), "projects");
}

/** The encoded per-project session folder for an absolute worktree/cwd path. */
export function projectDirFor(absPath: string): string {
  return path.join(claudeProjectsDir(), encodeProjectPath(absPath));
}

/** Whether a relocation moves the artifacts or copies them (a fork). */
export type RelocationMode = "move" | "copy";

/** One filesystem operation in a relocation plan. */
export interface RelocationOp {
  /** Absolute source path. */
  from: string;
  /** Absolute destination path. */
  to: string;
  /** A single file (the `.jsonl`) or a directory (the sidecar). */
  kind: "file" | "dir";
}

/** The ordered artifact operations to relocate one session. */
export interface RelocationPlan {
  sessionId: string;
  mode: RelocationMode;
  ops: RelocationOp[];
}

/**
 * Builds the ordered filesystem operations to relocate one session from `srcDir`
 * to `destDir`. The transcript `<id>.jsonl` is always included; the `<id>/`
 * sidecar dir only when `hasSidecar` (it exists solely when the session spawned
 * subagents or overflowed tool results — ~1 in 4 on disk). Ordered
 * transcript-first, so a partial failure still leaves a moved, resumable
 * transcript rather than an orphaned sidecar.
 */
export function planRelocation(args: {
  sessionId: string;
  srcDir: string;
  destDir: string;
  hasSidecar: boolean;
  mode: RelocationMode;
}): RelocationPlan {
  const { sessionId, srcDir, destDir, hasSidecar, mode } = args;
  const ops: RelocationOp[] = [
    {
      from: path.join(srcDir, `${sessionId}.jsonl`),
      to: path.join(destDir, `${sessionId}.jsonl`),
      kind: "file",
    },
  ];
  if (hasSidecar) {
    ops.push({
      from: path.join(srcDir, sessionId),
      to: path.join(destDir, sessionId),
      kind: "dir",
    });
  }
  return { sessionId, mode, ops };
}

/**
 * Extracts a short, human-readable preview of a session from the head lines of
 * its transcript — the quick-pick label a user recognizes the conversation by.
 * Prefers the first genuine user message's text, then a `summary` line, then the
 * session id. Non-JSON, bookkeeping (`queue-operation`/`attachment`), meta, and
 * slash-command wrapper lines are skipped. `lines` is the raw head of the
 * `.jsonl` (see {@link PREVIEW_SCAN_LINES}).
 */
export function parseSessionPreview(lines: string[], sessionId: string): string {
  let summary: string | undefined;
  for (const line of lines) {
    const trimmed = line.trim();
    if (!trimmed) {
      continue;
    }
    let obj: unknown;
    try {
      obj = JSON.parse(trimmed);
    } catch {
      continue;
    }
    const record = obj as Record<string, unknown>;
    if (record.type === "summary" && typeof record.summary === "string" && summary === undefined) {
      summary = record.summary.trim() || undefined;
      continue;
    }
    if (record.type === "user") {
      const text = firstUserText(record);
      if (text) {
        return truncateLabel(text);
      }
    }
  }
  return summary ? truncateLabel(summary) : sessionId;
}

/**
 * The first human-readable text of a `type: "user"` record, or `undefined` when
 * it carries none a user would recognize — a meta record, a tool-result-only
 * message (no `text` part), or a slash-command wrapper (`<command-name>…`).
 */
function firstUserText(record: Record<string, unknown>): string | undefined {
  if (record.isMeta === true) {
    return undefined;
  }
  const message = record.message as Record<string, unknown> | undefined;
  const content = message?.content;
  let text: string | undefined;
  if (typeof content === "string") {
    text = content;
  } else if (Array.isArray(content)) {
    for (const part of content) {
      if (
        part &&
        typeof part === "object" &&
        (part as Record<string, unknown>).type === "text" &&
        typeof (part as Record<string, unknown>).text === "string"
      ) {
        text = (part as Record<string, unknown>).text as string;
        break;
      }
    }
  }
  const value = text?.trim();
  if (!value) {
    return undefined;
  }
  // Slash-command / local-command wrappers are machine scaffolding, not an
  // opener the user would recognize; skip to the next candidate.
  if (value.startsWith("<command-name>") || value.startsWith("<local-command")) {
    return undefined;
  }
  return value;
}

/** Collapses whitespace and truncates a preview to a single tidy line. */
function truncateLabel(text: string): string {
  const oneLine = text.replace(/\s+/g, " ").trim();
  return oneLine.length > PREVIEW_MAX_CHARS
    ? `${oneLine.slice(0, PREVIEW_MAX_CHARS - 1)}…`
    : oneLine;
}

/**
 * Whether a transcript last written at `mtimeMs` should be treated as
 * possibly-live (and refused) relative to `nowMs`. Best-effort — catches an
 * actively-streaming session, not an idle-but-open one. See
 * {@link LIVE_THRESHOLD_MS}.
 */
export function isLikelyLive(mtimeMs: number, nowMs: number): boolean {
  return nowMs - mtimeMs < LIVE_THRESHOLD_MS;
}

/** A compact relative-time description like `5s ago`, `3m ago`, `2h ago`, `5d ago`. */
export function relativeTime(mtimeMs: number, nowMs: number): string {
  const sec = Math.max(0, Math.floor((nowMs - mtimeMs) / 1000));
  if (sec < 60) {
    return `${sec}s ago`;
  }
  const min = Math.floor(sec / 60);
  if (min < 60) {
    return `${min}m ago`;
  }
  const hr = Math.floor(min / 60);
  if (hr < 24) {
    return `${hr}h ago`;
  }
  return `${Math.floor(hr / 24)}d ago`;
}
