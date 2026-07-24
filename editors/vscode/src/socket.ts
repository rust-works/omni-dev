// Daemon control-socket client and wire protocol for the worktrees service.
//
// This module is deliberately free of any `vscode` import so it stays pure and
// unit-testable. It mirrors the daemon's own socket-path resolution
// (src/daemon/paths.rs) and the worktrees NDJSON contract
// (src/daemon/protocol.rs, src/daemon/services/worktrees.rs).

import * as net from "net";
import * as os from "os";
import * as path from "path";

/**
 * The daemon rejects a control-socket path whose byte length is `>=` this —
 * the portable `min(macOS 104, Linux 108)` `sockaddr_un` limit, matching
 * `MAX_SOCKET_PATH_LEN` in `src/daemon/paths.rs`.
 */
export const MAX_SOCKET_PATH_LEN = 104;

/** The service name the worktrees ops are routed to. */
export const WORKTREES_SERVICE = "worktrees";

/** The service name the Claude Code sessions ops are routed to (#1210). */
export const SESSIONS_SERVICE = "sessions";

/** A daemon request envelope — one newline-delimited JSON object on the wire. */
export interface Envelope {
  service: string;
  op: string;
  payload?: unknown;
}

/** A daemon reply envelope. */
export interface Reply {
  ok: boolean;
  // The success payload is op-specific; callers read known fields defensively.
  payload?: any;
  error?: string;
}

/**
 * The fields a window reports on `register` (the `RegisterRequest` DTO in
 * `src/worktrees.rs`). `key` is required and must be non-blank; the rest are
 * optional. `branch`/`ahead`/`behind` are daemon-computed and never reported.
 */
export interface RegisterPayload {
  key: string;
  folders: string[];
  repo?: string;
  title?: string;
  pid?: number;
}

/**
 * Recomputes the daemon's data directory the same way the Rust `dirs` crate
 * does, so the extension resolves the identical socket path:
 *  - macOS: `~/Library/Application Support` (`XDG_DATA_HOME` is ignored);
 *  - other unix: `$XDG_DATA_HOME` when set to an absolute path, else
 *    `~/.local/share`.
 */
export function defaultDataDir(
  env: NodeJS.ProcessEnv = process.env,
  platform: NodeJS.Platform = process.platform,
  home: string = os.homedir(),
): string {
  if (platform === "darwin") {
    return path.join(home, "Library", "Application Support");
  }
  const xdg = env.XDG_DATA_HOME;
  if (xdg && path.isAbsolute(xdg)) {
    return xdg;
  }
  return path.join(home, ".local", "share");
}

/** The default daemon control-socket path: `<data_dir>/omni-dev/daemon.sock`. */
export function defaultSocketPath(
  env: NodeJS.ProcessEnv = process.env,
  platform: NodeJS.Platform = process.platform,
  home: string = os.homedir(),
): string {
  return path.join(defaultDataDir(env, platform, home), "omni-dev", "daemon.sock");
}

/**
 * Throws when `socketPath` is too long for a `sockaddr_un`, matching the
 * daemon's own guard so the failure is actionable rather than an opaque OS
 * connect error.
 */
export function checkSocketPathLen(socketPath: string): void {
  const len = Buffer.byteLength(socketPath, "utf8");
  if (len >= MAX_SOCKET_PATH_LEN) {
    throw new Error(
      `socket path is ${len} bytes, exceeding the ${MAX_SOCKET_PATH_LEN}-byte limit: ${socketPath}`,
    );
  }
}

/** Builds a `register` envelope from a window snapshot. */
export function registerEnvelope(payload: RegisterPayload): Envelope {
  return { service: WORKTREES_SERVICE, op: "register", payload };
}

/** Builds a `heartbeat` envelope. */
export function heartbeatEnvelope(key: string): Envelope {
  return { service: WORKTREES_SERVICE, op: "heartbeat", payload: { key } };
}

/** Builds an `unregister` envelope. */
export function unregisterEnvelope(key: string): Envelope {
  return { service: WORKTREES_SERVICE, op: "unregister", payload: { key } };
}

/**
 * Builds a `tree` envelope — the one-shot repo/worktree snapshot request (used
 * by the manual "refresh" command; the live view uses `subscribe` instead).
 */
export function treeEnvelope(): Envelope {
  return { service: WORKTREES_SERVICE, op: "tree" };
}

/**
 * Builds a `subscribe` envelope — opens the push subscription. The daemon then
 * streams `tree` snapshots on the same connection until the client writes any
 * further line (a cancel) or closes the socket. See `TreeSubscription`.
 */
