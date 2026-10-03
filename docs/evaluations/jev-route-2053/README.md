# Open-question classifier evaluation (#2053)

## Result and status

**Experimental; not validated as a retrieval gate.** This document was written
to keep the work in draft until a calibration review. PR #2093 merged without
that review, so the gate is still open: do not act on `open_questions`
automatically, and do not build a retrieval gate on it until it is met.

Selected wording (v3) passes the motivating #3017 pair and synthetic controls,
but on the 34 held-out issues it is **no better than answering `factual` for
every issue**. Both repeats gave the same answer for all 34 issues, so the 68
predictions counted below are 34 issues' worth of evidence, not 68; the first
write-up's "30/68 (44.1%)" is 15/34. All live calls returned `jev-1.13.0`, with
no API failures. No further wording selection was made against the held-out
failures.

| Held-out, 34 issues (`agreement.py`)            | Three ladders (original run) | Default `anthropic` ladder |
|-------------------------------------------------|------------------------------|----------------------------|
| Issues whose two repeats agree                  | 34                           | 31                         |
| Correct (issues)                                | 15 (44.1%)                   | 15.5 (45.6%)               |
| 95% interval (Wilson, n = 34)                   | 28.9%–60.5%                  | 30.2%–61.9%                |
| Always answering `factual`                      | 14 (41.2%)                   | 14 (41.2%)                 |
| P(at least this many right, at the above rate)  | 0.43                         | 0.30                       |
| Cohen's kappa                                   | 0.23                         | 0.25                       |
| `both` correct (of 4 expected)                  | 0                            | 0                          |
| Retrieval gate (`factual`/`both` = yes): right  | 19 of 34                     | 20.5 of 34                 |
| Retrieval gate: always yes / always no          | 18 / 16 of 34                | 18 / 16 of 34              |
| Retrieval gate: issues needing it that it found | 8 of 18                      | 10 of 18                   |

The classifier is therefore not distinguishable from the majority-class
baseline, and as a gate for "is reading code likely to help" it is not
distinguishable from always saying yes. The default-ladder run
(`heldout-default-shape/`, 2026-10-04) was added after merge because the
original run sent all three ladders, while `ai jev route` defaults to
`anthropic` alone; it did not change the conclusion.

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
JSON files. `run.json` records that v3 was selected before held-out predictions
were inspected; see the caveat below, which the artifacts cannot settle.

- v1: #3017 returned `both` before, `factual` after in both repeats.
- v2: returned `none` for both states; it excluded execution tasks too broadly.
- v3: returns **`factual` before, `none` after** in both repeats. All four
  unambiguous synthetic controls pass twice; the ambiguous control returns
  `both`, within its predeclared acceptable set. This is 12/12 unambiguous
  predictions, plus two ambiguous predictions reported separately.

Two limits on that selection. First, the five synthetic controls pass under v1
and v3 alike (v2 differs only on the ambiguous control, inside its acceptable
set), so the wording was chosen on the four #3017 predictions alone. Second, the
trial files carry no timestamps and were committed together, so the order of the
runs cannot be verified. The held-out v1 run (compiled example, 136 calls) is
listed before v2 and v3 in `run.json` and used the earlier harness, so it very
likely existed first; it answered `both` for 62 of 68 predictions, and v3 moved
away from that. That is consistent with its outcome having been seen, though it
does not show it. If it did, v3's held-out figures would be optimistic, which
only strengthens the conclusion above; the order should still be established
before these results are cited as validation.

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
with 10/68 (5 of 34 issues, kappa 0.01: chance level); its failures are
retained too, without treating that comparison as independent confirmation of
v3.

Adding v3 changed **23/612 stage choices** and **9/204 provider classes** across
paired requests. Baseline repeat-to-repeat noise changed **11/306 stage
choices** and 5/102 provider classes; two augmented repeats differed by 14/306
and 4/102. Neither effect is distinguishable from that noise (Fisher exact
p = 1.0 for both), but a sample this small could not show an effect of a few
percent either, so output invariance is not established. The code does not
override class. `summarize.py` counts only a changed choice label as a change;
confidence and probabilities differ in most answers in every comparison,
including between baseline repeats. The supplementary v3 run changed five stage
choices and one provider class; its baseline repeats changed one stage choice.

On the default `anthropic` ladder (`heldout-default-shape/`), the question
changed 4/204 stage choices and 3/68 provider classes, against baseline noise of
1/102 and 0/34 (p = 0.67 and 0.55). The point estimates sit above the noise but
are not distinguishable from it at this sample size. That run used 632,508
input and 18,303 output tokens, with no API failures; billed cost is unknown.

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

Production differs from the three-ladder run in two ways that the original
evaluation did not measure. It sends the `anthropic` ladder by default, which
`heldout-default-shape/` covers. And it adds one `could_be_cheaper` question per
*open* cited issue; every held-out state references other issues by `#N`, so
real requests usually carry questions that no run here includes. Which of those
citations are open was not determined. The recorded requests place
`open_questions` in sorted-key order (after `gemini.*`), but the committed
`run_candidate.py` appends it last, because the trials used the temporary copy
noted in `run.json`; the committed script is not what produced them.

Recompute the effective-sample statistics (repeats counted once per issue, the
majority-class and always-yes baselines, kappa, the retrieval-gate collapse, and
the noise comparisons including provider classes) without any network or
credentials:

```bash
python3 "$WT/docs/evaluations/jev-route-2053/agreement.py" \
  "$WT/docs/evaluations/jev-route-2053/heldout-v3"
```

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
