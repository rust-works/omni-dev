# PR-Poll GitHub API Cost Spike — Conditional Requests (ETags) vs Webhooks

**Status:** Aspirational — spike complete; recommends **no-go** on migrating the daemon's PR-badge poller from `gh api graphql` to REST + conditional requests. Records the cost reasoning so the decision is not re-litigated.

**ADRs:** [ADR-0053](../adrs/adr-0053.md) · [ADR-0050](../adrs/adr-0050.md) · [ADR-0040](../adrs/adr-0040.md) · [ADR-0003](../adrs/adr-0003.md)

Spike for [#1377]. Companion to the rate-limit monitor ([#1375]) and the per-repo
polling toggle ([#1376]).

## Recommendation (TL;DR)

**No-go** on replacing the single aliased `gh api graphql` poll with per-`(repo, ref)`
REST + `If-None-Match` conditional requests. **No-go** on the conditional Events-API
hybrid. **Keep** the current design ([ADR-0053](../adrs/adr-0053.md)). Treat webhooks
as a *future* direction for **owned** repos only, out of scope here.

The one-line reason: the current poller already batches **every** repo into **one**
request that costs **~1 point** and runs at **~2 points/hour** in steady state.
Conditional requests optimise a different axis — making each *per-resource* request
free when unchanged — which only pays off when the baseline is one-request-per-resource.
Swapping the aliased query for REST+ETags trades a non-problem (primary points) for a
real one (secondary/abuse limits and request count at ~29 repos), plus a non-trivial
implementation cost, for no meaningful steady-state saving.

The genuine levers on daemon GitHub cost are the two companion issues, not the transport:

- **[#1375]** — surface `GET /rate_limit` usage in `daemon status` (zero-cost visibility).
- **[#1376]** — per-repo polling toggle, so idle repos drop out of the target set entirely.

## The premise, corrected

[#1377] frames the poller as *"a background monitor paying full price to learn 'nothing
changed' the vast majority of the time."* That was true of the **pre-[#1370]** design —
the extension-side `gh pr list` storm that fired per *window × repo ×* 60 s and hit
thousands of points/hour ([#1375] measured ~6,000 pts/hr at 31 windows / 29 repos). That
design is **already gone**: [#1337] + [ADR-0053](../adrs/adr-0053.md) moved resolution
into the daemon as **one aliased GraphQL query**, and [#1370] made "checked, no PR" an
explicit terminal state so the fallback stops re-firing.

For the *current* design the premise no longer holds, for two reasons:

1. **Cost is invariant to repo/worktree/window count.** `repository(owner:,name:)` and
   `ref(qualifiedName:)` are not GraphQL *connections* (no `first:`/`last:`), so aliasing
   every `(repo, branch)` pair into one query is free-per-alias. Measured against
   `rust-works/omni-dev`: **1 point** up to ~50 branches, 2 at 100, 4 at 200
   ([ADR-0053](../adrs/adr-0053.md) §2, [src/pr_status.rs](../../src/pr_status.rs) header).
   31 windows do **not** multiply it — the daemon resolves once and fans out over the
   `subscribe` stream.
2. **Adaptive backoff already collapses steady-state polling.** The poller *wakes* every
   ~10 s (a cached snapshot read — no subprocess, no network) but only *fetches* when the
   watched state moved or the backoff elapsed; the backoff doubles from 10 s to a
   **30-minute ceiling** once every badge is terminal
   ([`next_pr_poll_delay`](../../src/daemon/services/worktrees.rs), `pr_should_fetch`).

So the poller does **not** pay full price to learn nothing changed. It pays ~1 point per
30 minutes when quiet, and only ramps to the 10 s cadence while CI is actually in flight.

## What conditional requests would actually change

ETags make an **unchanged** `(repo, ref)` request return `304 Not Modified`, and GitHub
documents that a 304 *"does not count against your primary rate limit."* Confirmed
firsthand this spike: a conditional `GET /repos/rust-works/omni-dev/commits/main/status`
with `If-None-Match` returned `HTTP/2.0 304` and left `core.used` unmoved. (Precise
single-call deltas were unreliable — `/rate_limit` reporting lags and the `gh` token is
shared across concurrent windows/agents on the machine — which is itself a caveat for the
[#1375] monitor, not a contradiction of the documented model.)

The catch is what conditional requests do **not** change:

- **They are still requests.** The 304 exemption is for the **primary** rate limit only.
  Every conditional request — 304 or not — still counts toward the **secondary** (abuse)
  limits: GitHub's *"no more than 100 concurrent requests,"* *"~900 points/minute for
  REST,"* and the explicit guidance to *"make requests serially, not concurrently."*
- **They are per-resource.** GraphQL has no ETag support, so gaining conditional requests
  means abandoning the aliased query and issuing one request **per `(repo, ref)`** — and,
  because REST splits what the GraphQL ref-node returns in one shot, roughly **two to
  three** per ref:
  - `GET /repos/{o}/{r}/commits/{ref}/status` — combined status (ETag)
  - `GET /repos/{o}/{r}/commits/{ref}/check-runs` — check runs (ETag)
  - `GET /repos/{o}/{r}/pulls?head={o}:{branch}&state=open` — PR association (number, url,
    draft; cacheable but required — GraphQL returns this off the `Ref` today)

### Batching vs conditional caching are mutually exclusive

This is the crux. The aliased GraphQL query and REST+ETags are **two different
optimisations of the same cost, and you cannot have both:**

| Optimisation                       | Mechanism                                          | Wins when…                                                            |
| ---------------------------------- | -------------------------------------------------- | --------------------------------------------------------------------- |
| **Batching** (today)               | one request bills for *all* resources              | one caller, many resources, request-count/secondary limits dominate   |
| **Conditional caching** (proposed) | each *per-resource* request is free when unchanged | baseline is already one-request-per-resource; primary points dominate |

The daemon's access pattern — **one caller, ~29 repos, mostly quiet** — is squarely in
the first column. Batching all repos into one billable request is strictly better here
than making each of ~80–120 per-resource requests individually cheap, because request
**count** (not primary points) is the scarce resource once you fan out.

## Cost comparison at ~29 repos / ~40 watched refs

Scenario matches the incident scale ([#1375]): ~29 repos, ~40 distinct `(repo, branch)`
targets from open worktrees.

| Dimension                                    | Current — `gh api graphql` (GraphQL budget)                                                                           | Proposed — REST + ETags (core budget)                                                                            |
| -------------------------------------------- | --------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------- |
| Requests per poll cycle                      | **1**                                                                                                                 | **~80–120** (2–3 per ref × 40)                                                                                   |
| Steady state (all terminal, nothing changed) | ~2 pts/hr (30-min ceiling)                                                                                            | ~0 primary pts (all 304) — but still ~80–120 req/cycle                                                           |
| Active CI (fast cadence, 10 s)               | 1–2 pts/poll × 360 = **360–720 pts/hr**                                                                               | only *changed* refs pay: ~1–15 pts/cycle → 360–5400 pts/hr on core                                               |
| Secondary-limit pressure                     | negligible (1 req/cycle)                                                                                              | **high** — ~80–120 serial req/cycle; at 10 s ≈ 480–720 req/min vs the ~900/min cap                               |
| Latency: local commit/push                   | immediate (OID-move trigger)                                                                                          | immediate (if the OID-move trigger is kept)                                                                      |
| Latency: remote-only change at ceiling       | up to 30 min                                                                                                          | up to the poll interval — but a fast interval is unsafe at this request count                                    |
| Budget consumed                              | `graphql` (5,000/hr)                                                                                                  | `core` (5,000/hr) — a *different* pool                                                                           |
| Implementation                               | **shipped**; shells `gh` (token never enters daemon, [ADR-0003](../adrs/adr-0003.md)/[ADR-0040](../adrs/adr-0040.md)) | new: direct `reqwest` HTTP + `gh auth token`, per-ref ETag store, PR-assoc lookups, request pacing/serialisation |

Reading the table:

- **Steady state:** no meaningful saving. The current design is already ~2 pts/hr; you
  cannot beat that by much, and the ETag version still issues ~80–120 requests per cycle
  against the secondary limit even when every one is a free 304.
- **Active CI:** REST+ETags *can* be cheaper on **primary points** (only changed refs
  pay), but only by moving the load onto the **secondary** limit — the exact trade
  [#1377] flags as a risk. At ~80–120 serialised requests per cycle, a 10 s cadence is not
  safely sustainable; you would have to slow down, which erases the latency benefit.
- **Budget pool:** the one real structural difference is that REST+ETags spends the
  **core** budget instead of **graphql**. That offloads the GraphQL pool for a user's other
  GraphQL work — but the poller's current GraphQL draw is already negligible (it was **not**
  the exhaustion driver; the pre-[#1370] extension design was), so there is little to offload.

## Webhooks (the only real "subscribe")

GraphQL has no subscriptions/server-push; GitHub's push model is webhooks — real-time and
**zero** rate-limit cost. They are the correct endgame for **owned** repos, but not a
drop-in for this daemon:

- Need a **public HTTPS endpoint**; a local daemon behind NAT needs a tunnel/relay.
- Need per-repo/org **admin** to install — unavailable on upstreams you only contribute to.
- Best delivered via a **GitHub App**, a materially larger surface than "shell out to `gh`."

**Verdict:** out of scope for this spike. Worth a *separate* future proposal for the
subset of repos the user owns and can install an App on, where it would eliminate polling
cost and latency together. It cannot cover the general "any repo I have a worktree for" case.

## Hybrid: conditional Events API as a free change-detector

`GET /repos/{o}/{r}/events` is ETag-optimised (304 free) with an `X-Poll-Interval` header,
proposed as a cheap gate before the expensive resolve. Rejected:

- **Wrong signal.** The public Events feed carries `PushEvent`/`PullRequestEvent` (CI
  *start*) but **not** `check_run`/`check_suite`/`status` (CI *completion*) — and CI
  completion is exactly the transition a badge exists to show.
- **Redundant with what we already have.** The push signal it *does* carry is one the
  poller already detects locally and for free, by watching each target's `head_sha` /
  `upstream_sha` OIDs off the snapshot ([ADR-0053](../adrs/adr-0053.md) §4). Events would
  only add *remote-origin* pushes (a collaborator's) — marginal value.
- **Same fan-out cost.** It is still one request per repo (~29/cycle) against the secondary
  limit, to detect strictly less than the current OID watch.

## Conditions that would flip this decision

- **GitHub adds conditional support to GraphQL.** Then batching *and* conditional caching
  combine instead of competing, and it becomes a clear win. Revisit.
- **The GraphQL pool genuinely binds** because of heavy *other* GraphQL usage on the same
  token, *and* repo count is small enough that per-ref REST stays under the secondary limit.
  Then offloading badges to the core pool via REST+ETags is worth reconsidering — but
  [#1376]'s per-repo toggle addresses the same pressure more cheaply first.
- **Owned-repo subset grows** enough to justify a GitHub App + webhook relay. Different
  project; see above.

## What to do instead

1. **Keep [ADR-0053](../adrs/adr-0053.md)'s aliased-query + adaptive-backoff design.** It
   is already cost-optimal for this access pattern.
2. **Ship [#1375]** — poll `GET /rate_limit` (itself exempt) and surface used-percentage in
   `daemon status`. Turns a silent drain into an observable trend; the spike's own
   measurement noise shows why a first-class view beats ad-hoc `gh api rate_limit`.
3. **Ship [#1376]** — per-repo polling toggle. Dropping idle repos from
   `pr_watch_from_snapshot` shrinks the target set (and removes their pending-cadence
   triggers), cutting even the current small draw where the user actually wants it cut —
   without changing transport.

None of these touches the trust boundary or the no-secret posture
([ADR-0040](../adrs/adr-0040.md)): the daemon keeps shelling out to `gh`.

## Appendix — cost model and assumptions

- **Primary rate limits (authenticated):** REST `core` = 5,000/hr; GraphQL = 5,000
  points/hr — **separate** resources tracked independently.
- **GraphQL point cost:** derived from `first:`/`last:` on *connections*; non-connection
  fields (`repository`, `ref`) are free to alias. Current query measured at 1 pt (≤50
  branches) / 2 (100) / 4 (200).
- **Conditional requests:** a `304 Not Modified` does **not** count against the **primary**
  limit (confirmed firsthand). It **does** still count as a request toward the
  **secondary** limits.
- **Secondary (abuse) limits:** ≤100 concurrent requests; ~900 points/min for REST (most
  GET = 1 point); ≤90 s CPU per 60 s real time; *make requests serially*.
- **Events API:** ETag'd + `X-Poll-Interval`; carries push/PR events, **not**
  check-run/suite/status.
- **Measurement caveat:** `/rate_limit` reporting lags and the `gh` token is shared across
  concurrent windows/agents, so single-call point deltas could not be measured cleanly this
  session. Figures above rest on GitHub's documented cost model plus the in-repo measured
  GraphQL cost; the 304-is-free property was confirmed directly.

### References

- [Best practices for using the REST API — GitHub Docs](https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api)
- [Rate limits for the GraphQL API — GitHub Docs](https://docs.github.com/en/graphql/overview/rate-limits-and-query-limits-for-the-graphql-api)
- [REST API endpoints for events — GitHub Docs](https://docs.github.com/en/rest/activity/events)

[#1337]: https://github.com/rust-works/omni-dev/issues/1337
[#1370]: https://github.com/rust-works/omni-dev/issues/1370
[#1375]: https://github.com/rust-works/omni-dev/issues/1375
[#1376]: https://github.com/rust-works/omni-dev/issues/1376
[#1377]: https://github.com/rust-works/omni-dev/issues/1377
