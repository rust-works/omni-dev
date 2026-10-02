"""Regression checks for scoring exclusions, failures and evidence integrity."""
import gzip
import hashlib
import json
from pathlib import Path
import tempfile
import unittest

import analyze


class AnalysisTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.original_root = analyze.ROOT
        self.addCleanup(setattr, analyze, "ROOT", self.original_root)
        analyze.ROOT = Path(self.temp.name)
        self.cases = []
        self.sources = []
        for n, klass in ((1, "sonnet"), (2, "fable")):
            self.cases.append({"number": n, "source_class": klass, "plan_comment_ids": [],
                               "open_question_kind": "none",
                               "source_stage_labels": {"implement": klass if n == 1 else None},
                               "evaluator_stage_expectations": {"design": "none"}})
            self.sources.append({"number": n, "issue": {"title": "t", "body": "b",
                                 "html_url": f"url{n}"}, "comment_pages": [[]]})
        self.write("labels.json", {"cases": self.cases})
        self.write("selection.json", [{"number": 1}, {"number": 2}])
        self.write("sources.json.gz", self.sources)
        declared = {"evaluation_numbers": [1, 2]}
        for filename, key in (("labels.json", "labels_sha256"),
                              ("sources.json.gz", "sources_sha256")):
            declared[key] = hashlib.sha256((analyze.ROOT / filename).read_bytes()).hexdigest()
        self.write("predeclared.json", declared)
        self.write("pricing.json", {"model": "jev-1.13.0"})
        self.write("github-responses.json.gz", {"fixture": {"args": ["api", "graphql"],
                   "stdout": json.dumps({"data": {"r0": {
                       f"i{n}": {"title": "t", "body": "b", "url": f"url{n}",
                                 "comments": {"nodes": []}} for n in (1, 2)}}})}})

    def write(self, filename, value):
        data = json.dumps(value).encode()
        if filename.endswith(".gz"):
            data = gzip.compress(data, mtime=0)
        (analyze.ROOT / filename).write_bytes(data)

    def report(self, issues):
        self.write("baseline.json.gz", {"issues": issues, "model": "test", "usage": {}})

    def routed(self, n, klass="sonnet"):
        return {"ref": f"rust-works/succinctly#{n}", "providers": {"anthropic": {
            "class": klass, "stages": {"design": {"choice": "none"},
            "implement": {"choice": klass}}, "close_calls": []}}}

    def test_unsupported_class_is_not_coerced_or_scored(self):
        self.report([self.routed(1), self.routed(2, "opus")])
        summary = analyze.summarize()
        self.assertEqual(summary["agreement"]["source_class"],
                         {"agree": 1, "scored": 1, "mismatches": []})
        self.assertEqual(summary["unsupported_classes"], [2])
        self.assertEqual(summary["agreement"]["evaluator_design"]["scored"], 2)

    def test_failure_and_missing_prediction_remain_visible(self):
        self.report([{"ref": "rust-works/succinctly#1", "error": "fetch failed"}])
        summary = analyze.summarize()
        self.assertEqual(summary["holdout_failures"], [1, 2])
        self.assertEqual(summary["agreement"], {})

    def test_predeclared_labels_cannot_be_changed_silently(self):
        self.write("labels.json", {"cases": []})
        with self.assertRaises(AssertionError):
            analyze.summarize()

    def test_source_drift_does_not_disappear_from_scoring(self):
        self.report([self.routed(1, "opus"), self.routed(2)])
        summary = analyze.summarize()
        self.assertEqual(summary["agreement"]["source_class"]["mismatches"], [1])
        self.assertEqual(summary["rows"][0]["input_drift"], [])
        self.write("github-responses.json.gz", {})
        self.assertEqual(analyze.summarize()["rows"][0]["input_drift"], ["input_not_captured"])


if __name__ == "__main__":
    unittest.main()
