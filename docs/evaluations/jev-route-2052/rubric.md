# Label provenance and rubric

The external implementation labels come from the `Sonnet`, `Opus` and `Fable`
labels on succinctly issues, frozen with the actual label descriptions in
`sources.json.gz`. `Triaged` means “Issue has been read, planned, and its
model-class label verified”. This is a workflow assertion, not proof that an
independent human adjudicated it. All posted plans use the login `newhoggy`;
agent-assisted authorship cannot be ruled out from a GitHub login.

The standalone written rubric claimed by omni-dev #1823 was **not located**.
We checked succinctly's current `CLAUDE.md`, `AGENTS.md`, committed skill tree,
and `CLAUDE.md` at `2581b17c9`, the revision cited by September triage plans.
Do not treat the following reconstruction as a verbatim external rubric.
The complete source justifications, rather than an invented replacement,
are retained in the frozen comment pages:

- Sonnet: bounded changes with an existing sibling/helper precedent and
  oracle-pinned behavior; examples #2520, #2706, #2806, #3507, #3513.
- Opus: semantic decisions, shared invariants or interacting consumers,
  accepting-direction changes, or carefully attributed interleaved measurements
  on two architectures; examples #2598, #2663, #3459, #3512, #3514.
- Fable: new representation/ADR and broad audit obligations; #2802 proposes
  three representations and an audit of roughly 900 match sites.

These are external authors' stated reasons, not measured capability thresholds.
An issue can remain Sonnet for implementation yet merit a more demanding review.
Conversely, a posted plan can settle design while retaining an Opus implementation
label because execution needs careful measurement.

`labels.json` keeps inherited source labels separate from evaluator expectations.
There are no explicit external design/review class annotations in the selected
sources. Those fields are null. For exploratory stage scoring, Codex assigned
13 design and seven review expectations before seeing the baseline. Design
`none` requires an explicitly settled approach; `opus` requires remaining
interacting semantic choices. Review Sonnet means bounded oracle-pinned edits;
review Opus means accepting-direction, shared semantic or rollback invariants.
Ambiguous stages remain null, and Fable is not collapsed into Opus. The inherited
implementation class is scored where it is supported by the default ladder.

Open-question categories are evaluator reading judgments of the frozen full
thread, not outputs of a new Jev question. “Factual” includes oracle observations,
body/caller inspection and empirical measurements; not every factual question
can be answered by the proposed cheap definition-only round. A category of
“none” means no approach question was identified, not that implementation or
verification is finished. The per-case notes name the work a cheap round cannot
close. No cheap round was run in this study.
