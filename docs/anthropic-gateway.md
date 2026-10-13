# Anthropic Gateway

omni-dev can **serve** the Anthropic Messages API in front of a model that
speaks the OpenAI protocol. Claude Code, the Anthropic SDKs and anything else
that honours `ANTHROPIC_BASE_URL` can then use models from OpenAI, Ollama,
LM Studio, vLLM, OpenRouter or Fireworks AI. The gateway accepts
Anthropic-shaped requests, translates them, calls the upstream, and translates
the reply back, streaming included.

This is the opposite direction from the [AI backends](ai-backends.md): a
backend lets omni-dev *call* a model, while the gateway lets *other tools*
reach one through the Anthropic API. The two share code but not configuration.

The security model is [ADR-0098](adrs/adr-0098.md).

## Table of Contents

1. [Quick start](#quick-start)
2. [Endpoints](#endpoints)
3. [Configuration](#configuration)
4. [Choosing an upstream](#choosing-an-upstream)
5. [Model mapping](#model-mapping)
6. [Security](#security)
7. [How requests are translated](#how-requests-are-translated)
8. [Reasoning and thinking signatures](#reasoning-and-thinking-signatures)
9. [Errors](#errors)
10. [Running under the daemon](#running-under-the-daemon)
11. [Field policy](#field-policy)
12. [Keeping up with the Anthropic API](#keeping-up-with-the-anthropic-api)
13. [Limitations](#limitations)
14. [Troubleshooting](#troubleshooting)

## Quick start

Point the gateway at an upstream and give it a default model:

```sh
export OMNI_DEV_GATEWAY_UPSTREAM_URL=http://localhost:11434   # Ollama
export OMNI_DEV_GATEWAY_MODEL=qwen3-coder
omni-dev ai gateway serve
```

In another terminal, print the settings a client needs and run Claude Code
with them:

```sh
eval "$(omni-dev ai gateway env)"
claude
```

`ai gateway env` prints `ANTHROPIC_BASE_URL=http://127.0.0.1:9997` and
`ANTHROPIC_AUTH_TOKEN=<the gateway token>`. With `--format json` it prints an
`{"env": {...}}` object to paste into Claude Code's `~/.claude/settings.json`.
The token is created on first use and kept in `<data dir>/omni-dev/gateway.token`
(`0600`), so the settings stay valid across restarts.

## Endpoints

| Endpoint | Behaviour |
|---|---|
| `POST /v1/messages` | Translated, streaming (`stream: true`) and not. `?beta=true` (what Claude Code sends) is accepted. |
| `POST /v1/messages/count_tokens` | An **estimate** (see [Limitations](#limitations)), marked by an `x-omni-dev-token-estimate: heuristic` header. Passthrough forwards it, falling back to the estimate when the upstream answers 404. |
| `GET /v1/models` | The model map's exact names and the default model, in Anthropic's list shape. |
| `GET /v1/models/{id}` | The model, if the map resolves it. |

Message Batches, the Files API and the Admin API are not served. Every other
path is a `404 not_found_error`.

Every reply carries `x-omni-dev-gateway-upstream-model`, the model that served
it. When the gateway ignored any part of the request (see
[Field policy](#field-policy)), the reply also carries
`x-omni-dev-gateway-ignored`, listing what it ignored.

## Configuration

Every setting is an environment variable, so the `env` block of
`~/.omni-dev/settings.json` and `--profile` work as they do for the rest of
omni-dev. `ai gateway serve` flags override the matching variables. One
process serves one upstream; keep one profile per upstream to switch between
them.

| Variable | Flag | Default | Meaning |
|---|---|---|---|
| `OMNI_DEV_GATEWAY_UPSTREAM_URL` | `--upstream-url` | (required) | Upstream base URL. A trailing `/v1` is dropped; the gateway appends `/v1/...` itself. |
| `OMNI_DEV_GATEWAY_UPSTREAM_PROTOCOL` | `--upstream-protocol` | `chat` | `chat` (Chat Completions), `responses` (Responses API) or `anthropic` (passthrough). |
| `OMNI_DEV_GATEWAY_FLAVOR` | `--flavor` | `generic` | `generic`, `openai` or `fireworks`: which provider quirks apply. |
| `OMNI_DEV_GATEWAY_API_KEY` | — | none | Upstream key, sent as `Authorization: Bearer` (and `x-api-key` in passthrough, except to Fireworks). Optional for local servers. |
| `OMNI_DEV_GATEWAY_MODEL` | `--model` | none | Default upstream model. |
| `OMNI_DEV_GATEWAY_MODEL_MAP` | `--map` (repeatable) | none | `pattern=model,...`; see [Model mapping](#model-mapping). |
| `OMNI_DEV_GATEWAY_TOKEN` | — | persisted file | The inbound token. Without it, the token file is used (and created). |
| `OMNI_DEV_GATEWAY_PORT` | `--port` | `9997` | Port on `127.0.0.1`. `0` picks a free one. |
| `OMNI_DEV_GATEWAY_EFFORT_LEVELS` | — | per flavor | Comma-separated `reasoning_effort` values the upstream model accepts, e.g. `low,medium,high` for Harmony models. Efforts outside the set are clamped down. |
| `OMNI_DEV_GATEWAY_THINK_TAGS` | — | `true` | Split a leading inline `<think>…</think>` block out of the answer as reasoning. |
| `OMNI_DEV_GATEWAY_UNKNOWN_FIELDS` | — | `reject` | `warn` ignores request fields the gateway does not know instead of rejecting them. |
| `OMNI_DEV_GATEWAY_MAX_CONCURRENT` | — | `16` | Concurrent upstream calls; past it the gateway answers `429`. |

`OMNI_DEV_GATEWAY_API_KEY` and `OMNI_DEV_GATEWAY_TOKEN` are secrets. Each also
accepts a `_FILE` companion (`OMNI_DEV_GATEWAY_API_KEY_FILE=/path/to/key`, an
owner-only file) or a `_COMMAND` companion (`OMNI_DEV_GATEWAY_API_KEY_COMMAND`,
a helper program whose output is the key); see
[secret commands](secret-commands.md). Neither secret is ever accepted as a
flag.

The upstream idle-read timeout is `OMNI_DEV_AI_TIMEOUT_SECS` (default 300 s),
shared with the AI backends. While the upstream is silent, the gateway sends
the client a `ping` event every 15 seconds.

## Choosing an upstream

| Upstream | `UPSTREAM_URL` | Protocol | Flavor |
|---|---|---|---|
| Ollama | `http://localhost:11434` | `chat` | `generic` |
| LM Studio | `http://localhost:1234` | `chat` | `generic` |
| vLLM | `http://host:8000` | `chat` | `generic` |
| OpenRouter | `https://openrouter.ai/api` | `chat` | `generic` |
| OpenAI | `https://api.openai.com` | `chat` or `responses` | `openai` |
| Fireworks AI | `https://api.fireworks.ai/inference` | `anthropic` (recommended) or `chat` | `fireworks` |

**OpenAI.** Some OpenAI models call tools only through the Responses API, so
use `responses` for them. The gateway sends `store: false`, so OpenAI keeps no
conversation state, and asks for encrypted reasoning, which it carries between
turns inside its own thinking signatures.

**Fireworks AI.** Fireworks documents its own Anthropic-compatible surface at
`https://api.fireworks.ai/inference/v1/messages`. It covers streaming, tools,
`thinking` (`enabled`, `disabled`, `adaptive`), `output_config.effort` and
`format`, and images. That makes **passthrough** (`--upstream-protocol
anthropic --flavor fireworks`) the recommended path for Claude Code. The
gateway then maps the model, swaps the gateway token for your Fireworks key,
logs the call, and removes the fields Fireworks documents as unsupported
(`inference_geo`, and a tool's `cache_control`, `eager_input_streaming`,
`allowed_callers` and `input_examples`), reporting them as ignored. Fireworks
answers `count_tokens` with 404, so the gateway estimates it instead.

The `chat` protocol with the `fireworks` flavor is there for models or
features outside that surface. It handles Fireworks' quirks:

- it sends `thinking` *or* `reasoning_effort`, never both (Fireworks rejects
  both);
- it sends `max_tokens` only;
- it sets `context_length_exceeded_behavior: "error"`, since Fireworks
  otherwise truncates silently where Anthropic would error;
- it caps `stop` at 4 sequences;
- it folds the trailing usage chunk into the final event;
- it sets `prompt_cache_key`;
- it replays `reasoning_content` on multi-turn tool use.

Fireworks behaviour here is taken from its documentation as of 2026-10-14. See
[Limitations](#limitations).

## Model mapping

Clients name Claude models; the upstream serves something else. The gateway
resolves the client's `model` in this order:

1. an exact key in `OMNI_DEV_GATEWAY_MODEL_MAP`;
2. the first glob key that matches (`*` matches any run of characters);
3. a name that does not start with `claude-` is passed through as-is, so a
   client can name an upstream model directly (`claude --model
   accounts/fireworks/models/glm-5p1`);
4. `OMNI_DEV_GATEWAY_MODEL`.

A `claude-…` name with no match and no default is a `404 not_found_error`,
rather than silently answered by some other model.

```sh
export OMNI_DEV_GATEWAY_MODEL_MAP='claude-opus-*=accounts/fireworks/models/kimi-k2p6,claude-haiku-*=accounts/fireworks/models/minimax-m2p5'
export OMNI_DEV_GATEWAY_MODEL=accounts/fireworks/models/glm-5p1
```

Responses echo the model name the client sent. The upstream model is in the
`x-omni-dev-gateway-upstream-model` header.

## Security

The gateway is a new listening surface, so it is default-closed like the
browser bridge ([ADR-0036](adrs/adr-0036.md)). [ADR-0098](adrs/adr-0098.md) has
the reasoning.

- It binds **`127.0.0.1` only**, and fails if the port is taken.
- **Every** request must carry the gateway token, as `x-api-key` or
  `Authorization: Bearer` (what the Anthropic SDKs and Claude Code send),
  compared in constant time. Without it the answer is `401
  authentication_error`.
- The `Host` header must be a loopback authority for the bound port, which
  defeats DNS rebinding. A request with an `Origin` or a
  `Sec-Fetch-Site: cross-site|same-site|same-origin` header is refused, so a
  web page you visit cannot reach the gateway. Both answer `403
  permission_error`.
- The upstream key never reaches the client, and the gateway token never
  reaches the upstream.
- Each upstream call is recorded in the [request log](log.md) under the
  `anthropic-gateway` service, with auth headers redacted. Bodies are
  recorded only with `OMNI_DEV_LOG_BODIES`.

## How requests are translated

Each request goes through three steps:

1. The [field policy](#field-policy) classes every field, content block and
   tool field.
2. The conversation is lowered. A tool's result moves directly after the call
   it answers, and before any user text in the same Anthropic message.
   Mid-conversation `system` messages stay in place. Assistant prefill is sent
   as a finished turn, so the upstream answers after it rather than
   continuing it; it is reported as `assistant_prefill` in the ignored list.
3. The request is written in the upstream's protocol:
   - text and images (base64 as a `data:` URI, or a URL) become messages;
   - plain-text documents and search results become text;
   - tools become functions;
   - `tool_choice` maps `any`→`required`, `tool`→a named function and
     `disable_parallel_tool_use`→`parallel_tool_calls: false`;
   - `output_config.format` becomes a `json_schema` response format, strict on
     OpenAI;
   - `cache_control` anywhere sets a `prompt_cache_key` derived from the model,
     system prompt and tools, on upstreams that take one.

   Tool names over OpenAI's 64-character limit (long MCP tool names) are
   shortened with a hash and mapped back in the reply.

Replies come back in Anthropic's shape:

| Upstream | Anthropic `stop_reason` |
|---|---|
| `stop` | `end_turn` (`tool_use` if the reply has tool calls) |
| `length` | `max_tokens` |
| `tool_calls` / `function_call` | `tool_use` |
| `content_filter`, or a `refusal` | `refusal`, with `stop_details: {"type": "refusal"}` |

Usage is reported the Anthropic way. `input_tokens` excludes cached tokens;
cached tokens are `cache_read_input_tokens`; no OpenAI-protocol upstream
reports cache writes, so `cache_creation_input_tokens` is always 0; reasoning
tokens are `output_tokens_details.thinking_tokens`.

Streaming re-frames OpenAI's chunks as Anthropic's full event sequence:
`message_start`, `ping`, one `content_block_start` / `content_block_delta` /
`content_block_stop` run per block (`text_delta`, `thinking_delta`,
`signature_delta`, `input_json_delta`), `message_delta` with the stop reason
and usage, then `message_stop`. Anthropic blocks must not interleave. When an
upstream interleaves the arguments of parallel tool calls, the gateway streams
the first call live and buffers the others until it closes. A failure part-way
through ends the stream with an `error` event.

## Reasoning and thinking signatures

What the gateway sends upstream for each `thinking` setting:

| `thinking` | Fireworks (`chat`) | Other `chat` upstreams | `responses` |
|---|---|---|---|
| `enabled` + `budget_tokens` | `thinking` as sent | `reasoning_effort`: the request's `effort`, else bucketed from the budget | `reasoning.effort` the same way |
| `adaptive` / `between_tools` | `reasoning_effort` if `effort` is set, else the model's default | the same | the same |
| `disabled` | `thinking: {"type": "disabled"}` | nothing | nothing |

`output_config.effort` (`low` to `max`) is clamped to what the upstream takes:

- OpenAI: `low`–`xhigh`;
- generic: `low`–`high`;
- Fireworks: all five.

Override this with `OMNI_DEV_GATEWAY_EFFORT_LEVELS`. Claude Code's per-turn
`output_config` on a mid-conversation system message sets the effort for that
turn.

The upstream's reasoning comes back as `thinking` blocks. The gateway reads it
from `reasoning_content`, from `reasoning`, or from a leading inline `<think>`
block. It returns them only when the client asked for thinking, and as empty
blocks for `display: "omitted"`, as Anthropic does.

**Signatures.** Anthropic signs thinking blocks to bind them to the account
that produced them. No OpenAI-protocol upstream signs anything, so the gateway
signs its own blocks: `ogw1.<payload>.<HMAC-SHA256>`, keyed from the gateway
token. On the next turn:

- a block whose signature verifies is replayed upstream, as
  `reasoning_content` on the assistant message, or as a Responses `reasoning`
  item rebuilt from the encrypted content the payload carries;
- a block that does not verify (produced by Anthropic, by another gateway, or
  under an old token) is dropped and reported as ignored.

With `display: "omitted"`, the reasoning the client does not see travels
inside the payload, so it is replayed all the same. The payload is signed,
not encrypted. Rotating the token stops old signatures verifying, and the
next turn simply goes without that reasoning. Claude Code accepts these
signatures; it does not check them itself.

## Errors

Every error is answered in Anthropic's envelope,
`{"type": "error", "error": {"type": ..., "message": ...}, "request_id": ...}`,
with a status the Anthropic SDKs and Claude Code already handle.

| Upstream answer | Gateway answer |
|---|---|
| 400 / 422 | `400 invalid_request_error`. A context-length error is reworded to start with `prompt is too long`, which makes Claude Code compact the conversation and retry. |
| 401 / 403 | `403 permission_error` naming `OMNI_DEV_GATEWAY_API_KEY`. Not 401, which would tell Claude Code its own key is wrong. |
| 404 | `404 not_found_error` |
| 408 / 504 | `504 timeout_error` |
| 413 | `413 request_too_large` |
| 429 | `429 rate_limit_error`, forwarding `Retry-After` |
| 503 / 529 | `529 overloaded_error` |
| other 5xx | `500 api_error` |
| unreachable | `502 api_error` |

## Running under the daemon

The [daemon](browser-bridge.md#running-under-the-daemon) hosts the same server as the `anthropic-gateway`
service, so it starts at login and shows in the tray:

```sh
omni-dev daemon start                      # every service, including the gateway
omni-dev daemon start --services worktrees,anthropic-gateway
omni-dev daemon service anthropic-gateway status
omni-dev daemon service anthropic-gateway restart   # re-reads the configuration
omni-dev daemon service anthropic-gateway env       # the client settings
```

The daemon reads the same `OMNI_DEV_GATEWAY_*` settings. A service manager
starts the daemon with a minimal environment, so put them in the `env` block
of `~/.omni-dev/settings.json`. The service behaves as follows:

- With no upstream configured, it registers but binds nothing (`daemon status`
  reports "not configured"), so a default daemon opens no port.
- A configuration error or a taken port marks the service unhealthy, without
  stopping the daemon.
- The standalone `serve` and the daemon share the token file, so they also
  share the default port: run one or the other.

## Field policy

The gateway must never silently drop a field that changes behaviour. Every
request field, content-block type and tool field is therefore classed in one
table in code (`src/gateway/policy.rs`):

- **translated:** sent upstream in its equivalent form;
- **ignored:** accepted but not sent, named in `x-omni-dev-gateway-ignored`,
  and logged (`warn` the first time each field is seen);
- **rejected:** answered with `400 invalid_request_error`, and nothing is sent
  upstream.

A field the table does not know is rejected, since there is no way to tell
whether dropping it is safe. `OMNI_DEV_GATEWAY_UNKNOWN_FIELDS=warn` downgrades
unknown fields to ignored, for a client newer than the gateway. The same rule
covers message-level keys, `output_config` keys and `thinking.display` values.

`safeguards`, which Claude Code sends in auto permission mode, is rejected on
purpose. It asks Anthropic's servers to run a dangerous-tool-use classifier.
No upstream can run it, and dropping a safety control silently is exactly what
the policy forbids. Claude Code retries without it.

`omni-dev ai gateway fields` prints the table. A test keeps the copy below in
sync with the code.

<!-- BEGIN gateway-fields: generated by `omni-dev ai gateway fields`; keep in sync -->
#### Request fields

| Name | Class | Handling |
|---|---|---|
| `model` | translated | mapped to an upstream model |
| `max_tokens` | translated | `max_tokens`, `max_completion_tokens` (OpenAI) or `max_output_tokens` (Responses); 0 is rejected |
| `messages` | translated | chat messages / Responses input items |
| `system` | translated | a leading system message / Responses `instructions` |
| `stop_sequences` | translated | `stop`; more than 4 rejected on OpenAI and Fireworks; rejected on the Responses target |
| `stream` | translated | upstream streaming, re-framed as Anthropic events |
| `temperature` | translated | dropped (with a warning) for GPT-5/o-series models |
| `top_p` | translated | dropped (with a warning) for GPT-5/o-series models |
| `top_k` | translated | sent to Fireworks and generic upstreams; ignored on OpenAI and Responses |
| `tools` | translated | custom tools become functions; see the tool tables |
| `tool_choice` | translated | `auto`, `any`→`required`, `tool`→named function, `none`; `disable_parallel_tool_use`→`parallel_tool_calls: false` |
| `thinking` | translated | upstream reasoning controls; see the operator guide |
| `output_config` | translated | `effort`→reasoning effort, `format`→JSON-schema response format; other keys rejected |
| `cache_control` | translated | sets `prompt_cache_key` where the upstream takes one, else ignored |
| `metadata` | ignored | no upstream equivalent |
| `service_tier` | ignored | no upstream equivalent |
| `inference_geo` | ignored | no upstream equivalent |
| `diagnostics` | ignored | no upstream equivalent |
| `speed` | ignored | no upstream equivalent |
| `user_profile_id` | ignored | Anthropic account routing |
| `workspace_id` | ignored | Anthropic account routing |
| `context_management` | ignored | the client's history is sent whole; an overflow returns `prompt is too long` |
| `compaction` | ignored | the client's history is sent whole; an overflow returns `prompt is too long` |
| `fallbacks` | ignored | only the mapped model is called |
| `fallback_credit_token` | ignored | only the mapped model is called |
| `container` | rejected | server-side code execution has no upstream equivalent |
| `safeguards` | rejected | Anthropic's server-side tool-use classifiers cannot run upstream, and a safety control is never dropped silently; Claude Code retries without it |
| `mcp_servers` | rejected | the MCP connector has no upstream equivalent |

#### Content blocks

| Name | Class | Handling |
|---|---|---|
| `text` | translated | text |
| `image` | translated | `image_url` (base64 data URI or URL); `file` sources rejected |
| `document` | translated | plain-text and inline-content documents become text; PDF, URL and file sources rejected |
| `search_result` | translated | rendered as text |
| `thinking` | translated | replayed as upstream reasoning when the gateway's signature verifies, else dropped |
| `redacted_thinking` | ignored | encrypted for Anthropic; no upstream can read it |
| `tool_use` | translated | assistant `tool_calls` / Responses `function_call` |
| `tool_result` | translated | `role: tool` message / Responses `function_call_output` |
| `tool_reference` | translated | rendered as text; every tool is sent eagerly |
| `server_tool_use` | rejected | server tools have no upstream equivalent |
| `web_search_tool_result` | rejected | server tools have no upstream equivalent |
| `web_fetch_tool_result` | rejected | server tools have no upstream equivalent |
| `code_execution_tool_result` | rejected | server tools have no upstream equivalent |
| `bash_code_execution_tool_result` | rejected | server tools have no upstream equivalent |
| `text_editor_code_execution_tool_result` | rejected | server tools have no upstream equivalent |
| `tool_search_tool_result` | rejected | server tools have no upstream equivalent |
| `container_upload` | rejected | server-side code execution has no upstream equivalent |

#### Tool fields

| Name | Class | Handling |
|---|---|---|
| `type` | translated | absent or `custom`; any Anthropic-defined tool type is rejected |
| `name` | translated | names over 64 characters are shortened and mapped back |
| `description` | translated | function description |
| `input_schema` | translated | function parameters |
| `strict` | translated | function `strict` |
| `input_examples` | translated | appended to the description |
| `cache_control` | translated | contributes to `prompt_cache_key` |
| `defer_loading` | ignored | every tool is sent eagerly |
| `eager_input_streaming` | ignored | arguments always stream |
| `allowed_callers` | ignored | only direct calls exist upstream |

#### Stop reasons

| Name | Class | Handling |
|---|---|---|
| `end_turn` | translated | `finish_reason: stop` |
| `max_tokens` | translated | `finish_reason: length` |
| `stop_sequence` | ignored | upstreams do not say which sequence matched; reported as `end_turn` |
| `tool_use` | translated | `finish_reason: tool_calls` |
| `pause_turn` | ignored | only server tools pause |
| `refusal` | translated | `content_filter` or an upstream `refusal` |
| `model_context_window_exceeded` | ignored | upstreams return an error, answered as `prompt is too long` |

#### Stream deltas

| Name | Class | Handling |
|---|---|---|
| `text_delta` | translated | text |
| `input_json_delta` | translated | tool arguments |
| `thinking_delta` | translated | reasoning text |
| `signature_delta` | translated | the gateway's signature |
| `citations_delta` | ignored | upstreams return no citations |
<!-- END gateway-fields -->

## Keeping up with the Anthropic API

The Anthropic API changes often. A weekly job
(`.github/workflows/anthropic-api-drift.yml`) runs the `anthropic-api-drift`
binary. It reads the official Python SDK's generated types and compares them
with the tables above: request fields, content blocks, tool fields and types,
thinking types and display modes, stop reasons and stream deltas. Anything
upstream that the gateway does not class opens or updates an issue labelled
`anthropic-api-drift`, since such a field would be rejected as unknown until it
is classed. To run it by hand:

```sh
cargo run --bin anthropic-api-drift -- --output-dir /tmp/drift
```

## Limitations

- **`count_tokens` is an estimate**, about four characters per token plus
  per-message, per-tool and per-image overheads. An exact count needs the
  upstream model's tokenizer, which no OpenAI-protocol server exposes.
  Clients use the count to decide when to compact, where an estimate is
  enough.
- **`stop_sequence`** is never reported, because upstreams do not say which
  sequence matched. A stop on one reads as `end_turn`. `pause_turn` and
  `model_context_window_exceeded` cannot occur: there are no server tools, and
  an upstream errors on overflow instead.
- **Server tools** (web search and fetch, code execution, computer and browser
  use, memory, the MCP connector, containers) are rejected. Anthropic-defined
  client tools (`bash_*`, `text_editor_*`) are rejected too; define a custom
  tool with an `input_schema` instead.
- **Citations** in input blocks are ignored, and no citations come back.
- **PDF documents** and `file` sources are rejected. Plain-text and
  inline-content documents are sent as text.
- **Prompt caching** is as good as the upstream's own. `cache_control` only
  sets a routing key, and no upstream reports cache writes.
- **Live Fireworks conformance.** `tests/gateway_fireworks_live.rs` checks both
  modes against Fireworks, with a non-reasoning model and a reasoning model
  using a tool over two turns. It only runs with
  `OMNI_DEV_GATEWAY_LIVE_FIREWORKS=1` and a key; see the file header.
  Fireworks notes that some models (Kimi-K2-Thinking, MiniMax-M2, GLM-4.7)
  have returned no visible answer on a first turn; that is the model's
  behaviour, and the gateway passes it through.

## Troubleshooting

- **`401 authentication_error`**: the client's token does not match. Run
  `omni-dev ai gateway env` again; the token comes from
  `OMNI_DEV_GATEWAY_TOKEN` if set, otherwise from the token file.
- **`403 permission_error` mentioning `OMNI_DEV_GATEWAY_API_KEY`**: the
  upstream rejected the gateway's key.
- **`400` naming a field "not known to the omni-dev Anthropic gateway"**: the
  client sent something newer than the gateway's table. Set
  `OMNI_DEV_GATEWAY_UNKNOWN_FIELDS=warn` to carry on, and report it; the drift
  check will usually have opened an issue already.
- **`404` "not mapped to an upstream model"**: add the model to
  `OMNI_DEV_GATEWAY_MODEL_MAP` or set `OMNI_DEV_GATEWAY_MODEL`.
- **Fireworks `400` about `thinking` and `reasoning_effort`**: you are on the
  `chat` protocol with the `generic` flavor. Set `--flavor fireworks`.
- **What actually went upstream**: `omni-dev log --service anthropic-gateway`
  lists each upstream call. Set `OMNI_DEV_LOG_BODIES=1` to record bodies too.
