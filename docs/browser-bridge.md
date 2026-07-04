# Browser Bridge

`omni-dev browser bridge serve` runs a long-lived local process that lets you drive
HTTP requests **through an authenticated browser tab**. When you are
investigating internal services (Grafana/Loki, internal dashboards, SSO-gated
admin panels), the browser holds authenticated sessions — SSO, OAuth, session
cookies — that are hard to replicate programmatically. The bridge lets
`omni-dev` (or a local tool you control) issue requests inside the browser's
authenticated context **without exfiltrating cookies or tokens**.

This is a *confused deputy by design*: the bridge borrows the browser's
authority on behalf of whoever can talk to it. That makes the
[security model](#security-model) the load-bearing part of the design, not an
add-on — both planes are authenticated and default-closed. See
[ADR-0036](adrs/adr-0036.md) for the architectural rationale.

## Table of Contents

1. [How it works](#how-it-works)
2. [Quick start](#quick-start)
3. [Running under the daemon](#running-under-the-daemon)
4. [Talking to the control plane](#talking-to-the-control-plane)
5. [The `request` thin client](#the-request-thin-client)
6. [Routing to a specific tab](#routing-to-a-specific-tab)
7. [Security model](#security-model)
8. [Flags](#flags)
9. [Random ports](#random-ports)
10. [Worked example: downloading Grafana / Loki logs](#worked-example-downloading-grafana--loki-logs)
11. [Harvesting your own data (best-effort)](#harvesting-your-own-data-best-effort)
12. [WebSocket wire protocol](#websocket-wire-protocol)
13. [Caveats](#caveats)
14. [Recipes](#recipes)

## How it works

```
   operator CLI / authorized script             browser tab (DevTools console)
              │                                          │
   HTTP + bearer token + X-Omni-Bridge      WebSocket + token subprotocol
              │                                          │
              ▼                                          ▼
  ┌───────────────────────────── omni-dev browser bridge ─────────────────────┐
  │   HTTP control plane            id-correlator           WebSocket plane    │
  │   127.0.0.1:9998      ───►   pending: id → oneshot   ───►   127.0.0.1:9999 │
  │   (axum)              ◄───   resolve on WS reply     ◄───   (tungstenite)  │
  └────────────────────────────────────────────────────────────────────────-─┘
```

A request flows: **control plane (authenticated) → assign `id` + register a
`oneshot` waiter → serialize a command frame → WebSocket → browser `fetch()` →
response frame → correlator resolves the waiter by `id` → control plane returns
the HTTP response.**

## Quick start

1. Start the bridge:

   ```bash
   omni-dev browser bridge serve
   ```

   It prints the bound ports, the generated session token, and a ready-to-paste
   JS snippet.

2. Open the DevTools console on the authenticated tab (e.g.
   `https://grafana.internal/...`) and paste the snippet. It connects and
   reconnects with backoff, presenting the token via the WebSocket subprotocol.

3. Drive requests from your shell (the token is in the bridge's stdout):

   ```bash
   export OMNI_BRIDGE_TOKEN=<token printed by the bridge>
   omni-dev browser bridge request --url /loki/api/v1/labels
   ```

The bridge works only while the tab is open and the snippet is running.

## Running under the daemon

`serve` is the simplest way to run the bridge — one foreground process you start
and stop by hand. The bridge can **also** be hosted by the long-lived omni-dev
daemon ([ADR-0039](adrs/adr-0039.md)), which keeps it running across terminals,
supervises its lifecycle, and (on macOS) gives it a menu-bar presence. Both ways
run the *same* bridge core on the *same* loopback-TCP planes with the *same*
[security model](#security-model) — `serve` is not deprecated, and which one you
use is invisible to the `request` / `harvest` thin clients.

| Command | What it does |
|---|---|
| `omni-dev daemon run` | **Becomes** the daemon in the foreground: acquires the control socket (the launchd-activated fd when socket-activated, otherwise a self-bound socket that doubles as the single-instance lock), starts the bridge on its planes, and blocks until `SIGTERM`/`SIGINT`/`SIGHUP` or `daemon stop`. This is what launchd demand-spawns (or a launcher execs). |
| `omni-dev daemon start` | Installs the daemon in the background and returns once it is ready. On macOS it bootstraps a launchd LaunchAgent that **owns** the control socket and spawns the daemon on the first client connect (so it also activates at login); `start` warms it with one ping. Elsewhere it spawns `daemon run` in its own session (`setsid`, so it survives the launching terminal) with stdout/stderr appended to a `0600` `daemon.log` beside the socket — but with no auto-start at login ([#1174](https://github.com/rust-works/omni-dev/issues/1174) tracks a systemd user unit). You normally run this once — after that any CLI call re-activates the daemon on demand (macOS socket activation; elsewhere re-run `daemon start` after a reboot). |
| `omni-dev daemon stop` | Stops the daemon and, on macOS, boots out the LaunchAgent — removing the demand socket — so it is not re-activated until the next `daemon start` or login. |
| `omni-dev daemon restart` | `stop` then `start`. On macOS, the only step needed after upgrading from an older `RunAtLoad` daemon to pick up the socket-activated agent. |
| `omni-dev daemon status` | Reports the daemon and each hosted service (`--json` for machines). Under socket activation, "running" means a process is currently spawned; "not running" means none is resident right now, not that the daemon is unavailable (it re-activates on the next connect) — unless it was `stop`ped. |

`daemon run` accepts the bridge's port/scope flags as `--bridge-ws-port`,
`--bridge-control-port`, `--bridge-allow-origin`, and `--bridge-token-file`
(plus `--socket <PATH>` to override the control-socket location and `--no-menu`
to stay headless on a macOS menu-bar build).

### Token discovery (no foreground stdout)

A standalone `serve` prints the session token to stdout; a backgrounded daemon
has none. So the daemon **persists the resolved token to a `0600` file** —
`bridge.token` under the per-user runtime directory (`dirs::data_dir()`:
`~/Library/Application Support/omni-dev/` on macOS, `~/.local/share/omni-dev/` on
Linux; this is *not* the `~/.omni-dev` config directory). The thin clients fall
back to that file automatically, so under the daemon you usually need neither
`OMNI_BRIDGE_TOKEN` nor `--token-file`:

```bash
omni-dev daemon start
omni-dev browser bridge request --url /loki/api/v1/labels   # token discovered from the file
```

The token is still **generated** (never taken from argv), still compared in
constant time, and the file is still required to be `0600` — see the
[security model](#security-model). It is removed when the bridge shuts down.

### Checking on it

```bash
omni-dev daemon status
# daemon: running
#   browser-bridge   ok         1 tab(s), 0 pending (control :9998, ws :9999)
```

Before any tab connects the line reads `no tab connected (control :9998, ws
:9999)`; `omni-dev daemon status --json` emits the structured per-service report.
The `GET /__bridge/status` endpoint is unchanged and still feeds this view.

### Menu bar (macOS, `menu-bar` feature)

Built with `--features menu-bar` on macOS, the daemon adds a **Browser Bridge**
menu showing the connection line (`Connected — <origins> — N pending` /
`No tab connected`) and a **masked** session key (`Key: ••••<last 4 chars>`, so
the full token never appears in the menu bar, screenshots, or screen shares),
with actions:

- **Copy bridge key** — the raw session token, to paste into `OMNI_BRIDGE_TOKEN`.
- **Copy console snippet** — the ready-to-paste DevTools snippet.
- **Copy request command** — a complete `OMNI_BRIDGE_TOKEN='…' omni-dev browser
  bridge request …` line.
- **Disconnect tab `<id>`** — one entry per connected tab.
- **Restart bridge** — stop and relaunch the planes with the same token.

The menu is compiled out on non-macOS builds and when the `menu-bar` feature is
off; those builds (and `daemon run --no-menu`) are headless.

## Talking to the control plane

The control plane is a local HTTP server (default `127.0.0.1:9998`). Every
request must include `Authorization: Bearer <token>` **and** `X-Omni-Bridge: 1`.

### 1. Transparent proxy (the easy path)

Any request whose path is **not** under `/__bridge/` is forwarded (method,
headers, body) to the browser, with `url` set to the request path so it resolves
against the **page's origin**:

```bash
T="$OMNI_BRIDGE_TOKEN"
curl -H "Authorization: Bearer $T" -H "X-Omni-Bridge: 1" \
  http://localhost:9998/loki/api/v1/labels
```

### 2. Control / full-fidelity endpoints (reserved `/__bridge/` prefix)

| Method & path            | Purpose                                                                 |
|--------------------------|-------------------------------------------------------------------------|
| `GET  /__bridge/status`  | `{ "connected": bool, "browser_origin": str?, "tabs": [{id, origin?}], "pending": int }`. `tabs` lists every connected tab; `browser_origin` is the lone tab's origin (present only when exactly one is connected, for back-compat). |
| `POST /__bridge/request` | Full control. Body `{url, method, headers, body, stream?, target?, allow_origin?, credentials?}`; returns a structured response envelope, or — when `"stream": true` — an NDJSON chunk stream (see [Streaming](#streaming-responses)). Cross-origin `url` rejected unless `serve --allow-origin` or the per-request `allow_origin` permits it (see [Outbound request scope](#outbound-request-scope-default-closed)). See [Routing to a tab](#routing-to-a-specific-tab) for `target`. |

```bash
curl -s -H "Authorization: Bearer $T" -H "X-Omni-Bridge: 1" \
  http://localhost:9998/__bridge/request -d '{
  "url": "/loki/api/v1/labels", "method": "GET",
  "headers": {"Accept": "application/json"}, "body": null
}'
# → {"id": 7, "status": 200, "headers": {...}, "body": "..."}
```

## The `request` thin client

`omni-dev browser bridge request` reads the token (from `OMNI_BRIDGE_TOKEN` or
`--token-file`), adds the required headers, POSTs to `/__bridge/request` on a
*running* bridge, and prints the response envelope:

```bash
omni-dev browser bridge request --url /loki/api/v1/labels --method GET
omni-dev browser bridge request --url /api/foo --method POST --body @payload.json
omni-dev browser bridge request --control-port 9998 --url /api/foo   # custom port
omni-dev browser bridge request --url /api/foo --header "Accept: application/json"
omni-dev browser bridge request --url https://cdn.example.com/x.js --credentials omit
```

`--body @file` reads the body from a file; otherwise the value is sent verbatim.

`--credentials <include|omit|same-origin>` sets the browser `fetch()` credentials
mode (default `include`, unchanged). Use `omit` to read a wildcard-CORS
(`Access-Control-Allow-Origin: *`) cross-origin response — e.g. a public CDN
asset — which the browser refuses to expose to a credentialed request. **`omit`
sends no cookies, so never use it for a cross-origin endpoint that needs the
user's session: the request would be unauthenticated.** Pair with `--allow-origin`
to permit the cross-origin target.

Binary responses (images, gzipped blobs, file downloads) come back in the
envelope as a base64 string with `"encoding": "base64"`; the caller decodes it.
The transparent proxy (below) decodes automatically, so `curl` receives the raw
bytes — pipe straight to a file with `--output`.

Pass `--stream` to consume a streaming / chunked / long-lived endpoint (SSE,
log-tail) instead of buffering the whole response: the decoded body bytes are
written to stdout as they arrive, and the head status is printed to stderr.

```bash
omni-dev browser bridge request --stream --url /events | your-consumer   # SSE / chunked
```

## Routing to a specific tab

Several authenticated tabs can be connected at once — paste the snippet into each
(e.g. a Grafana tab *and* an internal admin tab). Each connection is independent:
a new tab never evicts an existing one, and each authenticates on its own via the
token subprotocol.

`GET /__bridge/status` lists them, each with a server-assigned **connection id**
and its `Origin`:

```bash
curl -s "${H[@]}" http://localhost:9998/__bridge/status
# → {"connected":true,
#    "tabs":[{"id":1,"origin":"https://grafana.internal"},
#            {"id":2,"origin":"https://admin.internal"}],
#    "pending":0}
```

Select which tab a request targets with either:

- the **`X-Omni-Bridge-Target`** header (works on the transparent proxy *and*
  `POST /__bridge/request`), or
- a **`target`** field in the `POST /__bridge/request` body (or `--target` on the
  thin client).

The header takes precedence over the body field. A target is either a **connection
id** (canonical, always unambiguous) or an **`Origin`** that uniquely matches one
tab:

```bash
omni-dev browser bridge request --target 2 --url /api/foo                # by id
omni-dev browser bridge request --target https://admin.internal --url /x # by origin
curl "${H[@]}" -H "X-Omni-Bridge-Target: 1" http://localhost:9998/api/foo
```

Resolution rules:

| Situation | Result |
|-----------|--------|
| No tab connected | `503` |
| Exactly one tab, no target | Routes to it (v1 back-compat) |
| Several tabs, no target | `409` — specify a target (the error lists the tabs) |
| Target id / origin matches one tab | Routes to it |
| Target id / origin matches none | `404` |
| Target origin matches several tabs | `409` — target by connection id instead |

> Requests are routed to **exactly one** tab; there is no fan-out to multiple
> tabs. `--allow-origin` remains a single global value applied to every
> connection (a per-origin allowlist is possible future work).

## Security model

The trust boundary, stated once: **a request is trusted only if it presents the
session token AND is not a cross-origin browser request; everything else is
denied.** A localhost bind is *necessary but not sufficient* — it stops off-host
access but not other local users/processes, nor web pages you visit while the
bridge runs.

### Session token (mandatory, both planes)
- Generated at startup and printed to stdout with the snippet. It is **never**
  accepted as a CLI argument (argv is world-readable via `ps`/`/proc`). It may
  optionally come from `OMNI_BRIDGE_TOKEN` or a `0600` `--token-file`.
- Control plane: every request must carry `Authorization: Bearer <token>`.
- WebSocket plane: the browser presents the token via the WS subprotocol; the
  upgrade is rejected without it. An unauthenticated peer can never connect or
  evict the authenticated browser connection.

### Control-plane anti-CSRF / anti-DNS-rebinding (all enforced)
- **`Host` allowlist** — only `localhost:<port>` / `127.0.0.1:<port>` /
  `[::1]:<port>` accepted (blocks DNS rebinding).
- **Reject browser-originated requests** — any request carrying an `Origin`
  header or `Sec-Fetch-Site: cross-site`/`same-site` is denied. A legitimate CLI
  client sends neither.
- **Require `X-Omni-Bridge: 1`** on every request — a custom header forces a
  CORS preflight, which the server refuses, blocking simple-request CSRF.
- **No CORS allow headers; `OPTIONS` is never answered.**

### Outbound request scope (default-closed)
- The browser snippet is **not trusted** to scope requests; the server enforces
  scope before sending the command.
- **Relative URLs only by default** (resolved against the page origin).
  Absolute / cross-origin URLs are rejected unless explicitly enabled with
  `--allow-origin <url>`.
- The `serve --allow-origin` global feeds **two** checks at different times: the
  WebSocket **upgrade** gate (the *extension's* origin, at connection time) and
  the per-request outbound-URL check (the *target's* origin). To widen the
  outbound scope for a single request **without** disturbing the connected tab,
  pass `request --allow-origin <url>` instead: it reaches only the outbound-URL
  check, takes precedence over the global for that request, and never the WS
  gate. A WARN is logged at dispatch whenever the per-request override is used.
- **Security note:** a per-request override lets a session-token holder direct
  the tab's `fetch` at an arbitrary cross-origin URL. Blast radius is bounded by
  the browser's own CORS (the response body is readable only if the target sends
  permissive CORS), and a token holder can already issue authenticated
  same-origin requests. It is **per-request and explicit** — never a default.

### Resource & robustness limits
- Per-request timeout (`--request-timeout`, default 30s → `504`), max response
  body size (`--max-body-bytes`), and a concurrency cap (`--max-concurrent`).
- **Fail-closed binding**: ports bind with `SO_REUSEADDR` so a restart can rebind
  through a lingering `TIME_WAIT` (#990); a live listener on a fixed port still
  yields `EADDRINUSE` and the bridge exits rather than racing a squatter
  (`SO_REUSEADDR` ≠ `SO_REUSEPORT`).
- Caller header names/values with CR/LF are rejected; the request path is
  normalized and traversal-checked before the `/__bridge/` routing split.

### Accepted risks / relied-upon controls
- `ws://localhost` is plaintext; loopback traffic is not sniffable without root —
  accepted.
- The browser forbids JS from setting `Cookie` or reading `Set-Cookie`, so the
  snippet cannot smuggle those — a relied-upon browser control.
- No request URLs, queries, headers, or bodies are logged at the default level
  (they contain authenticated/sensitive data).

## Flags

| Flag | Default | Purpose |
|------|---------|---------|
| `--ws-port <PORT>` | `9999` | WebSocket-plane port. `0` binds a random free port. |
| `--control-port <PORT>` | `9998` | HTTP control-plane port. `0` binds a random free port. |
| `--request-timeout <SECS>` | `30` | Per-request timeout before the control plane returns `504`. |
| `--allow-origin <URL>` | — | Permit this exact cross-origin for the WS upgrade and outbound URLs. |
| `--max-body-bytes <N>` | `8388608` | Maximum browser response body size accepted. |
| `--max-concurrent <N>` | `64` | Maximum concurrent in-flight requests. |
| `--token-file <PATH>` | — | Read the session token from this `0600` file instead of generating one. |

The `request` subcommand takes `--url`, `--method` (default `GET`),
`--header` (repeatable), `--body` (`@file` supported),
`--credentials <include|omit|same-origin>` (default `include`), `--stream`
(chunk the response to stdout), `--target` (route to a connection id / origin —
see [Routing to a specific tab](#routing-to-a-specific-tab)), `--allow-origin`
(permit a cross-origin outbound URL for this request only — see
[Outbound request scope](#outbound-request-scope-default-closed)),
`--control-port` (default `9998`), and `--token-file`.

## Random ports

Pass `0` to either port flag to let the OS assign a free port; the bridge reads
back the actual port and reflects it in the printed instructions and snippet:

```bash
omni-dev browser bridge serve --ws-port 0 --control-port 0
```

This is useful when the defaults are taken, or when running several bridges.
Point the thin client at the printed control port with
`--control-port <printed>`.

## Worked example: downloading Grafana / Loki logs

The bridge issues the same same-origin requests the Grafana UI makes, carrying
the `grafana_session` cookie; CSRF passes because the `fetch()` runs in-page.

```bash
T="$OMNI_BRIDGE_TOKEN"; H=(-H "Authorization: Bearer $T" -H "X-Omni-Bridge: 1")

# 1. Discover the Loki datasource UID (same-origin GET):
curl "${H[@]}" http://localhost:9998/api/datasources       # find {"type":"loki","uid":"…"}

# 2. Pull a time window of logs (GET → relative URL, page origin):
curl "${H[@]}" 'http://localhost:9998/api/datasources/proxy/uid/<DS_UID>/loki/api/v1/query_range\
?query={app="foo"}|="error"&start=<ns>&end=<ns>&limit=5000&direction=backward'
```

Pagination is client-side: query `direction=backward`, take the oldest returned
timestamp as the next `end`, repeat until empty. Keep each buffered page modest —
the bridge buffers full bodies under `--request-timeout`. For genuinely streaming
endpoints (Grafana Live, SSE, chunked APIs), opt into streaming with `--stream` /
`?__stream=1` (see [Streaming responses](#streaming-responses)) instead of
paginating. The `POST /api/ds/query` form is also supported via
`omni-dev browser bridge request --method POST`.

A canonical drain loop using the `request` thin client and `jq` (the bridge
prints the response envelope as JSON; the upstream body is the envelope's
`body`):

```bash
DS_UID=<loki-datasource-uid>
QUERY='{app="foo"}|="error"'
END=$(date +%s)000000000   # now, in nanoseconds
LIMIT=5000

while :; do
  page=$(omni-dev browser bridge request \
    --url "/api/datasources/proxy/uid/${DS_UID}/loki/api/v1/query_range?query=${QUERY}&end=${END}&limit=${LIMIT}&direction=backward")

  # The upstream JSON is the envelope's `body` string; parse it once.
  body=$(printf '%s' "$page" | jq -r '.body')

  # Stop when the page returns no log entries.
  rows=$(printf '%s' "$body" | jq '[.data.result[].values[]] | length')
  [ "$rows" -eq 0 ] && break

  printf '%s\n' "$body"   # collect / process this page

  # Oldest timestamp on this page (ns) becomes the next `end`, minus 1ns so we
  # don't re-fetch the boundary entry.
  oldest=$(printf '%s' "$body" | jq -r '[.data.result[].values[][0] | tonumber] | min')
  END=$((oldest - 1))
done
```

Each page must still fit under `--max-body-bytes`. If a single page trips the
over-limit error, it tells you exactly what to do: lower `--limit`/narrow the
window so each page is smaller, or raise `--max-body-bytes` to accept a larger
page.

## Harvesting your own data (best-effort)

The recipes above drive APIs you assemble by hand. For some sites the sequence
is involved enough — harvest CSRF tokens from HTML, discover a persisted-query
id from a lazily-loaded cross-origin bundle, then replay a paginating GraphQL
query — that it is packaged as a built-in command tree:

```
omni-dev browser bridge harvest <platform> <object>
```

Today the only target is your **own** Facebook timeline:

```bash
# Page your whole timeline to a file, newest-first, one JSON post per line:
omni-dev browser bridge harvest facebook posts --output my-posts.jsonl

# Sample the most recent 20 posts to stdout:
omni-dev browser bridge harvest facebook posts --limit 20

# Incremental archive: stop once posts predate a cutoff (Unix seconds or ISO-8601):
omni-dev browser bridge harvest facebook posts --since 2024-01-01T00:00:00Z

# Resume a run interrupted by a 504 / token rotation from its saved cursor:
omni-dev browser bridge harvest facebook posts --output my-posts.jsonl --resume run.state
```

Each post is `{ id, creation_time, text, url, shared_link }`. `--format jsonl`
(default) streams one post per line and is append-friendly on `--resume`;
`--format json` writes a single array when the run completes. `--target`,
`--token-file`, and `--control-port` mirror `bridge request`.

The command reuses the same `bridge request` dispatch path: it needs a running
`bridge serve` with a Facebook tab connected (verify with `GET /__bridge/status`),
and its cross-origin `doc_id`-discovery step requires the bridge to permit
`https://static.xx.fbcdn.net` (it sends that per-request via the same machinery
as `request --allow-origin … --credentials omit`). The full manual recipe this
encapsulates — harvesting tokens, discovering the pagination `doc_id`, and the
paginating GraphQL loop, with the field map — is written up in
[Recipe: querying your own Facebook data](recipes/browser-bridge-facebook.md).

> **Best-effort contract.** This drives **reverse-engineered, undocumented**
> Facebook internals. It re-harvests every volatile value (GraphQL `doc_id`s,
> relay-provider flags, session tokens) on each run — nothing is hardcoded — and
> fails with a staged, actionable error naming the step that drifted rather than
> panicking. Even so, it **can break whenever Facebook changes** its query ids,
> page structure, or response shape. It only ever uses the session already
> logged into the connected tab (**your own account only**; mind Facebook's ToS).
> For a stable archive, prefer Facebook's official **"Download Your Information"**
> export.

## WebSocket wire protocol

Newline-free JSON frames, correlated by a monotonic integer `id`.

**Server → browser (command):**
```json
{"id": 7, "url": "/loki/api/v1/labels", "method": "GET", "headers": {}, "body": null}
```

**Browser → server (success, text):**
```json
{"id": 7, "status": 200, "headers": {"content-type": "application/json"}, "body": "..."}
```

**Browser → server (success, binary):**
```json
{"id": 7, "status": 200, "headers": {"content-type": "image/png"}, "body": "iVBORw0K…", "encoding": "base64"}
```

The snippet reads non-text bodies (anything whose `Content-Type` is not
`text/*`, JSON, XML, or JavaScript) via `arrayBuffer()` and base64-encodes them,
tagging the frame with `"encoding": "base64"`. Text bodies omit `encoding`
(back-compat). `--max-body-bytes` is enforced against the **decoded** size.

**Browser → server (error):**
```json
{"id": 7, "error": "Failed to fetch"}
```

### Streaming responses

When a request opts in (`stream: true` on `POST /__bridge/request`, `--stream` on
the thin client, or `?__stream=1` on the transparent proxy), the server sends
`{… "stream": true}` in the command and the snippet reads
`response.body.getReader()`, emitting **a head frame, then one base64 chunk frame
per read, then a terminator** instead of a single buffered reply:

```json
{"id": 7, "status": 200, "headers": {"content-type": "text/event-stream"}, "stream": true}
{"id": 7, "seq": 0, "chunk": "ZGF0YTogMQ=="}
{"id": 7, "seq": 1, "chunk": "ZGF0YTogMg=="}
{"id": 7, "done": true}
```

The server streams the body out as it arrives:

- **Transparent proxy** (`?__stream=1`) → the decoded bytes as a native chunked
  HTTP body (curl-friendly: `curl -N … '/events?__stream=1'`).
- **`POST /__bridge/request`** (`"stream": true`) → an NDJSON body
  (`application/x-ndjson`): a `{status,headers}` head line, `{seq,chunk}` lines,
  then a `{done:true}` line.

For a stream, **`--request-timeout` is an inter-chunk idle timeout** (reset on
each chunk, not a total deadline) and **`--max-body-bytes` is a cumulative
ceiling** across all chunks; either limit ending the stream sends the browser a
`{"id": 7, "cancel": true}` frame so it stops its reader. The browser also
receives a cancel when the control-plane consumer disconnects mid-stream.

**Server → browser (cancel):**
```json
{"id": 7, "cancel": true}
```

Rules:
- `id` is assigned by the server; the browser echoes it back unchanged.
- Multiple commands may be in flight concurrently over one socket (id-keyed).
- A command with no reply within `--request-timeout` resolves the control-plane
  request with `504 Gateway Timeout` and is dropped from `pending`.
- **Several authenticated** tabs may be connected at once, keyed by connection
  id; a request routes to one of them (see
  [Routing to a specific tab](#routing-to-a-specific-tab)). A new connection
  never evicts an existing one, and an unauthenticated peer can neither connect
  nor evict any of them.

## Caveats

- A strict CSP `connect-src` (fallback `default-src`) can block the WebSocket.
- HTTPS page → `ws://localhost` is normally allowed (localhost is a
  "potentially trustworthy" origin); confirm per page.
- CORS constrains the in-page `fetch()`, not the bridge: same-origin (relative
  URLs) is unaffected; cross-origin targets depend on their
  `Access-Control-Allow-Origin`.
- The bridge works only while the tab is open and the snippet is running.
- Streaming reads the body ahead of the consumer: memory is bounded by
  `--max-body-bytes` (cumulative), but there is no socket-level backpressure to
  the browser's `getReader()` — a fast producer with a slow consumer is capped,
  not throttled.
- Remaining limitations: no stdin piping, and a request fans out to at most one
  tab. Multiple concurrent tabs (with routing), binary bodies, *and*
  streaming/chunked responses are all supported (see the wire protocol above).

## Recipes

Worked end-to-end procedures that drive a specific service through the bridge:

- [Querying your own Facebook data](recipes/browser-bridge-facebook.md) — page
  your own profile timeline via Facebook's internal Relay/GraphQL API: harvest
  CSRF/session tokens, discover the pagination `doc_id` from a cross-origin
  bundle (the worked example for `request --allow-origin` + `--credentials omit`),
  and run the paginating GraphQL loop. This is the manual recipe the built-in
  [`harvest facebook posts`](#harvesting-your-own-data-best-effort) command
  automates.
