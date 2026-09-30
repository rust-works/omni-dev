#!/usr/bin/env python3
"""Summarize and archive raw #1885 runs: summarize.py E1_DIR HOLDOUT_DIR."""
from collections import Counter
import gzip
import json
import pathlib
import sys

VARIANTS = ("baseline", "versioned", "legacy", "candidate")
STAGES = ("design", "implement", "review")
RANK = {
    "none": 0, "sonnet": 1, "claude-sonnet-5-5": 1,
    "opus": 2, "claude-opus-5-5": 2, "fable": 3,
}


def record(directory, variant, number, repeat):
    return json.loads((directory / f"{variant}-{number}-{repeat}.json").read_text())


def choices(item):
    return tuple(item["answer"]["answers"][f"stage_{stage}"]["choice"] for stage in STAGES)


def issue_class(stage_choices):
    return max(stage_choices[:2], key=RANK.get)


def archive(directory, destination):
    data = {
        file.name: json.loads(file.read_text()) if file.suffix == ".json" else file.read_text()
        for file in sorted(directory.iterdir())
        if file.suffix in (".json", ".yaml")
    }
    with destination.open("wb") as output:
        with gzip.GzipFile(fileobj=output, mode="wb", filename="", mtime=0) as zipped:
            zipped.write((json.dumps(data, sort_keys=True, ensure_ascii=False) + "\n").encode())


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    e1, holdout = (pathlib.Path(name) for name in sys.argv[1:])
    labels = json.loads((e1 / "labels.json").read_text())
    numbers = [str(doc["number"]) for doc in json.loads((e1 / "inputs.json").read_text())]
    holdout_numbers = [
        str(doc["number"]) for doc in json.loads((holdout / "inputs.json").read_text())
    ]
    output = {}
    for variant in VARIANTS:
        counts = Counter()
        stages = {stage: Counter() for stage in STAGES}
        flips = Counter()
        matched = Counter()
        tokens = Counter()
        seconds = 0
        for number in numbers:
            stage_runs = []
            for repeat in (0, 1):
                item = record(e1, variant, number, repeat)
                if item["error"] or item["answer"]["model"] != "jev-1.13.0":
                    sys.exit(f"Invalid answer: {variant} #{number} repeat {repeat}")
                choice = choices(item)
                stage_runs.append(choice)
                winner = issue_class(choice)
                counts[winner] += 1
                for stage, selected in zip(STAGES, choice):
                    stages[stage][selected] += 1
                if variant == "legacy":
                    matched["original_three_rung"] += winner == labels[number]
                matched["collapsed_fable_to_opus"] += (
                    min(RANK[winner], 2) == min(RANK[labels[number]], 2)
                )
                tokens.update(item["answer"]["usage"])
                seconds += item["seconds"]
            flips["class"] += issue_class(stage_runs[0]) != issue_class(stage_runs[1])
            for stage, first, second in zip(STAGES, *stage_runs):
                flips[stage] += first != second
        output[variant] = {
            "calls": len(numbers) * 2,
            "class_counts": counts,
            "stage_counts": stages,
            "agreement_with_old_labels": matched,
            "repeat_flips": flips,
            "usage": tokens,
            "sum_request_seconds": round(seconds, 3),
        }

    comparisons = {}
    for variant in ("versioned", "legacy", "candidate"):
        differences = Counter()
        for number in numbers:
            for repeat in (0, 1):
                base = choices(record(e1, "baseline", number, repeat))
                other = choices(record(e1, variant, number, repeat))
                for stage, a, b in zip(STAGES, base, other):
                    differences[stage] += RANK[a] != RANK[b]
                differences["class"] += (
                    min(RANK[issue_class(base)], 2) != min(RANK[issue_class(other)], 2)
                )
        comparisons[variant] = differences
    holdouts = {}
    for number in holdout_numbers:
        holdouts[number] = {
            variant: [
                choices(record(holdout, variant, number, repeat))
                for repeat in (0, 1)
            ]
            for variant in VARIANTS
        }
    summary = {
        "model": "jev-1.13.0",
        "e1_issue_count": len(numbers),
        "repeats": 2,
        "e1": output,
        "paired_differences_from_baseline": comparisons,
        "holdout": holdouts,
        "limits": "Old hand labels are not downstream success or calibrated probabilities.",
    }
    here = pathlib.Path(__file__).resolve().parent
    (here / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    archive(e1, here / "e1-results.json.gz")
    archive(holdout, here / "holdout-results.json.gz")
    print(f"Wrote summary and archives: {len(numbers) * 2 * 4} E1, {len(holdout_numbers) * 2 * 4} holdout calls")


if __name__ == "__main__":
    main()
