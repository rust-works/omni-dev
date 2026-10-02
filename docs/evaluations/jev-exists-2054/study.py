#!/usr/bin/env python3
"""Reproduce existence retrieval/judgment on pinned text and a detached source checkout."""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile

parser = argparse.ArgumentParser()
parser.add_argument("--binary", required=True)
parser.add_argument("--repo", required=True)
parser.add_argument("--model", default="jev-1.13.0")
parser.add_argument("--output", required=True)
parser.add_argument("--dry-run-only", action="store_true")
args = parser.parse_args()
here = Path(__file__).resolve().parent
out = Path(args.output).resolve()
out.mkdir(parents=True, exist_ok=True)
cases = json.loads((here / "inputs.json").read_text())
diagnostic = here / "diagnostic-3017.json"
if diagnostic.exists():
    cases.append(json.loads(diagnostic.read_text()))
revision = subprocess.check_output(["git", "-C", args.repo, "rev-parse", "HEAD"], text=True).strip()
expected = json.loads((here / "provenance.json").read_text())["source_revision"]
if revision != expected:
    raise SystemExit(f"source HEAD {revision} differs from pinned {expected}")
summary = []
for case in cases:
    number = case["number"]
    with tempfile.TemporaryDirectory() as temporary:
        text = Path(temporary) / "issue.txt"
        text.write_text(case["title"] + "\n\n" + (case["body"] or ""))
        command = [str(Path(args.binary).resolve()), "ai", "jev", "exists", "--issue-file", str(text),
                   "-C", args.repo, "--jev-model", args.model]
        dry = subprocess.run(command + ["--dry-run"], text=True, capture_output=True)
        (out / f"{number}-request.json").write_text(dry.stdout)
        record = {"number": number, "retrieval_exit": dry.returncode}
        if dry.returncode:
            record["error"] = dry.stderr
        elif not args.dry_run_only:
            result = subprocess.run(command, text=True, capture_output=True)
            (out / f"{number}-result.json").write_text(result.stdout)
            record["judgment_exit"] = result.returncode
            if result.returncode:
                record["error"] = result.stderr
            else:
                report = json.loads(result.stdout)
                record.update(status=report["status"], model=report["model"], usage=report["usage"],
                              candidates=len(report["candidates"]),
                              high_scores=[{"path": c["path"], "line": c["line"], "symbol": c["symbol"], "score": c["score"]}
                                           for c in report["candidates"] if c["score"] >= 0.8])
        summary.append(record)
        (out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(f"#{number}: {record.get('status', record.get('error', 'retrieved'))}", flush=True)
        error = record.get("error", "")
        if any(message in error for message in ("credentials not configured", "HTTP 401", "HTTP 403")):
            print("Stopping study after authentication failure.", flush=True)
            break
