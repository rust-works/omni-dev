#!/usr/bin/env python3
"""Record/replay only the read-only gh commands used by the route baseline.

Set ROUTE_GH_FIXTURE_DIR to a new directory to record, or set ROUTE_GH_REPLAY
to an existing directory to replay. No Jev traffic or credentials are recorded.
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys


def main():
    args = sys.argv[1:]
    if not (args[:2] in (["api", "graphql"], ["repo", "view"], ["issue", "list"])):
        raise SystemExit("fixture refuses commands other than read-only route fetches")
    if args[:2] == ["api", "graphql"] and not (
        len(args) == 4 and args[2] == "-f" and args[3].startswith("query=query{")
    ):
        raise SystemExit("fixture requires the route's read-only GraphQL query shape")
    key = hashlib.sha256(json.dumps(args).encode()).hexdigest()
    replay = os.environ.get("ROUTE_GH_REPLAY")
    root = Path(replay or os.environ["ROUTE_GH_FIXTURE_DIR"])
    if replay:
        record = json.loads((root / (key + ".json")).read_text())
        if record["args"] != args:
            raise SystemExit("fixture key collision")
    else:
        root.mkdir(parents=True, exist_ok=True)
        result = subprocess.run(
            [os.environ.get("ROUTE_REAL_GH", "/opt/homebrew/bin/gh"), *args],
            capture_output=True, text=True,
        )
        record = {"args": args, "stdout": result.stdout,
                  "stderr": result.stderr, "exit_code": result.returncode}
        path = root / (key + ".json")
        if path.exists() and json.loads(path.read_text()) != record:
            raise SystemExit("response changed for the same command; use a new recording")
        path.write_text(json.dumps(record, indent=2) + "\n")
    sys.stdout.write(record["stdout"])
    sys.stderr.write(record["stderr"])
    return record["exit_code"]


if __name__ == "__main__":
    sys.exit(main())
