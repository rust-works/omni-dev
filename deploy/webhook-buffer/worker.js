// SPIKE capture Worker for issue #1378 — NOT the production buffer.
//
// Purpose: give a repo you own a public HTTPS endpoint that verifies GitHub's
// webhook HMAC, stores each delivery to KV keyed by arrival time, and hands the
// buffered events back over an authenticated outbound pull — exactly the
// store-and-pull shape the daemon will later use, but pared down to what the
// feasibility spike needs (see ../../docs/plan/webhook-buffer-spike.md).
//
// The production buffer (the real #1378 deliverable) will refine cursor
// semantics, retention, consume/ack, and the read-token posture. This file is a
// throwaway capture harness — faithful enough to collect real payloads, small
// enough to read in one sitting.
//
// Security model, mirrored from the issue:
//   - The webhook HMAC secret lives ONLY here. The Worker verifies before it
//     stores, so the daemon never needs it. (POST /webhook)
//   - The daemon authenticates its outbound pull with a separate READ_TOKEN.
//     (GET /events)
// Both are `wrangler secret`s, never committed.

/** Seconds a stored event survives in KV. KV enforces a 60s floor. Kept short:
 *  the spike only needs a capture window, and the daemon's reconcile poll is the
 *  real catch-up path in production. Overridable via the RETENTION_SECONDS var. */
const DEFAULT_RETENTION_SECONDS = 259200; // 3 days

/** Zero-pad the arrival ms so KV's lexicographic key order is chronological. 15
 *  digits stays monotonic well past any realistic clock. */
const TS_WIDTH = 15;

/** Max events returned by one GET /events page. */
const DEFAULT_PAGE_LIMIT = 500;

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
        return json({ ok: true, service: "webhook-buffer-spike" });
      }
      return json({ error: "not found" }, 404);
    } catch (err) {
      // Never leak internals to an unauthenticated caller; log for `wrangler tail`.
      console.error("unhandled", err && err.stack ? err.stack : String(err));
      return json({ error: "internal error" }, 500);
    }
  },
};

// ---------------------------------------------------------------------------
// POST /webhook — verify HMAC, then store.
// ---------------------------------------------------------------------------

async function handleWebhook(request, env) {
  const secret = env.WEBHOOK_SECRET;
  if (!secret) {
    console.error("WEBHOOK_SECRET is not set");
    return json({ error: "buffer not configured" }, 500);
  }

  // Read the RAW bytes once: the HMAC is over the exact body GitHub signed, and
  // re-serialising parsed JSON would change the bytes and break verification.
  const rawBytes = new Uint8Array(await request.arrayBuffer());
  const signature = request.headers.get("x-hub-signature-256");
  if (!(await verifySignature(secret, rawBytes, signature))) {
    return json({ error: "signature mismatch" }, 401);
  }

  const eventType = request.headers.get("x-github-event") || "unknown";
  const delivery = request.headers.get("x-github-delivery") || crypto.randomUUID();

  // GitHub sends a one-off `ping` when the hook is created — store it too so the
  // operator can confirm end-to-end delivery, but it carries no CI signal.
  let payload;
  try {
    payload = JSON.parse(new TextDecoder().decode(rawBytes));
  } catch {
    payload = null; // keep the envelope; a non-JSON body is itself a finding
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
// GET /events?since=<key>&limit=<n> — authenticated outbound pull.
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

  // KV lists keys in lexicographic order, which our zero-padded timestamp makes
  // chronological. Take the first `limit` keys strictly after the cursor.
  const listed = await env.GITHUB_EVENTS.list({ prefix: "evt:", limit: 1000 });
  const fresh = listed.keys.map((k) => k.name).filter((name) => name > since);
  const page = fresh.slice(0, limit);

  const events = [];
  for (const name of page) {
    const value = await env.GITHUB_EVENTS.get(name);
    if (value !== null) events.push(JSON.parse(value));
  }

  // Advance the cursor to the last key returned; the caller passes it back as
  // `since` next time. An empty page leaves the cursor where it was.
  const cursor = page.length ? page[page.length - 1] : since;
  return json({ events, cursor, count: events.length, more: fresh.length > page.length });
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
  return Math.min(Math.floor(n), 1000);
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
