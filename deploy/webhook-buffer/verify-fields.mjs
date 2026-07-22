#!/usr/bin/env node
// SPIKE verifier for issue #1378 — see ../../docs/plan/webhook-buffer-spike.md
//
// Answers the spike's question mechanically: given captured GitHub webhook
// deliveries, does each event type carry the fields PrStatusCache needs, and can
// the rolled-up CI verdict be reconstructed from the per-check deltas?
//
// It reads envelopes ({ event, delivery, received, payload }) from one of:
//   --dir <path>    every *.json in a directory (the samples/ fixtures)
//   --file <path>   a single JSON file (array, one envelope, or a /events reply)
//   --url <base> --token <READ_TOKEN>   the live buffer's GET /events (paginated)
//
// and prints two things:
//   1. a field-presence matrix — which event types actually supplied each of the
//      seven needed fields, on THIS data;
//   2. the reconstructed rollup per (repo, branch, head_sha), so it can be eyeballed
//      against `gh pr checks` / the GraphQL poll.
//
// No dependencies (Node >= 18 built-ins only). Read-only.

import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

// --- CI state classification — kept in lockstep with src/pr_status.rs ----------
// (GraphQL enums are UPPERCASE; webhook conclusions/states are lowercase, so we
// upper-case before lookup exactly as pr_status.rs does.)
const FAILURE_STATES = new Set([
  "FAILURE", "ERROR", "CANCELLED", "TIMED_OUT",
  "ACTION_REQUIRED", "STARTUP_FAILURE", "STALE",
]);
const SUCCESS_STATES = new Set(["SUCCESS", "NEUTRAL", "SKIPPED"]);

/** Classify one check entry the way `check_entry_state` does: a not-COMPLETED
 *  status is Pending regardless of conclusion; else the conclusion/state decides;
 *  anything unrecognised is Pending (never a false pass). */
function entryState(status, verdict) {
  const s = (status || "").toUpperCase();
  if (s && s !== "COMPLETED") return "Pending";
  const v = (verdict || "").toUpperCase();
  if (FAILURE_STATES.has(v)) return "Failure";
  if (SUCCESS_STATES.has(v)) return "Success";
  return "Pending";
}

/** Reduce entry states to a badge verdict: failure dominates; else any pending;
 *  else success; no entries → None. Mirrors `rollup_check_state`. */
function rollup(entries) {
  if (entries.length === 0) return "None";
  if (entries.some((e) => e === "Failure")) return "Failure";
  if (entries.some((e) => e === "Pending")) return "Pending";
  return "Success";
}

// --- per-event extraction ------------------------------------------------------
// Each returns a normalised view of what the event carries. `undefined`/`null`
// means "absent", which is exactly what the presence matrix reports.

function repoOf(p) {
  const owner = p?.repository?.owner?.login;
  const name = p?.repository?.name;
  return owner && name ? { owner, name } : undefined;
}

