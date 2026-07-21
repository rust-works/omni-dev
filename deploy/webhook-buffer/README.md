# Webhook Buffer — production Worker (#1384)

The **non-polling (webhook-driven) PR status source**: a Cloudflare Worker + KV
buffer that receives GitHub webhook deliveries for a repo **you own** and serves
them back to the omni-dev daemon over an authenticated outbound pull. The daemon
reconstructs the same PR/CI badge the GraphQL poll would — in real time, at ~zero
GitHub API cost — living side-by-side with the default poller and selected by the
VS Code setting `omniDevWorktrees.prStatusSource`.

Design and the field-availability spike that proved this viable:
[`../../docs/plan/webhook-buffer-spike.md`](../../docs/plan/webhook-buffer-spike.md).
Secret posture and trust boundary: [ADR-0055](../../docs/adrs/adr-0055.md).

> **Owned repos only.** Installing a webhook needs repo **admin**. Repos without a
> webhook keep working under `webhook` mode via the daemon's reconcile poll (the
> GraphQL fallback), so nothing degrades — those repos just are not real-time.

## What's here

| File | Role |
|------|------|
| `worker.js` | The Worker: `POST /webhook` verifies the GitHub HMAC and stores to KV; `GET /events` serves buffered events over an authenticated (Bearer) pull; `GET /health`. |
| `wrangler.toml` | Worker + KV binding config. Fill the namespace `id`; secrets are set separately. |
| `verify-fields.mjs` | Reads captured events and prints the field-presence matrix + reconstructed rollup (no deps, Node ≥ 18) — the spike verifier, kept for diagnostics. |
| `samples/` | Documented-schema mock payloads for a zero-setup `verify-fields.mjs --dir samples` dry-run. **Not evidence.** |
| `package.json` | `wrangler` dev-dependency + `npm run` shortcuts. |

Nothing here compiles into the Rust crate — `deploy/` is outside the single crate
(like `editors/`), so `cargo build`/`publish` never touch it.

## Deploy

Requires a free [Cloudflare](https://dash.cloudflare.com/sign-up) account. From this
directory:

```sh
npm install                       # pulls wrangler locally
npx wrangler login                # opens a browser once

# Create the KV namespace, then paste the printed id into wrangler.toml → kv_namespaces.id
npx wrangler kv namespace create GITHUB_EVENTS

# Set the two secrets (never committed):
#  - WEBHOOK_SECRET: the GitHub webhook HMAC key (you give the same value to GitHub)
#  - READ_TOKEN:     the Bearer token the daemon presents on GET /events
openssl rand -hex 32 | npx wrangler secret put WEBHOOK_SECRET
openssl rand -hex 32 | npx wrangler secret put READ_TOKEN

npx wrangler deploy               # prints your https://<name>.<you>.workers.dev URL
```

Sanity check: `curl https://<name>.<you>.workers.dev/health` → `{"ok":true,...}`.
Keep the two secret values — `WEBHOOK_SECRET` for the hook, `READ_TOKEN` for the
daemon. `npx wrangler tail` streams live logs.

## Register the webhook on a repo you own

Use the daemon helper (preferred — it targets `POST /webhook` with the right
events and content type):

```sh
omni-dev daemon webhook register <owner>/<repo> \
  --url  https://<name>.<you>.workers.dev/webhook \
  --secret "<the WEBHOOK_SECRET you set above>"
# omni-dev daemon webhook list   <owner>/<repo>
# omni-dev daemon webhook remove <owner>/<repo> --id <hook-id>
```

Or by hand with `gh` (the events the daemon consumes are `check_run`,
`check_suite`, `status`, `pull_request`):

```sh
gh api -X POST repos/<owner>/<repo>/hooks \
  -f name=web -F active=true \
  -f 'events[]=check_run' -f 'events[]=check_suite' \
  -f 'events[]=status'    -f 'events[]=pull_request' \
  -f config[url]="https://<name>.<you>.workers.dev/webhook" \
  -f config[content_type]=json \
  -f config[secret]="<WEBHOOK_SECRET>"
```

GitHub immediately sends a `ping`; confirm it stored via `wrangler tail`.

## Point the daemon at the buffer

Set these where the daemon resolves config (process env or `settings.json` via
`Settings::get_env_var`), then flip the VS Code setting to `webhook`:

| Variable | Meaning |
|----------|---------|
| `OMNI_DEV_WEBHOOK_BUFFER_URL` | the Worker base URL (`https://<name>.<you>.workers.dev`) |
| `OMNI_DEV_WEBHOOK_READ_TOKEN` **or** `OMNI_DEV_WEBHOOK_READ_TOKEN_PATH` | the `READ_TOKEN` inline, or a file to read it from |
| `OMNI_DEV_DAEMON_WEBHOOK_PULL` | buffer-pull cadence, seconds (default 10) |
| `OMNI_DEV_DAEMON_WEBHOOK_RECONCILE` | reconcile-poll cadence, seconds (default 900) |

The daemon persists the resolved token to `0600 <data-dir>/omni-dev/webhook.token`
and pulls **outbound-only** — nothing local is exposed. The HMAC secret stays only
in the Worker.

## The `/events` contract

`GET /events?since=<cursor>&limit=<n>` with `Authorization: Bearer <READ_TOKEN>`
returns:

```json
{ "events": [ { "event": "...", "delivery": "...", "received": 1234, "payload": { } } ],
  "cursor": "evt:<ts>:<delivery>", "count": 1, "more": false }
```

Keys are `evt:<zero-padded-ms>:<delivery>` (chronological). Pass the returned
`cursor` back as `since` to resume; `more: true` means drain again. Events expire
from KV after `RETENTION_SECONDS` (default 3 days) — deliberately far below the
daemon's reconcile cadence, so a daemon offline beyond retention self-heals from
the reconcile poll rather than showing stale badges.

## Diagnostics

```sh
node verify-fields.mjs --url https://<name>.<you>.workers.dev --token "$READ_TOKEN"
node verify-fields.mjs --dir samples          # zero-setup dry-run of the analysis
```

Prints the field-presence matrix and the reconstructed rollup per
`(repo, branch, head_sha)`, using the same state sets as
[`../../src/pr_status.rs`](../../src/pr_status.rs).
