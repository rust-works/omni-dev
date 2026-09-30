# Route signal follow-up — #1871

Status: relation labeling is supported for conservative explicit phrases.
The proposed implementation-stage `could_be_cheaper` and bounded-spike
questions remain unshipped.

## Frozen inputs and method

`inputs.json` contains ten cases from public
[`omni-dev`](https://github.com/rust-works/omni-dev) and
[`succinctly`](https://github.com/rust-works/succinctly) issues;
`holdout-inputs.json` contains four additional cases selected and labeled
before running their Jev requests. Each case records its source URL and
source update time. The corresponding `labels.json` files record one
author's expected relation and spike reading. The two "pre-probe" cases
are reconstructions from first plan comments, not archived route inputs.
Their tracker labels were corrected to `unspecified` after a source-text
review: the reconstructed text did not itself establish that relation.
They are retained to make that limitation visible.

`results.json.gz` and `holdout-results.json.gz` contain every request
(including exact state and question map), response, model name and token
count. All 56 calls resolved to `jev-1.13.0`. The baseline uses the
production Anthropic class questions and the existing design-stage
`could_be_cheaper` question where a citation was selected. The candidate
adds the implementation-stage and own-issue spike questions. Each case
was run twice in each variant. The candidate wording is in
[`examples/jev_route_signal_eval.rs`](../../../examples/jev_route_signal_eval.rs).

Reproduce with configured Jev credentials from a worktree:

```bash
WT=/absolute/path/to/omni-dev/worktree
cargo run --manifest-path "$WT/Cargo.toml" --example jev_route_signal_eval -- \
  "$WT/docs/evaluations/jev-route-1871/inputs.json" \
  /private/tmp/jev-route-1871-repeat.json 2
```

Use a new output filename for the holdout input. These are public issue
texts; the harness does not fetch linked ADR or cited issue content. The
pre-probe reconstructions are not evidence of the exact historic output.

## Observations

- **Implementation-stage score:** the cited work absorbed by #1753 scored
  0.80–0.82, explicit blocker #2709 scored 0.78, and explicit blockers
  #1351 and #1129 scored 0.66 and 0.55–0.59. A sibling/related citation
  #2063 scored 0.48–0.50, overlapping the weaker blocker. All fourteen
  cases chose the minimum `sonnet` implementation tier in both baseline
  and candidate runs. Thus this set cannot establish that the score predicts
  a reduction in the chosen implementation tier. Earlier resolved
  simulations on #1871 also hit this floor. Do not ship the question yet.
- **Bounded spike:** the two reconstructed pre-probe plans scored
  0.88–0.89; completed #1845 scored 0.11–0.12; the #1871 worked example
  scored 0.17–0.21. An independent live measurement with a stated decision
  threshold, succinctly#2640, scored only 0.56 in both repeats. The
  non-spike succinctly#2705 scored 0.47–0.48. A threshold between them
  would have little margin and no forward validation. Do not ship this
  wording or an arbitrary display threshold.
- **Stage drift and cost:** among 28 paired baseline/candidate comparisons,
  design choice changed four times and review choice three times;
  implementation choice did not change. #2640's design answer flipped in
  opposite directions across the two repeats, showing baseline instability
  near `none`. Baseline requests used 91,422 input and 3,584 output
  tokens total; candidate requests used 97,154 input and 4,577 output
  tokens. These totals cover this set only and are not a cost forecast.
- **Relations:** the frozen issue texts support `tracker` for the explicit
  "Split out of #1830" in current #1845 and `blocker` for the explicit
  "blocked on" phrasing in succinctly#1356, #2511 and #2705. Related,
  absorbed, example and reconstruction citations stay `unspecified`.
  The rule is intentionally conservative and does not alter citation
  visibility or route scores. It has fixture and adversarial tests for
  repeated mentions, sentence scope, negation, quotes and conflicting
  phrases. It does not verify whether the claimed dependency is true.

The labels are one person's readings, not independent ground truth.
The implementation question still needs cases whose current
implementation stage is above the bottom tier or observed forward
outcomes. The spike question needs more independent positive cases and
a stable gap from near-negative conditional plans before it can become a
user-facing signal.
