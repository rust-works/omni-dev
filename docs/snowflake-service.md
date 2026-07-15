# Snowflake service

The omni-dev daemon hosts a **Snowflake query service**: it authenticates a
Snowflake session **once per `(account, user)`** via external-browser SSO and
reuses that live session for **arbitrary SQL** against **any** account. It is the
daemon's second service, modelled on the browser bridge.

## Why a resident service

Calling `snowsql --authenticator externalbrowser` per query fires an SSO popup on
every invocation, and Snowflake's token caching (`ALLOW_ID_TOKEN`) is often
disabled by account admins, so the session token cannot be reused across
processes. A live `SnowflakeSession` lives only in memory with no serialization
API — so the only way to "authenticate once, query many times" is a resident
process. The daemon already is one, and (via the in-tree client below) it renews
the session token in place so the auth survives the ~1h token lifetime.

## Architecture

Mirrors the bridge's engine/adapter split:

- `src/snowflake/` — the engine:
  - `mod.rs` — `SnowflakeEngine` (lazy auth, per-query `USE …`, transparent
    token renew-and-retry, overall sign-in deadline) and `SnowflakeEngineConfig`
    (env + settings resolution).
  - `session.rs` — the bounded `SessionPool` + `PoolRegistry` (below).
  - `client/` — a **clean-room Snowflake v1 REST client** (no third-party
    connector): external-browser SSO login, query, and — crucially — session
    token **renewal** (`session/token-request`) via the master token, plus
    timeouts/retries and arbitrary-schema → JSON decoding.
- `src/daemon/services/snowflake.rs` — `SnowflakeService`, the thin
  `DaemonService` adapter (routes ops, renders the tray submenu / status).
- `src/cli/snowflake.rs` — the `omni-dev snowflake` client.

### Concurrency model — bounded session pool

On the v1 query endpoint, statement context (`warehouse`/`role`/`database`/
`schema`) is **session-global** (changed via `USE`, shared by every query on the
session token). To run **concurrent** queries on a single authentication identity
*and* still honor per-query context, each `(account, user)` keeps a **bounded
pool** of authenticated sessions (`SNOWFLAKE_POOL_SIZE`, default 4):

- A query **checks out** a session (reusing the most-recently-returned idle one,
  or authenticating a new one if none is idle and the pool is under capacity),
  applies only the `USE`s that differ from the session's current context, runs,
  and returns it. Concurrency is capped at the pool size by a
  `tokio::sync::Semaphore`, so the number of live sessions — and thus browser
  auths — never exceeds it and grows lazily with demand.
- Each session captures its **base context** at creation
  (`SELECT CURRENT_WAREHOUSE()/CURRENT_ROLE()/CURRENT_DATABASE()/CURRENT_SCHEMA()`)
  so a per-query override can always be reset back to the account/user default on
  reuse — context switches are deterministic and isolated (the session is held
  exclusively for its `USE … + query`).
