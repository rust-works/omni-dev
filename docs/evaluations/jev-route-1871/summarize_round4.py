#!/usr/bin/env python3
"""Summarise the round-4 runs and evaluate the pre-registered decision rules.

Reads the run archives written by `examples/jev_route_signal_eval.rs`
(`*.json` or `*.json.gz`, arrays of observations), the labels frozen before any
live request (`round4-labels.json`, `corrected-labels.json`) and the earlier
corrected-comparison archive, which supplies the spike comparators. Writes the
summary JSON and prints the figures quoted in the README. Standard library only.

Usage: summarize_round4.py OUT_SUMMARY.json ARCHIVE [ARCHIVE ...]
Archives are named `<run>=<path>`, e.g. `A=runs/A.json`; the run letter selects
the interpretation (A asis, B variants, C spike, D1/D2 production-shaped).
"""
import collections
import gzip
import itertools
import json
import pathlib
import statistics
import sys

HERE = pathlib.Path(__file__).resolve().parent
BOTTOM = {"anthropic": "sonnet", "openai": "terra", "gemini": "flash"}
ORDER = {
    "anthropic": ["none", "sonnet", "opus"],
    "openai": ["none", "terra", "sol", "astra"],
    "gemini": ["none", "flash", "pro", "deep-think"],
}
CLOSE_CALL, CLOSE_MARGIN = 0.3, 0.2


def load(path):
    path = pathlib.Path(path)
    opener = gzip.open if path.suffix == ".gz" else open
    with opener(path, "rt") as handle:
        return json.load(handle)


def answers(o):
    return o["response"]["answers"]


def stage_key(ladder, stage):
    return f"{ladder}.stage_{stage}"


def ladders_of(o):
    return o.get("ladders") or ["anthropic"]


def p_above_floor(o, ladder="anthropic"):
    """P(implement is above the ladder's bottom tier)."""
    probs = answers(o)[stage_key(ladder, "implement")]["probabilities"]
    return 1.0 - probs.get(BOTTOM[ladder], 0.0)


def choice(o, ladder, stage):
    return answers(o)[stage_key(ladder, stage)]["choice"]


def is_close(a):
    probs = sorted(a["probabilities"].values(), reverse=True)
    margin = probs[0] - probs[1] if len(probs) > 1 else 1.0
    return a["confidence"] < CLOSE_CALL or margin < CLOSE_MARGIN


def class_of(o, ladder):
    order = ORDER[ladder]
    d, i = choice(o, ladder, "design"), choice(o, ladder, "implement")
    return d if order.index(d) > order.index(i) else i


def class_flagged(o, ladder):
    return any(is_close(answers(o)[stage_key(ladder, s)]) for s in ("design", "implement"))


def by_case(observations):
    grouped = collections.defaultdict(lambda: collections.defaultdict(list))
    for o in observations:
        grouped[o["id"]][o["variant"]].append(o)
    for variants in grouped.values():
        for runs in variants.values():
            runs.sort(key=lambda o: o["repeat"])
    return grouped


def rnd(x, n=3):
    return None if x is None else round(x, n)


def noul_scores(obs, key):
    return [answers(o)[key]["noul"] for o in obs if key in answers(o)]


def drift(runs):
    """Candidate-vs-baseline and baseline-vs-baseline stage/class changes."""
    pair_changes = pair_total = repeat_changes = repeat_total = 0
    class_changes = class_changes_unflagged = class_total = 0
    for run in runs:
        for variants in by_case(run["obs"]).values():
            base, cand = variants.get("baseline", []), variants.get("candidate", [])
            for b in base:
                for ladder in ladders_of(b):
                    for stage in ("design", "implement", "review"):
                        for c in (c for c in cand if c["repeat"] == b["repeat"]):
                            pair_total += 1
                            pair_changes += choice(b, ladder, stage) != choice(c, ladder, stage)
                    for c in (c for c in cand if c["repeat"] == b["repeat"]):
                        class_total += 1
                        if class_of(b, ladder) != class_of(c, ladder):
                            class_changes += 1
                            class_changes_unflagged += not class_flagged(b, ladder)
            for b0, b1 in itertools.combinations(base, 2):
                for ladder in ladders_of(b0):
                    for stage in ("design", "implement", "review"):
                        repeat_total += 1
                        repeat_changes += choice(b0, ladder, stage) != choice(b1, ladder, stage)
    return {
        "candidate_vs_baseline_stage_changes": pair_changes,
        "candidate_vs_baseline_stage_total": pair_total,
        "candidate_vs_baseline_rate": rnd(pair_changes / pair_total) if pair_total else None,
        "baseline_repeat_stage_changes": repeat_changes,
        "baseline_repeat_stage_total": repeat_total,
        "baseline_repeat_rate": rnd(repeat_changes / repeat_total) if repeat_total else None,
        "class_changes": class_changes,
        "class_changes_outside_a_close_call": class_changes_unflagged,
        "class_comparisons": class_total,
    }


