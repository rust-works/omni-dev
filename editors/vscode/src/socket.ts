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
 * The fields the extension sends on a `rebase` op — mirrors the daemon's
 * `RebaseRequest` (`src/daemon/services/worktrees.rs`).
 *
 * Like `merge-queue` this is a **single batched** op over `paths`, not a
 * client-side fan-out — which is exactly what buys the fetch-once-per-repository
 * contract: the daemon can only group the selection by repository if it sees the
 * whole selection at once.
 */
export interface RebasePayload {
  paths: string[];
  requester_key: string;
  check?: boolean;
  confirmed?: boolean;
  keep_conflicts?: boolean;
}

/**
 * One repository's one-shot fetch in a `rebase` reply (mirrors `FetchOutcome` in
 * Rust). One entry per repository is the visible proof of fetch-once-per-repo.
 */
export interface RebaseFetch {
  repo_root: string;
  onto: string;
  fetched: boolean;
  ok: boolean;
  detail?: string;
}

/**
 * One worktree's outcome in a `rebase` reply (mirrors `WorktreeOutcome` +
 * the flattened `RebaseResult` in Rust).
 *
 * `status` is the kebab-case discriminant: `would-rebase` / `up-to-date` /
 * `skipped` in phase 1, and `rebased` / `conflict` / `skipped` / `fetch-failed`
 * in phase 2. The other fields ride along per variant — `behind` on a
 * rebase-shaped one, `reason` on a skip, `detail` on a failure.
 */
export interface RebaseOutcome {
  path: string;
  branch?: string;
  onto: string;
  status: string;
  /** Commits behind the rebase target (on `would-rebase` / `rebased`). */
  behind?: number;
  /** Why a worktree was skipped, e.g. `dirty`, `main-working-tree`. */
  reason?: string;
  /** The git error, on `conflict` / `fetch-failed`. */
  detail?: string;
  /**
   * Set when a conflicting worktree was **left mid-rebase** to resolve in place
   * rather than aborted (#1415). Absent means aborted — including from a
   * pre-#1415 daemon, which only ever aborted.
   */
  left_in_place?: boolean;
}

/** A `rebase` reply. Both phases share one shape; only the statuses differ. */
export interface RebaseReply {
  fetches?: RebaseFetch[];
  worktrees?: RebaseOutcome[];
}

/**
 * Builds a `rebase` **phase-1** envelope (`check:true`): the daemon fetches each
 * repository's target ref once and classifies every selected worktree, rebasing
 * nothing. That classification is the "is this worth doing?" gate *and* the
 * source of the real behind-counts the confirmation modal shows.
 */
export function rebaseCheckEnvelope(paths: string[], requesterKey: string): Envelope {
  return {
    service: WORKTREES_SERVICE,
    op: "rebase",
    payload: { paths, requester_key: requesterKey, check: true },
  };
}

/**
 * Builds a `rebase` **phase-2** execute envelope (`confirmed:true`): the daemon
 * re-plans from scratch and rebases each still-pending worktree. One envelope for
 * the whole selection — a batch confirms once (ADR-0049 §1).
 *
 * `keep_conflicts` is always set from this surface: a conflicted worktree is left
 * mid-rebase so it can be resolved in place, which is the whole point of #1415.
 * The tree row then cues it until it is finished.
 */
export function rebaseEnvelope(paths: string[], requesterKey: string): Envelope {
  return {
    service: WORKTREES_SERVICE,
    op: "rebase",
    payload: {
      paths,
      requester_key: requesterKey,
      confirmed: true,
      keep_conflicts: true,
    },
  };
}

/**
 * The fields the extension sends on a `reposition` op — mirrors the daemon's
 * `RepositionRequest` (`src/daemon/services/worktrees.rs`).
 *
 * Keyed by **window key**, not worktree path, unlike `close`/`merge-queue`: the
 * subject is an OS window, and only the daemon's registry knows which window has a
 * worktree open. `target_keys` may include `reference_key` — a multi-selection
 * naturally contains the invoking window — which the daemon reports and skips.
 */
export interface RepositionPayload {
  reference_key: string;
  target_keys: string[];
  check?: boolean;
}

/**
 * One target's outcome in a `reposition` reply: a stable machine `outcome` slug
 * (`moved`, `unchanged`, `partial`, `no-window`, `not-found`, `ambiguous`,
 * `minimized`, `fullscreen`, `reference`, `failed`, …) plus a human `detail`, so a
 * summary can be written without the client knowing every slug.
 */