- Session **creation (browser SSO) is serialized across all pools** by a shared
  `tokio` auth gate, so only one auth window opens at a time (concurrent
  first-queries don't each pop a browser).
- `menu()` / `status()` are synchronous and must not block: they read each pool's
  bookkeeping behind a `std::sync::Mutex` that is **never held across an `.await`**.

A separate session is required per concurrent in-flight context because v1 `USE`
mutates session-global state; the SQL API **v2** would allow per-request context
without this (a v2 client was proposed in #1003 but closed as not planned).
Non-interactive auth — PAT and key-pair JWT — is supported on the v1 client for
headless/daemon use; see [Authentication](#authentication).

`daemon status`, `omni-dev snowflake sessions`, and the tray submenu list each
pool **and each individual authenticated session** under it — its id, current
context, query count, and what it's doing: the running query (a truncated
preview) and how long it's been running, or its idle time. So every browser auth
is visible along with live activity (the tray's elapsed timer ticks in place
without closing the menu).

## CLI

Lifecycle stays on `omni-dev daemon` (`start` / `stop` / `status` / `restart`).
The `snowflake` subcommands are thin clients over the daemon's control socket.

```bash
# Run SQL (a browser opens once for first-time sign-in for this account+user).
omni-dev snowflake query --account MYACCT --user me "SELECT CURRENT_TIMESTAMP(), 1, 'x'"

# SQL can also come from stdin (the lumon pipe path).
echo "SELECT 1" | omni-dev snowflake query --account MYACCT --user me

# Multi-statement scripts run in one submission; the payload's `statements`
# array carries one result set per statement (JSON/YAML only).
omni-dev snowflake query --account MYACCT --user me "USE WAREHOUSE WH; SELECT 1"

# Per-query context is applied with USE … on the reused session.
omni-dev snowflake query --account MYACCT --user me \
  --warehouse WH --role ANALYST --database DB --schema PUBLIC "SELECT 1"

# YAML instead of JSON.
omni-dev snowflake query --account MYACCT --user me -o yaml "SELECT 1"

# CSV / TSV (a header row plus one row per result row).
omni-dev snowflake query --account MYACCT --user me -o csv "SELECT 1, 'x'"

# Write to a file instead of stdout (works for any -o format).
omni-dev snowflake query --account MYACCT --user me -o csv --out-file rows.csv "SELECT 1"

# List / evict multiplexed sessions.
omni-dev snowflake sessions          # table; -o json for machines
omni-dev snowflake disconnect --account MYACCT --user me   # one pool by identity
omni-dev snowflake disconnect --id 3                       # one pool by id (from `sessions`)
omni-dev snowflake disconnect --all                        # every pool

# Cancel a runaway query without evicting the session (frees it promptly).
omni-dev snowflake cancel --id 3                # every busy member in pool 3
omni-dev snowflake cancel --id 3 --member 2     # one authenticated session's query
omni-dev snowflake cancel --account MYACCT --user me   # by identity
omni-dev snowflake cancel --all                 # every running query
```

`disconnect` takes exactly one selector — the `--account`/`--user` pair, `--id`
(the numeric pool id shown by `sessions`), or `--all`. The by-id and bulk forms
were previously reachable only from the macOS tray, so non-macOS operators had to
restart the daemon to bulk-evict sessions (#1228).

`cancel` aborts the running query (via `queries/v1/abort-request`) **without**
evicting the session, so a runaway statement frees its pooled session promptly
(within one poll interval) instead of holding it until `SNOWFLAKE_QUERY_TIMEOUT`
(#1225). It takes the same three selectors as `disconnect`, plus an optional
`--member <id>` (a member id shown by `sessions`) to target one authenticated
session's query rather than every busy member in the pool; `--member` is not
valid with `--all`. It reports how many running queries an abort was issued for
and is a no-op when nothing is running. A multi-statement script is submitted
under one request id, so `cancel` aborts the whole script.

`query` returns a self-describing payload. Result sets are always wrapped in a
`statements` array — **one entry per `;`-separated statement** — so a single-statement
query is a one-element array:

```json
{
  "statements": [
    {
      "columns": [{ "name": "ID", "type": "fixed(38,0)" }, { "name": "NAME", "type": "text(16777216)" }],
      "rows": [{ "ID": 1, "NAME": "hello" }]
    }
  ]
}
```

Each `statements[]` entry is one result set. Cell types map as: `fixed` (scale 0) →
integer (falling back to the exact string when it overflows `i64`); `fixed` (scale > 0)
/ `real`/`float`/`double` → number; `boolean` → bool; `text` → string;
`date`/`time`/`timestamp_*` → ISO-8601 string; `variant`/`object`/`array` → parsed
JSON; `binary` (hex) and other types → the raw string. An empty result reports
`columns: []` (the connector exposes column metadata per-row only).

### Multi-statement scripts

A semicolon-separated script (e.g. `USE WAREHOUSE WH; SELECT …`) runs as one
submission. When the SQL parses to more than one statement the service sets
`MULTI_STATEMENT_COUNT = 0` ("any count") on the v1 REST submission; the server then
returns a list of child result ids, which the service fetches and returns in order as
successive `statements[]` entries. Single-statement queries keep the original,
cheaper path (no extra round trips) and still return a one-element `statements` array.
Statement detection ignores `;` inside string/quoted-identifier literals, line/block
comments, and `$$`-delimited blocks.

`-o json` (default) and `-o yaml` serialize the whole `statements` payload. `-o csv` /
`-o tsv` render a **single** tabular result set — a header row of column names (order
taken from `columns[]`) followed by one row per result row, with RFC 4180 quoting (a
field is double-quoted, and embedded quotes doubled, when it contains the delimiter, a
quote, or a newline); `null` cells become empty fields and `variant`/`object`/`array`
cells become compact JSON. A zero-row query (`columns: []`) renders empty. Because a
flat CSV/TSV has no multi-table shape, `-o csv`/`-o tsv` **error** on a multi-statement
result — use `-o json`/`-o yaml` for those. `--out-file <PATH>` writes the rendered
output to a file instead of stdout (for any `-o` format).

## Configuration

Account, user, and default context resolve **in the client invocation** — env
var first, then `~/.omni-dev/settings.json`, honouring `--profile` /
`OMNI_DEV_PROFILE` (the Atlassian credential pattern) — and the resolved values
are sent with each query. Explicit flags override the resolution. The daemon
also resolves the same variables once at startup, but only as a **fallback**
for requests that omit a field (e.g. clients speaking the socket protocol
directly without resolving defaults themselves):

| Setting   | Env var               | `query` flag    |
|-----------|-----------------------|-----------------|
| Account   | `SNOWFLAKE_ACCOUNT`   | `--account`     |
| User      | `SNOWFLAKE_USER`      | `--user`        |
| Host override | `SNOWFLAKE_HOST`  | — (config only) |
| Warehouse | `SNOWFLAKE_WAREHOUSE` | `--warehouse`   |
| Role      | `SNOWFLAKE_ROLE`      | `--role`        |
| Database  | `SNOWFLAKE_DATABASE`  | `--database`    |
| Schema    | `SNOWFLAKE_SCHEMA`    | `--schema`      |
| Pool size | `SNOWFLAKE_POOL_SIZE` | — (config only) |
| HTTP timeout (s) | `SNOWFLAKE_HTTP_TIMEOUT` | — (config only) |
| Sign-in deadline (s) | `SNOWFLAKE_AUTH_TIMEOUT` | — (config only) |
| Query deadline (s) | `SNOWFLAKE_QUERY_TIMEOUT` | — (config only) |
| Keep-alive interval (s) | `SNOWFLAKE_HEARTBEAT_INTERVAL` | — (config only) |
| Browser command | `SNOWFLAKE_BROWSER_COMMAND` | — (config only) |

`SNOWFLAKE_POOL_SIZE` (default 4) caps the concurrent sessions — and therefore the
browser auths — per `(account, user)`. A burst of more than that many concurrent
queries queues for a free session rather than authenticating more.

`SNOWFLAKE_HTTP_TIMEOUT` (default 120s) bounds a single REST call (generous, so the
query long-poll isn't cut short); `SNOWFLAKE_AUTH_TIMEOUT` (default 150s) bounds one
sign-in end-to-end; `SNOWFLAKE_QUERY_TIMEOUT` (default 3600s) bounds a whole query
**including async-result polling**, so long-running queries are limited by it
rather than by the per-request timeout.

`SNOWFLAKE_HEARTBEAT_INTERVAL` (default 900s; `0` disables) sets how often the
background keep-alive task heartbeats idle sessions (see
[Reliability](#reliability)).

`SNOWFLAKE_HOST`, when set, is used **verbatim** as the API host instead of
deriving `<account>.snowflakecomputing.com` from the account identifier. Set it
to reach an AWS/Azure **PrivateLink** endpoint
(`<account>.privatelink.snowflakecomputing.com`) or a gov/custom host that the
default derivation can't produce. It applies to every session this daemon
creates; leave it unset for the standard public host.

The host override, the pool size, the timeouts, the heartbeat interval, and the
browser command are operational (config-only) settings read from the **daemon's**
environment once at startup — restart the daemon (`omni-dev daemon restart`) to
change them. They are not affected by the client's `--profile`.

`settings.json` fallback example:

```json
{ "env": { "SNOWFLAKE_ACCOUNT": "MYACCT", "SNOWFLAKE_USER": "me" } }
```

To hold several accounts on one machine, put each account's `SNOWFLAKE_*`
variables in a named **profile** and select it with `--profile` /
`OMNI_DEV_PROFILE` (see
[Credential Profiles](configuration-best-practices.md#credential-profiles)).
The CLI resolves the profile's values and sends them with the query, so the
resident daemon serves whichever profile each invocation selects — its own
startup profile is irrelevant. Because the service pools sessions by
`(account, user)`, distinct profiles produce distinct pools automatically.

The service is **account-agnostic**: there is no hardcoded account list. The
daemon's startup defaults apply as the session's base context at creation;
client-resolved values and per-query flags issue `USE WAREHOUSE/ROLE/
DATABASE/SCHEMA` on the reused session, so one session serves varied query
contexts without re-auth.

## Security

- **No new socket or port.** Requests ride the daemon's existing `0600` Unix
  control socket in the `0700` runtime directory; those filesystem permissions
  are the trust boundary. Arbitrary SQL runs as the authenticated user — intended.
- **No secret persisted.** Unlike the browser bridge (which writes a `0600` token
  file), the Snowflake service keeps the live `SnowflakeSession` in memory only;
  shutdown simply drops it. The non-interactive credentials (PAT, private key) are
  held in memory in a redacting wrapper that keeps them out of logs and are never
  written to disk by the service — but they enter the process from the daemon's
  environment or `settings.json`, so protect those as you would any secret (a PAT
  or private key in `settings.json` sits in plaintext at rest; prefer
  `SNOWFLAKE_PRIVATE_KEY_PATH` to a `0600` key file over an inline key). Enabling
  request-body logging (`OMNI_DEV_LOG_BODIES`, opt-in and off by default) would
  capture the login body, which carries the credential — leave it off in
  production. Per-query context flags are validated as bare identifiers before
  interpolation into `USE …`.

## Authentication

The engine authenticates every session in every pool with one method, selected by
`SNOWFLAKE_AUTHENTICATOR` (resolved from the daemon's environment, then
`settings.json`, once at startup). The default is interactive external-browser
SSO; two **non-interactive** methods make the daemon usable headless — CI,
servers, or containers with no display.

| `SNOWFLAKE_AUTHENTICATOR` | Method | Credential var(s) |
|---|---|---|
| `externalbrowser` (default) | External-browser SSO | — |
| `programmatic_access_token` (alias `pat`) | Programmatic access token | `SNOWFLAKE_TOKEN` |
| `snowflake_jwt` (aliases `keypair`, `key_pair`, `jwt`) | Key-pair RS256 JWT | `SNOWFLAKE_PRIVATE_KEY_PATH` or `SNOWFLAKE_PRIVATE_KEY` |

An unknown selector — or a non-interactive method missing its credential — fails
fast at daemon startup with an actionable error. All three reuse the same session
pool, token renewal, and keep-alive heartbeat; they differ only in how the first
session for each `(account, user)` is established.

### External-browser SSO (default)

The client's external-browser flow auto-opens a browser and binds an ephemeral
localhost callback — no TTY needed, suitable for the resident daemon. The first
query for a new `(account, user)` triggers the popup; reuse is silent. On a
session-expiry error the session is evicted and the next query re-authenticates
(so the popup is always tied to a user action). If browser launch is unreliable
under a background launchd daemon, run the first auth from a foreground context
(`omni-dev daemon start --foreground`).

By default the SSO URL opens with the OS default handler (`open` / `xdg-open` /
`explorer`). Set **`SNOWFLAKE_BROWSER_COMMAND`** to launch a specific browser or
Chrome profile instead — the value is a single command line with a `{url}`
placeholder (or the URL is appended as a trailing argument when the placeholder
is absent):

```json
{
  "env": {
    "SNOWFLAKE_BROWSER_COMMAND": "\"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome\" --profile-directory=\"Profile 1\" --new-window {url}"
  }
}
```

The command is split with POSIX-style quoting (single quotes, double quotes, and
backslash escapes), so a program path or an argument value may contain spaces
(`Google Chrome.app`, `--profile-directory="Profile 1"`). A value that is present
but malformed (an unterminated quote, or one that tokenizes to no words) is a hard
error at daemon startup rather than a silent fall back to the default handler.
Leave it unset for the default handler. It applies only to external-browser SSO;
the non-interactive methods below open no browser and ignore it.

> **macOS note:** to open a *specific* Chrome profile you must exec the Chrome
> binary directly (as above). `open -a "Google Chrome" --args
> --profile-directory=…` is ignored when Chrome is already running — `open`
> just focuses the existing process without applying the profile flag.

### Programmatic access token (PAT)

Set `SNOWFLAKE_AUTHENTICATOR=programmatic_access_token` and `SNOWFLAKE_TOKEN` to a
PAT minted in Snowsight (or via SQL). The token is presented in place of a
password — no browser, no callback. **Prerequisite:** Snowflake requires the user
to be covered by a **network policy** to use a PAT; without one the login is
rejected. Mint and scope the token per Snowflake's
[programmatic access tokens](https://docs.snowflake.com/en/user-guide/programmatic-access-tokens)
guide.

### Key-pair JWT

Set `SNOWFLAKE_AUTHENTICATOR=snowflake_jwt` and point `SNOWFLAKE_PRIVATE_KEY_PATH`
at an **unencrypted PKCS#8** PEM private key (`-----BEGIN PRIVATE KEY-----`), or
put the PEM inline in `SNOWFLAKE_PRIVATE_KEY`. The client signs a short-lived
RS256 JWT locally each time it authenticates — no browser, no callback, and no
secret leaves the machine except the signed assertion. **Prerequisite:** register
the matching public key on the user once:

```sql
ALTER USER my_user SET RSA_PUBLIC_KEY = 'MIIB...';
```

Generate a key pair with, e.g.:

```bash
openssl genpkey -algorithm RSA -out sf_key.p8 -pkeyopt rsa_keygen_bits:2048
openssl rsa -in sf_key.p8 -pubout -out sf_key.pub   # its body is the RSA_PUBLIC_KEY value
```

**Encrypted keys are not yet supported.** If your `.p8` is passphrase-encrypted
(`-----BEGIN ENCRYPTED PRIVATE KEY-----`), decrypt it first
(`openssl pkcs8 -in enc.p8 -out sf_key.p8`); setting
`SNOWFLAKE_PRIVATE_KEY_PASSPHRASE` is rejected with that same guidance.

## Reliability

- **Token renewal (the point of the in-tree client).** The session token expires
  (~1h); the daemon renews it in place via `session/token-request` (authorized by
  the master token) rather than re-authenticating. A session near expiry is
  renewed before its next query, and a query that hits an expired token renews
  and retries once transparently — so routine expiry is invisible. Only a failed
  renew (master token also expired) falls back to evict + lazy re-auth.
- **Keep-alive heartbeat (idle pools never re-prompt SSO).** Renewal is
  authorized by the master token, which itself expires (~4h) unless the client
  heartbeats — `CLIENT_SESSION_KEEP_ALIVE` only extends it server-side when
  periodic `session/heartbeat` calls arrive. A background task heartbeats every
  **idle** session each `SNOWFLAKE_HEARTBEAT_INTERVAL` (default 900s; `0`
  disables), renewing a session token that would lapse before the next tick, so
  a pool idle overnight still answers its next query without a browser popup.
  Busy sessions are skipped (the query path keeps them alive); a session whose
  master token has expired anyway is evicted so the next query lazily
  re-authenticates. The task stops on daemon shutdown.
- **Long-running queries.** The v1 endpoint long-polls and, for anything slower
  than its synchronous window, returns an "in progress" code with a result URL;
  the client **polls** that URL until the query completes. Heavy queries (e.g. a
  multi-CTE `QUALIFY` over billions of rows) therefore run to completion, bounded
  by `SNOWFLAKE_QUERY_TIMEOUT` (default 3600s) rather than the per-request timeout.
- **Cancelling a runaway query.** `cancel` (`omni-dev snowflake cancel`, or the
  socket `cancel` op) aborts a running statement via `queries/v1/abort-request`,
  authorized by the running session's own token and identified by the query's
  original `requestId`. It does **not** evict the session: the server cancels the
  query, the client's poll loop then returns a cancelled error within one poll
  interval, and the pooled session is checked back in for reuse — freeing it
  promptly instead of holding it until `SNOWFLAKE_QUERY_TIMEOUT` (#1225).
- **Transient retries.** REST calls retry connection errors and `429/502/503/504`
  with exponential backoff, reusing the same `requestId` so a retried query is
  de-duplicated server-side (never re-executed). A per-request *timeout* is **not**
  retried, so a slow query is never re-run.
- **Timeouts / no hung auths.** A single REST call is bounded by
  `SNOWFLAKE_HTTP_TIMEOUT` (120s — generous, so the query long-poll isn't cut
  short) over a 10s connect timeout; a whole sign-in is bounded by
  `SNOWFLAKE_AUTH_TIMEOUT` (150s) so a stalled SSO releases the shared auth gate
  instead of blocking every other new authentication daemon-wide.
- **One browser at a time.** Authentications are serialized by a shared auth gate;
  a request that's waiting grabs a session freed by another the instant it appears
  rather than opening a redundant auth.
- **Operator reset.** A wedged sign-in self-heals after the deadline; to force a
  full reset, `omni-dev daemon restart`.

## Programmatic contract (for downstream callers)

The service is reachable directly over the daemon's Unix control socket
(newline-delimited JSON), without the `omni-dev snowflake` CLI — e.g. for a lumon
`query-snowflake` rewrite, with a `snowsql` fallback still possible on that side.

- **Socket:** `<data_dir>/omni-dev/daemon.sock` (`dirs::data_dir()`; on macOS
  `~/Library/Application Support/omni-dev/daemon.sock`), mode `0600` in a `0700`
  directory.
- **Request envelope:** one JSON line —
  `{ "service": "snowflake", "op": "<op>", "payload": <object> }`.
- **Reply:** one JSON line — `{ "ok": true, "payload": <object> }` or
  `{ "ok": false, "error": "<message>" }`.

Ops:

| op           | payload                                                            | success payload                                        |
|--------------|--------------------------------------------------------------------|--------------------------------------------------------|
| `query`      | `{ account?, user?, warehouse?, role?, database?, schema?, sql }`  | `{ statements: [ { columns: [...], rows: [...] }, … ] }` — one entry per `;`-separated statement |
| `sessions`   | `null`                                                             | `{ sessions: [<pool>] }` — one entry per pool, see below |
| `cancel`     | (`{ account, user }` \| `{ id }` \| `{ all: true }`) `+ member?`   | `{ cancelled: <n> }`                                   |
| `disconnect` | `{ account, user }` \| `{ id }` \| `{ all: true }`                 | `{ disconnected: <bool>, count: <n> }`                |

`disconnect` accepts three mutually-exclusive selectors: the `(account, user)`
pair (the original contract), `id` (a numeric pool id from `sessions`), or
`all: true` (every pool). `count` is the number of pools evicted (`0` or `1` for
the pair/id forms); `disconnected` is `count > 0`, preserved for callers written
against the pre-#1228 reply.

`cancel` aborts running queries without evicting the session (#1225). It takes
the same three selectors as `disconnect`, plus an optional `member` (a numeric
member id from `sessions`) that narrows the pair/id forms to one authenticated
session's query instead of every busy member (`member` is ignored with
`all: true`). `cancelled` is the number of running statements an abort was issued
for — `0` when nothing was running. The abort makes each targeted session's poll
loop return a cancelled error within one poll interval, so the pooled session is
freed promptly rather than held until `SNOWFLAKE_QUERY_TIMEOUT`.

Each `<pool>` entry in the `sessions` reply describes one `(account, user)`
session pool, including its live members (the CLI and tray render all of these
fields):

```json
{
  "id": 1, "account": "MYACCT", "user": "me",
  "created_at": "…", "last_used": "…", "query_count": 7,
  "sessions": 2, "max_sessions": 4,
  "members": [
    {
      "id": 3, "busy": true,
      "context": { "warehouse": "WH", "role": null, "database": null, "schema": null },
      "last_used": "…", "query_count": 5,
      "running": { "sql": "SELECT …", "started_at": "…" }
    }
  ]
}
```

`sessions` / `max_sessions` count live authenticated sessions against the pool
cap; `members[]` carries one entry per live session, whose `running` is `null`
while the member is idle.

Example exchange:

```text
→ {"service":"snowflake","op":"query","payload":{"account":"MYACCT","user":"me","sql":"SELECT 1 AS N"}}
← {"ok":true,"payload":{"statements":[{"columns":[{"name":"N","type":"fixed(1,0)"}],"rows":[{"N":1}]}]}}
```

The first `query` for an unseen `(account, user)` blocks while the browser SSO
completes (which can take many seconds); the control socket has no client-side
timeout. `account`/`user` omitted from the payload fall back to the daemon's
`SNOWFLAKE_*` / settings.json configuration.
