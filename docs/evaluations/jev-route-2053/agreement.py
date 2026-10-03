"""Effective-sample statistics for a paired open-question run.

`summarize.py` counts every repeat as an independent prediction. Repeats of one
frozen state are not independent: when they agree, they add no information. This
reports the figures that account for that, plus the baselines the raw agreement
should be read against. Input layout and rows are the same as `summarize.py`.

Each issue counts once: its repeats share its weight equally, so a count can be
fractional when an issue's repeats disagree. Counts are out of the number of
issues, not the number of calls.
"""
import collections
import gzip
import json
import math
import pathlib
import sys

RETRIEVAL = {"factual", "both"}  # kinds for which reading code is expected to help

root = pathlib.Path(sys.argv[1])
inputs = {case["id"]: case for case in json.loads((root / "inputs.json").read_text())}
tier_order = json.loads((root / "tier-order.json").read_text())
rows_path = root / "results.json"
rows = json.loads(rows_path.read_text() if rows_path.exists()
                  else gzip.decompress((root / "results.json.gz").read_bytes()))


def wilson(successes, n, z=1.96):
    p = successes / n
    centre = (p + z * z / (2 * n)) / (1 + z * z / n)
    half = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / (1 + z * z / n)
    return [round(centre - half, 3), round(centre + half, 3)]


def binomial_tail(k, n, p):
    """P(X >= k) for X ~ Binomial(n, p)."""
    return sum(math.comb(n, i) * p ** i * (1 - p) ** (n - i) for i in range(k, n + 1))


def fisher(a, b, c, d):
    """Two-sided Fisher exact p for [[a, b], [c, d]]."""
    row1, col1, total = a + b, a + c, a + b + c + d

    def prob(x):
        return math.comb(col1, x) * math.comb(total - col1, row1 - x) / math.comb(total, row1)

    observed = prob(a)
    low, high = max(0, row1 - (total - col1)), min(row1, col1)
    return round(min(1.0, sum(prob(x) for x in range(low, high + 1)
                              if prob(x) <= observed * (1 + 1e-9))), 4)


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


cells = collections.defaultdict(dict)  # (id, repeat) -> mode -> row
for row in rows:
    if row["result"].get("response") is not None:
        cells[row["id"], row["repeat"]][row["mode"]] = row

# One prediction per issue: the answers of the repeats, and whether they agree.
kinds = collections.defaultdict(dict)
for (case_id, repeat), modes in cells.items():
    if "augmented" in modes:
        kinds[case_id][repeat] = modes["augmented"]["result"]["response"]["answers"][
            "open_questions"]["choice"]
unambiguous = {i: k for i, k in kinds.items() if len(inputs[i]["expected"]) == 1}
issues = sorted(unambiguous)
expected = {i: inputs[i]["expected"][0] for i in issues}
n = len(issues)
agreeing = sum(len(set(unambiguous[i].values())) == 1 for i in issues)
# (expected, predicted, weight): one issue weighs 1 however many repeats it has.
units = [(expected[i], kind, 1 / len(unambiguous[i]))
         for i in issues for kind in unambiguous[i].values()]


def count(test):
    return sum(weight for exp, got, weight in units if test(exp, got))


def num(value):
    return int(round(value)) if abs(value - round(value)) < 1e-9 else round(value, 2)


correct = count(lambda exp, got: exp == got)
majority_label, majority = collections.Counter(expected.values()).most_common(1)[0]
labels = sorted(set(expected.values()) | {got for _, got, _ in units})
chance = sum(count(lambda exp, got, k=k: exp == k) / n * count(lambda exp, got, k=k: got == k) / n
             for k in labels)
per_class = {
    label: {"predicted": num(count(lambda exp, got, k=label: got == k)),
            "expected": num(count(lambda exp, got, k=label: exp == k)),
            "correct": num(count(lambda exp, got, k=label: exp == got == k))}
    for label in labels}

tp = count(lambda exp, got: got in RETRIEVAL and exp in RETRIEVAL)
fp = count(lambda exp, got: got in RETRIEVAL and exp not in RETRIEVAL)
fn = count(lambda exp, got: got not in RETRIEVAL and exp in RETRIEVAL)
tn = n - tp - fp - fn
needs_retrieval = tp + fn


def changes(extract, pair_of):
    """(changed, compared) between the two rows that `pair_of` yields."""
    changed = compared = 0
    for case_id, repeat in cells:
        pair = pair_of(case_id, repeat)
        if pair is None:
            continue
        before, after = extract(pair[0]), extract(pair[1])
        for key in before.keys() | after.keys():
            compared += 1
            changed += before.get(key) != after.get(key)
    return changed, compared


def between(mode_a, mode_b, *, same_repeat):
    def pair_of(case_id, repeat):
        other = (case_id, repeat) if same_repeat else (case_id, repeat - 1)
        a, b = cells.get(other, {}).get(mode_a), cells.get((case_id, repeat), {}).get(mode_b)
        return (a, b) if a and b and (same_repeat or repeat > 0) else None
    return pair_of


noise = {}
for name, extract in [("stage_choices", choices), ("classes", classes)]:
    effect = changes(extract, between("baseline", "augmented", same_repeat=True))
    baseline = changes(extract, between("baseline", "baseline", same_repeat=False))
    augmented = changes(extract, between("augmented", "augmented", same_repeat=False))
    noise[name] = {
        "added_question_vs_baseline": list(effect),
        "baseline_repeat_to_repeat": list(baseline),
        "augmented_repeat_to_repeat": list(augmented),
        "fisher_p_effect_vs_baseline_noise": fisher(
            effect[0], effect[1] - effect[0], baseline[0], baseline[1] - baseline[0]),
    }

print(json.dumps({
    "issues": n,
    "issues_with_identical_repeats": agreeing,
    "correct_issues": num(correct),
    "accuracy": round(correct / n, 3),
    "accuracy_wilson_95": wilson(correct, n),
    "majority_label": majority_label,
    "majority_baseline": [majority, n],
    # A fractional count is rounded up, which can only understate how close to the
    # baseline the classifier is.
    "p_accuracy_at_least_this_given_majority_rate": round(
        binomial_tail(math.ceil(correct - 1e-9), n, majority / n), 3),
    "cohen_kappa": round((correct / n - chance) / (1 - chance), 3),
    "per_class": per_class,
    "retrieval_gate": {
        "needs_retrieval": [num(needs_retrieval), n],
        "true_positive": num(tp), "false_positive": num(fp),
        "false_negative": num(fn), "true_negative": num(tn),
        "accuracy": [num(tp + tn), n],
        "always_yes_accuracy": [num(needs_retrieval), n],
        "always_no_accuracy": [num(n - needs_retrieval), n],
    },
    "existing_answer_changes_[changed,compared]": noise,
}, indent=2, sort_keys=True))
