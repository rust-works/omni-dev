# Webhook Buffer Spike — Confirming the CI Signal Is Available

**Status:** Built — spike complete; **Go**. Live capture on `rust-works/omni-dev` (PR [#1383], 80 real events) confirms every needed field is obtainable and the rollup reconstructs to the true CI verdict.

**ADRs:** [ADR-0053](../adrs/adr-0053.md) · [ADR-0040](../adrs/adr-0040.md) · [ADR-0036](../adrs/adr-0036.md) · [ADR-0003](../adrs/adr-0003.md) · [ADR-0055](../adrs/adr-0055.md) (the #1384 read-token posture)

Spike for [#1378]. Follows the [#1377] cost spike ([`pr-poll-cost-spike.md`](pr-poll-cost-spike.md)),
which concluded the GraphQL poll is already cost-optimal and named webhooks the only
zero-cost real-time path — for **owned** repos only. Companion to the rate-limit monitor
([#1375]) and the per-repo polling toggle ([#1376]).

## The question this spike answers

Before building the [#1378] store-and-pull buffer (`GitHub → Cloudflare Worker + KV →
daemon pulls → PrStatusCache`), confirm the **premise**:

> Do GitHub's `check_run` / `check_suite` / `status` / `pull_request` webhook events
> actually carry **every field** [`PrStatusCache`](../../src/pr_status.rs) needs to mint
> a badge — keyed back to a `(owner, name, branch)` target — such that the buffer path
> can feed badges *at least as completely* as today's aliased GraphQL poll?

This is a **feasibility** spike, not an implementation. It does **not** build the daemon
pull loop, the config surface, or the production buffer. It builds only what is needed to
capture real webhook payloads from a repo you own and mechanically check the field
availability, so [#1378] starts from confirmed fact rather than assumption.

**Out of scope (deferred to [#1378] proper):** the daemon-side pull loop
(`start_pr_poller` sibling), the read-token secret posture / ADR, config resolution,
retention-vs-reconcile tuning, and any CLI surface. This spike touches **no Rust** and
**no daemon trust boundary** — it is a throwaway capture harness plus a documented-schema
analysis, both under [`deploy/webhook-buffer/`](../../deploy/webhook-buffer/).

## The information needed (derived from `pr_status.rs`)

The badge engine ([src/pr_status.rs](../../src/pr_status.rs)) resolves, per
`(owner, name, branch)` target (a [`PrTarget`](../../src/pr_status.rs)), these fields —
the GraphQL query in [`build_query`](../../src/pr_status.rs) asks for exactly this set:

| # | Field (in `PrBadge` / `PrResolution`) | Today's GraphQL source                              | Used for                                         |
|---|---------------------------------------|-----------------------------------------------------|--------------------------------------------------|
| 1 | `owner`, `name`                       | query alias (`repository(owner:,name:)`)            | the target key                                   |
| 2 | branch → ref                          | `ref(qualifiedName:"refs/heads/<branch>")`          | keying the event back to a `PrWatch` target      |
| 3 | `head_oid`                            | `ref.target ...on Commit { oid }`                   | staleness ([`is_stale_for`](../../src/pr_status.rs)) — a push must invalidate the badge with no network call |
| 4 | `checks` (rollup verdict)             | `statusCheckRollup.contexts.nodes[]` → `CheckRun{status,conclusion}` / `StatusContext{state}` | the badge colour: Success / Failure / Pending / None |
| 5 | `number`                              | `associatedPullRequests(first:1,states:OPEN).number`| which PR the badge links                          |
| 6 | `is_draft` (`isDraft` on the wire)    | `associatedPullRequests…isDraft`                    | draft marker                                      |
| 7 | `url`                                 | `associatedPullRequests…url`                        | the "open PR" action (must be the **html** URL)   |

The rollup (#4) reduction the buffer must reproduce (see
[`rollup_check_state`](../../src/pr_status.rs) / `check_entry_state`): **failure dominates**;
else any **pending/unknown** → Pending; else Success; **no checks** → None. A `CheckRun`
whose `status != COMPLETED` is Pending regardless of conclusion. Canonical state sets
(upper-cased before lookup):

- **Failure:** `FAILURE`, `ERROR`, `CANCELLED`, `TIMED_OUT`, `ACTION_REQUIRED`, `STARTUP_FAILURE`, `STALE`
- **Success:** `SUCCESS`, `NEUTRAL`, `SKIPPED`
- anything else → **Pending** (never a false pass)

> **The crux:** GraphQL hands the daemon a *pre-rolled* `statusCheckRollup` for the ref's
> current commit in one shot. Webhooks instead deliver **per-check deltas** — one
> `check_run` per run, one `check_suite` per app-suite, one `status` per context — that the
> daemon must **aggregate itself**, keyed by `(repo, branch, head_sha)`, to reconstruct the
> same verdict. Confirming that reconstruction matches is the load-bearing part of this spike.

## Hypothesis — webhook-field → needed-field mapping

From GitHub's documented webhook payload schemas. `✅` = present and reliable; `⚠️` =
present but conditional/needs live confirmation; `❌` = absent. The **Confirmed?** column
is filled from live capture (see [Results](#results)).

| Needed field           | `check_run`                                    | `check_suite`                              | `status`                                  | `pull_request`                     | Confirmed? |
|------------------------|------------------------------------------------|--------------------------------------------|-------------------------------------------|------------------------------------|:----------:|
| 1 owner / name         | ✅ `repository.owner.login` / `.name`          | ✅ same                                    | ✅ same                                   | ✅ same                            | ✅ live       |
| 2 branch (ref)         | ⚠️ `check_run.check_suite.head_branch` (nullable) | ⚠️ `check_suite.head_branch` (nullable) | ⚠️ `branches[]` (only refs whose head **is** this sha) | ✅ `pull_request.head.ref`      | ✅ **not null** |
| 3 head oid             | ✅ `check_run.head_sha`                         | ✅ `check_suite.head_sha`                  | ✅ `sha`                                  | ✅ `pull_request.head.sha`         | ✅ live       |
| 4 check state          | ✅ `status` + `conclusion` (per run)           | ✅ `status` + `conclusion` (per **suite**, one app) | ✅ `state` (per context)         | ❌                                 | ✅ live²      |
| 5 PR number            | ⚠️ `check_run.pull_requests[].number` (empty for forks) | ⚠️ `check_suite.pull_requests[]`  | ❌                                        | ✅ `number`                        | ✅ live¹      |
| 6 PR isDraft           | ❌                                             | ❌                                         | ❌                                        | ✅ `pull_request.draft`            | ✅ PR-event only |
| 7 PR url (html)        | ⚠️ `pull_requests[].url` is the **API** URL, not `html_url` | ⚠️ same                       | ❌                                        | ✅ `pull_request.html_url`         | ✅ PR-event only |

Live capture: 80 relevant events on `rust-works/omni-dev` (PR [#1383]) — 73 `check_run`, 5 `check_suite`, 2 `pull_request`; **0 `status`**.
¹ PR number present on PR-branch check events (72/73); the one `n/a` is a push to `main` (no associated PR — correct).
² `status` events were **not emitted** — this repo's CI uses the Checks API (`check_run`/`check_suite`), not legacy commit statuses, so the `status` column stays at documented-schema confidence.

### What the hypothesis already tells us (the likely findings to confirm)

1. **No single event type is sufficient.** The CI events (`check_run` / `check_suite` /
   `status`) carry the *verdict* (#4) plus repo + sha + branch, but **not** the PR draft
   flag (#6) and not an `html_url` (#7). The `pull_request` event carries the PR metadata
   but **no** CI verdict. So the daemon must either (a) **merge** both streams keyed by
   `(repo, branch, head_sha)`, or (b) let the retained **reconcile GraphQL poll** fill
   `is_draft` / `url` while webhooks drive the fast check-state transitions. Deciding (a)
   vs (b) is a [#1378] design output; this spike just confirms the split is real.

2. **The rollup must be reconstructed from deltas.** No event carries a
   `statusCheckRollup`. The daemon must keep per-`(repo, branch, head_sha)` accumulator of
   the latest state per check name/context and reduce it with the same failure-dominates
   logic. The spike verifier does exactly this and prints the reconstructed verdict for
   eyeball comparison against `gh pr checks` / the GraphQL poll.

3. **Two nullability risks, both mitigated by the owned-repo scope but worth confirming:**
   - `head_branch` is `null` on `check_run`/`check_suite` for cross-repo (fork) PRs. Scope
     is repos you own and push branches to directly, so it *should* be populated — confirm.
   - `pull_requests[]` on check events is empty for fork PRs and can lag PR creation.
     `status` events carry no PR link at all. If PR association off the CI event proves
     unreliable, the `pull_request` event (or the reconcile poll) is the authority for #5–7.

4. **`html_url` is not on the CI events.** `check_run.pull_requests[].url` is the REST API
   URL. The badge's open action needs the browser URL, so #7 comes from the `pull_request`
   event or the reconcile poll — reinforcing finding 1.

If live capture upholds the ✅/⚠️ above, [#1378] is feasible with a **CI-events-drive-state,
`pull_request`-event-plus-reconcile-poll-fill-metadata** design. A ❌ where the table says
✅/⚠️ is a stop-and-rethink.

## Method — how to run the spike

Infrastructure is under [`deploy/webhook-buffer/`](../../deploy/webhook-buffer/). Full
runbook in its [`README.md`](../../deploy/webhook-buffer/README.md); summary:

0. **Pre-flight, zero setup — dry-run the verifier against documented-schema fixtures:**

   ```
   cd deploy/webhook-buffer && node verify-fields.mjs --dir samples
   ```

   Confirms the analysis logic and prints the field matrix + reconstructed rollup for the
   canned payloads. This validates the *tooling*, not GitHub — the fixtures are hand-built
   from the documented schemas and are explicitly **not** evidence.

1. **Deploy the capture Worker** (needs a free Cloudflare account):
   `wrangler kv namespace create GITHUB_EVENTS`, set the `WEBHOOK_SECRET` and `READ_TOKEN`
   secrets, `wrangler deploy`. You get a public `https://…workers.dev/webhook` URL.

2. **Register the webhook on a repo you own** (needs repo admin):
   `gh api repos/<owner>/<repo>/hooks -f … events[]=check_run …` with the same secret.

3. **Trigger CI** — push a branch / open a PR / re-run checks so real `check_run`,
   `check_suite`, `status`, and `pull_request` deliveries land in KV.

4. **Pull and verify:**

   ```
   node verify-fields.mjs --url https://…workers.dev --token "$READ_TOKEN"
   ```

   The verifier pulls the buffered events, prints the **field-presence matrix** (which
   event types actually supplied each needed field, on *your* payloads), and the
   **reconstructed rollup** per `(repo, branch, head_sha)`. Copy its summary into
   [Results](#results) and fill the **Confirmed?** column above.

## Results

**Outcome: Go.** Captured live on `rust-works/omni-dev` via PR [#1383] — 80 relevant
deliveries (73 `check_run`, 5 `check_suite`, 2 `pull_request`; plus 1 `ping`), through the
deployed capture Worker, read back with `verify-fields.mjs`.

- **Field matrix (live):** repo, branch, and head_sha present on **100%** of `check_run`
  (73/73), `check_suite` (5/5), and `pull_request` (2/2). Check state present on every check
  event. PR number present on PR-branch check events (72/73) and both `pull_request` events.
  `isDraft` and the browser `html_url` present **only** on `pull_request` (check events carry
  the REST `api` url) — the metadata split is real and exactly as hypothesised.
- **Rollup reconstruction vs the real CI verdict:** the verifier rebuilt the badge verdict
  per `(repo, branch, head_sha)` from the raw per-check stream and matched the *actual* CI
  result at **both** commits of this PR — `dd15e4a1` (pre-fix) → **Failure** (18 Success + the
  one failed Commit Message Check), `3aec0a5b` (fixed head) → **Success** (19/19). A `main`
  push at `9fc5bb69` → Success (2). No false pass, no false fail.
- **`head_branch` populated on owned-repo pushes?** **Yes** — 73/73 `check_run` and 5/5
  `check_suite` carried the branch. The fork-nullability risk does not apply to owned,
  same-repo branches; field #2 resolves from ⚠️ to ✅.
- **`pull_requests[]` populated / timely on check events?** **Yes for PR-branch checks**
  (72/73 carried PR number + api-url). The single miss is the `main`-push check event, which
  correctly has no associated PR. But `isDraft` is never on check events, and the url there is
  the REST api url — so PR draft + browser url must come from the `pull_request` event (or the
  reconcile poll).
- **Deltas needed to rebuild the verdict:** ~19 `check_run`/`check_suite` deliveries per
  commit (each Actions job emits `queued`→`in_progress`→`completed`); reducing to the latest
  state per check name converges cleanly. `status` events were not emitted (Checks-API CI), so
  a repo relying on legacy status contexts stays at documented-schema confidence here.

**Design implication (confirmed):** the **CI-events-drive-the-verdict,
`pull_request`-event-(plus reconcile poll)-fill-the-PR-metadata** design is viable.
`isDraft` and `html_url` are the *only* needed fields absent from the CI stream, and both are
supplied by the `pull_request` event — which the capture confirmed arrives.

## Decision criteria

**→ Met: Go.** All seven fields are obtainable (with `isDraft`/`html_url` off the
`pull_request` event), and the reconstructed rollup matched the true CI verdict at both a
failed and a passing commit. [#1378] proceeds with the merge/fill design below.

- **Go** — every needed field (#1–7) is obtainable from the event stream (directly or via
  the `pull_request`-event / reconcile-poll fill), and the reconstructed rollup matches the
  GraphQL verdict on the captured runs. → [#1378] proceeds with the merge/fill design.
- **Partial** — CI verdict (#3, #4) reconstructs cleanly but PR metadata (#5–7) is
  unreliable off webhooks. → [#1378] proceeds, but the reconcile poll is *load-bearing* for
  metadata (not just a catch-up path), which changes the retention/cadence tuning.
- **No-go** — the rollup cannot be reconstructed to match (missing check names, no way to
  know the full expected check set per commit), so a badge would show a false pass/fail. →
  webhooks accelerate *notification* only; keep GraphQL as the verdict authority.

<!-- link references -->
[#1375]: https://github.com/rust-works/omni-dev/issues/1375
[#1376]: https://github.com/rust-works/omni-dev/issues/1376
[#1377]: https://github.com/rust-works/omni-dev/issues/1377
[#1378]: https://github.com/rust-works/omni-dev/issues/1378
