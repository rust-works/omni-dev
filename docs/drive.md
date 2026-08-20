# Drive Integration

omni-dev exposes access to the Google Drive v3 API through the `omni-dev
drive` command tree — search, read a file's metadata or content, find
duplicates, rename a file, and move it between folders. `drive.readonly`
(the default scope) is enough for search/read/dedupe; rename/move need the
opt-in `drive.metadata` scope
(`drive auth login --write`), the narrowest write scope Google offers — it
covers `files.update` on `name`/`parents` only, with no file-content access
at all. There is still no upload/create/trash/share/permission-mutation
capability anywhere in this surface.

**Move is security-gated.** Moving a file can change who can see it — Drive
resolves a file's effective visibility from both direct permissions on the
file and permissions inherited from its parent folder chain, and moving a
file changes that chain. `drive move` refuses any move that would change
visibility **by default**; three independent `--allow-*` flags opt in. See
[Move](#move) and [ADR-0070](adrs/adr-0070.md) for the full design.

The MCP tool surface (`drive_auth_status`/`drive_search`/`drive_dedupe`/
`drive_file_read`/`drive_account_list`, mirroring the CLI one-for-one like
Gmail's `gmail_*` tools) is read-only, like the rest of the MCP surface —
`rename`/`move` have no MCP equivalent. See
[docs/mcp.md](mcp.md#drive-5-tools) for the full tool reference.

New to this integration? Follow the
[Drive Quickstart](drive-quickstart.md) for a linear, zero-to-first-search
walkthrough — this page is the topic-by-topic reference.

## Table of Contents

1. [Prerequisites](#prerequisites)
2. [Authentication](#authentication)
3. [Multiple accounts](#multiple-accounts)
4. [Output formats](#output-formats)
5. [Search](#search)
6. [Read](#read)
7. [Duplicate detection](#duplicate-detection)
8. [Rename](#rename)
9. [Move](#move)
10. [Rate limits and retry behaviour](#rate-limits-and-retry-behaviour)
11. [Troubleshooting](#troubleshooting)
12. [See also](#see-also)

## Prerequisites

`drive.readonly` is a Google **restricted scope** — an application
distributed to third parties that requests it must pass a Google CASA
security assessment with annual recertification. omni-dev doesn't carry
that burden, so **each user creates their own Google Cloud OAuth2 client**
— the same model as [Gmail](gmail.md#prerequisites):

1. Create (or reuse) a project in the [Google Cloud console].
2. Enable the **Google Drive API** for that project.
3. Create an OAuth2 client of type **Desktop app** (not "Web
   application" — the loopback-redirect flow below requires it).
4. Note the client's **Client ID** and **Client secret**.
5. When you run `drive auth login` below, Google's consent screen lists
   Drive as its own separate permission tick-box, distinct from the basic
   profile/email checkboxes it also requests. **Explicitly tick it.**
   Leaving it unticked makes login fail immediately with an error naming
   the scopes Google actually granted — no Drive scope at all — instead of
   writing an unusable refresh token to `settings.json`. See
   [Troubleshooting](#no-drive-scope-was-granted) for the exact error.

**Prominent callout:** a freshly created OAuth2 client's consent screen
defaults to **Testing** publishing status. In that status, Google expires
issued refresh tokens after **7 days**, so `omni-dev drive auth login` will
need to be re-run weekly until you push the project to **In production**
(no Google verification review is required below 100 test users for a
self-scoped read-only request). See [Troubleshooting](#invalid_grant) for
the error this produces.

A second, Drive-only OAuth2 client/consent screen is perfectly fine — the
`drive` settings block is wholly independent of `gmail`'s (see
[Multiple accounts](#multiple-accounts) and [ADR-0069](adrs/adr-0069.md)).
Reusing the *same* Google Cloud project with both the Gmail and Drive APIs
enabled on one OAuth client is equally valid. It's your choice either way —
omni-dev doesn't impose either shape.

[Google Cloud console]: https://console.cloud.google.com/

## Authentication

### Environment variables

| Variable              | Purpose                                                                                                                    | Default |
|------------------------|------------------------------------------------------------------------------------------------------------------------------|---------|
| `DRIVE_CLIENT_ID`      | OAuth2 client id from your own Google Cloud project (required).                                                              | _none_  |
| `DRIVE_CLIENT_SECRET`  | OAuth2 client secret for the same client (required).                                                                         | _none_  |
| `DRIVE_REFRESH_TOKEN`  | Written by `drive auth login`; not meant to be hand-set.                                                                     | _none_  |
| `DRIVE_SCOPE`          | Written by `drive auth login`; records the granted scope (`drive.readonly`, or `drive.metadata` after `--write`) so `auth status` can report it without a network call. | _none_ |
| `DRIVE_API_URL`        | Explicit API base URL; overrides the real `www.googleapis.com` host entirely. Use for a proxy or a forced egress gateway.    | _unset_ |

Unlike Gmail, there is **no `drive auth import`** — no
`client_secret.json`-import path exists for Drive. `DRIVE_CLIENT_ID`/
`DRIVE_CLIENT_SECRET` can only reach `drive auth login` two ways: set them
by hand (in your shell profile, or in `~/.omni-dev/settings.json`'s `env`
map), or leave them unset and `drive auth login` prompts for them
interactively — the client id echoes normally, the secret does not.

### Interactive setup

```bash
$ omni-dev drive auth login
DRIVE_CLIENT_ID is not set. Create an OAuth2 client id in Google Cloud Console (see docs/adrs/adr-0069.md) and set DRIVE_CLIENT_ID, or paste it here.
Client id: 123456789-abc.apps.googleusercontent.com
Client secret: 

Credentials saved to ~/.omni-dev/settings.json
  Granted scope: https://www.googleapis.com/auth/drive.readonly

Run `omni-dev drive auth status` to verify.
```

This opens a browser to Google's consent screen via a loopback OAuth2
authorization-code + PKCE flow (see [ADR-0063](adrs/adr-0063.md), inherited
unchanged by [ADR-0069](adrs/adr-0069.md)); once you approve, the refresh
token is written to `~/.omni-dev/settings.json`. By default this requests
only `drive.readonly`. Pass `--write` to additionally request
`drive.metadata` (needed for `drive rename`/`drive move`):

```bash
$ omni-dev drive auth login --write
```

`--write` requests `drive.metadata` *alongside* `drive.readonly`, never as a
replacement — `drive.metadata` alone grants no file-content access, so
`search`/`read` still need `drive.readonly` too. Google's consent screen
lists both as separate permission tick-boxes; tick both when `--write` is
passed. Re-run `drive auth login --write` at any time to upgrade an
existing `drive.readonly`-only login — Google's `prompt=consent` re-issues a
fresh refresh token with the broader grant.

### Verifying credentials

```bash
$ omni-dev drive auth status
Checking Drive authentication...
Authenticated as: user@example.com
Granted scope: drive.readonly
```

After a `--write` login, this instead reports `Granted scope: drive.readonly,
drive.metadata`. This calls `about.get`, a live network call.

Pass `--all` to report every configured named account (see
[Multiple accounts](#multiple-accounts)) in one call instead of just the
resolved one:

```bash
$ omni-dev drive auth status --all

== work ==
Checking Drive authentication...
Authenticated as: alice@work.com
Granted scope: drive.readonly, drive.metadata

== personal ==
Checking Drive authentication...
Authenticated as: alice@gmail.com
Granted scope: drive.readonly
```

`--all` degenerates to the single-account output above when no named
accounts are configured. Each successful check also backfills that
account's cached `email_address` in `settings.json` if it isn't already
set (never used for authentication itself — only for the browser-profile
targeting below) — an explicit value, whether you set it by hand or a
previous check backfilled it, is never overwritten.

### Removing credentials

```bash
$ omni-dev drive auth logout
Drive credentials removed from ~/.omni-dev/settings.json
```

Idempotent: if no credentials are configured, it prints
`No Drive credentials were configured.` and exits successfully. Removes
the resolved account (see [Multiple accounts](#multiple-accounts) below) —
pass `--account NAME` to target a specific named account.

## Multiple accounts

`--profile` (see [Prerequisites](#prerequisites) and
[ADR-0045](adrs/adr-0045.md)) selects a whole credential bundle — Atlassian,
Datadog, the Claude API key, Gmail, *and* Drive all at once. That's the
wrong tool for "I just want a second Drive account while everything else
about my environment stays the same," so Drive accounts are a second,
independent axis: named entries in a `drive` block of
`~/.omni-dev/settings.json`, selected per invocation via an `--account
NAME` flag or the `OMNI_DEV_DRIVE_ACCOUNT` environment variable (AWS-CLI
style, mirroring `--profile`). `--account` is scoped to the `drive`
command tree — usable after the `drive` subcommand name, but not before it,
since it isn't a CLI-wide flag (this also keeps it from colliding with
Snowflake's own unrelated `snowflake ... --account`). See
[ADR-0069](adrs/adr-0069.md) for the full design rationale, and
[ADR-0066](adrs/adr-0066.md) for the Gmail precedent it applies unchanged.

Unlike Gmail, **there is no `drive account import-legacy`** — Drive is a
brand-new feature with no pre-existing single-account credential state to
migrate from. An installation with no configured `drive` accounts simply
starts `Unconfigured`; that's the normal starting state, not a
compatibility shim.

### Configuring accounts

Create a second (or subsequent) account the same way you configured the
first, adding `--account NAME`:

```bash
$ omni-dev drive auth login --account personal
```

`--account` need not already exist — `auth login` is how an account comes
into existence. Every other Drive command (`search`, `read`, `auth
status`, `auth logout`) also accepts `--account NAME` to target a specific
account, and every `drive_*` MCP tool accepts the equivalent `account`
parameter.

### Managing accounts

```bash
$ omni-dev drive account list
NAME      EMAIL              SCOPE                                              DEFAULT
personal  alice@gmail.com    https://www.googleapis.com/auth/drive.readonly
work      alice@work.com     https://www.googleapis.com/auth/drive.readonly     *

$ omni-dev drive account set-default work
Default Drive account set to 'work'.
```

`drive account list` reads only `settings.json` — no network call, no
secret ever rendered. With no accounts configured, it prints
`No named Drive accounts configured. Run \`omni-dev drive auth login
--account <name>\` to create one.`

### Resolution order

When a command runs, the account it uses is resolved in this order:

1. A literal `DRIVE_CLIENT_ID`/`DRIVE_CLIENT_SECRET`/`DRIVE_REFRESH_TOKEN`
   set directly in the process environment bypasses account resolution
   entirely — a scripting/CI convenience, not a migration path (there's
   nothing to migrate).
2. `--account NAME` / `OMNI_DEV_DRIVE_ACCOUNT`, if set, selects that named
   account. An unknown name is a hard error listing the accounts that
   *are* configured — never a silent fallback to the wrong account.
3. No explicit account, with one or more named accounts configured: the
   configured default (`drive account set-default`) if it still names a
   real account, else the sole account if exactly one is configured, else
   a hard error naming both remedies (`pass --account or run
   \`drive account set-default <name>\``).
4. No named accounts configured at all: falls through to the literal-env
   values above, or a clear "not configured, run `drive auth login`"
   error if those are absent too.

### Browser profile targeting

With several named accounts, `drive auth login` opening whatever profile
your default browser happens to be on means you have to switch Google
identities by hand on the consent screen — easy to get wrong, and it can
land the refresh token on the wrong account entirely. Two escape hatches,
both configured per account in `settings.json`'s `drive.accounts.<name>`
and both opt-in — inherited by field from Gmail's
([ADR-0067](adrs/adr-0067.md)), since the browser-targeting UX is
orthogonal to which Google API is being authorized:

**Manual — `browser_command`.** An explicit launch command, with `{url}`
substituted for the authorization URL (or appended, if no `{url}`
placeholder is present). Takes precedence over automatic resolution below.
Works for any browser, not just Chrome:

```json
"drive": {
  "accounts": {
    "jky.greens": {
      "browser_command": "open -na \"Google Chrome\" --args --profile-directory=\"Profile 7\" {url}"
    }
  }
}
```

**Automatic — `chrome_profile_from_email`.** Set this `true` alongside
`email_address` (see [Verifying credentials](#verifying-credentials) above
— set it by hand, or let `drive auth status --all` backfill it after a
first login) and `drive auth login` looks up which local Chrome profile is
signed into that address, launching the authorization URL targeting it
instead of the OS default browser:

```json
"drive": {
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
rather than picking one — resolution failure is always a fallback, never a
login failure.

## Output formats

Every subcommand that renders a list or record (`search`, `read`, `dedupe`,
`rename`, `move`, `account list`) accepts `-o <format>` (`table` / `json` /
`yaml` / `yamls` / `jsonl`, default `table`) — the same convention as every
other `omni-dev` domain (see [ADR-0046](adrs/adr-0046.md)). `auth login`/
`auth logout`/`auth status`/`account set-default` print a fixed
human-readable status line instead and have no `-o` flag. `--out-file`
exists only on `drive read --content` — metadata always renders via
`-o/--output`.

## Search

```bash
$ omni-dev drive search "name contains 'report'"
$ omni-dev drive search "mimeType = 'application/vnd.google-apps.folder'" --limit 20
$ omni-dev drive search "'1AbCdEfGhIjKlMnOpQrStUvWxYz' in parents"
```

The query is passed **verbatim** to `files.list`'s `q` parameter — omni-dev
does not reinterpret it. It's [Drive's own query language], not Gmail's
search syntax: `name contains 'report'`, `'<folder-id>' in parents`
(browsing a folder's contents is just a query, not a separate subcommand),
`mimeType = 'application/vnd.google-apps.folder'`, and operators can be
combined with `and`/`or`. `--limit 0` fetches every match up to a 10,000
hard cap, auto-paginating underneath (1,000 results per page).

Unlike `gmail search`, there is **no `--enrich`/concurrency split**:
`files.list` returns full metadata (id/name/mimeType/modifiedTime/size/
md5Checksum/sha1Checksum/sha256Checksum/...) per hit in one call via the
`fields` parameter, so there's no separate hydration step to opt into.
Every search also sends
`supportsAllDrives=true` and `includeItemsFromAllDrives=true`
unconditionally — results aren't silently scoped to My Drive only; there's
no flag to control this because there's no reason to turn it off.

[Drive's own query language]: https://developers.google.com/workspace/drive/api/guides/search-files

## Read

```bash
$ omni-dev drive read 1AbCdEfGhIjKlMnOpQrStUvWxYz
$ omni-dev drive read 1AbCdEfGhIjKlMnOpQrStUvWxYz --content
$ omni-dev drive read 1AbCdEfGhIjKlMnOpQrStUvWxYz --content --out-file report.pdf
$ omni-dev drive read 1AbCdEfGhIjKlMnOpQrStUvWxYz --content --verify --out-file report.pdf
$ omni-dev drive read <google-doc-id> --content
$ omni-dev drive read <google-sheet-id> --content --export-mime-type text/csv
```

Without `--content`, `drive read` returns metadata only:
`Id`/`Name`/`MimeType`/`Size`/`Modified`/`Parents`/`WebViewLink`/
`Md5Checksum`/`Sha1Checksum`/`Sha256Checksum` (optional fields shown only
if present). Pass `--content` to fetch the file's actual bytes instead:

- **Regular files** (PDFs, images, plain text, ...) are downloaded as-is
  via `alt=media`.
- **Google-native files** (Docs/Sheets/Slides/Forms/Drawings/...) have no
  raw bytes — they're exported via `/export?mimeType=...`. Default export
  MIME types: Google Docs → `text/markdown`, Google Sheets → `text/csv`
  (first sheet only — Drive's export API has no multi-sheet CSV format),
  Google Slides → `text/plain`. Every other Google-native type (Forms,
  Drawings, Apps Script, Sites, ...) has no safe default — omitting
  `--export-mime-type` for one of these errors out, naming the file's
  actually-supported export MIME types (from `exportLinks`) so you know
  what to pass.
- **Folders and shortcuts** are rejected with an actionable error rather
  than silently returning nothing — see
  [Troubleshooting](#reading-a-folder-or-shortcuts-content).

Without `--out-file`, texty content (`text/*` or `application/json`) that
decodes as valid UTF-8 prints directly to stdout; anything else refuses
with `refusing to print binary content ... use --out-file`. `--out-file`
writes the bytes to disk instead and prints a short confirmation
(`Saved N bytes to <path> (mimeType: ...).`) — the only place `--out-file`
is valid; passing it without `--content` is a hard error.

**Size caps:** `files.export` inherits Drive's own **10 MB** export cap
server-side (surfaces as an ordinary API error if a Google-native file is
too large to export). Raw `alt=media` downloads are capped client-side at
**500 MB** via the response's declared `Content-Length` — a download
whose length exceeds that is refused before any bytes are buffered into
memory. A missing `Content-Length` (e.g. chunked encoding) passes through
unchecked.

**Content hashes:** `md5Checksum`/`sha1Checksum`/`sha256Checksum` are
present only for binary-content files — absent for folders and
Google-native documents, which have no fixed byte content to hash. `md5`
has the broadest historical coverage (sha1/sha256 were added to the Drive
API later, so a very old, untouched file may carry only `md5`). These
fields aren't shown by `drive search`'s table renderer; use `-o
json`/`-o yaml`/`-o jsonl` to see them there. `drive read`'s table output
shows them directly (see above).

**Verifying downloaded content:** pass `--content --verify` to locally
recompute the SHA-256 checksum of the downloaded bytes and check it
against Drive's reported `sha256Checksum`, printing a one-line
confirmation on success. Fails clearly on a mismatch or on a file with no
`sha256Checksum` reported. Only supported for regular (non-Google-native)
files — Drive never returns a checksum for exported content, so
`--verify` on a Google-native file errors immediately rather than
exporting first.

## Duplicate detection

```bash
$ omni-dev drive dedupe "'1AbCdEfGhIjKlMnOpQrStUvWxYz' in parents"
$ omni-dev drive dedupe "name contains 'invoice'" --limit 0 -o json
```

`drive dedupe` reuses the same bulk-search path as `drive search` —
`files.list` already returns `md5Checksum` per hit, so finding duplicates
needs no per-file follow-up call. It groups the query's results by
`md5Checksum` (the broadest-coverage checksum field — see [Content
hashes](#read) above), keeping only groups with 2 or more files; a file
with no checksum (a folder or Google-native document) is skipped
entirely. The query argument and `--limit` behave exactly like `drive
search`'s.

Table output columns: `HASH | COUNT | FILES`, with `FILES` a comma-joined
`name (id)` list. An empty result prints `No duplicate files found.`. Pass
`-o json`/`-o yaml`/`-o jsonl` for machine-readable output instead.

Grouping is currently fixed to `md5Checksum` — there's no `--by` flag to
choose `sha1Checksum`/`sha256Checksum` instead.

## Rename

```bash
$ omni-dev drive rename 1AbCdEfGhIjKlMnOpQrStUvWxYz "Q3 Report (final)"
Renamed: Q3 Report -> Q3 Report (final) (1AbCdEfGhIjKlMnOpQrStUvWxYz)

$ omni-dev drive rename 1AbCdEfGhIjKlMnOpQrStUvWxYz "Q3 Report (final)" --dry-run
Would rename: Q3 Report -> Q3 Report (final) (1AbCdEfGhIjKlMnOpQrStUvWxYz)
```

Renaming only ever touches a file's `name` field — it never changes
`parents`, so it can never change who can see the file (Drive resolves
visibility from direct permissions plus permissions inherited from the
parent folder chain; renaming doesn't touch either). There is nothing to
gate, unlike [Move](#move): `drive rename` always proceeds, subject only to
the ordinary API/auth failures below.

Requires the `drive.metadata` scope (`drive auth login --write`). Without
it, the rename fails with an actionable hint:

```
Error: Drive API request failed: HTTP 403: Insufficient Permission (reason: insufficientPermissions)
  Run `omni-dev drive auth login --write` to grant the drive.metadata scope needed for rename/move
```

Every rename attempt — success or failure — is written to the
[request log](log.md) as a `kind: "drivemutation"` record, tagged
`service: "drive"`, carrying the file id, name, and outcome status. This is
a hard invariant, not a best-effort convenience: logging happens inside the
rename engine itself, not the CLI layer, so it holds for every current and
future caller.

## Move

```bash
$ omni-dev drive move 1AbCdEfGhIjKlMnOpQrStUvWxYz --to 1FolderIdGoesHere
STATUS           NAME                           DETAIL
moved            Q3 Report (final)

$ omni-dev drive move 1AbCd... 1Efgh... --to 1FolderId --dry-run
STATUS           NAME                           DETAIL
would-move       Q3 Report (final)
blocked          Confidential Salary Data       visibility increase (--allow-visibility-increase); adds user:external@partner.com
```

Moving a file can change **who can see it**: Drive resolves a file's
effective visibility from direct permissions on the file *plus* permissions
inherited from its parent folder chain, and moving a file changes that
chain. `drive move` computes the exact visibility diff a move would cause
and, by default, **refuses any move that would change visibility** — an
increase (new principals gain access) or a decrease (existing principals
lose access) either one. Three independent opt-in flags, none implying the
others:

- `--allow-visibility-increase` — proceed even if the move would grant new
  principals access.
- `--allow-visibility-decrease` — proceed even if the move would revoke
  existing principals' access.
- `--allow-drive-boundary-crossing` — proceed even if the move crosses a My
  Drive / Shared Drive boundary (independent of the visibility diff — a
  boundary crossing can block a move that changes nobody's *access*, only
  which Drive the file lives in).

**Bulk moves skip only the unsafe files, never fail the whole batch.**
`drive move ID1 ID2 ID3... --to FOLDER` shares one destination across every
file id given; a file whose move is blocked is reported as `blocked` and
left where it is, while every other file in the same call still moves. Pass
multiple file ids to move them all into the same folder in one call;
different files to different destinations needs separate calls.

A file already in the destination folder is reported `already-in-folder`
and never touched (no `permissions.list` call is even made for it). A
folder being moved gets a loud warning — its own visibility is evaluated,
but v1 does not recurse into a moved folder's contents, so their visibility
is not:

```
Warning: 'Old Projects' is a folder — its own visibility was evaluated, but its contents' visibility was not (folder moves don't recurse in v1).
```

**No interactive confirmation, `--dry-run` or not.** `--dry-run` plus the
`--allow-*` flags are the entire gate — an interactive-by-default confirm
would hang (or be silently force-skipped) over a future MCP caller, and
every flag passed is already captured in the request log's `command_line`.
`--dry-run` never calls the mutating `files.update` endpoint; the same
`permissions.list` reads back the exact plan a real run would act on.

**Exit code is always 0** as long as the command mechanically completed —
individual `blocked`/`failed` outcomes live in the table/JSON output, not
the exit code (the same convention `worktree push` uses). Check the output
if scripting against this.

Requires the `drive.metadata` scope (`drive auth login --write`), same as
[Rename](#rename) — see its [troubleshooting
entry](#insufficientpermissions-on-rename-or-move) for the actionable hint
on a 403.

Every move attempt — moved, blocked, already-in-folder, or failed — is
written to the [request log](log.md) as a `kind: "drivemutation"` record.
A `blocked` record carries the specific `added_principals`/
`removed_principals` that triggered it, so a refusal is fully auditable
even though no API call was made:

```bash
$ omni-dev log --query 'kind:drivemutation status:blocked'
```

**Known limitation — shadowed grants.** Drive's API doesn't expose whether
a principal's access on a file is direct or inherited (that split is only
populated for Shared Drive items, not My Drive files), so `drive move`
derives it by subtraction. If a principal has *both* a direct grant on the
file *and* inherited access via its current parent, the subtraction can't
tell them apart — a move that only removes the parent-inherited grant is
reported as revoking that principal's access, even though their direct
grant means they actually keep it. This is a **safe failure direction**: it
can only produce an unnecessary `--allow-visibility-decrease` requirement,
never a missed visibility increase. See
[ADR-0070](adrs/adr-0070.md) for the full algorithm.

## Rate limits and retry behaviour

Drive signals quota exhaustion two ways: a plain **HTTP 429**, and **HTTP
403** with `reason: userRateLimitExceeded` specifically — not any 403 with
a `reason` (e.g. `insufficientPermissions` is also a 403 and is never
retried, since retrying a permission error just wastes the backoff window
before failing anyway). Both retry through the shared driver
(`retry_if`/`retry_429`, `src/utils/http.rs`) with the same
`Retry-After`-then-exponential-backoff schedule. Unlike Gmail's client,
Drive's retry match does **not** also cover the bare `rateLimitExceeded`
reason string — that's confirmed for Gmail but not (yet) confirmed for
Drive against [Drive's error-handling guide]; it'll widen if testing
surfaces a real case.

`search` auto-paginates when `--limit 0` is passed (or any `--limit`
larger than the 1,000-per-page cap), capped at **10,000 records** per
invocation.

[Drive's error-handling guide]: https://developers.google.com/workspace/drive/api/guides/handle-errors

## Troubleshooting

### Credentials not configured

```
Error: Drive credentials not configured. Run `omni-dev drive auth login`
```

Means `DRIVE_CLIENT_ID`, `DRIVE_CLIENT_SECRET`, or `DRIVE_REFRESH_TOKEN` is
missing from both the environment and `settings.json`. Run
`omni-dev drive auth login` — it prompts for the first two if they're
still absent; the third is written by `auth login` itself.

### `invalid_grant`

Google's `invalid_grant` response is identical for two different causes;
`drive auth login`/token-refresh distinguish which call failed and give a
tailored message:

```
Error: Failed to obtain a Drive access token
  Caused by: Google rejected the request (invalid_grant): this almost always means either (1) your Drive OAuth client is in "Testing" publishing status, where refresh tokens expire after 7 days — publish it to "In production" in Google Cloud Console to avoid this, or (2) access was revoked. Run `omni-dev drive auth login` again to re-authenticate.
```

(during a refresh — by far the most common cause, the 7-day testing-mode
expiry described in [Prerequisites](#prerequisites)), or:

```
Error: Google rejected the request (invalid_grant): the authorization code was invalid, already used, expired (codes are single-use and valid only a few minutes), or the PKCE code_verifier did not match the code_challenge sent at the start of login. Run `omni-dev drive auth login` again.
```

(during the initial code exchange, right after approving the consent
screen). Either way, re-run `omni-dev drive auth login`, or push your OAuth
client to "In production" in Google Cloud Console to stop the 7-day
expiry recurring.

### `access_denied`

```
Error: Google denied the authorization request: access_denied
```

You (or another user) clicked "Cancel" on Google's consent screen, or your
OAuth client's test-user allowlist doesn't include the account you tried to
authorize (a Testing-mode consent screen only allows explicitly added test
users). Re-run `omni-dev drive auth login` and either approve the prompt or
add the account under **OAuth consent screen → Test users** in Google Cloud
Console.

### Could not start the local OAuth callback listener

```
Error: Failed to start the local OAuth callback listener
```

The loopback listener binds an OS-assigned ephemeral port, so this should
be rare. The one common cause is a stale process from a previously
interrupted `drive auth login` holding a socket resource open — retry,
and if it persists, check for a leftover `omni-dev` process.

### Timed out waiting for the browser sign-in callback

```
Error: Timed out after 120s waiting for the browser sign-in callback; re-run `omni-dev drive auth login`
```

Nothing hit the loopback callback within 120 seconds — most often because
the consent screen was left open too long, or the browser never opened
(see below). Just re-run `omni-dev drive auth login`.

### Browser did not open

`drive auth login` opens your default browser automatically. If it fails
to open (e.g. over SSH, or in a headless environment), the authorization
URL is printed to the terminal for you to open manually — no CLI flag is
needed to force this fallback; it's the same code path.

If it opens the *wrong* browser profile (mixing up which named account
lands on which Google identity), see [Browser profile
targeting](#browser-profile-targeting) above.

### No Drive scope was granted

```
Error: Google did not grant the drive.readonly scope (received: openid, email, profile).
  On the consent screen, tick the Drive permission — restricted scopes are
  not granted by default. Re-run `omni-dev drive auth login`.
```

Cause: the consent screen's Drive permission tick-box (see
[Prerequisites](#prerequisites)) was left unticked, so Google granted only
`openid`/`email`/`profile` — no Drive scope at all. `auth login` rejects
this immediately, naming the scopes Google actually granted, and writes
nothing to `settings.json`. Fix: re-run `omni-dev drive auth login` and
tick the Drive permission this time.

### Reading a folder or shortcut's content

```
Error: '<name>' is a folder; folders have no content to read — use `drive search` to list what it contains
```

```
Error: '<name>' is a shortcut; `drive read --content` doesn't follow shortcuts to their target file — resolve the target file's id and read that instead
```

`drive read --content` refuses both up front rather than returning an
empty or misleading response. For a folder, list its contents with
`drive search "'<folder-id>' in parents"`. For a shortcut, `drive read
<shortcut-id>` (metadata only, no `--content`) shows what it points at;
resolve that id and read it directly.

### `refusing to load N bytes into memory`

```
Error: refusing to load 734003200 bytes into memory (limit: 524288000 bytes); ...
```

The file's declared size exceeds the 500 MB `alt=media` download cap (see
[Read](#read)). There's no override flag — very large files aren't a fit
for this command today.

### `insufficientPermissions` on rename (or move)

```
Error: Drive API request failed: HTTP 403: Insufficient Permission (reason: insufficientPermissions)
  Run `omni-dev drive auth login --write` to grant the drive.metadata scope needed for rename/move
```

The active credentials only carry `drive.readonly` — there is no
client-side check before the call, so this surfaces from Google's own 403.
Re-run `omni-dev drive auth login --write` to upgrade the grant (see
[Interactive setup](#interactive-setup)), then retry.

### No default export format for a Google-native file

```
Error: '<name>' (mimeType: application/vnd.google-apps.form) has no default export format; pass --export-mime-type. Supported export MIME types: application/pdf, application/zip
```

Only Docs/Sheets/Slides have a safe default export MIME type (see
[Read](#read)). Pass one of the listed `--export-mime-type` values.

## See also

- [Drive Quickstart](drive-quickstart.md) — a linear, zero-to-first-search
  walkthrough for first-time setup.
- [Gmail Integration](gmail.md) — the sibling Google integration; shares
  the same named-account/OAuth2 storage pattern.
- [ADR-0069](adrs/adr-0069.md) — the Drive-specific named-account store and
  original read-only OAuth2 client design, and why it deliberately
  duplicates rather than shares code with Gmail's.
- [ADR-0070](adrs/adr-0070.md) — reverses ADR-0069 §2 to add rename/move:
  the additive `drive.metadata` scope, the visibility-diff algorithm behind
  `move`'s safety gate, and the three-flag opt-in model.
- [ADR-0063](adrs/adr-0063.md) — the OAuth2 authorization-code + PKCE
  design, refresh-token-only persistence, and bring-your-own Google Cloud
  project rationale ADR-0069 applies unchanged.
- [ADR-0066](adrs/adr-0066.md) — the named-account store behind
  [Multiple accounts](#multiple-accounts), and why it's orthogonal to
  `--profile`.
- MCP tools — planned, not yet available; tracked by
  [issue #1525](https://github.com/rust-works/omni-dev/issues/1525).
- [Drive API documentation](https://developers.google.com/workspace/drive/api/reference/rest/v3) — upstream reference.