function classifyUrl(url) {
  if (!url) return { kind: "absent" };
  if (/github\.com\/[^/]+\/[^/]+\/pull\//.test(url)) return { kind: "html", url };
  if (/\/repos\/[^/]+\/[^/]+\/pulls\//.test(url)) return { kind: "api", url };
  return { kind: "other", url };
}

function extract(env) {
  const p = env.payload || {};
  const repo = repoOf(p);
  const base = { event: env.event, repo, branch: undefined, sha: undefined, check: undefined, pr: undefined };

  switch (env.event) {
    case "check_run": {
      const cr = p.check_run || {};
      const pr = (cr.pull_requests || [])[0];
      return {
        ...base,
        branch: cr.check_suite?.head_branch ?? undefined,
        sha: cr.head_sha ?? undefined,
        check: { key: `run:${cr.name ?? cr.id}`, state: entryState(cr.status, cr.conclusion) },
        pr: pr ? { number: pr.number, draft: undefined, url: classifyUrl(pr.url) } : undefined,
      };
    }
    case "check_suite": {
      const cs = p.check_suite || {};
      const pr = (cs.pull_requests || [])[0];
      return {
        ...base,
        branch: cs.head_branch ?? undefined,
        sha: cs.head_sha ?? undefined,
        check: { key: `suite:${cs.app?.id ?? cs.id}`, state: entryState(cs.status, cs.conclusion) },
        pr: pr ? { number: pr.number, draft: undefined, url: classifyUrl(pr.url) } : undefined,
      };
    }
    case "status": {
      // `branches[]` lists only refs whose HEAD is this sha; take the first as the
      // best-effort branch, but record that association is head-only.
      const branch = (p.branches || [])[0]?.name;
      return {
        ...base,
        branch: branch ?? undefined,
        sha: p.sha ?? undefined,
        check: { key: `ctx:${p.context}`, state: entryState("", p.state) },
        pr: undefined, // status events carry no PR link
      };
    }
    case "pull_request": {
      const pr = p.pull_request || {};
      return {
        ...base,
        branch: pr.head?.ref ?? undefined,
        sha: pr.head?.sha ?? undefined,
        check: undefined, // no CI verdict on this event
        pr: {
          number: p.number ?? pr.number,
          draft: typeof pr.draft === "boolean" ? pr.draft : undefined,
          url: classifyUrl(pr.html_url),
        },
      };
    }
    default:
      return base; // ping / unrelated event: repo only
  }
}

// --- field-presence matrix -----------------------------------------------------

const FIELDS = [
  ["1 repo (owner/name)", (x) => (x.repo ? "yes" : "no")],
  ["2 branch (ref)",       (x) => (x.branch ? "yes" : "no")],
  ["3 head sha",           (x) => (x.sha ? "yes" : "no")],
  ["4 check state",        (x) => (x.check ? "yes" : "n/a")],
  ["5 PR number",          (x) => (x.pr ? (x.pr.number != null ? "yes" : "no") : "n/a")],
  ["6 PR isDraft",         (x) => (x.pr ? (x.pr.draft != null ? "yes" : "no") : "n/a")],
  ["7 PR url",             (x) => (x.pr ? x.pr.url.kind : "n/a")],
];

function buildMatrix(views) {
  const byType = new Map();
  for (const v of views) {
    if (!byType.has(v.event)) byType.set(v.event, []);
    byType.get(v.event).push(v);
  }
  return byType;
}

function pad(s, w) {
  s = String(s);
  return s.length >= w ? s : s + " ".repeat(w - s.length);
}

function printMatrix(byType) {
  const types = [...byType.keys()].sort();
  const w0 = 22;
  const w = 16;
  console.log("\n=== Field-presence matrix (present / seen, per event type) ===\n");
  console.log(pad("field", w0) + types.map((t) => pad(t, w)).join(""));
  console.log("-".repeat(w0 + w * types.length));
  for (const [label, fn] of FIELDS) {
    let row = pad(label, w0);
    for (const t of types) {
      const vs = byType.get(t);
      const counts = {};
      for (const v of vs) {
        const r = fn(v);
        counts[r] = (counts[r] || 0) + 1;
      }
      row += pad(summarize(counts, vs.length), w);
    }
    console.log(row);
  }
  console.log("\nlegend: yes=present  no=absent  n/a=not carried by this event");
  console.log("        PR url: html=browser url (needed)  api=REST url  other/absent");
}

function summarize(counts, total) {
  // Collapse to the dominant outcome for a compact cell, e.g. "yes 3/3", "api 2/2".
  const entries = Object.entries(counts).sort((a, b) => b[1] - a[1]);
  if (entries.length === 0) return "-";
  const [kind, n] = entries[0];
  const extra = entries.slice(1).map(([k, c]) => `${k} ${c}`).join(", ");
  const head = `${kind} ${n}/${total}`;
  return extra ? `${head} (+${extra})` : head;
}

// --- rollup reconstruction -----------------------------------------------------

function reconstructRollups(views) {
  // Key by repo + sha (the commit a badge is about). Branch(es) recorded alongside.
  const groups = new Map();
  for (const v of views) {
    if (!v.check || !v.sha || !v.repo) continue;
    const key = `${v.repo.owner}/${v.repo.name}@${v.sha}`;
    if (!groups.has(key)) {
      groups.set(key, { repo: v.repo, sha: v.sha, branches: new Set(), checks: new Map() });
    }
    const g = groups.get(key);
    if (v.branch) g.branches.add(v.branch);
    // Latest state per check identity wins (later delivery supersedes earlier).
    g.checks.set(v.check.key, v.check.state);
  }
  return groups;
}

function printRollups(groups) {
  console.log("\n=== Reconstructed rollup per (repo, branch, head_sha) ===\n");
  if (groups.size === 0) {
    console.log("(no check_run / check_suite / status events with a head_sha in this data)");
    return;
  }
  for (const g of groups.values()) {
    const states = [...g.checks.values()];
    const verdict = rollup(states);
    const branches = g.branches.size ? [...g.branches].join(", ") : "(branch not on CI events)";
    console.log(`${g.repo.owner}/${g.repo.name}  branch=${branches}`);
    console.log(`  sha=${g.sha}`);
    console.log(`  checks=${g.checks.size}  verdict=${verdict}`);
    const tally = {};
    for (const s of states) tally[s] = (tally[s] || 0) + 1;
    console.log(`  breakdown=${JSON.stringify(tally)}`);
  }
}

// --- loading -------------------------------------------------------------------

function asEnvelopes(parsed) {
  // Accept an array of envelopes, a single envelope, or a GET /events reply.
  if (Array.isArray(parsed)) return parsed;
  if (parsed && Array.isArray(parsed.events)) return parsed.events;
  if (parsed && parsed.event) return [parsed];
  return [];
}

function loadFromDir(dir) {
  const out = [];
  for (const name of readdirSync(dir)) {
    if (!name.endsWith(".json")) continue;
    const text = readFileSync(join(dir, name), "utf8");
    out.push(...asEnvelopes(JSON.parse(text)));
  }
  return out;
}

function loadFromFile(file) {
  const text = readFileSync(file, "utf8").trim();
  // Try whole-file JSON first; fall back to NDJSON.
  try {
    return asEnvelopes(JSON.parse(text));
  } catch {
    return text.split("\n").filter(Boolean).map((l) => JSON.parse(l));
  }
}

async function loadFromUrl(base, token, since, limit) {
  const out = [];
  let cursor = since || "";
  for (;;) {
    const u = new URL("/events", base);
    if (cursor) u.searchParams.set("since", cursor);
    if (limit) u.searchParams.set("limit", String(limit));
    const res = await fetch(u, { headers: { authorization: `Bearer ${token}` } });
    if (!res.ok) throw new Error(`GET /events -> ${res.status} ${await res.text()}`);
    const body = await res.json();
    const events = body.events || [];
    out.push(...events);
    if (!body.more || events.length === 0 || body.cursor === cursor) break;
    cursor = body.cursor;
  }
  return out;
}

// --- main ----------------------------------------------------------------------

function parseArgs(argv) {
  const args = { limit: undefined };
  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    if (a === "--dir") args.dir = argv[++i];
    else if (a === "--file") args.file = argv[++i];
    else if (a === "--url") args.url = argv[++i];
    else if (a === "--token") args.token = argv[++i];
    else if (a === "--since") args.since = argv[++i];
    else if (a === "--limit") args.limit = Number(argv[++i]);
    else if (a === "-h" || a === "--help") args.help = true;
    else throw new Error(`unknown arg: ${a}`);
  }
  return args;
}

const HELP = `verify-fields.mjs — spike field-presence + rollup checker (#1378)

Usage:
  node verify-fields.mjs --dir samples
  node verify-fields.mjs --file captured/events.json
  node verify-fields.mjs --url https://<name>.workers.dev --token "$READ_TOKEN" [--since <cursor>] [--limit N]
`;

async function main() {
  const args = parseArgs(process.argv);
  if (args.help) {
    console.log(HELP);
    return;
  }

  let envelopes;
  if (args.dir) envelopes = loadFromDir(args.dir);
  else if (args.file) envelopes = loadFromFile(args.file);
  else if (args.url) {
    if (!args.token) throw new Error("--url requires --token");
    envelopes = await loadFromUrl(args.url, args.token, args.since, args.limit);
  } else {
    console.log(HELP);
    throw new Error("one of --dir | --file | --url is required");
  }

  const nonPing = envelopes.filter((e) => e.event && e.event !== "ping");
  console.log(`Loaded ${envelopes.length} envelope(s); ${nonPing.length} relevant (excluding ping).`);
  const seen = {};
  for (const e of envelopes) seen[e.event] = (seen[e.event] || 0) + 1;
  console.log("By type:", JSON.stringify(seen));

  const views = nonPing.map(extract);
  printMatrix(buildMatrix(views));
  printRollups(reconstructRollups(views));

  console.log(
    "\nReminder: fixtures under samples/ are documented-schema mock-ups, NOT evidence.",
  );
  console.log("Confirm the spike against LIVE owned-repo captures via --url.\n");
}

main().catch((err) => {
  console.error("error:", err.message);
  process.exit(1);
});
