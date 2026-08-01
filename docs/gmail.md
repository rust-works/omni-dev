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
8. [Rate limits and retry behaviour](#rate-limits-and-retry-behaviour)
9. [Troubleshooting](#troubleshooting)
10. [See also](#see-also)

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
```

`--query` uses [Gmail's own search syntax] (the same operators as the Gmail
search box: `from:`, `label:`, `after:`, `has:attachment`, etc.) — omni-dev
does not reinterpret it. `--limit 0` fetches every match up to a 10,000
hard cap, auto-paginating underneath. Each hit is enriched with
From/Subject/Date/snippet via one extra `messages.get` request per result
(`messages.list` itself only returns bare ids).

[Gmail's own search syntax]: https://support.google.com/mail/answer/7190

### MCP equivalent(s)

`gmail_search`.

## Messages

```bash
$ omni-dev gmail read <message-id>
$ omni-dev gmail read <message-id> --format meta
$ omni-dev gmail read <message-id> --format raw --out-file message.eml
```

`--format` controls how much of the message is fetched: `meta` (headers +
snippet only), `full` (default; parsed MIME structure), or `raw`
(base64url-encoded RFC 2822 source — the cheapest way to get a
byte-for-byte copy). `--out-file` writes a flat text rendering to disk
instead of stdout.

### MCP equivalent(s)

`gmail_message_read` — takes the same `format` values, plus `output_file`
(writes to disk and returns a short YAML summary instead of the inline
body — for large messages/attachments that would exceed the response size
limit).

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

## Rate limits and retry behaviour

Gmail enforces a **per-user quota of 250 units/second**; `messages.get` and
`messages.list` each cost 5 units, `messages.batchModify` costs 50 units
for up to 1000 ids. `gmail search`'s enrichment step (one `messages.get`
per hit) is usually the dominant per-call cost, not the initial list call.

**Known gap:** Gmail signals quota exhaustion as **HTTP 403** with
`reason: rateLimitExceeded` / `userRateLimitExceeded`, not HTTP 429.
omni-dev's shared retry driver (`retry_429`, used by every REST client in
this project) only retries literal 429s, so a Gmail rate-limit error is
**not** automatically retried in this release — it surfaces immediately
with the `reason` included in the error message so it's at least
actionable. Narrow your `--query`/`--limit` if you hit this.

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
