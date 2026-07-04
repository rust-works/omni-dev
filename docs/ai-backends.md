# AI Backends

omni-dev's AI features — `twiddle`, PR creation, JIRA/Confluence drafting,
`ai chat` — call out to a large language model through a pluggable
**backend**. Five backends are supported and selected at runtime by environment
variables or a CLI flag.

This guide covers what each backend is, how to wire it up, and how to choose
between them. For dev-facing notes on the dispatch implementation, see the
"AI Backend Dispatch" section of [CLAUDE.md](../CLAUDE.md).

## Table of Contents

1. [Backends at a Glance](#backends-at-a-glance)
2. [Dispatch Order and Model Selection](#dispatch-order-and-model-selection)
3. [Claude API (default)](#claude-api-default)
4. [Claude CLI (sandboxed subprocess)](#claude-cli-sandboxed-subprocess)
5. [OpenAI](#openai)
6. [Ollama](#ollama)
7. [AWS Bedrock](#aws-bedrock)
8. [Claude CLI Deep-dive](#claude-cli-deep-dive)
9. [Model Registry](#model-registry)
10. [Choosing a Backend](#choosing-a-backend)
11. [Troubleshooting](#troubleshooting)

## Backends at a Glance

| Backend       | Selector                                                                         | Required credentials                                                                       | Default model                | Best when                                                            |
|---------------|----------------------------------------------------------------------------------|--------------------------------------------------------------------------------------------|------------------------------|----------------------------------------------------------------------|
| Claude CLI    | `--ai-backend claude-cli` or `OMNI_DEV_AI_BACKEND=claude-cli`                    | An authenticated `claude` CLI session                                                      | `claude-sonnet-4-6`          | You already use Claude Code and want to reuse its auth and billing.  |
| Ollama        | `--ai-backend ollama` (legacy: `USE_OLLAMA=true`)                                | None (local server)                                                                        | `llama2`                     | Offline / local-only inference, experimenting with open models.      |
| OpenAI        | `--ai-backend openai` (legacy: `USE_OPENAI=true`)                                | `OPENAI_API_KEY` or `OPENAI_AUTH_TOKEN`                                                    | `gpt-5-mini` (registry)      | You're an OpenAI customer or want non-Claude models via OpenAI.      |
| AWS Bedrock   | `--ai-backend bedrock` (legacy: `CLAUDE_CODE_USE_BEDROCK=true`)                  | `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_BEDROCK_BASE_URL`                                       | `claude-sonnet-4-6`          | Your org runs Anthropic models through AWS Bedrock.                  |
| Claude API    | *(default — no flag)* or `--ai-backend default`                                  | `CLAUDE_API_KEY` or `ANTHROPIC_API_KEY` or `ANTHROPIC_AUTH_TOKEN`                          | `claude-sonnet-4-6`          | You have an Anthropic API key. Lowest-friction starting point.       |

## Dispatch Order and Model Selection

The global `--ai-backend` flag accepts `default`, `claude-cli`, `openai`,
`ollama`, and `bedrock`, and is equivalent to setting `OMNI_DEV_AI_BACKEND`
to the same value (the flag wins when both are set). When
`OMNI_DEV_AI_BACKEND` is set it decides the backend **outright** — including
`default`, which forces the direct Claude API even when the legacy `USE_*`
variables are set. An unknown value is a hard error listing the valid values.

When `OMNI_DEV_AI_BACKEND` is unset, the legacy selection flags apply in a
strict priority order — the first match wins:

| Priority | Selector (only when `OMNI_DEV_AI_BACKEND` is unset)    | Backend       |
|----------|---------------------------------------------------------|---------------|
| 1        | `USE_OLLAMA=true`                                       | Ollama        |
| 2        | `USE_OPENAI=true`                                       | OpenAI        |
| 3        | `CLAUDE_CODE_USE_BEDROCK=true`                          | AWS Bedrock   |
| 4        | *(none of the above)*                                   | Claude API    |

**Model resolution.** Every backend resolves the model through the same
precedence chain, stopping at the first non-empty value:

1. `--model <id>` (global CLI flag)
2. `OMNI_DEV_MODEL` (what `--model` propagates to; settable directly or via
   `~/.omni-dev/settings.json` env bundles / profiles)
3. The backend family's own variables:
   - Claude family (Claude API, Bedrock, Claude CLI):
     `CLAUDE_MODEL` → `CLAUDE_CODE_MODEL` → `ANTHROPIC_MODEL`
   - OpenAI: `OPENAI_MODEL`
   - Ollama: `OLLAMA_MODEL`
4. Registry default for the active provider (`claude-sonnet-4-6`,
   `gpt-5-mini`, or `llama2` for Ollama)

The Claude-family variables are deliberately scoped to Claude-family
backends: an exported `CLAUDE_MODEL` can never leak a Claude model id into
the OpenAI or Ollama backends.

**Beta headers.** The global `--beta-header key:value` flag (equivalent to
`OMNI_DEV_BETA_HEADER`) attaches a beta header to API requests when the
model registry lists it as supported. It is ignored (with a warning) by the
`claude-cli` backend.

See the [Model Registry](#model-registry) section below for how to list,
override, or extend the catalogue.

## Claude API (default)

The default backend calls the Anthropic Messages API over HTTPS. No flags
required. To force it when legacy `USE_*` variables are exported in your
environment, pass `--ai-backend default`.

**Credentials.** Set one of (checked in order):

```bash
export CLAUDE_API_KEY="sk-ant-..."
# or
export ANTHROPIC_API_KEY="sk-ant-..."
# or
export ANTHROPIC_AUTH_TOKEN="sk-ant-..."
```

Get a key from [console.anthropic.com](https://console.anthropic.com/).

**Model.** Resolved from the precedence chain above. The registry default is
`claude-sonnet-4-6`. Override per-invocation with `--model`:

```bash
omni-dev --model claude-opus-4-6 git commit message twiddle 'origin/main..HEAD' --use-context
```

**Verification.**

```bash
export CLAUDE_API_KEY="sk-ant-..."
omni-dev git commit message twiddle 'origin/main..HEAD' --use-context
```

## Claude CLI (sandboxed subprocess)

Routes AI calls through an already-authenticated
[Claude Code](https://github.com/anthropics/claude-code) session by shelling
out to `claude -p` in a locked-down sandbox. This avoids provisioning a
separate API key when you already have Claude Code installed and signed in.

**Selection.** Either flag or env var works; the flag wins if both are set:

```bash
omni-dev --ai-backend claude-cli git commit message twiddle 'origin/main..HEAD' --use-context
# or persistently:
export OMNI_DEV_AI_BACKEND=claude-cli
```

**Credentials.** None passed by omni-dev — the nested `claude -p` process
uses whatever auth your `claude` CLI is configured with. Run `claude` once
interactively first to confirm it works on its own.

**Model.** Resolved through the standard chain. Short aliases (`sonnet`,
`opus`, `haiku`) and full identifiers (`claude-sonnet-4-6`) are both accepted
and forwarded verbatim to `claude -p --model`. The `--beta-header` flag is
**ignored** for this backend (`claude`'s `--betas` flag has different
semantics).

**Sandbox.** By default the nested session has no tools, no MCP servers, no
filesystem access, and a scrubbed environment. The [Claude CLI
Deep-dive](#claude-cli-deep-dive) section below covers what's blocked, the
escape hatches, and the spending cap.

**Verification.**

```bash
claude --version          # confirm the CLI is installed and authenticated
omni-dev --ai-backend claude-cli git commit message twiddle 'origin/main..HEAD' --use-context
```

## OpenAI

Calls the OpenAI Chat Completions API.

**Selection.**

```bash
omni-dev --ai-backend openai ...
# or persistently:
export OMNI_DEV_AI_BACKEND=openai
# legacy (applies only when OMNI_DEV_AI_BACKEND is unset):
export USE_OPENAI=true
```

**Credentials.** Set one of:

```bash
export OPENAI_API_KEY="sk-..."
# or
export OPENAI_AUTH_TOKEN="sk-..."
```

**Model.** Registry default is `gpt-5-mini`. Override with `OPENAI_MODEL` or
`--model`:

```bash
export OPENAI_MODEL="gpt-5"
# or
omni-dev --model gpt-5 git commit message twiddle ...
```

**Endpoint.** Fixed to `https://api.openai.com/v1/chat/completions`. To point
at an OpenAI-compatible third-party service running on a custom endpoint,
use the [Ollama](#ollama) backend (which exposes `OLLAMA_BASE_URL`).

**Limitations.**

- omni-dev requests structured output via JSON Schema (`response_format:
  json_schema`). Older OpenAI models that don't support `json_schema` will
  fail.
- GPT-5 series models use `max_completion_tokens` rather than `max_tokens` —
  omni-dev handles this automatically based on the model identifier.

**Verification.**

```bash
export USE_OPENAI=true
export OPENAI_API_KEY="sk-..."
omni-dev git commit message twiddle 'origin/main..HEAD' --use-context
```

## Ollama

Calls a local [Ollama](https://ollama.ai/) (or any OpenAI-compatible) server
over HTTP. No API key required.

**Selection.**

```bash
omni-dev --ai-backend ollama ...
# or persistently:
export OMNI_DEV_AI_BACKEND=ollama
# legacy (applies only when OMNI_DEV_AI_BACKEND is unset):
export USE_OLLAMA=true
```

**Endpoint.** Defaults to `http://localhost:11434`. Override with
`OLLAMA_BASE_URL` to target a remote Ollama server or a compatible service
like [LM Studio](https://lmstudio.ai/):

```bash
export OLLAMA_BASE_URL="http://gpu-box.local:11434"
```

**Model.** Defaults to `llama2`. Override with `OLLAMA_MODEL` or `--model`:

```bash
export OLLAMA_MODEL="llama3.1:70b"
```

The model must already be pulled into the local server (`ollama pull
llama3.1:70b`). omni-dev does not pull on demand.

**Context-length probing.** At startup, omni-dev queries the server for the
actually-loaded context window — LM Studio's `/api/v0/models` first, then
Ollama's `/api/show` as a fallback. The probed length is used for token
budgeting; if probing fails (older Ollama versions, custom servers), the
registry default applies and a debug log is emitted.

**Limitations.**

- Local models often have much smaller context windows than Claude / GPT —
  large `twiddle` ranges may need `--concurrency` tuning.
- Quality varies sharply by model size.
- JSON-schema enforcement depends on the model's instruction-following; small
  models may emit invalid YAML.

**Verification.**

```bash
ollama serve &              # if not already running
ollama pull llama3.1
export USE_OLLAMA=true
export OLLAMA_MODEL="llama3.1"
omni-dev git commit message twiddle 'HEAD~1..HEAD' --use-context
```

## AWS Bedrock

Routes Anthropic model calls through AWS Bedrock's bearer-token API.

**Selection.**

```bash
omni-dev --ai-backend bedrock ...
# or persistently:
export OMNI_DEV_AI_BACKEND=bedrock
# legacy (applies only when OMNI_DEV_AI_BACKEND is unset):
export CLAUDE_CODE_USE_BEDROCK=true
```

**Credentials.** Both are required:

```bash
export ANTHROPIC_AUTH_TOKEN="..."           # bearer token for the Bedrock endpoint
export ANTHROPIC_BEDROCK_BASE_URL="https://bedrock-runtime.<region>.amazonaws.com"
```

**Model.** Resolved through the standard chain (registry default
`claude-sonnet-4-6`). Bedrock identifiers are URL-encoded automatically when
invoking the API, and regional / provider prefixes are normalised by the
registry so any of these forms work for token-budget lookup:

- `claude-sonnet-4-6`
- `anthropic.claude-sonnet-4-6`
- `us.anthropic.claude-sonnet-4-6-v1:0`

Set whichever form your Bedrock deployment expects:

```bash
export ANTHROPIC_MODEL="us.anthropic.claude-sonnet-4-6-v1:0"
```

**Limitations.**

- Bedrock model availability is region-specific — not every Anthropic model
  is listed in every region.
- Inference profiles (the `us.`-prefixed regional IDs) may be required by
  your AWS account; check the Bedrock console.

**Verification.**

```bash
export CLAUDE_CODE_USE_BEDROCK=true
export ANTHROPIC_AUTH_TOKEN="..."
export ANTHROPIC_BEDROCK_BASE_URL="https://bedrock-runtime.us-east-1.amazonaws.com"
omni-dev git commit message twiddle 'origin/main..HEAD' --use-context
```

## Claude CLI Deep-dive

The `claude-cli` backend is the only one with sandbox semantics — it spawns
a real subprocess with elevated capabilities by default, so omni-dev locks
it down explicitly. This section documents what's blocked and how to relax
the sandbox when you need to.

### Sandbox defaults

Every invocation of the subprocess runs with this argv suffix and
environment treatment:

| Constraint                              | Mechanism                                                              |
|-----------------------------------------|------------------------------------------------------------------------|
| Built-in tools disabled                 | `--tools ""` (Read / Edit / Write / Bash / Glob / Grep are unavailable) |
| MCP servers blocked                     | `--strict-mcp-config` with no `--mcp-config`                           |
| User/project/local settings ignored     | `--setting-sources ""`                                                 |
| Slash commands and skills disabled      | `--disable-slash-commands`                                             |
| Session persistence off                 | `--no-session-persistence`                                             |
| Permission prompts disabled             | `--permission-mode default`                                            |
| Fresh working directory                 | Subprocess runs in a unique temp dir, not your repo root               |
| Environment scrubbed                    | `CLAUDE_PROJECT_DIR`, `CLAUDE_CODE_*`, `CLAUDE_PROJECT_*` removed      |
| Output capped                           | `OMNI_DEV_CLAUDE_CLI_STDOUT_MAX_BYTES` (default 4 MiB)                 |
| Wall-clock timeout                      | `OMNI_DEV_CLAUDE_CLI_TIMEOUT_SECS` (default 600)                       |

A defence-in-depth suffix is also appended to the system prompt instructing
the model not to emit `function_calls` XML.

When the [tool-access escape hatch](#escape-hatch-tool-access) is enabled,
the environment scrub additionally removes well-known secret vars
(`*_API_KEY`, `*_TOKEN`, `*_SECRET`, `*_PASSWORD`, `*_CREDENTIALS`, AWS
credentials) — see that section for details and the
`OMNI_DEV_CLAUDE_CLI_KEEP_ENV` exemption list.

### Tuning environment variables

| Variable                                  | Default        | Effect                                                              |
|-------------------------------------------|----------------|---------------------------------------------------------------------|
| `OMNI_DEV_CLAUDE_CLI_BIN`                 | `claude` (PATH) | Path to the `claude` binary.                                       |
| `OMNI_DEV_CLAUDE_CLI_TIMEOUT_SECS`        | `600`          | Wall-clock timeout for one subprocess invocation.                   |
| `OMNI_DEV_CLAUDE_CLI_STDOUT_MAX_BYTES`    | `4194304`      | Stdout cap; output beyond this aborts the invocation.               |
| `OMNI_DEV_CLAUDE_CLI_KEEP_ENV`            | (unset)        | Comma-separated exact names exempted from the [tool-hatch secret scrub](#escape-hatch-tool-access). |

### Escape hatch: tool access

When the nested session needs filesystem or shell access:

```bash
omni-dev --ai-backend claude-cli --claude-cli-allow-tools git branch create pr
# or persistently:
export OMNI_DEV_CLAUDE_CLI_ALLOW_TOOLS=true
```

With this flag the `--tools ""` argument is removed from the subprocess argv,
so the nested session uses Claude Code's default tool set (Read, Edit, Write,
Bash, Glob, Grep) and can act on your filesystem and shell. All other
sandbox flags still apply unless you also enable
[MCP access](#escape-hatch-mcp-access).

**Be aware:** the prompt sent to the nested session is built from untrusted
content — diffs, commit messages, JIRA/Confluence text. With tools enabled, a
prompt-injection payload hidden in that content can drive tool calls against
your filesystem and shell ([#1144](https://github.com/rust-works/omni-dev/issues/1144)).
To limit the blast radius, enabling this hatch also scrubs well-known secret
env vars from the subprocess: anything ending in `_API_KEY`, `_TOKEN`,
`_SECRET`, `_PASSWORD`, or `_CREDENTIALS`, plus `AWS_ACCESS_KEY_ID` and
`AWS_SECRET_ACCESS_KEY`. Two exceptions: `ANTHROPIC_API_KEY` and
`ANTHROPIC_AUTH_TOKEN` are always kept (the nested `claude` may authenticate
through them), and names listed in `OMNI_DEV_CLAUDE_CLI_KEEP_ENV`
(comma-separated, exact) are exempted — use it when a tool-enabled workflow
legitimately needs a credential, e.g.
`OMNI_DEV_CLAUDE_CLI_KEEP_ENV=GITHUB_TOKEN` for env-authenticated `gh`. When
both escape hatches are enabled, MCP servers spawned by the nested session
also see the scrubbed environment; servers that rely on inherited env
credentials need the same exemption.

A `WARN` log is emitted on every invocation while this is active. Grep for
it with `RUST_LOG=omni_dev=warn`:

```
claude -p sandbox weakened: tool-access escape hatch is enabled ...
```

### Escape hatch: MCP access

When the nested session needs MCP servers from your `~/.claude/settings.json`:

```bash
omni-dev --ai-backend claude-cli --claude-cli-allow-mcp git branch create pr
# or:
export OMNI_DEV_CLAUDE_CLI_ALLOW_MCP=true
```

With this flag the `--strict-mcp-config` argument is removed, so the nested
session can load any MCP server you have configured. **Be aware:** MCP
servers frequently hold OAuth tokens (Gmail, Drive, Slack) or expose internal
network services — enabling this exposes them to the nested session. Use
deliberately.

A `WARN` log is emitted on every invocation while this is active:

```
claude -p sandbox weakened: MCP-access escape hatch is enabled ...
```

The two escape hatches are independent — enable tools without MCP, MCP
without tools, both, or neither.

### Spending cap

Pass a per-invocation cap in USD:

```bash
omni-dev --ai-backend claude-cli --claude-cli-max-budget-usd 0.50 \
  git commit message twiddle 'HEAD~3..HEAD'
# or:
export OMNI_DEV_CLAUDE_CLI_MAX_BUDGET_USD=0.50
```

The value is forwarded to `claude -p --max-budget-usd`. If the nested
session exceeds the cap it aborts with an error rather than running away
with cost. Non-positive, non-finite, or non-numeric values are ignored
with a `WARN` and the invocation runs with **no** cap — a typo like
`--claude-cli-max-budget-usd 0` does not silently disable spending
altogether, but it does not cap it either. The cap is ignored on backends
other than `claude-cli`.

Two limitations to be aware of (#1135):

- Enforcement is delegated to `claude -p` itself; omni-dev has no
  independent kill switch if the subprocess mishandles the flag.
- The cost of a model turn is only known after it completes, so a single
  invocation can overshoot the cap by up to one turn before aborting.

Regardless of whether a cap is set, every invocation's `total_cost_usd` is
logged at `INFO` level — run with `RUST_LOG=omni_dev=info` to see it:

```
claude -p invocation cost total_cost_usd=0.0341 max_budget_usd=Some(0.5) model="claude-sonnet-4-6"
```

If the reported cost ever exceeds the configured cap, an additional `WARN`
is emitted.

### When to choose the CLI backend

Use `claude-cli` when:

- You're running inside an existing Claude Code session that already holds
  Anthropic credentials, and you don't want to manage a second API key.
- You want all Anthropic spend to flow through one billing source.
- You explicitly want the sandbox semantics described above.

Prefer the direct [Claude API](#claude-api-default) when:

- You're running in CI or another non-interactive environment where the
  `claude` CLI isn't installed.
- You care about the per-invocation cost floor — the `claude -p` system
  prompt has a small fixed cost (~$0.007 Haiku, ~$0.03 Sonnet, ~$0.15 Opus)
  on each call that the direct API doesn't have.

## Model Registry

omni-dev ships with a built-in catalogue of model identifiers and their
token limits. Inspect it with:

```bash
omni-dev config models show              # merged catalogue with source annotations
omni-dev config models show --embedded-only   # just the built-in entries
```

**Catalogue precedence** — entries from later files override earlier ones:

1. Built-in [src/templates/models.yaml](../src/templates/models.yaml)
2. `~/.omni-dev/models.yaml` (user-level)
3. `./.omni-dev/models.yaml` (project-local)
4. The path in `OMNI_DEV_MODELS_YAML` (if set — short-circuits user/project)

**Bedrock identifiers** are normalised before lookup: regional prefixes
(`us.anthropic.…`), provider prefixes (`anthropic.…`), and version suffixes
(`…-v1:0`) are stripped so the same registry entry serves every form your
Bedrock deployment might emit.

**Architectural background:**
[ADR-0011](adrs/adr-0011.md) (superseded) and
[ADR-0022](adrs/adr-0022.md) (current) — the layered catalogue with user and
project overrides.

## Choosing a Backend

| Criterion           | Claude CLI                    | Claude API                | OpenAI                | Ollama                  | Bedrock                |
|---------------------|-------------------------------|---------------------------|-----------------------|-------------------------|------------------------|
| Setup effort        | Already have Claude Code? Zero | Get an API key           | Get an API key       | Install Ollama + pull model | Configure AWS / endpoint |
| Latency             | High (subprocess spawn)       | Low                       | Low                   | Depends on hardware     | Low                    |
| Per-call floor cost | ~$0.007–$0.15 system prompt   | None                      | None                  | Free (local)            | None                   |
| Sandboxed           | Yes (configurable)            | n/a                       | n/a                   | n/a                     | n/a                    |
| Offline support     | No                            | No                        | No                    | **Yes**                 | No                     |
| Best model quality  | Claude flagship               | Claude flagship           | GPT flagship          | Variable                | Claude flagship        |
| Choose when…        | You already use Claude Code   | Default, simplest path    | OpenAI customer       | Local-only / offline    | AWS shop, Bedrock-only |

Most users should start with the default [Claude API](#claude-api-default)
backend. Move to `claude-cli` once you have Claude Code installed; move to
Bedrock or OpenAI when your organisation requires it; reach for Ollama when
you need offline / local inference.

## Troubleshooting

See the [Troubleshooting Guide](troubleshooting.md) for backend-specific
errors. The most common cases:

| Error                                           | Likely backend     | See                                                                 |
|-------------------------------------------------|--------------------|---------------------------------------------------------------------|
| `CLAUDE_API_KEY not found`                      | Claude API         | [troubleshooting.md](troubleshooting.md#error-claude_api_key-not-found) |
| `the assistant tried to use a tool …`           | Claude CLI         | [troubleshooting.md](troubleshooting.md#claude-cli-backend-issues)  |
| `MCP server X not loaded`                       | Claude CLI         | [troubleshooting.md](troubleshooting.md#claude-cli-backend-issues)  |
| `claude -p exited with cost cap exceeded`       | Claude CLI         | [troubleshooting.md](troubleshooting.md#claude-cli-backend-issues)  |
| Connection refused on `localhost:11434`         | Ollama             | Start `ollama serve` or set `OLLAMA_BASE_URL`.                      |
| `401 Unauthorized` from OpenAI                  | OpenAI             | Check `OPENAI_API_KEY` / `OPENAI_AUTH_TOKEN`.                       |
| `AccessDeniedException` on a model ID           | Bedrock            | Region doesn't have the model, or your IAM policy blocks it.        |

Enable verbose logging when reporting issues:

```bash
RUST_LOG=omni_dev=debug omni-dev git commit message twiddle 'HEAD~1..HEAD' --use-context
```
