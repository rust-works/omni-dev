"""Replay paired calls with explicit candidate wording and frozen base questions.

Usage: run_candidate.py INPUT.json NEW_OUTPUT_DIR CANDIDATE.json BASE_RESULTS.json.gz
Uses installed omni-dev and its configured Jev credentials. No secrets are copied.
"""
import gzip
import json
from pathlib import Path
import shutil
import subprocess
import sys

inputs, output, candidate, base_results = map(lambda p: Path(p).resolve(), sys.argv[1:])
cases = json.loads(inputs.read_text())
if not cases:
    raise SystemExit("inputs must be nonempty")
raw = base_results.read_bytes()
base_rows = json.loads(gzip.decompress(raw) if base_results.suffix == ".gz" else raw)
questions = next(row["request"]["questions"] for row in base_rows if row["mode"] == "baseline")
questions["open_questions"] = json.loads(candidate.read_text())
cli = shutil.which("omni-dev")
if not cli:
    raise SystemExit("omni-dev must be installed")
output.mkdir()  # Refuse mixing results in an existing directory.
shutil.copy(inputs, output / "inputs.json")
shutil.copy(base_results.parent / "tier-order.json", output / "tier-order.json")
(output / "cli-version.txt").write_text(subprocess.check_output([cli, "--version"], text=True))
rows = []
for repeat in range(2):
    for case in cases:
        modes = ["baseline", "augmented"] if repeat == 0 else ["augmented", "baseline"]
        for mode in modes:
            selected = {key: value for key, value in questions.items()
                        if mode == "augmented" or key != "open_questions"}
            question_file = output / "questions.json"
            question_file.write_text(json.dumps(selected))
            command = [cli, "ai", "jev", "ask", "--questions", str(question_file),
                       "--jev-model", "jev-1.13.0", "-o", "json"]
            result = subprocess.run(command, input=case["state"], text=True,
                                    capture_output=True, cwd=inputs.parent)
            response = {"response": json.loads(result.stdout)} if result.returncode == 0 else {
                "error": result.stderr, "exit_code": result.returncode}
            rows.append({"id": case["id"], "repeat": repeat, "mode": mode,
                         "request": {"state": case["state"], "model": "jev-1.13.0",
                                     "questions": selected}, "result": response})
            (output / "results.json").write_text(json.dumps(rows, indent=2) + "\n")
            print(case["id"], repeat, mode, flush=True)
