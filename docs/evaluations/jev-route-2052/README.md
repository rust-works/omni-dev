# Held-out route baseline — #2052

This artifact freezes an external evaluation census before any new question
wording is tuned for [#1823](https://github.com/rust-works/omni-dev/issues/1823).
The plan was [posted before implementation and predictions](https://github.com/rust-works/omni-dev/issues/2052#issuecomment-5944867147).
No production routing behavior or question wording changes in this study.

## Selection and labels

Selection: every open issue carrying `Triaged` in `rust-works/succinctly` at
collection on 2026-10-02, sorted by number: **34 issues** and **42 comments**.
`selection.json` is the exact `gh issue list --repo rust-works/succinctly
--label Triaged --limit 200 --json number,title,labels,updatedAt,url` response.
`open-inventory.json` records the surrounding open population using limit 1000.
Neither limit was reached. `predeclared.json` freezes membership and hashes of
sources and labels before the baseline. The tuning set is empty. Future studies
must keep these issues out of training/question-selection sets; once repeatedly
used to select wording, this census is no longer an untouched holdout.

`sources.json.gz` preserves complete REST issue responses and every comment page
(`gh api repos/rust-works/succinctly/issues/N`, and
`gh api 'repos/rust-works/succinctly/issues/N/comments?per_page=100' --paginate
--slurp`). It records collection timestamps and source `updated_at` values,
comment IDs, authors, timestamps, labels and bodies. It pins the fetched revision
only; it cannot recover earlier edits or establish the time a label was applied.

`labels.json` contains the external source class, cited plan comment IDs,
predeclared evaluator stage expectations and case-level open-question notes.
See [rubric.md](rubric.md) for the reconstructed external justifications and the
separate stage-assessment rubric. External classes are implementation labels,
while the router's overall class is `max(design, implement)`. Both comparisons
are reported. Fable (#2802) is unsupported by the two-rung Anthropic ladder and
excluded from class agreement, with its raw prediction retained. #2875 and
#2883 are Triaged but have no posted comment plan; their source labels are
retained and their ambiguous stages left unscored.

No explicit external design/review class annotations were found. Stage agreement
against Codex's pre-output assessments is exploratory, **not human-stage accuracy**.
The standalone external rubric asserted by #1823 could not be located; the
source comments preserve the actual class explanations. This limits completion
of the originally desired independent human labeling, and should be resolved
before stronger calibration claims.

## Baseline provenance

`run.json` records the exact command, source commit, CLI version, binary hash,
start/end times, duration, exit status, requested alias and cost accounting.
`templates.json` freezes the unmodified stage questions and Anthropic ladder.
The pinned source also constructs citation questions. The default confidence
threshold is 0.3, top-two margin threshold 0.2 and input cap 60,000 characters;
strict comparisons apply. Effort advice is disabled. Every open issue is routed,
then evaluation membership is applied offline; non-Triaged issues are not tuning
examples and are not included in agreement scores.

`github-responses.json.gz` maps fixture filenames to the exact arguments,
stdout, stderr and exit status of every `gh` command consumed by the CLI.
This includes the all-open enumeration, main inputs and cited-item fetches.
Referenced bodies are not added to Jev's issue text; citation states determine
extra dependency questions. The archive preserves partial/failing responses.
`baseline.json.gz` is the complete CLI stdout and `baseline.stderr.txt` its
stderr, including warnings. This is raw **CLI output**, not a dump of the Jev
HTTP wire response. The CLI decodes/discards some wire metadata and aggregates
token usage; no per-issue billed dollar total is available. No credentials or
settings files are included.

## Recorded results: 2026-10-02

The unchanged baseline routed **214 issues in 130.26 seconds**, resolving
`jev-latest` to **jev-1.13.0**. All 34 holdout inputs matched the predeclared body,
title and human comment snapshot. There were zero route failures, missing
holdout predictions or citation-fetch failures. No holdout input was truncated.
Outside the holdout, #3456 and #235 reached the 60,000-character cap; #235 also
had 308 comments, of which the CLI used the latest 100. Warnings are preserved.

- **Overall class:** 30/33 agreement (90.9%). Of 21 external Opus labels, 19
  routed Opus and two Sonnet. Of 12 Sonnet labels, 11 routed Sonnet and one Opus.
- **Implementation stage:** 16/33 agreement (48.5%). All 12 external Sonnet
  labels matched; only four of 21 Opus labels matched. The other 17 routed
  Sonnet for implementation. Overall agreement consequently masks substantial
  stage disagreement: design can raise an issue to Opus even when implementation
  does not agree with the external implementation label.
- **Evaluator stage subset:** design 12/13, review 5/7. These are predeclared
  agent assessments, not independent human-stage labels. Design disagreed on
  #2806 (Sonnet rather than none, confidence 0.27, flagged close call). Review
  disagreed on #2706 and #3507 (Sonnet rather than the evaluator's Opus).
- **Open-question reading categories:** seven none, 14 factual, nine design,
  four both. These categories describe the full post-plan threads and are not
  tested outputs of the proposed new question.

Overall mismatches, with the frozen plans' context:

1. **#2663, Opus → Sonnet.** The source plan explicitly says no design question
   but assigns Opus to careful performance attribution and an interleaved
   two-architecture A/B. The router chose design none (confidence 0.18) and
   implement Sonnet (0.39); design and review are close calls. Reading function
   definitions does not supply the required measurements.
2. **#2875, Sonnet → Opus.** No posted comment plan exists. Design routed Opus
   (0.53), implement Sonnet (0.79): the source implementation label actually
   matches. Treat this as a class-semantics/input-completeness mismatch, not
   evidence that the implementation label is wrong.
3. **#3479, Opus → Sonnet.** The latest implementation plan says the approach
   questions are settled and retains Opus for execution and two-architecture
   performance verification. The router chose design none (0.18), implement
   Sonnet (0.51), review Opus (0.05); design and review are close calls. A
   definition-only pass cannot replace its WP0 measurement gate.

Observed aggregate usage was **606,983 input and 28,076 output tokens**. At the
published Jev 1.13 rate checked on 2026-10-02 ($0.042 per million input tokens,
output free), the list-price estimate is **$0.025493286 USD** for the entire
all-open run. [TypeSafe's official model reference](https://docs.typesafe.ai/models)
supports the rate; `pricing.json` freezes the rate and caveats. This is an estimate,
not an account invoice: credits, gateway/contract rates and any usage omitted
from failed requests are unknown. Token totals are not broken down by holdout
issue, and build/fetch wall time and downstream execution costs are separate.

## Audit and reproduce

Offline analysis needs only Python's standard library:

```bash
WT=/absolute/path/to/omni-dev/worktree
EVAL="$WT/docs/evaluations/jev-route-2052"
python3 "$EVAL/test_analyze.py"
python3 "$EVAL/analyze.py" > /private/tmp/2052-summary.json
cmp "$EVAL/summary.json" /private/tmp/2052-summary.json
(cd "$EVAL" && shasum -a 256 -c SHA256SUMS)
```

For a fresh live census (different input revisions), build the CLI and use a
**new** output directory with configured Jev credentials. This incurs API cost:

```bash
cargo build --manifest-path "$WT/Cargo.toml" --bin omni-dev
python3 "$EVAL/capture.py" /absolute/path/to/succinctly /private/tmp/route-new-run
```

For an exact GitHub-input replay, unpack the archived fixtures into a new
writable directory, build the source commit recorded in `run.json`, and run:

```bash
python3 - "$EVAL" /private/tmp/route-frozen-gh <<'PY'
import gzip, json, pathlib, sys
root = pathlib.Path(sys.argv[2])
root.mkdir(exist_ok=False)
records = json.loads(gzip.decompress((pathlib.Path(sys.argv[1]) / 'github-responses.json.gz').read_bytes()))
for name, value in records.items():
    (root / name).write_text(json.dumps(value))
PY
OMNI_DEV_GH_BIN="$EVAL/gh_fixture.py" \
ROUTE_GH_REPLAY=/private/tmp/route-frozen-gh \
"$WT/target/debug/omni-dev" ai jev route --all-open --refresh \
  --ladders anthropic --jev-model jev-latest -o json \
  -C /absolute/path/to/succinctly > /private/tmp/route-replay.json
```

The checkout must resolve to `rust-works/succinctly` and the pinned command
argument shape must match. The replay makes **new paid Jev calls**, with no live
GitHub reads, and is not a deterministic reproduction of model judgments.
Use the resolved model in `summary.json` instead of `jev-latest` to avoid alias
movement where that version remains available. Offline reanalysis of the
archived report is deterministic. New runs must write elsewhere, preserving
this baseline and predeclared labels.

## Interpretation and study boundaries

Read `summary.json` for every per-case probability, mismatch, close call,
source-drift warning, unsupported class, failed/omitted holdout item and failed
citation fetch, together with all-open failures. Agreement denominators count
supported, successful predictions only; failures are separately listed rather
than treated as correct. The comment/input reconciliation checks the exact CLI
inputs against the predeclared snapshot; drift is not silently scored away.

The case categories separate questions answerable by observation from choices
between approaches. Definition existence alone cannot close oracle behavior
matrices (#2588, #2643), two-machine measurement gates (#2640, #2663, #3479),
accepting-direction identity/consumer audits (#2801, #3459, #3460), or a new
representation decision (#2802). See each `cheap_round_limit` in `labels.json`.
These are study candidates identified by reading, not measured success/failure
of the proposed cheap rounds: existence, templating and call-site studies have
not run here. The post-triage snapshot alone does not measure a before/after
comment effect or whether any open design question is actually closed.

This is a small, domain-correlated Rust jq/yq census from one repository and
one source login. Posted plans and class explanations are part of the router's
input, creating label leakage; conditional Sonnet labels can assume dependencies
have landed, whereas the baseline still sees open dependencies. Labels may be
stale and are not successful downstream model executions. There is one baseline
run, without independent labellers, repeat variance or cost-optimality evidence.
No probability calibration, 90% task-success rate or general cross-repository
routing accuracy is established. Preserve this baseline before additive studies
and obtain independent human stages/rubric adjudication for stronger claims.
