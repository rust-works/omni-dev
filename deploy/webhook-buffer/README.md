# Webhook Buffer — Spike Capture Harness (#1378)

> **This is a SPIKE**, not the production buffer. Its only job is to let you point a
> real GitHub webhook (on a repo **you own**) at a public endpoint, capture the
> deliveries, and confirm they carry the fields the daemon's PR badge needs. The
> production `#1378` buffer + daemon pull loop is a separate deliverable.
>
> Design and the question being answered: [`../../docs/plan/webhook-buffer-spike.md`](../../docs/plan/webhook-buffer-spike.md).

## What's here

| File | Role |
|------|------|
| `worker.js` | Cloudflare Worker: `POST /webhook` verifies the HMAC and stores to KV; `GET /events` serves buffered events over an authenticated pull. |
| `wrangler.toml` | Worker + KV config. Fill the namespace `id`; secrets are set separately. |
| `verify-fields.mjs` | Reads captured events and prints the field-presence matrix + reconstructed rollup. No dependencies (Node ≥ 18). |
| `samples/` | Documented-schema mock payloads for a zero-setup dry-run. **Not evidence** — replace with live captures. |
| `package.json` | `wrangler` dev-dependency + `npm run` shortcuts. |

Nothing here is compiled into the Rust crate — `deploy/` is outside the single crate
(like `editors/`), so `cargo build`/`publish` never touch it.

## Step 0 — Dry-run with zero setup (validate the tooling)

```sh
node verify-fields.mjs --dir samples
```

This confirms the verifier works and shows the shape of its output. The sample payloads
are hand-built from GitHub's documented schemas — they demonstrate the analysis, they do
**not** prove anything about live GitHub. The real confirmation is Step 4.

## Step 1 — Deploy the capture Worker (needs your Cloudflare account)

Requires a free [Cloudflare](https://dash.cloudflare.com/sign-up) account. From this
directory:

```sh
npm install                       # pulls wrangler locally
npx wrangler login                # opens a browser once

# Create the KV namespace, then paste the printed id into wrangler.toml -> kv_namespaces.id
npx wrangler kv namespace create GITHUB_EVENTS

# Set the two secrets (never committed):
#  - WEBHOOK_SECRET: a random string you'll also give GitHub (the HMAC key)
#  - READ_TOKEN:     a random string the pull side (verify-fields / the daemon) presents
openssl rand -hex 32 | npx wrangler secret put WEBHOOK_SECRET
openssl rand -hex 32 | npx wrangler secret put READ_TOKEN

npx wrangler deploy
```

`deploy` prints your endpoint, e.g. `https://omni-dev-webhook-buffer-spike.<you>.workers.dev`.
Sanity check: `curl https://…workers.dev/health` → `{"ok":true,...}`.

> Keep the two secret values — you need `WEBHOOK_SECRET` for Step 2 and `READ_TOKEN`
> for Step 4. `npx wrangler tail` streams live logs while you test.

## Step 2 — Register the webhook on a repo you own (needs repo admin)

Installing a webhook requires admin on the repo — which is why this path is **owned
repos only**; everything else keeps the GraphQL poll. Use the **same** `WEBHOOK_SECRET`:

```sh
SECRET="<the WEBHOOK_SECRET you set above>"
URL="https://<name>.<you>.workers.dev/webhook"

gh api -X POST repos/<owner>/<repo>/hooks \
  -f name=web \
  -F active=true \
  -f 'events[]=check_run' \
  -f 'events[]=check_suite' \
  -f 'events[]=status' \
  -f 'events[]=pull_request' \
  -f config[url]="$URL" \
  -f config[content_type]=json \
  -f config[secret]="$SECRET"
```

GitHub immediately sends a `ping`; confirm it stored via `wrangler tail` or Step 4.
(To clean up later: `gh api repos/<owner>/<repo>/hooks` to list, then
`gh api -X DELETE repos/<owner>/<repo>/hooks/<id>`.)

## Step 3 — Trigger real CI

Do the things a badge reacts to, so every event type lands:

- **push a branch / open a PR** → `pull_request`
- **CI runs** (GitHub Actions or any Checks app) → `check_run`, `check_suite`
- **a legacy commit status** (e.g. a deploy-preview bot) → `status`
- **re-run checks** to capture the pending → completed transitions

## Step 4 — Pull and verify (the actual confirmation)

```sh
node verify-fields.mjs --url https://<name>.<you>.workers.dev --token "$READ_TOKEN"
```

Reads the buffered events and prints:

1. **Field-presence matrix** — for each of the seven needed fields, which event types
   supplied it *on your real payloads* (and, for the PR url, whether it was the browser
   `html` url or the REST `api` url).
2. **Reconstructed rollup** per `(repo, branch, head_sha)` — the Success/Failure/Pending/
   None verdict rebuilt from the per-check deltas, using the same state sets as
   [`src/pr_status.rs`](../../src/pr_status.rs). Eyeball it against `gh pr checks <n>`.

Paste the summary into the **Results** section of the spike doc and fill the
**Confirmed?** column of its hypothesis table. Then apply the doc's decision criteria.

`--since <cursor>` resumes from a prior pull's printed cursor; `--limit N` caps a page.
To snapshot raw evidence locally (git-ignored):

```sh
curl -s -H "authorization: Bearer $READ_TOKEN" \
  "https://<name>.<you>.workers.dev/events" > captured/events.json
node verify-fields.mjs --file captured/events.json
```

## Teardown

- Delete the GitHub webhook (Step 2 cleanup note).
- `npx wrangler delete` removes the Worker; `npx wrangler kv namespace delete --namespace-id <id>` removes the store.

## Security notes (spike-grade)

- The **HMAC secret lives only in the Worker** — it verifies `X-Hub-Signature-256`
  before storing, so the pull side (and later the daemon) never needs it.
- The pull is gated by `READ_TOKEN` (Bearer header or `?token=`), constant-time compared.
- For a repo whose event data (branch names, SHAs, PR titles) is sensitive, remember the
  payloads sit in **your** Cloudflare KV for the retention window (`RETENTION_SECONDS`,
  default 3 days) — it's self-owned, but it is off-machine. Tear down when done.
