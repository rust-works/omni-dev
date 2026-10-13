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
- **`kind: "gh"`** — one per `gh` CLI subprocess invocation. Every GitHub call
  funnels through `gh` (the token never enters our process), so these are
  subprocess records, not `http` ones. Each carries the semantic subcommand
  (`api graphql`, `pr list`, `repo view`, …) in `command`, the scrubbed argv in
  `command_line`, the `source`, the exit code, and the duration. This is what
  `omni-dev log count --kind gh` aggregates — see [Counting GitHub API calls](#counting-github-api-calls).
- **`kind: "worktree"`** — one per `omni-dev git worktree <verb>` invocation
  (thin logged wrappers over `git worktree add/remove/list/move/prune/repair`,
  #1392). Tagged `service: "worktree"`, with `command` set to
  `["git", "worktree", "<verb>"]` and the recovery-relevant metadata in the
  free-form `context` map — for `remove`: the worktree's `path`, `branch`,
  `commit` (HEAD *before* removal), `had_uncommitted`, and `used_force`; for
  `prune`: a JSON `pruned` list of `{path, branch, commit}`. See
  [Recovering a removed worktree](#recovering-a-removed-worktree).
- **`kind: "drivemutation"`** — **legacy, read-only.** Older omni-dev releases wrote
  one per Drive, Docs, Sheets or Slides mutation attempt (tagged `service: "drive"`,
  `command` `["drive", "<operation>"]`, the outcome detail in the free-form `context`
  map). The Drive commands moved to [gwi](https://github.com/rust-works/gwi) in
  [#2203](https://github.com/rust-works/omni-dev/issues/2203) /
  [ADR-0095](adrs/adr-0095.md), so omni-dev no longer writes this kind. `omni-dev log`
  still decodes and displays existing records, and every filter and `--query` term
  keeps working over them (`omni-dev log --query kind:drivemutation`). The per-field
  documentation for these records is maintained in
  [gwi's `docs/log.md`](https://github.com/rust-works/gwi/blob/main/docs/log.md); gwi writes the same shape to its own log.

- **`kind: "audit"`** — **legacy, read-only.** Written by older releases to the
  **separate** `audit.jsonl` sink, never to this file; see [Audit log](#audit-log).

Every HTTP, `gh` and `worktree` record shares an
`invocation_id` with the invocation that issued it, so you can pull a run and
all of its requests with a single `--id`.

**Coverage caveat:** `omni-dev`'s own in-process HTTP clients and its Rust `gh`
calls (daemon *and* one-shot CLI) are recorded. The one gap left is the VS Code
companion extension's `gh pr list`, which runs in a **separate Node process** and
does not write to this log.

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

| Variable                  | Effect                                                                                                                              |
|---------------------------|-------------------------------------------------------------------------------------------------------------------------------------|
| `OMNI_DEV_LOG_FILE`       | Override the log path.                                                                                                              |
| `OMNI_DEV_LOG_DISABLE=1`  | Disable logging entirely.                                                                                                           |
| `OMNI_DEV_LOG_BODIES=1`   | Opt in to recording request/response bodies (off by default; payloads are large and may contain customer content).                  |
| `OMNI_DEV_LOG_HEADERS=1`  | Opt in to recording (redacted) request/response headers.                                                                            |
| `OMNI_DEV_LOG_MAX_SIZE`   | Enable automatic size-capped rotation on write, e.g. `10mb` (unix only; see [Bounding growth](#bounding-growth)).                   |
| `OMNI_DEV_LOG_KEEP_FILES` | Number of rotated files to keep when rotation is enabled (default `3`).                                                             |
| `OMNI_DEV_AUDIT_LOG_FILE` | Override the path `omni-dev log --audit` reads — see [Audit log](#audit-log) (legacy); none of the other variables above affect it. |

Logging is **best effort**: a write failure is swallowed (logged only at
`tracing::debug`) and can never change the command's exit code. (The legacy
audit log was the one fail-closed exception; omni-dev no longer writes it — see
[Audit log](#audit-log).)

## `omni-dev log`

```
omni-dev log [OPTIONS]          # search (default)
omni-dev log prune [OPTIONS]    # trim the log — see Bounding growth
```

With no subcommand, `omni-dev log` searches and prints the log using the filters
below. The `prune` subcommand trims it; see [Bounding growth](#bounding-growth).

### Filters

| Flag | Matches |
|------|---------|
| `--since <DUR_OR_TS>` | Lower time bound: a relative window (`30m`, `2h`, `1d`, `1w`, `45s`), a date (`2026-07-01`), or an RFC3339 timestamp. |
| `--until <DUR_OR_TS>` | Upper time bound: same forms as `--since` (a relative value means that long ago). Pair with `--since` for a bounded window. |
| `--method <METHOD>` | HTTP method (case-insensitive). |
| `--status <STATUS>` | Exact (`200`), class (`5xx`), or comma list (`4xx,5xx`). |
| `--service <NAME>` | `jira`, `confluence`, `datadog`, `browser-bridge`, `snowflake`, `transcript`, `anthropic`, `bedrock`, `openai`, `ollama`, `claude-cli`. |
| `--command <PATH>` | Resolved command-path prefix on whole segments, e.g. `"jira read"`. |
| `--url <SUBSTR>` | Substring of the request URL. |
| `--grep <REGEX>` | Regular expression against the raw JSON line. |
| `--fuzzy <TOKEN>` | Substring of the raw line; repeatable, AND-ed. |
| `--query <EXPR>` | Query expression (see below); repeatable, AND-ed. |
| `--id <ID>` | This record `id` **or** `invocation_id` — pulls a run and its requests. |

### Output

| Flag | Effect |
|------|--------|
| `-o, --output <oneline\|json\|full>` | `oneline` (default), `json` (NDJSON, byte-identical to the file — composes with `jq`), or `full` (pretty block). |
| `-n, --limit <N>` | Show at most the N most recent matching records. |
| `-f, --follow` | Tail the log, printing new matching records as they arrive. |
| `--audit` | Read `audit.jsonl` instead of `log.jsonl` — see [Audit log](#audit-log). Every filter, the `--query` mini-language, and all three output formats apply unchanged; only the file being read differs. |

### The `--query` mini-language

- **Structured terms:** `field:value` — `kind`, `source`, `service`, `method`,
  `status` (supports `5xx`), `command`, `url`, `id`, `invocation_id` (alias
  `inv`), `mcp_tool` (alias `tool`), `via_daemon`, `error`, plus the fields that
  were previously reachable only through `--grep`: `exit_code` (alias `exit`),
  `duration_ms` (aliases `duration`, `dur`), `elapsed_ms` (alias `elapsed`),
  `hostname` (alias `host`), `system_user` (alias `user`), `cwd`, and
  `auth_principal` (alias `principal`). Field matching is shared with the flags,
  so `--status 5xx` and `status:5xx` behave identically.
- **`status` is kind-aware:** for every kind but the legacy `drivemutation` it
  matches the HTTP `status_code` (class or comparator syntax, below). A
  `drivemutation` record has no `status_code` — its domain status (`blocked`, `written`,
  `stale-revision`, ...) lives in the `context` map instead, so `status:` there
  matches `context["status"]` by exact case-insensitive equality, e.g.
  `kind:drivemutation status:blocked`.
- **Numeric comparisons:** numeric fields (`exit_code`, `duration_ms`,
  `elapsed_ms`, and `status`) accept a leading comparator — `>`, `>=`, `<`,
  `<=`, or bare `=`/`N` for equality — so slow requests and failed runs are
  expressible directly (`elapsed:>1000`, `exit_code:>0`). `status` still also
  accepts its class syntax (`5xx`). Comparator/class syntax applies only to the
  HTTP `status_code` path, not the `drivemutation` case above.
- **Text fields** (`url`, `hostname`, `system_user`, `cwd`, `auth_principal`)
  match a case-insensitive substring.
- **Context fields:** any other field name falls back to the record's free-form
  `context` map (case-insensitive substring), so worktree recovery fields query
  directly: `branch:issue-1392`, `path:demo-wt`.
- **Bare tokens** are fuzzy substring matches against the raw JSON line.
- **Operators:** `AND` (also implicit between adjacent terms), `OR`, `NOT` (or a
  leading `-`), and parentheses. Use `"quotes"` for a value containing spaces.

```bash
omni-dev log --query 'kind:http AND (status:5xx OR method:POST)'
omni-dev log --query 'service:jira -status:2xx'        # jira requests that did not 2xx
omni-dev log --query 'elapsed:>1000'                   # requests slower than 1s
omni-dev log --query 'kind:invocation exit_code:>0'    # failed runs
```

### Examples

```bash
# The last 20 things you ran.
omni-dev log -n 20

# Server errors in the last two hours.
omni-dev log --since 2h --status 5xx

# Slow datadog requests (native — no jq needed).
omni-dev log --service datadog --query 'elapsed:>1000'

# A bounded historical window.
omni-dev log --since 2026-07-01 --until 2026-07-02

# A run and every request it made.
omni-dev log --id 0001718000000-0a1b2c3d4e5f6071

# Compose with jq for anything not directly expressible.
omni-dev log -o json --service datadog | jq 'select(.status_code == 429)'

# Follow live.
omni-dev log -f --service browser-bridge
```

### Recovering a removed worktree

`omni-dev git worktree remove` records the worktree's branch, HEAD commit, and
dirtiness *before* the removal, so an accidental remove is recoverable minutes
later from the log alone:

```bash
# What worktree did I just remove?
omni-dev log --command 'git worktree remove' --since 1d -o full

# Or by any recovery field directly.
omni-dev log --service worktree --since 30m
omni-dev log --query 'branch:issue-1392'
```

The `full` record's `context` carries `path`, `branch`, `commit`,
`had_uncommitted`, and `used_force` — enough to re-create the branch
(`git branch <branch> <commit>`) and re-attach a worktree
(`omni-dev git worktree add <path> <branch>`). `prune` records carry the same
triple per pruned entry in a JSON `pruned` list.

## Counting records

`omni-dev log count` aggregates the log by record **kind** and **source** over an
optional window — a quick tally without paging through matches:

```
omni-dev log count [--since <DUR_OR_TS>] [--until <DUR_OR_TS>] [--source <SOURCE>] [--kind <KIND>] [--json]
```

| Flag | Effect |
|------|--------|
| `--since <DUR_OR_TS>` | Lower time bound — same forms as `omni-dev log --since` (`1h`, `2026-07-01`, RFC3339). |
| `--until <DUR_OR_TS>` | Upper time bound (a relative value means that long ago). |
| `--source <SOURCE>` | Restrict to `cli`, `daemon`, or `mcp`. |
| `--kind <KIND>` | Restrict to `invocation`, `http`, `gh`, `worktree`, or `drivemutation`. `gh` unlocks the GitHub breakdown (below). |
| `--json` | Emit JSON (string map keys, composes with `jq`) instead of the table. |

With no `--kind` it reports the total and the split by kind and source:

```bash
# What has this machine done in the last day?
omni-dev log count --since 1d
```

### Counting GitHub API calls

Because every GitHub call is recorded as a `kind: "gh"` line (see [What gets
recorded](#what-gets-recorded)), `--kind gh` turns the log into a ground-truth
count of the GitHub API calls `omni-dev` itself makes — the *local* counterpart
to `gh api rate_limit`, which is GitHub's *server-side* view of everything the
token spent. It renders a richer breakdown by **category** — `api` (`gh api …`),
`subcommand` (`gh pr list`, `gh repo view`, …), and `local` (`gh --version`,
excluded from the API total) — as well as by subcommand and by source:

```bash
# All GitHub API calls in the last hour.
omni-dev log count --kind gh --since 1h

# Just what the daemon's pollers spent today, as JSON.
omni-dev log count --kind gh --since 1d --source daemon --json

# The raw records (composes with jq / anything log can express).
omni-dev log --query 'kind:gh'
```

The counts share the request log's storage, so they honor the same window and
are subject to `prune`/rotation. The daemon also logs a summary of these counters
to its own log (`daemon.log` / the journal) ~5 seconds after startup, every 10
minutes, and once on shutdown — a periodic, restart-delimited footprint without
anyone running a command; `daemon status` surfaces the current count too.

Those daemon figures count calls **since the daemon started**. It reads no part of
the log that was already there when it started, and each summary reads only the
records appended since the previous one, so what a summary costs does not grow with
the size of the log — a multi-gigabyte `log.jsonl` no longer delays startup,
`daemon status` or shutdown. A [size-capped rotation](#automatic-size-capped-rotation)
does not disturb the count: rotation only renames the log, so the daemon finds the
file it was reading as `log.jsonl.N` by its file id, counts what was appended to it
after the last summary, then reads every newer rotated file and the new `log.jsonl`
in order. Several rotations between two summaries are followed the same way, and so
is a rotation that has not yet been followed by a new `log.jsonl`. If the log is
instead pruned (`log prune` unlinks the file it replaces), truncated or rewritten, or
the file the daemon was reading has been rotated out of retention
(`OMNI_DEV_LOG_KEEP_FILES`), nothing next to the log is the file it was reading, and
it rebuilds its count once from what the live file holds, which is what `omni-dev
log count --kind gh --since <daemon start>` reports at that point. The same goes for
a daemon that had no position in the log to follow: one that started before any log
existed, or could not read the end of it. A summary that
shutdown cut short, or that could not read the log, is marked `incomplete`: in the
summary line, and as `"incomplete": true` in the `summary` op's reply.

`omni-dev log count` itself reads only the live `log.jsonl`, so a `--since` window
that reaches back past the last rotation counts only what the live file holds.

**Not yet counted:** the VS Code companion extension's `gh pr list` runs in a
separate process and does not write to the log (a planned follow-up).

## Bounding growth

The log is default-on for every invocation **and** every outbound request, so on
an active machine it grows steadily. Two opt-in bounds keep it in check; both are
off by default, so nothing changes unless you ask for it.

### `omni-dev log prune`

Trims the log in place, by age and/or by size:

```
omni-dev log prune [--older-than <DUR>] [--max-size <SIZE>] [--dry-run]
```

| Flag | Effect |
|------|--------|
| `--older-than <DUR>` | Remove records older than a relative window (`7d`, `24h`, `2w`, `45m`). |
| `--max-size <SIZE>` | After age pruning, drop the **oldest** records until the file is at most `<SIZE>` (`10mb`, `512kb`, or a bare byte count). |
| `--dry-run` | Report what would be removed without modifying the file. |

At least one of `--older-than` / `--max-size` is required. Sizes are binary
(`kb`/`mb`/`gb` = 1024-based). Records with a missing or unparseable timestamp
are kept (age pruning only removes records it can positively date as old), and
`--max-size` always keeps at least the single most recent record.

```bash
# Keep the last 30 days; preview first.
omni-dev log prune --older-than 30d --dry-run
omni-dev log prune --older-than 30d

# Cap the file at 20 MB, dropping the oldest records to fit.
omni-dev log prune --max-size 20mb
```

Pruning rewrites the file atomically (a same-directory temp file is renamed over
the original, preserving the `0600` mode), so a concurrent reader never sees a
half-written file. It is not locked against concurrent **writers**, though: a
record appended during the rewrite may be lost, and — because `prune` is itself
a logged invocation — pruning the active log appends one new record of its own.
For exact accounting, prune with `OMNI_DEV_LOG_DISABLE=1` or when the log is idle.

### Automatic size-capped rotation

Set `OMNI_DEV_LOG_MAX_SIZE` (e.g. `10mb`) to rotate on write: before an append
that would push the file past the cap, `log.jsonl` is renamed to `log.jsonl.1`
(shifting any existing `log.jsonl.1` → `.2`, and so on) and a fresh `log.jsonl`
is started. `OMNI_DEV_LOG_KEEP_FILES` (default `3`) bounds how many rotated files
are retained; the oldest beyond that is deleted. Total on-disk use is therefore
roughly `(OMNI_DEV_LOG_KEEP_FILES + 1) × OMNI_DEV_LOG_MAX_SIZE`.

Rotation is **unix-only** and **best effort**: when it is enabled, writers
serialize on a stable `log.jsonl.lock` file (created `0600`) for the
check-rotate-append sequence, and a rotation failure falls back to appending
without rotating rather than dropping the record. The env vars are read per
write, so they must be present in the environment of whatever writes the log
(your shell for CLI runs, or the daemon's environment for daemon-served
requests). A set-but-invalid `OMNI_DEV_LOG_MAX_SIZE` is ignored (logged at
`tracing::debug`) and leaves rotation off. The `omni-dev log` reader already
tolerates truncation and rotation, so `-f/--follow` keeps working across a
rotation (it restarts from the top of the fresh file). The daemon's GitHub-call
counters follow a rotation through the numbered files (see [Counting GitHub API
calls](#counting-github-api-calls)); the rotated files keep their file ids because
rotation only renames them.

## Audit log

`audit.jsonl` is a **separate** file, a sibling of `log.jsonl` under the same
runtime directory, reusing the same `LogRecord` schema and the same
`omni-dev log` reader — pass `--audit` to read it instead of `log.jsonl`; see
[Filters](#filters)/[Output](#output) above, all of which apply unchanged.

**It is legacy.** Its only writer was the Drive leased-write lifecycle
([ADR-0080](adrs/adr-0080.md)), which moved to
[gwi](https://github.com/rust-works/gwi) with the rest of Drive
([#2203](https://github.com/rust-works/omni-dev/issues/2203),
[ADR-0095](adrs/adr-0095.md)). omni-dev no longer writes `audit.jsonl`, and it never
deletes, truncates, rotates or prunes an existing one: a file left by an older
release stays exactly as it was and stays readable with `omni-dev log --audit`.
gwi writes its own `audit.jsonl` under its own state directory; records are not
merged, so history from before the move stays here. The fail-closed guarantee those
records were written under (an operation whose audit record could not be written did
not happen) is gwi's to keep and is documented in
[gwi's `docs/log.md`](https://github.com/rust-works/gwi/blob/main/docs/log.md).

What remains in omni-dev is the read side and the guards that keep the file out of
the request log's reach:

|                                  | `log.jsonl`                      | `audit.jsonl`                                                                                                                      |
|----------------------------------|----------------------------------|------------------------------------------------------------------------------------------------------------------------------------|
| Written by omni-dev              | Yes (best effort)                | No (legacy; read-only)                                                                                                             |
| `OMNI_DEV_LOG_DISABLE=1`         | Suppresses all writes            | No effect                                                                                                                          |
| `omni-dev log prune`             | Trims by age/size                | Refuses `--audit` outright (`log prune does not support --audit`), **and** refuses any resolved log path that names the audit file |
| `OMNI_DEV_LOG_MAX_SIZE` rotation | Applies                          | Never applies                                                                                                                      |
| Path override                    | `OMNI_DEV_LOG_FILE`              | `OMNI_DEV_AUDIT_LOG_FILE`                                                                                                          |
| Location when unset              | `<state dir>/omni-dev/log.jsonl` | `<state dir>/omni-dev/audit.jsonl`                                                                                                 |

`--audit` is a flag on the bare search form (`omni-dev log --audit ...`) and,
separately, on `prune` (`omni-dev log prune --audit`, always refused, above);
placed before a subcommand name instead — `omni-dev log --audit prune ...` —
it is refused rather than silently ignored, since a subcommand never
consults it. If `OMNI_DEV_LOG_FILE` and `OMNI_DEV_AUDIT_LOG_FILE` are
configured to resolve to the **same file**, the request-log path itself
simply fails to resolve — compared by file identity (the `same-file` crate
— inode on unix, a file handle on Windows — falling back to a normalized
path when one side doesn't exist yet), not merely by identical spelling, so
a `..` segment, a relative-vs-absolute spelling, or a symlink can't slip
past the check
([#1747](https://github.com/rust-works/omni-dev/issues/1747)). Neither a
best-effort append, rotation, nor `prune` can therefore reach `audit.jsonl`
through an aliasing `OMNI_DEV_LOG_FILE`.

Every record carries `kind: "audit"` and, in `context`, an `integration` key
(`"drive"` in every record omni-dev wrote), a `file_id`, and a `verdict`,
alongside the usual invocation/HTTP fields. Because the records are Drive
lease-lifecycle events (`lease-acquire`, `lease-restore`, `lease-release` and the
write-ahead/outcome pairs of leased writes), their verdict vocabulary is documented
with the lease feature in gwi, not here.

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
session pool) set `via_daemon: true`. Although the daemon is a separate process
from the CLI that asked it to act, such requests are still stamped with the
*originating* CLI invocation's `invocation_id`: the thin client threads its id
across to the daemon — on the control socket for Snowflake (an
`origin_invocation_id` envelope field) and on the `X-Omni-Bridge-Origin` header
for the bridge — and the daemon scopes it around the request it serves. So
`omni-dev log --id <cli-invocation>` surfaces the `via_daemon` requests that run
triggered, correlating both halves of one logical operation
([#1198](https://github.com/rust-works/omni-dev/issues/1198)).

The threaded id is non-secret (it is not a token), and the record's `source`
stays `daemon`, so a daemon-served request remains distinguishable by
`via_daemon` / `source` even while its `invocation_id` points at the caller. The
same mechanism correlates a standalone `bridge serve` (there `source` stays
`cli` and `via_daemon` is unset).

## Schema and compatibility

Records are read and written through a single forward-compatible struct: every
field is `#[serde(default)]` and every optional field is `skip_serializing_if`.
A newer reader never chokes on an older line, and an older reader never chokes on
a newer one — the same forward-rolling contract the daemon wire types use. The
record id is time-sortable, so sorting lines by `id` ≈ sorting by time.
