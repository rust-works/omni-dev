# Gmail Quickstart

This guide takes you from "I have a Gmail account" to "I have a local,
greppable archive of my mail" in one linear pass. It's the fastest path
through the setup; [gmail.md](gmail.md) is the topic-by-topic reference for
everything it glosses over.

## What you'll do

1. Create a Google Cloud project and enable the Gmail API.
2. Create an OAuth2 client and import its credentials.
3. Log in — and correctly grant the Gmail permission, which Google hides
   behind its own tick-box.
4. Verify the grant actually worked, before touching `sync`.
5. Dry-run, then run, your first archive sync.
6. Extract attachments and confirm what landed on disk.

## Prerequisites

- A Gmail account.
- A browser you can complete an OAuth consent flow in.
- 10 minutes, plus however long your first sync takes (see
  [gmail.md#sync](gmail.md#sync) for timing).

## 1. Create a Google Cloud project and enable the Gmail API

Gmail read scopes are Google **restricted scopes**, so you bring your own
OAuth2 client rather than using a shared one — see
[gmail.md#prerequisites](gmail.md#prerequisites) for why. In the
[Google Cloud console](https://console.cloud.google.com/):

1. Create (or reuse) a project.
2. Enable the **Gmail API** for it.

## 2. Create an OAuth2 client

Still in Google Cloud console, create an OAuth2 client of type **Desktop
app** — not "Web application"; the loopback-redirect flow `auth login`
uses below requires it. Download its `client_secret.json`, then import it
directly — the client id/secret are saved to `settings.json` without ever
passing through your shell:

```bash
omni-dev gmail auth import
```

(Discovery finds the file automatically if it's still in `~/Downloads`;
pass an explicit path otherwise.) If you'd rather not save the file,
skip this and `auth login` in step 3 will prompt for the client id/secret
interactively instead.

If your OAuth client's consent screen is still in Google's **Testing**
publishing status (the default for a freshly created client), refresh
tokens expire after 7 days and you'll need to re-run `auth login` weekly
until you push it to **In production** — see
[gmail.md#prerequisites](gmail.md#prerequisites) for details. No Google
verification review is required below 100 test users.

## 3. Log in — and tick the Gmail permission

```bash
omni-dev gmail auth login
```

This opens a browser to Google's consent screen. **The Gmail permission is
its own separate tick-box there**, distinct from the basic
profile/email checkboxes the screen also shows. **Explicitly tick it.**

This is the step that actually catches new users: leaving it unticked
makes `auth login` fail immediately with an error naming the scopes
Google actually granted (`openid`, `email`, `profile` — no Gmail access
at all), and nothing is written to `settings.json`. Tick the box and
re-run the command.

Pass `--modify` instead if you also want to manage labels
(`gmail label add`/`remove`) — not needed for sync:

```bash
omni-dev gmail auth login --modify
```

## 4. Verify — before you attempt a sync

```bash
omni-dev gmail auth status
```

Expect something like:

```
Checking Gmail authentication...
Authenticated as: user@example.com
Messages in mailbox: 5842
Granted scope: gmail.readonly
```

**Treat this as a gate, not a formality.** It makes a live
`users.getProfile` call, so it catches anything a successful `auth login`
can't — a revoked grant, a stale refresh token, or (if you're planning to
use `label add`/`remove`) a `gmail.readonly`-only scope that needs
`auth login --modify` instead. If it errors, don't retry `sync` — fix
what it reports first (see
[gmail.md#insufficientpermissions](gmail.md#insufficientpermissions) for
the label-scope case).

If this succeeds, everything downstream will work — you're clear to sync.

## 5. Dry-run your first sync

Pick a small slice of your mailbox first (a label, a date range) rather
than your whole account, so a first pass finishes in seconds and you can
sanity-check the output before committing to a full run:

```bash
omni-dev gmail sync --output-dir ~/mail-archive --query 'label:finance' --dry-run
```

`--dry-run` reports every action `sync` *would* take without writing a
single file — not even `state.json`. Check the counts look sane for the
query you chose.

## 6. Run it for real, with attachments extracted

```bash
omni-dev gmail sync --output-dir ~/mail-archive --query 'label:finance' --extract-attachments
```

Drop `--query` to archive the whole mailbox instead. A first sync of a
several-thousand-message mailbox takes minutes, not seconds — see
[gmail.md#sync](gmail.md#sync) for real-world throughput figures. A re-run
against an already-synced mailbox is fast (typically one `history.list`
call), so it's safe to run this command again later to pick up new mail.

`--extract-attachments` writes each message's attachment MIME parts to
disk as separate files, alongside the `.eml` they came from. Without it,
attachments still exist — just inline inside the `.eml`, not as standalone
files.

## 7. See what you've got

```bash
ls ~/mail-archive
cat ~/mail-archive/manifest.jsonl | head -1
```

You now have `state.json` (the sync watermark), `manifest.jsonl` (one JSON
record per message — id, subject, from/to, attachment info, and more),
and `messages/<year>/<month>/<day>/<id>.eml` files, with a sibling
`attachments/` directory per message that had any. Find which messages
have attachments straight from the manifest:

```bash
grep -c '"attachment_count":[1-9]' ~/mail-archive/manifest.jsonl
```

The full archive layout, manifest schema, and incremental-sync semantics
are documented in [gmail.md#sync](gmail.md#sync).

## Where to go next

- **Full Gmail reference** — [gmail.md](gmail.md): every subcommand
  (search, read, threads, labels), rate limits, and troubleshooting.
- **User Guide overview** —
  [user-guide.md#gmail-integration](user-guide.md#gmail-integration).
- **MCP tools** — every read-only Gmail subcommand has a matching
  `gmail_*` MCP tool: [mcp.md#gmail-5-tools](mcp.md#gmail-5-tools).

## Troubleshooting quick links

- `auth login` fails with "Google did not grant a Gmail scope" →
  [gmail.md#no-gmail-scope-was-granted](gmail.md#no-gmail-scope-was-granted)
  — you missed the tick-box in step 3.
- `insufficientPermissions` only on `label add`/`remove` →
  [gmail.md#insufficientpermissions](gmail.md#insufficientpermissions) —
  re-run `auth login --modify`.
- `invalid_grant` →
  [gmail.md#invalid_grant](gmail.md#invalid_grant) — testing-mode refresh
  token expired after 7 days.
- Credentials not configured →
  [gmail.md#credentials-not-configured](gmail.md#credentials-not-configured).