export interface RepositionResult {
  key: string;
  title?: string;
  outcome: string;
  detail: string;
}

/**
 * A `reposition` / `reposition-undo` reply.
 *
 * `trusted` is a **field, not an error**: a daemon lacking the macOS Accessibility
 * permission replies `trusted: false` with nothing attempted, so the UI can offer
 * the settings pane rather than pattern-matching an error string. `blocked` is set
 * when the reference window itself could not be resolved, in which case no target
 * was touched. `undoable` marks a reply whose moves the daemon recorded.
 */
export interface RepositionReply {
  trusted?: boolean;
  blocked?: { reason: string; detail: string };
  reference?: { key: string; title: string };
  results?: RepositionResult[];
  moved?: number;
  skipped?: number;
  undoable?: boolean;
}

/**
 * Builds a `reposition` envelope: move each target window onto the invoking
 * window's geometry. `check` makes it a dry run that resolves everything and writes
 * nothing.
 *
 * Not two-phase like `close`/`merge-queue` — nothing durable changes, so a
 * confirmation on a routine layout command would cost more than it protects.
 * Reversibility comes from {@link repositionUndoEnvelope} instead.
 */
export function repositionEnvelope(
  referenceKey: string,
  targetKeys: string[],
  check = false,
): Envelope {
  const payload: RepositionPayload = {
    reference_key: referenceKey,
    target_keys: targetKeys,
  };
  if (check) {
    payload.check = true;
  }
  return { service: WORKTREES_SERVICE, op: "reposition", payload };
}

/**
 * Builds a `reposition-undo` envelope: put the windows the last `reposition` moved
 * back where they were.
 *
 * Payload-free by design — the daemon holds the one-level undo record, so a client
 * cannot ask it to move windows to arbitrary geometry.
 */
export function repositionUndoEnvelope(): Envelope {
  return { service: WORKTREES_SERVICE, op: "reposition-undo" };
}

/**
 * The `reload` op payload (#1417) — mirrors `ReloadRequest` in
 * `src/daemon/services/worktrees.rs`.
 *
 * Keyed by **window**, like {@link RepositionPayload} and unlike
 * {@link ClosePayload}: a reload acts on a window, and one tree row is one
 * window, whereas a path can be open in several. There is no `requester_key` —
 * a window that wants to reload itself does so directly rather than waiting a
 * heartbeat for its own directive, so the daemon never needs to know who asked.
 */
export interface ReloadPayload {
  target_keys: string[];
}

/**
 * A `reload` reply. `signalled` is deliberately not "reloaded": the daemon marks
 * a directive that the target picks up on its next heartbeat, and a reload has
 * no completion the daemon can observe. `unknown` lists the keys it had no live
 * window for — a window that closed between this client listing its targets and
 * sending the op, which is routine rather than an error.
 */
export interface ReloadReply {
  requested?: number;
  signalled?: number;
  unknown?: string[];
}

/**
 * Builds a `reload` envelope: signal each target window to reload itself.
 *
 * Not two-phase like `close` — a reload creates, modifies and destroys nothing
 * (VS Code's hot exit preserves dirty editors), so it follows the `reposition`
 * precedent of firing and reporting rather than confirming first.
 */
export function reloadEnvelope(targetKeys: string[]): Envelope {
  const payload: ReloadPayload = { target_keys: targetKeys };
  return { service: WORKTREES_SERVICE, op: "reload", payload };
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
 * Builds a sessions `list` envelope — every Claude session the daemon currently
 * tracks, across every window and terminal (#1406). Read-only; the tree tallies
 * the reply onto its worktree rows.
 */
export function sessionsListEnvelope(): Envelope {
  return { service: SESSIONS_SERVICE, op: "list" };
}

/**
 * Builds a sessions `subscribe` envelope — opens the push subscription (#1414).
 * The daemon then streams the same `{ sessions: [...] }` body `list` serves,
 * pushing a fresh frame on every real change, so every window's cues flip
 * together instead of drifting up to a poll period apart. See
 * `SessionsSubscription`.
 *
 * A daemon predating the op replies `{ ok: false }` and keeps the connection
 * open, which is the caller's signal to fall back to polling `list`.
 */
export function sessionsSubscribeEnvelope(): Envelope {
  return { service: SESSIONS_SERVICE, op: "subscribe" };
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
