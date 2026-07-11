// The long-lived push-subscription clients for the worktrees daemon streams.
//
// Like `socket.ts` this module is `vscode`-free so it is unit-testable under
// `node --test` against a plain `net` server. Each subscription opens ONE
// persistent connection to the daemon control socket, sends a single subscribe
// line, and then only reads: the daemon pushes an initial snapshot followed by a
// fresh one on every real change (`src/daemon/server.rs` `run_stream`). Writing
// any further line is treated by the daemon as a cancel, so a client never
// writes again — it unsubscribes by closing the socket. A dropped/absent daemon
// triggers a silent exponential-backoff reconnect loop; nothing here ever throws
// at the caller, matching the reporter's "daemon down is a no-op" contract.
//
// There are two streams, each on its own connection and its own daemon
// change-notify so a toggle never wakes the tree stream (and vice versa):
//   - `TreeSubscription`      — the repo/worktree `tree` snapshots.
//   - `ViewStateSubscription` — the shared cross-window view preferences (#1293).
// Both share the generic `PushSubscription<T>` machinery below; only the
// subscribe envelope and the frame-parse differ.

import * as net from "net";

import {
  Envelope,
  Reply,
  ViewState,
  checkSocketPathLen,
  subscribeEnvelope,
  subscribeViewStateEnvelope,
} from "./socket";
import { TreeRepoPayload } from "./tree";

/** Injectable collaborators + backoff tuning (defaults wire real timers). */
export interface PushSubscriptionOptions<T> {
  /** Called with the parsed payload of every pushed snapshot. */
  onSnapshot: (value: T) => void;
  /** Called on connect↔disconnect transitions (drives the daemon-down hint). */
  onStatus?: (connected: boolean) => void;
  /** Called with a human-readable message on each recoverable drop. */
  onError?: (message: string) => void;
  /** First reconnect delay; doubles each failure up to `maxBackoffMs`. */
  initialBackoffMs?: number;
  /** Reconnect backoff ceiling. */
  maxBackoffMs?: number;
  /** Timer hooks, injected so tests drive reconnection deterministically. */
  setTimeoutFn?: (cb: () => void, ms: number) => ReturnType<typeof setTimeout>;
  clearTimeoutFn?: (handle: ReturnType<typeof setTimeout>) => void;
  /** Jitter source in `[0, 1)`; injected for deterministic tests. */
  random?: () => number;
}

/** Back-compat alias: the tree stream's caller-facing options. */
export type TreeSubscriptionOptions = PushSubscriptionOptions<TreeRepoPayload[]>;

/** The view-state stream's caller-facing options (`null` = daemon unseeded). */
export type ViewStateSubscriptionOptions = PushSubscriptionOptions<ViewState | null>;

const DEFAULT_INITIAL_BACKOFF_MS = 500;
const DEFAULT_MAX_BACKOFF_MS = 10_000;

/**
 * A resilient push subscription to one daemon stream. Construct a subclass, then
 * call {@link start}; call {@link close} to tear it down (idempotent, safe to
 * hand to `context.subscriptions`).
 *
 * Subclasses supply just the two things that differ per stream: which subscribe
 * envelope to write ({@link buildEnvelope}) and how to extract the payload of
 * interest from each frame ({@link parseFrame}). All the connection, NDJSON
 * framing, backoff/jitter reconnect, and connect↔disconnect status handling live
 * here, once.
 */
export abstract class PushSubscription<T> {
  protected readonly onSnapshot: (value: T) => void;
  private readonly onStatus?: (connected: boolean) => void;
  private readonly onError?: (message: string) => void;
  private readonly initialBackoffMs: number;
  private readonly maxBackoffMs: number;
  private readonly setTimeoutFn: (cb: () => void, ms: number) => ReturnType<typeof setTimeout>;
  private readonly clearTimeoutFn: (handle: ReturnType<typeof setTimeout>) => void;
  private readonly random: () => number;

  private conn?: net.Socket;
  private buf = "";
  private backoff: number;
  private reconnectTimer?: ReturnType<typeof setTimeout>;
  private closed = false;
  private connected = false;

  constructor(
    private readonly socketPath: string,
    options: PushSubscriptionOptions<T>,
  ) {
    this.onSnapshot = options.onSnapshot;
    this.onStatus = options.onStatus;
    this.onError = options.onError;
    this.initialBackoffMs = options.initialBackoffMs ?? DEFAULT_INITIAL_BACKOFF_MS;
    this.maxBackoffMs = options.maxBackoffMs ?? DEFAULT_MAX_BACKOFF_MS;
    this.setTimeoutFn = options.setTimeoutFn ?? ((cb, ms) => setTimeout(cb, ms));
    this.clearTimeoutFn = options.clearTimeoutFn ?? ((handle) => clearTimeout(handle));
    this.random = options.random ?? Math.random;
    this.backoff = this.initialBackoffMs;
  }

  /** The subscribe envelope written once on connect (the only line sent). */
  protected abstract buildEnvelope(): Envelope;

  /**
   * Extracts the payload of interest from a successful (`ok:true`) frame.
   * Returns `undefined` to ignore the frame (e.g. it is not this stream's
   * snapshot shape); any other value — `null` included — is delivered to
   * `onSnapshot` and counts as proof the daemon is up.
   */
  protected abstract parseFrame(payload: unknown): T | undefined;

