#!/usr/bin/env python3
"""Audit the frozen artifact and reproduce summary.json without API access."""
from collections import Counter
import gzip
import hashlib
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent


def read(name):
    path = ROOT / name
    data = gzip.decompress(path.read_bytes()) if name.endswith(".gz") else path.read_bytes()
    return json.loads(data)


def summarize():
    declared = read("predeclared.json")
    for name in ("labels.json", "sources.json.gz"):
        key = "labels_sha256" if name == "labels.json" else "sources_sha256"
        assert hashlib.sha256((ROOT / name).read_bytes()).hexdigest() == declared[key], name
    labels = read("labels.json")["cases"]
    numbers = declared["evaluation_numbers"]
    assert len(numbers) == len(set(numbers)) == len(labels)
    assert sorted(numbers) == sorted(x["number"] for x in labels)
    assert sorted(numbers) == sorted(x["number"] for x in read("selection.json"))
    sources = {x["number"]: x for x in read("sources.json.gz")}
    assert sorted(sources) == sorted(numbers)
    report = read("baseline.json.gz")
    predictions = {x["ref"]: x for x in report["issues"]}
    assert len(predictions) == len(report["issues"]), "duplicate predictions"
    # Retain source drift separately from model disagreement.
    consumed = {}
    fixtures = read("github-responses.json.gz")
    for fixture in fixtures.values():
        if fixture["args"][:2] != ["api", "graphql"]:
            continue
        try:
            data = json.loads(fixture["stdout"]).get("data") or {}
        except json.JSONDecodeError:
            continue
        for repo in data.values():
            if not isinstance(repo, dict):
                continue
            for node in repo.values():
                if isinstance(node, dict) and "body" in node and "comments" in node:
                    consumed[node["url"]] = node
    rows = []
    for label in labels:
        n = label["number"]
        ref = f"rust-works/succinctly#{n}"
        result = predictions.get(ref)
        source = sources[n]
        comments = [c for page in source["comment_pages"] for c in page]
        assert set(label["plan_comment_ids"]) <= {c["id"] for c in comments}
        node = consumed.get(source["issue"]["html_url"])
        drift = []
        if node is None:
            drift.append("input_not_captured")
        else:
            for field in ("title", "body"):
                if node[field] != source["issue"][field]:
                    drift.append(field)
            frozen_comments = [(c["id"], (c.get("user") or {}).get("login", "ghost"), c["body"]) for c in comments
                               if (c.get("user") or {}).get("type") != "Bot"]
            actual_comments = [(c.get("databaseId"), (c.get("author") or {}).get("login", "ghost"), c["body"])
                               for c in node["comments"]["nodes"]
                               if (c.get("author") or {}).get("__typename") != "Bot"]
            if frozen_comments != actual_comments:
                drift.append("comments")
        row = {"number": n, "source_class": label["source_class"],
               "input_drift": drift, "open_question_kind": label["open_question_kind"],
               "checks": {}}
        if result is None or "error" in result:
            row["error"] = "not_in_baseline" if result is None else result["error"]
        else:
            provider = result["providers"]["anthropic"]
            row.update({"class": provider["class"], "stages": provider["stages"],
                        "class_from": provider.get("class_from"),
                        "close_calls": provider["close_calls"],
                        "truncated": result.get("truncated", False),
                        "reference_fetch_failures": result.get("reference_fetch_failures", [])})
            if label["source_class"] in ("sonnet", "opus"):
                row["checks"]["source_class"] = provider["class"] == label["source_class"]
            for origin, expected in (("source", label["source_stage_labels"]),
                                     ("evaluator", label["evaluator_stage_expectations"])):
                for stage, choice in expected.items():
                    if choice is not None:
                        row["checks"][f"{origin}_{stage}"] = provider["stages"][stage]["choice"] == choice
        rows.append(row)
    metrics = {}
    for key in sorted({key for row in rows for key in row["checks"]}):
        checks = [row["checks"][key] for row in rows if key in row["checks"]]
        metrics[key] = {"agree": sum(checks), "scored": len(checks),
                        "mismatches": [row["number"] for row in rows if row["checks"].get(key) is False]}
    confusion = Counter((row["source_class"], row["class"]) for row in rows
                        if "source_class" in row["checks"])
    pricing = read("pricing.json")
    cost = None
    if report["model"] == pricing["model"]:
        cost = (report["usage"]["input_tokens"] * pricing["input_per_million"]
                + report["usage"]["output_tokens"] * pricing["output_per_million"]) / 1_000_000
    return {"holdout_count": len(rows), "all_open_count": len(predictions),
            "model": report["model"], "usage": report["usage"], "agreement": metrics,
            "estimated_list_price_usd": cost,
            "source_class_confusion": {f"{expected}->{actual}": count
                                       for (expected, actual), count in sorted(confusion.items())},
            "all_open_failures": [x for x in report["issues"] if "error" in x],
            "all_open_reference_fetch_failures": [
                {"ref": x["ref"], "failures": x["reference_fetch_failures"]}
                for x in report["issues"] if x.get("reference_fetch_failures")],
            "all_open_truncated": [x["ref"] for x in report["issues"] if x.get("truncated")],
            "holdout_failures": [row["number"] for row in rows if "error" in row],
            "unsupported_classes": [row["number"] for row in rows
                                    if row["source_class"] not in ("sonnet", "opus")],
            "open_question_counts": dict(sorted(Counter(row["open_question_kind"] for row in rows).items())),
            "rows": rows}


if __name__ == "__main__":
    print(json.dumps(summarize(), indent=2, sort_keys=True))
