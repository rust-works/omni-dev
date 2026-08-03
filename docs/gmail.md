# Gmail Integration

omni-dev exposes read access (and, opt-in, label mutation) to the Gmail v1
API through the `omni-dev gmail` command tree, with a matching `gmail_*` MCP
tool for every read-only subcommand. Authentication and output formats are
identical across both surfaces; the MCP tools simply return YAML matching the
CLI's `-o yaml` output. For the MCP-tool reference (parameters only), see
[docs/mcp.md](mcp.md#gmail-5-tools).

## Table of Contents

1. [Prerequisites](#prerequisites)
2. [Authentication](#authentication)
3. [Output formats](#output-formats)
4. [Search](#search)
5. [Messages](#messages)
6. [Threads](#threads)
7. [Labels](#labels)
8. [Sync](#sync)
9. [Rate limits and retry behaviour](#rate-limits-and-retry-behaviour)
10. [Troubleshooting](#troubleshooting)
11. [See also](#see-also)

## Prerequisites

Gmail read scopes are Google **restricted scopes** — an application
distributed to third parties that requests them must pass a Google CASA
security assessment with annual recertification. omni-dev doesn't carry that
burden, so **each user creates their own Google Cloud OAuth2 client**:

1. Create (or reuse) a project in the [Google Cloud console].
2. Enable the **Gmail API** for that project.
3. Create an OAuth2 client of type **Desktop app** (not "Web application" —
   the loopback-redirect flow below requires it).
4. Note the client's **Client ID** and **Client secret**.

**Prominent callout:** a freshly created OAuth2 client's consent screen
defaults to **Testing** publishing status. In that status, Google expires
issued refresh tokens after **7 days**, so `omni-dev gmail auth login` will
need to be re-run weekly until you push the project to **In production**
(no Google verification review is required below 100 test users for a
self-scoped read/label-modify request). See
[Troubleshooting](#invalid_grant) for the error this produces.

[Google Cloud console]: https://console.cloud.google.com/

## Authentication

### Environment variables

| Variable               | Purpose                                                        | Default |
|-------------------------|-----------------------------------------------------------------|---------|
| `GMAIL_CLIENT_ID`       | OAuth2 client id from your own Google Cloud project (required). | _none_  |
| `GMAIL_CLIENT_SECRET`   | OAuth2 client secret for the same client (required).            | _none_  |
| `GMAIL_REFRESH_TOKEN`   | Written by `gmail auth login`; not meant to be hand-set.        | _none_  |
| `GMAIL_SCOPE`           | Written by `gmail auth login`; records the granted scope (`gmail.readonly` or `gmail.modify`) so `auth status` can report it without a network call. | _none_ |
| `GMAIL_API_URL`         | Explicit API base URL; overrides the real `gmail.googleapis.com` host entirely. Use for a proxy or a forced egress gateway. | _unset_ |

`GMAIL_CLIENT_ID`/`GMAIL_CLIENT_SECRET` must be set (in your shell profile,
or in `~/.omni-dev/settings.json`'s `env` map) before running
`gmail auth login` — unlike the refresh token, login does not prompt for
them interactively.

### Interactive setup

```bash
$ export GMAIL_CLIENT_ID=...            # from your Google Cloud OAuth2 client
$ export GMAIL_CLIENT_SECRET=...
$ omni-dev gmail auth login

Credentials saved to ~/.omni-dev/settings.json
  Granted scope: https://www.googleapis.com/auth/gmail.readonly

Run `omni-dev gmail auth status` to verify.
```

This opens a browser to Google's consent screen via a loopback OAuth2
authorization-code + PKCE flow (see [ADR-0063](adrs/adr-0063.md)); once you
approve, the refresh token is written to `~/.omni-dev/settings.json`. Pass
`--modify` to additionally request the `gmail.modify` scope, needed for
`gmail label add`/`remove`:

```bash
$ omni-dev gmail auth login --modify
```

### Verifying credentials

```bash
$ omni-dev gmail auth status
Checking Gmail authentication...
Authenticated as: user@example.com
Messages in mailbox: 5842
Granted scope: gmail.readonly
```

This calls `users.getProfile`, a live network call. The matching MCP tool,
`gmail_auth_status`, returns boolean presence flags and the granted scope
only — it never calls the Gmail API, so it can't confirm the refresh token
is still accepted.

### Removing credentials

```bash
$ omni-dev gmail auth logout
Gmail credentials removed from ~/.omni-dev/settings.json
```

Idempotent: if no credentials are configured, it prints
`No Gmail credentials were configured.` and exits successfully.

## Output formats

Every leaf subcommand accepts `-o <format>` (`table` / `json` / `yaml` /
`yamls` / `jsonl`, default `table`) — the same convention as every other
`omni-dev` domain (see [ADR-0046](adrs/adr-0046.md)). `--out-file` exists
only on `gmail read`, the one command with a naturally file-shaped payload
(a message body/attachment source worth writing to disk); no other Gmail
leaf has a use for it.

## Search

```bash
$ omni-dev gmail search --query 'label:finance after:2026/01/01' --limit 50
$ omni-dev gmail search --query 'label:finance' --limit 50 --enrich --concurrency 4
```

`--query` uses [Gmail's own search syntax] (the same operators as the Gmail
search box: `from:`, `label:`, `after:`, `has:attachment`, etc.) — omni-dev
does not reinterpret it. `--limit 0` fetches every match up to a 10,000
hard cap, auto-paginating underneath.

By default `search` returns only `id`/`threadId` per hit — `messages.list`
itself never returns more than that, and it's the quota-safe choice. Pass
`--enrich` to add From/Subject/Date/snippet, at the cost of one extra
`messages.get` request **per hit**. `--concurrency` (default 4) bounds how
many of those hydration requests run at once; see
[Rate limits and retry behaviour](#rate-limits-and-retry-behaviour) for the
quota math before raising it or combining `--enrich` with a large `--limit`.

[Gmail's own search syntax]: https://support.google.com/mail/answer/7190

### MCP equivalent(s)

`gmail_search` — same ids-only default; pass `enrich: true` (and optionally
`concurrency`) for the enriched rows.

## Messages

```bash
$ omni-dev gmail read <message-id>
$ omni-dev gmail read <message-id> --detail minimal
$ omni-dev gmail read <message-id> --detail metadata
$ omni-dev gmail read <message-id> --detail raw --out-file message.eml
```

`--detail` controls how much of the message is fetched — named `--detail`,
not `--format`, since `-o/--output` already owns that word for this
project's rendering axis (see [ADR-0046](adrs/adr-0046.md)); the values
match Gmail's own wire values verbatim: `minimal` (only
`id`/`threadId`/`labelIds`/`sizeEstimate` — no headers or body), `metadata`
(headers + snippet only), `full` (default; parsed MIME structure), or `raw`
(the RFC 2822 source, base64url-encoded over the wire — the cheapest way to
get a byte-for-byte copy). `--out-file` writes a flat text rendering to disk
instead of stdout for `minimal`/`metadata`/`full`; for `raw` it decodes the
base64url payload first and writes the literal RFC 2822 bytes, so
`--detail raw --out-file message.eml` produces a genuine `.eml` rather than
still-encoded text.

### MCP equivalent(s)

`gmail_message_read` — takes the same `format` values (`minimal` /
`metadata` / `full` / `raw`), plus `output_file` (writes to disk and
returns a short YAML summary instead of the inline body — for large
messages/attachments that would exceed the response size limit).

## Threads

```bash
$ omni-dev gmail thread <thread-id>
```

Fetches the whole conversation (`format=full` always — a thread's point is
showing every message in it). No `--format` or `--out-file` flag.

### MCP equivalent(s)

`gmail_thread_read`. Always truncation-guarded — a thread's N messages,
each potentially carrying attachments, is the single highest payload-size
risk on the whole Gmail surface.

## Labels

```bash
$ omni-dev gmail label list
$ omni-dev gmail label add <message-id...> --label IMPORTANT
$ omni-dev gmail label remove <message-id...> --label UNREAD
```

`label add`/`remove` require the `gmail.modify` scope (`gmail auth login
--modify`) — a `gmail.readonly`-only token gets a 403
`insufficientPermissions` error. `label add` is unconditional; `label
remove` prompts for confirmation by default (per [ADR-0027](adrs/adr-0027.md)),
accepting `--force` to skip the prompt and `--dry-run` to preview without
calling the API (`--dry-run` wins if both are set).

### MCP equivalent(s)

`gmail_label_list` ships in this release. A mutating `gmail_label_modify`
tool (add/remove) is planned as a fast-follow — until then, label mutation
is CLI-only.

## Sync

```bash
$ omni-dev gmail sync --output-dir ~/mail-archive
$ omni-dev gmail sync --output-dir ~/mail-archive --query 'label:finance'
$ omni-dev gmail sync --output-dir ~/mail-archive --full
$ omni-dev gmail sync --output-dir ~/mail-archive --dry-run
```

Maintains a durable, greppable local archive of a mailbox — full-fidelity
`.eml` files plus a JSONL manifest — incrementally updated on each run.
Unlike every other Gmail command, `sync` is a genuinely long-running bulk
operation: **a first sync of a several-thousand-message mailbox takes
minutes, not seconds**, and a 50k-message mailbox roughly 15-20 minutes,
bounded by Gmail's per-second quota (see
[Rate limits](#rate-limits-and-retry-behaviour) below). A re-run against an
already-synced mailbox with no new mail is fast — typically a single
`history.list` call.

**Archive layout:**

```
<output-dir>/
  state.json                  # watermark (historyId) + account identity
  manifest.jsonl               # one record per message: id, threadId, labelIds,
                                #   internalDate, subject, from, rfc822_msgid, path
  messages/<aa>/<id>.eml        # sharded by the first 2 characters of the message id
```

`.eml` files are **immutable** once written — Gmail labels aren't part of
the RFC 2822 body, so a label change updates only the manifest record, never
the message file. The manifest is *not* a derived index that could be
regenerated from the `.eml` files; it is the sole record of each message's
Gmail-side metadata (labels, thread, watermark).

**Backfill vs. incremental:** the first run (or `--full`) lists the whole
mailbox and fetches whatever's missing on disk — presence-on-disk is the
real idempotence mechanism, so an interrupted backfill simply picks up
where it left off on the next run, no cursor required. Subsequent runs use
`history.list` from the stored watermark, applying
`messagesAdded`/`messagesDeleted`/`labelsAdded`/`labelsRemoved` events.
Google does not guarantee history availability past roughly **one week**;
a `startHistoryId` older than that gets a 404, which `sync` treats as a
signal to fall back to the same full-listing pass as a backfill (not a
silent gap, and not a blind re-download of everything) — the `historyId`
watermark is purely an optimisation over that fallback, never a
correctness requirement. A run that hits a per-item error never advances
the watermark, so the next run safely re-examines the same range (already
-archived messages are skipped for free).

**`--query` and incremental sync (a known limitation):** `--query` scopes a
backfill/`--full`/reconciliation pass, but `history.list` has no query
filter, so an incremental run cannot re-apply it — newly-arrived mail that
would match your `--query` is only picked up by a later `--full` re-run. If
you sync a query-scoped subset of your mailbox regularly, plan on an
occasional `--full` pass.

**Header fields:** `subject`/`from`/`rfc822_msgid` in the manifest are
parsed directly from the already-fetched raw message bytes (no second
network request), and are stored as their raw wire encoding — non-ASCII
subjects encoded per RFC 2047 (`=?UTF-8?B?...?=`) are **not** decoded to
human-readable text in this release.

**`--dry-run`** reports every action sync would take without writing any
file — not `state.json`, not `manifest.jsonl`, not a single `.eml`.

No MCP equivalent — a bulk, potentially long-running filesystem operation
is a poor fit for a synchronous MCP tool call (the same reasoning that kept
label mutation CLI-only above).

## Rate limits and retry behaviour

Gmail enforces a **per-user quota of 250 units/second**; `messages.get` and
`messages.list` each cost 5 units, `messages.batchModify` costs 50 units
for up to 1000 ids. `gmail search`'s ids-only default costs a flat 5 units
regardless of `--limit` (auto-pagination is still one `messages.list` call
per page). `--enrich` adds one `messages.get` (5 units) **per hit**, so
`--enrich --limit 50` can cost up to 255 units — nearly the entire
per-second budget in one command — and `--limit 0 --enrich` against a large
mailbox can cost tens of thousands of units, spread across as many seconds
as `--concurrency` allows. `--concurrency` (default 4) bounds how many of
those `messages.get` calls are in flight at once; it does not itself pace
requests against the per-second budget, so a large `--limit --enrich`
combination should be sized deliberately, not left at defaults.

Gmail signals quota exhaustion as **HTTP 403** with `reason:
rateLimitExceeded` / `userRateLimitExceeded`, not HTTP 429 — the Gmail
client's requests retry both `429` and this specific 403 shape through the
shared retry driver (`retry_if`/`retry_429`, `src/utils/http.rs`), with the
same `Retry-After`-then-exponential-backoff schedule; any other 403 (e.g.
`insufficientPermissions`) is never retried. `gmail sync` additionally
paces its own `messages.get` requests against the 250-units/second budget
with a proactive token-bucket limiter, rather than relying on this reactive
retry — see [Sync](#sync) above. `search --enrich`/`thread` still rely on
`--concurrency` alone (a concurrency bound, not a rate limiter) plus this
retry driver as their only quota protection.

The list endpoints (`search`, `thread`'s underlying calls) auto-paginate
when `--limit 0` is passed, capped at **10,000 records** per invocation.
Any non-zero `--limit` is upper-bounded by the same cap.

## Troubleshooting

### Credentials not configured

```
Error: Gmail credentials not configured. Run `omni-dev gmail auth login`
```

Means `GMAIL_CLIENT_ID`, `GMAIL_CLIENT_SECRET`, or `GMAIL_REFRESH_TOKEN` is
missing. The first two must be set by hand before `auth login`; the third
is written by `auth login` itself.

### `invalid_grant`

```
Error: Failed to obtain a Gmail access token
  Caused by: Google rejected the request (invalid_grant): this almost always means either (1) your Gmail OAuth client is in "Testing" publishing status, where refresh tokens expire after 7 days — publish it to "In production" in Google Cloud Console to avoid this, or (2) access was revoked. Run `omni-dev gmail auth login` again to re-authenticate.
```

The most common cause by far is the 7-day testing-mode refresh-token
expiry described in [Prerequisites](#prerequisites). Re-run
`omni-dev gmail auth login`, or push your OAuth client to "In production"
in Google Cloud Console to stop it recurring.

### `access_denied`

```
Error: Google denied the authorization request: access_denied
```

You (or another user) clicked "Cancel" on Google's consent screen, or your
OAuth client's test-user allowlist doesn't include the account you tried to
authorize (a Testing-mode consent screen only allows explicitly added test
users). Re-run `omni-dev gmail auth login` and either approve the prompt or
add the account under **OAuth consent screen → Test users** in Google Cloud
Console.

### Could not start the local OAuth callback listener

```
Error: Failed to start the local OAuth callback listener
```

The loopback listener binds an OS-assigned ephemeral port
(`127.0.0.1:0`), so this should be rare. The one common cause is a stale
process from a previously interrupted `gmail auth login` holding a socket
resource open — retry, and if it persists, check for a leftover `omni-dev`
process.

### Browser did not open

`gmail auth login` opens your default browser automatically. If it fails
to open (e.g. over SSH, or in a headless environment), the authorization
URL is printed to the terminal for you to open manually — no CLI flag is
needed to force this fallback; it's the same code path.

### `insufficientPermissions` (label add/remove)

```
Error: Gmail API request failed: HTTP 403: Insufficient Permission (reason: insufficientPermissions)
```

Only `gmail.readonly` was granted. Fix is `omni-dev gmail auth login
--modify` (re-consent with the write scope), not a retry.

### MCP server cannot see credentials

Same as every other domain: environment variables exported in your
interactive shell are not inherited by an MCP client unless it launched
the server from that same shell. Run `omni-dev gmail auth login` once —
this persists the refresh token (plus client id/secret) to
`~/.omni-dev/settings.json`, read by every invocation regardless of how
the process started.

## See also

- [User Guide](user-guide.md#gmail-integration) — short reference; primary
  content lives here.
- [MCP Reference — Gmail](mcp.md#gmail-5-tools) — parameter-only listing of
  all 5 `gmail_*` MCP tools.
- [ADR-0063](adrs/adr-0063.md) — OAuth2 authorization-code + PKCE design,
  refresh-token-only persistence, and the bring-your-own Google Cloud
  project rationale.
- [Gmail API documentation](https://developers.google.com/workspace/gmail/api/reference/rest) — upstream reference.
