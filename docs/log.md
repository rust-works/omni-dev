# Request log

`omni-dev` keeps a local, append-only log of every invocation **and** the HTTP
requests it issues, and ships an `omni-dev log` subcommand to search and
pretty-print it. It is the single, durable, queryable record of *what was run*
and *what it talked to over the network* — the thing `RUST_LOG` tracing (ephemeral,
stderr-only) is not.

## What gets recorded

One JSON object per line (NDJSON). A `kind` field discriminates the two record
types, so the log is a complete invocation history, not just an HTTP history:

- **`kind: "invocation"`** — one per process run (and one per MCP tool call):
  the resolved subcommand path, full argv (with secret-bearing flag values
  redacted), exit code, duration, any top-level error, and a whitelisted
  `OMNI_DEV_*` env snapshot.
- **`kind: "http"`** — one per outbound request (recorded *inside* each client's
  retry loop, so retries and transport failures are captured too): service,
  method, URL (secret-bearing query/fragment parameter **values** redacted —
  see [Redaction posture](#redaction-posture)), status, elapsed, and any error.

Every HTTP record shares an `invocation_id` with the invocation that issued it,
so you can pull a run and all of its requests with a single `--id`.

## Location

Resolved in this order:

1. `OMNI_DEV_LOG_FILE` if set.
2. Otherwise `dirs::state_dir()` joined with `omni-dev/log.jsonl` — on Linux,
   `~/.local/state/omni-dev/log.jsonl`.
3. On platforms without a state dir (macOS), it falls back to the data dir,
   matching the daemon's convention: `~/Library/Application Support/omni-dev/log.jsonl`.

The directory is created `0700` and the file `0600`, the same posture as other
`omni-dev` runtime state. The log lives entirely on your machine.

## Environment variables

| Variable | Effect |
|----------|--------|
| `OMNI_DEV_LOG_FILE` | Override the log path. |
| `OMNI_DEV_LOG_DISABLE=1` | Disable logging entirely. |
| `OMNI_DEV_LOG_BODIES=1` | Opt in to recording request/response bodies (off by default; payloads are large and may contain customer content). |
| `OMNI_DEV_LOG_HEADERS=1` | Opt in to recording (redacted) request/response headers. |

Logging is **best effort**: a write failure is swallowed (logged only at
`tracing::debug`) and can never change the command's exit code.

## `omni-dev log`

```
omni-dev log [OPTIONS]
```

### Filters

| Flag | Matches |
|------|---------|
| `--since <DUR>` | Records newer than a relative window: `30m`, `2h`, `1d`, `1w`, `45s`. |
| `--method <METHOD>` | HTTP method (case-insensitive). |
| `--status <STATUS>` | Exact (`200`), class (`5xx`), or comma list (`4xx,5xx`). |
| `--service <NAME>` | `jira`, `confluence`, `datadog`, `browser-bridge`, `snowflake`, `claude`, `claude-cli`. |
| `--command <PATH>` | Resolved command-path prefix on whole segments, e.g. `"jira read"`. |
| `--url <SUBSTR>` | Substring of the request URL. |
| `--grep <REGEX>` | Regular expression against the raw JSON line. |
| `--fuzzy <TOKEN>` | Substring of the raw line; repeatable, AND-ed. |
| `--query <EXPR>` | Query expression (see below); repeatable, AND-ed. |
| `--id <ID>` | This record `id` **or** `invocation_id` — pulls a run and its requests. |

### Output

| Flag | Effect |
|------|--------|
| `--format <oneline\|json\|full>` | `oneline` (default), `json` (NDJSON, byte-identical to the file — composes with `jq`), or `full` (pretty block). |
| `-n, --limit <N>` | Show at most the N most recent matching records. |
| `-f, --follow` | Tail the log, printing new matching records as they arrive. |

### The `--query` mini-language

- **Structured terms:** `field:value` — `kind`, `source`, `service`, `method`,
  `status` (supports `5xx`), `command`, `url`, `id`, `invocation_id` (alias
  `inv`), `mcp_tool` (alias `tool`), `via_daemon`, `error`. Field matching is
  shared with the flags, so `--status 5xx` and `status:5xx` behave identically.
- **Bare tokens** are fuzzy substring matches against the raw JSON line.
- **Operators:** `AND` (also implicit between adjacent terms), `OR`, `NOT` (or a
  leading `-`), and parentheses. Use `"quotes"` for a value containing spaces.

```bash
omni-dev log --query 'kind:http AND (status:5xx OR method:POST)'
omni-dev log --query 'service:jira -status:2xx'        # jira requests that did not 2xx
```

### Examples

```bash
# The last 20 things you ran.
omni-dev log -n 20

# Server errors in the last two hours.
omni-dev log --since 2h --status 5xx

# A run and every request it made.
omni-dev log --id 0001718000000-0a1b2c3d4e5f6071

# Compose with jq.
omni-dev log --format json --service datadog | jq 'select(.elapsed_ms > 1000)'

# Follow live.
omni-dev log -f --service browser-bridge
```

## Redaction posture

No secret material is ever written, under any code path:

- Auth headers/tokens are **redacted centrally** before writing; only a
  non-secret `auth_principal` identity is ever kept. Redaction matches both a
  fixed list of known header names and any name containing `auth`, `token`,
  `key`, `secret`, `cookie`, `password`, `session`, `signature`, or
  `credential` (case-insensitive).
- URL query and fragment parameters whose keys look secret-bearing have their
  **values** replaced with `REDACTED` before writing: keys suffixed `token`,
  `secret`, `password`, `passwd`, `signature`, or `api_key`/`apikey`; the exact
  keys `sig`, `sas`, `jwt`, and `auth`; and the `X-Amz-*` / `X-Goog-*`
  signed-URL families. Host, path, and parameter keys are preserved, so
  `--url` substring filtering keeps working. This matters mostly for the
  browser bridge, which logs arbitrary operator-supplied target URLs
  (presigned URLs, `?access_token=…`).
- Request/response bodies are **opt-in** via `OMNI_DEV_LOG_BODIES=1`.
- The `OMNI_DEV_*` env snapshot redacts any name containing `TOKEN`, `SECRET`,
  `KEY`, `PASSWORD`, or `PASSWD`.
- Argv in the invocation record is scrubbed before writing, in both
  `--flag value` and `--flag=value` forms: `--header` values naming a sensitive
  header are redacted keeping the name (`Authorization: REDACTED`), inline
  `--body` values are redacted (`@file` references are kept), and any flag
  whose name has a `token`/`secret`/`password`/`passwd`/`key` segment has its
  value redacted (flags ending in `-file`/`-path` carry paths and are exempt).
  Every argv element is then run through the same URL query/fragment redaction
  as `--url` above, so a secret carried in a URL argument (e.g.
  `--url /path?access_token=…` or a presigned target) is redacted even though
  `--url` is not a secret-bearing flag name; benign argv passes through
  byte-identical.

### What redaction does not cover: prompt bodies

The guarantees above keep *secret material* out of the log; they do not make
prompt **content** secret. AI prompts carry whatever you asked about — repo
diffs, commit messages, JIRA/Confluence data — and that content surfaces on
three paths:

- `OMNI_DEV_LOG_BODIES=1` records AI request/response bodies, which **are** the
  prompts.
- `RUST_LOG=debug` stderr tracing emits the **full system/user prompt and full
  response** from every AI backend. Tracing is ephemeral and stderr-only, but
  if you redirect it to a file or paste it into a bug report, prompt bodies go
  with it.
- On a `claude-cli` subprocess failure, the returned error embeds the child's
  **stdout/stderr verbatim**, which can carry prompt-derived content into
  whatever captures the error (terminal, CI logs, this log's `error` field).

No API keys or tokens appear on any of these paths — this is a
data-sensitivity note, not a credential leak.

## Daemon-served requests

Requests executed inside the daemon (the browser bridge and the Snowflake
session pool) set `via_daemon: true`. Because the daemon is a separate process
from the CLI that asked it to act, such requests carry the *daemon's*
`invocation_id`, not the CLI's — cross-socket correlation is a planned follow-up.

## Schema and compatibility

Records are read and written through a single forward-compatible struct: every
field is `#[serde(default)]` and every optional field is `skip_serializing_if`.
A newer reader never chokes on an older line, and an older reader never chokes on
a newer one — the same forward-rolling contract the daemon wire types use. The
record id is time-sortable, so sorting lines by `id` ≈ sorting by time.
