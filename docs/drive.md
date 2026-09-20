# Drive Integration

omni-dev exposes access to the Google Drive v3 API through the `omni-dev
drive` command tree — search, read a file's metadata or content, find
duplicates, rename a file, move it between folders, and create/upload/edit
file content. `drive.readonly` (the default scope) is enough for
search/read/dedupe; rename/move need the opt-in `drive.metadata` scope
(`drive auth login --write`), the narrowest write scope Google offers — it
covers `files.update` on `name`/`parents` only, with no file-content access
at all. Content mutation needs a broader grant still: `--write-file`
(`drive.file`, app-created files only) or `--write-full` (the unrestricted
`drive` scope, needed to edit any pre-existing file). `drive lease prune`
has a trash capability now ([`FilesApi::trash`](#lease), see
[Prune](#prune)); share/permission-mutation is still absent anywhere in
this surface.

**Move is security-gated.** Moving a file can change who can see it — Drive
resolves a file's effective visibility from both direct permissions on the
file and permissions inherited from its parent folder chain, and moving a
file changes that chain. `drive move` refuses any move that would change
visibility **by default**; three independent `--allow-*` flags opt in. See
[Move](#move) and [ADR-0070](adrs/adr-0070.md) for the full design.

**Create/upload/edit are gated by a second, independent, local
permission system.** Google's OAuth scopes are all-or-nothing across your
*entire* Drive — there's no way to grant "write access to just this
folder." `write_permissions` rules in `settings.json` are omni-dev's own
policy layer filling that gap: read defaults open, every write defaults
**refused everywhere** until a rule explicitly grants it for that folder.
Both the OAuth scope and the local gate must allow an operation — neither
alone is sufficient. See [Write permissions](#write-permissions) and
[ADR-0071](adrs/adr-0071.md) for the full design.

The MCP tool surface (`drive_auth_status`/`drive_search`/`drive_dedupe`/
`drive_file_read`/`drive_sheets_info`/`drive_sheets_read`/`drive_account_list`,
mirroring the CLI one-for-one like Gmail's `gmail_*` tools) is read-only, like
the rest of the MCP surface — `rename`/`move`/`create`/`upload`/`edit` and
`sheets write`/`append`/`clear` have no MCP equivalent. See
[docs/mcp.md](mcp.md#drive-7-tools) for the full tool reference.

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
10. [Write permissions](#write-permissions)
11. [Create](#create)
12. [Upload](#upload)
13. [Edit](#edit)
14. [Lease](#lease)
15. [Sheets](#sheets)
16. [Docs](#docs)
17. [Rate limits and retry behaviour](#rate-limits-and-retry-behaviour)
18. [Troubleshooting](#troubleshooting)
19. [See also](#see-also)

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

To go to **In production**: OAuth consent screen → **Publish App**. This
by itself does not trigger a verification review — the next time you (or
any of your up-to-100 test users) sign in, Google shows an "unverified
app" interstitial; click **Advanced → Go to `<your project>` (unsafe)**
to proceed. That warning is expected and permanent for a project like
this one — it's not a sign anything is misconfigured, and it's the
tradeoff for not taking on CASA. **Don't upload a logo** on the Branding
page: Google requires a full verification review (including CASA for
restricted scopes like `drive`/`drive.readonly`) before it will display a
logo, so uploading one moves your project onto that track even though
you never asked for a review. Branding fields otherwise (app name,
support email) don't trigger it.

A second, Drive-only OAuth2 client/consent screen is perfectly fine — the
`drive` settings block is wholly independent of `gmail`'s (see
[Multiple accounts](#multiple-accounts) and [ADR-0069](adrs/adr-0069.md)).
Reusing the *same* Google Cloud project with both the Gmail and Drive APIs
enabled on one OAuth client is equally valid. It's your choice either way —
omni-dev doesn't impose either shape.

[Google Cloud console]: https://console.cloud.google.com/

## Authentication

### Environment variables

| Variable              | Purpose                                                                                                                                                                                                                                               | Default |
|-----------------------|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|---------|
| `DRIVE_CLIENT_ID`     | OAuth2 client id from your own Google Cloud project (required).                                                                                                                                                                                       | _none_  |
| `DRIVE_CLIENT_SECRET` | OAuth2 client secret for the same client (required).                                                                                                                                                                                                  | _none_  |
| `DRIVE_REFRESH_TOKEN` | Written by `drive auth login`; not meant to be hand-set.                                                                                                                                                                                              | _none_  |
| `DRIVE_SCOPE`         | Written by `drive auth login`; records the granted scope(s) — any combination of `drive.readonly`, `drive.metadata` (`--write`), `drive.file` (`--write-file`), and `drive` (`--write-full`) — so `auth status` can report it without a network call. | _none_  |
| `DRIVE_API_URL`       | Explicit API base URL; overrides the real `www.googleapis.com` host entirely. Use for a proxy or a forced egress gateway.                                                                                                                             | _unset_ |

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
only `drive.readonly`. Three independent flags request more, combinable
freely in one call:

| Flag           | Scope requested        | Needed for                                                                                           |
|----------------|------------------------|------------------------------------------------------------------------------------------------------|
| `--write`      | `drive.metadata`       | `drive rename`/`drive move`                                                                          |
| `--write-file` | `drive.file`           | `drive create`/`drive upload`, and `drive edit` on files `omni-dev` itself created                   |
| `--write-full` | `drive` (unrestricted) | `drive edit` on any pre-existing file — the largest privilege grant this integration ever requests   |

```bash
$ omni-dev drive auth login --write --write-file --write-full
```

Every flag requests its scope *alongside* `drive.readonly`, never as a
replacement — none of `drive.metadata`/`drive.file`/`drive` alone grants
read access, so `search`/`read` still need `drive.readonly` too. Google's
consent screen lists each as a separate permission tick-box; tick all that
apply to the flags you passed. Re-run `drive auth login` with more flags at
any time to upgrade an existing login — Google's `prompt=consent` re-issues
a fresh refresh token with the broader grant.

`--write-file` alone cannot edit a file that already existed in your Drive
before `omni-dev` touched it — Google restricts `drive.file` to files this
app itself created via that scope. `drive edit` on any pre-existing file
needs `--write-full`, the only scope that can. Requesting `--write-full` is
a significant privilege escalation (unrestricted read/write over your
*entire* Drive) — the [Write permissions](#write-permissions) gate below is
what bounds it to specific folders in practice.

### Verifying credentials

```bash
$ omni-dev drive auth status
Checking Drive authentication...
Authenticated as: user@example.com
Granted scope: drive.readonly
```

After a `--write --write-file` login, this instead reports `Granted scope:
drive.readonly, drive.metadata, drive.file` — every granted scope, listed
in the order shown in the [Interactive setup](#interactive-setup) table
above. This calls `about.get`, a live network call.

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
`rename`, `move`, `create`, `upload`, `edit`, `account list`,
`permissions show`/`check`, `sheets info`, `sheets read`) accepts
`-o <format>` (`table` / `json` / `yaml` / `yamls` / `jsonl`, default `table`) — the same convention as every
other `omni-dev` domain (see [ADR-0046](adrs/adr-0046.md)). `auth login`/
`auth logout`/`auth status`/`account set-default` print a fixed
human-readable status line instead and have no `-o` flag. `--out-file`
exists only on `drive read --content` — metadata always renders via
`-o/--output`. One command reads `table` unusually: `drive sheets read`
renders CSV for it, since a grid of cells is what a spreadsheet range *is*
(see [Sheets](#sheets)).

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

## Write permissions

`drive create`/`drive upload`/`drive edit`, and `drive sheets
write`/`append`/`clear`/`create`/`add-sheet`/`rename-sheet`/`insert-rows`/
`insert-columns` (below), need a much broader OAuth grant
than rename/move — `--write-file`/`--write-full` — but Google's
scopes are all-or-nothing across your whole Drive. There's no way to tell
Google "only let this credential write inside folder X." So `omni-dev` adds
its own, independent, local policy layer on top: an allow/deny rule list
in `settings.json` — scoped to a folder, or to a single file — evaluated
**before** any mutating API call is attempted, regardless of what the OAuth
scope would technically permit.

**Default policy** — what applies when no configured rule names an
operation anywhere in a target's ancestor chain:

| Operation           | Default | Granted to |
|---------------------|---------|------------|
| `read`              | allow   | `search`, `read`, `dedupe` (not yet enforced) |
| `create`            | deny    | `create`, `sheets create` |
| `upload`            | deny    | `upload` |
| `edit`              | deny    | `edit` — raw file content only |
| `sheets-write`      | deny    | `sheets write`, `sheets append`, `sheets clear` — cell values |
| `sheets-structure`  | deny    | `sheets add-sheet`, `rename-sheet`, `insert-rows`, `insert-columns`, `duplicate-sheet`, `reorder-sheet`, `hide-sheet`, `show-sheet`, `format-cells`, `update-borders`, `merge-cells`, `unmerge-cells`, `auto-resize-dimension`, `update-dimension-properties`, `set-data-validation`, `clear-data-validation`, `set-developer-metadata`, `delete-developer-metadata`, `set-basic-filter`, `clear-basic-filter`, `add-filter-view`, `update-filter-view`, `delete-filter-view`, `add-conditional-format`, `update-conditional-format`, `delete-conditional-format`, `add-named-range`, `update-named-range`, `delete-named-range` |
| `sheets-delete`     | deny    | `sheets delete-sheet`, `delete-rows`, `delete-columns`, `delete-range` |
| `sheets-protection` | deny    | `sheets protect-range`, `update-protection`, `unprotect-range` |
| `docs-write`        | deny    | `docs replace`, `docs append` |

There is no "enabled: true" flag — an absent or empty rule list already
means "deny every write everywhere," via this table alone, which *is* the
disabled state.

**`sheets-write` is deliberately separate from `edit`.** Writing cells is a
content mutation, so folding it into `edit` would have been the obvious
choice — but every `allow: ["edit"]` rule that exists today was written when
`drive edit` refused every Google-native document outright. Reusing `edit`
would have retroactively turned those rules into cell-write permission with
no config change and no re-consent. If you want a folder's existing `edit`
grant to cover Sheets too, add `sheets-write` to it explicitly. See
[ADR-0073](adrs/adr-0073.md) §3.

**`sheets-structure` is separate from `sheets-write` for the same reason.**
Every `allow: ["sheets-write"]` rule that exists today was written when
structural edits were impossible, so reusing it would have turned those
rules into permission to restructure a workbook — again with no config
change and no re-consent. Granting one does not grant the other; name both
if you want both. See [ADR-0075](adrs/adr-0075.md) §1.

**`sheets-delete` is separate from `sheets-structure`, for the same reason
again.** Every `allow: ["sheets-structure"]` rule that exists today was
written when deletion was impossible, so reusing it would have turned those
rules into permission to destroy data — again with no config change and no
re-consent. Granting `sheets-structure` does not grant `sheets-delete`; name
both if you want both. See [ADR-0077](adrs/adr-0077-sheets-deletion-via-batchupdate.md).

**`sheets-structure` also covers formatting, data validation, duplicating a
sheet, and reorder/hide** (issue #1643, [ADR-0078](adrs/adr-0078.md)). None
of that destroys data — `merge-cells` is the one request that discards
non-top-left values, and its `--dry-run` (and real run) names every cell
that would be lost before it happens — so it earns the same operation as
the original four verbs rather than a new one.

**`sheets-structure` also covers developer-metadata management, restricted
to `DOCUMENT` visibility** (issue #1795, [ADR-0081](adrs/adr-0081.md) §4).
`createDeveloperMetadata`/`updateDeveloperMetadata`/`deleteDeveloperMetadata`
attach or remove key/value annotations on the spreadsheet, a sheet, a row
or a column — none of that is grid data and none of it is a permission
change, so it earns the same operation as everything else here rather than
a new one. The surface reads and writes `DOCUMENT`-visibility metadata
only; `PROJECT`-visibility metadata belongs to whatever OAuth client
created it and is never reachable through this tool. `delete-developer-metadata`
reads back and reports the key, value and location of every entry it would
remove before deleting it, the same preview pattern as `merge-cells`.
`search-developer-metadata` is read-only and ungated, like
`list-protections` below.

**Conditional formatting joins `sheets-structure` too** (issue #1793,
[ADR-0081](adrs/adr-0081.md) §1) — presentational, destroys no data, exactly
like `format-cells`/`set-data-validation`. `list-conditional-formats` is a
plain read and needs no grant, the same as `list-protections`.

**`sheets-structure` also covers named-range add/update/delete** (issue
#1796, [ADR-0081](adrs/adr-0081.md) §2). A named range is a label over a
region, not grid data, so `delete-named-range` leaves every cell's stored
value and formula text untouched — even though a formula referencing the
removed name starts evaluating to `#NAME?`. That effect is mitigated the
same way `merge-cells`' data loss is: `delete-named-range`'s `--dry-run`
(and real run) scans the workbook's formulas for the name and reports the
count and A1 locations of every reference before it deletes.
`drive sheets list-named-ranges` is a plain read and needs no grant, the
same as `list-protections`.

**`sheets-protection` is separate from `sheets-structure`, and the reason is
different in kind from every split above.** A protected range is a
*permission* inside the document — who may edit, not what the sheet
contains. `update-protection` can widen who may edit, and
`unprotect-range` removes a guard someone deliberately placed. Folding
either into `sheets-structure` would let a grant meant for "may
reformat/validate/restructure this workbook" silently double as "may also
change who can edit it." See [ADR-0078](adrs/adr-0078.md) §2.
`drive sheets list-protections` is a plain read and needs no grant, the same
as `sheets info`.

**`docs-write` is separate from all four**, and the same argument runs
again. An `allow: ["edit"]` rule predates Docs being reachable through this
tool at all; an `allow: ["sheets-write"]`, `allow: ["sheets-structure"]`,
`allow: ["sheets-delete"]` or `allow: ["sheets-protection"]` rule was
written when the same was true, and all four are about *cells or
who-may-edit-them*, so letting any of them govern prose would make the
config vocabulary say something untrue. Grant `docs-write` explicitly. See
[ADR-0076](adrs/adr-0076.md) §2.

Each write operation is independent in both directions: `docs-write` confers
no cell writes, no structural sheet edits, no destructive sheet edits, no
protection changes, and no raw-content edits either.

Rules live per Drive account, since a folder id only means something inside
the one Drive it came from. A rule keys on **either** a `folder_id` or a
`file_id` — exactly one, never both:

```jsonc
{
  "drive": {
    "accounts": {
      "work": {
        "write_permissions": {
          "rules": [
            { "folder_id": "1AbC...AiWorkspace",  "recursive": true,  "allow": ["create", "upload", "edit"] },
            { "folder_id": "1XyZ...DropZone",     "recursive": false, "allow": ["create"] },
            { "folder_id": "1Scr...Scratch",      "recursive": true,  "allow": ["edit"], "require_lease": false },
            { "folder_id": "1Sen...Confidential", "recursive": true,  "deny": ["read"] },

            // File rules — for a file you can't reach with a folder rule.
            { "file_id": "1Sh4r3d...QuarterlyPlan",   "allow": ["sheets-write"] },
            { "file_id": "1Sh4r3d...SignedContract",  "deny":  ["edit", "sheets-write"] }
          ]
        }
      }
    }
  }
}
```

- `folder_id` — Drive's own canonical folder id, not a path (Drive names
  aren't unique, and files can have multiple parents). Find one with
  [`drive permissions lookup-folder`](#drive-permissions-lookup-folder)
  below.
- `file_id` — Drive's own canonical **file** id, matched against the target
  itself. See [Granting a file shared with you](#granting-a-file-shared-with-you).
- `recursive` — when `true`, the rule also matches every descendant of
  `folder_id`, not just the folder itself. Only valid on a `folder_id`
  rule: a file has no descendants, so `recursive: true` alongside a
  `file_id` is a configuration error rather than a no-op.
- `allow`/`deny` — any of `read`, `create`, `upload`, `edit`,
  `sheets-write`, `sheets-structure`, `sheets-delete`, `docs-write`. A `deny`
  entry for `read` is schema-ready today for a future `search`/`read`/
  `dedupe` enforcement fast-follow (not wired up yet — see
  [ADR-0071](adrs/adr-0071.md) §11); the write operations are enforced now.
- `require_lease` — whether a write this rule decides also *needs* a valid
  Drive write lease (`--lease`, [ADR-0080](adrs/adr-0080.md); see
  [Lease](#lease)). Defaults to `true`; set `false` to relax it for a
  specific folder or file — this skips **both** the backup and the Touch ID
  prompt, not just one of them. It does not change what a lease token
  *means*: a `--lease` presented anyway is still validated, consumed and
  audited exactly as it would be under a requiring rule, and refused if
  it's expired, wrong-file, or stale. Orthogonal to `allow`/`deny`: the
  lease is checked in addition to this gate's verdict, never instead of it.

A rule that names neither key, names both, or puts `recursive: true` on a
`file_id` is a **settings load error**, not a silently-ignored rule — and
the failure is closed: a settings file that fails to parse yields no rules
at all, so every write is refused until it is fixed.

**Resolution**: a `file_id` rule naming the target is checked first, at
what is effectively **depth −1** — strictly more specific than any folder
rule. Otherwise, for the target's ancestor chain (the folder itself at
depth 0, then its parent, grandparent, …), the closest matching rule wins;
if rules at the same depth disagree, `deny` wins. No matching rule anywhere
falls through to the default policy table above.

Because a file rule is closer than every folder rule, it wins in **both**
directions: a file `deny` overrides a recursive folder `allow`, and a file
`allow` overrides a folder `deny`. That includes the multi-parent case —
"`deny` wins across parents" is a tie-break among a target's several
parents, which are peers of each other; a file rule is not one of them.

`drive create`/`drive upload`/`drive sheets create` resolve the chain from
`--parent`, so a `file_id` rule never applies to them: their target is a
folder, and the file being created has no id yet. `drive edit` and every
`drive sheets` mutating verb resolve from the target file's *current*
parent(s) — unioned across every current parent for a legacy multi-parent
file, with `deny` winning if any parent disagrees — but only after the
`file_id` lookup has come up empty.

#### Granting a file shared with you

`files.get` returns only the parents **this account can see**. A file
shared with you by link or email is not in a folder you can see, so it
comes back with no parents at all — which means no `folder_id` rule you
could write would ever apply to it, and before file rules there was no way
to permit writing to it short of moving it into your own Drive.

A `file_id` rule is the fix. Grab the id out of the URL
(`https://docs.google.com/spreadsheets/d/<id>/edit`) and name it directly:

```jsonc
{ "file_id": "1Sh4r3d...QuarterlyPlan", "allow": ["sheets-write"] }
```

Confirm it with `drive permissions check <id> --operation sheets-write`,
which reports `decided by: rule on file <id>`. When no rule applies, that
same command prints a `note:` line saying the target has no visible parent
— that is the signal to reach for a `file_id` rule rather than hunting for
a folder rule bug.

### Diagnostics

Three read-only subcommands, none of which can ever mutate anything —
useful for authoring and debugging rules before relying on them.

#### `drive permissions show`

```bash
$ omni-dev drive permissions show
SCOPE   TARGET_ID                RECURSIVE  LEASE  ALLOW                DENY
folder  1AbC...AiWorkspace       true       true   create,edit,upload   -
folder  1XyZ...DropZone          false      false  create               -
file    1Sh4r3d...QuarterlyPlan  -          true   sheets-write         -
```

`RECURSIVE` shows `-` rather than `false` for a file rule: the column has
no meaning there. `LEASE` (ADR-0080 §13) renders `require_lease` directly —
`false` means writes matching that rule skip the write-lease's Touch ID
prompt and backup requirement (below).

Reads only `settings.json` — no network call. With no rules configured, it
explains that every write is refused everywhere and points at the
`write_permissions.rules` key above.

#### `drive permissions lookup-folder`

```bash
$ omni-dev drive permissions lookup-folder "Workspace"
ID                    NAME       PATH
1AbC...AiWorkspace     Workspace  My Drive/Team/Workspace
```

Searches by name and resolves each hit's full root-to-leaf path (via the
same ancestor-chain walk the gate itself uses), so you can tell apart
same-named folders in different locations before pasting an id into
config.

#### `drive permissions check`

```bash
$ omni-dev drive permissions check 1AbC...AiWorkspace --operation create
target:     1AbC...AiWorkspace
operation:  create
verdict:    allow
decided by: rule on folder 1AbC...AiWorkspace (depth 0)

$ omni-dev drive permissions check 1Sh4r3d...QuarterlyPlan --operation sheets-write
target:     1Sh4r3d...QuarterlyPlan
operation:  sheets-write
verdict:    allow
decided by: rule on file 1Sh4r3d...QuarterlyPlan

$ omni-dev drive permissions check 1Unknown...Shared --operation sheets-write
target:     1Unknown...Shared
operation:  sheets-write
verdict:    deny
decided by: default policy (no matching rule)
note:       this target has no parent folder visible to this account, so no
            folder_id rule can apply — grant it with a file_id rule instead
```

Evaluates the real configured rules against a real target and operation —
the exact functions `create`/`upload`/`edit`/`sheets write` themselves
call, so this diagnostic can never drift from actual enforcement. Accepts
either a folder id (checked directly) or a file id (its own `file_id`
rules first, then its current parent(s), matching `edit`'s own semantics).

The `note:` line is the one this command exists for: it appears only on a
`deny` against a target with no visible parent, which is the single case
where no `folder_id` rule could ever help. It is gated on the verdict as
well, because `read` defaults to allow on an empty ancestor chain — a
link-shared target checked for `read` is *permitted*, and advice on how to
grant it would read as a refusal that isn't one.

`-o json` adds `decided_by_file_id` and `evaluated_via` (`"file-rule"`,
`"folder-chain"` or `"no-visible-parents"`) alongside the existing
`decided_by_folder_id`/`decided_by_depth`, which keep their exact meaning —
a file id never appears in the folder field.

## Create

```bash
$ omni-dev drive create --name "Notes.txt" --parent 1AbC...AiWorkspace
Created: Notes.txt (1NewFileIdHere) in 1AbC...AiWorkspace

$ omni-dev drive create --name "Notes.txt" --parent 1AbC...AiWorkspace --dry-run
Would create: Notes.txt in 1AbC...AiWorkspace

$ omni-dev drive create --name "Reports" --parent 1AbC...AiWorkspace --folder
Created: Reports (1NewFolderIdHere) in 1AbC...AiWorkspace
```

Creates a new file (metadata only — no content; see [Upload](#upload) to
push local content in) or, with `--folder`, a new folder. `--mime-type`
sets the content type for a plain file (default
`application/octet-stream`); it conflicts with `--folder`, which always
creates `application/vnd.google-apps.folder`.

Gated by [Write permissions](#write-permissions) against `--parent` —
refused before any `files.create` call if no rule allows `create` there.
`--dry-run` classifies against the exact same gate a real run would,
without ever calling `files.create`:

```bash
$ omni-dev drive create --name "x" --parent 1Sen...Confidential --dry-run
Blocked: x in 1Sen...Confidential
  refused by default policy (no matching rule)
```

Requires the `drive.file` or `drive` scope (`drive auth login --write-file`
or `--write-full`); without either, the call fails with an actionable hint
naming both flags. Every real attempt — created, blocked, or failed — is
written to the [request log](log.md#what-gets-recorded) as a `kind:
"drivemutation"` record, even when the gate refused before any API call
was made; `--dry-run` previews are never logged.

## Upload

```bash
$ omni-dev drive upload ./report.pdf --parent 1AbC...AiWorkspace
Uploaded: report.pdf (1NewFileIdHere) in 1AbC...AiWorkspace

$ omni-dev drive upload ./report.pdf --parent 1AbC...AiWorkspace --name "Q3 Report.pdf" --dry-run
Would upload: Q3 Report.pdf in 1AbC...AiWorkspace
```

Uploads local content as a new file — everything [Create](#create) does,
plus reading a local file's bytes. `--name` defaults to the local file's
own name; `--mime-type` defaults to `application/octet-stream`.

**5 MB size cap.** Drive's simple (non-resumable) upload endpoint —
the only one this command uses — caps request bodies at 5 MB. The local
file is stat'd and refused *before* it's ever read into memory if it's too
large, so this fires identically whether or not `--dry-run` is set:

```bash
$ omni-dev drive upload ./huge-video.mp4 --parent 1AbC...AiWorkspace
Error: refusing to upload 83886080 bytes (limit: 5242880 bytes); Drive's simple upload endpoint caps requests at 5 MB — larger content needs resumable upload, not supported by `drive upload`/`drive edit` yet
```

Larger content needs Drive's chunked resumable-upload protocol, not
supported by this command in v1 (an explicit, documented boundary — see
[ADR-0071](adrs/adr-0071.md) §10 — not a silent gap).

Same gate, scope requirement, and logging behavior as [Create](#create).

## Edit

```bash
$ omni-dev drive lease acquire 1ExistingFileId
lease-abc123...
Backed up to /home/user/.local/state/omni-dev/drive-backups/20260911T000000Z-1ExistingFileId-report.pdf (expires 2026-09-11 00:30:00 UTC)

$ omni-dev drive edit 1ExistingFileId --content ./new-report.pdf --lease lease-abc123...
Edited: 1ExistingFileId

$ cat ./new-report.pdf | omni-dev drive edit 1ExistingFileId --content - --lease lease-abc123...
Edited: 1ExistingFileId

$ omni-dev drive edit 1ExistingFileId --content ./new-report.pdf --dry-run
Would edit: 1ExistingFileId
```

Replaces an existing file's raw content. `--content` accepts a local path,
or `-` to read from stdin (bounded at the same 5 MB cap — an
unbounded pipe is never buffered past the limit before being refused).

**Requires a Drive write lease** (`--lease`, [ADR-0080](adrs/adr-0080.md)),
unless the deciding write-permission rule sets `require_lease: false` — see
[Lease](#lease) below. Never needed with `--dry-run`.

**Gated differently from create/upload.** Since there's no `--parent` to
check, the gate evaluates the target's *current* parent folder(s) instead
— unioned across every parent for a legacy multi-parent file, with `deny`
winning if any parent disagrees (see [Write
permissions](#write-permissions) above). An orphan file with no parent
falls straight to the default policy (refused).

**Google-native documents are refused outright, before the gate even
runs:**

```bash
$ omni-dev drive edit 1SomeGoogleDocId --content ./file.txt
Refused: 1SomeGoogleDocId is a Google-native document (Docs/Sheets/Slides/...) — no raw content to replace
```

A Docs/Sheets/Slides file has no fixed byte content a raw media `PATCH` can
replace — editing one is a Docs-API/Sheets-API problem, out of scope here
(the same deferral [ADR-0069](adrs/adr-0069.md) already made for Docs
export).

**Scope depends on the file's origin.** `--write-file` (`drive.file`) is
enough only if `omni-dev` itself created the target via `drive
create`/`drive upload`; any other pre-existing file needs the unrestricted
`--write-full`. A 403 names both flags, since the client has no cheap way
to tell which a given file id needs:

```
Error: Drive API request failed: HTTP 403: Insufficient Permission (reason: insufficientPermissions)
  Run `omni-dev drive auth login --write-file` if this file was created by omni-dev, or `--write-full` to edit any pre-existing file's content, then retry
```

Same request-log behavior as [Create](#create)/[Upload](#upload).

## Lease

```bash
$ omni-dev drive lease acquire 1ExistingFileId
lease-abc123...
Backed up to /home/user/.local/state/omni-dev/drive-backups/20260911T000000Z-1ExistingFileId-report.pdf (expires 2026-09-11 00:30:00 UTC)
```

Before `drive edit` can write, it needs a **lease**: a token bound to a
mandatory pre-write backup and the file's current Drive `version`
([ADR-0080](adrs/adr-0080.md)). Acquiring one prompts for **device-owner
authentication** — Touch ID, or the account password when biometrics are
unavailable — so an agent can obtain its own lease, but a human is
provably present at the moment consent is given. The prompt is a real
system dialog rendered by macOS itself; there is no way to answer it from
a script or a PTY.

**`drive lease acquire` works against both binary files and native
documents** (see the fidelity split below). `--lease` is required by every
content-mutating write verb: `drive edit`; `drive sheets`
`write`/`append`/`clear`, `add-sheet`/`rename-sheet`/`insert-rows`/
`insert-columns`/`duplicate-sheet`/`reorder-sheet`/`hide-sheet`/
`show-sheet`, `delete-sheet`/`delete-rows`/`delete-columns`/`delete-range`,
`format-cells`/`merge-cells`/`unmerge-cells`/`update-borders`/
`update-dimension-properties`/`auto-resize-columns`,
`set-data-validation`/`clear-data-validation`,
`protect-range`/`update-protection`/`unprotect-range`, and
`set-basic-filter`/`clear-basic-filter`/`add-filter-view`/
`update-filter-view`/`delete-filter-view`; and `drive docs
replace`/`append`. `--dry-run` never needs one on any of them.

**The backup fidelity splits by file type** ([ADR-0080](adrs/adr-0080.md)
§3). A binary file backs up as **bytes on this machine**, named
`<YYYYMMDDTHHMMSSZ>-<fileId>-<name>` under `--backup-dir` (default
`<state dir>/omni-dev/drive-backups`) — UTC, seconds precision, the file id
first since Drive names collide and may contain `/`. A Google-native
document (Docs/Sheets/Slides) has no bytes to back up this way, so it
backs up instead as a **lossless Drive-side copy** (`files.copy`) into the
account's configured `lease_backup_folder_id` — restorable by a human in
the Drive UI even without this tool. Configure it in `settings.json`:

```jsonc
{ "drive": { "accounts": { "work": { "lease_backup_folder_id": "1BaCkup...Folder" } } } }
```

Without it, a native-document target is refused outright, before
authenticating at all — there is nowhere configured to put the copy:

```bash
$ omni-dev drive lease acquire 1SomeGoogleSheetId
Refused: this is a Google-native document (Doc/Sheet/Slide) and no backup folder is configured for this account — set `lease_backup_folder_id` in settings.json to enable leasing native documents
```

**The backup is proven to match the recorded `version` before any token is
minted** ([ADR-0080](adrs/adr-0080.md) §2). A collaborator can edit the
file while its backup is being taken — for a large binary file that window
is the whole download — and neither backup kind can report which revision
it captured. So a byte backup's own SHA-256 is compared against the
`sha256Checksum` Drive reports for the file afterwards, and a native
document's `version` is read immediately before *and* after its
`files.copy` and must agree. If the proof fails the acquisition refuses
(status `refused-concurrent-change`), the backup is discarded, and no
lease exists — re-run `acquire` once the file is quiet (a native
document's moved `version` is first retried automatically; see below):

```bash
$ omni-dev drive lease acquire 1ExistingFileId
Refused: the file changed while its backup was being taken: Drive reports checksum 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08 at version 8, but the bytes backed up hash to 2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824. No lease was minted and the backup was discarded — retry.
```

A rename, move or permission change mid-backup bumps `version` without
touching the bytes, so a byte backup with a checksum is *not* refused for
it; a native document, having no checksum to compare, conservatively is.

Because a native document's `version` can move for reasons that are not
edits — Drive's `version` counts "every change made to the file on the
server, even those not visible to the user" — a moved `version` does not
refuse straight away. The discarded copy is replaced by a fresh one, up to
three attempts in all, after the single authentication prompt; only if
every attempt sees the `version` move does the acquisition refuse, saying
so:

```bash
$ omni-dev drive lease acquire 1SomeGoogleSheetId
Refused: the file's version changed across its backup on each of 3 attempts (last: version 12 immediately before, 13 immediately after). A version that keeps moving may be changing for reasons unrelated to edits, so waiting for the file to be quiet may not help. No lease was minted and the backups were discarded.
```

A checksum mismatch is not retried: it proves the content itself moved,
and each attempt would re-download the whole file.

**The token is an identifier, not a bearer credential** — safe to log or
paste, since a write under it still needs this account's own OAuth
credentials and folder-permission grant. Present it via `--lease`:

```bash
$ omni-dev drive edit 1ExistingFileId --content ./new-report.pdf --lease lease-abc123...
```

A refusal names what to do next:

| Refusal | Meaning |
|---|---|
| `requires a Drive write lease` | No `--lease` was presented, and the deciding write-permission rule requires one. |
| `expired, released, or unknown` | The token doesn't resolve to a live lease — it expired, was never valid, or the ledger doesn't recognise it. Acquire a new one. |
| `acquired for a different file` | The token is bound to a different file id than the one being edited. |
| `changed since the lease was acquired` | The file's Drive `version` moved since the lease was taken out (or last written under) — someone or something else edited it. Acquire a fresh lease against the current version before writing. |

The last three rows are reachable even under a `require_lease: false` rule
if a `--lease` is presented anyway — that rule opts out of *needing* one, not
of validating one that shows up (see `require_lease` above).

**Expiry is absolute and never extends.** `--expiry-minutes` (default 30)
is fixed at acquisition; a write under the lease never resets it. The only
way to get a fresh window is a fresh `drive lease acquire` — which means a
fresh authentication prompt. A lease is otherwise multi-use: each
successful write refreshes its recorded `version`, so a second write under
the same lease is checked against the file's state *after* the first, not
the original backup point.

**At most one live lease per file.** Acquiring a lease on a file that
already has an unexpired one returns that lease's existing token instead of
minting a second, independent one — two independently-checked leases on the
same file could otherwise each pass their own staleness check against a
version the other's write had already moved past, letting the second
writer silently clobber the first's change. Wait for the existing lease to
expire (or write under it) before a fresh acquisition mints a new one.

**`--biometrics-only`** requires Touch ID specifically, failing outright
rather than falling back to the account password — for operators who want
no keyboard-answerable prompt at all, at the cost of needing Touch ID
hardware. The default policy (device-owner authentication) works on every
Mac.

**A folder rule can opt out** with `require_lease: false` in
`write_permissions.rules` (default `true`), which skips both the backup
and the authentication prompt for that folder — see [Write
permissions](#write-permissions). This opts the folder out of *needing* a
lease, not out of the lease mechanism entirely: a `--lease` presented on a
write to that folder anyway is still validated, consumed, and audited —
including refusing an expired, wrong-file, or stale token — exactly as it
would be under a requiring rule.

**Global settings** (ADR-0080 §13) let this machine's operator set defaults
for `--expiry-minutes`, `--backup-dir`, and `--biometrics-only` without
passing them on every invocation, plus the headless opt-out below. They live
in a top-level `lease` block, sibling of `drive`:

```jsonc
{
  "lease": {
    "default_expiry_minutes": 60,
    "backup_dir": "/Users/alice/drive-backups",
    "biometrics_only": true,
    "allow_headless": false
  }
}
```

Each also has an env var, and every setting resolves in the same order:
the CLI flag, if given, wins outright; then the env var; then the
`settings.json` field; then the built-in default.

| Setting          | Flag                | Env var                                | Default                              |
|------------------|---------------------|----------------------------------------|--------------------------------------|
| Lease expiry     | `--expiry-minutes`  | `OMNI_DEV_DRIVE_LEASE_EXPIRY_MINUTES`  | 30                                   |
| Backup directory | `--backup-dir`      | `OMNI_DEV_DRIVE_LEASE_BACKUP_DIR`      | `<state dir>/omni-dev/drive-backups` |
| Auth policy      | `--biometrics-only` | `OMNI_DEV_DRIVE_LEASE_BIOMETRICS_ONLY` | device-owner                         |
| Headless opt-out | `--allow-headless`  | `OMNI_DEV_DRIVE_LEASE_ALLOW_HEADLESS`  | off (fails closed)                   |

For `biometrics_only`/`allow_headless`, any layer that opts in wins — there
is no way to force one back off from a lower layer once it is set.

**Off-macOS and headless**, `drive lease acquire` fails closed by default:
no authenticator is available, so no lease can ever be acquired there, and
every gated write refuses in turn. This is deliberate (ADR-0080 §8) — a TTY
prompt would let a script answer on the human's behalf, defeating the
point. An operator can explicitly waive this with `--allow-headless`, the
`OMNI_DEV_DRIVE_LEASE_ALLOW_HEADLESS` env var, or `lease.allow_headless` in
`settings.json` — the acquisition then proceeds with no human ever
prompted, and the resulting lease (and its audit record) is marked as
having used the waiver, so it stays visible after the fact.

"Headless" means no prompt can reach a human at all: off-macOS, or a macOS
session with no graphical access (an SSH login, a background launchd job,
CI). A `drive lease acquire` running in a plain SSH session refuses
immediately as `unavailable` instead of showing a prompt on the Mac's own
screen, which macOS would otherwise do, to whoever happens to be sitting
there. The check follows the process's security session, not who is typing:
a command inside a tmux/screen server started at the console and attached
over SSH, or one launched into the console session with `osascript`, still
prompts on the console. Treat it as a guard against accidental remote
prompts, not a security boundary. The waiver never
covers an **attended** Mac whose chosen policy cannot be met right now,
such as Touch ID locked out after failed attempts, not enrolled, absent, or
suspended by a closed lid under `biometrics_only`, or no passcode set.
Those still refuse as `unavailable` with `allow_headless` set, and the
message says the opt-out does not apply. Fix the underlying cause, or drop
`biometrics_only` so the password fallback can answer.

### Restore

```bash
$ omni-dev drive lease restore lease-abc123...
lease-def456...
Restored. Backed up the pre-restore content to /home/user/.local/state/omni-dev/drive-backups/20260912T000000Z-1ExistingFileId-report.pdf (expires 2026-09-12 00:30:00 UTC)
```

`drive lease restore <TOKEN>` restores a file from the backup a lease
recorded, closing the recovery gap [ADR-0077](adrs/adr-0077-sheets-deletion-via-batchupdate.md)
§5 admitted: `<TOKEN>` names the **backup** lease — the one whose row
records where the content to restore from lives — not a lease presented to
authorise this write. It locates the backup and authorises nothing itself;
restore mints its own fresh lease internally (Touch ID, a backup of the
file's *current* state, a new ledger row) before ever writing, so the
restore is itself reversible by the same verb, and prints the new token for
exactly that reason. One command, one prompt — the same `--backup-dir`/
`--expiry-minutes`/`--biometrics-only` flags `drive lease acquire` takes
apply to this fresh lease. The backup lease's `<TOKEN>` works whether it has
expired or not — an expired-but-kept row is the expected common case,
since a restore is almost always wanted after the fact, once a bad write
has been noticed — and whether it is still **live** or not: the fresh lease
*supersedes* it, releasing its row in the same locked ledger write that
records the new one, so restoring the moment a bad write is noticed never
refuses itself by naming the very token you passed (issue #1685). The file
is covered by exactly one live lease throughout — the backup lease until
the instant the fresh one replaces it — and a denied or failed prompt
leaves the backup lease exactly as it was. Releasing it costs nothing a
successful restore had not already spent: the restore write moves the
file's Drive `version` on under the *fresh* token, so the backup lease
would fail the staleness check on any later write regardless. Its row and
backup are kept, and it can be restored from again.

**Binary files restore in full**, by re-uploading the backed-up bytes —
verified against the backup's recorded SHA-256 first, so a backup that has
been corrupted or tampered with on disk since it was taken is never
silently written back to Drive.

**A spreadsheet with exactly one sheet deleted since the backup restores
that sheet**, via `spreadsheets.sheets.copyTo` from the backup spreadsheet
into the live one — the one typed native-document path ADR-0080 §10 names
worth building. Detection is structural: `restore` diffs the backup's and
the live spreadsheet's sheet-id sets (Drive's `files.copy` preserves
internal sheet ids verbatim), and restores the one id present in the backup
but missing live. It also renames the restored sheet back to its original
title when that title is currently free:

```bash
$ omni-dev drive lease restore lease-native789...
lease-def456...
Restored sheet 'Q3 Numbers' (id 1481923) back into spreadsheet 1SpreadsheetId. Backed up the pre-restore content to Drive copy 1FreshBackupCopyId (expires 2026-09-12 00:30:00 UTC)
```

**Restoring the same sheet backup twice is refused, not repeated.**
`copyTo` gives the restored sheet a *fresh* id, so the backup sheet's own id
stays missing from the live spreadsheet and the structural diff above would
happily fire again — silently adding another "Copy of …" every run. The
ledger row records the id each restore creates, and a re-run is refused while
that sheet is still there, before the authentication prompt and before the
fresh backup copy:

```bash
$ omni-dev drive lease restore lease-native789...
Refused: this backup's deleted sheet was already restored on 2026-09-12 00:00:00 UTC into spreadsheet 1SpreadsheetId as 'Q3 Numbers' (id 1481923), which is still there — restoring again would only add a second copy. Delete that sheet first if you do want another one. No fresh lease was minted, no Touch ID was spent.
```

The check keys on that sheet still being live, not on the backup merely
having been restored from before — so if the restored sheet is deleted
*again*, re-running restores it again as normal.

**Everything else native has no typed restore path.** Zero or more than one
sheet missing (nothing to restore this way, or ambiguous — this never
guesses), a Docs/Slides backup, or anything below whole-sheet granularity
(a deleted row/column/range) all fall back to reporting the backup's
location — restorable today by a human via the Drive UI:

```bash
$ omni-dev drive lease restore lease-native789...
No typed restore path exists for this backup yet — it is a Drive copy at 1BackupCopyFileId you can restore from by hand in the Drive UI
```

**The folder write-permission gate still applies.** Restore mints its own
lease, but that is a *third*, independent check alongside OAuth scope and
the write-permission gate — never a substitute for either: a write-blocked
folder refuses a restore the same way it refuses any other write.

**A backup over the 5 MB simple-upload cap can't be restored yet**, for the
same reason a byte upload that large is refused elsewhere in this CLI (see
"5 MB size cap" above) — the restore write goes through that same capped
endpoint. Refused up front from the backup's own recorded size, before any
network call, so no Touch ID prompt is spent on a restore that could never
have succeeded:

```bash
$ omni-dev drive lease restore lease-large123...
Refused: this backup is 83886080 bytes, over Drive's 5 MB simple-upload limit — restoring it is not supported yet (no fresh lease was minted, no Touch ID was spent)
```

**A file that became native since the backup was taken is also refused.**
If the target at `file_id` was binary when its backup was recorded but has
since been replaced by a Google-native document, restore refuses the write
rather than PATCHing raw bytes into it — checked immediately before the
write, since the account may have a `lease_backup_folder_id` configured
that would otherwise let the internal fresh lease acquire successfully
against the now-native file.

**The backup lease's own row is marked once restored from**, kept (never
dropped) alongside the fresh lease's new row — both remain findable by
token in the ledger and in `audit.jsonl`, which records both tokens on a
restore (`lease_id` the fresh one, `restored_from_lease_id` the backup one
read from), and the supersede on the internal acquire's own record
(`superseded_lease_id`) — see [docs/log.md](log.md#audit-log).

**If the restore write fails after the fresh lease was minted**, that fresh
token is still printed — it is real and live (Touch ID was answered, a
backup taken, a ledger row written) and must not be lost. Present it to
`--lease` for an ordinary write, but **do not restore from it**: its backup
is the file's *pre-restore* content, which is exactly what you were undoing.
Because it now covers the file, a second `restore` against the original
backup token is refused until it is stood down, which is what
[`drive lease release`](#release) is for:

```bash
$ omni-dev drive lease restore lease-abc123...
lease-def456...
Failed: the write-permission gate no longer allows this write, re-checked after the fresh lease's authentication prompt
A fresh lease was minted before the failure and is still live (expires 2026-09-12 00:30:00 UTC) — present it to `--lease` for an ordinary write.
Do not restore from it: its backup is this file's pre-restore content, which is what you were undoing.
To retry the restore, stand it down first:
  omni-dev drive lease release lease-def456...
  omni-dev drive lease restore <the original backup token>
```

A `restore` can otherwise only be refused by a live lease that is *not* the
one you passed — one acquired independently, or minted by an earlier
restore attempt whose write failed. Its token is printed so you can present
it to `--lease`, release it, or wait for it to expire.

### Release

```bash
$ omni-dev drive lease release lease-def456...
Released lease-def456... (covered file 1ExistingFileId, would have expired 2026-09-12 00:30:00 UTC). Its backup is kept — `omni-dev drive lease restore lease-def456...` still works.
```

`drive lease release <TOKEN>` ends a lease's write window early, without
waiting for it to expire — the counterpart to the absolute expiry `acquire`
fixes, which can be up to 24 hours away. It **prompts for nothing and makes
no Drive call**: releasing only ever *reduces* what a token can do, so
spending a Touch ID prompt to give up authority would be backwards (and
would leave a headless installation unable to stand a lease down at all).
It is a pure ledger mutation, and the one `drive lease` verb that needs no
Drive client — it is dispatched before credentials are even resolved, so it
works after `drive auth logout` or for an `--account` with none configured.

**The backup is kept.** Release ends the lease's authority to write, not its
usefulness: `drive lease restore <TOKEN>` looks a row up by token and never
requires it to be live, so a released lease's content stays recoverable
until [`drive lease prune`](#prune) drops the row and its backup together.
One knock-on effect worth knowing: a *live* row never enters `prune
--max-size`'s budget, so releasing a lease before its expiry adds its local
backup bytes to that budget straight away — bounded by the window the lease
had left. It cannot cost the released row its own backup in favour of an
*expired* one: candidates sort newest-`expires_at`-first, and a released
row's expiry is still in the future, so every expired row is evicted before
it. Only another early-released row can outrank it, by expiring later.
`--older-than` is unaffected: it compares `expires_at`, which release never
moves.

An already-expired or already-released token is reported rather than
silently re-stamped, so an earlier release's timestamp is never overwritten:

```bash
$ omni-dev drive lease release lease-def456...
Nothing to do: lease lease-def456... is not live — it was already released on 2026-09-12 00:05:00 UTC. Its backup is unaffected and still restorable.
```

Every attempt writes a best-effort `audit.jsonl` record (`verdict:
"released"`/`"release-not-live"`/`"release-no-such-token"`/`"failed"`,
`lease_id` the token presented) — see [docs/log.md](log.md#audit-log).

### Prune

```bash
$ omni-dev drive lease prune --older-than 30d --dry-run
$ omni-dev drive lease prune --older-than 30d
```

`drive lease prune` bounds the ledger's and the backup directory/folder's
otherwise-unbounded growth ([ADR-0080](adrs/adr-0080.md) Consequences,
#1678) by dropping expired rows together with the backups they point at.
It mirrors [`omni-dev log prune`](log.md#omni-dev-log-prune)'s shape:

| Flag | Effect |
|------|--------|
| `--older-than <DUR>` | Drop non-live rows whose expiry is strictly before this relative window (`7d`, `24h`, `2w`). A row expiring exactly at the cutoff survives. |
| `--max-size <SIZE>` | After age pruning, additionally drop the oldest-expiring survivors until their local backup bytes total at most `<SIZE>` (`10mb`, `512kb`, or a bare byte count). A Drive-copy backup counts as zero local bytes, so it's only reachable through `--older-than`. |
| `--dry-run` | Report what would be removed without deleting/trashing any backup or modifying the ledger. |

At least one of `--older-than`/`--max-size` is required. A **live** lease
(unexpired and unreleased) is never a removal candidate regardless of
either bound — pruning can never invalidate a lease a write is still
relying on. `--max-size` always keeps at least the single
most-recently-expired row's backup, even if it alone exceeds the budget.

A second case is exempted from the `--max-size` budget the same way a
Drive-copy backup is (issue #1768): a row released because
[`drive lease restore`](#restore) superseded it (ADR-0080 §10) stays
exempt for as long as the restore that used it as its source never
actually completed — the write failed after the fresh lease was
minted (`FreshLeaseButWriteFailed`). Such a row remains the
file's only real backup, while the superseding lease's own backup is a
pre-restore snapshot nothing ever wrote over and is comparatively
worthless — and the superseding lease's `expires_at` is always later,
since it was minted after the row it replaced. Without the exemption, a
tight budget could compete the two by raw `expires_at` and evict the
wanted row in favor of the useless one. A row released by a *successful*
restore keeps competing normally, since its content is live again and
the superseding lease's own backup is now meaningful too; so does a row
released by a plain `drive lease release` (the case discussed
[above](#release)). Like the Drive-copy case, an exempt row is reachable
only through `--older-than`.

A row and the backup it points at are always dropped **together, never one
without the other**: a byte backup is deleted from local disk, a
Drive-copy backup is moved to Drive Trash (recoverable by hand for ~30
days via the Drive UI) — and the ledger row is dropped, with that removal
persisted to disk, only once its own backup has been cleared (or found
already gone). Persistence happens one row at a time, not batched across
the whole run, so an interrupted prune (a crash, a killed process) can
leave at most the one row it was working on inconsistent with its
already-cleared backup — never the rest of the run. The ledger lock
itself is likewise taken only per row, not for the whole run — but each
row now holds it across *both* that row's backup deletion/trash call and
its ledger removal, secured *before* the backup is touched (issue #1687):
a lock collision on one row is therefore fully recoverable (neither the
backup nor the row has been touched yet) and simply leaves that row for a
future prune, rather than the old failure mode of deleting a backup and
then being unable to record its row as gone. The trade-off is that a large
batch can now hold the lock for a row's full Drive API round trip, not
just its local disk I/O — a concurrent leased write queues behind it
rather than failing outright (see [Concurrent access](#concurrent-access)
below), but a very large prune run can make one wait noticeably longer. A
backup deletion/trash failure for one row (e.g. a transient Drive error),
or a failure to lock the ledger for one row, is logged and skips just that
row, leaving it for a future prune run, rather than failing the whole
command.

### Concurrent access

Two overlapping `drive lease acquire`/write/prune/restore invocations
against the same ledger are serialized by an advisory lock — a
`flock(2)` on a persistent `<ledger-path>.lock` sibling file, kernel-
released on process death, so a crashed or killed holder never leaves a
stale lock. **Never delete this lock file by hand** — it is not a marker
of anything being wrong, and nothing in this codebase ever advises
deleting it. (This holds on Unix; on non-Unix platforms, where `flock`
isn't available, the lock falls back to the older create-and-delete
marker scheme, so a crashed holder there can still leave a stale lock.) A leased write, `drive lease acquire` and `drive lease release`
each wait for a busy lock rather than failing outright (printing a
one-line notice while they do), up to
`OMNI_DEV_LEASE_LOCK_WAIT_SECS` (default: four times the HTTP read
timeout, since a held lock can span several sequential Drive calls, e.g.
`drive lease restore`'s copy-then-edit-then-rename sequence). The lock
is **ledger-global**, not per-file: a write to one file and a concurrent
write to a *different* file still serialize against each other, they
just wait instead of hard-failing. `drive lease prune` is the exception:
it does not wait, and a row whose lock it cannot take is simply left
for a future prune.

```bash
$ omni-dev drive lease prune --older-than 30d
Removed 12 lease(s); kept 4 (3 trashed Drive backup(s), 0 failure(s), freed 8241203 bytes of local backups).
```

Every removal attempt — successful or failed — writes its own best-effort
`audit.jsonl` record (`verdict: "pruned"` or `"prune-failed"`, the latter
carrying the underlying error), the same fail-open posture `drive lease
acquire`'s own audit trail uses, so `omni-dev log --audit` can always
answer "why is this backup gone" for a specific lease. This is distinct
from `audit.jsonl` itself being out of scope *as a pruning target*: the
file is append-only forensic history by design
([ADR-0080](adrs/adr-0080.md) §11) and `drive lease prune` never rotates
or deletes its content, the same exemption it has from
`OMNI_DEV_LOG_DISABLE` and `omni-dev log prune`'s own rotation — see
[docs/log.md](log.md#audit-log).

### Exit codes

`acquire`, `restore` and `release` each exit **`0`** when the caller ends
up holding what they asked for, and **`1`** for every refusal, denial or
failure — under every `-o` format, not just `table`. This is narrower than
[Move](#move)'s "exit code is always 0, check the output" convention: a
move is a batch operation with one outcome per file, so no single exit
code could ever represent all of them, while a single `acquire`/`restore`/
`release` call has exactly one outcome, so its exit code can name it
(issue #1775).

The `0` outcomes include two idempotent-reuse cases, not just the obvious
ones:

| Command   | Exits `0`                    |
|-----------|-------------------------------|
| `acquire` | `Acquired`, and `AlreadyLeased` (a live lease already covers the file — its own token is returned for reuse) |
| `restore` | `Restored`, `RestoredSheet` |
| `release` | `Released`, and `NotLive` (the token names a lease that was already expired or released — nothing to do, not a failure) |

Everything else exits `1`. Two outcomes are easy to misjudge from their
name or their payload alone:

- **`restore`'s own `AlreadyLeased` is *not* one of the `0` cases above**,
  despite sharing a name with `acquire`'s. It means some *other*, unrelated
  live lease blocked this restore — nothing was restored — and names that
  lease's token so it can be presented to `--lease`, released, or waited
  out; see [Restore](#restore).
- **`FreshLeaseButWriteFailed` still prints a real, usable token** — Touch
  ID was answered, a backup taken, a ledger row written — but exits `1`
  regardless, because the restore write itself did not go through.

A script that only checks the exit code — `omni-dev drive lease acquire
"$ID" > /tmp/out || exit 1` — can now rely on it; one that also wants the
lease token or the refusal detail still reads the output as before.

## Sheets

`drive sheets` reads and writes the *cells* of a Google Sheet through the
Sheets v4 API (issue #1589, [ADR-0073](adrs/adr-0073.md)), and edits its
*structure* (issue #1613, [ADR-0075](adrs/adr-0075.md)). The Drive API
cannot do either: it treats a Sheet as an opaque native document with no notion of a range,
a row or a cell. In particular, `drive read --content` on a Sheet exports **the
first sheet only**, because Drive's export API has no multi-sheet CSV format —
`drive sheets read` is the way to get the rest.

No new login flag is needed. Reading works with the `drive.readonly` scope
every account already has.

#### `drive sheets info`

Shows the workbook title and the sheets (tabs) it contains, with each grid
sheet's allocated dimensions. Hidden sheets are listed and marked, not omitted.

```bash
$ omni-dev drive sheets info 1AbC_dEfGhIjKlMnOpQrStUvWxYz
Id: 1AbC_dEfGhIjKlMnOpQrStUvWxYz
Title: 2026 Budget
Sheets: 3
  Q1 (1000x26)
  Q2 (1000x26)
  Notes [hidden]
```

#### `drive sheets read`

With neither `--range` nor `--sheet`, reads **every** sheet: one
`spreadsheets.get` for the tab list, then `values.batchGet` for the data.

```bash
$ omni-dev drive sheets read 1AbC_dEfGhIjKlMnOpQrStUvWxYz
# Q1
Region,Revenue
North,1200
South,950

# Q2
Region,Revenue
North,1310
```

Narrow it with `--sheet` (a tab title), `--range` (an A1 range), or both:

```bash
omni-dev drive sheets read <ID> --sheet 'Q1'
omni-dev drive sheets read <ID> --range 'A1:B10'
omni-dev drive sheets read <ID> --sheet 'My Sheet' --range 'A1:B10'
omni-dev drive sheets read <ID> --range "'My Sheet'!A:A"
```

`--range` may carry its own `Sheet!` prefix. Passing `--sheet` *as well as* a
prefixed `--range` is an error rather than a precedence rule, since the two can
disagree and guessing would read the wrong sheet. Sheet titles are always
quoted internally, so titles containing spaces, apostrophes or `!` need no
special handling — and a sheet literally titled `A1` is unambiguous.

Unbounded and open-ended ranges are passed through untouched (`A:A`, `1:2`,
`A5:A`, a bare sheet name, or a defined name). `omni-dev` deliberately does not
validate A1 grammar client-side; the server is authoritative and returns a
clearer error than a local guess would.

**Output formats.** The default `-o table` emits CSV, which is what a grid of
cells is. When more than one sheet is read, each block is preceded by a
`# <title>` comment line and separated by a blank line.

Two differences between CSV and the structured formats are worth knowing:

- **CSV pads rows; JSON/YAML do not.** The API truncates trailing empty cells
  from each row, so rows come back ragged. CSV pads each row to the widest row
  in that sheet, because a ragged CSV is malformed. `-o json` and `-o yaml`
  preserve the raggedness, which is the truthful shape.
- **CSV emits cell content verbatim.** Cell values are content, not chrome, so
  they are not stripped of control characters — a multi-line cell survives
  intact as a properly quoted CSV field. Sheet *titles*, which are rendered as
  chrome, are sanitised.

`-o json`/`-o yaml` emit an ordered **list** of `{title, values}` objects
rather than a `{title: rows}` map, so workbook order is preserved:

```bash
omni-dev drive sheets read <ID> -o json
```

**`--render`** controls how the API renders each cell:

| Value         | Meaning                                                   |
|---------------|-----------------------------------------------------------|
| `formatted`   | Locale-formatted strings as displayed in the UI (default) |
| `unformatted` | Raw typed values — JSON numbers and booleans, not strings |
| `formula`     | The formula text (`=SUM(A1:A3)`) rather than its result   |

`unformatted` is usually what you want when feeding the output to something
that will do arithmetic on it; `formatted` matches what `drive read --content`
already produces for a Sheet.

#### `drive sheets write` / `append` / `clear`

Writing cells is gated by the folder [write permissions](#write-permissions)
under the **`sheets-write`** operation, and needs the `drive.file` or `drive`
scope (`drive auth login --write-file` / `--write-full`). `drive.file` reaches
only Sheets `omni-dev` itself created; a pre-existing Sheet needs
`--write-full`.

```bash
# Overwrite a range from a CSV file
omni-dev drive sheets write <ID> --range 'A1:B10' --values ./cells.csv

# Append rows after the end of a table, from stdin
printf 'North,1200\nSouth,950\n' | omni-dev drive sheets append <ID> --range 'A:B' --values -

# Clear a range's values, leaving formatting intact
omni-dev drive sheets clear <ID> --range 'Q1!A2:B100'
```

**Always dry-run first.** `--dry-run` reports the gate verdict *and* the
parsed dimensions, which is how you catch a transposed or ragged input before
it lands:

```bash
$ omni-dev drive sheets write <ID> --range 'A1:B10' --values ./cells.csv --dry-run
Would write: 10 row(s) x 2 column(s) into A1:B10 of '2026 Budget'
```

A dry run makes no Sheets API call and writes no request-log record, matching
`create`/`upload`/`edit`.

**`--values`** takes a file path or `-` for stdin. CSV by default; JSON (an
array of arrays) when the path ends in `.json` or `--values-format json` is
given. Ragged rows are preserved rather than padded — padding would write
empty strings over cells you never mentioned. The first CSV row is **data,
not a header**.

**`--input` is the one option whose wrong value silently mangles data:**

- **`user-entered`** (default) — parse each value as if typed into the UI:
  `=SUM(A1:A3)` becomes a formula, `2026-09-06` a date, `1,234` a number.
- **`raw`** — store every value verbatim as text; a leading `=` stays literal
  rather than becoming a formula.

Neither errors on the "wrong" choice — you get formulas you meant as text, or
text you meant as formulas. The dry run echoes nothing about this, so decide
it deliberately.

**Refusals you may see**, each distinct from a rule denial:

- *not a Google Sheet* — the id points at something else. Checked before the
  gate; the operation is meaningless rather than disallowed.
- *is a shortcut* — shortcuts are never followed. Resolve the target
  spreadsheet's id and use that.
- *no parent folder visible to this account* — the Sheet was shared with you
  by link or email and is not in a folder you can see, so it has no ancestor
  chain and no `folder_id` rule could ever grant it. Grant it directly with a
  `file_id` rule instead — see
  ["Granting a file shared with you"](#granting-a-file-shared-with-you). A
  `file_id` rule only satisfies the local gate, though: writing to a Sheet
  `omni-dev` didn't create also needs the `--write-full` scope, since
  `--write-file` (`drive.file`) only reaches files `omni-dev` itself
  created.

Exit code is 0 whether the write succeeded, was blocked, or failed — inspect
the output, not `$?`.

#### `drive sheets create`

Creates a spreadsheet, optionally seeded with values. Gated under the
**`create`** operation, not `sheets-write` — the same rule that governs
`drive create`.

```bash
omni-dev drive sheets create --name '2027 Budget' --parent <FOLDER_ID>
omni-dev drive sheets create --name '2027 Budget' --parent <FOLDER_ID> --values ./seed.csv
```

Without `--values` this is shorthand for
`drive create --mime-type application/vnd.google-apps.spreadsheet`, which
does the same thing; the reason it exists is `--values` and being
discoverable inside the `sheets` tree.

**The seeding write is not separately gated.** A folder that grants `create`
but not `sheets-write` can still be seeded: the `create` verdict authorises
the pair. That is safe only because the id being written is always the one
`files.create` just returned inside an already-cleared folder, never
something you supplied — gating it separately would make `--values` unusable
in a create-only folder for no gain. See [ADR-0073](adrs/adr-0073.md) §11.

**If seeding fails after the spreadsheet is created**, you get a *partial
failure* naming the new file id, because there is no `files.delete` anywhere
in this integration and the empty spreadsheet cannot be rolled back
automatically:

```
Partially failed: created '2027 Budget' (1AbC…) in <FOLDER_ID>, but writing
its values failed: … The spreadsheet exists and is empty — it cannot be
rolled back automatically.
```

Delete it yourself if you don't want it.

#### drive sheets add-sheet / rename-sheet / insert-rows / insert-columns / duplicate-sheet / reorder-sheet / hide-sheet / show-sheet

Structural edits — changing the *shape* of a workbook rather than its cell
values. Gated by the separate `sheets-structure` operation (above), so a
folder granted `sheets-write` cannot be restructured without an explicit
additional grant.

Every one of these previews with `--dry-run` first:

```bash
# What would change, and to what — no mutation is attempted.
omni-dev drive sheets insert-rows <ID> --sheet Q2 --at 5 --count 3 --dry-run
```

```
Would insert 3 row(s) before row 5 of 'Q2' (sheetId 118293) in 'Budget'
  (500 rows -> 503; existing rows 5-500 shift down)
```

That second line is the point of a structural dry run. An insert's effect
isn't expressible as a range — it shifts everything below it — so the
preview names the resulting dimension *and* the shift, read from the sheet's
real current size rather than assumed.

```bash
# Add a tab. --rows/--columns are optional; omitted takes Sheets' own
# defaults (1000 x 26) rather than a size omni-dev invents.
omni-dev drive sheets add-sheet <ID> --title Q3
omni-dev drive sheets add-sheet <ID> --title Q3 --index 2 --rows 200 --columns 8

# Rename a tab, by its current title.
omni-dev drive sheets rename-sheet <ID> --sheet Q2 --title 'Q2 (final)'

# Insert rows or columns. --at is 1-based and inclusive — the row or column
# number the spreadsheet itself shows — and inserts *before* it.
omni-dev drive sheets insert-rows <ID> --sheet Q2 --at 5 --count 3
omni-dev drive sheets insert-columns <ID> --sheet Q2 --at 2
```

`--at 5` puts the new rows above the current row 5. Column A is 1.
`--count` defaults to 1.

```bash
# Copy a sheet. --title omitted takes Sheets' own "Copy of X" default;
# --index omitted takes Sheets' own default position — confirmed against
# the live API to be the front of the workbook (index 0), not the end,
# unlike add-sheet. A given --title must not already be in use, including
# by the source sheet itself.
omni-dev drive sheets duplicate-sheet <ID> --sheet Q2 --title 'Q2 (copy)'

# Move a sheet to a new zero-based position among its siblings.
omni-dev drive sheets reorder-sheet <ID> --sheet Q2 --index 0

# Hide/show a tab. Hiding the workbook's last visible sheet is refused —
# Sheets requires at least one to stay visible.
omni-dev drive sheets hide-sheet <ID> --sheet Q2
omni-dev drive sheets show-sheet <ID> --sheet Q2
```

Several refusals are specific to these verbs, and all of them are checked
before anything is written so a `--dry-run` can never promise a change the
real run then fails:

```
Refused: 'Budget' has no sheet titled 'Nope'. Available: 'Q1', 'Q2'
Refused: 'Budget' already has a sheet titled 'Q1'
Refused: --at 502 is past the end of the sheet, which has 500 row(s); the furthest valid position is 501
```

The duplicate-title refusal applies to `rename-sheet` too — renaming a
sheet to a title a *different* sheet already has fails the same way
`add-sheet` does. Renaming a sheet to the title it already has is not a
collision, since it names itself rather than a different sheet.
The *positions* — `--at` on `insert-rows`/`insert-columns` and `--index` on
`add-sheet` — are checked against the workbook's actual current size. `--at`
may name one past the sheet's last row/column (that's a valid append), never
further. The *counts* — `--count`, `--rows`, `--columns` — are checked only
for being positive: the workbook's state implies no upper bound on how much
you may add, so `omni-dev` doesn't invent one, and Sheets remains the
authority on how large a sheet may actually get. A count large enough to
overflow the row/column index space is refused rather than sent.

#### drive sheets delete-sheet / delete-rows / delete-columns / delete-range

Destructive edits — the same `spreadsheets.batchUpdate` mechanism as above,
but these actually remove data. Gated by the separate `sheets-delete`
operation, **not** `sheets-structure`: a folder granted `sheets-structure`
cannot delete anything without an explicit additional grant, and vice versa.
See [ADR-0077](adrs/adr-0077-sheets-deletion-via-batchupdate.md).

There is still no interactive confirmation and no `--force` anywhere in this
tool, deletion included — the permission gate and an honest `--dry-run` are
the whole consent mechanism, the same as every other write in this
integration. And there is still no raw `spreadsheets.batchUpdate` request
array: every verb, destructive or not, is its own typed command.

```bash
# What would be destroyed — no mutation is attempted.
omni-dev drive sheets delete-rows <ID> --sheet Q2 --at 5 --count 3 --dry-run
```

```
Would delete 3 row(s) 5-7 of 'Q2' (sheetId 118293) in 'Budget'
  (500 rows -> 497; existing rows 8-500 shift up; formulas elsewhere in the
   workbook that reference the deleted rows may break, which cannot be
   checked automatically)
```

That caveat is deliberate and load-bearing: checking whether some other
sheet's formula references what would be deleted would mean reading the
whole workbook's formulas, not just the target's own dimensions, and
`--dry-run` for a destructive verb stays exactly as structural as the
additive one above — no extra `values.get` read, no cell content in its
output or the request log.

```bash
# Delete an entire tab. Cannot be undone through omni-dev.
omni-dev drive sheets delete-sheet <ID> --sheet Q2

# Delete rows or columns. --at is 1-based inclusive, same as insert-rows.
omni-dev drive sheets delete-rows <ID> --sheet Q2 --at 5 --count 3
omni-dev drive sheets delete-columns <ID> --sheet Q2 --at 2

# Delete a rectangular range, shifting what remains up or left to close the
# gap. All four bounds are required — an open-ended span is delete-rows/
# delete-columns's job, not this one's.
omni-dev drive sheets delete-range <ID> --sheet Q2 \
  --start-row 2 --end-row 4 --start-column 2 --end-column 3 --shift rows
```

Every real (non-`--dry-run`) delete says how to recover — there is still
no `files.delete` in this integration, and `drive lease restore` (below)
is only a partial undo. The `--lease` these verbs require
([ADR-0080](adrs/adr-0080.md) §9) backed the whole
spreadsheet up as a Drive copy when it was acquired, so that copy — named
by its file id, not Drive's own version history — is the primary recovery
path:

```
Deleted sheet 'Q2' (sheetId 118293) from 'Budget'; this cannot be undone
through omni-dev — the lease this write required backed the whole
spreadsheet up when it was acquired (Drive copy 1AbC…); run `omni-dev
drive lease restore <TOKEN>` — it restores a single deleted sheet
automatically, or otherwise locates the copy to restore from by hand in
the Drive UI — or fall back to Google Drive's own version history
```

Two things the wording is careful about. The copy dates from **acquisition**,
not from immediately before this delete: a lease is multi-use for its
lifetime ([ADR-0080](adrs/adr-0080.md) §5), so earlier writes under the
same token are not in it. And a folder whose deciding rule sets
`require_lease: false` takes no backup at all, so a delete there says
``no lease backup was taken (the deciding write-permission rule sets
`require_lease: false`), so Google Drive's version history is the only
recovery path`` rather than pointing at a copy that does not exist. The
`--output json` outcome carries the same copy as a `backup` field
(`{"kind": "drive_copy", "file_id": …}`), omitted when none was taken.
`drive lease restore` ([ADR-0080](adrs/adr-0080.md) §10) is named here for
what it actually does today: deleting exactly one sheet is the one typed
path it restores automatically, via `spreadsheets.sheets.copyTo` (see
[Restore](#restore) below) — every other shape here (multiple sheets,
rows, columns or a range) it only *locates* the copy for, restoring it
into the live spreadsheet is still a manual Drive-UI copy-back.

The same bounds-checking as `insert-rows`/`insert-columns` applies, inverted:
`--at`/`--count` (or the range bounds) must name rows/columns/cells that
already exist — deletion has no append-boundary case, since everything named
must be real.

#### drive sheets format-cells / update-borders / merge-cells / unmerge-cells / auto-resize-dimension / update-dimension-properties

Cell and border formatting, merging, and row/column sizing. Also gated by
`sheets-structure` — see [ADR-0078](adrs/adr-0078.md).

```bash
# Format cells. At least one property flag is required; the batchUpdate
# fields mask sent is built from exactly the flags given.
omni-dev drive sheets format-cells <ID> --sheet Q2 --range A1:D1 \
  --bold true --background '#FFFF00'

# The CellFormat subset now also reaches font family, text rotation,
# hyperlink display type, padding and text direction (#1791) — the union
# --text-rotation-angle/--text-rotation-vertical is mutually exclusive.
omni-dev drive sheets format-cells <ID> --sheet Q2 --range B2:B100 \
  --number-format '#,##0.00' --number-format-type currency \
  --font-family Arial --padding-top 4 --padding-bottom 4

# Borders: at least one of --top/--bottom/--left/--right/--all/
# --inner-horizontal/--inner-vertical. --all covers only the four outer
# edges; the two inner-grid-line flags need to be named explicitly.
omni-dev drive sheets update-borders <ID> --sheet Q2 --range A1:D1 --all \
  --style solid-medium --color '#000000'

# Merging discards every value but the top-left's. --dry-run lists exactly
# which cells and values would be lost — read it before running for real.
omni-dev drive sheets merge-cells <ID> --sheet Q2 --range A1:D1 --dry-run
omni-dev drive sheets unmerge-cells <ID> --sheet Q2 --range A1:D1

# Resize rows/columns. --start/--end are 1-based and inclusive.
omni-dev drive sheets auto-resize-dimension <ID> --sheet Q2 \
  --dimension columns --start 1 --end 4
omni-dev drive sheets update-dimension-properties <ID> --sheet Q2 \
  --dimension columns --start 1 --end 1 --pixel-size 200
```

```
Would merge (MERGE_ALL), discarding 3 cell(s): B1: old note; C1: 12; D1: draft
```

`format-cells` can never write a *value* — it builds a `repeatCell` request
whose payload has no field to put one in, regardless of what flags are
given, so a `sheets-structure` grant that lets you reformat a workbook can
never be used to change what it says.

#### drive sheets set-data-validation / clear-data-validation

Restricts what may be entered into a range. Also gated by
`sheets-structure`.

```bash
# Exactly one condition flag is required.
omni-dev drive sheets set-data-validation <ID> --sheet Q2 --range C2:C100 \
  --one-of-list Draft,Final,Archived
omni-dev drive sheets set-data-validation <ID> --sheet Q2 --range D2:D100 \
  --number-between 0 100
omni-dev drive sheets set-data-validation <ID> --sheet Q2 --range E2:E100 --checkbox
omni-dev drive sheets set-data-validation <ID> --sheet Q2 --range F2:F100 \
  --custom-formula '=F2<=D2'

# Tranche 2 (#1792): a dropdown sourced from a range, numeric comparators,
# text conditions, date conditions (absolute or relative), and blank checks.
omni-dev drive sheets set-data-validation <ID> --sheet Q2 --range G2:G100 \
  --one-of-range 'Lists!A1:A10'
omni-dev drive sheets set-data-validation <ID> --sheet Q2 --range H2:H100 \
  --number-greater 0
omni-dev drive sheets set-data-validation <ID> --sheet Q2 --range I2:I100 \
  --text-contains '@example.com'
omni-dev drive sheets set-data-validation <ID> --sheet Q2 --range J2:J100 \
  --date-after today
omni-dev drive sheets set-data-validation <ID> --sheet Q2 --range K2:K100 --not-blank

# --show-warning allows an invalid entry through with a warning instead of
# rejecting it outright (the default).
omni-dev drive sheets clear-data-validation <ID> --sheet Q2 --range C2:C100
```

Tranche 1 (#1643) shipped `--one-of-list`, `--number-between`, `--checkbox`,
and `--custom-formula`. Tranche 2 (#1792) added every remaining condition
type addressable with a flat flag: `--one-of-range`; the numeric comparators
(`--number-not-between`, `--number-greater(-eq)`, `--number-less(-eq)`,
`--number-eq`, `--number-not-eq`); the text conditions (`--text-contains`,
`--text-not-contains`, `--text-starts-with`, `--text-ends-with`,
`--text-eq`); the date conditions (`--date-after`, `--date-before`,
`--date-on`, `--date-between` — the single-value forms also accept a
relative keyword: `today`, `tomorrow`, `yesterday`, `past-week`,
`past-month`, `past-year`); and `--blank`/`--not-blank`. Still not reachable,
a documented cut rather than a silent gap: `TEXT_IS_EMAIL`, `TEXT_IS_URL`,
`DATE_ON_OR_BEFORE`, `DATE_ON_OR_AFTER`, `DATE_NOT_BETWEEN`,
`DATE_IS_VALID`, and every condition type meaningful only inside a
conditional-format rule.

#### drive sheets set-developer-metadata / delete-developer-metadata / search-developer-metadata

Key/value pairs attached to a spreadsheet, sheet, row or column — the
channel other add-ons key their own state on. Also gated by
`sheets-structure` (issue #1795, [ADR-0081](adrs/adr-0081.md) §4).
`--sheet`/`--dimension`/`--start`/`--end` are optional on all three and
compose into one of three locations: none of them means the whole
spreadsheet, `--sheet` alone means the whole sheet, and all four together
mean a row or column span.

```bash
# Spreadsheet-scoped: no --sheet/--dimension/--start/--end at all.
omni-dev drive sheets set-developer-metadata <ID> --key owner --value team-a

# Sheet-scoped.
omni-dev drive sheets set-developer-metadata <ID> --key owner --value team-a \
  --sheet Q2

# Row/column-scoped, 1-based and inclusive like every other --start/--end
# pair in this crate.
omni-dev drive sheets set-developer-metadata <ID> --key source --value import \
  --sheet Q2 --dimension rows --start 2 --end 100

# Re-running set-developer-metadata with an existing key and location
# updates its value instead of creating a duplicate entry.
omni-dev drive sheets set-developer-metadata <ID> --key owner --value team-b

# --dry-run reports every entry that would be removed before it happens.
omni-dev drive sheets delete-developer-metadata <ID> --key owner --dry-run
omni-dev drive sheets delete-developer-metadata <ID> --key owner

# search-developer-metadata is read-only and ungated. Omit --key and every
# location flag to list every DOCUMENT-visibility entry in the workbook.
omni-dev drive sheets search-developer-metadata <ID>
omni-dev drive sheets search-developer-metadata <ID> --sheet Q2
```

**There is no `--visibility` flag.** `DeveloperMetadata` carries a
visibility of `DOCUMENT` or `PROJECT`; `PROJECT`-visibility metadata
belongs to whatever OAuth client created it, not to this tool, so every
request this surface sends is hardcoded to `DOCUMENT` and every response it
reads is checked against it — there is no way, from the CLI or otherwise,
to reach a `PROJECT`-visibility entry through `drive sheets`.
`delete-developer-metadata` bulk-removes: since a key/location filter can
match more than one entry, the preview (and the real run) lists every entry
it applies to, not just one.

#### drive sheets add-conditional-format / update-conditional-format / delete-conditional-format / list-conditional-formats

Conditional formatting rules — a `BooleanRule` (a condition-triggered
format) or a `GradientRule` (a color scale). Also gated by
`sheets-structure` (issue #1793, [ADR-0081](adrs/adr-0081.md) §1).

Rules are an **ordered list per sheet, addressed by index** — deleting a
rule shifts every later index. `update-conditional-format`/
`delete-conditional-format`'s `--index` is only valid against a snapshot
just read, so run `list-conditional-formats` (a plain, ungated read, like
`list-protections`) immediately before acting to confirm the index is still
current. `--dry-run` on `update`/`delete` echoes the rule *currently* at the
given index alongside the change that would be made, so a stale index is
visible before it's acted on.

```bash
# A BooleanRule: exactly one condition flag, plus at least one of
# --background/--text-color/--bold.
omni-dev drive sheets add-conditional-format <ID> --sheet Q2 --range C2:C100 \
  --number-greater 100 --background '#FF0000' --bold true

# A GradientRule: --gradient-min-color/--gradient-max-color, plus an
# optional --gradient-mid-color/--gradient-mid-type/--gradient-mid-value.
omni-dev drive sheets add-conditional-format <ID> --sheet Q2 --range D2:D100 \
  --gradient-min-color '#FFFFFF' --gradient-max-color '#00FF00' \
  --gradient-mid-color '#FFFF00' --gradient-mid-type percent --gradient-mid-value 50

# A rule can span more than one range — repeat --range.
omni-dev drive sheets add-conditional-format <ID> --sheet Q2 \
  --range C2:C100 --range D2:D100 --cell-empty --background '#CCCCCC'

# See what exists, and at what index — a plain, ungated read.
omni-dev drive sheets list-conditional-formats <ID>

# update-conditional-format replaces the whole rule at --index, ranges
# included; it does not move a rule to a different index.
omni-dev drive sheets update-conditional-format <ID> --sheet Q2 --index 0 \
  --range C2:C100 --number-greater 200 --background '#FF0000'

omni-dev drive sheets delete-conditional-format <ID> --sheet Q2 --index 1
```

Unlike `set-data-validation`, `--sheet` is required on `add`/`update` (a
rule's ranges must all share one sheet). The condition set is curated the
same way `set-data-validation`'s is, cut to a different boundary: dropdown
types (`ONE_OF_LIST`/`ONE_OF_RANGE`/`CHECKBOX`) don't apply to a format
trigger, so they're absent here; `--cell-empty`/`--cell-not-empty` are
present instead, since they're meaningful only as a format trigger. Still
not reachable, the same documented cut `set-data-validation` names:
`TEXT_IS_EMAIL`, `TEXT_IS_URL`, `DATE_ON_OR_BEFORE`, `DATE_ON_OR_AFTER`,
`DATE_NOT_BETWEEN`, `DATE_IS_VALID`. `GradientRule`'s two endpoints are
always anchored `MIN`/`MAX`; Sheets also allows an endpoint anchored at an
explicit `NUMBER`/`PERCENT`/`PERCENTILE` value, which is not reachable here.

#### drive sheets protect-range / update-protection / unprotect-range / list-protections

Protected ranges, gated by the **separate `sheets-protection`** operation —
not `sheets-structure`. See [ADR-0078](adrs/adr-0078.md) §2 for why: a
protected range is a permission inside the document, not a structural
change.

```bash
# Protect a range, or an entire sheet with --whole-sheet.
omni-dev drive sheets protect-range <ID> --sheet Q2 --range A1:A10 \
  --description 'Locked headers' --editor teammate@example.com
omni-dev drive sheets protect-range <ID> --sheet Signed --whole-sheet --description Final

# See what's protected — a plain, ungated read.
omni-dev drive sheets list-protections <ID>

# Change or remove an existing protection, resolved by exact range match.
omni-dev drive sheets update-protection <ID> --sheet Q2 --range A1:A10 \
  --add-editor another@example.com --remove-editor teammate@example.com
omni-dev drive sheets unprotect-range <ID> --sheet Q2 --range A1:A10

# A whole-sheet protection has no range of its own — --whole-sheet is the
# only way to update-protection/unprotect-range one.
omni-dev drive sheets unprotect-range <ID> --sheet Signed --whole-sheet
```

`update-protection`/`unprotect-range` need the *exact* range (or, with
`--whole-sheet`, the exact sheet) a protection covers — `list-protections`
is how you find it, since Sheets exposes no other user-facing handle. An
ambiguous or non-matching target is refused rather than guessed at. Sheets
has no incremental editor add/remove either: `--add-editor`/
`--remove-editor` compute the full resulting list from the protection's
current editors before sending it.

**Limits.** A whole-workbook read refuses a spreadsheet beyond a fixed sheet
count rather than returning part of it — silently returning half a workbook is
indistinguishable from a workbook that small. Narrow the read with `--sheet` or
`--range` if you hit it.

#### drive sheets set-basic-filter / clear-basic-filter / add-filter-view / update-filter-view / delete-filter-view / list-filter-views

The basic filter and filter views (issue #1794), gated by
`sheets-structure` like formatting and data validation — see
[ADR-0081](adrs/adr-0081.md): a filter hides rows, which is view state, not
data. The two are shaped differently: a sheet has **at most one** basic
filter, so `set-basic-filter` is an upsert and `clear-basic-filter` needs
only `--sheet`. Filter views are **many, named and id-addressed** —
`list-filter-views` is how you discover a view's numeric id, the same way
`list-protections` is for protected ranges.

```bash
# The basic filter — one per sheet.
omni-dev drive sheets set-basic-filter <ID> --sheet Q2 --range A1:D100 \
  --sort-by 0:asc --hide-values 1:Discontinued,Returned
omni-dev drive sheets clear-basic-filter <ID> --sheet Q2

# Filter views — many per sheet, addressed by id.
omni-dev drive sheets add-filter-view <ID> --sheet Q2 --range A1:D100 \
  --title 'Open only' --hide-values 2:Closed
omni-dev drive sheets list-filter-views <ID>
omni-dev drive sheets update-filter-view <ID> --filter-view-id 3 \
  --hide-values 2:Closed,Cancelled
omni-dev drive sheets delete-filter-view <ID> --filter-view-id 3
```

`--sort-by`/`--hide-values` take `COLUMN:...` pairs, where `COLUMN` is a
0-based column index (not an A1 letter) — the same indexing the underlying
API uses. `update-filter-view`'s `--sort-by`/`--hide-values` **merge** onto
the view's existing sort order and criteria: a given column's entry is
replaced (or appended, for a new sort column), but every other column's
entry survives untouched. This matters because Sheets' own `fields` mask
would otherwise replace `sortSpecs`/`criteria` wholesale — `--clear-sort`/
`--clear-criteria` reset to empty first, if that whole-replacement behavior
is actually what you want.

**Two things this issue does not cover.** `duplicateFilterView` has no CLI
verb — the issue's own proposed scope omits it, though the API supports it.
And `FilterCriteria` support is `hiddenValues` only: filtering by a boolean
condition (the same vocabulary `set-data-validation` curates) isn't
exposed. Both are documented cuts, not silent gaps.

#### drive sheets add-named-range / update-named-range / delete-named-range / list-named-ranges

Named ranges, gated by `sheets-structure` — including `delete-named-range`.
See [ADR-0081](adrs/adr-0081.md) §2 for why: a named range is a label over a
region, not grid data, so removing one leaves every cell's stored value and
formula text untouched, even though every formula referencing the removed
name starts evaluating to `#NAME?`.

```bash
# Add a named range, or one covering an entire sheet with --whole-sheet.
omni-dev drive sheets add-named-range <ID> --name Prices --sheet Q2 --range B2:B50
omni-dev drive sheets add-named-range <ID> --name AllOfQ2 --sheet Q2 --whole-sheet

# See what's defined — a plain, ungated read.
omni-dev drive sheets list-named-ranges <ID>

# Rename and/or re-point an existing named range, resolved by exact name.
omni-dev drive sheets update-named-range <ID> --name Prices --new-name UnitPrices
omni-dev drive sheets update-named-range <ID> --name Prices --sheet Q3 --range B2:B50

# Remove a named range — read --dry-run first.
omni-dev drive sheets delete-named-range <ID> --name Prices --dry-run
omni-dev drive sheets delete-named-range <ID> --name Prices
```

`update-named-range`/`delete-named-range` resolve their target by the
*exact* name — `list-named-ranges` is how you find it. Unlike
`update-protection`/`unprotect-range`'s range-based lookup, this can never
be ambiguous: Sheets enforces unique names workbook-wide, so a name either
matches one named range or none. `update-named-range` may rename only,
re-point only, or both — passing neither `--new-name` nor a new range is
refused as nothing to change.

`delete-named-range --dry-run` (and the real run, before mutating) scans
every sheet's formulas for the name being removed and reports the count and
A1 locations of every reference — never the formula text or a cell's value
— so read it before running for real. The break is also recoverable:
re-adding a named range with the same name over the same range restores
every dependent formula to working order, since the name is what changed,
not the formula text.

## Docs

`drive docs` reads the *structural model* of a Google Doc through the Docs v1
API (issue #1615). Editing is a separate, later phase; today this tree is
read-only.

No new login flag is needed. Reading works with the `drive.readonly` scope
every account already has — the Docs API accepts the Drive scopes, exactly as
the Sheets API does.

### Why this exists alongside `drive read --content`

`drive read --content` already exports a Doc to markdown, and for reading the
*prose* it is the better command. What an export structurally cannot give you
is the **address space**. Every Docs edit is addressed by a numeric index into
the document, and a markdown rendering has no path back to one. So:

- **`drive read --content` is the prose channel.**
- **`drive docs read` is the model channel** — each element's `[start, end)`
  index range, its kind, its style, and the document's `revisionId`.

That is also why indices are shown by default rather than behind a flag:
without them this command would just be a worse `drive read --content`.

### Indices are UTF-16 code units

This is the one thing worth internalising before using the output for
anything. Docs indices count **UTF-16 code units**, not characters and not
bytes, and `endIndex` is exclusive. The distinction is invisible in ASCII and
matters the moment a document contains an emoji or a CJK character: `😀` is one
character, two UTF-16 code units and four UTF-8 bytes.

`omni-dev` never computes an index itself — it only reports what the server
sent — so nothing here rounds the difference away silently.

### Tabs

Google Docs supports tabs, and `drive docs` always requests every tab's
content. That is deliberate: a request without it returns only the **first**
tab, in a response shaped identically to a single-tab document, so reading a
third of a document would be indistinguishable from reading all of a small
one. (That is precisely the trap `drive read --content` still has on a Sheet,
where it exports the first sheet only.)

Narrow with `--tab <TAB_ID>` after the fact. An unknown tab id is an error
listing the real ones, never an empty result.

#### `drive docs info`

Shows the document's identity, its revision, its per-tab counts and its
heading outline.

```bash
$ omni-dev drive docs info 1AbC_dEfGhIjKlMnOpQrStUvWxYz
Id: 1AbC_dEfGhIjKlMnOpQrStUvWxYz
Title: Design Doc
Revision: ALm37BXk3nQ
Tabs: 2
Named ranges: 1
  intro (1 range(s))

Tab: t.0 "Overview" — 12045 chars, 143 paragraphs, 2 tables, 1 section breaks
  HEADING_1  [1..18)  Overview
  HEADING_2  [220..241)  Goals

Tab: t.1 "Appendix" — 890 chars, 12 paragraphs
  HEADING_1  [1..12)  Appendix
```

Two fields are worth more than they look:

- **`Revision`** is the token an edit has to present so a write against a
  document that changed underneath it is refused rather than misapplied.
  Nothing else in the CLI surfaces it. When you see
  `Revision: (none — read-only access)`, Google withheld it because the account
  has no edit access — and a later edit will refuse for that reason.
- **Named ranges** are the *stable* way to name a region. An index shifts on
  every insertion; a named range's name does not.

**Body only.** The per-tab counts and heading outline above cover the tab's
**body** alone. Headers, footers and footnotes — which `drive docs read`
fetches and renders (see below) — do not contribute a paragraph, table, or
heading to this command's output. A heading that lives inside a header,
footer or footnote is invisible here even though `drive docs read` on the
same document now shows it. Folding segments into the outline is unstarted
follow-up work.

#### `drive docs read`

One line per structural element, indented by nesting depth.

```bash
$ omni-dev drive docs read 1AbC_dEfGhIjKlMnOpQrStUvWxYz
START  END  KIND           STYLE        TEXT
    0    1  section-break
    1   18  paragraph      HEADING_1    Overview
   18  220  paragraph      NORMAL_TEXT  This document describes the approach…
  220  241  paragraph      HEADING_2    Goals
  241  310  table                       3x2
  243  251    paragraph    NORMAL_TEXT  Name
  252  266    paragraph    NORMAL_TEXT  Description
```

With more than one tab, each block is preceded by a `# <tabId> <title>` line.

`--suggestions-view default|inline|accepted|without` selects which view of
pending suggestions the text *and the indices* are reported against. It is a
correctness knob rather than a display preference: a document with pending
suggestions has a different index space per view.

**Output formats.** `-o table` (the default) **sanitises** element text,
stripping control characters. This differs from `drive sheets read`, whose CSV
emits cell values verbatim, and the difference is deliberate: CSV is an
interchange format that must round-trip, so stripping there would corrupt real
data, while this table is an orientation view whose entire value is column
alignment — a soft line break or an escape sequence in the text would destroy
it. Use **`-o json`** when you want content: it is the unsanitised channel and
carries every field. **`-o jsonl`** emits **one line per element**, with the
document id, revision and tab id repeated on each, so a single line is
self-describing to `jq`.

**Headers, footers and footnotes.** These live in their own segments,
addressed by `segmentId` rather than an index range, and are always included
whenever a tab has any — there is no flag to opt in or out, the same "always
fetch, never mask" precedent tabs use above. In `-o table` each one renders
as its own block, after the tab's body:

```
## header kix.abc123
   0   12  paragraph      NORMAL_TEXT  Confidential draft
## footnote kix.def456
   0    9  paragraph      NORMAL_TEXT  See intro.
```

In `-o json`/`-o yaml` they appear as `headers`/`footers`/`footnotes` arrays
on each tab, each entry carrying its `segment_id` and `elements`; a tab with
none of a given kind omits that array entirely rather than sending `[]`. In
`-o jsonl` each segment's elements ride the same flat record stream as the
body, with an added `segment: {kind, segment_id}` field (absent for a body
element).

### Editing a document

`drive docs replace` and `drive docs append` mutate text, gated by the
`docs-write` permission (see [Write permissions](#write-permissions)) and
requiring `--write-file` or `--write-full`.

```bash
# Preview first — reports the occurrence count without sending anything
$ omni-dev drive docs replace 1AbC… --search Q3 --replace Q4 --dry-run
Would replace: 7 occurrence(s) in 'Roadmap' (counted from the copy just read)

$ omni-dev drive docs replace 1AbC… --search Q3 --replace Q4
Replaced: 7 occurrence(s) in 'Roadmap'

$ omni-dev drive docs append 1AbC… --text $'\nAppended by omni-dev.'
Appended: 22 char(s) / 22 byte(s) to 'Roadmap'
```

`append` also takes `--text-file <PATH>`, or `--text-file -` for stdin.

#### Every edit is leased against a revision

This is the part worth understanding. `documents.batchUpdate` is addressed
by *index*, and the indices an edit is computed from come from a read that
has already returned. If someone edits the document in between, those
indices still resolve — just against different text. Nothing errors; the
edit simply lands in the wrong place.

So every edit presents the `revisionId` from the read that computed it, and
Google refuses the write if the document has moved:

```
Refused: 'Roadmap' changed since it was read (revision lease ALm37BXk3nQ no
longer current) — nothing was written. Re-run to apply against the current
version.
```

Nothing was written — the batch is atomic. **Re-running is the fix**, and it
is the only one: there is deliberately no flag to force the write through,
because the alternative the API offers rebases your edit over the other
person's changes and reports success on a document nobody has looked at.
See [ADR-0076](adrs/adr-0076.md) §3.

If the account has only read access Google withholds the revision id
entirely, and the edit is refused up front rather than attempted unleased.

#### Things to know

- **`--search` is a literal substring, never a regex**, and matching is
  **case-sensitive by default** — which inverts the API's own default. Under
  Google's default, `--search it` also rewrites `It` and `IT`, in a verb with
  no undo. Use `--ignore-case` when you want that.
- **`--dry-run`'s occurrence count is an estimate.** It is counted over the
  body text this command read, while the server matches over its own view —
  a match can span a styling boundary, or sit in a header, footer or
  footnote, which `drive docs read` now fetches (see above) but this count
  does not yet include. The count never decides anything: a count of zero
  still sends the request, because reporting "nothing to do" from an
  estimate would be wrong exactly when the estimate is. The real run
  reports the server's own number.
- **`replace` spans every tab; `append` lands in the first.** That asymmetry
  is the Docs API's, confirmed against it directly, and it is why the preview
  counts across all tabs.
- **`append` adds no separator.** Appending `hello` to a document ending
  `world` gives `worldhello`. Include a leading newline if you want one.
- **Deletion is not supported**, and not merely unimplemented: no delete
  request is constructible anywhere in this codebase, enforced by a test.
  Replacing text *with nothing* (`--replace ""`) is the supported way to
  remove it.

#### `drive docs create`

Creates a Google Doc, optionally seeded with text. Gated by the `create`
operation, not `docs-write`.

```bash
$ omni-dev drive docs create --name "Q4 Plan" --parent 1FoLdEr… --text "Draft."
Created: 'Q4 Plan' (1NeW…) in 1FoLdEr…, seeded with 6 char(s)
```

`--text-file <PATH>` (or `-` for stdin) is the alternative to `--text`.

The seed is **not** separately gated under `docs-write` in the normal case:
routing it through the write engine would re-check `docs-write` against the
new file's parents, which defaults to deny, so `--text` would create an empty
document and then report itself blocked on every folder that grants only
`create`. The `create` verdict authorises the pair — which is defensible only
because the id being written is one this same invocation just created inside
an already-cleared folder. An **explicit** `deny: ["docs-write"]` on that
folder is a deliberate signal and *does* block the seed, before anything is
created.

If creation succeeds but seeding fails, the result says so and names the new
document's id:

```
Partially failed: created 'Q4 Plan' (1NeW…) in 1FoLdEr…, but seeding its text
failed: … The document exists and is empty — it cannot be rolled back
automatically.
```

There is no `files.delete` anywhere in this integration, so an empty document
cannot be cleaned up automatically and must never be reported as a plain
failure that leaves something you can't find. Delete it yourself if you don't
want it.

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

### `insufficientPermissions` on create/upload/edit

```
Error: Drive API request failed: HTTP 403: Insufficient Permission (reason: insufficientPermissions)
  Run `omni-dev drive auth login --write-file` (or `--write-full`) to grant the scope needed to create files/folders and upload content
```

Same shape as the rename/move hint above, but for `create`/`upload` (needs
`--write-file` or `--write-full`) or `edit` (needs `--write-file` if
`omni-dev` created the file, `--write-full` for any pre-existing one — see
[Edit](#edit)). Re-run `drive auth login` with the named flag(s), then
retry.

### `Blocked` — refused by the write-permission gate

```bash
$ omni-dev drive create --name "x" --parent 1Sen...Confidential
Blocked: x in 1Sen...Confidential
  refused by default policy (no matching rule)
```

This is not an error — the command exits 0, same as a `Blocked` move (see
[Move](#move)). No `files.create`/`files.update` call was ever made. Run
`drive permissions check <id> --operation <op>` to see exactly which rule
(if any) decided the refusal, and [Write
permissions](#write-permissions) to add a rule that allows it.

A refusal naming a rule says which kind decided it — `refused by rule on
folder <id> (depth 2)` or `refused by rule on file <id>`. A file rule has
no depth because it matches the target itself, and it beats every folder
rule (see [Resolution](#write-permissions)).

### `Refused: … has no parent folder visible to this account`

```bash
$ omni-dev drive sheets write 1Sh4r3d...Plan --range 'A1' --values data.csv
Refused: 'Quarterly Plan' has no parent folder visible to this account, so no
folder rule can apply to it. This is normal for a Sheet shared by link or
email. Grant it by id instead: add {"file_id": "<spreadsheet id>", "allow":
["sheets-write"]} to write_permissions.rules.
```

`files.get` returns only the parents **this account** can see, and a file
shared with you by link or email is not in a folder you can see — so it
arrives with none, and the gate has no ancestor chain to evaluate.

This is deliberately *not* reported as an ordinary `Blocked`: there is no
`folder_id` rule you could write that would change it, so telling you to
fix your folder rules would send you hunting for a bug that isn't there.
The fix is a `file_id` rule — see [Granting a file shared with
you](#granting-a-file-shared-with-you). `drive edit` reports the same way
for a shared binary file.

If you would rather not grant by id, the alternative still works: add the
file to a folder in your own Drive and grant that folder.

### `Refused: … changed since it was read (revision lease … no longer current)`

```bash
$ omni-dev drive docs replace 1AbC… --search Q3 --replace Q4
Refused: 'Roadmap' changed since it was read (revision lease ALm37BXk3nQ no
longer current) — nothing was written. Re-run to apply against the current
version.
```

Someone edited the document between the read that computed this edit and the
write that would have applied it. **Nothing was written** — the request is
atomic, so the document is exactly as the other person left it.

**Re-running is the fix**, and it is the only one. There is deliberately no
flag to force the write through: the Docs API's alternative rebases your edit
on top of the other person's changes and reports success, which would mean
`omni-dev` editing a document nobody had looked at. See
[ADR-0076](adrs/adr-0076.md) §3 and [Every edit is leased against a
revision](#every-edit-is-leased-against-a-revision).

If it happens repeatedly, the document is being actively edited; `--dry-run`
first to see what your change would touch.

### `Refused: … returned no revision id`

```bash
$ omni-dev drive docs replace 1AbC… --search Q3 --replace Q4
Refused: 'Roadmap' returned no revision id, which Google sends only to
callers with edit access — so this write cannot be leased against a known
version. Request edit access, or check the account in use.
```

The account can *read* the document but not edit it. Google signals that by
omitting the revision id, and rather than attempt a write that would fail
anyway — or worse, write without a lease — the edit is refused up front.

Two things to check: whether the account actually has edit access to the
document, and whether `--account` is selecting the account you meant (see
[Multiple accounts](#multiple-accounts)). Note this is distinct from a
`Blocked`, which is *omni-dev's* own gate refusing, and from an
`insufficientPermissions` error, which is the OAuth scope being too narrow.

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
- [ADR-0071](adrs/adr-0071.md) — extends ADR-0069/ADR-0070 to add
  `create`/`upload`/`edit`: the `--write-file`/`--write-full` scope tiers,
  the [write-permission gate](#write-permissions) and its resolution
  algorithm, and why both layers are independently required.
- [ADR-0073](adrs/adr-0073.md) — extends ADR-0069/0070/0071 to add the
  Sheets v4 API: the shared transport core behind a second Google host, the
  separate `sheets-write` gate operation and why reusing `edit` was
  rejected, and the CSV/JSON rendering rules.
- [ADR-0075](adrs/adr-0075.md) — extends ADR-0073 with structural edits via
  `spreadsheets.batchUpdate`: the separate `sheets-structure` gate
  operation, why the surface is typed verbs with no raw request
  passthrough, and why deletion is deferred rather than gated.
- [ADR-0077](adrs/adr-0077-sheets-deletion-via-batchupdate.md) — the
  deferred deletion design pass: the separate `sheets-delete` gate
  operation, why the typed-verbs-only property survives deletion too, and
  why there is still no interactive confirmation or `--force` for the most
  dangerous operation in the tree.
- [ADR-0078](adrs/adr-0078.md) — the remaining `spreadsheets.batchUpdate`
  surface: formatting, data validation and protected ranges. Why formatting
  and data validation join the existing `sheets-structure` operation while
  protected ranges get their own `sheets-protection` operation instead
  (a permission change inside the document, not a structural one), and
  `merge-cells`' `--dry-run` honesty requirement for the one request here
  that discards data.
- [ADR-0081](adrs/adr-0081.md) — the gate mapping for the second Sheets
  capability tranche (issue #1663), settled once rather than per issue:
  named-range add/update/delete join `sheets-structure`, including why
  `delete-named-range` stays there rather than joining `sheets-delete`, and
  the mandatory referencing-formula preview that mitigates it.
- [ADR-0063](adrs/adr-0063.md) — the OAuth2 authorization-code + PKCE
  design, refresh-token-only persistence, and bring-your-own Google Cloud
  project rationale ADR-0069 applies unchanged.
- [ADR-0066](adrs/adr-0066.md) — the named-account store behind
  [Multiple accounts](#multiple-accounts), and why it's orthogonal to
  `--profile`.
- MCP tools — planned, not yet available; tracked by
  [issue #1525](https://github.com/rust-works/omni-dev/issues/1525).
- [Drive API documentation](https://developers.google.com/workspace/drive/api/reference/rest/v3) — upstream reference.
