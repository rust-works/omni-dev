# One-round existence filter: #2054

This is an experimental diagnostic study, not validation for automated closure
or routing comments. The existence question was implemented before looking at
live results. The diagnostic high-score threshold was fixed at 0.8 in
`provenance.json`; it is not a calibrated decision threshold.

## Inputs and provenance

`inputs.json` copies the title/body of all 34 cases in #2052's predeclared held-out
Triaged census. `provenance.json` records the original compressed source hash,
predeclaration and issue numbers. That source snapshot was being prepared in the
#2052 worktree during this implementation; the parent calibration issue was still
open. These are pinned inputs, not a claim that its routing baseline is complete.
The original fetched/updated times are retained. Comments are excluded from
existence input because code vocabulary should come from the issue itself.

`diagnostic-3017.json` is a separate tuning/diagnostic case, outside the held-out
set. Source retrieval is pinned to succinctly commit
`1ebdda39729110ab0d513b35a990d0d170c1fbf6`. That commit already includes the #3017
fix. Results therefore describe retrieval on current code, not reconstruction of
the original pre-fix discovery at `69a76d346`.

## Reproduction

Use a local succinctly checkout at the exact source commit (the runner rejects a
different HEAD), then build this PR and run:

```sh
cargo build --manifest-path /path/to/omni-dev/Cargo.toml --bin omni-dev
python3 /path/to/omni-dev/docs/evaluations/jev-exists-2054/study.py \
  --binary /path/to/omni-dev/target/debug/omni-dev \
  --repo /path/to/succinctly --model jev-1.13.0 --output /tmp/jev-exists-study
```

Use `--dry-run-only` to reproduce retrieval without credentials or paid calls.
The runner saves each exact request, response/report and a summary, and stops on
authentication failure. Production endpoint, credentials and transport retries
come from the CLI's usual Jev settings. The source HEAD SHA in every report is
part of the evidence; mutable working files are excluded.

## Measurement limits

The #2052 labels describe model routing, not whether code already supplies an
issue's proposed implementation. They cannot serve as ground truth for existence
precision or recall. Independent, predeclared per-definition existence labels
are absent, so held-out precision, false-positive rate and recall are not
estimable from those labels. A missed source definition is a retrieval miss;
a missed oracle or body-level trap is an intentional capability limit. Token
usage and returned model are recorded in raw reports; monetary billing is not
returned by Jev and should not be invented from token counts.

The #3017 oracle probe and `Vec` double-push trap require execution or inspection
of function bodies. The filter cannot discover either, even if it retrieves
`any_pattern_key`. A high score must still be checked against code and behavior
before writing a finding or making a routing decision.
