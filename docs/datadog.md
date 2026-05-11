# Datadog Integration

omni-dev exposes read-only access to the Datadog v1/v2 APIs through the
`omni-dev datadog` command tree, with a matching `datadog_*` MCP tool for every
subcommand. Authentication, time-range syntax, rate-limit behaviour, and
pagination are identical across both surfaces; the MCP tools simply return YAML
matching the CLI's `-o yaml` output. For the MCP-tool reference (parameters
only), see [docs/mcp.md](mcp.md#datadog-14-tools).

## Table of Contents

1. [Prerequisites](#prerequisites)
2. [Authentication](#authentication)
3. [Output formats](#output-formats)
4. [Time-range syntax](#time-range-syntax)
5. [Metrics](#metrics)
6. [Monitors](#monitors)
7. [Dashboards](#dashboards)
8. [Logs](#logs)
9. [Events](#events)
10. [SLOs](#slos)
11. [Downtimes](#downtimes)
12. [Hosts](#hosts)
13. [Rate limits and retry behaviour](#rate-limits-and-retry-behaviour)
14. [Troubleshooting](#troubleshooting)
15. [See also](#see-also)

## Prerequisites

- A Datadog account with both an **API key** (organisation-scoped) and an
  **Application key** (user-scoped). Both are required for every endpoint
  exposed here — the API key alone is not sufficient.
- See Datadog's [API and Application Keys] documentation for how to generate
  each. Free-tier accounts work for most endpoints; some (e.g. some SLO
  details) require a paid plan.

[API and Application Keys]: https://docs.datadoghq.com/account_management/api-app-keys/

## Authentication

Credentials are read from environment variables first, falling back to
`~/.omni-dev/settings.json` (written by `omni-dev datadog auth login`).

### Environment variables

| Variable           | Purpose                                                                                       | Default          |
|--------------------|-----------------------------------------------------------------------------------------------|------------------|
| `DATADOG_API_KEY`  | Organisation API key (required).                                                              | _none_           |
| `DATADOG_APP_KEY`  | Application key tied to a user account (required).                                            | _none_           |
| `DATADOG_SITE`     | Datadog region identifier (e.g. `datadoghq.eu`).                                              | `datadoghq.com`  |
| `DATADOG_API_URL`  | Explicit API base URL; overrides `DATADOG_SITE` entirely. Use for on-prem, proxies, or tests. | _unset_          |

Environment variables take precedence over the settings file. Setting
`DATADOG_API_URL` to `https://datadog.internal.example.com` bypasses the
`api.{site}` derivation and points the client at that URL verbatim — useful
for on-prem Datadog or when routing traffic through a corporate proxy.

`DATADOG_SITE` is normalised: `https://api.us3.datadoghq.com/` collapses to
`us3.datadoghq.com`, so pasting a Datadog UI URL also works.

### Known sites

These site values are recognised silently:

| Site               | Region                                       |
|--------------------|----------------------------------------------|
| `datadoghq.com`    | US1 (default)                                |
| `us3.datadoghq.com`| US3                                          |
| `us5.datadoghq.com`| US5                                          |
| `datadoghq.eu`     | EU1 (Frankfurt)                              |
| `ap1.datadoghq.com`| AP1 (Tokyo)                                  |
| `ddog-gov.com`     | US1-FED (US Government)                      |

Any other value is accepted but prints a stderr warning:

```
warning: Datadog site 'foo.example' is not a known region; proceeding anyway
```

For genuinely-new regions this is harmless; for on-prem installs prefer
`DATADOG_API_URL` (which silences the warning).

### Interactive setup

```bash
$ omni-dev datadog auth login
Configure Datadog API credentials

API key: ***
Application key: ***
Site [default: datadoghq.com]:

Credentials saved to ~/.omni-dev/settings.json
  Site: datadoghq.com

Run `omni-dev datadog auth status` to verify.
```

The site prompt accepts the same shorthand as `DATADOG_SITE` (including full
URLs, which are normalised).

### Verifying credentials

```bash
$ omni-dev datadog auth status
Checking Datadog authentication for site 'datadoghq.com'...
Authenticated successfully.
Site: datadoghq.com
Base URL: https://api.datadoghq.com
```

This calls `/api/v1/validate` against the configured site. The matching MCP
tool, `datadog_auth_status`, returns boolean presence flags only — it never
echoes secret values.

### Removing credentials

```bash
$ omni-dev datadog auth logout
Datadog credentials removed from ~/.omni-dev/settings.json
```

Idempotent: if no credentials are configured, it prints
`No Datadog credentials were configured.` and exits successfully.

## Output formats

Every leaf subcommand accepts `-o <format>`. Defaults to `table`.

| Format  | Best for                                          |
|---------|---------------------------------------------------|
| `table` | Human-readable terminal output (default).          |
| `json`  | Scripting; pipe into `jq`.                         |
| `yaml`  | Single-document YAML; matches the MCP tool output. |
| `yamls` | `---`-separated multi-document YAML stream.        |
| `jsonl` | One compact JSON object per line; Unix-pipe friendly. |

Example:

```bash
$ omni-dev datadog monitor list --tags env:prod -o jsonl | \
    jq -c 'select(.overall_state == "Alert") | {id, name}'
{"id":12345,"name":"API latency p99"}
{"id":67890,"name":"Worker queue depth"}
```

## Time-range syntax

`--from` and `--to` on `metrics query`, `logs search`, and `events list`
accept four forms:

- **Relative shorthand**: `{N}{s|m|h|d|w}` — e.g. `30s`, `15m`, `1h`, `7d`,
  `2w`. Anchored at "now" at the moment the command runs.
- **Literal `now`**.
- **RFC 3339 timestamp** _with_ timezone, e.g. `2026-04-22T10:00:00Z` or
  `2026-04-22T10:00:00+10:00`. Timezone is mandatory.
- **Unix epoch seconds** as a non-negative integer, e.g. `1700000000`.

**Compound forms are rejected.** `1h30m` produces
`Invalid time range: 1h30m`; use `90m` instead, or RFC 3339 timestamps.

The `--from` flag on `metrics catalog list` and `hosts list` is different —
those take only **Unix epoch seconds** (no shorthand). Their help text calls
this out explicitly.

## Metrics

The metrics family has two distinct subcommands:

- `metrics query` — point-in-time timeseries values.
- `metrics catalog list` — discover metric *names* available in the org.

### Query a timeseries

```bash
$ omni-dev datadog metrics query \
    --query 'avg:system.cpu.user{host:web-01}' --from 1h

QUERY        avg:system.cpu.user{host:web-01}
FROM         1700000000
TO           1700003600
SERIES       1
  scope      host:web-01
  points     58
  unit       percent
```

Explicit window using RFC 3339:

```bash
$ omni-dev datadog metrics query \
    --query 'sum:requests.total{env:prod}.as_rate()' \
    --from 2026-04-22T09:00:00Z --to 2026-04-22T10:00:00Z -o yaml

query: sum:requests.total{env:prod}.as_rate()
from: 1745312400
to: 1745316000
series:
  - scope: env:prod
    expression: sum:requests.total{env:prod}.as_rate()
    unit: request/s
    points: 60
```

Query-string syntax is Datadog's, not omni-dev's — see Datadog's
[metrics query reference]. The common shape is:

```
<aggregator>:<metric>{<scope tags>}[.<function>(...)]
```

where `<aggregator>` is `avg`, `sum`, `min`, `max`, or `count`, and
`<function>` adds rollups (`.rollup(avg, 60)`), arithmetic
(`.as_rate()`, `.as_count()`), or chained transforms.

[metrics query reference]: https://docs.datadoghq.com/dashboards/functions/

### List metric names

```bash
$ omni-dev datadog metrics catalog list --host web-01

system.cpu.user
system.cpu.system
system.cpu.idle
system.disk.in_use
system.mem.used
...
```

Filter by ingestion cutoff (epoch seconds — relative shorthand is not
accepted here):

```bash
$ omni-dev datadog metrics catalog list --from 1700000000
```

### MCP equivalents

- `datadog_metrics_query` — same `query`, `from`, `to` parameters.
- `datadog_metrics_catalog_list` — same `host`, `from` parameters.

## Monitors

Three subcommands:

- `monitor list` — server-side filter by name/tags.
- `monitor search` — Datadog's faceted-search query language.
- `monitor get` — fetch one by id (positional argument, integer).

### List with filters

```bash
$ omni-dev datadog monitor list --tags env:prod --limit 50

ID         NAME                            STATE   TYPE     TAGS
12345      API latency p99                 OK      metric   env:prod,team:platform
12389      Worker queue depth              ALERT   metric   env:prod,team:platform
67890      Error rate > 5%                 OK      metric   env:prod,team:checkout
```

Substring match on monitor name, with separate filters for resource tags vs
monitor metadata tags:

```bash
$ omni-dev datadog monitor list \
    --name 'API latency' --monitor-tags team:platform
```

**Note the two `--tags` flags:** `--tags` filters by the resource tags that
the monitor *tracks* (e.g. tags on the metric being alerted on);
`--monitor-tags` filters by user-applied tags on the monitor itself. This
mirrors Datadog's distinction between `tags` and `monitor_tags` query
parameters.

`--limit` defaults to 100; pass `0` to fetch every match across pages
(capped at 10,000 monitors per invocation).

### Faceted search

```bash
$ omni-dev datadog monitor search --query 'status:alert AND env:prod'

ID         NAME                            STATE   TYPE     TAGS
12389      Worker queue depth              ALERT   metric   env:prod,team:platform
77001      Checkout latency p99            ALERT   metric   env:prod,team:checkout
```

Datadog's monitor-search DSL supports faceted keys (`status:`, `type:`,
`tag:`, `env:`, `service:`) combined with `AND`/`OR`/`NOT`. See Datadog's
[monitor search docs] for the full grammar.

[monitor search docs]: https://docs.datadoghq.com/monitors/manage/search/

### Get a single monitor

```bash
$ omni-dev datadog monitor get 12345 -o yaml

id: 12345
name: API latency p99
type: metric alert
query: 'avg(last_5m):avg:trace.http.request.duration{service:api,env:prod} > 1.5'
message: |
  p99 latency on api in prod has exceeded 1.5s.
  Owner: @platform
overall_state: OK
tags:
  - env:prod
  - team:platform
options:
  thresholds:
    critical: 1.5
    warning: 1.0
```

The ID is **positional and numeric**.

### MCP equivalents

- `datadog_monitor_list`
- `datadog_monitor_search`
- `datadog_monitor_get`

## Dashboards

### List dashboards

```bash
$ omni-dev datadog dashboard list

ID            TITLE                           TYPE              LAYOUT_TYPE  AUTHOR
abc-123-xyz   Platform overview               custom_timeboard  ordered      alice@example.com
def-456-uvw   API service health              custom_timeboard  free         bob@example.com
ghi-789-rst   Database replication            custom_timeboard  ordered      carol@example.com
```

```bash
$ omni-dev datadog dashboard list --filter-shared
```

`--filter-shared` restricts results to dashboards explicitly shared with the
wider organisation.

### Pull a dashboard's full definition

```bash
$ omni-dev datadog dashboard get abc-123-xyz -o json | \
    jq '.widgets | length'
12
```

The `widgets` array is preserved as raw JSON because per-widget schemas are
heterogeneous (each visualisation type — `timeseries`, `query_value`,
`toplist`, etc. — has its own fields). Inspect a single widget:

```bash
$ omni-dev datadog dashboard get abc-123-xyz -o json | \
    jq '.widgets[0].definition'
{
  "type": "timeseries",
  "requests": [
    {
      "q": "avg:system.cpu.user{host:web-01}",
      "display_type": "line"
    }
  ],
  "title": "CPU on web-01"
}
```

The ID is **positional and a string**.

### MCP equivalents

- `datadog_dashboard_list`
- `datadog_dashboard_get`

## Logs

`logs search` uses Datadog's v2 Logs API
(`POST /api/v2/logs/events/search`). Defaults: `--from 15m`, `--to now`,
`--limit 100`, `--sort timestamp-desc`.

### Error logs from a service

```bash
$ omni-dev datadog logs search --filter 'service:api status:error'

TIMESTAMP                 HOST      SERVICE  STATUS  MESSAGE
2026-05-11T14:02:31.812Z  web-04    api      error   500 Internal Server Error /v1/checkout
2026-05-11T14:02:30.118Z  web-02    api      error   500 Internal Server Error /v1/checkout
2026-05-11T14:01:55.044Z  web-04    api      error   connection reset by peer
...
```

### HTTP 5xx, oldest first, paginate up to the cap

```bash
$ omni-dev datadog logs search \
    --filter '@http.status_code:5*' \
    --from 1h --limit 0 --sort timestamp-asc -o jsonl | \
    jq -c '{ts: .attributes.timestamp, msg: .attributes.message}'
{"ts":"2026-05-11T13:02:11.000Z","msg":"503 Service Unavailable upstream"}
{"ts":"2026-05-11T13:02:12.000Z","msg":"504 Gateway Timeout upstream"}
...
```

`--limit 0` fetches every match across pages with a hard cap of 10,000
events. Any non-zero value caps the total at that count, paginating
underneath as needed.

The query DSL is Datadog's — see [Logs Search Syntax]. Common facets:

- `service:`, `host:`, `status:`, `env:` — Datadog reserved facets.
- `@attribute.path` — arbitrary JSON attributes (note the `@` prefix).
- Wildcards (`5*`), boolean operators (`AND`/`OR`/`NOT`), and grouping (`()`)
  are supported.

[Logs Search Syntax]: https://docs.datadoghq.com/logs/explorer/search_syntax/

### MCP equivalent

- `datadog_logs_search` — same parameters, same 10,000-event cap.

## Events

`events list` reads Datadog's v2 events feed (`GET /api/v2/events`).
Defaults: `--from 1h`, `--to now`, `--limit 100`.

### Deployment events for a service

```bash
$ omni-dev datadog events list \
    --filter 'service:api' \
    --sources kubernetes,aws \
    --tags env:prod \
    --from 24h

TIMESTAMP              SOURCE       TITLE                              TAGS
2026-05-11T13:42:01Z   kubernetes   Deployment api rolled out 2.18.0   env:prod,service:api
2026-05-11T11:08:55Z   kubernetes   Pod api-7d4-xq2 evicted (memory)   env:prod,service:api
2026-05-10T22:14:32Z   aws          ELB target deregistered            env:prod,service:api
```

`--sources` is comma-separated, matches Datadog event sources (e.g.
`aws`, `kubernetes`, `github`, `terraform`).

`--limit 0` fetches every match across pages (hard cap 10,000).

### MCP equivalent

- `datadog_events_list`

## SLOs

Two subcommands:

- `slo list` — filter by tags / free-text / explicit IDs / metrics
  referenced.
- `slo get` — fetch one by id.

**`slo list --limit` defaults to 50**, unlike the other 100-default families.

### List with filters

```bash
$ omni-dev datadog slo list --tags team:platform

ID                NAME                          TYPE     TARGET  STATUS
abc123def456      Checkout API availability     metric   99.9    OK
ghi789jkl012      Search latency p99 < 200ms    metric   99.5    BREACH
mno345pqr678      Order pipeline success rate   metric   99.95   OK
```

Combine filters (Datadog AND-combines them server-side):

```bash
$ omni-dev datadog slo list --query 'checkout' --metrics-query 'requests'
$ omni-dev datadog slo list --ids abc123def456,ghi789jkl012
```

### "Near burn-rate threshold" — client-side filtering

Datadog's SLO list endpoint does not filter by burn-rate server-side. List
the SLOs of interest and filter client-side on the JSON output:

```bash
$ omni-dev datadog slo list --tags team:platform -o json | \
    jq '.[] | select(.overall_status[]?.error_budget_remaining < 25)
        | {id, name, remaining: .overall_status[]?.error_budget_remaining}'
{
  "id": "ghi789jkl012",
  "name": "Search latency p99 < 200ms",
  "remaining": 18.3
}
```

### Get a single SLO

```bash
$ omni-dev datadog slo get abc123def456 -o yaml

id: abc123def456
name: Checkout API availability
type: metric
target: 99.9
timeframe: 30d
query:
  numerator: sum:trace.http.request.hits{service:api,!http.status_code:5*}.as_count()
  denominator: sum:trace.http.request.hits{service:api}.as_count()
overall_status:
  - timeframe: 30d
    status: ok
    error_budget_remaining: 67.4
```

The ID is **positional and a string**.

### MCP equivalents

- `datadog_slo_list`
- `datadog_slo_get`

## Downtimes

### List scheduled downtimes

```bash
$ omni-dev datadog downtime list

ID         SCOPE                          START                  END                    ACTIVE  MESSAGE
778899     env:staging                    2026-05-10T20:00:00Z   2026-05-15T08:00:00Z   true    Staging upgrade window
778900     host:db-replica-3              2026-05-12T02:00:00Z   2026-05-12T04:00:00Z   false   Disk replacement
```

```bash
$ omni-dev datadog downtime list --active-only

ID         SCOPE                          START                  END                    ACTIVE  MESSAGE
778899     env:staging                    2026-05-10T20:00:00Z   2026-05-15T08:00:00Z   true    Staging upgrade window
```

### MCP equivalent

- `datadog_downtime_list`

## Hosts

### List hosts by tag filter

```bash
$ omni-dev datadog hosts list --filter env:prod --limit 200

NAME           AKA                  LAST_REPORT_TS    UP    TAGS
web-01.prod    i-0abc123            2026-05-11T14:03  true  env:prod,role:web
web-02.prod    i-0def456            2026-05-11T14:03  true  env:prod,role:web
db-01.prod     i-0fed789            2026-05-11T14:02  true  env:prod,role:db
worker-01      i-09876ab            2026-05-11T14:03  true  env:prod,role:worker
```

Cutoff hosts that haven't reported recently (epoch seconds — no relative
shorthand):

```bash
$ omni-dev datadog hosts list --from 1747000000
```

Hosts whose `last_reported_time` is older than `--from` are excluded.

### MCP equivalent

- `datadog_hosts_list`

## Rate limits and retry behaviour

Datadog enforces per-endpoint rate limits and signals exhaustion with
HTTP 429. omni-dev's client retries 429 responses automatically:

- Up to **3 retries** per request (4 attempts total).
  See [src/datadog/client.rs:19](../src/datadog/client.rs#L19) (`MAX_RETRIES`).
- Backoff delay, in order of preference:
  1. `Retry-After` response header.
  2. `X-RateLimit-Reset` response header.
  3. Exponential fallback `DEFAULT_RETRY_DELAY_SECS ^ (attempt + 1)`
     (base = 2s).
- Each retry logs to stderr: `Rate limited (429). Retrying in {N}s
  (attempt {K})...`

When retries are exhausted the surfaced error includes a rate-limit summary
parsed from `X-RateLimit-*` headers:

```
Error: Datadog API request failed: HTTP 429: <body> [rate-limit: remaining=0, limit=300, reset_in=42s]
```

### Pagination caps

The list endpoints (logs, events, hosts, monitors, SLOs) auto-paginate when
`--limit 0` is passed, capped at **10,000 records** per invocation
(`HARD_CAP` in each `*_api.rs` file). Any non-zero `--limit` is upper-
bounded by the same cap. Set `--limit` to the actual ceiling you need —
fetching all 10,000 records issues up to 100 page requests under the hood
and is the easiest way to trip rate-limiting.

## Troubleshooting

### Credentials not configured

```
Error: Datadog credentials not configured. Run `omni-dev datadog auth login`
```

Means **either** `DATADOG_API_KEY` **or** `DATADOG_APP_KEY` is missing from
the environment **and** the settings file. Both are required.

Run `omni-dev datadog auth status` — or for the MCP version,
`datadog_auth_status` — to see boolean presence flags for each credential
without exposing values.

### HTTP 403: Forbidden — bad API key

```
Error: Datadog API request failed: HTTP 403: {"errors":["Forbidden"]}
```

Most common causes:

- `DATADOG_API_KEY` is revoked, mistyped, or for a different Datadog org.
- The API key is regional and the configured site does not match. Datadog
  keys created in EU1 do not work against `datadoghq.com`. Set
  `DATADOG_SITE=datadoghq.eu` (or re-run `omni-dev datadog auth login`).

### HTTP 403/401 — missing application key

```
Error: Datadog API request failed: HTTP 403: ...
```

If the API key is fine but reads still fail, check `DATADOG_APP_KEY`. Most
read endpoints exposed here require **both** keys; an API-key-only setup
will pass `/api/v1/validate` (via `auth status`) but fail on `metric`,
`monitor`, `dashboard`, etc.

### Wrong site (404 or 403)

A site mismatch typically surfaces as 403 (wrong-region key) or 404
(endpoint not present in that region). Verify with:

```bash
$ omni-dev datadog auth status
Checking Datadog authentication for site 'datadoghq.com'...
...
```

The first line echoes the site being used; if it doesn't match where the
key was created, fix with:

```bash
$ omni-dev datadog auth login            # interactive, updates settings
# or
$ DATADOG_SITE=datadoghq.eu omni-dev datadog auth status   # one-off
```

### Rate-limited, retries exhausted

```
Error: Datadog API request failed: HTTP 429: <body> [rate-limit: remaining=0, limit=300, reset_in=42s]
```

The `reset_in` value is the number of seconds before the bucket refills.
Either wait that long, narrow your time range / filter to make fewer calls,
or pass a smaller `--limit` to avoid auto-pagination.

### Invalid time range

```
Error: Invalid time range: 1h30m
```

Compound shorthand (`1h30m`, `2d3h`) is rejected. Use a single unit
(`90m`, `51h`), an RFC 3339 timestamp, or Unix epoch seconds. See
[Time-range syntax](#time-range-syntax).

```
Error: Invalid time range: to (1700000000) is before from (1700003600)
```

`--to` resolves to an earlier instant than `--from`. Swap them or extend
the window.

### Unknown site warning

```
warning: Datadog site 'foo.example' is not a known region; proceeding anyway
```

Not an error — printed on stderr when the site isn't in omni-dev's
hard-coded list (Datadog adds regions occasionally). For new official
regions it is harmless. For on-prem or proxied Datadog, set
`DATADOG_API_URL` instead and clear `DATADOG_SITE` to silence the warning.

### MCP server cannot see credentials

When running via an MCP client (Claude Desktop / Claude Code), environment
variables exported in your interactive shell are **not** inherited unless
the client launched the MCP server from that same shell. The reliable fix:

```bash
$ omni-dev datadog auth login
```

This persists credentials to `~/.omni-dev/settings.json`, which is read by
every invocation regardless of how the process was started.

## See also

- [User Guide](user-guide.md#datadog-integration) — short reference;
  primary content lives here.
- [MCP Reference — Datadog](mcp.md#datadog-14-tools) — parameter-only listing
  of all 14 `datadog_*` MCP tools.
- [Datadog API documentation](https://docs.datadoghq.com/api/latest/) — upstream
  reference for query DSLs, endpoint behaviour, and rate-limit policy.
