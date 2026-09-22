# Jev (TypeSafe System One)

omni-dev calls TypeSafe AI's **Jev** "System One" API through the
`omni-dev ai jev` command tree. Jev does not write text. You give it an opaque
program **state** (a string, object or array) and one or more typed
**questions**, and it returns **typed, probabilistic judgments**. A question
can pick one of a labelled set, rate the state on an ordered scale, or give the
probability of a yes/no condition. All the questions are answered in one
parallel pass over `POST /v1/systemone`.

Use it instead of `omni-dev ai chat` when a script needs a *decision* rather
than prose: routing a ticket, grading a diff's risk, or asking "is this commit
a breaking change?". The answer arrives as a number or a label with
probabilities attached. There is no free text to parse, no prompt to coax into
JSON, and no model-registry token limit to fit into.

Jev lives under `ai` but is deliberately **not** an [AI backend](ai-backends.md).
Most of its subcommands do not accept the AI backend flags (`--ai-backend`,
`--model`, `--beta-header`, `--claude-cli-*`, `--models-yaml`) — passing any
of them is a clap error, "unexpected argument". [`route`](#route) and
[`verify-decision`](#verify-decision) are the two exceptions to `-C/--repo`,
which they use to decide which repository a bare `#N` means.
[`verify-decision`](#verify-decision) is also the one Jev command that
**does** accept the AI backend flags: it uses an AI backend, not Jev, to
split a decision comment into statements before checking each one with Jev.
Commit/PR generation never uses Jev, and its request shape (state plus a
question map) has nothing in common with a chat completion.

## Table of Contents

1. [Prerequisites](#prerequisites)
2. [Authentication](#authentication)
3. [Choosing a model](#choosing-a-model)
4. [State input](#state-input)
5. [Output formats](#output-formats)
6. [choice](#choice)
7. [score](#score)
8. [noul](#noul)
9. [ask](#ask)
10. [route](#route)
11. [verify-decision](#verify-decision)
12. [Ordering caveats](#ordering-caveats)
13. [Best practices](#best-practices)
14. [Retries and timeouts](#retries-and-timeouts)
15. [Request log](#request-log)
16. [Troubleshooting](#troubleshooting)
17. [See also](#see-also)

## Prerequisites

- A TypeSafe account and a Jev **API key**. The same key works with
  TypeSafe's own SDKs, which read it from `TYPESAFE_API_KEY`.
- For [`route`](#route) only: the GitHub CLI (`gh`), authenticated with read
  access to the repositories whose issues you route. The token stays inside
  `gh`; omni-dev never reads it.

## Authentication

Credentials are read from environment variables first, falling back to the
`env` map in `~/.omni-dev/settings.json`. There is no `auth login` flow, so to
persist the key, add it to that map by hand.

### Environment variables

| Variable                | Purpose                                                                                   | Default                   |
|-------------------------|-------------------------------------------------------------------------------------------|---------------------------|
| `TYPESAFE_API_KEY`      | Jev API key. This is the vendor's own variable, so a key you already exported just works. | _none_                    |
| `OMNI_DEV_JEV_API_KEY`  | omni-dev-specific API key, used only when `TYPESAFE_API_KEY` is unset or empty.           | _none_                    |
| `OMNI_DEV_JEV_BASE_URL` | API base URL. A trailing `/` is trimmed. Use it for proxies or tests.                     | `https://api.typesafe.ai` |
| `TYPESAFE_MODEL`        | Jev model identifier. See [Choosing a model](#choosing-a-model).                          | `jev-latest`              |

One of the two key variables is required. An empty value counts as unset.

Each variable is looked up in the process environment first, then in the
`settings.json` `env` map, so an exported shell or CI variable always wins:

```json
{
  "env": {
    "TYPESAFE_API_KEY": "..."
  }
}
```

Credential profiles apply too. Under `--profile work` (or
`OMNI_DEV_PROFILE=work`), the lookup falls back to `profiles.work.env` instead
of the base `env` map. See
[Credential Profiles](configuration-best-practices.md#credential-profiles) for
the general rule.

The key is sent as `Authorization: Bearer <key>` and is redacted from `Debug`
output and from the [request log](#request-log).

## Choosing a model

The model is resolved in this order, and the first one set wins:

1. `--jev-model <MODEL>` on the subcommand.
2. `TYPESAFE_MODEL` (process env, then `settings.json`).
3. `jev-latest`.

> **Footgun: Jev never reads `OMNI_DEV_MODEL`.** `omni-dev ai jev choice
> --model x` is now a clap error — `--model` is scoped to the AI backend
> commands and jev is not one of them, so it no longer even parses there. But
> the *environment variable* `OMNI_DEV_MODEL` is a different matter: Jev
> silently ignores it. A user with `OMNI_DEV_MODEL=claude-opus-5` exported for
> the AI backends must not have every Jev call sent to a model Jev has never
> heard of. Use `--jev-model` or `TYPESAFE_MODEL`.

The response's `model` field names the **concrete** version that answered,
such as `jev-1.13.0`, even when you asked for the `jev-latest` alias. Pin that
value with `--jev-model` if you need reproducible judgments across a Jev
release.

## State input

Every subcommand takes the state as an optional positional `[STATE]` argument,
and reads it from **stdin** when that argument is omitted. A state that is
empty or only whitespace is rejected before any request is sent:

```
Error: state must not be empty (pass it as an argument or on stdin)
```

By default the state is sent as a **literal JSON string**, exactly as given.
omni-dev never guesses, because silently re-parsing plain text that happens to
start with `{` would change what Jev sees.

Pass `--state-json` to send **structured** state instead. The text is then
parsed as YAML. YAML is a superset of JSON, so plain JSON works too, and the
resulting object or array is sent as-is. The API accepts only a string, an
object or an array, so a state that parses to a number, boolean or `null`
(`42`, `true`) is rejected before any request is sent. Drop `--state-json` to
send such text as a literal string:

```bash
# Literal string (default)
omni-dev ai jev noul "payouts failing for 3 days" --instructions "Is this urgent?"

# Structured state from a JSON file on stdin
omni-dev ai jev noul --state-json --instructions "Is this urgent?" < ticket.json

# A diff piped in as a literal string
git diff origin/main | omni-dev ai jev score --instructions "How risky is this change?" \
    --level Trivial --level Moderate --level Risky
```

## Output formats

Every subcommand accepts `-o/--output <FORMAT>`. It defaults to `json`.

| Format | Best for                                   |
|--------|--------------------------------------------|
| `json` | Scripting; pipe into `jq`. Pretty-printed. |
| `yaml` | Reading in a terminal.                     |

`route` alone also accepts `text` — one human-readable paragraph per issue,
for reading rather than scripting. See [Text output](#text-output) below.

There is no bare-value mode. `model` and `usage` are always kept in the output,
because they are the only place the concrete model version and the token cost
show up. `jq -r .answer.choice` gets a bare value when a script needs one.

The single-question subcommands (`choice`, `score`, `noul`) emit:

```json
{ "model": "...", "answer": { "type": "...", ... }, "usage": { "input_tokens": 0, "output_tokens": 0 } }
```

`ask` emits the same thing with an `answers` map, keyed by your question names,
in place of `answer`:

```json
{ "model": "...", "answers": { "<name>": { "type": "...", ... } }, "usage": { ... } }
```

Each answer carries a `type` tag (`choice`, `score` or `noul`), and the rest of
its shape depends on that type. The shapes are shown in the sections below.

## choice

Picks exactly one of a labelled set of options.

| Flag                    | Meaning                                                |
|-------------------------|--------------------------------------------------------|
| `--instructions <TEXT>` | What to choose and why (required).                     |
| `--option <NAME=DESC>`  | One option. Repeatable, and at least two are required. |

`--option` splits on the **first** `=` only, so `--option "eq=a = b"` keeps
`a = b` as the description. An empty name is rejected, and so is a duplicate
name. A later duplicate does not silently win.

```bash
$ omni-dev ai jev choice "Card was charged twice for one order" \
    --instructions "Which team should handle this ticket?" \
    --option billing="Payments, refunds, invoices" \
    --option technical="Bugs, outages, errors" \
    --option sales="Pricing, upgrades, new accounts"
{
  "model": "jev-1.13.0",
  "answer": {
    "type": "choice",
    "choice": "billing",
    "confidence": 0.97,
    "probabilities": {
      "billing": 0.98,
      "sales": 0.0,
      "technical": 0.02
    }
  },
  "usage": {
    "input_tokens": 142,
    "output_tokens": 24
  }
}
```

`choice` is one of the option names. `probabilities` has one entry per option.
Options come out **alphabetised**, not in `--option` order. See
[Ordering caveats](#ordering-caveats).

## score

Rates the state against an ordered scale.

| Flag                    | Meaning                                                                                          |
|-------------------------|--------------------------------------------------------------------------------------------------|
| `--instructions <TEXT>` | What to rate and how (required).                                                                 |
| `--level <DESC>`        | One scale level, **low to high**. Repeatable, and at least two are required. Order is preserved. |

```bash
$ omni-dev ai jev score "This is the THIRD time I've asked. Fix it today." \
    --instructions "How frustrated is the customer?" \
    --level "Calm, just stating facts" \
    --level "Frustrated but civil" \
    --level "Very angry, strong language" \
    -o yaml
model: jev-1.13.0
answer:
  type: score
  score: 1.01
  confidence: 0.98
  legend:
    '0': Calm, just stating facts
    '1': Frustrated but civil
    '2': Very angry, strong language
  probabilities:
    '0': 0.0
    '1': 0.99
    '2': 0.01
usage:
  input_tokens: 151
  output_tokens: 23
```

`score` is a **continuous** position on the scale. It is index-like, counting
from `0` for the first `--level`, but it is not necessarily a whole number.
`legend` maps each index to the level you supplied. `probabilities` gives each
index's probability, keyed by that index as a string.

## noul

Estimates the probability that a yes/no condition holds.

| Flag                    | Meaning                                        |
|-------------------------|------------------------------------------------|
| `--instructions <TEXT>` | The yes/no condition to estimate (required).   |
| `--true-means <TEXT>`   | Optional: what a `true`-leaning answer means.  |
| `--false-means <TEXT>`  | Optional: what a `false`-leaning answer means. |

When neither `--true-means` nor `--false-means` is given, the request omits
`criteria` entirely.

```bash
$ omni-dev ai jev noul "Payouts have been failing for 3 days, I want my money back" \
    --instructions "Is the customer asking for a refund?"
{
  "model": "jev-1.13.0",
  "answer": {
    "type": "noul",
    "noul": 0.98
  },
  "usage": {
    "input_tokens": 124,
    "output_tokens": 24
  }
}
```

`noul` is the probability that the condition is true, in `[0, 1]`. Unlike
`choice` and `score`, a `noul` answer has **no `confidence` field**. The
probability already *is* the confidence.

## ask

Asks several questions about **one** state in a single request, and so in a
single parallel pass. The questions come from a YAML or JSON file given with
`--questions <FILE>`. Both formats go through the same parser, so the file
extension does not matter.

The questions are read only from the file. The state keeps the
positional-or-stdin slot, so `git diff | omni-dev ai jev ask --questions q.yaml`
is never ambiguous about which input stdin feeds.

The file maps a caller-chosen **question name** to a question spec. The spec is
the Jev wire shape, with a `type` of `choice`, `score` or `noul`:

| `type`   | `criteria`                                                |
|----------|-----------------------------------------------------------|
| `choice` | Required map of option name to description, at least two. |
| `score`  | Required list of levels, low to high, at least two.       |
| `noul`   | Optional map with `"true"` and/or `"false"` descriptions. |

```yaml
# triage.yaml
department:
  type: choice
  instructions: Which team should handle this ticket?
  criteria:
    billing: Payments, refunds, invoices
    technical: Bugs, outages, errors
    sales: Pricing, upgrades, new accounts
frustration:
  type: score
  instructions: How frustrated is the customer?
  criteria:
    - Calm, just stating facts
    - Frustrated but civil
    - Very angry, strong language
refund_requested:
  type: noul
  instructions: Is the customer asking for a refund?
  criteria:
    "true": Explicitly asks for money back
    "false": Wants the problem fixed, not refunded
```

The `"true"`/`"false"` keys are quoted above for clarity and for other YAML
tools, which may read bare `true`/`false` as booleans. omni-dev accepts them
either way.

```bash
$ omni-dev ai jev ask "Payouts failing 3 days, I want my money back NOW" \
    --questions triage.yaml
{
  "model": "jev-1.13.0",
  "answers": {
    "department": {
      "type": "choice",
      "choice": "billing",
      "confidence": 0.97,
      "probabilities": {
        "billing": 0.98,
        "sales": 0.0,
        "technical": 0.02
      }
    },
    "frustration": {
      "type": "score",
      "score": 1.01,
      "confidence": 0.98,
      "legend": {
        "0": "Calm, just stating facts",
        "1": "Frustrated but civil",
        "2": "Very angry, strong language"
      },
      "probabilities": {
        "0": 0.0,
        "1": 0.99,
        "2": 0.01
      }
    },
    "refund_requested": {
      "type": "noul",
      "noul": 0.98
    }
  },
  "usage": {
    "input_tokens": 417,
    "output_tokens": 71
  }
}
```

A file that defines no questions is rejected before any request is sent. So is
a spec with an unknown `type` or a missing required field, which fails with
`Failed to parse questions file <path>` plus the parser's reason. Each spec is
also held to the same minimums as the single-question subcommands: a `choice`
with fewer than two options, or a `score` with fewer than two levels, fails
with an error naming the question.

These minimums are omni-dev's own rule, not the API's. The API accepts a
one-option `choice` or a one-level `score`, but it can only answer either one
with `confidence: 1.0`. Such a call costs money and tells you nothing, and it
usually means the spec was mistyped.

## route

Asks which **model class** should handle each stage of the work on a GitHub
issue: the **design** (choosing the approach and settling open questions), the
**implementation** and the **review**. The classes come from a named model
ladder (see [Built-in ladders](#built-in-ladders) and
[Custom ladders](#custom-ladders)); `--ladders` routes against several at
once. It makes one Jev call per issue, with three `choice` questions per
ladder plus one `noul` question per open issue/PR the text cites, and
fetches the issues through `gh`.

```bash
omni-dev ai jev route '#1779' rust-works/omni-dev#1641 -o yaml
omni-dev ai jev route https://github.com/rust-works/omni-dev/issues/1779
omni-dev ai jev route --all-open -C ~/src/omni-dev
omni-dev ai jev route '#1820' --ladders anthropic,openai,gemini
omni-dev ai jev route '#1826' --ladders anthropic,mine --ladder-definition mine=my-tiers.yaml
omni-dev ai jev route --all-open -o text
```

`<ISSUE>` is `#N` or `N` (in the current repository, which `-C/--repo`
changes), `owner/repo#N`, or an issue URL. `--all-open` routes every open issue
in the current repository instead. Quote `#N` in the shell, where an unquoted
`#` starts a comment.

```yaml
model: jev-1.13.0
issues:
- ref: rust-works/omni-dev#1641
  url: https://github.com/rust-works/omni-dev/issues/1641
  title: ...
  providers:
    anthropic:
      stages:
        design:    {choice: fable,  confidence: 0.52, probabilities: {fable: 0.74, none: 0.01, opus: 0.24, sonnet: 0.01}}
        implement: {choice: sonnet, confidence: 0.83, probabilities: {fable: 0.0, opus: 0.12, sonnet: 0.88}}
        review:    {choice: opus,   confidence: 0.41, probabilities: {fable: 0.02, opus: 0.57, sonnet: 0.41}}
      class: fable
      close_calls: []
    openai:
      stages:
        design:    {choice: astra, confidence: 0.49, probabilities: {astra: 0.71, none: 0.01, sol: 0.27, terra: 0.01}}
        implement: {choice: terra, confidence: 0.80, probabilities: {astra: 0.0, sol: 0.14, terra: 0.86}}
        review:    {choice: sol,   confidence: 0.22, probabilities: {astra: 0.02, sol: 0.53, terra: 0.45}}
      class: astra
      close_calls: [review]
  depends_on:
  - ref: "#1129"
    state: open
    could_be_cheaper: {design: 0.75}
  reference_fetch_failures:
  - ref: "#404"
    error: not found
usage: {input_tokens: 1432, output_tokens: 61}
```

- **`providers`** holds one entry per requested ladder (default `anthropic`),
  keyed by that ladder's own name, each with its own `stages`, `class` and
  `close_calls`. A built-in ladder is keyed by its provider name; a custom
  one registered via `--ladder-definition` is keyed by the name it was given.
- **`class`** is the higher of the design and implement choices. The most
  capable class earns its cost in the design stage; once a plan exists, the
  design answer usually becomes `none` and implementation drops to a cheaper
  class. Review does not count towards the class.
- **`close_calls`** lists the stages whose confidence is below `--close-call`
  (default `0.3`, a working heuristic that has not been validated). Jev is
  noisy on close calls: identical inputs changed the design answer in 2 of 19
  cases, so treat a flagged stage as a judgement call, not an answer.
- **`depends_on`** lists every issue or pull request the text cites (via the
  same citation regex `verify-decision` uses, see below) that is still
  **open**; a closed citation is settled and is not reported. `ref` is the
  citation exactly as written (`"#1129"`, `"PR #1629"`, `"owner/repo#42"`),
  not a reconstructed `owner/repo#N`. `could_be_cheaper.design` is the
  probability, from one extra `noul` Jev call per open citation, that
  resolving it would leave **less design work** remaining than the text
  implies — as opposed to this issue's own remaining work being unaffected
  (already scoped separately, a parallel/sibling effort, or not a
  precondition). Only the `design` stage is asked for v1; `implement` is
  deferred pending the same kind of validation `design` got (see
  [#1812](https://github.com/rust-works/omni-dev/issues/1812)). There is no
  suppression threshold: every open citation gets a score, unfiltered.
- **`reference_fetch_failures`** lists citations GitHub could not fetch, so a
  stale or mistyped reference is visible rather than being indistinguishable
  from no citation. Each entry preserves the citation's original `ref` and
  reports its `error`; it is not included in `depends_on` because its state is
  unknown. It remains visible if the Jev request fails; the field is omitted
  when every cited reference resolved.
- **`truncated: true`** appears when the issue was longer than
  `--max-input-chars` (default 60,000 characters). The first and last halves
  are kept, with a visible `[... truncated]` marker between them, and a
  warning is logged. The end is kept on purpose: it holds the latest comments,
  where a decision comment lands, and that is what moves the design stage
  most. The longest input tested was about 48,000 characters, so no tested
  input was ever cut and this policy is itself untested; Jev's own input limit
  is undocumented.
- **`model`** and **`usage`** are summed over every call and never stripped.
- **`error`** replaces `providers` and `depends_on` on an issue whose Jev
  call failed (after the usual 429/529 retries) or whose answer was unusable
  for any requested provider. The other issues are still routed, so a long `--all-open` run
  keeps the answers already paid for; the command prints the whole report and
  then exits non-zero, naming how many issues failed. An authentication
  failure (HTTP 401 or 403) would fail every issue, so it stops the run at
  once instead.

### Text output

`-o text` renders the same report as one block per issue, blank-line
separated, in request order, followed by a line reporting `model` and the
summed `usage` — for reading, not scripting. Plain text with no markdown: a
terminal doesn't render `**bold**`/`*italic*` markers, so they'd just be
clutter. Each issue is a header line (`ref — title`) followed by one indented
line per fact, rather than one run-on sentence — which keeps a
multi-`--ladders` issue readable. Its wording is **not** a stable contract
and may change without notice; scripts should keep using `json` or `yaml`.

```
rust-works/omni-dev#1641 — Some issue title
  fable — design needs fable (0.52), implementation sonnet (0.83), review opus (0.41, close call)
  cites open #1129, which could leave less design work if resolved (0.75)
  reference fetch failed: #404 (not found)

model: jev-1.13.0, usage: 1432 input tokens, 61 output tokens
```

Routing against several `--ladders` adds one indented line per ladder,
labelled `<ladder>: <class> — ...` (a single ladder, the common case,
drops the label, as above). A failed issue's line reads `  failed: <error>`
instead of a routing. A truncated issue gets a trailing `  input truncated at
<N> characters` line; an issue with no open citations has no `cites` line.
An issue that cites more than one open item gets one `cites` line per
citation, in citation order, rather than one line joining every clause with
`; ` ([#1849](https://github.com/rust-works/omni-dev/issues/1849)):

```
rust-works/omni-dev#1845 — feat(drive): randomizeRange for drive sheets (#1830)
  fable — design needs fable (0.42), implementation sonnet (0.66), review opus (0.36)
  cites open #1830, which could leave less design work if resolved (0.46)
  cites open #1831, which could leave less design work if resolved (0.52)
```

When any stage's chosen tier name is multi-model (a custom ladder's
comma-joined tier name, [#1826](https://github.com/rust-works/omni-dev/issues/1826)),
the compact line above would repeat that name up to four times and become
unreadable, so the block switches to one line per fact instead
([#1847](https://github.com/rust-works/omni-dev/issues/1847)): `class:` once,
then `design:`/`implementation:`/`review:` each on their own line, with the
close-call marker still per stage:

```
rust-works/omni-dev#1832 — feat(drive): banded ranges for drive sheets (#1830)
  class: global.anthropic.claude-sonnet-4-6,global.anthropic.claude-sonnet-5
  design: needs no further work (0.94)
  implementation: global.anthropic.claude-sonnet-4-6,global.anthropic.claude-sonnet-5 (0.94)
  review: global.anthropic.claude-sonnet-4-6,global.anthropic.claude-sonnet-5 (0.70)
  cites open #1830, which could leave less design work if resolved (0.48)
```

With more than one `--ladders` requested, a multi-model ladder's block is
headed by its provider name (`  anthropic:`, with the fact lines nested one
indent level deeper) rather than prefixing every line, and the layout is
decided per ladder — one multi-model ladder does not force another,
single-model ladder into the block form too. Single-word tiers (every
built-in ladder) keep the compact line unchanged, with no output churn.

### What Jev sees

Only the issue's title, body and **human** comments, oldest first. Bot
comments (anything GitHub reports as a `Bot`, or whose login ends in `[bot]`)
are dropped, and only the latest 100 comments are fetched. The format is the
one the evidence below was gathered with, with one difference: the evaluation
script kept bot comments.

```
# #<N> <title>

<body>


---

## Comments on this issue


**Comment by <author>:**

<comment body>
```

Referenced issues and pull requests are **not** included. In the experiments
they cost about twice the tokens, made agreement worse (12 of 16 fell to 9),
and inflated small issues: an issue asking to update two constants was routed
to the top class once it carried the whole of the issue it referenced. If an
issue depends on a decision made elsewhere, write the decision into the issue
as a comment (see below) rather than relying on the reference.

`depends_on` scans this same text for citations — it does not fetch or send
the cited issue's content, only its open/closed state, so it costs a `gh`
lookup, not extra tokens in the routing call itself.

### Closed issues

`route` refuses a closed issue unless `--allow-closed` is given, because its
comments often describe how the work was actually done, which leaks the
answer. `--allow-closed` is for evaluation runs against issues whose outcome
you already know.

### Built-in ladders

`--ladders <NAMES>` is a comma-separated list of ladder names to route
against (default `anthropic`; an unknown name is an error listing the known
built-in and `--ladder-definition`-registered ones). A built-in name selects
one of three embedded three-rung ladders, least capable first, named by the
**abbreviated model name** so a consumer that drives that provider's agents
gets a model to hand the work to rather than a rung it has to translate.
There is deliberately no provider-neutral rung name.

| Provider    | Rungs, least capable first          | Expansions                                                          |
|-------------|-------------------------------------|---------------------------------------------------------------------|
| `anthropic` | `sonnet` / `opus` / `fable`         | Claude Sonnet / Claude Opus / Claude Fable                          |
| `openai`    | `terra` / `sol` / `astra`           | gpt-5.6-terra / gpt-5.6-sol / gpt-6-astra                           |
| `gemini`    | `flash` / `pro` / `deep-think`      | gemini-3-flash-preview / gemini-3.1-pro-preview / Gemini 3 Deep Think |

The ladders live in `src/templates/jev-route-tiers-<provider>.yaml`. Several
ladders still cost **one Jev call per issue**: Jev takes a map of questions
over one state, so the three stage questions are keyed per ladder name
(`openai.stage_design`, …) and share the issue text. `depends_on` is about
the issue, not a ladder, so its `could_be_cheaper` question is asked once
per open citation, not once per ladder.

Only the `anthropic` ladder was validated (see
[Evidence and its limits](#evidence-and-its-limits)). Because rewording a
description shifts answers across the board, the `openai` and `gemini`
ladders reuse the `anthropic` descriptions **rung for rung, byte for byte**;
only the tier names differ, and a test pins the equality. Two things are
therefore untested: the effect of the new tier names, which Jev sees as
criterion keys, and keying several ladders' questions into one call. Treat
a non-`anthropic` answer as a starting point until an evaluation like
#1779's is repeated for it, and if `anthropic`'s answers in a multi-ladder
run drift from a single-ladder run, route one ladder per run instead.

A fourth, cheapest rung (Haiku 4.5, gpt-5.6-luna, gemini-3.1-flash-lite), an
effort/reasoning-level knob, and the Chinese-lab and open-weight providers are
out of scope: each changes the validated `anthropic` question or needs its own
validation.

### Custom ladders

`--ladder-definition NAME=FILE` registers a custom ladder under `NAME`, its
tiers loaded from `FILE`, so it can be routed alongside built-in ladders in
the same `--ladders` list (#1826):

```bash
omni-dev ai jev route '#1234' --ladders anthropic,mine --ladder-definition mine=my-tiers.yaml
```

```yaml
tiers:
  - name: small
    description: Reliable at executing a clear specification ...
  - name: large
    description: Strongest at open-ended design and research ...
```

A tiers file needs at least two tiers, unique non-empty names and
descriptions, and no tier named `none`, which is reserved for "no design work
remains". Rank comes from the order in the file.

A ladder name must be non-empty and match `[a-z0-9_-]+`; a built-in provider's
name (`anthropic`/`openai`/`gemini`) is reserved and cannot be redefined. A
`--ladder-definition` registered but never listed in `--ladders` is an error,
on the assumption that it is a typo'd `--ladders` entry rather than an
intentionally unused definition.

The `anthropic` descriptions and the three stage questions are the exact text
the evidence was gathered with. Every question ends with the same bar:
*"Choose the least capable class likely to complete this stage correctly with
no rework, about 9 times in 10. Judge the work that remains given the text, not
the size of the text."* Rewording shifts answers across the board: an earlier
"pick the cheapest class" pushed every answer down a tier, and adding an
advisory instruction moved answers even on inputs it was not aimed at. Re-run
the evaluation before trusting different wording or descriptions.

### Decision comments

Jev reacts to decisions stated **in the issue itself**. It cannot infer that a
referenced issue settled a question, even with that issue and its closing pull
request supplied. A comment that states how the open questions were resolved
lowered the expected design tier by one to two and a half tiers, while a
neutral comment of the same length, or a decoy "Decided:" comment settling only
irrelevant details, moved it by no more than about a quarter of a tier.
Write decisions like this:

```markdown
**Decision** (settled by #1614, closed by PR #1629):

- PR #1629 added the read-only `drive_sheets_info` and `drive_sheets_read` MCP tools.
- The Sheets write verbs stayed CLI-only and are tracked separately.

For this issue: expose only `drive_docs_info` and `drive_docs_read`. The mutating
verbs are out of scope here.
```

- Cite the item that settled the question.
- State what the cited item did as separate bullets, one fact each.
- Keep "for this issue" decisions separate from claims about the cited item.
- Say what remains open, if anything. The design stage stays at the middle
  class for work the decision does not cover, which is the desired behaviour.

Because Jev trusts such comments, a comment that overstates what was decided
would under-route the issue. After adding one, re-run `route` to check that the
design work has actually gone away.

### Evidence and its limits

The design comes from experiments on this repository's issues with
`jev-1.13.0` (September 2026), recorded in
[#1779](https://github.com/rust-works/omni-dev/issues/1779) together with a
script that reproduces them. Asking per stage agreed with hand-assigned labels
on 10 of 12 fresh issues, against never choosing the top class for a single
"which class should implement this?" question, and 3 of 12 for a rule built on
eight factual questions. On full issues with their comments, the input `route`
sends, agreement was 11–12 of 16.

The sets are small (9, 12 and 16 issues), come from this one repository, and
the labels are one person's judgement, not ground truth. A newer model behind
`jev-latest` needs re-checking: note the `model` in the output.

`depends_on`/`could_be_cheaper` come from a separate, smaller round of
experiments (9 citations total, this repository, `jev-1.13.0`, September
2026), recorded in
[#1812](https://github.com/rust-works/omni-dev/issues/1812). Two alternatives
were tried and dropped: a classification-*range* idea (no evidence it added
anything once `could_be_cheaper` exists per citation) and plain
deletion-ablation as the bearing signal (confounded whenever a citation rides
along with independent decision content — a citation to an already-closed
issue produced the *largest* measured shift of the set, because the sentence
also independently disposed of a separate open item). The self-report
question tracked a more expensive resolved-simulation check in all 4
validation cases and matched the author's own reading throughout, but there
has been no **forward** validation: no case yet where an open citation
actually resolved and `could_be_cheaper`'s prediction was checked against
what really happened. Treat the score as informative, not calibrated.

## verify-decision

Checks a decision comment — the kind `route`'s [Decision comments](#decision-comments)
section asks you to write — against the issues or pull requests it cites.
Jev trusts a decision comment; `verify-decision` is how you find out whether
it should. It uses **two** things: the configured [AI backend](ai-backends.md)
to split the comment into single factual statements, and Jev to check each
statement, one `noul` question per statement, against the source it names.

```bash
omni-dev ai jev verify-decision '#1641'
omni-dev ai jev verify-decision rust-works/omni-dev#1641 --comment 1234567890 -o yaml
```

By default it checks the issue's most recent comment that cites another
issue or pull request; `--comment ID` (a numeric comment id) or a full
`...#issuecomment-<id>` URL selects a specific one instead.

```yaml
issue: rust-works/omni-dev#1641
url: https://github.com/rust-works/omni-dev/issues/1641
comment: {id: 1234567890, author: newhoggy}
verdict: accepted
coverage: 0.93
sources:
- {source: "#1614 and PR #1629", items: ["#1614", "PR #1629"]}
statements:
- {text: "PR #1629 added a `drive_sheets_info` MCP tool.", source: "#1614", supported: 0.99}
- {text: "For this issue, only `drive_docs_info` and `drive_docs_read` are in scope.", source: null}
models: {jev: jev-1.13.0, ai: claude-sonnet-5}
usage: {jev: {input_tokens: 5120, output_tokens: 61}}
```

- **`verdict`** is `rejected` if any statement scores below `--reject-below`
  (default `0.3`); otherwise `accepted` if every statement clears
  `--threshold` (default `0.5`) and coverage did too; otherwise
  `needs_review` — an uncertain statement, a citation that could not be
  resolved, low coverage, or a comment that cites nothing verifiable at all
  (in which case no AI or Jev call is made). `reasons` explains every
  contributing statement or coverage score. **Rejection is the validated
  half of this command**: checking one statement at a time against its
  source accepted 5 of 5 accurate claims and 0 of 20 wrong ones in the
  #1779 experiment, where checking a whole comment at once let
  overstatements through (a false claim scored 0.65–0.69 because the source
  merely *mentioned* the topic).
- **`coverage`** guards against a splitter that drops or strengthens a
  claim: a separate Jev call asks whether the split statements together say
  everything the comment claims. **A coverage failure alone only downgrades
  the verdict to `needs_review`, never to `rejected`**, because a failure
  says the *split* is suspect, not that the comment is false — see
  [Coverage: why it never rejects](#coverage-why-it-never-rejects).
- **A statement with `source: null`** is a claim about the judged issue
  itself (`cites: null`) rather than about a cited item — reported, but
  never checked, since there is nothing external to check it against.
- **`sources`** lists every cited issue merged with the pull requests that
  closed it (named `"#1614 and PR #1629"`), or a standalone cited pull
  request (named `"PR #1629"`) if it did not close any cited issue. A
  source that resolved but that no statement was attributed to is listed
  too, so it is distinguishable from a citation that never resolved. A
  source's text is truncated the same way `route`'s issue text is (see
  [route](#route)); an `error` field replaces a source's check when its Jev
  call failed. A source that could not be checked leaves its statements
  unchecked, which is itself a `needs_review` reason — a partly checked
  comment is never `accepted`.
- **`models`** reports the Jev model and the AI backend model. Either is
  omitted when that model never answered, as happens when the comment
  cites nothing verifiable and no call is made at all. **`usage`** reports
  only Jev's token counts (`usage.jev`): no AI backend in this project
  reports token counts today, only cost, so there is no `usage.ai` to show.

### What the splitter and Jev see

The AI backend receives the comment and every citation `verify-decision`
found in it (`#N`, `PR #N` / `pull request #N`, `owner/repo#N`, or a full
GitHub issue/pull URL). A number that runs straight into a letter, digit,
`_` or `-` is **not** a citation, so a hex colour (`#1f77b4`) or a heading
anchor (`#1-overview`) is left alone, and any other `http(s)://` link is
skipped whole, so a path or fragment inside it (`docs/jev.md#4-state-input`)
is never read as one. The backend is asked to split the comment into
statements that each name one item exactly as the comment names it,
keeping the original wording's certainty ("was decided" and "was
considered" are different facts) and adding nothing the comment does not
say. The prompt also tells the backend which issue is being judged and
what shape to reply in; it is new to #1779, not validated the way
`route`'s questions were.

**No JSON schema is attached to this call**, unlike every other structured
call omni-dev makes. Attaching one made the default model degenerate:
running the issue's 25 claim comments twice, the schema-enforced call
returned unusable output (`placeholder`, or several statements run together
into one) in 22 of 50 runs, against 0 of 50 for the same prompt with no
schema. The reply shape is therefore described in the prompt instead, and
the parser accepts JSON or YAML, with or without a code fence.

**`verify-decision` is not adversarial-resistant.** The comment's own text
goes into the splitter's prompt, so a comment written to manipulate that
prompt could in principle steer how it is split, and a statement the
splitter never produces is a statement Jev never checks. The coverage
check makes that harder rather than impossible. Treat the command as a
guard against honest mistakes and overstatements, not against a
deliberately hostile comment author.

Each cited source is checked separately. For a cited issue, Jev sees the
issue's title, body and human comments, followed by the body of every pull
request that closed it:

```
## SOURCE: #1614 and PR #1629

# Issue #1614: <title>

<body>

**Comment by <author>:**

<comment body>


# Pull request #1629: <title>

<pull request body>
```

This exact format — including the single blank line before a comment,
different from `route`'s two-blank-line separator — is the one validated in
#1779's per-statement experiment. A standalone cited pull request (one that
did not close any cited issue) uses the same `## SOURCE:` wrapper around
just its own heading and body; that shape was not exercised by the
experiment.

### Thresholds

| Flag | Default | Meaning |
|---|---|---|
| `--threshold` | `0.5` | A statement at or above this is supported. |
| `--reject-below` | `0.3` | A statement below this rejects the comment. |
| `--coverage-threshold` | `0.5` | Coverage below this adds a review reason. |

The two statement thresholds come from the experiment: true statements
scored 0.83 or higher, false ones 0.34 or lower, with a clean gap between
them at any threshold from 0.3 to 0.5.

### Coverage: why it never rejects

A failed coverage check means the *statements* do not match the comment.
That points at the split, not at the comment, so it can only ever mark a
comment `needs_review`. Rejecting on it would throw out correct comments:
across 200 live runs of the 25 claim comments, coverage failed 25 times and
**8 of those were on accurate comments**, every one caused by a bad split
rather than a false claim.

It is still worth running, because it is the only guard against a splitter
that quietly weakens a claim. Measured on hand-built splits, two runs each:

| What the split did to the comment | Coverage score | Caught at 0.5 |
|---|---|---|
| Nothing (faithful, or merely reordered) | 0.56–0.83 | — (correctly passes) |
| Added a claim the comment never made | 0.06–0.10 | yes |
| Made a claim stronger | 0.06–0.22 | yes |
| Made a claim weaker | 0.30–0.44 | yes |
| Dropped a claim | 0.32–0.77 | 3 of 5 |

The weakening row is the one that matters: when the splitter softens an
overstatement into something the source really does support, the
per-statement check passes it (0.52–0.98) and only coverage objects. The
dropped-claim row is why the check cannot be trusted to reject on its own.

### Evidence and its limits

The per-statement check is validated against five sources and 25
constructed claims (five accurate, twenty wrong in four different ways) in
[#1779](https://github.com/rust-works/omni-dev/issues/1779). The splitter
and coverage prompts were then tuned against that same set, run through
this command end to end (25 comments × 2 runs × 4 variants): the shipped
wording accepted 10 of 10 accurate comments and accepted 0 of 40 wrong
ones, with no degenerate splits. Note that this tunes the prompt on the
set it is measured on, so it shows the wording is not broken rather than
that it generalises. Cross-repository citations and standalone
pull-request sources are still unexercised; treat those as reasonable
defaults, not measured ones.

## Ordering caveats

Every map in the request and the response is kept **sorted by key**. This
makes the output byte-stable from run to run, which scripts and snapshot tests
rely on. The cost is that insertion order is never preserved:

- **`choice` options are alphabetised.** Both the request's `criteria` and the
  answer's `probabilities` list options by name, not in the order you passed
  `--option` (or wrote them in an `ask` file). If option order matters to how
  Jev reads the question, spell that out in `--instructions`.
- **`score` keys sort as strings, not numbers.** `legend` and `probabilities`
  are keyed `"0"`, `"1"`, `"2"`, and so on, in lexicographic order. With 11 or
  more levels, `"10"` would sort before `"2"`. `jev-1.13.0` accepts at most 10
  levels (keys `"0"` to `"9"`), so today the string order and the numeric order
  agree; this only bites if a later model raises that cap. The `--level` order
  you gave is always what is *sent*, because the request's `criteria` is an
  ordered list, so this only affects how the answer maps are displayed. To be
  safe against a future cap, sort numerically when consuming them, e.g.
  `jq '.answer.probabilities | to_entries | sort_by(.key | tonumber)'`.
- **`ask` answers are ordered by question name**, not by their order in the
  questions file.

## Best practices

The sections above cover how to *call* Jev. This one covers how to call it
*well*: how to phrase a question, what to put in the state, and how to act on
the answer. Most of it comes from TypeSafe's own documentation for
`jev-1.13.0`. Jev is a young product, so check the
[TypeSafe docs](https://docs.typesafe.ai) again when the model version you use
changes.

### Write questions Jev can take literally

Jev answers the question you wrote, not the one you meant. Scoping words,
negations and implied conditions are read at face value, so spell out every
condition, including the boundary cases.

- **Put the whole question in `--instructions`.** Don't rely on an option name
  or a question name to carry meaning.
- **Keep `--instructions` and the criteria consistent.** If the two ask for
  different things, answers get worse.
- **Give `choice` a way out.** Without an escape option, an input that fits
  nothing is forced into the nearest wrong label.
  Add one whenever your list might not cover every input:

  ```bash
  omni-dev ai jev choice "Do you ship to Iceland?" \
      --instructions "Which team should handle this ticket?" \
      --option billing="Payments, refunds, invoices" \
      --option technical="Bugs, outages, errors" \
      --option other="None of the above fit this ticket"
  ```

- **Pass the full list of options, not a shortlist.** Each option costs only a
  few tokens.
- **Describe `score` levels as situations, not degrees.** "Broken feature, but a
  workaround exists" gives Jev something to match the state against.
  "Moderately severe" does not.
- **One dimension per question.** A `score` that mixes, say, urgency and
  politeness has no right answer when the two disagree.

### Decompose, then combine in code

Don't hide several judgments inside one fuzzy question such as "how important
is this ticket?". Ask each part as its own question in one `ask` file, then
combine the answers with weights you control:

```yaml
# priority.yaml
urgency:
  type: score
  instructions: How soon does the customer need this resolved?
  criteria:
    - No deadline mentioned
    - Wants it this week
    - Blocked right now, business impact stated
angry:
  type: noul
  instructions: Is the customer angry?
```

```bash
omni-dev ai jev ask --questions priority.yaml < ticket.txt \
    | jq '0.7 * (.answers.urgency.score / 2) + 0.3 * .answers.angry.noul'
```

Each question stays narrow enough to answer well. Changing the weights is then
a code change you can test, not a prompt rewrite.

### Keep the state small and relevant

Accuracy drops as the state fills with material that has nothing to do with
the question, because irrelevant detail distracts the model. Retrieve and
filter in code first, and send only the fields the questions need. Prefer a
structured object with named fields (`--state-json`) over one large string, so
each part of the state has a name the instructions can refer to.

The request limits for `jev-1.13.0` are:

| Limit                                 | `jev-1.13.0`                   |
|---------------------------------------|--------------------------------|
| Whole request (state + all questions) | 64k tokens                     |
| State + the longest single question   | 32k tokens                     |
| `choice` options                      | up to 255                      |
| `score` levels                        | 2 to 10                        |

The token limits come from TypeSafe's
[Models](https://docs.typesafe.ai/models) page, which is the one to check for
the version you use. TypeSafe's
[Primitives](https://docs.typesafe.ai/primitives) page gives a lower figure,
about 32k tokens for the whole request, so treat 32k as the safe budget until
the two agree.

omni-dev checks only the minimums locally (two options, two levels). It does
not check the token limits or the 255-option and 10-level caps, so the API is
what rejects an oversized request. If a `choice` needs more than 255 options,
split it into two stages: first choose a group, then choose within that group.

### Leave exact work to code

Jev makes judgments. It does not calculate, and it does not write text. For
`jev-1.13`, TypeSafe lists these as unreliable:

- **Counting**: characters, occurrences of a term, or items in a long list.
- **Dates**: which of two dates comes first, how far apart they are, or whether
  one falls inside a window.
- **Arithmetic** of any kind.
- **Generating or extracting text**: summaries, replies, or pulling a value out
  of free text.

The pattern is to let Jev turn the input into bounded, typed fields and let
code do the exact work. For example, ask a `choice` over the twelve months
instead of asking whether an invoice is overdue, then compare dates in code.
To extract a value, find candidates with a regex (or a chat model) and let Jev
pick between them.

### Batch related questions into one `ask`

Jev answers every question in a request in parallel, so adding a question
usually adds no latency. It adds tokens, which you pay for and can see in
`usage.input_tokens`. Several single-question calls about the same state each
pay the full round trip and send the state again.

So when a script needs several judgments about one state, put them all in one
`ask` file, including ones it might not end up using. This reverses the usual
habit of making the cheapest call first and the rest only when needed. It also
keeps you further from the rate limit (see
[HTTP 429 / 529](#http-429--529-retries-exhausted)).

### Gate actions on confidence

A single global cutoff is the wrong tool. Set a threshold for **each action**,
based on what it costs to get that action wrong. Applying a label that a person
reviews anyway can act on a modest confidence. Refunding money or closing a
ticket should demand a high one. Below the threshold, escalate instead of
acting:

```bash
answer=$(omni-dev ai jev ask --questions triage.yaml < ticket.txt)
if jq -e '.answers.department.confidence >= 0.9' <<<"$answer" > /dev/null; then
    auto_route "$(jq -r .answers.department.choice <<<"$answer")"
else
    send_to_human
fi
```

A `noul` answer has no `confidence` field. Gate on the probability itself,
at both ends: `noul >= 0.9` to act as if true, `noul <= 0.1` to act as if
false, and escalate anything in between.

A flat distribution (a near 50/50 `choice`, or a `score` spread over several
levels) usually means the *question* is ambiguous: the options overlap, the
levels mix several dimensions, or the state lacks the facts. Reword the
question before you lower a threshold to live with it.

### Cascade: filter, judge, escalate

Jev is cheap and fast enough to put in front of expensive work, not in place of
it:

1. **Filter.** Use a `noul` per candidate (a retrieved passage, a log line, a
   file) as a relevance check, and send only the survivors on.
2. **Judge.** Ask the real questions in one `ask` call.
3. **Decide in code.** Keep control flow, deterministic rules and side effects
   in your own code, not in the question.
4. **Escalate.** Send only the low-confidence or flagged cases on to a full
   chat model or to a person. Anything that needs prose goes there too.

### Treat untrusted state as adversarial

TypeSafe notes that content written to steer the model can move the answer.
State built from content users control (email bodies, ticket text, PR
descriptions) can argue for its own classification, just as it can inject
instructions into a chat model.

Before a pipeline acts on Jev's output automatically (auto-labeling,
auto-routing, auto-closing), test it with crafted inputs that try to push the
answer you would least want, and keep a confidence gate in front of every
consequential action. Also remember that the state is part of the request
body, so it appears in the [request log](#request-log) if you enable
`OMNI_DEV_LOG_BODIES`.

### Check calibration on your own data

Jev's confidence is TypeSafe's own metric, and TypeSafe's calibration claims
are its own measurements. Calibration also holds across a group of answers,
not for any single one, so a 0.95 answer can still be wrong.

Before trusting a threshold for a consequential action, run Jev over a set of
examples from your own data whose answers you already know. Then check that
high-confidence answers really are right more often. Pin the model with
`--jev-model` while you do (see [Choosing a model](#choosing-a-model)), so a
`jev-latest` release cannot move the numbers underneath your thresholds.

## Retries and timeouts

Jev signals throttling with HTTP **429** and overload with HTTP **529**, and
omni-dev retries both automatically:

- Up to **3 retries** per request (4 attempts in total).
- The backoff delay comes from, in order of preference:
  1. The `Retry-After` response header.
  2. The `X-RateLimit-Reset` response header.
  3. An exponential fallback, `2 ^ (attempt + 1)` seconds.
- Each retry logs to stderr: `Rate limited (529). Retrying in {N}s (attempt {K})...`

No other status is retried. A 401 or 422 fails on the first attempt.

Jev uses the REST-client timeouts shared with Atlassian, Datadog and Gmail,
**not** the AI backends' 300 s `OMNI_DEV_AI_TIMEOUT_SECS`:

| Variable                             | Purpose                                    | Default |
|--------------------------------------|--------------------------------------------|---------|
| `OMNI_DEV_HTTP_CONNECT_TIMEOUT_SECS` | Connect phase (TCP + TLS handshake).       | `10`    |
| `OMNI_DEV_HTTP_READ_TIMEOUT_SECS`    | Each individual read of the response body. | `120`   |

Both take whole seconds. A missing, non-numeric or non-positive value falls
back to the default. Both can also be set in the `settings.json` `env` map.

## Request log

Every Jev call, including each retry attempt, is recorded in the local
[request log](log.md) with `service: jev`, like any other HTTP call omni-dev
makes:

```bash
omni-dev log --query 'service:jev' --limit 5
```

Request/response headers and bodies are **not** logged unless you opt in with
`OMNI_DEV_LOG_HEADERS` / `OMNI_DEV_LOG_BODIES`. Even then, the `Authorization`
header is redacted centrally, so the API key never reaches the log. The state
you send *is* part of the request body, so think before enabling
`OMNI_DEV_LOG_BODIES` for sensitive state.

## Troubleshooting

### Credentials not configured

```
Error: Jev credentials not configured. Set TYPESAFE_API_KEY (or OMNI_DEV_JEV_API_KEY), or add one to ~/.omni-dev/settings.json
```

Neither `TYPESAFE_API_KEY` nor `OMNI_DEV_JEV_API_KEY` was found in the process
environment or in the active `settings.json` `env` map. An empty value counts
as missing. If you use profiles, remember that a selected profile's `env` map
**replaces** the base one: a key stored only in the base map is invisible
under `--profile work`.

### HTTP 401: bad or missing API key

```
Error: Jev API request failed: HTTP 401: <body>
```

The key was sent but rejected. It may be revoked or mistyped. Check which
variable is actually winning: `TYPESAFE_API_KEY` beats `OMNI_DEV_JEV_API_KEY`,
and an exported variable beats `settings.json`. If you set
`OMNI_DEV_JEV_BASE_URL`, also check that it points at a server that accepts
this key.

### HTTP 422: invalid request

```
Error: Jev API request failed: HTTP 422: <body>
```

Jev understood the request but rejected its contents. omni-dev validates only
what it can see locally: option names and count, level count, the shape of an
`ask` file, and the `--state-json` type. Check the things it cannot: the model
name passed to `--jev-model` or `TYPESAFE_MODEL`, and the wording inside each
question spec. The error body is undocumented upstream and is passed through
as-is, so read it for specifics.

### HTTP 429 / 529: retries exhausted

```
Error: Jev API request failed: HTTP 529: <body>
```

omni-dev already retried 3 times (see
[Retries and timeouts](#retries-and-timeouts)). Wait and re-run. When running
a batch, prefer one `ask` call with several questions over several
single-question calls: it is one request instead of many.

### Unrecognised answer

```
Error: Jev API returned answers this version of omni-dev cannot read: "sentiment" (type "range": unknown variant `range`, expected one of `choice`, `score`, `noul`); if the type is new, try upgrading omni-dev
```

The error names every answer omni-dev could not read, together with its
`type`. The usual cause is a `type` other than `choice`, `score` or `noul`.
omni-dev is deliberately strict here rather than guessing at an unknown shape.
A new *field* on a known answer type, by contrast, is ignored without error.
If the named type is new, upgrade omni-dev.

When no individual answer is at fault (for example the body is missing
`answers`), the error is `Failed to parse Jev API response` followed by the
parser's reason instead.

### `--model`, `--ai-backend`, `--repo`, and other flags are rejected

```
error: unexpected argument '--model' found
```

Most Jev subcommands accept none of the AI backend flags (`--ai-backend`,
`--model`, `--beta-header`, `--claude-cli-*`, `--models-yaml`) — clap rejects
them outright rather than silently ignoring them (#1778) — and only `route`
and `verify-decision` accept `-C/--repo`. `verify-decision` is the exception:
it accepts the AI backend flags too, since it uses an AI backend to split a
decision comment into statements. Use `--jev-model` to choose the *Jev*
model on any subcommand; see [Choosing a model](#choosing-a-model). If
instead an exported `OMNI_DEV_MODEL` seems to have no effect on a Jev call,
that part is expected: see the footgun note above.

## See also

- [AI Backends](ai-backends.md): the chat-completion backends, which Jev is
  deliberately not one of.
- [Configuration Best Practices: Credential Profiles](configuration-best-practices.md#credential-profiles):
  the environment and `settings.json` precedence.
- [Request Log](log.md): querying and redaction.
- [Issue #1760](https://github.com/rust-works/omni-dev/issues/1760): the
  original request and API notes.
- [Issue #1779](https://github.com/rust-works/omni-dev/issues/1779): the
  experiments behind `route`, and a script that reproduces them.
- TypeSafe's own guidance, which [Best practices](#best-practices) draws on
  (as of `jev-1.13.0`, 2026-09-19):
  [Primitives](https://docs.typesafe.ai/primitives),
  [Confidence](https://docs.typesafe.ai/confidence),
  [How to build with System One](https://docs.typesafe.ai/concepts/how-to-build-with-system-one),
  [Jev 1.13 jaggedness](https://docs.typesafe.ai/model-jaggedness/jev-1.13)
  (its known weak spots) and [Models](https://docs.typesafe.ai/models) (limits).
