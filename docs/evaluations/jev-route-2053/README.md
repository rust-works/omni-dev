# Open-question classifier evaluation (#2053)

## Result and shipping gate

**Keep this PR in draft.** Selected wording (v3) passes the motivating #3017
pair and synthetic controls, but agrees with only **30/68 held-out predictions
(44.1%)**, across two repeats of 34 issues. All live calls returned
`jev-1.13.0`, with no API failures. These results do not justify trusting the
classifier as an automatic retrieval gate. No further wording selection was
made against the held-out failures.

The calibration prerequisite became available during implementation in
[PR #2094](https://github.com/rust-works/omni-dev/pull/2094), commit
`51bbb0ac2f0397f6c4d59a1ffb0734402c5cd5cb`. `heldout-provenance.json` pins
its source/label/template hashes. Its 34 public Triaged issues have
predeclared question-kind assessments: 7 none, 14 factual, 9 design, 4 both.
Labels are evaluator assessments, not independently adjudicated human truth;
the parent documents missing independent stage labels and a missing standalone
rubric. Some factual assessments involve empirical work beyond definition-only
retrieval. We retain all labels and failures, rather than filtering them after
seeing predictions.

## Wording selection and motivating pair

The three stage questions and tier descriptions are unchanged. The new answer
never enters class derivation, but its presence can influence Jev's existing
answers.

Three wording candidates were tried using only the #3017 pair and five synthetic
controls. v1 is preserved in its raw requests; v2/v3 have explicit candidate
JSON files. v3 was selected before held-out predictions were inspected.

- v1: #3017 returned `both` before, `factual` after in both repeats.
- v2: returned `none` for both states; it excluded execution tasks too broadly.
- v3: returns **`factual` before, `none` after** in both repeats. All four
  unambiguous synthetic controls pass twice; the ambiguous control returns
  `both`, within its predeclared acceptable set. This is 12/12 unambiguous
  predictions, plus two ambiguous predictions reported separately.

v3 distinguishes an unverified suggested fix from a code-informed decision,
and prescribed caller audits/tests from open questions. Production wording is
pinned to `candidate-v3.json` by a regression test. `supplementary-inputs.json`
uses the fetched current #3017 body and first triage comment, excluding the
later implementation plan. Historical body revisions are unverified; this is
an explicit reconstruction. The pair uses the exact production comment layout.
Synthetic controls and this motivating pair are tuning cases, not holdouts.

## Held-out result and effects on existing answers

For v3, expected → predicted counts across the two repeats:

- none: none 8, factual 4, design 2.
- factual: factual 10, none 8, design 8, both 2.
- design: design 12, both 6.
- both: factual 4, design 4.

Every prediction and mismatch is in `heldout-v3/summary.json`. In this set,
**both** is never selected for the eight expected-both predictions. v1 agreed
with 10/68; its failures are retained too, without treating that comparison as
independent confirmation of v3.

Adding v3 changed **23/612 stage choices** and **9/204 provider classes** across
paired requests. Baseline repeat-to-repeat noise changed **11/306 stage
choices**. Thus the code does not override class, but output invariance is not
established. Summaries count choice, confidence, and probability-map changes;
raw answers retain every value. The supplementary v3 run changed five stage
choices and one provider class; its baseline repeats changed one stage choice.

The selected held-out paired run used **833,516 input and 52,577 output tokens**.
All trial usage is preserved separately. Actual billed dollar cost is unknown;
no billing receipt was available. `run.json` records model selection, repeats,
CLI version, and call counts. An initial sandbox attempt failed all 28 calls
before obtaining any model answer; it is retained with zero reported usage.

## Artifacts and reproduction

Each trial directory has compressed exact requests/raw outputs, input label
metadata, tier order, and a deterministic summary. The full frozen states are
also in the root input files. No credentials are recorded. All issue inputs
are from the public `rust-works/succinctly` repository.

The paired harness uses identical frozen state and all three built-in class
ladders. Baseline omits only `open_questions`; augmented includes it once.
Call order reverses on alternate repeats. Effort and citation `noul` questions
are omitted in both variants: this isolates class effects, not every production
request shape. All held-out states fit the 60,000-character production cap.

Recompute a saved summary without any network or credentials:

```bash
WT=/path/to/this/worktree
python3 "$WT/docs/evaluations/jev-route-2053/summarize.py" \
  "$WT/docs/evaluations/jev-route-2053/heldout-v3"
```

Repeat the selected paired study with installed `omni-dev` and configured Jev
credentials (the output directory must be new):

```bash
EVAL="$WT/docs/evaluations/jev-route-2053"
python3 "$EVAL/run_candidate.py" "$EVAL/heldout-inputs.json" \
  /private/tmp/jev-route-2053-replay "$EVAL/candidate-v3.json" \
  "$EVAL/supplementary-v3/results.json.gz"
python3 "$EVAL/summarize.py" /private/tmp/jev-route-2053-replay
```

`run_candidate.py` uses the frozen baseline questions and explicitly pins
`jev-1.13.0`; actual trial execution used a temporary copy before its paths were
made portable. The compiled-source harness is also available:

```bash
cargo run --manifest-path "$WT/Cargo.toml" --example jev_open_questions_eval -- \
  "$EVAL/heldout-inputs.json" /private/tmp/jev-route-2053-source-replay 2
```

That harness uses configured model selection and current built-in ladders,
recording all actual requests. Reruns need not reproduce stochastic answers.
Further prompt work requires separate tuning examples and fresh independent
validation, with the label/empirical-work ambiguity explicitly adjudicated.
