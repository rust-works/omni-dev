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
Its subcommands do not accept the AI backend flags (`--ai-backend`, `--model`,
`--beta-header`, `--claude-cli-*`, `--models-yaml`) — passing any of them is
now a clap error, "unexpected argument" — nor `--repo`: a Jev call never
investigates a repository, it judges only the `state` text you give it.
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
10. [Ordering caveats](#ordering-caveats)
11. [Best practices](#best-practices)
12. [Retries and timeouts](#retries-and-timeouts)
13. [Request log](#request-log)
14. [Troubleshooting](#troubleshooting)
15. [See also](#see-also)

## Prerequisites

- A TypeSafe account and a Jev **API key**. The same key works with
  TypeSafe's own SDKs, which read it from `TYPESAFE_API_KEY`.

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

Jev subcommands accept none of the AI backend flags (`--ai-backend`,
`--model`, `--beta-header`, `--claude-cli-*`, `--models-yaml`) or `--repo` —
clap rejects them outright rather than silently ignoring them (#1778). Use
`--jev-model` instead; see [Choosing a model](#choosing-a-model). If instead
an exported `OMNI_DEV_MODEL` seems to have no effect, that part is expected:
see the footgun note above.

## See also

- [AI Backends](ai-backends.md): the chat-completion backends, which Jev is
  deliberately not one of.
- [Configuration Best Practices: Credential Profiles](configuration-best-practices.md#credential-profiles):
  the environment and `settings.json` precedence.
- [Request Log](log.md): querying and redaction.
- [Issue #1760](https://github.com/rust-works/omni-dev/issues/1760): the
  original request and API notes.
- TypeSafe's own guidance, which [Best practices](#best-practices) draws on
  (as of `jev-1.13.0`, 2026-09-19):
  [Primitives](https://docs.typesafe.ai/primitives),
  [Confidence](https://docs.typesafe.ai/confidence),
  [How to build with System One](https://docs.typesafe.ai/concepts/how-to-build-with-system-one),
  [Jev 1.13 jaggedness](https://docs.typesafe.ai/model-jaggedness/jev-1.13)
  (its known weak spots) and [Models](https://docs.typesafe.ai/models) (limits).
