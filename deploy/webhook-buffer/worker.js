// Production webhook-buffer Worker for issue #1384 — the store-and-pull design
// proposed in #1378 and proven by its spike (docs/plan/webhook-buffer-spike.md).
//
//   GitHub  ──POST /webhook──▶  this Worker  ──put──▶  KV (TTL-bounded)
//   daemon  ──GET  /events ──▶  this Worker  ──list/get──▶  buffered deliveries
//
// The daemon's `webhook` PR source pulls buffered `check_run`/`check_suite`/
// `status`/`pull_request` deliveries over an authenticated **outbound** connection
// and reconstructs the same PR badge the GraphQL poll would (src/pr_status/webhook.rs).
//
// Security model (see ADR-0055):
//   - The webhook HMAC secret (`WEBHOOK_SECRET`) lives ONLY here. The Worker
//     verifies GitHub's signature before it stores, so the daemon never holds it.
//   - The daemon authenticates its pull with a separate `READ_TOKEN` (Bearer),
//     constant-time compared. Both are `wrangler secret`s, never committed.
//
// This is the hardened successor to the spike's capture harness: the `/events`
// reader pages through the KV keyspace via its list cursor (rather than a fixed
// first-1000-keys window), so a backlog larger than one KV page can never hide the
// newest events behind the oldest.

/** Seconds a stored event survives in KV. KV enforces a 60s floor. Kept well below
 *  a week: the daemon's reconcile poll is the catch-up path for anything older, so
 *  the buffer only needs to cover the real-time window. Override with `RETENTION_SECONDS`. */
const DEFAULT_RETENTION_SECONDS = 259200; // 3 days

/** Zero-pad arrival ms so KV's lexicographic key order is chronological. 15 digits
 *  stays monotonic well past any realistic clock. */
const TS_WIDTH = 15;

/** Default / max events returned by one `GET /events` page. */
const DEFAULT_PAGE_LIMIT = 500;
const MAX_PAGE_LIMIT = 1000;

/** Keys fetched per KV `list` call (KV's own page size). */
const KV_LIST_LIMIT = 1000;

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    try {
      if (request.method === "POST" && url.pathname === "/webhook") {
        return await handleWebhook(request, env);
      }
      if (request.method === "GET" && url.pathname === "/events") {
        return await handleEvents(request, env, url);
      }
      if (request.method === "GET" && url.pathname === "/health") {
        return json({ ok: true, service: "webhook-buffer" });
      }
      return json({ error: "not found" }, 404);
    } catch (err) {
      // Never leak internals to a caller; log for `wrangler tail`.
      console.error("unhandled", err && err.stack ? err.stack : String(err));
      return json({ error: "internal error" }, 500);
    }
  },
};

// ---------------------------------------------------------------------------
// POST /webhook — verify GitHub's HMAC over the raw body, then store.
// ---------------------------------------------------------------------------

async function handleWebhook(request, env) {
  const secret = env.WEBHOOK_SECRET;
  if (!secret) {
    console.error("WEBHOOK_SECRET is not set");
    return json({ error: "buffer not configured" }, 500);
  }

  // The HMAC is over the exact bytes GitHub signed; re-serialising parsed JSON
  // would change them and break verification. Read the raw bytes once.
  const rawBytes = new Uint8Array(await request.arrayBuffer());
  const signature = request.headers.get("x-hub-signature-256");
  if (!(await verifySignature(secret, rawBytes, signature))) {
    return json({ error: "signature mismatch" }, 401);
  }

  const eventType = request.headers.get("x-github-event") || "unknown";
  const delivery = request.headers.get("x-github-delivery") || crypto.randomUUID();

  // A non-JSON body is itself a finding: keep the envelope with a null payload.
  let payload;
  try {
    payload = JSON.parse(new TextDecoder().decode(rawBytes));
  } catch {
    payload = null;
  }

  const receivedMs = Date.now();
  const key = `evt:${String(receivedMs).padStart(TS_WIDTH, "0")}:${delivery}`;
  const envelope = { event: eventType, delivery, received: receivedMs, payload };

  await env.GITHUB_EVENTS.put(key, JSON.stringify(envelope), {
    expirationTtl: retentionSeconds(env),
    // Surface the event type in `wrangler kv key list` without a value fetch.
    metadata: { event: eventType, received: receivedMs },
  });

  return json({ stored: true, key, event: eventType });
}

