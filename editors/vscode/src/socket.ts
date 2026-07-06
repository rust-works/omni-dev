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
