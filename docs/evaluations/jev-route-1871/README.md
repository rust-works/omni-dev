# Route signal follow-up — #1871

Status: relation labeling is supported for conservative explicit phrases.
The proposed implementation-stage `could_be_cheaper` and bounded-spike
questions remain unshipped.

## Historical baseline mismatch

The original harness inserted literal `+` characters and padding inside its
copy of the design question and both candidate question strings. The stored
`results.json.gz` and `holdout-results.json.gz` faithfully record those requests,
but their design question is **not byte-for-byte production wording**. Treat the
observations below as historical candidate results, not a clean comparison
against the production design signal. The conservative relation implementation
was checked independently against source text and tests; it does not use these
candidate scores.

The corrected harness imports the production design question directly and pins
both candidate strings with regression tests. Citation selection now comes from
an optional `citation` field on each input, rather than a fixed case-ID lookup.
The old inputs have been migrated to explicit fields; the old stored requests
remain unchanged. Re-running them now uses corrected wording and today's embedded
class/issue-level questions, so it intentionally cannot reproduce the old question
map. Exact maps are preserved in each response record.

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
count. All 56 calls resolved to `jev-1.13.0`. The historical baseline used the
then-current production Anthropic class questions and a malformed copy of the
design-stage
`could_be_cheaper` question where a citation was selected. The candidate
adds the implementation-stage and own-issue spike questions. Each case
was run twice in each variant. Corrected candidate wording is in
[`examples/jev_route_signal_eval.rs`](../../../examples/jev_route_signal_eval.rs).

Reproduce with configured Jev credentials from a worktree:

```bash
WT=/absolute/path/to/omni-dev/worktree
cargo run --manifest-path "$WT/Cargo.toml" --example jev_route_signal_eval -- \
  "$WT/docs/evaluations/jev-route-1871/inputs.json" \
  /private/tmp/jev-route-1871-repeat.json 2
```

Use a new output filename for each input. The harness creates the output
exclusively and checkpoints it after every response; a failed request leaves
completed observations available rather than discarding the run. It rejects
oversized inputs before making requests and stops on a model-version or answer-key
mismatch after preserving the returned response for diagnosis. These are
public issue texts; the harness does not fetch linked ADR or cited issue content. The
pre-probe reconstructions are not evidence of the exact historic output.

## Historical observations

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

## Corrected comparison (2026-10-03)

`corrected-inputs.json` freezes eleven cases, including six comparators from
previous sets and five newly selected inputs. `corrected-labels.json` contains
this author's readings recorded before inspecting the new answers, plus an
explicit post-run source-review correction: #2640 was initially called borderline,
but its triage comment states a deterministic threshold and both actions, so it
is treated as an independent bounded-measurement positive. That correction is
not a blind label. The new succinctly cases reconstruct pending states from current bodies and a stated
comment cutoff; they are not archived historic route inputs. Closed-issue
resolution comments are intentionally excluded. Metadata records that exclusion
and the source update time. No linked ADR text is added.

`corrected-results.json.gz` records all 44 requests and responses (22 repeated
baseline/candidate pairs). All resolved to `jev-1.13.0`. `corrected-summary.json`
contains per-case scores, implementation choices, drift counts and token totals.
The baseline uses the current embedded Anthropic class criteria, the production
design-bearing question, and the current independent `open_questions` question.
The ladder deliberately omits model effort bindings, so this tests class-only
single-ladder requests, not default effort-aware or multi-ladder routing. The
candidate adds the corrected implementation question for the selected citation
and one own-issue spike question. No case is truncated at the 60,000-character
route limit; a fixture test verifies that assumption. Each repeat runs baseline
first and candidate second; request order is not randomized, so time/order
effects have not been isolated.

Reproduce using a new output filename:

```bash
WT=/absolute/path/to/omni-dev/worktree
cargo run --manifest-path "$WT/Cargo.toml" --example jev_route_signal_eval -- \
  "$WT/docs/evaluations/jev-route-1871/corrected-inputs.json" \
  /private/tmp/jev-route-1871-corrected-repeat.json 2
```

### Results and shipping decision

- **Implementation bearing still hits the measurement floor.** All 44 answers
  chose `sonnet` for implementation, including the complex #2889 representation
  case and #3191's thin-string proposal. #2889 → #2999 scored 0.62 in both
  candidate repeats; absorbed #1740 → #1753 scored 0.80. Parent-tracker
  #1845 → #1830 scored 0.32–0.34; example-provenance #1871 → #1845 scored
  0.19. These self-reports do not establish a reduction in required tier or
  validate prediction of remaining work against forward outcomes. Keep the
  implementation question unshipped.
- **Spike wording remains unvalidated.** Reconstructed #1845 pre-probe scored
  0.88–0.90; completed #1845 scored 0.11–0.12; example-provenance #1871
  scored 0.09. The independent #2640 pending measurement scored 0.55–0.58,
  still near conditional-plan #2705 at 0.44–0.47. Newly selected #3182's
  linked-ADR measurement scored 0.27–0.31, #3191's unsafe/price judgment
  scored 0.21–0.23, and #2999's representation experiment scored 0.42–0.44.
  The latter has acceptance criteria but still asks for implementation and
  explanation of clone behavior; it is not a clean binary API probe. These
  additions do not establish a display threshold or independent positive
  separation. Keep the spike question unshipped.
- **No observed choice drift in this set.** Design, implementation and review
  choices matched in all 22 baseline/candidate pairs and all repeat pairs.
  This small class-only comparison does not prove batching is harmless for
  other issues, effort questions or multiple ladders.
- **Measured token cost.** Baseline: 87,180 input and 3,704 output tokens;
  candidate: 90,804 input and 4,388 output tokens. These are totals for this
  frozen set and model, not a general price estimate.

Next evidence should include implementation outcomes that actually disappear
or reduce after a dependency lands, and independent pending checks with explicit
own-text actions for every outcome. Expand the batching comparison to production
effort-aware requests before shipping either signal. The malformed historical
results cannot establish baseline drift for those questions.