def spike_comparators():
    """Earlier-round spike observations (candidate variant) and their labels."""
    labels = {l["id"]: l for l in load(HERE / "corrected-labels.json")}
    out = {}
    for o in load(HERE / "corrected-results.json.gz"):
        if o["variant"] != "candidate" or "bounded_spike" not in answers(o):
            continue
        label = labels[o["id"]]["bounded_spike"]
        kind = {"positive": "positive", "negative": "negative", "borderline": "borderline"}[label]
        if o["id"] == "succinctly-2640-current":
            kind = "positive_postrun"  # corrected after seeing scores; documented, not blind
        if o["id"] == "succinctly-2705-current":
            kind = "negative_conditional"
        out.setdefault(o["id"], {"kind": kind, "scores": [], "source": "corrected-results"})
        out[o["id"]]["scores"].append(answers(o)["bounded_spike"]["noul"])
    return out


def main():
    out_path = pathlib.Path(sys.argv[1])
    runs = {}
    for spec in sys.argv[2:]:
        name, path = spec.split("=", 1)
        runs[name] = {"name": name, "obs": load(path)}
    labels = load(HERE / "round4-labels.json")
    cases_by_id = {c["id"]: c for c in load(HERE / "round4-inputs.json")}
    variant_inputs = {c["id"]: c for c in load(HERE / "round4-variant-inputs.json") + load(HERE / "round4-posthoc-inputs.json")}

    def edited_position(base_id, tag):
        """Index, in the base case's citations, of the citation an edit rewrote."""
        return str(cases_by_id[base_id]["citations"].index(variant_inputs[f"{base_id}-{tag}-open"]["edit"]["citation"]))

    summary = {"runs": {}, "model": sorted({o["response"]["model"] for r in runs.values() for o in r["obs"]})}

    usage = collections.defaultdict(lambda: [0, 0, 0])
    first = collections.Counter()
    for name, run in runs.items():
        for o in run["obs"]:
            u = o["response"]["usage"]
            usage[(name, o["variant"])][0] += u["input_tokens"]
            usage[(name, o["variant"])][1] += u["output_tokens"]
            usage[(name, o["variant"])][2] += 1
        summary["runs"][name] = {"requests": len(run["obs"])}
        cases = by_case(run["obs"])
        for variants in cases.values():
            for r in {o["repeat"] for o in run["obs"]}:
                both = [o for v in variants.values() for o in v if o["repeat"] == r]
                if len(both) == 2:
                    first[min(both, key=lambda o: o["sequence"])["variant"]] += 1
    summary["usage"] = {f"{k[0]}/{k[1]}": {"input_tokens": v[0], "output_tokens": v[1], "requests": v[2]} for k, v in sorted(usage.items())}
    summary["first_variant_counts"] = dict(first)

    # ---- implementation floor and scores (runs A and B) -------------------------
    impl = {}
    for name in ("A", "B"):
        if name not in runs:
            continue
        for case_id, variants in by_case(runs[name]["obs"]).items():
            base, cand = variants.get("baseline", []), variants.get("candidate", [])
            entry = {
                "baseline_implement_choices": [choice(o, "anthropic", "implement") for o in base],
                "baseline_p_above_floor": [rnd(p_above_floor(o)) for o in base],
                "baseline_design_choices": [choice(o, "anthropic", "design") for o in base],
                "open_questions_baseline": [answers(o)["open_questions"]["choice"] for o in base],
                "candidate_implement_bearing": {},
                "design_could_be_cheaper": {},
                "spike_score": [rnd(s) for s in noul_scores(cand, "bounded_spike")],
            }
            keys = sorted({k for o in cand for k in answers(o) if k.startswith("could_be_cheaper_implement_")})
            for key in keys:
                entry["candidate_implement_bearing"][key.rsplit("_", 1)[1]] = [rnd(s) for s in noul_scores(cand, key)]
            for key in sorted({k for o in base for k in answers(o) if k.startswith("could_be_cheaper_") and "implement" not in k}):
                entry["design_could_be_cheaper"][key.rsplit("_", 1)[1]] = [rnd(s) for s in noul_scores(base, key)]
            entry["above_floor_in_majority"] = sum(
                c != BOTTOM["anthropic"] for c in entry["baseline_implement_choices"]
            ) * 2 > len(entry["baseline_implement_choices"])
            impl[case_id] = entry
    summary["implementation"] = impl
    asis = {k: v for k, v in impl.items() if "-noclass" not in k and "-inject" not in k}
    noclass = {k: v for k, v in impl.items() if k.endswith("-noclass")}
    summary["i1_above_floor_cases"] = {
        "asis": sorted(k for k, v in asis.items() if v["above_floor_in_majority"]),
        "noclass": sorted(k for k, v in noclass.items() if v["above_floor_in_majority"]),
        "required": 4,
    }
    yes_pairs = [p for p in labels["implementation_bearing"] if p["author"] == "yes" and p["blind"] == "yes"]
    no_pairs = [p for p in labels["implementation_bearing"] if p["author"] == "no" and p["blind"] == "no"]
    summary["i2_natural_pairs"] = {"yes": len(yes_pairs), "no": len(no_pairs), "required_each": 2}
    natural_no_scores = []
    for p in no_pairs:
        entry = impl.get(p["id"])
        if not entry:
            continue
        position = cases_by_id[p["id"]]["citations"].index(p["citation"])
        scores = entry["candidate_implement_bearing"].get(str(position), [])
        natural_no_scores.append({"id": p["id"], "citation": p["citation"], "scores": scores})
    summary["natural_no_pair_implement_scores"] = natural_no_scores

    inject = {}
    for base_id in ("succinctly-2800-r4", "succinctly-2799-r4"):
        natural = impl.get(base_id)
        opened = impl.get(base_id + "-inject-open")
        resolved = impl.get(base_id + "-inject-resolved")
        if not (natural and opened and resolved):
            continue
        position = edited_position(base_id, "inject")
        inject[base_id] = {
            "natural_baseline_p_above_floor": natural["baseline_p_above_floor"],
            "inject_open_baseline_p_above_floor": opened["baseline_p_above_floor"],
            "inject_resolved_baseline_p_above_floor": resolved["baseline_p_above_floor"],
            "natural_candidate_score": natural["candidate_implement_bearing"].get(position),
            "inject_open_candidate_score": opened["candidate_implement_bearing"].get(position),
        }
        mean = statistics.fmean
        inject[base_id]["resolved_minus_open_p_above_floor"] = rnd(
            mean(resolved["baseline_p_above_floor"]) - mean(opened["baseline_p_above_floor"])
        )
    summary["blocker_simulation"] = inject

    # ---- is the implementation question redundant with the design one? ----------
    pairs = []
    for case_id, entry in asis.items():
        for i, citation in enumerate(cases_by_id[case_id]["citations"]):
            design = entry["design_could_be_cheaper"].get(str(i))
            implement = entry["candidate_implement_bearing"].get(str(i))
            if design and implement:
                pairs.append({"id": case_id, "citation": citation, "source": "round4-A",
                              "design": rnd(statistics.fmean(design)), "implement": rnd(statistics.fmean(implement))})
    earlier = by_case(load(HERE / "corrected-results.json.gz"))
    for case_id, variants in earlier.items():
        base, cand = variants.get("baseline", []), variants.get("candidate", [])
        design = noul_scores(base, "could_be_cheaper_0")
        implement = noul_scores(cand, "could_be_cheaper_implement_0")
        if design and implement:
            pairs.append({"id": case_id, "citation": "(first)", "source": "corrected-results",
                          "design": rnd(statistics.fmean(design)), "implement": rnd(statistics.fmean(implement))})
    diffs = [abs(p["design"] - p["implement"]) for p in pairs]
    d, im = [p["design"] for p in pairs], [p["implement"] for p in pairs]
    mean_d, mean_i = statistics.fmean(d), statistics.fmean(im)
    cov = sum((x - mean_d) * (y - mean_i) for x, y in zip(d, im))
    var = (sum((x - mean_d) ** 2 for x in d) * sum((y - mean_i) ** 2 for y in im)) ** 0.5
    summary["design_vs_implement_could_be_cheaper"] = {
        "pairs": pairs,
        "count": len(pairs),
        "pearson_r": rnd(cov / var) if var else None,
        "mean_abs_difference": rnd(statistics.fmean(diffs)),
        "max_abs_difference": rnd(max(diffs)),
    }

    # ---- post-hoc absorbed-work simulation (run E; exploratory, never a gate) ----
    if "E" in runs:
        posthoc = {}
        for base_id in ("succinctly-2800-r4", "succinctly-2799-r4"):
            cases = by_case(runs["E"]["obs"])
            opened, resolved = cases[base_id + "-absorb-open"], cases[base_id + "-absorb-resolved"]
            natural = impl[base_id]
            position = edited_position(base_id, "absorb")
            posthoc[base_id] = {
                "natural_implement_score": natural["candidate_implement_bearing"].get(position),
                "absorb_open_implement_score": [rnd(s) for s in noul_scores(opened["candidate"], f"could_be_cheaper_implement_{position}")],
                "natural_design_score": natural["design_could_be_cheaper"].get(position),
                "absorb_open_design_score": [rnd(s) for s in noul_scores(opened["candidate"], f"could_be_cheaper_{position}")],
                "open_p_above_floor": [rnd(p_above_floor(o)) for o in opened["baseline"]],
                "resolved_p_above_floor": [rnd(p_above_floor(o)) for o in resolved["baseline"]],
            }
            posthoc[base_id]["resolved_minus_open_p_above_floor"] = rnd(
                statistics.fmean(posthoc[base_id]["resolved_p_above_floor"])
                - statistics.fmean(posthoc[base_id]["open_p_above_floor"])
            )
        summary["posthoc_absorb_simulation"] = posthoc

    # ---- spike (run C plus earlier comparators) ---------------------------------
    spike_labels = {s["id"]: s for s in labels["spike"]}
    spike = {}
    if "C" in runs:
        for case_id, variants in by_case(runs["C"]["obs"]).items():
            lab = spike_labels[case_id]
            # Only an agreed label counts. A disagreement across the positive line
            # is excluded as borderline rather than resolved toward either reader,
            # and "conditional plan" is kept only when both readers said so.
            negative_like = ("negative", "negative_conditional")
            if lab["author"] == lab["blind"]:
                kind = lab["author"]
            elif lab["author"] in negative_like and lab["blind"] in negative_like:
                kind = "negative"
            else:
                kind = "borderline"
            binary = "positive" if kind == "positive" else ("borderline" if kind == "borderline" else "negative")
            base = variants.get("baseline", [])
            spike[case_id] = {
                "kind": kind,
                "binary": binary,
                "scores": [rnd(s) for s in noul_scores(variants.get("candidate", []), "bounded_spike")],
                "source": "round4-C",
                "open_questions_baseline": [answers(o)["open_questions"]["choice"] for o in base],
            }
    for case_id, c in spike_comparators().items():
        spike[case_id] = {
            "kind": c["kind"],
            "binary": "positive" if c["kind"].startswith("positive") else ("borderline" if c["kind"] == "borderline" else "negative"),
            "scores": [rnd(s) for s in c["scores"]],
            "source": c["source"],
        }
    summary["spike"] = spike
    positives = {k: v for k, v in spike.items() if v["binary"] == "positive"}
    negatives = {k: v for k, v in spike.items() if v["binary"] == "negative"}
    conditional = [k for k, v in negatives.items() if v["kind"] == "negative_conditional"]
    blind_positives = {k: v for k, v in positives.items() if v["source"] == "round4-C"}

    def gap(pos, neg):
        if not pos or not neg:
            return None
        return rnd(min(min(v["scores"]) for v in pos.values()) - max(max(v["scores"]) for v in neg.values()))

    pairs = [(p, n) for p in positives.values() for n in negatives.values()]
    separated = sum(min(p["scores"]) > max(n["scores"]) for p, n in pairs)
    summary["spike_gate"] = {
        "positives": sorted(positives),
        "blind_double_labelled_positives": sorted(blind_positives),
        "negatives": len(negatives),
        "conditional_plan_negatives": sorted(conditional),
        "min_positive_minus_max_negative": gap(positives, negatives),
        "blind_positives_only_gap": gap(blind_positives, negatives),
        "pairs_fully_separated": f"{separated}/{len(pairs)}",
        "max_repeat_spread": rnd(max((max(v["scores"]) - min(v["scores"]) for v in spike.values() if len(v["scores"]) > 1), default=0)),
        "required": {"positives": 5, "negatives": 5, "conditional": 3, "gap": 0.15, "spread": 0.07},
    }
    summary["spike_vs_open_questions"] = {
        k: {"binary": v["binary"], "open_questions": collections.Counter(v.get("open_questions_baseline", [])).most_common(1)}
        for k, v in spike.items() if v.get("open_questions_baseline")
    }

    # ---- batching drift (runs A, B, C, D) ---------------------------------------
    summary["drift"] = {
        "single_ladder_class_only": drift([runs[n] for n in ("A", "B", "C") if n in runs]),
        "production_shaped": drift([runs[n] for n in ("D1", "D2") if n in runs]),
    }
    out_path.write_text(json.dumps(summary, indent=2, ensure_ascii=False) + "\n")

    print("models", summary["model"], "requests", {k: v["requests"] for k, v in summary["runs"].items()})
    print("I1 above-floor (>=4 needed):", summary["i1_above_floor_cases"])
    print("I2 natural pairs:", summary["i2_natural_pairs"])
    print("blocker simulation:", json.dumps(summary["blocker_simulation"]))
    print("design vs implement could_be_cheaper:", json.dumps({k: v for k, v in summary["design_vs_implement_could_be_cheaper"].items() if k != "pairs"}))
    print("post-hoc absorb:", json.dumps(summary.get("posthoc_absorb_simulation")))
    print("spike gate:", json.dumps(summary["spike_gate"]))
    print("drift:", json.dumps(summary["drift"]))
    print("usage:", json.dumps(summary["usage"]))


if __name__ == "__main__":
    main()
