# Route signal follow-up — #1871

Status: relation labeling is supported for conservative explicit phrases.
The proposed implementation-stage `could_be_cheaper` and bounded-spike
questions remain unshipped. Round 4 ([below](#round-4-2026-10-04)) closes the
three gaps the previous round named: it finds why implementation cases above
the bottom tier were missing, shows the implementation question duplicates the
shipped design question, and finds no spike threshold that separates the
labelled cases. It also shows adding either question does not move stage or
class answers beyond repeat noise.

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

## Round 4 (2026-10-04)

Everything below was frozen and labelled before any live request. The inputs,
the author's labels, an independent blind labeller's labels and the decision
rules (posted on #1871) are in the commit that precedes the results.

### What was run

- **Screening, offline.** The #2052 census baseline (`../jev-route-2052/baseline.json.gz`,
  214 routed issues, `jev-1.13.0`) routed `implement` to `sonnet` for 209 issues
  and to `opus` for 5. The five, plus the four next closest by `P(opus)`, are
  the implementation set (`round4-inputs.json`, `freeze_round4.py`). Eight use
  the archived text that baseline saw. succinctly#2839 has no triage comment, so
  it is not archived and was fetched live; the script requires its last edit to
  predate the census collection, which checks the timestamp but does not recover
  the exact revision. All six explicit `blocker` citations
  in the census sit on issues with `P(opus) <= 0.17`, so no census issue is both
  above the floor and an explicit blocker.
- **Labels** (`round4-labels.json`, `round4-rubric.md`). The author labelled
  every open citation and every spike candidate; a separate model (`claude-opus-5-5`),
  given only the rubric and the frozen text, did the same blind. The author's
  file was hashed before the blind file was opened. Implementation bearing
  agrees **9/9** (every pair is `no`); spike labels agree 17/19 exactly and
  19/19 once `negative_conditional` counts as a negative. Nothing was relabelled
  after a score was seen.
- **Spike candidates** (`round4-spike-inputs.json`): 19 issues from a recorded
  keyword scan of the census and the open omni-dev research issues, plus a
  `re-measure` search of open succinctly issues after the first scan found one
  clear positive. omni-dev#2122 is frozen as labelled, before its plan comment.
  The eleven earlier comparators are reused from `corrected-results.json.gz`.
- **Variants** (`round4-variant-inputs.json`, `derive_round4_variants.py`): each
  census case with the triage `**Class:**` paragraph removed (`-noclass`), and
  two edited simulations that state an explicit hard prerequisite or its landing
  (`-inject-open` / `-inject-resolved`). A third kind, `-absorb-*`
  (`round4-posthoc-inputs.json`), was written **after** the first scores were
  read, because the prerequisite edit turned out to test ordering rather than
  work reduction. It is exploratory and enters no decision.
- **Runs** (`round4-*-results.json.gz`, `summarize_round4.py` ->
  `round4-summary.json`): 254 requests on `jev-1.13.0` (1,660,537 input and
  128,929 output tokens). Three repeats for the implementation set and variants,
  two for the spike set and the production-shaped subset. Baseline/candidate
  order is fixed per case and repeat by a recorded seed (61 baseline-first, 60
  candidate-first). D1 and D2 use the production request shape: all three
  built-in ladders with `--effort-advice`.

[`run_round4.sh`](run_round4.sh) holds the exact command for every archive (A
asis, B variants, C spike, D1/D2 production-shaped, E post hoc) and rebuilds the
summary; the archived requests are checked against the harness's own builders by a
test in the example. Live answers vary run to run, so a replay reproduces the
requests exactly and the answers approximately.

```bash
run_round4.sh /absolute/path/to/omni-dev/worktree /private/tmp/empty-output-dir
```

The committed summary is reproducible byte for byte from the committed archives:

```bash
D=docs/evaluations/jev-route-1871
python3 $D/summarize_round4.py /tmp/summary.json A=$D/round4-A-asis-results.json.gz \
  B=$D/round4-B-variant-results.json.gz C=$D/round4-C-spike-results.json.gz \
  D1=$D/round4-D1-production-impl-results.json.gz D2=$D/round4-D2-production-spike-results.json.gz \
  E=$D/round4-E-posthoc-absorb-results.json.gz
```

### Results

- **Most of the room above the floor is a class paragraph.** Four of the five
  census issues routed above the bottom tier, and eight of the nine cases here,
  carry a triage paragraph beginning `**Class:** Opus —` that states the class
  and why. Removing only that paragraph lowers `P(implement above floor)` by
  0.31 to 0.54 in every one of the eight cases that have it (mean 0.50 to 0.10),
  leaves no case above the floor in a majority of repeats (3 of 8 before), and no
  repeat chooses `opus` (10 of 24 did). The paragraph is not sufficient: five of
  those eight sit at the floor with it, and succinctly#2839, which has none, is
  just under the line (0.46-0.49). So the room above the floor the earlier
  rounds lacked comes largely from text that announces its own class, and a stage
  answer anchored by that sentence is a poor instrument for checking whether a
  cited issue lowers the tier.
- **No natural pair can test the question.** All nine open citations on those
  cases are `no` (independent, sibling, coordinate, or the dependency runs the
  other way). The implementation scores on those `no` pairs span 0.31-0.61;
  #1183 and #1076, which the plan only says cap fidelity, score 0.55-0.61.
- **The implementation question duplicates the design one.** Over 15 open
  citations (the nine here and six from the corrected set), the implementation
  score and the shipped `could_be_cheaper.design` score from a separate request
  correlate at r = 0.99, with a mean absolute difference of 0.024 and a maximum
  of 0.047. The model reads both as "would resolving this leave less work", so the
  implementation wording adds no signal the design one does not already carry.
- **Edited simulations (weakest evidence).** Stating a hard prerequisite moved
  neither score (0.33 -> 0.32 on #2800, 0.49 -> 0.47 on #2799), which is the right
  answer: a prerequisite orders work without shrinking it. Stating that the cited
  issue delivers part of the implementation (post hoc) raised the implementation
  score to 0.79 and 0.66 and the design score to 0.76 and 0.64, together. Declaring the
  overlapping work landed lowered `P(implement above floor)` by only 0.03 and 0.01,
  inside repeat noise, with the class paragraph still in the text.
- **Spike: no separating threshold.** Five labelled positives (three
  independently double-labelled: #2663, #3685, #1993; plus the reconstructed #1845
  pre-probe and #2640, whose label was corrected after its scores were seen)
  scored 0.89, 0.80, 0.64, 0.56 and 0.45 (case means). Twenty negatives, three of
  them conditional plans, include succinctly#2607 at 0.68 and #2705 at 0.46, above
  three of the five positives. Minimum positive minus maximum negative is **-0.27**
  (the rule needed +0.15); 94 of 100 positive/negative pairs rank correctly but
  no cut-off keeps all positives and drops all negatives. Repeat spread reached
  0.07. The baseline `open_questions` answer for the three new positives was
  `design`, `factual`, `design`, so it does not stand in for the spike score
  either. #2607's plan contains an A/B contingency (take the single-pass scanner if
  the two-pass form regresses) and scoring as a bounded probe is the clearest false positive.
- **Batching does not move the answers.** Class-only single-ladder requests:
  15 of 285 stage choices changed between baseline and candidate (5.3%), against
  14 of 246 between baseline repeats (5.7%). Production-shaped requests (three
  ladders, effort advice, 10 cases, two repeats): 8 of 180 (4.4%) against 4 of 90
  (4.4%). Class changed in 2 of 95 and 3 of 60 comparisons, every one inside a
  baseline close call. The set is small, so this bounds the effect rather than
  proving harmlessness.

### Decision against the pre-registered rules

| Gate | Rule | Outcome |
|---|---|---|
| I1 | at least 4 issues above the floor in a majority of repeats | not met: 3, and 0 without the class paragraph |
| I2 | at least 2 `yes` and 2 `no` natural pairs | untestable: 0 `yes`, 9 `no` |
| I3, I4 | `yes` pairs score and shift above `no` pairs | untestable on natural data |
| Spike | gap at least 0.15, spread at most 0.07 | not met: gap -0.27 |
| Batching | change rate within repeat noise + 0.02 | met on this set |

Neither question ships. The evidence favours retiring the implementation
question rather than re-running it: it is not just unvalidated, it is redundant
with `could_be_cheaper.design`, which [docs/jev.md](../../jev.md) now describes as
a work-reduction score in general. The spike question stays out. A future attempt
needs more positives that are not conditional-plan lookalikes and a wording that
scores contingencies like #2607's low, which a repeat spread of 0.07 leaves little
room for.

### Limits

Labels are two readers' judgements on public text. Three positives are
independent and double-labelled; the other two are the earlier author-only labels,
one reconstructed and one corrected after its scores were seen. Nine
implementation cases, all from one repository and one triage style. The
class-paragraph removal is an edit, not a recovered historical input. The
`-inject` and `-absorb` texts are synthetic. omni-dev#2122 and the other omni-dev
issues are frozen as fetched on 2026-10-04.
