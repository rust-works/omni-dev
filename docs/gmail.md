# Gmail Integration

omni-dev exposes read access (and, opt-in, label mutation) to the Gmail v1
API through the `omni-dev gmail` command tree, with a matching `gmail_*` MCP
tool for every read-only subcommand. Authentication and output formats are
identical across both surfaces; the MCP tools simply return YAML matching the
CLI's `-o yaml` output. For the MCP-tool reference (parameters only), see
[docs/mcp.md](mcp.md#gmail-6-tools).

New to this integration? Follow the
[Gmail Quickstart](gmail-quickstart.md) for a linear, zero-to-synced-archive
walkthrough — this page is the topic-by-topic reference.

## Table of Contents

1. [Prerequisites](#prerequisites)
2. [Authentication](#authentication)
3. [Multiple accounts](#multiple-accounts)
4. [Output formats](#output-formats)
5. [Search](#search)
6. [Messages](#messages)
7. [Threads](#threads)
8. [Labels](#labels)
9. [Sync](#sync)
10. [Rate limits and retry behaviour](#rate-limits-and-retry-behaviour)
11. [Troubleshooting](#troubleshooting)
12. [See also](#see-also)

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
5. When you run `gmail auth login` below, Google's consent screen lists
   Gmail as its **own separate permission tick-box**, distinct from the
   basic profile/email checkboxes it also requests. **Explicitly tick
   it.** Leaving it unticked makes login fail immediately with an error
   naming the scopes Google actually granted (e.g. `openid`, `email`,
   `profile` — no Gmail scope at all) instead of writing an unusable
   refresh token to `settings.json`. See
   [Troubleshooting](#no-gmail-scope-was-granted) for the exact error.

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

`GMAIL_CLIENT_ID`/`GMAIL_CLIENT_SECRET` can reach `gmail auth login` three
ways: run `omni-dev gmail auth import [PATH]` first to read them straight
out of the `client_secret.json` Google Cloud Console hands out (the
secret never transits a shell, an env var, or an agent's context — see
[below](#interactive-setup)); set them by hand (in your shell profile, or
in `~/.omni-dev/settings.json`'s `env` map); or leave them unset and
`gmail auth login` prompts for them interactively — the client id echoes
normally, the secret does not.

### Interactive setup

If you downloaded the OAuth client's `client_secret.json` from the Cloud
console, import it directly — the client id/secret are saved to
`settings.json` without ever passing through your shell:

```bash
$ omni-dev gmail auth import
Found ~/Downloads/client_secret_1234.apps.googleusercontent.com.json (Desktop app client)
Client id/secret saved to ~/.omni-dev/settings.json

Run `omni-dev gmail auth login` to authorize.
```

`PATH` is optional: discovery tries `$GMAIL_CLIENT_SECRET_FILE`, then
`~/.config/gws/client_secret.json`, then the most-recently-modified
`~/Downloads/client_secret_*.apps.googleusercontent.com.json` (the Cloud
console's default download name).

Then run `auth login` — if `auth import` wasn't run and the client
id/secret aren't in the environment or `settings.json` either, it prompts
for them instead:

```bash
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

Pass `--all` to report every configured named account (see
[Multiple accounts](#multiple-accounts)) in one call instead of just the
resolved one:

```bash
$ omni-dev gmail auth status --all

== work ==
Checking Gmail authentication...
Authenticated as: alice@work.com
Messages in mailbox: 12034
Granted scope: gmail.readonly, gmail.modify

== personal ==
Checking Gmail authentication...
Authenticated as: alice@gmail.com
Messages in mailbox: 5842
Granted scope: gmail.readonly
```

`--all` degenerates to the single-account output above when no named
accounts are configured. Each successful check also backfills that
account's cached `email_address` in `settings.json` if it isn't already
set (never used for authentication itself — only for the browser-profile
targeting below) — an explicit value, whether you set it by hand or a
previous check backfilled it, is never overwritten.

### Removing credentials

```bash
$ omni-dev gmail auth logout
Gmail credentials removed from ~/.omni-dev/settings.json
```

Idempotent: if no credentials are configured, it prints
`No Gmail credentials were configured.` and exits successfully. Removes
the resolved account (see [Multiple accounts](#multiple-accounts) below) —
pass `--account NAME` to target a specific named account.

## Multiple accounts

`--profile` (see [Prerequisites](#prerequisites) and
[ADR-0045](adrs/adr-0045.md)) selects a whole credential bundle — Atlassian,
Datadog, the Claude API key, *and* Gmail all at once. That's the wrong tool
for "I just want a second mailbox while everything else about my
environment stays the same," so Gmail accounts are a second, independent
axis: named entries in a `gmail` block of `~/.omni-dev/settings.json`,
selected per invocation via an `--account NAME` flag or the
`OMNI_DEV_GMAIL_ACCOUNT` environment variable (AWS-CLI style, mirroring
`--profile`). `--account` is scoped to the `gmail` command tree — usable
either right after `gmail` or after the leaf subcommand
(`gmail --account work search ...` or `gmail search --account work ...`),
but not before `gmail` itself, since it isn't a CLI-wide flag. See
[ADR-0066](adrs/adr-0066.md) for the full design rationale.

**Zero-migration guarantee:** an installation that never configures a named
account behaves exactly as before — every command in this guide works
identically whether or not you ever touch `--account`.

### Configuring accounts

Create a second (or subsequent) account the same way you configured the
first, adding `--account NAME`:

```bash
$ omni-dev gmail auth import --account personal
$ omni-dev gmail auth login --account personal
```

`--account` need not already exist — `auth login`/`auth import` are how an
account comes into existence. Every other Gmail command (`search`, `read`,
`thread`, `label`, `sync`, `auth status`, `auth logout`) also accepts
`--account NAME` to target a specific mailbox, and the MCP tools accept the
equivalent `account` parameter.

If you already have a single-account setup and want to migrate it into a
named account instead of starting over:

```bash
$ omni-dev gmail account import-legacy --name work
Legacy Gmail credentials migrated to account 'work'. Legacy credentials left
in place — pass --remove-legacy to delete them.
```

Non-destructive by default; pass `--remove-legacy` to delete the old
credentials once you've confirmed the migration worked. `import-legacy`
takes `--name`, not `--account` — `--account` is inherited by every `gmail`
subcommand (including `import-legacy`) and selects an *existing* account,
while this one names the account being *created*, and clap doesn't allow a
subcommand to redefine an inherited flag. `--name` defaults to the literal
name `default` if omitted.

**One sharp edge:** the moment a first named account is created — via
`auth login --account NAME` or `account import-legacy` — while legacy
credentials still exist, those legacy credentials become **shadowed**: a
no-`--account` invocation from then on resolves through the named-account
rules below and no longer falls back to them. omni-dev prints a one-time
stderr notice at that exact transition, pointing at `gmail account
import-legacy` (to migrate any other legacy account) or `gmail auth logout`
(to remove the now-unreachable legacy credentials).

### Managing accounts

```bash
$ omni-dev gmail account list
NAME      EMAIL              SCOPE                          DEFAULT
personal  alice@gmail.com    gmail.readonly                 
work      alice@work.com     gmail.readonly, gmail.modify   *

$ omni-dev gmail account set-default work
Default Gmail account set to 'work'.
```

`gmail account list` reads only `settings.json` — no network call, no
secret ever rendered. The matching MCP tool is `gmail_account_list`; call
it before passing an `account` parameter to any other Gmail tool, since an
unknown name is a hard error rather than a silent fallback.

### Resolution order

When a command runs, the account it uses is resolved in this order:

1. A literal `GMAIL_CLIENT_ID`/`GMAIL_CLIENT_SECRET`/`GMAIL_REFRESH_TOKEN`
   set directly in the process environment bypasses account resolution
   entirely — today's exact single-account behaviour, unchanged.
2. `--account NAME` / `OMNI_DEV_GMAIL_ACCOUNT`, if set, selects that named
   account. An unknown name is a hard error listing the accounts that
   *are* configured — never a silent fallback to the wrong mailbox.
3. No explicit account, with one or more named accounts configured: the
   configured default (`gmail account set-default`) if it still names a
   real account, else the sole account if exactly one is configured, else
   a hard error naming both remedies.
4. No named accounts configured at all: falls through unchanged to the
   pre-multi-account resolution (process env → the active `--profile`'s
   `env` map → the base `env` map) — the zero-migration path.

### Browser profile targeting

With several named accounts, `gmail auth login` opening whatever profile
your default browser happens to be on means you have to switch Google
identities by hand on the consent screen — easy to get wrong, and it can
land the refresh token on the wrong mailbox entirely. Two escape hatches,
both configured per account in `settings.json`'s `gmail.accounts.<name>`
and both opt-in — neither changes behaviour for an account that sets
neither:

**Manual — `browser_command`.** An explicit launch command, with `{url}`
substituted for the authorization URL (or appended, if no `{url}`
placeholder is present). Takes precedence over automatic resolution below.
Works for any browser, not just Chrome:

```json
"gmail": {
  "accounts": {
    "jky.greens": {
      "browser_command": "open -na \"Google Chrome\" --args --profile-directory=\"Profile 7\" {url}"
    }
  }
}
```

**Automatic — `chrome_profile_from_email`.** Set this `true` alongside
`email_address` (see [Verifying credentials](#verifying-credentials) above
— set it by hand, or let `gmail auth status --all` backfill it after a
first login) and `gmail auth login` looks up which local Chrome profile is
signed into that address, launching the authorization URL targeting it
instead of the OS default browser:

```json
"gmail": {
  "accounts": {
    "jky.greens": {
      "email_address": "jky.greens@example.com",
      "chrome_profile_from_email": true
    }
  }
}
```

Chrome-only for now (no Chromium/Brave/Edge support yet — use
`browser_command` for those). Resolution reads Chrome's own `Local State`
file and never guesses: zero matching profiles or more than one profile
signed into the same address both fall back to the OS default browser
rather than picking one, same as Chrome not being installed or the file
being unreadable — resolution failure is always a fallback, never a login
failure. See [ADR-0067](adrs/adr-0067.md) for the full design rationale.

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
$ omni-dev gmail sync --output-dir ~/mail-archive --extract-attachments
```

Maintains a durable, greppable local archive of a mailbox — full-fidelity
`.eml` files plus a JSONL manifest — incrementally updated on each run.
Unlike every other Gmail command, `sync` is a genuinely long-running bulk
operation: **a first sync of a several-thousand-message mailbox takes
minutes, not seconds**. A 50k-message mailbox is roughly 15-20 minutes at
Gmail's theoretical 50 msg/s quota ceiling, but real-world throughput
depends on message sizes and network too — a measured run against a
5,824-message mailbox sustained 36.4 msg/s, which extrapolates to roughly
23 minutes for 50k. Either figure is bounded by Gmail's per-second quota
(see
[Rate limits](#rate-limits-and-retry-behaviour) below). A re-run against an
already-synced mailbox with no new mail is fast — typically a single
`history.list` call.

On a terminal, a backfill/`--full`/reconciliation run shows two live
progress indicators on stderr — a listing spinner (pages fetched, ids
discovered so far) and a fetch bar (messages fetched so far out of the
currently-known total, plus a running error count) — updated as the
mailbox is listed and fetched *concurrently*, rather than only printing a
report once the entire run finishes. Total wall-clock time is unchanged
(still bounded by the same per-second quota above); what changes is that
fetching now begins as soon as the first listing page arrives, instead of
waiting for the whole mailbox to be listed first. Pass `--quiet` to
suppress the bars; they're also disabled automatically when stderr isn't a
terminal or when `-o json`/`-o yaml`/`-o yamls`/`-o jsonl` is selected.
Whenever the bars ran (or `--quiet` was passed), the final text report
skips the per-action listing too (see **Report summary** below), since
bars already showed every fetch/delete live and repeating them as text
would just be a second, redundant dump.

**Archive layout:**

```
<output-dir>/
  state.json                  # watermark (historyId) + account identity
  manifest.jsonl               # one record per message: id, thread_id, label_ids,
                                #   internal_date, subject, from, to, rfc822_msgid,
                                #   in_reply_to, references, attachment_count,
                                #   attachment_filenames, path, size, history_id,
                                #   deleted_at (soft-deleted messages only)
  messages/<year>/<month>/<day>/<id>.eml   # sharded by the message's internal_date
  messages/<year>/<month>/<day>/<id>/attachments/<filename>  # only with --extract-attachments
```

`.eml` files are **immutable** once written — Gmail labels aren't part of
the RFC 2822 body, so a label change updates only the manifest record, never
the message file. The manifest is *not* a derived index that could be
regenerated from the `.eml` files; it is the sole record of each message's
Gmail-side metadata (labels, thread, watermark).

**Backfill vs. incremental:** the first run (or `--full`) lists the whole
mailbox and fetches whatever's missing on disk — listing and fetching are
pipelined, so the fetch fan-out for early-listed messages starts
immediately rather than waiting for the whole mailbox to be listed first.
Presence-on-disk is the real idempotence mechanism, so an interrupted
backfill simply picks up where it left off on the next run, no cursor
required. The manifest itself is checkpointed to disk every 200 fetched
messages during a large backfill (not only once at the end), so a crash
loses at most that many messages' worth of already-completed work, not the
whole run. Subsequent runs use `history.list` from the stored watermark,
applying `messagesAdded`/`messagesDeleted`/`labelsAdded`/`labelsRemoved`
events. Google does not guarantee history availability past roughly **one
week**; a `startHistoryId` older than that gets a 404, which `sync` treats
as a signal to fall back to the same full-listing pass as a backfill (not a
silent gap, and not a blind re-download of everything) — the `historyId`
watermark is purely an optimisation over that fallback, never a
correctness requirement. (An incremental run's own `history.list` pass is
not pipelined — it's typically a single page already, so there's little to
overlap; only the full-listing path above gains concurrent
listing+fetching.) A run that hits a per-item error never advances the
watermark, so the next run safely re-examines the same range (already
-archived messages are skipped for free).

**`--query` and incremental sync (a known limitation):** `--query` scopes a
backfill/`--full`/reconciliation pass, but `history.list` has no query
filter, so an incremental run cannot re-apply it — newly-arrived mail that
would match your `--query` is only picked up by a later `--full` re-run. If
you sync a query-scoped subset of your mailbox regularly, plan on an
occasional `--full` pass.

**Header fields:** `subject`/`from`/`to`/`rfc822_msgid`/`in_reply_to`/
`references` in the manifest are parsed directly from the already-fetched
raw message bytes (no second network request), and are stored as their raw
wire encoding — non-ASCII subjects encoded per RFC 2047
(`=?UTF-8?B?...?=`) are **not** decoded to human-readable text in this
release. `in_reply_to`/`references` are what let a conversation be
reconstructed from the manifest alone, without re-parsing every `.eml`.

**Attachments:** `attachment_count` and `attachment_filenames` record how
many MIME parts are marked `Content-Disposition: attachment` and whichever
filenames could be parsed from them (including RFC 2231 percent-encoded
filenames), scanned from the same already-decoded bytes — no second fetch.
This is metadata only, computed the same way regardless of
`--extract-attachments` (see [ADR-0065](adrs/adr-0065.md)): attachments
always stay inline inside the `.eml` too (lossless, since `format=raw`
preserves them).

**`--extract-attachments`** additionally writes each message's
`Content-Disposition: attachment` MIME parts to disk as separate files
under `messages/<year>/<month>/<day>/<id>/attachments/<filename>` — a
sibling directory of the message's own `.eml`. Off by default: it's extra
I/O and disk usage per message, and the `.eml` remains the lossless source
of truth either way, so this is purely a convenience projection, never a
new archive contract. Filenames are sanitised against path traversal; a
second attachment in one message that sanitises to an already-used name
gets a `-N` suffix (`image.png` -> `image-1.png`); an attachment with no
usable filename gets a synthesised one. A message that fails to parse as
MIME simply yields no attachment files — it never fails the `.eml` fetch
itself. Because `sync` only ever fetches messages missing on disk
(presence-on-disk is the archive's idempotence mechanism — see above),
turning this flag on does **not** retroactively extract attachments for
messages already archived by an earlier run, even under `--full`; delete
the affected `.eml` files (or the whole archive) and re-run `--full
--extract-attachments` to force re-extraction.

**Report summary:** every report — table/text and `-o json`/`-o yaml`/
`-o yamls`/`-o jsonl` alike — ends with an at-a-glance tally, e.g.
`5,794 fetched, 30 deleted, 0 errors` in text output, or an explicit
`summary` field (`fetched`/`would_fetch`/`labels_updated`/`deleted`/
`undeleted`/`would_delete`/`would_undelete`/`errors` counts) in the
structured formats. `-o json`/`-o yaml`/`-o yamls`/`-o jsonl` always
include the full per-action listing alongside `summary` too — the
authoritative, complete record. Text output includes the per-action
listing only when nothing else already showed it: if the live progress
bars ran, or `--quiet` was passed, text output shows just `Note`s, errors,
and the summary — a large sync's per-action listing can run into the
thousands of lines, and printing it again once bars already rendered it
live would just be a second, redundant dump. A non-interactive `stderr`
(no bars possible) still gets the full per-action listing in text, since
it's the only record of what happened in that case.

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
missing from both the environment and `settings.json`. Run
`omni-dev gmail auth import` or just `omni-dev gmail auth login` — it
prompts for the first two if they're still absent — to fix the first two;
the third is written by `auth login` itself.

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

If it opens the *wrong* browser profile (mixing up which named account
lands on which Google identity), see [Browser profile
targeting](#browser-profile-targeting) above.

### No Gmail scope was granted

```
Error: Google did not grant a Gmail scope (received: openid, email, profile).
  On the consent screen, tick the Gmail permission — restricted scopes are
  not granted by default. Re-run `omni-dev gmail auth login`.
```

Cause: the consent screen's Gmail permission tick-box (see
[Prerequisites](#prerequisites)) was left unticked, so Google granted only
`openid`/`email`/`profile` — no Gmail scope at all. `auth login` rejects
this immediately, naming the scopes Google actually granted, and writes
nothing to `settings.json`. Fix: re-run `omni-dev gmail auth login` and
tick the Gmail permission this time — `--modify` does not help here,
since the problem isn't *which* Gmail scope was granted, it's that none
was.

### `insufficientPermissions`

```
Error: Gmail API request failed: HTTP 403: Insufficient Permission (reason: insufficientPermissions)
```

`gmail.readonly` was granted, but `label add`/`remove` fails — read
commands (`search`, `read`, `thread`, `auth status`) all work fine; only
label mutation 403s. Fix is `omni-dev gmail auth login --modify`
(re-consent with the write scope), not a retry.

### MCP server cannot see credentials

Same as every other domain: environment variables exported in your
interactive shell are not inherited by an MCP client unless it launched
the server from that same shell. Run `omni-dev gmail auth login` once —
this persists the refresh token (plus client id/secret) to
`~/.omni-dev/settings.json`, read by every invocation regardless of how
the process started.

### `operation timed out` fetching a message during `sync`

```
Error: <id> failed: Failed to parse messages.get response: error decoding response body for url (...): request or response body error: operation timed out
```

`messages.get?format=raw` returns the whole message (headers, body, and
every attachment, base64-encoded) in one response. The Gmail client (like
the Atlassian and Datadog clients) sets two independent timeouts, not one:
a 10-second connect timeout (DNS + TCP + TLS handshake) and a 120-second
**read** timeout that covers each individual read of the response body and
resets on every successful one — it's a stall detector, not a fixed total
deadline, so a download that's slow-but-still-progressing keeps extending
it rather than getting cut off partway through. A handful of large
messages (tens of MB — attachment-heavy mail) downloading concurrently
under `--concurrency` divide the available bandwidth, so each read can
individually stall long enough to trip the read timeout even though
nothing is actually stuck. This is more likely the more of `--concurrency`
is spent on large messages at once, not a sign of a broken connection.

`sync` is safe to just re-run: a run with errors never advances the
watermark, and presence-on-disk means already-archived messages are
skipped, so a re-run only retries what failed. Two ways to make it
succeed:

- Lower `--concurrency` (even down to `1`) so each large download gets
  more of the available bandwidth to itself.
- Raise the read timeout instead via `OMNI_DEV_HTTP_READ_TIMEOUT_SECS`
  (whole seconds; a missing, non-numeric, or non-positive value falls back
  to the 120-second default) — shared by the Gmail, Atlassian, and Datadog
  REST clients, e.g.
  `OMNI_DEV_HTTP_READ_TIMEOUT_SECS=300 omni-dev gmail sync ...`. The
  connect timeout has its own override, `OMNI_DEV_HTTP_CONNECT_TIMEOUT_SECS`
  (default 10s), for the unrelated case of a slow-to-establish connection.

## See also

- [Gmail Quickstart](gmail-quickstart.md) — a linear, zero-to-synced-archive
  walkthrough for first-time setup.
- [User Guide](user-guide.md#gmail-integration) — short reference; primary
  content lives here.
- [MCP Reference — Gmail](mcp.md#gmail-6-tools) — parameter-only listing of
  all 6 `gmail_*` MCP tools.
- [ADR-0063](adrs/adr-0063.md) — OAuth2 authorization-code + PKCE design,
  refresh-token-only persistence, and the bring-your-own Google Cloud
  project rationale.
- [ADR-0066](adrs/adr-0066.md) — the named-account store behind
  [Multiple accounts](#multiple-accounts), and why it's orthogonal to
  `--profile`.
- [Gmail API documentation](https://developers.google.com/workspace/gmail/api/reference/rest) — upstream reference.
