"""Summarize paired raw calls without discarding mismatches or failures."""
import collections
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
inputs = {case["id"]: case for case in json.loads((root / "inputs.json").read_text())}
tier_order = json.loads((root / "tier-order.json").read_text())
rows = json.loads((root / "results.json").read_text())
summary = {
    "models": [], "usage": {"input_tokens": 0, "output_tokens": 0},
    "failures": [], "predictions": [], "confusion": {},
    "stage_changes": [], "class_changes": [], "baseline_repeat_changes": [],
    "billed_cost": None,
}
pairs = collections.defaultdict(dict)
models = set()
confusion = collections.Counter()
for row in rows:
    response = row["result"].get("response")
    if response is None:
        summary["failures"].append(row)
        continue
    models.add(response["model"])
    for key in summary["usage"]:
        summary["usage"][key] += response.get("usage", {}).get(key, 0)
    pairs[row["id"], row["repeat"]][row["mode"]] = row
    if row["mode"] == "augmented":
        choice = response["answers"].get("open_questions", {}).get("choice")
        expected = inputs[row["id"]]["expected"]
        summary["predictions"].append({
            "id": row["id"], "repeat": row["repeat"], "choice": choice,
            "expected": expected, "accepted": choice in expected,
            "ambiguous": len(expected) > 1,
        })
        if len(expected) == 1:
            confusion[expected[0], choice] += 1


def choices(row):
    return {key: answer.get("choice") for key, answer in
            row["result"]["response"]["answers"].items() if ".stage_" in key}


def classes(row):
    answers = choices(row)
    result = {}
    for key in row["request"]["questions"]:
        if not key.endswith(".stage_implement"):
            continue
        provider = key.removesuffix(".stage_implement")
        order = tier_order[provider]
        design = answers.get(provider + ".stage_design")
        implement = answers.get(key)
        rank = lambda value: order.index(value) if value in order else -1
        result[provider] = design if rank(design) > rank(implement) else implement
    return result


for (case_id, repeat), pair in pairs.items():
    if set(pair) != {"baseline", "augmented"}:
        continue
    for category, extract in [("stage_changes", choices), ("class_changes", classes)]:
        before, after = extract(pair["baseline"]), extract(pair["augmented"])
        for key in before.keys() | after.keys():
            if before.get(key) != after.get(key):
                summary[category].append({"id": case_id, "repeat": repeat,
                                         "key": key, "before": before.get(key),
                                         "after": after.get(key)})
    prior = pairs.get((case_id, repeat - 1), {}).get("baseline")
    if prior:
        before, after = choices(prior), choices(pair["baseline"])
        for key in before.keys() | after.keys():
            if before.get(key) != after.get(key):
                summary["baseline_repeat_changes"].append({
                    "id": case_id, "repeat": repeat, "key": key,
                    "before": before.get(key), "after": after.get(key)})
summary["models"] = sorted(models)
summary["confusion"] = {f"{expected} -> {actual}": count
                        for (expected, actual), count in confusion.items()}
print(json.dumps(summary, indent=2))