export function subscribeEnvelope(): Envelope {
  return { service: WORKTREES_SERVICE, op: "subscribe" };
}

/**
 * Builds an `ahead-behind` envelope — the lazy per-worktree divergence op (#1306).
 * The streamed `tree`/`subscribe` snapshot no longer carries ahead/behind (it was
 * the dominant per-worktree cost when computed for every worktree on every tick),
 * so the tree view requests it on demand — batched by path, one call per repo
 * expand. The reply payload is `{ results: { "<path>": { ahead, behind } } }`,
 * omitting any path that tracks no upstream.
 */
export function aheadBehindEnvelope(paths: string[]): Envelope {
  return { service: WORKTREES_SERVICE, op: "ahead-behind", payload: { paths } };
}

/**
 * Builds an `open-prs` envelope — fetches a repo's open pull requests from the
 * daemon's shared, TTL-cached `gh pr list` (#1389, fix 7). Serving "Open Pull
 * Request…" (and the transient badge fallback) from the daemon means N windows
 * dedupe to **one** counted `gh` per repo instead of each shelling its own. The
 * reply payload is `{ pull_requests: [...] }`; an older daemon without the op
 * comes back `{ ok: false }`, so the caller falls back to its own `gh`.
 */
export function openPrsEnvelope(owner: string, name: string): Envelope {
  return { service: WORKTREES_SERVICE, op: "open-prs", payload: { owner, name } };
}

/**
 * Builds an `open` envelope — focuses (or opens) a worktree folder in VS Code
 * via the daemon's launcher. The daemon guards `path` to an absolute, existing
 * directory, so a relative/nonexistent path comes back as `{ ok: false }`.
 */
export function openEnvelope(path: string): Envelope {
  return { service: WORKTREES_SERVICE, op: "open", payload: { path } };
}

/**
 * Builds a `set-show-closed` envelope — sets the daemon-backed show/hide-closed
 * toggle (#1301). The daemon holds this single cross-window value and re-pushes
 * a `tree` snapshot (carrying the new `show_closed`) to every subscribed window,
 * so the toggle syncs live everywhere instead of living in per-window
 * `globalState`.
 */
export function setShowClosedEnvelope(showClosed: boolean): Envelope {
  return {
    service: WORKTREES_SERVICE,
    op: "set-show-closed",
    payload: { show_closed: showClosed },
  };
}

/**
 * Builds a `set-polling` envelope — enables or disables the daemon's PR-badge
 * polling for one GitHub repo (#1376). Polling defaults **off**, so a repo only
 * issues `gh` once enabled; the daemon holds the (persisted) per-repo state and
 * re-pushes a `tree` snapshot carrying the new `polling_enabled` to every window,
 * so the icon recolours and badges drop/appear in sync — the `set-show-closed`
 * pattern. Keyed by `owner`/`name` so it covers every worktree of the repo.
 */
export function setPollingEnvelope(
  repo: { owner: string; name: string },
  enabled: boolean,
): Envelope {
  return {
    service: WORKTREES_SERVICE,
    op: "set-polling",
    payload: { owner: repo.owner, name: repo.name, enabled },
  };
}

/**
 * The fields the extension sends on a `close` op — mirrors the daemon's
 * `CloseRequest` (`src/daemon/services/worktrees.rs`). `remove` selects delete
 * (linked "Close Worktree") vs close-only (main "Close Window"); `requester_key`
 * is this window's key, so the daemon can tell a self-close from a cross-window
 * one; `confirmed` promotes the phase-1 safety check to the phase-2 execute.
 */
export interface ClosePayload {
  path: string;
  remove: boolean;
  requester_key: string;
  confirmed?: boolean;
}

/**
 * Builds a `close` **phase-1** safety-check envelope: `remove:true` with no
 * confirmation, so the daemon inspects the worktree and reports what a removal
 * would lose without touching anything.
 */
export function closeCheckEnvelope(path: string, requesterKey: string): Envelope {
  return {
    service: WORKTREES_SERVICE,
    op: "close",
    payload: { path, remove: true, requester_key: requesterKey },
  };
}

/**
 * Builds a `close` **execute** envelope. With `remove:true` it deletes the
 * (linked) worktree after closing its window; with `remove:false` it only
 * closes the window ("Close Window", never a delete). A `remove:true` execute
 * carries `confirmed:true` so the daemon proceeds past any risks.
 */
