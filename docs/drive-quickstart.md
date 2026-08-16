# Drive Quickstart

This guide takes you from "I have a Google Drive account" to "I've searched
it and read a file from the command line" in one linear pass. It's the
fastest path through the setup; [drive.md](drive.md) is the topic-by-topic
reference for everything it glosses over.

## What you'll do

1. Create a Google Cloud project and enable the Drive API.
2. Create an OAuth2 client.
3. Log in — and correctly grant the Drive permission, which Google hides
   behind its own tick-box.
4. Verify the grant actually worked.
5. Run your first search and read a file's content.

## Prerequisites

- A Google account with some files in Drive.
- A browser you can complete an OAuth consent flow in.
- 5 minutes.

## 1. Create a Google Cloud project and enable the Drive API

Drive's read scope is a Google **restricted scope**, so you bring your own
OAuth2 client rather than using a shared one — see
[drive.md#prerequisites](drive.md#prerequisites) for why. In the
[Google Cloud console](https://console.cloud.google.com/):

1. Create (or reuse) a project.
2. Enable the **Google Drive API** for it.

A second, Drive-only project/OAuth client is fine, and so is reusing the
same project you already set up for [Gmail](gmail-quickstart.md) with both
APIs enabled — the two features' credential stores are fully independent
either way (see [ADR-0069](adrs/adr-0069.md)). This is your choice, not a
constraint omni-dev imposes.

## 2. Create an OAuth2 client

Still in Google Cloud console, create an OAuth2 client of type **Desktop
app** — not "Web application"; the loopback-redirect flow `auth login`
uses below requires it. Note its **Client ID** and **Client secret**.

Unlike Gmail, there's no `client_secret.json`-import command for Drive —
you'll either set `DRIVE_CLIENT_ID`/`DRIVE_CLIENT_SECRET` as environment
variables before logging in, or just let `auth login` prompt for them
interactively in the next step (the client id echoes normally, the secret
does not).

If your OAuth client's consent screen is still in Google's **Testing**
publishing status (the default for a freshly created client), refresh
tokens expire after 7 days and you'll need to re-run `auth login` weekly
until you push it to **In production** — see
[drive.md#prerequisites](drive.md#prerequisites) for details. No Google
verification review is required below 100 test users.

## 3. Log in — and tick the Drive permission

```bash
omni-dev drive auth login
```

This opens a browser to Google's consent screen. **The Drive permission is
its own separate tick-box there**, distinct from the basic profile/email
checkboxes the screen also shows. **Explicitly tick it.**

This is the step that actually catches new users: leaving it unticked
makes `auth login` fail immediately with an error naming the scopes
Google actually granted (`openid`, `email`, `profile` — no Drive access
at all), and nothing is written to `settings.json`. Tick the box and
re-run the command.

## 4. Verify

```bash
omni-dev drive auth status
```

Expect something like:

```
Checking Drive authentication...
Authenticated as: user@example.com
Granted scope: https://www.googleapis.com/auth/drive.readonly
```

**Treat this as a gate, not a formality.** It makes a live `about.get`
call, so it catches anything a successful `auth login` can't — a revoked
grant or a stale refresh token. If it errors, fix what it reports before
moving on (see
[drive.md#troubleshooting](drive.md#troubleshooting)).

If this succeeds, everything downstream will work.

## 5. Search and read your first file

Search for something you know is in your Drive:

```bash
omni-dev drive search "name contains 'report'"
```

```
ID                                   NAME              MIMETYPE                MODIFIED                  SIZE
1AbCdEfGhIjKlMnOpQrStUvWxYz          Q1 report.pdf     application/pdf         2026-02-01T09:14:00.000Z  184320
```

Read that file's metadata:

```bash
omni-dev drive read 1AbCdEfGhIjKlMnOpQrStUvWxYz
```

Then fetch its actual content. For a regular file (like the PDF above),
save it to disk:

```bash
omni-dev drive read 1AbCdEfGhIjKlMnOpQrStUvWxYz --content --out-file report.pdf
```

For a Google Doc, Sheet, or Slides file instead, `--content` exports it —
Docs become Markdown, Sheets become CSV, Slides become plain text, by
default:

```bash
omni-dev drive read <google-doc-id> --content
```

The full flag reference — including size caps and folder/shortcut
handling — is in [drive.md#read](drive.md#read).

## Where to go next

- **Full Drive reference** — [drive.md](drive.md): every subcommand
  (search, read, accounts), rate limits, and troubleshooting.
- **Multiple accounts** — a second Drive account (personal + work, say)
  doesn't need a second `--profile`: see
  [drive.md#multiple-accounts](drive.md#multiple-accounts).
- **MCP tools** — planned but not yet available; tracked by
  [issue #1525](https://github.com/rust-works/omni-dev/issues/1525).

## Troubleshooting quick links

- `auth login` fails with "Google did not grant the drive.readonly scope" →
  [drive.md#no-drive-scope-was-granted](drive.md#no-drive-scope-was-granted)
  — you missed the tick-box in step 3.
- `invalid_grant` →
  [drive.md#invalid_grant](drive.md#invalid_grant) — most often a
  testing-mode refresh token that expired after 7 days.
- Credentials not configured →
  [drive.md#credentials-not-configured](drive.md#credentials-not-configured).
