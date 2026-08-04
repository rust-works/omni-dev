# Gmail Quickstart

This guide takes you from "I have a Gmail account" to "I have a local,
greppable archive of my mail" in one linear pass. It's the fastest path
through the setup; [gmail.md](gmail.md) is the topic-by-topic reference for
everything it glosses over.

## What you'll do

1. Create a Google Cloud project and enable the Gmail API.
2. Create an OAuth2 client and export its credentials.
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
uses below requires it. Note the **Client ID** and **Client secret** it
gives you, then export them:

```bash
export GMAIL_CLIENT_ID=...
export GMAIL_CLIENT_SECRET=...
```

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

This is the step that actually catches new users: leaving it unticked lets
login *succeed* and write a valid refresh token, while granting only
`openid`/`email`/`profile` — no Gmail access at all. Nothing in the login
output tells you this happened; every Gmail call you make afterward will
simply fail with `insufficientPermissions`. That's exactly what step 4
below is for.

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
`users.getProfile` call, so it's the only step in this whole walkthrough
that fails loudly if step 3's tick-box was missed. If it errors with
`insufficientPermissions`, don't retry `sync` — go back to step 3, re-run
`auth login`, and tick the box this time (see
[gmail.md#insufficientpermissions](gmail.md#insufficientpermissions) for
the full diagnosis).

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

- `insufficientPermissions` on every Gmail call, including `auth status` →
  [gmail.md#insufficientpermissions](gmail.md#insufficientpermissions) —
  you missed the tick-box in step 3.
- `insufficientPermissions` only on `label add`/`remove` →
  [gmail.md#insufficientpermissions](gmail.md#insufficientpermissions) —
  re-run `auth login --modify`.
- `invalid_grant` →
  [gmail.md#invalid_grant](gmail.md#invalid_grant) — testing-mode refresh
  token expired after 7 days.
- Credentials not configured →
  [gmail.md#credentials-not-configured](gmail.md#credentials-not-configured).