export function closeEnvelope(
  path: string,
  opts: { remove: boolean; requesterKey: string; confirmed?: boolean },
): Envelope {
  const payload: ClosePayload = {
    path,
    remove: opts.remove,
    requester_key: opts.requesterKey,
  };
  if (opts.confirmed) {
    payload.confirmed = true;
  }
  return { service: WORKTREES_SERVICE, op: "close", payload };
}

/**
 * The fields the extension sends on a `merge-queue` op — mirrors the daemon's
 * `MergeQueueRequest` (`src/daemon/services/worktrees.rs`). Unlike `close`, this
 * is a **single batched** op over `paths`: `check` reports eligibility only,
 * `confirmed` enqueues the eligible ones. `requester_key` is this window's key,
 * carried for parity with `close` (the daemon logs it).
 */
export interface MergeQueuePayload {
  paths: string[];
  requester_key: string;
  check?: boolean;
  confirmed?: boolean;
}

/**
 * Builds a `merge-queue` **phase-1** eligibility-check envelope (`check:true`):
 * the daemon evaluates every gate per path and reports which worktrees are
 * enqueue-eligible and which are skipped-with-reason, without touching anything.
 */
export function mergeQueueCheckEnvelope(paths: string[], requesterKey: string): Envelope {
  return {
    service: WORKTREES_SERVICE,
    op: "merge-queue",
    payload: { paths, requester_key: requesterKey, check: true },
  };
}

/**
 * Builds a `merge-queue` **phase-2** execute envelope (`confirmed:true`): the
 * daemon re-validates eligibility and enqueues each still-eligible PR. One
 * envelope for the whole selection — a batch confirms once (ADR-0049 §1).
 */
export function mergeQueueEnvelope(paths: string[], requesterKey: string): Envelope {
  return {
    service: WORKTREES_SERVICE,
    op: "merge-queue",
    payload: { paths, requester_key: requesterKey, confirmed: true },
  };
}

/**
 * The fields a window reports on the sessions `window` op (mirrors `WindowReport`
 * in `src/sessions.rs`) — how many Claude editor tabs / integrated terminals this
 * window has, plus its folders, so the daemon can tag a session's source as VS
 * Code by joining a session's `cwd` against these folders (#1210). The companion
 * cannot expose a tab's `session_id` (Claude Code's extension has no public API),
 * so it reports only counts, never per-tab ids.
 */
export interface SessionWindowPayload {
  key: string;
  folders: string[];
  tabs: number;
  terminals: number;
}

/** Builds a sessions `window` envelope — this window's Claude-embedding report. */
export function sessionWindowEnvelope(payload: SessionWindowPayload): Envelope {
  return { service: SESSIONS_SERVICE, op: "window", payload };
}

/** Builds a sessions `window-unregister` envelope (the window closed). */
export function sessionWindowUnregisterEnvelope(key: string): Envelope {
  return { service: SESSIONS_SERVICE, op: "window-unregister", payload: { key } };
}

/**
 * Sends one request envelope to the daemon and resolves with its reply.
 *
 * Opens a fresh connection, writes one `\n`-terminated JSON line, reads one
 * `\n`-terminated JSON line back, and closes. Rejects on connect failure
 * (daemon not running), timeout, or a malformed reply — callers treat any
 * rejection as "daemon unavailable" and no-op.
 */
export function sendEnvelope(
  socketPath: string,
  envelope: Envelope,
  timeoutMs = 2000,
): Promise<Reply> {
  return new Promise<Reply>((resolve, reject) => {
    // A too-long path would otherwise fail with an opaque OS error.
    checkSocketPathLen(socketPath);

    const conn = net.createConnection(socketPath);
    let buf = "";
    let settled = false;
    let timer: ReturnType<typeof setTimeout>;

    const finish = (fn: () => void) => {
      if (settled) {
        return;
      }
      settled = true;
      clearTimeout(timer);
      conn.destroy();
      fn();
    };

    timer = setTimeout(
      () => finish(() => reject(new Error("timed out waiting for daemon reply"))),
      timeoutMs,
    );

    conn.on("connect", () => {
      conn.write(JSON.stringify(envelope) + "\n");
    });
    conn.on("data", (chunk: Buffer) => {
      buf += chunk.toString("utf8");
      const nl = buf.indexOf("\n");
      if (nl < 0) {
        return;
      }
      const line = buf.slice(0, nl);
      finish(() => {
        try {
          resolve(JSON.parse(line) as Reply);
        } catch (err) {
          reject(err instanceof Error ? err : new Error(String(err)));
        }
      });
    });
    conn.on("error", (err) => finish(() => reject(err)));
    conn.on("end", () =>
      finish(() => reject(new Error("daemon closed the connection with no reply"))),
    );
  });
}
