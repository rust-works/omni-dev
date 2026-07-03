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
without this (see issue #1003).

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

# Per-query context is applied with USE … on the reused session.
omni-dev snowflake query --account MYACCT --user me \
  --warehouse WH --role ANALYST --database DB --schema PUBLIC "SELECT 1"

# YAML instead of JSON.
omni-dev snowflake query --account MYACCT --user me --format yaml "SELECT 1"

# List / evict multiplexed sessions.
omni-dev snowflake sessions          # table; --json for machines
omni-dev snowflake disconnect --account MYACCT --user me
```

`query` returns a self-describing payload:

```json
{
  "columns": [{ "name": "ID", "type": "fixed(38,0)" }, { "name": "NAME", "type": "text(16777216)" }],
  "rows": [{ "ID": 1, "NAME": "hello" }]
}
```

Cell types map as: `fixed` (scale 0) → integer (falling back to the exact string
when it overflows `i64`); `fixed` (scale > 0) / `real`/`float`/`double` → number;
`boolean` → bool; `text` → string; `date`/`time`/`timestamp_*` → ISO-8601 string;
`variant`/`object`/`array` → parsed JSON; `binary` (hex) and other types → the raw
string. An empty result reports `columns: []` (the connector exposes column
metadata per-row only).

## Configuration

Account, user, and default context resolve **env var first, then
`~/.omni-dev/settings.json`** (the Atlassian credential pattern), and are
overridable per request via flags:

| Setting   | Env var               | `query` flag    |
|-----------|-----------------------|-----------------|
| Account   | `SNOWFLAKE_ACCOUNT`   | `--account`     |
| User      | `SNOWFLAKE_USER`      | `--user`        |
| Warehouse | `SNOWFLAKE_WAREHOUSE` | `--warehouse`   |
| Role      | `SNOWFLAKE_ROLE`      | `--role`        |
| Database  | `SNOWFLAKE_DATABASE`  | `--database`    |
| Schema    | `SNOWFLAKE_SCHEMA`    | `--schema`      |
| Pool size | `SNOWFLAKE_POOL_SIZE` | — (config only) |
| HTTP timeout (s) | `SNOWFLAKE_HTTP_TIMEOUT` | — (config only) |
| Sign-in deadline (s) | `SNOWFLAKE_AUTH_TIMEOUT` | — (config only) |
| Query deadline (s) | `SNOWFLAKE_QUERY_TIMEOUT` | — (config only) |
| Keep-alive interval (s) | `SNOWFLAKE_HEARTBEAT_INTERVAL` | — (config only) |

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

`settings.json` fallback example:

```json
{ "env": { "SNOWFLAKE_ACCOUNT": "MYACCT", "SNOWFLAKE_USER": "me" } }
```

To hold several accounts on one machine, put each account's `SNOWFLAKE_*`
variables in a named **profile** and select it with `--profile` /
`OMNI_DEV_PROFILE` (see
[Credential Profiles](configuration-best-practices.md#credential-profiles)).
Because the service pools sessions by `(account, user)`, distinct profiles
produce distinct pools automatically.

The service is **account-agnostic**: there is no hardcoded account list. Defaults
are applied at session creation; the per-query flags issue `USE WAREHOUSE/ROLE/
DATABASE/SCHEMA` on the reused session, so one session serves varied query
contexts without re-auth.

## Security

- **No new socket or port.** Requests ride the daemon's existing `0600` Unix
  control socket in the `0700` runtime directory; those filesystem permissions
  are the trust boundary. Arbitrary SQL runs as the authenticated user — intended.
- **No secret persisted.** Unlike the browser bridge (which writes a `0600` token
  file), the Snowflake service keeps the live `SnowflakeSession` in memory only;
  shutdown simply drops it. Per-query context flags are validated as bare
  identifiers before interpolation into `USE …`.

## SSO under the daemon

The client's default external-browser flow auto-opens a browser and binds an
ephemeral localhost callback — no TTY needed, suitable for the resident daemon.
(The browser launch is configurable, so it can target a specific profile in a new
window; threading that from settings is a follow-up.) The first query for a new `(account, user)`
triggers the popup; reuse is silent. On a session-expiry error the session is
evicted and the next query re-authenticates (so the popup is always tied to a user
action). If browser launch is unreliable under a background launchd daemon, run
the first auth from a foreground context (`omni-dev daemon start --foreground`).

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

| op           | payload                                                                 | success payload                          |
|--------------|-------------------------------------------------------------------------|------------------------------------------|
| `query`      | `{ account?, user?, warehouse?, role?, database?, schema?, sql }`        | `{ columns: [...], rows: [...] }`        |
| `sessions`   | `null`                                                                  | `{ sessions: [{ id, account, user, created_at, last_used, query_count }] }` |
| `disconnect` | `{ account, user }`                                                     | `{ disconnected: <bool> }`               |

Example exchange:

```text
→ {"service":"snowflake","op":"query","payload":{"account":"MYACCT","user":"me","sql":"SELECT 1 AS N"}}
← {"ok":true,"payload":{"columns":[{"name":"N","type":"fixed(1,0)"}],"rows":[{"N":1}]}}
```

The first `query` for an unseen `(account, user)` blocks while the browser SSO
completes (which can take many seconds); the control socket has no client-side
timeout. `account`/`user` omitted from the payload fall back to the daemon's
`SNOWFLAKE_*` / settings.json configuration.