  /** Opens the subscription and begins the reconnect loop. */
  start(): void {
    // A too-long socket path can never connect; fail permanently (logged) rather
    // than spin a doomed reconnect loop or throw into activation.
    try {
      checkSocketPathLen(this.socketPath);
    } catch (err) {
      this.closed = true;
      this.onError?.(err instanceof Error ? err.message : String(err));
      return;
    }
    this.connect();
  }

  /** Tears the subscription down: stops reconnects and drops the socket. */
  close(): void {
    this.closed = true;
    if (this.reconnectTimer !== undefined) {
      this.clearTimeoutFn(this.reconnectTimer);
      this.reconnectTimer = undefined;
    }
    if (this.conn) {
      this.conn.destroy();
      this.conn = undefined;
    }
  }

  private connect(): void {
    if (this.closed) {
      return;
    }
    const conn = net.createConnection(this.socketPath);
    this.conn = conn;
    this.buf = "";

    // `error` fires then `close`, so guard so exactly one drop is handled per
    // connection and later events on this (already-replaced) socket are ignored.
    let settled = false;
    const drop = (message: string) => {
      if (settled) {
        return;
      }
      settled = true;
      if (this.conn === conn) {
        this.conn = undefined;
      }
      conn.destroy();
      this.handleDrop(message);
    };

    conn.on("connect", () => {
      // The one and only write: request the stream. Any further write would be
      // read by the daemon as a cancel.
      conn.write(JSON.stringify(this.buildEnvelope()) + "\n");
    });
    conn.on("data", (chunk: Buffer) => this.onData(chunk));
    conn.on("error", (err: Error) => drop(err.message));
    conn.on("end", () => drop("daemon ended the stream"));
    conn.on("close", () => drop("connection closed"));
  }

  private onData(chunk: Buffer): void {
    this.buf += chunk.toString("utf8");
    let nl = this.buf.indexOf("\n");
    while (nl >= 0) {
      const line = this.buf.slice(0, nl);
      this.buf = this.buf.slice(nl + 1);
      if (line.trim().length > 0) {
        this.onLine(line);
      }
      nl = this.buf.indexOf("\n");
    }
  }

  private onLine(line: string): void {
    let reply: Reply;
    try {
      reply = JSON.parse(line) as Reply;
    } catch (err) {
      this.onError?.(`malformed snapshot: ${err instanceof Error ? err.message : String(err)}`);
      return;
    }
    if (!reply.ok) {
      // A stream only ever pushes `ok:true` frames; ignore anything else.
      return;
    }
    const value = this.parseFrame(reply.payload);
    if (value === undefined) {
      // Not this stream's snapshot shape — ignore without disturbing backoff.
      return;
    }
    // A parseable snapshot proves the daemon is up: reset backoff and, on the
    // first one, announce the connection.
    this.backoff = this.initialBackoffMs;
    if (!this.connected) {
      this.connected = true;
      this.onStatus?.(true);
    }
    this.onSnapshot(value);
  }

  private handleDrop(message: string): void {
    if (this.closed) {
      return;
    }
    if (this.connected) {
      this.connected = false;
      this.onStatus?.(false);
    }
    this.onError?.(message);
    this.scheduleReconnect();
  }

  private scheduleReconnect(): void {
    if (this.closed) {
      return;
    }
    // Full jitter on the high side: delay ∈ [backoff, 1.5·backoff).
    const delay = this.backoff + this.backoff * 0.5 * this.random();
    this.reconnectTimer = this.setTimeoutFn(() => {
      this.reconnectTimer = undefined;
      this.connect();
    }, delay);
    this.backoff = Math.min(this.backoff * 2, this.maxBackoffMs);
  }
}

/**
 * A resilient subscription to the daemon's worktrees `tree` stream. Construct
 * it, then call {@link PushSubscription.start}; call {@link PushSubscription.close}
 * to tear it down.
 */
export class TreeSubscription extends PushSubscription<TreeRepoPayload[]> {
  protected buildEnvelope(): Envelope {
    return subscribeEnvelope();
  }

  protected parseFrame(payload: unknown): TreeRepoPayload[] | undefined {
    // A well-formed `tree` snapshot is `{ repos: [...] }`; anything else (an
    // error reply, a stray frame) is ignored.
    const repos = (payload as { repos?: unknown } | undefined)?.repos;
    return Array.isArray(repos) ? (repos as TreeRepoPayload[]) : undefined;
  }
}

/**
 * A resilient subscription to the daemon's `subscribe-view-state` stream
 * (#1293): the shared cross-window view preferences. `onSnapshot(null)` signals
 * the daemon is unseeded (fresh or restarted) — the cue to re-seed it from the
 * companion's durable `globalState`.
 */
export class ViewStateSubscription extends PushSubscription<ViewState | null> {
  protected buildEnvelope(): Envelope {
    return subscribeViewStateEnvelope();
  }

  protected parseFrame(payload: unknown): ViewState | null | undefined {
    // Frame shape is `{ view_state: null | { show_closed: bool } }`. A missing
    // `view_state` key is not this stream's snapshot → ignore; an explicit
    // `null` is the valid "unseeded" signal → deliver as `null`.
    const p = payload as { view_state?: unknown } | undefined;
    if (!p || !("view_state" in p)) {
      return undefined;
    }
    const vs = p.view_state;
    if (vs === null) {
      return null;
    }
    return typeof (vs as { show_closed?: unknown })?.show_closed === "boolean"
      ? { show_closed: (vs as ViewState).show_closed }
      : undefined;
  }
}
