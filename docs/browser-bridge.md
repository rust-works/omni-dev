# Browser Bridge

`omni-dev browser bridge` runs a long-lived local process that lets you drive
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
3. [Talking to the control plane](#talking-to-the-control-plane)
4. [The `request` thin client](#the-request-thin-client)
5. [Security model](#security-model)
6. [Flags](#flags)
7. [Random ports](#random-ports)
8. [Worked example: downloading Grafana / Loki logs](#worked-example-downloading-grafana--loki-logs)
9. [WebSocket wire protocol](#websocket-wire-protocol)
10. [Caveats](#caveats)

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
   omni-dev browser bridge
   ```

   It prints the bound ports, the generated session token, and a ready-to-paste
   JS snippet.

2. Open the DevTools console on the authenticated tab (e.g.
   `https://grafana.internal/...`) and paste the snippet. It connects and
   reconnects with backoff, presenting the token via the WebSocket subprotocol.

3. Drive requests from your shell (the token is in the bridge's stdout):

   ```bash
   export OMNI_BRIDGE_TOKEN=<token printed by the bridge>
   omni-dev browser request --url /loki/api/v1/labels
   ```

The bridge works only while the tab is open and the snippet is running.

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
| `GET  /__bridge/status`  | `{ "connected": bool, "browser_origin": str?, "pending": int }`         |
| `POST /__bridge/request` | Full control. Body `{url, method, headers, body}`; returns a structured response envelope. Cross-origin `url` rejected unless `--allow-origin` permits it. |

```bash
curl -s -H "Authorization: Bearer $T" -H "X-Omni-Bridge: 1" \
  http://localhost:9998/__bridge/request -d '{
  "url": "/loki/api/v1/labels", "method": "GET",
  "headers": {"Accept": "application/json"}, "body": null
}'
# → {"id": 7, "status": 200, "headers": {...}, "body": "..."}
```

## The `request` thin client

`omni-dev browser request` reads the token (from `OMNI_BRIDGE_TOKEN` or
`--token-file`), adds the required headers, POSTs to `/__bridge/request` on a
*running* bridge, and prints the response envelope:

```bash
omni-dev browser request --url /loki/api/v1/labels --method GET
omni-dev browser request --url /api/foo --method POST --body @payload.json
omni-dev browser request --control-port 9998 --url /api/foo   # custom port
omni-dev browser request --url /api/foo --header "Accept: application/json"
```

`--body @file` reads the body from a file; otherwise the value is sent verbatim.

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

### Resource & robustness limits
- Per-request timeout (`--request-timeout`, default 30s → `504`), max response
  body size (`--max-body-bytes`), and a concurrency cap (`--max-concurrent`).
- **Fail-closed binding**: ports bind without `SO_REUSEADDR`; if a fixed port is
  taken, the bridge exits rather than racing a squatter.
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
`--header` (repeatable), `--body` (`@file` supported), `--control-port`
(default `9998`), and `--token-file`.

## Random ports

Pass `0` to either port flag to let the OS assign a free port; the bridge reads
back the actual port and reflects it in the printed instructions and snippet:

```bash
omni-dev browser bridge --ws-port 0 --control-port 0
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
timestamp as the next `end`, repeat until empty. Keep each page modest — v1
buffers full bodies (`resp.text()`) under `--request-timeout`. The
`POST /api/ds/query` form is also supported via
`omni-dev browser request --method POST`.

## WebSocket wire protocol

Newline-free JSON frames, correlated by a monotonic integer `id`.

**Server → browser (command):**
```json
{"id": 7, "url": "/loki/api/v1/labels", "method": "GET", "headers": {}, "body": null}
```

**Browser → server (success):**
```json
{"id": 7, "status": 200, "headers": {"content-type": "application/json"}, "body": "..."}
```

**Browser → server (error):**
```json
{"id": 7, "error": "Failed to fetch"}
```

Rules:
- `id` is assigned by the server; the browser echoes it back unchanged.
- Multiple commands may be in flight concurrently over one socket (id-keyed).
- A command with no reply within `--request-timeout` resolves the control-plane
  request with `504 Gateway Timeout` and is dropped from `pending`.
- Only **one authenticated** browser connection is held; an unauthenticated peer
  cannot connect or evict it.

## Caveats

- A strict CSP `connect-src` (fallback `default-src`) can block the WebSocket.
- HTTPS page → `ws://localhost` is normally allowed (localhost is a
  "potentially trustworthy" origin); confirm per page.
- CORS constrains the in-page `fetch()`, not the bridge: same-origin (relative
  URLs) is unaffected; cross-origin targets depend on their
  `Access-Control-Allow-Origin`.
- The bridge works only while the tab is open and the snippet is running.
- v1 limitations: full response bodies are buffered (no streaming), text bodies
  only (no binary), a single browser tab, and no stdin/stdout piping.
