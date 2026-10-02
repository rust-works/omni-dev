# Open-question classifier evaluation (#2053)

## Shipping gate

The required held-out calibration set from #2052 is not available: that issue
was open with no comments or artifact when checked on 2026-10-02. This feature
must remain in a draft PR until that set is published and evaluated. The
supplementary inputs here are not a substitute for it.

The classifier wording is fixed in `open_questions_question` in
`src/jev/route.rs`. The three stage questions and tier descriptions are
unchanged. Class derivation does not consult the new answer; adding a question
can still affect Jev's existing answers, which must be measured.

## Supplementary inputs

`supplementary-inputs.json` freezes seven cases and expected labels before the
first model call: the succinctly #3017 before/after pair, four synthetic controls
covering each label, and one ambiguous control with multiple acceptable labels.
The pair uses the fetched current body, before and after the first triage
comment; it excludes the later implementation plan. Historical body revisions
have not been verified, so it is explicitly a reconstruction, not a claimed
historical snapshot. Each input records provenance. The synthetic controls are
sanity checks, not held-out public issue evidence. This harness isolates class
questions: effort advice and citation `noul` questions are omitted in both
variants. It measures effects on stage/class answers in that mode, not every
possible production request shape. All states are used verbatim; prepare frozen
states using the production input cap when evaluating longer issues.

## Reproduce

From this worktree, with configured Jev credentials:

```bash
WT=/Users/jky/wrk/work-trees/omni-dev/issue-2053-open-questions-classifier
cargo run --manifest-path "$WT/Cargo.toml" --example jev_open_questions_eval -- \
  "$WT/docs/evaluations/jev-route-2053/supplementary-inputs.json" \
  /private/tmp/jev-route-2053-results 2
```

The output directory must be new. Each case uses identical state, model and all
three built-in class ladders for baseline and augmented calls. Baseline omits
only `open_questions`; augmented includes it once. Calls reverse order on
alternate repeats. No code or referenced issue text is retrieved into state.
The harness persists exact requests, raw answers, returned model versions,
usage, tier order, and failures after each call; credentials are never written to results.

Summarize predictions, confusion, failures, stage/class changes and baseline noise:

```bash
python3 "$WT/docs/evaluations/jev-route-2053/summarize.py" \
  /private/tmp/jev-route-2053-results > /private/tmp/jev-route-2053-summary.json
```

## Required held-out follow-up

Once #2052 publishes its frozen inputs, label their question kinds independently
before evaluation. Keep those labels separate from tuning cases. Run the same
paired harness and record:

- Exact input provenance/revisions, requested and returned model versions,
  commands, question/ladders, repeats and date.
- Confusion matrix and every failed or ambiguous prediction, separately from
  controls and the motivating #3017 pair.
- Changes to each existing design/implement/review answer and derived class,
  including repeat-to-repeat baseline noise.
- Raw outputs, failures, token totals, and actual billed cost if available.

No held-out results or accuracy claim is made by this artifact.
