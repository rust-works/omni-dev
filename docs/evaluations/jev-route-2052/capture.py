"""Run a fresh live all-open baseline: capture.py SUCCINCTLY_CHECKOUT OUTPUT_DIR.

Requires a built target/debug/omni-dev and configured Jev credentials.
"""
import datetime
import gzip
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time


def capture(wt, checkout, output):
    """Preserve raw results even on failure, then return the CLI exit status."""
    output.mkdir(parents=True, exist_ok=False)
    fixture = output / "github-responses"
    binary = wt / "target/debug/omni-dev"
    shim = Path(__file__).with_name("gh_fixture.py")
    command = [str(binary), "ai", "jev", "route", "--all-open", "--refresh",
               "--ladders", "anthropic", "--jev-model", "jev-latest", "-o", "json",
               "-C", str(checkout)]
    env = os.environ.copy()
    env.pop("ROUTE_GH_REPLAY", None)
    env.update(OMNI_DEV_GH_BIN=str(shim), ROUTE_GH_FIXTURE_DIR=str(fixture))
    started_at = datetime.datetime.now(datetime.timezone.utc).isoformat()
    clock = time.monotonic()
    result = subprocess.run(command, cwd=wt, env=env, capture_output=True)
    seconds = time.monotonic() - clock
    finished_at = datetime.datetime.now(datetime.timezone.utc).isoformat()
    (output / "baseline.stderr.txt").write_bytes(result.stderr)
    with gzip.GzipFile(filename=str(output / "baseline.json.gz"), mode="wb", mtime=0) as file:
        file.write(result.stdout)
    records = {path.name: json.loads(path.read_text()) for path in sorted(fixture.glob("*.json"))}
    with gzip.GzipFile(filename=str(output / "github-responses.json.gz"), mode="wb", mtime=0) as file:
        file.write(json.dumps(records, indent=2).encode())
    metadata = {
        "command": command, "started_at": started_at, "finished_at": finished_at,
        "seconds": seconds, "exit_code": result.returncode,
        "source_commit": subprocess.check_output(
            ["git", "-C", str(wt), "rev-parse", "HEAD"], text=True).strip(),
        "version": subprocess.check_output([str(binary), "--version"], text=True).strip(),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "fixture_environment": {"OMNI_DEV_GH_BIN": str(shim),
                                "ROUTE_GH_FIXTURE_DIR": str(fixture)},
        "requested_model": "jev-latest", "monetary_cost": None,
        "monetary_cost_note": "Account invoice unavailable; see pricing.json for dated list-price estimate.",
    }
    (output / "run.json").write_text(json.dumps(metadata, indent=2) + "\n")
    print(json.dumps(metadata, indent=2))
    return result.returncode


def main():
    if len(sys.argv) != 3:
        raise SystemExit("usage: capture.py SUCCINCTLY_CHECKOUT OUTPUT_DIR")
    return capture(Path(__file__).resolve().parents[3],
                   Path(sys.argv[1]).resolve(), Path(sys.argv[2]).resolve())


if __name__ == "__main__":
    sys.exit(main())
