#!/usr/bin/env python3
"""Rerun #1779's frozen E1 inputs against the #1885 routing ladders.

Usage: python3 run.py OUTPUT_DIR [REPEATS] [INPUTS.json]
Requires PyYAML, configured Jev credentials, and the omni-dev CLI on PATH.
OUTPUT_DIR must be new. Outputs contain inputs, questions and raw answers.
"""
import concurrent.futures
import json
import pathlib
import subprocess
import sys
import time

import yaml

HERE = pathlib.Path(__file__).resolve().parent
SOURCE = HERE.parent / "jev-effort-1888"
MODEL = "jev-1.13.0"
BAR = (
    " Choose the least capable class likely to complete this stage correctly with no rework, "
    "about 9 times in 10. Judge the work that remains given the text, not the size of the text."
)
STAGES = {
    "stage_design": (
        "Which class should do the design work that remains before implementation can start: "
        "choosing the approach, settling open questions, and writing a plan?"
    ),
    "stage_implement": (
        "Assume any remaining design work has been completed well. Which class should write "
        "the code, tests and docs?"
    ),
    "stage_review": (
        "Which class should review the finished change before merge, so that mistakes the "
        "automated tests would miss are caught?"
    ),
}
NONE = "No design work remains: the text already settles the approach and the open questions."


def questions(tiers):
    criteria = {tier["name"]: tier["description"] for tier in tiers}
    return {
        stage: {
            "type": "choice",
            "instructions": prompt + BAR,
            "criteria": ({"none": NONE, **criteria} if stage == "stage_design" else criteria),
        }
        for stage, prompt in STAGES.items()
    }


def run(job):
    variant, number, repeat, state, questions_file, output = job
    start = time.monotonic()
    command = [
        "omni-dev", "ai", "jev", "ask", "--questions", str(questions_file),
        "--jev-model", MODEL, "-o", "json",
    ]
    try:
        result = subprocess.run(command, input=state, text=True, capture_output=True, timeout=120)
        answer = json.loads(result.stdout) if result.returncode == 0 else None
        error = result.stderr.strip() if result.returncode else None
    except (subprocess.TimeoutExpired, json.JSONDecodeError) as exc:
        answer, error = None, str(exc)
    record = {
        "variant": variant, "number": number, "repeat": repeat,
        "seconds": round(time.monotonic() - start, 3), "answer": answer, "error": error,
    }
    (output / f"{variant}-{number}-{repeat}.json").write_text(
        json.dumps(record, indent=2, ensure_ascii=False) + "\n"
    )
    return record


def main():
    if len(sys.argv) not in (2, 3, 4):
        sys.exit(__doc__)
    output = pathlib.Path(sys.argv[1]).resolve()
    repeats = int(sys.argv[2]) if len(sys.argv) == 3 else 2
    if repeats < 1 or output.exists():
        sys.exit("REPEATS must be positive and OUTPUT_DIR must be new")
    output.mkdir(parents=True)
    ladders = yaml.safe_load((HERE / "ladders.yaml").read_text())
    inputs = pathlib.Path(sys.argv[3]) if len(sys.argv) == 4 else SOURCE / "inputs.json"
    docs = json.loads(inputs.read_text())
    (output / "inputs.json").write_text(json.dumps(docs, indent=2, ensure_ascii=False) + "\n")
    if len(sys.argv) != 4:
        labels = json.loads((SOURCE / "labels.json").read_text())
        (output / "labels.json").write_text(json.dumps(labels, indent=2) + "\n")
    jobs = []
    for variant, tiers in ladders.items():
        question_file = output / f"{variant}-questions.yaml"
        question_file.write_text(yaml.safe_dump(questions(tiers), sort_keys=False, width=1000))
        for doc in docs:
            state = f"# #{doc['number']} {doc['title']}\n\n{doc['body'].strip()}\n"
            for repeat in range(repeats):
                jobs.append((variant, doc["number"], repeat, state, question_file, output))
    with concurrent.futures.ThreadPoolExecutor(max_workers=6) as pool:
        records = list(pool.map(run, jobs))
    failures = [r for r in records if r["error"]]
    print(f"{len(records) - len(failures)}/{len(records)} requests succeeded")
    for record in failures[:5]:
        print(f"{record['variant']} #{record['number']} repeat {record['repeat']}: {record['error']}")
    if len(failures) > 5:
        print(f"... and {len(failures) - 5} more failures; see the output files")
    if failures:
        sys.exit(1)


if __name__ == "__main__":
    main()
