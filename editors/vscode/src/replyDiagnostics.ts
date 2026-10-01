import { Envelope, Reply } from "./socket";

/** Preserve fail-open replies while making every rejection visible in output. */
export async function sendWithDiagnostics(
  envelope: Envelope,
  request: () => Promise<Reply>,
  log: (message: string) => void,
  context?: string,
): Promise<Reply | undefined> {
  const label = `${envelope.service ?? "daemon"}/${envelope.op}${context ? ` (${context})` : ""}`;
  try {
    const reply = await request();
    if (!reply.ok) {
      log(`${label} rejected: ${reply.error ?? "daemon refused the request"}`);
    }
    return reply;
  } catch (err) {
    log(`${label} skipped: ${err instanceof Error ? err.message : String(err)}`);
    return undefined;
  }
}
