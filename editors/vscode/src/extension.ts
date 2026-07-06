// The worktrees companion: a thin per-window reporter. On activation it
// registers this window with the omni-dev daemon, heartbeats every ~10s, and
// unregisters on deactivation. When the daemon is not running, every call is a
// silent no-op. See docs/worktrees-service.md for the contract.

import { randomUUID } from "crypto";
import * as path from "path";
import * as vscode from "vscode";
import {
  Envelope,
  RegisterPayload,
  Reply,
  defaultSocketPath,
  heartbeatEnvelope,
  registerEnvelope,
  sendEnvelope,
  unregisterEnvelope,
} from "./socket";

const CONFIG_SECTION = "omniDevWorktrees";

/**
 * The stable per-window identity the daemon keys this window by, generated
 * once per `activate()` (a UUID) — deliberately not `vscode.env.sessionId`,
 * whose per-window uniqueness is unverified.
 */
let windowKey: string;
let heartbeatTimer: ReturnType<typeof setInterval> | undefined;
let output: vscode.OutputChannel | undefined;

function config(): vscode.WorkspaceConfiguration {
  return vscode.workspace.getConfiguration(CONFIG_SECTION);
}

function socketPath(): string {
  const override = config().get<string>("socketPath")?.trim();
  return override ? override : defaultSocketPath();
}

function heartbeatMs(): number {
  const seconds = config().get<number>("heartbeatSeconds") ?? 10;
  return Math.max(1, seconds) * 1000;
}

/** Snapshots this window's open folders for a `register`. */
function registerPayload(): RegisterPayload {
  const folders = (vscode.workspace.workspaceFolders ?? []).map((f) => f.uri.fsPath);
  const payload: RegisterPayload = { key: windowKey, folders, pid: process.pid };
  if (folders.length > 0) {
    // The daemon enriches the primary folder with live git state; `repo` is
    // just a friendly fallback label.
    payload.repo = path.basename(folders[0]);
  }
  if (vscode.workspace.name) {
    payload.title = vscode.workspace.name;
  }
  return payload;
}

/**
 * Sends one envelope, swallowing every failure — a missing daemon must be a
 * silent no-op, never a user-facing error. Returns the reply, or `undefined`
 * when the daemon was unreachable.
 */
async function send(envelope: Envelope): Promise<Reply | undefined> {
  try {
    return await sendEnvelope(socketPath(), envelope);
  } catch (err) {
    output?.appendLine(
      `${envelope.op} skipped: ${err instanceof Error ? err.message : String(err)}`,
    );
    return undefined;
  }
}

async function register(): Promise<void> {
  await send(registerEnvelope(registerPayload()));
}

async function heartbeat(): Promise<void> {
  const reply = await send(heartbeatEnvelope(windowKey));
  // The registry is in-memory, so a restarted daemon has forgotten this
  // window; `known: false` is our signal to re-register.
  if (reply?.ok && reply.payload?.known === false) {
    await register();
  }
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  windowKey = randomUUID();
  output = vscode.window.createOutputChannel("omni-dev Worktrees");
  context.subscriptions.push(output);

  await register();

  heartbeatTimer = setInterval(() => void heartbeat(), heartbeatMs());
  context.subscriptions.push({
    dispose: () => {
      if (heartbeatTimer) {
        clearInterval(heartbeatTimer);
        heartbeatTimer = undefined;
      }
    },
  });

  // Workspace folders can change without a reactivation; report the new set.
  context.subscriptions.push(
    vscode.workspace.onDidChangeWorkspaceFolders(() => void register()),
  );
}

export async function deactivate(): Promise<void> {
  if (heartbeatTimer) {
    clearInterval(heartbeatTimer);
    heartbeatTimer = undefined;
  }
  await send(unregisterEnvelope(windowKey));
}
