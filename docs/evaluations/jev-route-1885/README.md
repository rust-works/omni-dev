# Anthropic route ladder re-evaluation — #1885

Run on 2026-10-01 against `jev-1.13.0`, after Claude Sonnet 5.5 shipped on
2026-09-28. The [release announcement](https://www.anthropic.com/claude-sonnet-5-5)
reports major gains over Sonnet 5 in coding and says Opus 5.5 remains stronger
on complex, open-ended work. The
[model page](https://platform.claude.com/docs/en/models/sonnet-5-5/overview)
confirms the `claude-sonnet-5-5` ID; the
[effort documentation](https://platform.claude.com/docs/en/build-with-claude/effort)
confirms its five levels, `low` through `max`. These are provider claims, not
measurements on this repository's tasks.

## Inputs and method

`run.py` uses the 20 frozen #1779 E1 problem-only issue texts and the
hand-assigned labels in `../jev-effort-1888/`. It sends the exact #1779 stage
instructions to Jev with four criteria sets from `ladders.yaml`, twice each:

- `baseline`: the current two-rung Anthropic descriptions and short names.
- `versioned`: the same descriptions with `claude-sonnet-5-5` and
  `claude-opus-5-5` as criterion keys. This tests the suggested custom
  versioned-name ladder, not model execution.
- `legacy`: the original three-rung #1779 ladder, including `fable`. This
  shows which stages would reach the old top rung under the suggested
  `fable`→Opus 5.5 mapping.
- `candidate`: the proposed replacement Sonnet and Opus descriptions, fixed
  before inspecting these results.

Six previously frozen #1888 holdouts were run the same way: four full issue
bodies and two synthetic controls. `summary.json` holds counts and choices.
`e1-results.json.gz` and `holdout-results.json.gz` preserve every input,
question map, raw answer, resolved model, usage, elapsed time and error.
All 208 requests succeeded; none was dropped or retried.

Reproduce with configured Jev credentials and the omni-dev CLI on PATH:

```sh
python3 docs/evaluations/jev-route-1885/run.py /private/tmp/jev-route-e1 2
python3 docs/evaluations/jev-route-1885/run.py /private/tmp/jev-route-holdout 2 \
  docs/evaluations/jev-effort-1888/holdout-inputs.json
python3 docs/evaluations/jev-route-1885/summarize.py \
  /private/tmp/jev-route-e1 /private/tmp/jev-route-holdout
```

Use fresh output directories. The command sends the public issue texts to
Jev. The archived results are the observed run, not an expected exact-output
test: near-boundary answers can change between runs.

## Observations

- The original three-rung variant agreed with the original labels in 16/20
  issues on both repetitions, matching #1779's total of 6/8 plus 10/12.
  `fable` won design for eight issues per repeat, including all seven issues
  labelled `fable`. It never won implementation or review in these 40 calls.
- With `fable` collapsed into `opus` for the two-rung comparison, the
  baseline agreed with 15/20 old labels in each repeat. The candidate agreed
  with 18/20 in each repeat. Its two misses were #1652 and #1605, both
  concurrency-sensitive issues labelled Opus but routed Sonnet. Agreement is
  with one person's old labels, not proof that Sonnet 5.5 can finish the work.
- Candidate wording changed the overall class on seven of 20 issues in both
  repeats, all Opus→Sonnet. It changed 14/40 design, 18/40 implementation and
  21/40 review stage choices in paired comparisons. It selected Sonnet for
  implementation in all 40 E1 calls. This is a material routing change,
  especially for review, where E1 has no independent labels.
- Versioned tier names kept the same overall class on every paired E1 call,
  but changed implementation in 10/40 and review in 5/40 comparisons.
  Criterion names therefore affect stage choices even with byte-identical
  descriptions; the custom versioned ladder is evaluated but is not
  equivalent to the built-in ladder.
- In the six holdouts, the candidate kept the two synthetic controls at
  no remaining design and kept #1861's open research at Opus design in both
  repeats. #1880's design switched between Sonnet and Opus under the
  candidate. The other full inputs kept their broad design status, while
  candidate review often moved to Sonnet. These cases are too few to validate
  review quality.

## Decision and limits

Adopt the candidate descriptions for the first two rungs. They describe the
current Sonnet 5.5 / Opus 5.5 capability distinction more plausibly than
“Sonnet tends to miss non-obvious interactions” and “Opus can under-explore
open design”; the frozen comparison improves agreement with old labels while
retaining Opus for the labelled open-design cases. Keep the two-rung Anthropic
ladder: the legacy `fable` description was chosen for design, but
Anthropic's published comparison supports Opus 5.5 as the practical top
route for most work, and this evaluation did not measure model outcomes.
The proposed `fable`→Opus 5.5 implement/review binding has no directly
observed `fable` implement/review selections in E1; with the `fable` rung
retired, `opus` binds Opus 5.5 for all three stages.

Copy the first two revised descriptions to OpenAI and Gemini to preserve
their existing rung-for-rung invariant. The old third-rung description
remains for those providers. No independent OpenAI/Gemini task labels or
downstream outcomes were measured, so their new routing remains a hypothesis.
The #1779 stage instructions themselves are unchanged, but the revised
criteria are **#1885-evaluated wording**, not the original #1779-tested text.
Jev confidence is not calibrated task-success probability. Forward outcomes
and separately labelled review cases are needed before claiming reliability
or cost savings.
