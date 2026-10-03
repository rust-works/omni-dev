# Blind labelling rubric (round 4)

You are labelling frozen GitHub issue texts BEFORE any model scoring is run. You do
not see, and must not look for, any model scores or other people's labels. Judge only
the text of each file. Do not browse the repo's `docs/evaluations`, do not run `omni-dev`,
do not call any API. Read the packet files only.

A model will later be asked yes/no questions about each issue's *submitted text*. Your
labels are the ground truth those answers are compared with, so judge what the text
itself establishes, not what you can infer from linked ADRs or cited issues you cannot see.

## Part A — `bearing` (for each implementation case listed in manifest.json, per open citation)

Question: if the cited issue were resolved (landed), would LESS *implementation* work
(code, tests, docs) remain for THIS issue than its current text implies?

- `yes`   — the text says (or clearly implies) the cited item is a precondition, or that
            part of this issue's own implementation would be done, absorbed or unblocked
            by the cited item.
- `no`    — the text says the cited item is independent, parallel, sibling, provenance, a
            tracker, a "coordinate rather than conflict" note, or only shares a mechanism;
            this issue's own implementation work is unaffected.
- `unclear` — the text does not let a careful reader tell.

Give one sentence quoting or paraphrasing the decisive text.

## Part B — `bounded_spike` (for each case in the "spike" list)

Consider only THIS issue's OWN remaining work (not a worked example, a quoted case, a
cited issue, or a proposed feature).

- `positive` — there is a specific empirical check (a measurement, probe, live test,
               benchmark, experiment) that has NOT yet been run, AND THIS issue's text
               already states the concrete action to take for each relevant result, so that
               running the check settles a live decision fork without further judgment.
- `negative` — no such check; or the check is already complete; or the issue is a plain
               fix/feature/plan; or it needs judgment beyond the check.
- `negative_conditional` — a plan that contains "if X then fall back to Y" style
               contingencies, but those are ordinary engineering contingencies inside an
               implementation plan, not a pending empirical fork that decides the approach.
- `borderline` — a check exists and some consequences are stated, but not for every result,
               or some real judgment would remain after it. Use this sparingly.

Give one sentence naming the check (if any) and where the text states its consequences.

## Output

Write JSON to the output path you are given, exactly:

```json
{
  "labeller": "<your model name>",
  "implementation": [{"id": "...", "citation": "#N", "bearing": "yes|no|unclear", "evidence": "..."}],
  "spike": [{"id": "...", "label": "positive|negative|negative_conditional|borderline", "evidence": "..."}]
}
```

Label every listed citation and every listed spike case. If a text is too long to read in
full, read the parts that matter, but do not label from the title alone.
