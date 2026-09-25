# Gmail Integration

omni-dev exposes read access (and, opt-in, label mutation) to the Gmail v1
API through the `omni-dev gmail` command tree, with a matching `gmail_*` MCP
tool for every read-only subcommand. Authentication and output formats are
identical across both surfaces; the MCP tools simply return YAML matching the
CLI's `-o yaml` output. For the MCP-tool reference (parameters only), see
[docs/mcp.md](mcp.md#gmail-8-tools).

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
9. [Drafts](#drafts)
10. [Sync](#sync)
11. [Sync all accounts](#sync-all-accounts)
12. [Extract attachments](#extract-attachments)
13. [Render](#render)
14. [Insert](#insert)
15. [Rate limits and retry behaviour](#rate-limits-and-retry-behaviour)
16. [Troubleshooting](#troubleshooting)
17. [See also](#see-also)

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

To go to **In production**: OAuth consent screen → **Publish App**. This
by itself does not trigger a verification review — the next time you (or
any of your up-to-100 test users) sign in, Google shows an "unverified
app" interstitial; click **Advanced → Go to `<your project>` (unsafe)**
to proceed. That warning is expected and permanent for a project like
this one — it's not a sign anything is misconfigured, and it's the
tradeoff for not taking on CASA. **Don't upload a logo** on the Branding
page: Google requires a full verification review (including CASA for
restricted scopes like this one) before it will display a logo, so
uploading one moves your project onto that track even though you never
asked for a review. Branding fields otherwise (app name, support email)
don't trigger it.

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

Every subcommand that renders a list or record (`search`, `read`, `thread`,
`label list`, `draft list`, `draft show`, `sync`, `sync-all`, `extract-attachments`, `render`, `account
list`) accepts `-o <format>` (`table` / `json` / `yaml` / `yamls` / `jsonl`,
default `table`) — the same convention as every other `omni-dev` domain
(see [ADR-0046](adrs/adr-0046.md)). `auth login`/`auth logout`/`auth
status`, `label add`/`label remove`, and `account set-default`/`account
import-legacy` print a fixed human-readable status line instead and have no
`-o` flag. `--out-file` exists only on `gmail read`, the one command with a
naturally file-shaped payload (a message body/attachment source worth
writing to disk); no other Gmail leaf has a use for it.

`gmail read` additionally accepts `-o markdown` — a human-readable
Markdown rendering of the message rather than a machine-readable format;
see [Messages](#messages) and [Render](#render).

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
$ omni-dev gmail read <message-id> -o markdown
$ omni-dev gmail read <message-id> -o markdown --out-file message.md
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

**`-o markdown`** renders the message as human-readable Markdown: a header
block (Subject/From/To/Cc/Date/Message-Id/In-Reply-To/References, RFC
2047-decoded — unlike the raw wire encoding [Sync](#sync)'s manifest
fields keep), the body (`text/plain` preferred, `text/html` converted to
Markdown otherwise), and an attachment filename list. It always fetches the
complete raw MIME message regardless of `--detail` (rendering needs the
full structure), so `--detail` is ignored when combined with `-o markdown`.
The same rendering function backs [`gmail render`](#render) for already-
archived `.eml` files — `-o markdown` is the live-fetch equivalent, useful
when you want readable text for one message without archiving the whole
mailbox first.

**`--fold-quotes`** (only relevant with `-o markdown`; ignored otherwise,
the reverse of `--detail`'s asymmetry) collapses `>`-quoted reply history
nested more than one level deep into a one-line `*(N quoted lines
omitted)*` marker, so a thread with 10-20+ levels of quoting doesn't drown
its new content in repeated older quotes. The immediately-preceding
reply's quote (depth 1) always stays visible for context; only deeper
nesting folds. Off by default — verbatim rendering is fully
information-preserving, and the full text is one re-render away without
the flag.

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

## Drafts

```bash
$ omni-dev gmail draft list
$ omni-dev gmail draft list --query 'to:alice subject:report' --limit 10
$ omni-dev gmail --account work draft list -o yaml
```

`draft list` lists the mailbox's drafts. Each row shows the **draft id**,
the id of the draft's current message, its thread id, the `To` header,
the subject, the date and a snippet. `-o yaml`/`json` also carry `cc` and
`bcc`.

The draft id and the message id are different things, and the output
labels them `DRAFT_ID` / `MESSAGE_ID` (`draft_id` / `message_id` in machine
output) so they can't be confused. Every drafts endpoint is addressed by the
draft id. The message id changes each time a draft is saved. `gmail search
--query in:drafts` finds draft *messages*, but it cannot return their draft
ids. That is what this command is for.

`--query` takes the same [Gmail search syntax][Gmail's own search syntax]
as `gmail search`. `--limit` also works the same way: the default is 50,
and `0` fetches every draft up to the 10,000 hard cap, auto-paginating
underneath.

`drafts.list` returns only ids, so each row costs one extra `messages.get`
(5 quota units). That is the same cost as `gmail search --enrich`, and
these calls run at the same fixed concurrency (4); see
[Rate limits and retry behaviour](#rate-limits-and-retry-behaviour).
`draft list` has no `--concurrency` flag. Saving a draft replaces its
message. If a draft is saved while `draft list` runs, the old message it
was about to fetch is gone. That row still appears, with its ids but blank
headers, and running the command again shows it in full.

### Showing a draft

```bash
$ omni-dev gmail draft show r-1234567890
$ omni-dev gmail draft show r-1234567890 -o markdown
$ omni-dev gmail draft show r-1234567890 --detail raw --out-file draft.eml
```

`draft show` fetches one draft by its **draft id** (the `DRAFT_ID` column of
`draft list`). `gmail read <message-id>` can read a draft's message too, but
that id goes stale the next time the draft is saved. The draft id does not.

It takes `gmail read`'s flags and shares its output code, so the two
render a message the same way:

- `--detail minimal|metadata|full|raw` (default `full`) picks how much of
  the message Gmail returns.
- The default table view prints `Draft-Id`, `Message-Id`, `Thread-Id`,
  labels and the snippet. `-o markdown` renders a `Draft-Id` line, the
  headers and the body. Unlike `gmail read`, it includes `Bcc`, which a
  draft keeps. `read` and `render` leave `Bcc` out because Gmail keeps it
  on Sent mail too, and hiding those recipients is the point of `Bcc`. `-o json`/`yaml`/`yamls`/`jsonl`
  emit Gmail's `drafts.get` response, `{id, message}`, so machine output
  carries the draft id beside the message.
- `--detail raw --out-file PATH` writes the draft's exact stored RFC 2822
  bytes: an `.eml` file you can edit and hand back to
  [`draft update --raw`](#updating-drafts).

A draft id that doesn't exist fails with `No draft with id "…"`. The usual
cause is passing a message id from `gmail search` or `gmail read`, which no
drafts endpoint accepts.

**Read-only scope is enough.** `drafts.list`, `drafts.get` and
`messages.get` all accept `gmail.readonly`, so `draft list` and `draft show`
work for an account authorised without `--modify`.

### Creating drafts

```bash
$ omni-dev gmail draft create --to alice@example.com --subject 'Quarterly report' \
    --body 'Figures attached.' --attach q3.pdf
$ omni-dev gmail draft create --to 'Zoë Ångström <zoe@example.com>' --cc bob@example.com \
    --subject 'Grüße' --body-file note.txt
$ git log -1 --format=%B | omni-dev gmail draft create --to team@example.com --subject 'Release notes'
$ omni-dev gmail draft create --to alice@example.com --subject 'Quarterly report' \
    --html-body-file note.html --attach q3.pdf
$ omni-dev gmail draft create --to alice@example.com --reply-to 18c2f0a1b2c3d4e5 --body 'Thanks!'
$ omni-dev gmail draft create --reply-to 18c2f0a1b2c3d4e5 --reply-all --body 'Thanks, all!'
$ omni-dev gmail draft create --from 'Sales Team <sales@example.com>' --to alice@example.com \
    --subject 'Your order' --body 'It shipped today.'
$ omni-dev gmail draft create --raw message.eml
```

`draft create` stages a new draft and prints its `DRAFT_ID`, `MESSAGE_ID`
and `THREAD_ID` (`-o yaml`/`json` give `draft_id`/`message_id`/`thread_id`).
Nothing is sent. The draft waits in Gmail's Drafts folder for a person to
review and send.

- **Recipients.** `--to` is required unless `--reply-to` is given, which
  defaults it (see *Reply recipients* below). `--cc` and `--bcc` are
  optional. Each value is one mailbox, either `addr@example.com` or
  `Name <addr@example.com>`.
  Repeat the flag, or list several values after it, for more recipients.
  Values are never split on commas, so `"Doe, Jane" <jane@example.com>`
  works. Non-ASCII names and subjects are sent as RFC 2047 encoded words.
  A line break in any header value is rejected.
- **Body.** The plain-text body is taken from `--body TEXT`,
  `--body-file PATH` or, failing both, standard input. When standard input
  is a terminal, the command errors instead of waiting for typed input. A
  script that runs it with an open but silent stdin must pass `--body` or
  `--body-file`, or redirect `</dev/null` for an empty body. The body must be
  UTF-8.
- **HTML body.** `--html-body HTML` or `--html-body-file PATH` adds an HTML
  version. The message is then `multipart/alternative`, with the plain-text
  part first and the HTML part second (nested inside `multipart/mixed` when
  there are attachments), so Gmail and other HTML-capable clients show the
  HTML. With `--body` or `--body-file` as well, that text is the plain-text
  part, used as given. Without either, the plain-text part is the HTML
  converted to Markdown with the converter `gmail render` uses, so links
  survive as `[text](url)`, leaving out a full document's `<head>`,
  `<style>` and `<script>` content; standard input is never read in that
  case. The
  HTML must be UTF-8 and is sent as given, not sanitised. Inline images
  aren't supported: HTML that refers to one with `cid:` gets a warning,
  because nothing attaches the image and it would show as broken.
- **Attachments.** Each `--attach PATH` becomes a base64 part of a
  `multipart/mixed` message. Its type is guessed from the file extension,
  defaulting to `application/octet-stream`.
- **From.** Not set by default, so Gmail fills in the account's own
  address. `--from ADDR` (`addr@example.com` or `Name <addr@example.com>`)
  sends as one of the account's **send-as addresses** instead: the primary
  address or an alias added under Gmail's *Settings → Accounts → Send mail
  as*. `draft create` checks it first with one `users.settings.sendAs.list`
  call and refuses, before anything is created, an address that isn't one
  of them or an alias still awaiting verification. The error lists the
  addresses the account can use. The check is there because Gmail is
  reported to send from the primary address instead when a draft's `From`
  isn't a verified alias, so the draft would look right and go out wrong.
  The address is matched case-insensitively and written as Gmail stores it.
  Without a name, the name is the one Gmail has for that address, else the
  primary address's, as Gmail itself does for a nameless alias. When that
  leaves the primary address with no name at all, `From` is left out, so
  Gmail fills in the address and your account name rather than a bare
  address. The uploaded `Message-ID` then ends in the `From` address's
  domain. No new scope is needed: `gmail.modify` (and even
  `gmail.readonly`) allows the call. `--from` doesn't change who a
  `--reply-to` draft is addressed to.
- **Message-ID.** The uploaded id ends in the `--from` address's domain,
  or else the account's own domain (for example `<…@gmail.com>`), looked
  up with one `users.getProfile` call, which `gmail.readonly` allows,
  alongside any `--reply-to` lookup. If that call fails, the id ends in
  `@localhost` instead and a warning is printed once the draft has been
  created. **Gmail doesn't keep it:** every save, by `draft create` or
  [`draft update`](#updating-drafts), `--raw` included, gives the stored
  draft a `Message-ID` of Gmail's own (`<…@mail.gmail.com>`) and a new
  `Date`, so `draft show` returns those rather than the uploaded values.
  What id the message carries once it is sent from the Gmail UI is
  unverified ([#1953](https://github.com/rust-works/omni-dev/issues/1953)),
  which is why the uploaded one is still made valid.
- **Replies.** `--reply-to` takes the **Gmail message id** of the message
  being answered, as `gmail search`/`read` print it, not its `Message-ID`
  header. Gmail only files a reply into the original's thread when the
  draft's `threadId`, its `In-Reply-To`/`References` headers and its
  subject all line up. `draft create` fetches the original's headers (one
  `messages.get`) and sets all three. The subject defaults to
  `Re: <original subject>`, without doubling an existing `Re:`. An explicit
  `--subject` is compared with the original's once leading `Re:` prefixes
  are removed from both. If they differ, a warning is printed, because
  Gmail may start a new thread for it.
- **Reply recipients.** With `--reply-to` and no `--to`, the draft is
  addressed the way a mail client's Reply addresses it. The same
  `messages.get` also reads the original's `From`, `Reply-To`, `To` and
  `Cc`, so this costs no extra request.

  | Original message                      | Draft `To`                  |
  |---------------------------------------|-----------------------------|
  | From someone else, has `Reply-To`     | the `Reply-To` mailbox(es)  |
  | From someone else, no `Reply-To`      | `From`                      |
  | Sent by you (Gmail's `SENT` label)    | the original's `To`         |

  `--reply-all` (only with `--reply-to`) also adds the original's `To` to the
  draft's `To` and its `Cc` to the draft's `Cc`, keeping each recipient in
  the header they were in, as Gmail's web UI does. `Bcc` is never carried
  over.

  Your own address is left out of every defaulted header. The primary
  address comes from the `users.getProfile` call `draft create` already
  makes. `--reply-all` also leaves out your send-as aliases, found with one
  `users.settings.sendAs.list` call. If that call fails, the command fails
  rather than risk putting an alias in its own reply. A plain reply doesn't
  use that call, so it only recognises the primary address.

  An explicit `--to` or `--cc` **replaces** that header's default rather than
  adding to it, so you can drop someone. A defaulted header also leaves out
  anyone you already named in `--to`, `--cc` or `--bcc`, and repeats, both
  compared case-insensitively. So naming the original's sender in `--cc` or
  `--bcc` keeps them out of the defaulted `To`, and the draft can end up with
  no `To` at all; pass `--to` as well to keep them there. If leaving yourself
  out empties `To`, a defaulted `Cc` moves up into `To`, as mail clients do:
  replying to a message you sent yourself, copying others, goes to them. A
  reply to a note to self goes back to you, as in Gmail, without
  `--reply-all`'s extras. The command fails before creating anything, asking
  for `--to`, only when the draft would have no recipient at all. Once the
  draft exists, a `note: replying to …; cc …` line on stderr says who was
  defaulted. Standard output and `-o` output are unchanged.

  Addresses are decoded from the original's headers (RFC 2047 names,
  groups, quoted names, headers repeated or folded). One that can't be used,
  such as a name that decodes to a line break, is skipped, with a warning
  when it was in a header the reply drew on. Your addresses are matched as
  whole addresses, so Gmail's dot and `+tag` variants of an `@gmail.com`
  address (`j.doe+x@gmail.com` for `jdoe@gmail.com`) are **not** recognised
  as yours. `Mail-Followup-To`/`Mail-Reply-To` are ignored.
- **`--raw FILE`** uploads a complete RFC 5322 message byte for byte, with
  no parsing or line-ending changes (Gmail still replaces its `Message-ID`
  and `Date` on save). It can't be combined with any of the composing
  flags, `--from` included: the file's own `From` is uploaded unchecked.
- **Size.** Messages over Gmail's 35 MB per-message limit are refused
  before any request is sent. This is the same limit as [Insert](#insert).
  Attachments are checked on disk before they're read, with base64's growth
  of about a third counted along with the body. The built message is
  checked again, exactly, before the upload.
  The upload uses the same `/upload/` multipart transport as `gmail insert`.

**Needs `gmail.modify`.** `drafts.create` isn't allowed with
`gmail.readonly`. A read-only account gets an error telling it to re-run
`omni-dev gmail auth login --modify` (see
[`insufficientPermissions`](#insufficientpermissions)).

**Drafts are never sent or deleted.** omni-dev can only stage a draft for a
person to review. It deliberately has no `draft send` and no `draft delete`:
Gmail's `drafts.send` delivers mail that can't be recalled, and
`drafts.delete` skips Trash, so a deleted draft can't be recovered. Send
or discard drafts in Gmail itself. A unit test fails the build if either
endpoint is ever added to the drafts client (#1920).

### Updating drafts

```bash
$ omni-dev gmail draft update r-1234567890 --subject 'Quarterly report (final)'
$ omni-dev gmail draft update r-1234567890 --cc bob@example.com --cc carol@example.com
$ omni-dev gmail draft update r-1234567890 --body-file revised.txt --remove-attachment q3-draft.pdf \
    --attach q3.pdf
$ omni-dev gmail draft update r-1234567890 --html-body-file revised.html
$ omni-dev gmail draft update r-1234567890 --from sales@example.com
$ omni-dev gmail draft update r-1234567890 --raw draft.eml --if-message-id 18c2f0a1b2c3d4e5
```

`draft update` revises a staged draft and prints its `DRAFT_ID` (unchanged),
its new `MESSAGE_ID`, the `PREVIOUS_MESSAGE_ID` it replaced, and its
`THREAD_ID`. `-o yaml`/`json` give the same ids in snake case. At least one
edit is required. Nothing is sent.

Gmail's `drafts.update` has no partial form: every update replaces the whole
message. So `draft update` reads the stored message
(`drafts.get?format=raw`), changes only what the flags name, and uploads the result.
**Everything else is uploaded byte for byte**: `From` (unless `--from` is
given), `In-Reply-To`/`References`, any other header, the body, and every
attachment you didn't name. The upload also carries the stored `Date` and
`Message-ID` unchanged, but Gmail replaces both on every save, as it does
for `draft create`, so after an update `draft show` returns a new
`<…@mail.gmail.com>` `Message-ID` and the time of the update as `Date`.

- **Recipients and subject.** `--to`, `--cc`, `--bcc` and `--subject`
  replace just that header, encoded the way `draft create` encodes it. Each
  recipient flag replaces that header's **whole** list, so pass every
  recipient you want to keep, repeating the flag for each one (unlike
  `draft create`, one flag takes one value, so the draft id can follow it).
  `--subject ''` removes the subject. A recipient header can't be cleared
  yet.
- **From.** `--from ADDR` replaces `From` with one of the account's send-as
  addresses, checked and named exactly as in `draft create`. The check runs
  before the draft is read, so a refused address changes nothing and costs
  no draft reads. `--from` with your primary address and no name known for
  it removes `From`, so Gmail fills it in with your account name; otherwise
  there's no way to remove `From`.
- **Body.** `--body TEXT` or `--body-file PATH` replaces the body with plain
  text. There's no standard-input fallback, unlike `draft create`, because
  the body is optional here. A draft written in the Gmail UI also has an
  HTML version (and possibly inline images). That version is **dropped**,
  with a warning, rather than left contradicting the new text, since Gmail
  shows the HTML version when there is one.
- **HTML body.** `--html-body HTML` or `--html-body-file PATH` replaces the
  body with a `multipart/alternative` of plain text and that HTML, as in
  `draft create`. The plain-text part is `--body`/`--body-file` when given,
  and otherwise derived from the new HTML. It is never kept from the old
  body, where it could say something different. Replacing a body that had
  inline images drops them, with a warning. New HTML that refers to a
  `cid:` image gets the same warning as in `draft create`.
- **Attachments.** `--attach PATH` adds files after the existing
  attachments, with the same on-disk size check as `draft create`.
  `--remove-attachment NAME` removes one attachment: the one
  `draft show -o markdown` lists under `NAME` (sanitised, with `-1`, `-2`… added to repeated
  names and `attachment-N` for an unnamed part), or else the one stored
  under that exact filename, if only one is. Two attachments both called
  `image.png` are removed as `image.png` and `image-1.png`. A name the draft
  doesn't have is an error that lists the names it does have.
- **`--raw FILE`** replaces the whole message with an `.eml` file, uploaded
  byte for byte (Gmail then replaces its `Message-ID` and `Date`, as on
  every save). `draft show --detail raw --out-file` writes such a file.
  Use this for anything the flags can't express, such as inline images. It
  can't be combined with the editing flags.
- **Threads.** The draft's `threadId` is copied from the stored draft into
  every update, `--raw` included. Without it, a reply draft silently falls
  out of its thread. Gmail also threads by subject, so changing a reply
  draft's subject prints a warning. If Gmail files the result in a different
  thread anyway, a second warning names both thread ids.

**Concurrent edits.** Drafts have no ETag or precondition, so an update
always overwrites whatever is stored, including an edit made in the Gmail UI
after omni-dev read the draft. The draft's message id changes on every save,
which gives a cheap check. `draft update` reads the draft again
(`format=minimal`) just before the upload and refuses with `draft changed since it
was read` if the message id moved. `--if-message-id ID` extends the check
back to an earlier read: pass the `MESSAGE_ID` that `draft show` or `draft
list` printed, and the update is refused unless the draft still holds that
message. The window is **smaller, not closed**: a save that lands between
the final read and the upload is still overwritten.

**Needs `gmail.modify`**, like `draft create`, with the same actionable error
for a read-only account.

### MCP equivalent(s)

- `gmail_draft_list` mirrors `draft list`. It takes the same `query` and
  `limit` (default 50, `0` for every draft up to the 10,000 hard cap) and
  returns the same rows as `-o yaml`, with `draft_id`, `message_id` and
  `thread_id` named apart. Hydration concurrency is fixed at the CLI's
  default of 4. Neither surface has a knob for it.
- `gmail_draft_show` mirrors `draft show`. It takes a `draft_id` and the
  same `format` values as `gmail_message_read` (`minimal` / `metadata` /
  `full` / `raw`) and returns `drafts.get`'s `{id, message}` as YAML. A
  message id fails with the same `No draft with id` hint. `output_file`
  writes that YAML to disk and returns a short summary. Unlike
  `--detail raw --out-file draft.eml`, it never decodes the message to an
  `.eml`.

Both need only `gmail.readonly`. `draft create` and `draft update` stay
CLI-only: they would be the first Gmail MCP tools that need `gmail.modify`,
and their file inputs (`--attach`, `--body-file`, `--raw`) would let an MCP
client make the server read local files into the mailbox, so they get their
own review. `send` and `delete` are not offered on either surface.

## Sync

```bash
$ omni-dev gmail sync --output-dir ~/mail-archive
$ omni-dev gmail sync --output-dir ~/mail-archive --query 'label:finance'
$ omni-dev gmail sync --output-dir ~/mail-archive --full
$ omni-dev gmail sync --output-dir ~/mail-archive --dry-run
$ omni-dev gmail sync --output-dir ~/mail-archive --extract-attachments
$ omni-dev gmail sync --output-dir ~/mail-archive --exclude-label SPAM --exclude-label TRASH
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
  state.json                  # watermark (historyId) + account identity +
                                #   pending_fetch (ids that failed last run)
  manifest.jsonl               # one record per message: id, thread_id, label_ids,
                                #   internal_date, subject, from, to, rfc822_msgid,
                                #   in_reply_to, references, attachment_count,
                                #   attachment_filenames, path, size, history_id,
                                #   deleted_at (soft-deleted messages only),
                                #   excluded_labels (--exclude-label reason, only
                                #   for a record soft-deleted by run_incremental's
                                #   own precise tracking — see Sync below)
  messages/<year>/<month>/<day>/<id>.eml   # sharded by the message's internal_date
  messages/<year>/<month>/<day>/<id>/attachments/<filename>  # only with --extract-attachments
  insert-ledger.jsonl          # only after `gmail insert` has run — see Insert below
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
listing+fetching.) A run that hits per-item errors still advances the
watermark, but records the failed message ids in `state.json`'s
`pending_fetch`; the next incremental run retries them directly, alongside
the new history window, and drops any that are by then archived, deleted,
or vanished. This means a mailbox that keeps hitting errors (for example a
sustained `rateLimitExceeded`) never lets its watermark age past the
one-week limit and fall back to a full listing (#1784). One exception: a
message that vanishes from the server in the window between being listed
and being fetched (`messages.get` returns a 404 with reason `notFound`) is
not an error. It's recorded as a `Vanished` action instead and never added
to `pending_fetch`, since Gmail's `history.list` and `messages.get` aren't
perfectly consistent and retrying that particular id can never succeed;
see the Troubleshooting section below.

**`--query` and incremental sync (a known limitation):** `--query` scopes a
backfill/`--full`/reconciliation pass, but `history.list` has no query
filter, so an incremental run cannot re-apply it — newly-arrived mail that
would match your `--query` is only picked up by a later `--full` re-run. If
you sync a query-scoped subset of your mailbox regularly, plan on an
occasional `--full` pass.

**`--exclude-label` (label-based exclusion, #1780):** for the common case of
excluding spam/trash (or any other label) from the archive, prefer
`--exclude-label LABEL_ID` (repeatable, e.g. `--exclude-label SPAM
--exclude-label TRASH`) over `--query` — unlike `--query`, it's fully
general and takes effect on **incremental** syncs too, since it works off
the label data `history.list` events already carry for free rather than a
server-side query string: a new message with an excluded label is never
fetched, and a `labelsAdded`/`labelsRemoved` event that later moves an
already-archived message across the excluded boundary soft-deletes/
undeletes it. That undelete is deliberately conservative: the manifest
records *precisely which* configured label(s) caused a soft-delete only
when `run_incremental` itself made that call (from the message's real,
event-driven current label set) — a record soft-deleted for any other
reason (a real message deletion, or simply falling out of a `--full`
listing) never gets tagged, so an unrelated later label change can never be
mistaken for that exclusion having lifted and resurrect it.

On backfill/`--full`/reconciliation passes, only labels with a well-known
`-in:` query translation take effect this way — currently just
`SPAM`/`TRASH`, since Gmail's `label:`/`in:` search operators match a
label's *display name*, not its internal id, and most other system labels
use `is:` rather than `in:`; folding an arbitrary label id in reliably would
need an extra `labels.list` lookup. Any other `--exclude-label` value still
filters incremental runs (fully generally, by id), and a full/backfill pass
prints a `Note` explaining it wasn't excluded on that pass — add it to
`--query` yourself if you also want it gone from a `--full` re-run. A
message a full/backfill pass excludes by a custom label (tracked only via a
prior incremental run) is not casually resurrected just because it still
appears in a listing that couldn't have filtered that label out — only an
actual `labelsRemoved` event (via a later incremental run) or a
`--query`-assisted `--full` pass can undo it. Combining `--query` with
`--exclude-label` is safe with either operator: any of your own query's
`OR`-joined clauses is parenthesized before an exclusion clause is
appended, so the exclusion always applies to the query as a whole rather
than binding only to its last term.

One edge case, not solved: a message that arrives *already* carrying an
excluded label is never archived, so if that label is later removed
there's no manifest record for the `labelsRemoved` event to act on — it
self-heals on the next `--full`/reconciliation pass, same as this
section's other races.

**Header fields:** `subject`/`from`/`to`/`rfc822_msgid`/`in_reply_to`/
`references` in the manifest are parsed directly from the already-fetched
raw message bytes (no second network request), and are stored as their raw
wire encoding — non-ASCII subjects encoded per RFC 2047
(`=?UTF-8?B?...?=`) are **not** decoded to human-readable text in this
release. `in_reply_to`/`references` are what let a conversation be
reconstructed from the manifest alone, without re-parsing every `.eml`. To
read an individual archived message with its headers properly decoded, use
[`gmail render`](#render) against its `.eml` file (or `gmail read -o
markdown` for a live, not-yet-archived message) rather than the manifest.

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
messages already archived by an earlier run, even under `--full`; run
[`gmail extract-attachments`](#extract-attachments) with `--archive-dir`
pointed at the same directory instead — it extracts from the `.eml` files
already on disk, no re-fetch required.

**Report summary:** every report — table/text and `-o json`/`-o yaml`/
`-o yamls`/`-o jsonl` alike — ends with an at-a-glance tally, e.g.
`5,794 fetched, 30 deleted, 0 errors` in text output, or an explicit
`summary` field (`fetched`/`would_fetch`/`vanished`/`excluded`/
`labels_updated`/`deleted`/`undeleted`/`would_delete`/`would_undelete`/
`errors` counts) in the
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

## Sync all accounts

```bash
$ omni-dev gmail sync-all
$ omni-dev gmail sync-all --concurrency 10
$ omni-dev gmail sync-all --full --dry-run
$ omni-dev gmail sync-all -o json
```

Runs [`sync`](#sync) for every account listed in `.omni-dev/gmail-sync.yaml`,
concurrently, replacing a wrapper script that loops `gmail sync --account
...` over each mailbox one at a time. Each account keeps its own archive
and its own [rate limit](#rate-limits-and-retry-behaviour) budget — nothing
about a single account's sync changes, only that several now run at once.
See [ADR-0068](adrs/adr-0068.md) for the full design rationale.

**Config file:** `.omni-dev/gmail-sync.yaml`, discovered the same way as
every other `.omni-dev/` file (see
[docs/omni-dev-directory.md](omni-dev-directory.md#gmail-syncyaml)) — walk-up
from the current directory, a `local/` override, `--context-dir`/
`OMNI_DEV_CONFIG_DIR`:

```yaml
concurrency: 20
accounts:
  - account: jky.greens
    output_dir: emails/jky.greens/
  - account: newhoggy
    output_dir: emails/newhoggy/
    query: "-in:spam"
    exclude_labels: [SPAM, TRASH]
    extract_attachments: true
```

`account` must name an account already configured under
[Multiple accounts](#multiple-accounts) — `gmail-sync.yaml` says only
*which* accounts to sync and *where*, never a second credential store.
`output_dir` resolves relative to the project root (the parent of the
discovered `.omni-dev/`) unless absolute. Unlike every other `.omni-dev/`
config file, a missing, empty, or malformed `gmail-sync.yaml`, or one
naming an account `gmail account list` doesn't know about, is a hard error
before any network call is made — see
[docs/omni-dev-directory.md's Validation behaviour](omni-dev-directory.md#gmail-syncyaml-1).

**`--account` is incompatible with `sync-all`:** the global `--account`/
`OMNI_DEV_GMAIL_ACCOUNT` selector picks one mailbox; `sync-all` always
targets the whole `gmail-sync.yaml` list, so passing both is a hard error
rather than a silent no-op or an ignored flag.

**Concurrency:** two independent caps compose. Each account's own fetch
fan-out is still bounded by the same local concurrency `gmail sync` itself
uses — unaffected by this command. A second, *shared* cap —
`sync-all --concurrency` if given, else `gmail-sync.yaml`'s top-level
`concurrency`, else the same default as `gmail sync --concurrency` — bounds
how many fetch requests are in flight *across every account combined* at
once, so one account can never claim the whole shared budget for itself.
Each account still paces its own requests against its own Gmail quota
independently (see [Rate limits](#rate-limits-and-retry-behaviour) below) —
the shared cap is a purely local resource limit, unrelated to quota
compliance.

**Progress and output:** on the same interactive-terminal condition a single
`gmail sync` uses (`-o table`, not `--quiet`, a `stderr` that's actually a
tty), every account gets its own listing spinner + fetch bar, all registered
on one shared `MultiProgress` — a single shared renderer, rather than each
account's bars fighting another's over the same terminal, is what lets them
all advance concurrently and stay legible. Independent of the bars, a
one-line summary still prints per account as soon as that account finishes
(not only once every account is done), e.g. `jky.greens: 42 fetched, 0
errors`, followed by a trailing `combined: ...` total once every account has
finished — that line prints through the bars (via `suspend`) rather than
racing their redraw. `--quiet` suppresses both the live bars and the
per-account summary lines; the combined total and any per-message error
lines always print regardless. `-o json`/`-o yaml`/`-o yamls`/`-o jsonl`
instead emit one structured record per account (`account`, `actions`,
`errors`, `summary`, and — only for an account whose task failed before
producing a report at all, e.g. bad credentials or a rejected output
directory — `account_error`) plus a `combined_summary`, once every account
has finished — the same `summary` shape [Sync](#sync)'s own **Report
summary** describes.

**Exit code:** non-zero if *any* configured account either failed outright
(bad credentials, a rejected output directory, …) or reported one or more
per-message errors — one account's failure is never silently swallowed
because the others succeeded. Every other account still runs to completion
regardless of an earlier one's failure.

No MCP equivalent — same reasoning as `sync` itself, doubled: a bulk,
potentially long-running filesystem operation across several mailboxes at
once is an even poorer fit for a synchronous MCP tool call.

## Extract attachments

```bash
$ omni-dev gmail extract-attachments --archive-dir ~/mail-archive
$ omni-dev gmail extract-attachments --archive-dir ~/mail-archive --dry-run
$ omni-dev gmail extract-attachments --archive-dir ~/mail-archive -o json
```

Retroactively extracts attachments for messages [`sync`](#sync)/[`sync-all`](#sync-all-accounts)
already archived, without contacting Gmail at all — the fix for
[`--extract-attachments`](#sync)'s "no retroactive backfill" limitation
(see [ADR-0065](adrs/adr-0065.md)). Purely local and fast: it reads the
manifest and `.eml` files already under `--archive-dir` and never resolves
a client, so no credentials, `--account`, or network access are needed —
unlike every other `gmail` subcommand. Named `--archive-dir` rather than
`sync`'s `--output-dir` since this command's primary interaction with the
directory is reading an existing archive, not producing one — it happens
to also write new `attachments/` subdirectories into it, but that's
incidental to what the flag names.

For each message in the manifest, it trusts `attachment_count > 0` (the
same cheap heuristic scan `sync` always runs, regardless of whether
`--extract-attachments` was ever passed — see [Sync](#sync)'s Attachments
paragraph) as a fast-path filter, skipping the rest without opening their
`.eml`. A candidate whose `messages/<year>/<month>/<day>/<id>/attachments/`
directory already exists is skipped too — the same presence-on-disk
idempotence `sync` itself relies on, which is what makes this command safe
to re-run at any time to pick up whatever an earlier run missed (including
a partial/interrupted one). Everything else is read from disk, parsed with
the same real MIME parser `sync --extract-attachments` uses, and written
out identically. A message whose real parse finds nothing — the rare
heuristic/parser disagreement ADR-0065 documents — is silently skipped,
not an error; a missing or unreadable `.eml` is recorded as a per-message
error and the run continues with the rest of the archive.

**`--dry-run`** parses every candidate `.eml` (so its reported counts are
accurate, not just an echo of the heuristic) and reports what it would
extract without writing any file.

**Report summary:** mirrors [Sync](#sync)'s — a trailing `N extracted, N
would extract, N errors` tally in text output, or a `summary` field in the
structured formats, with the full per-action listing always included in
`-o json`/`-o yaml`/`-o yamls`/`-o jsonl`.

No MCP equivalent — a bulk filesystem operation is as poor a fit here as
it is for `sync` itself.

## Render

```bash
$ omni-dev gmail render message.eml
$ omni-dev gmail render messages/2026/01/*/*/*.eml
$ omni-dev gmail render message.eml --out-dir rendered/
$ omni-dev gmail render *.eml -o json
$ omni-dev gmail render message.eml --fold-quotes
$ omni-dev gmail render --archive-dir archive/ --all --out-dir rendered/
```

Renders one or more `.eml` files as human-readable Markdown: a header
block (Subject/From/To/Cc/Date/Message-Id/In-Reply-To/References, RFC
2047-decoded), the body (`text/plain` preferred, `text/html` converted to
Markdown otherwise), and an attachment filename list (listed, never
embedded — this is a readable rendering, not an export). Purely local and
fast, like [Extract attachments](#extract-attachments): it never resolves
a client, so no credentials, `--account`, or network access are needed.

By default `render` takes bare file paths, with no dependency on the
mailbox having been synced by this tool at all — this works equally well
piped a glob from a `gmail sync` archive
(`messages/<year>/<month>/<day>/*.eml`) or any other `.eml` file, from
anywhere. The same rendering function backs `gmail read -o markdown`; see
[Messages](#messages).

**`--archive-dir PATH --all`** is the alternative for rendering an entire
synced archive: it reads `PATH`'s `manifest.jsonl` and renders every
non-deleted message, in place of gathering paths yourself. `--all` is
required alongside `--archive-dir` (rather than `--archive-dir` alone
implying it), reserving room for a future non-`--all` selector; the two
are mutually exclusive with positional `PATH` arguments. Paired with
`--out-dir`, a message whose `.md` file already exists there is silently
skipped, mirroring [`extract-attachments`](#extract-attachments)'s own
presence-on-disk idempotence for `attachments/` dirs — so re-running
against a growing archive only renders what's new.

By default (no `--out-dir`), each input's rendered Markdown is printed
directly to stdout — with more than one input, successive renderings are
separated by a `---` thematic break — so `omni-dev gmail render *.eml >
combined.md` produces clean, redirectable Markdown as long as every input
renders successfully. **`--out-dir DIR`** instead writes one `.md` file
per input into `DIR` (named after the input's stem, e.g. `abc123.eml` ->
`abc123.md`; `DIR` is created if missing), printing a `Saved to:` line per
file instead.

A per-file read/parse/write failure (a missing path, a permission error)
is recorded against that file rather than aborting the run — the rest of
the batch still renders — but the command still exits non-zero if any
file failed. An unparseable message degrades to a short placeholder rather
than failing outright, the same posture
[`extract-attachments`](#extract-attachments) takes for a message whose
real MIME parse disagrees with the cheap heuristic.

`-o json`/`-o yaml`/`-o yamls`/`-o jsonl` emit one structured record per
input (`path`, and either `markdown` or `saved_to`, plus `error` for a
failed file) instead of the Table view above.

**`--fold-quotes`** collapses deeply-nested `>`-quoted reply history in
each rendered body — see [Messages](#messages)'s `-o markdown` section for
the full description; the behavior is identical since both call the same
rendering function.

No MCP equivalent — same reasoning as
[Extract attachments](#extract-attachments).

## Insert

```bash
$ omni-dev gmail insert --archive-dir ~/mail-archive --all --label RESTORED --dry-run
$ omni-dev gmail insert --archive-dir ~/mail-archive --all --label RESTORED
$ omni-dev gmail insert --archive-dir ~/mail-archive --since 2020-01-01 --until 2020-12-31 --label RESTORED
$ omni-dev gmail insert --archive-dir ~/mail-archive --all --label RESTORED --drop-label INBOX --drop-label UNREAD
$ omni-dev gmail insert --archive-dir ~/mail-archive --all --label RESTORED --verify-remote
```

Restores archived `.eml` messages into a mailbox via `messages.insert`,
closing the loop the rest of this page describes: [`sync`](#sync)/
[`sync-all`](#sync-all-accounts) capture, [`render`](#render)/
[`extract-attachments`](#extract-attachments) read the archive back, and
`insert` restores it — making the archive a genuine backup rather than a
read-only copy. Unlike `extract-attachments`/`render`, this contacts Gmail
and needs a client, so `--account` works normally.

Uses `messages.insert`, not `messages.import` — `import` runs a message
through Gmail's normal spam/classification pipeline, which is actively
wrong for mail that may be years old (it can land in spam, or be caught by
filters written long after the mail actually arrived). `insert` places the
message exactly where instructed, with `internalDateSource=dateHeader` (not
Gmail's default) so the archived `Date:` header — not the moment of
insertion — sets Gmail's sort order. No additional OAuth scope is needed;
`gmail.modify` already covers it.

**Selection** — one of `--all`, `--since DATE`/`--until DATE`
(`YYYY-MM-DD`, inclusive), `--id ID` (repeatable), `--ids-from FILE` (one id
per line, `-` for stdin, blank lines and `#` comments skipped — the same
shape a `--dry-run` report's ids can be piped back into), or
`--source-label LABEL_ID` (a raw archived label id, distinct from the
destination `--label` below) is required — a bare `gmail insert
--archive-dir DIR` is rejected rather than silently restoring the whole
archive. Combining selectors narrows rather than widens the match set
(`--id` plus `--since` requires both); `--all` combined with anything else
is rejected outright, since it already means "everything." This selection
logic is local-only, filtering the already-synced `manifest.jsonl` — it is
**not** Gmail's search query syntax.

**`--label NAME`** tags every inserted message with an existing destination
label, resolved by name — never auto-created, so a typo fails before any
write rather than silently creating a stray label. It matters more here
than it might sound: inserted mail's raw headers still name the *original*
recipient, so `to:` searches on the destination account won't match it, and
this tag becomes the only reliable handle for "what came from the archive."

**Label replay:** an archived message's Gmail system labels
(`INBOX`/`SENT`/`UNREAD`/`STARRED`/`IMPORTANT`/`SPAM`/`TRASH`/`CATEGORY_*`)
replay onto the inserted copy automatically — this is what preserves
**sent mail** for free: `messages.batchModify` ([Labels](#labels)) forbids
adding `SENT`, so `insert` is the only mechanism able to restore mail the
source mailbox sent rather than received. `DRAFT` never replays (it would
create a Drafts row with no backing Draft resource), and any *user* label
from the source mailbox is dropped (it's foreign to the destination, or
worse, collides with an unrelated label there). `--drop-label LABEL_ID`
(repeatable) strips a label after that filter — most commonly
`--drop-label INBOX --drop-label UNREAD`, which restores mail as
already-read and archived instead of dumping it into a live Inbox. Before
the first request (including under `--dry-run`), the run reports how many
selected messages will land in INBOX/UNREAD, and separately how many will
land in TRASH/SPAM — Gmail auto-purges Trash after 30 days, so a "restore"
that silently lands 400 messages there quietly destroys them a month later.

**Idempotency:** a local `insert-ledger.jsonl` (a sibling of
`manifest.jsonl` — see [Sync](#sync)'s Archive layout) records every
message this command has already accounted for against a destination
mailbox, keyed by **(destination address, archived Message-ID)** — Gmail
assigns a brand-new id on every insert, so unlike `sync`/
`extract-attachments`, the source manifest's own id can't serve as the
presence check. The destination address is part of the key, not incidental
metadata: consolidating a legacy mailbox into a *different* current one is
the primary use case, and a ledger scoped only by Message-ID would make a
completed restore into account A silently suppress every insert into
account B. Re-running the same selection inserts nothing the second time;
an interrupted run resumes and only inserts what the ledger doesn't already
have. **`--verify-remote`** adds a pre-insert `rfc822msgid:` probe against
the destination itself — useful for a first run into a mailbox that may
already hold some of this mail, or as a recovery path after losing the
ledger — but is a supplement to the ledger, not a substitute: the probe
costs extra quota and a round-trip per message, and Gmail's search index
can lag a real insert by seconds to minutes.

**`--dry-run`** does two read-only preflight calls (destination identity,
label resolution) and reports exactly what would be inserted, with the
INBOX/TRASH counts above — no `messages.insert` call is made and no ledger
file is written.

**Concurrency and rate limits:** default `--concurrency` is **4**, well
below `sync`'s default of 20 — `messages.insert` costs 5x a `messages.get`
against the same quota bucket, so a wider fan-out buys no extra throughput
here and only widens how many messages could be inserted-but-unledgered if
the process crashes mid-run (see the note below). See
[Rate limits and retry behaviour](#rate-limits-and-retry-behaviour) for the
shared retry/backoff mechanics.

**Report summary:** mirrors [Sync](#sync)'s — a trailing `N inserted, N
skipped, N errors` tally in text output (`N would insert` under
`--dry-run`), or a `summary` field in the structured formats, with the full
per-action listing (including informational `Note`s) always included in
`-o json`/`-o yaml`/`-o yamls`/`-o jsonl`. A per-message failure (a
malformed `.eml`, a transient API error) is recorded and the run continues
with the rest of the batch; the command exits non-zero if any message
failed.

**Known limitation:** the ledger is written only after each message's
successful response, so a crash mid-batch can leave a handful of messages
inserted on Gmail's side but not yet recorded locally — a following run
would then insert them again. The alternative (recording *before* sending)
trades a rare, visible, deletable duplicate for a silent, undetectable gap
in restored mail, which is strictly worse for a backup-restore tool.
Recovery is re-running with `--verify-remote`.

**Cleaning up a test/live-verification pass:** the ledger's `inserted_id`
field doubles as an undo list — since `messages.batchModify` has no
client-side scope guard and `TRASH` is addable (only `SENT`/`DRAFT` are
rejected by the API), the *existing* [`gmail label add`](#labels) command
is enough to trash everything a run inserted into a given destination:

```bash
$ jq -r 'select(.destination=="throwaway@example.com") | .inserted_id' \
    ~/mail-archive/insert-ledger.jsonl | xargs omni-dev gmail label add --label TRASH
```

No MCP equivalent — a bulk, mutating, potentially long-running operation is
as poor a fit here as it is for `sync`/`extract-attachments`.

## Rate limits and retry behaviour

Gmail enforces a **per-user quota of 250 units/second**; `messages.get` and
`messages.list` each cost 5 units, `messages.batchModify` costs 50 units
for up to 1000 ids. Gmail doesn't document a separate cost for
`drafts.create`, so assume the `messages.insert` cost (25 units) until it's
verified. `draft create` makes one such call and one `users.getProfile`
(1 unit), plus one `messages.get` with `--reply-to` and one
`users.settings.sendAs.list` (1 unit) with `--reply-all` or `--from`.
`draft update` makes one `drafts.update` (assume the same 25 units) plus two
`drafts.get` calls, or one with `--raw`, and one `users.settings.sendAs.list`
with `--from`. `gmail search`'s ids-only default costs a flat
5 units regardless of `--limit` (auto-pagination is still one `messages.list` call
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
retry — see [Sync](#sync) above. `search --enrich`/`thread`/`draft list` still rely on
`--concurrency` (or `draft list`'s fixed bound of 4) alone (a concurrency bound, not a rate limiter) plus this
retry driver as their only quota protection.

The list endpoints (`search`, `draft list`, `thread`'s underlying calls) auto-paginate
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

`gmail.readonly` was granted, but `label add`/`remove`, `insert`,
`draft create` or `draft update` fails — read commands (`search`, `read`, `thread`,
`draft list`, `auth status`) all work fine; only mailbox writes 403. Fix is
`omni-dev gmail auth login --modify` (re-consent with the write scope), not
a retry. `draft create`, `draft update` and `label add`/`remove` say so themselves (`insert`
still shows the bare 403):

```
Error: This Gmail account is authorised read-only, and this command needs the `gmail.modify` scope. Re-run `omni-dev gmail auth login --modify` (adding `--account NAME` for a named account) to grant it.
  Caused by: Gmail API request failed: HTTP 403: …
```

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

`sync` is safe to just re-run: a run with errors records the failed ids in
`state.json`'s `pending_fetch`, and presence-on-disk means already-archived
messages are skipped, so a re-run only retries what failed. Two ways to
make it succeed:

- Lower `--concurrency` (even down to `1`) so each large download gets
  more of the available bandwidth to itself.
- Raise the read timeout instead via `OMNI_DEV_HTTP_READ_TIMEOUT_SECS`
  (whole seconds; a missing, non-numeric, or non-positive value falls back
  to the 120-second default) — shared by the Gmail, Atlassian, and Datadog
  REST clients, e.g.
  `OMNI_DEV_HTTP_READ_TIMEOUT_SECS=300 omni-dev gmail sync ...`. The
  connect timeout has its own override, `OMNI_DEV_HTTP_CONNECT_TIMEOUT_SECS`
  (default 10s), for the unrelated case of a slow-to-establish connection.

### `Vanished <id>` in a `sync` report

```
Vanished m1a2b3c4 (message no longer existed on the server; skipped, not an error)
```

This is expected and benign, not something to fix by re-running: Gmail's
`history.list` and `messages.get` aren't perfectly consistent, so a message
can be permanently deleted from the server in the window between being
listed and being fetched — auto-filtered mail, a sent message recalled
immediately, and similar routine churn. Unlike every other per-item
failure, this can never succeed on retry, so it is not counted as an error
(`report.errors` stays empty), is not added to `pending_fetch`, and does
not fail `sync`'s or `sync-all`'s exit code. A message missing for any
*other* reason (a 404 with a different `reason`, or any non-404 failure)
still surfaces as an ordinary error and is retried on the next run. See
[ADR-0064](adrs/adr-0064.md)'s 2026-08-06 amendment for #1509.

## See also

- [Gmail Quickstart](gmail-quickstart.md) — a linear, zero-to-synced-archive
  walkthrough for first-time setup.
- [Drive Integration](drive.md) — the sibling Google integration; shares
  the same named-account/OAuth2 storage pattern.
- [User Guide](user-guide.md#gmail-integration) — short reference; primary
  content lives here.
- [MCP Reference — Gmail](mcp.md#gmail-8-tools) — parameter-only listing of
  all 8 `gmail_*` MCP tools.
- [ADR-0063](adrs/adr-0063.md) — OAuth2 authorization-code + PKCE design,
  refresh-token-only persistence, and the bring-your-own Google Cloud
  project rationale.
- [ADR-0066](adrs/adr-0066.md) — the named-account store behind
  [Multiple accounts](#multiple-accounts), and why it's orthogonal to
  `--profile`.
- [ADR-0068](adrs/adr-0068.md) — the `gmail-sync.yaml` config file and
  shared-semaphore concurrency model behind
  [Sync all accounts](#sync-all-accounts).
- [Gmail API documentation](https://developers.google.com/workspace/gmail/api/reference/rest) — upstream reference.