// ---------------------------------------------------------------------------
// GET /events?since=<cursor>&limit=<n> — authenticated outbound pull.
// ---------------------------------------------------------------------------

async function handleEvents(request, env, url) {
  const token = env.READ_TOKEN;
  if (!token) {
    console.error("READ_TOKEN is not set");
    return json({ error: "buffer not configured" }, 500);
  }
  if (!authorized(request, url, token)) {
    return json({ error: "unauthorized" }, 401);
  }

  const since = url.searchParams.get("since") || "";
  const limit = clampLimit(url.searchParams.get("limit"));

  // Page through the whole `evt:` keyspace (lexicographic == chronological), via
  // KV's own list cursor, collecting keys strictly after `since` up to `limit`.
  // Unlike a single fixed-size list, this returns the newest events even when the
  // retained backlog exceeds one KV page.
  const page = [];
  let listCursor;
  let more = false;
  for (;;) {
    const opts = { prefix: "evt:", limit: KV_LIST_LIMIT };
    if (listCursor) opts.cursor = listCursor;
    const listed = await env.GITHUB_EVENTS.list(opts);
    for (const k of listed.keys) {
      if (k.name <= since) continue; // already delivered to this consumer.
      if (page.length >= limit) {
        more = true;
        break;
      }
      page.push(k.name);
    }
    if (more || listed.list_complete) break;
    listCursor = listed.cursor;
  }

  const events = [];
  for (const name of page) {
    const value = await env.GITHUB_EVENTS.get(name);
    if (value !== null) events.push(JSON.parse(value));
  }

  // Advance the cursor to the last key returned; the caller passes it back as
  // `since` next time. An empty page leaves the cursor where it was.
  const cursor = page.length ? page[page.length - 1] : since;
  return json({ events, cursor, count: events.length, more });
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

function retentionSeconds(env) {
  const raw = Number(env.RETENTION_SECONDS);
  if (Number.isFinite(raw) && raw >= 60) return Math.floor(raw);
  return DEFAULT_RETENTION_SECONDS;
}

function clampLimit(raw) {
  const n = Number(raw);
  if (!Number.isFinite(n) || n <= 0) return DEFAULT_PAGE_LIMIT;
  return Math.min(Math.floor(n), MAX_PAGE_LIMIT);
}

/** Bearer header or `?token=` query param, constant-time compared. */
function authorized(request, url, token) {
  const header = request.headers.get("authorization") || "";
  const bearer = header.startsWith("Bearer ") ? header.slice(7) : "";
  const presented = bearer || url.searchParams.get("token") || "";
  return timingSafeEqual(presented, token);
}

/** Verify GitHub's `X-Hub-Signature-256: sha256=<hex>` over the raw body. */
async function verifySignature(secret, bodyBytes, signatureHeader) {
  if (!signatureHeader || !signatureHeader.startsWith("sha256=")) return false;
  const expectedHex = signatureHeader.slice("sha256=".length);
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const sigBuf = await crypto.subtle.sign("HMAC", key, bodyBytes);
  return timingSafeEqual(toHex(sigBuf), expectedHex);
}

function toHex(arrayBuffer) {
  const bytes = new Uint8Array(arrayBuffer);
  let out = "";
  for (const b of bytes) out += b.toString(16).padStart(2, "0");
  return out;
}

/** Length-independent constant-time string compare (no early return on length). */
function timingSafeEqual(a, b) {
  const enc = new TextEncoder();
  const ba = enc.encode(a);
  const bb = enc.encode(b);
  let diff = ba.length ^ bb.length;
  const len = Math.max(ba.length, bb.length);
  for (let i = 0; i < len; i++) {
    diff |= (ba[i] || 0) ^ (bb[i] || 0);
  }
  return diff === 0;
}

function json(body, status = 200) {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json; charset=utf-8" },
  });
}
