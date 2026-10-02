# GitHub conditional request evaluation

Evaluated for [#1814](https://github.com/rust-works/omni-dev/issues/1814) on
2026-10-03. **Decision: retain the current GraphQL data-fetch paths.** No current
consumer qualifies for a REST conditional-request implementation. Resolve the
evaluation as not applicable to current consumers and revisit it when a concrete
single-resource poller meets the criteria below.

This is an evaluation of cache validation, separate from GitHub App authentication
in [#1813](https://github.com/rust-works/omni-dev/issues/1813). It adds no client,
cache schema, dependency, or runtime behavior.

## What conditional requests buy

REST validators apply to the requested representation. An authenticated conditional
GET returning `304 Not Modified` avoids the primary REST charge; a changed response
still costs a request. Neither result eliminates HTTP latency, subprocess overhead,
or secondary-limit considerations. A validator for an issue does not establish
freshness of separately fetched comments, checks, reviews, or Projects-v2 items.
See [GitHub's REST best practices](https://docs.github.com/en/rest/using-the-rest-api/best-practices-for-using-the-rest-api#use-conditional-requests).

The existing `gh api graphql` queries are POST requests with query bodies and have
no REST conditional-GET validation path. Adding `If-None-Match` to the shared
GraphQL runner would not provide that mechanism. REST and GraphQL have separate
primary budgets, so reducing REST charges alone is not evidence of lower total
cost; compare round trips, response coverage, and both budgets.
See [GitHub's rate-limit documentation](https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api).

## Current consumers and candidate outcomes

- **Issue documents and comments:** [`github_issues.rs`](../../src/github_issues.rs)
  uses repository-grouped aliases in batches of at most 25 references. Full issue
  documents include the latest 100 comments and closing-PR references. A REST
  issue GET alone does not supply that aggregate; comment and other representations
  need their own requests and validators. Retain the bounded GraphQL batches.
- **Cached issue or PR state:** [`IssueCache`](../../src/github_issues/cache.rs)
  reuses documents for a default 300 seconds. `fetch_issues_cached_current` and
  `fetch_items_cached_current` validate state with a shallow aliased `fetch_states`
  request, or use the caller's known listing state. State mismatches and failed
  checks cause full fetches. Replacing this with per-item REST requests adds a
  second representation and fetch path for an already batched operation.
- **Timestamp-only freshness:** `fetch_updated_at` in `github_issues.rs`, added for
  [#1815](https://github.com/rust-works/omni-dev/issues/1815), already offers bounded,
  aliased issue/PR timestamp reads. It is a helper, not wired into the disk cache:
  callers must retain a baseline and choose when to fetch full data. A failed or
  missing check cannot certify freshness, and it is not an atomic snapshot with
  the full fetch. Do not assume a timestamp detects every related-resource edit;
  Projects-v2 board changes require their own freshness strategy.
- **PR check badges:** [`pr_status.rs`](../../src/pr_status.rs) resolves branch-to-PR
  discovery and commit check/status rollups together across repositories. Its
  module documents measured GraphQL costs of 1 point up to roughly 50 branches,
  rising for larger batches. REST would need discovery plus the appropriate
  commit statuses/checks; a PR GET's validator cannot certify the badge. Retain
  GraphQL and the daemon's existing coalescing, adaptive polling, and budget backoff.
- **Merge eligibility:** `resolve_merge_targets` in `pr_status.rs` fetches PR
  identity, head OID, draft/merge state, and checks freshly at enqueue time. This
  safety decision needs current aggregate facts, not a cached PR representation
  whose validator covers only one part. Retain the fresh batched query.
- **Open PR lists and PR creation context:** the daemon's
  [`OpenPrCache`](../../src/daemon/services/worktrees.rs) shares `gh pr list` results
  with a default 60-second TTL; the
  [extension fallback](../../editors/vscode/src/github.ts) uses the same CLI when
  needed. [`create_pr.rs`](../../src/cli/git/create_pr.rs) consumes PR context and
  invokes mutations, which are not conditional-read candidates. A REST pull-list
  endpoint is plausible for list-only polling, but replacing today's paths would
  require matching fields, filters, pagination, and cache behavior. There is no
  demonstrated bottleneck justifying that migration. GitHub CLI implements both
  its [ordinary PR list](https://github.com/cli/cli/blob/trunk/pkg/cmd/pr/shared/lister.go)
  and [search list](https://github.com/cli/cli/blob/trunk/pkg/cmd/pr/list/http.go)
  using GraphQL.
- **Rate-limit monitoring, the REST exception:**
  [`github_rate_limit::run_gh_rate_limit`](../../src/github_rate_limit.rs) already
  calls `gh api rate_limit`. Unlike the issue's initial all-GraphQL inventory,
  this is REST. [GET /rate_limit](https://docs.github.com/en/rest/rate-limit/rate-limit#get-rate-limit-status-for-the-authenticated-user)
  is already exempt from the primary REST limit. Adding validators cannot improve
  that budget, and the consumer needs changing counters across resource families.
  Retain the existing periodic overview and GraphQL budget observations.

Aliasing does not make GraphQL unlimited: connection selection, node limits, and
query size still matter. Conversely, unchanged REST polls can cost zero primary
points, so GraphQL is not universally cheaper. For current consumers, preserving
batching and aggregate coverage outweighs that possible saving. This is a source
audit and architectural judgment, not a measured REST-versus-GraphQL benchmark.

## Transport choice for a future qualified consumer

Prefer `gh api <rest-path> --method GET --include` through
[`github_metrics::run_gh`](../../src/github_metrics.rs), following the repository's
existing shell-`gh` approach in [ADR-0003](../adrs/adr-0003.md). The CLI already
handles the active account and host; the wrapper preserves counting and request
logging. `--include` exposes the status line and headers, and `--header` can send
an opaque ETag or Last-Modified value. These options are documented in the
[GitHub CLI manual](https://cli.github.com/manual/gh_api).

A direct `reqwest` client makes typed status/header handling easier and could
reuse connections, but it would need a credential acquisition and host policy,
plus equivalent instrumentation and redaction. No current consumer warrants
that additional ownership. Header parsing alone is not a reason to migrate.

A future implementation must:

1. Key the cached body and validator by authenticated account/host, exact URL
   including filters and page, and representation headers/API version. Reset or
   miss on identity changes. Store opaque validators unchanged and retain a body
   for the same representation; reuse the disk cache's private-data protections.
2. Distinguish status/headers from the JSON body. On `200`, validate and replace
   body plus validator together. On `304`, reuse only the matching valid body;
   without one, retry unconditionally. Never parse the empty 304 body as JSON.
   Handle redirects and verify the supported `gh` version's exit behavior rather
   than relying solely on subprocess success.
3. Treat `401`, `403`, `404`, `429`, malformed responses, and transport failures
   as errors or explicit misses under the consumer's policy, never proof of
   freshness. Respect rate-limit headers and backoff. In particular, `404` may
   mean lost access to a private resource.
4. Use ETag when supplied, or Last-Modified when supported; without a validator,
   perform an ordinary fetch. Validate each paginated representation separately
   and account for collection membership changes. Keep existing TTL, explicit
   refresh, and mutation-triggered invalidation semantics deliberate.
5. Test unchanged, changed, missing-body, missing-validator, identity-switch,
   pagination, and failure cases using fake `gh` responses before enabling it.

## When to revisit

Reopen the design for a recurring single-resource REST read whose representation
fully covers the consumer's freshness requirement, with stable request parameters,
a high unchanged-response rate, and no useful aliased-batch alternative. Measure
unchanged and changed workloads, including total HTTP calls, subprocess time,
payload size, primary charges, and secondary-limit behavior. A successful result
must justify both the migration and its cache/error-handling complexity.

Until then, use existing TTL/state checks and the timestamp helper where its
documented coverage fits. Board freshness, webhook infrastructure, and new
authentication remain separate work.
