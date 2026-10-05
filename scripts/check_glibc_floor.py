#!/usr/bin/env python3
"""Fail if a Linux binary needs a newer glibc than the stated floor (#2178).

The release binaries are dynamically linked against the glibc of the runner
image that builds them, so the image decides the oldest host they run on, and a
moving label such as `ubuntu-latest` raises that floor with no change here.
This reads the binary's version-needs table (`readelf -V`, `.gnu.version_r`),
finds the highest `GLIBC_x.y` it requires, and fails when that is above the
floor, so a runner-image change is a failed release build rather than an archive
that downloads fine and then dies in the dynamic loader.

A requirement the table marks WEAK is not fatal to the loader (it prints
`weak version ... not found` and carries on), so it is reported but does not
fail the check.

    python3 scripts/check_glibc_floor.py --floor 2.35 target/release/omni-dev

Exit status: 0 within the floor, 1 above it, 2 usage or `readelf` error.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys

NEEDS_HEADING = re.compile(r"^Version needs section\b")
# `  0x0020:   Name: GLIBC_2.38  Flags: WEAK  Version: 2`
NEED = re.compile(r"\bName:\s*GLIBC_(?P<version>\d+(?:\.\d+)*)\s+Flags:\s*(?P<flags>\S+)")

Version = tuple[int, ...]


def parse_version(text: str) -> Version:
    """`2.38` -> `(2, 38)`, so versions compare numerically (2.9 < 2.38)."""
    try:
        return tuple(int(part) for part in text.split("."))
    except ValueError:
        raise ValueError(f"not a glibc version: {text!r}") from None


def format_version(version: Version) -> str:
    return ".".join(str(part) for part in version)


def glibc_needs(readelf_output: str) -> tuple[list[Version], list[Version]]:
    """The (hard, weak) `GLIBC_` versions in the version-needs table.

    Only `.gnu.version_r` counts: `readelf -V` also prints `.gnu.version`, which
    lists the same names per symbol, and `.gnu.version_d`, which is what the
    binary itself defines.
    """
    hard: list[Version] = []
    weak: list[Version] = []
    in_needs = False
    for line in readelf_output.splitlines():
        if NEEDS_HEADING.match(line):
            in_needs = True
            continue
        # Section headings start in column 0 and everything under them is
        # indented, so any other unindented line (or a blank one) ends the table.
        if not line.strip() or not line[0].isspace():
            in_needs = False
            continue
        if not in_needs:
            continue
        need = NEED.search(line)
        if need:
            target = weak if "WEAK" in need.group("flags") else hard
            target.append(parse_version(need.group("version")))
    return hard, weak


def check(readelf_output: str, floor: Version) -> tuple[bool, str]:
    """Whether the binary is within the floor, and a one-line account of why."""
    hard, weak = glibc_needs(readelf_output)
    limit = f"floor GLIBC_{format_version(floor)}"
    if not hard:
        # A binary linked against glibc always has a hard GLIBC_ entry, so finding
        # none means the output was not what this parses (or the binary is static,
        # which needs neither a floor nor this check): fail rather than ship it
        # unverified.
        return False, f"no GLIBC_ requirement found in the version-needs table; {limit}"
    highest = max(hard)
    parts = [f"highest hard requirement GLIBC_{format_version(highest)}"]
    if weak and max(weak) > highest:
        parts.append(f"highest weak requirement GLIBC_{format_version(max(weak))}")
    return highest <= floor, f"{', '.join(parts)}; {limit}"


class ReadelfError(Exception):
    """readelf is missing or could not read the binary (exit status 2)."""


def readelf_versions(path: str) -> str:
    try:
        done = subprocess.run(
            ["readelf", "-V", path], capture_output=True, text=True, check=False
        )
    except FileNotFoundError:
        raise ReadelfError("readelf not found (install binutils)") from None
    if done.returncode != 0:
        raise ReadelfError(f"readelf -V {path} failed: {done.stderr.strip()}")
    return done.stdout


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--floor", required=True, help="highest allowed glibc, e.g. 2.35")
    parser.add_argument("binaries", nargs="+", help="ELF binaries to check")
    args = parser.parse_args(argv)
    try:
        floor = parse_version(args.floor)
    except ValueError as err:
        parser.error(str(err))

    failed = False
    for path in args.binaries:
        try:
            ok, detail = check(readelf_versions(path), floor)
        except ReadelfError as err:
            print(f"::error::{err}")
            return 2
        if ok:
            print(f"ok: {path}: {detail}")
        else:
            failed = True
            print(f"::error::{path} needs a newer glibc than the floor: {detail}")
    if failed:
        print(
            "A binary that needs more than the floor will not start on older hosts. "
            "Build on an image with an older glibc, or raise GLIBC_FLOOR on purpose "
            "and update the floor stated in README.md and docs/RELEASE.md.",
            file=sys.stderr,
        )
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
