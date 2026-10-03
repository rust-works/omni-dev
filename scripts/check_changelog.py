#!/usr/bin/env python3
"""Guard the changelogs against two mistakes that nothing else catches (#2129).

1. A bullet added to a section that has already been released.  A pull request
   branched before a release adds its bullet under `## [Unreleased]`; the
   release prep then renames that heading to the release; when the pull request
   merges, git applies its hunk inside the now-released section.  The change
   was not in that release, and its notes are wrong.
2. A user-visible change to the VS Code extension with no entry in
   `editors/vscode/CHANGELOG.md`.

Both are judged against the merge base of --base and --head, so a pull request
is blamed only for what it adds.  Run it on the merge ref (`pull_request`) and
on the merge-queue branch (`merge_group`): the first mistake is made by the
merge, so a check of the pull request's own diff cannot see it.

    python3 scripts/check_changelog.py --base origin/main
    python3 scripts/check_changelog.py --base v0.45.0 --no-extension-check

The second form is the release-time audit: it compares the working tree with
the previous release's tag.

Exit status: 0 clean, 1 findings, 2 usage or git error.
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path

ROOT_CHANGELOG = 'CHANGELOG.md'
EXTENSION_DIR = 'editors/vscode'
EXTENSION_CHANGELOG = f'{EXTENSION_DIR}/CHANGELOG.md'
UNRELEASED = 'unreleased'

# Files at the top of the extension directory that never ship behaviour.
EXTENSION_NOT_USER_VISIBLE = frozenset({
    'CHANGELOG.md',
    'README.md',
    'LICENSE',
    'package-lock.json',
    'tsconfig.json',
    'esbuild.js',
})
TEST_FILE = re.compile(r'\.(test|spec)\.[cm]?[jt]sx?$')
TEST_DIRS = frozenset({'test', 'tests', '__tests__'})

HEADING = re.compile(r'^##\s+(?P<text>.+?)\s*$')
BRACKETED = re.compile(r'^\[(?P<label>[^\]]+)\]')
ITEM = re.compile(r'^\s*(?:[-*+]|\d+[.)])\s+(?P<text>\S.*?)\s*$')
FENCE = re.compile(r'^\s*(```|~~~)')
TRAILER = re.compile(
    r'^[ \t]*Changelog:[ \t]*(?P<kind>none|amend-released)\b', re.IGNORECASE | re.MULTILINE)

KIND_RELEASED = 'released'
KIND_EXTENSION = 'extension'
WAIVER = {KIND_RELEASED: 'amend-released', KIND_EXTENSION: 'none'}


class GitError(Exception):
    """A git invocation failed; the message is fit to show the user."""


@dataclass
class Item:
    text: str
    line: int


@dataclass
class Section:
    key: str
    heading: str
    items: list[Item] = field(default_factory=list)

    def texts(self) -> Counter:
        return Counter(item.text for item in self.items)


@dataclass(frozen=True)
class Finding:
    kind: str
    path: str
    line: int
    message: str


def section_key(heading: str) -> str:
    """Identify a section by its version label, so a date edit is not a new section."""
    bracketed = BRACKETED.match(heading)
    label = bracketed.group('label') if bracketed else heading.split()[0]
    return label.strip().lower()


def parse(text: str) -> dict[str, Section]:
    """Split a changelog into its `## ` sections and collect each one's list items.

    Fenced code, the preamble above the first heading and link reference
    definitions are ignored: none of them is a bullet.
    """
    sections: dict[str, Section] = {}
    current: Section | None = None
    fence: str | None = None
    for number, line in enumerate(text.splitlines(), start=1):
        marker = FENCE.match(line)
        if marker:
            # A fence closes only on its own marker, so ``` inside ~~~ stays code.
            if fence is None:
                fence = marker.group(1)
            elif fence == marker.group(1):
                fence = None
            continue
        if fence is not None:
            continue
        heading = HEADING.match(line)
        if heading:
            key = section_key(heading.group('text'))
            current = sections.setdefault(key, Section(key, heading.group('text')))
            continue
        item = ITEM.match(line)
        if item and current is not None:
            current.items.append(Item(' '.join(item.group('text').split()), number))
    return sections


def preview(text: str, limit: int = 110) -> str:
    return text if len(text) <= limit else text[:limit - 1].rstrip() + '…'


def added_to_released(path: str, base: dict[str, Section], head: dict[str, Section]) -> list[Finding]:
    """Findings for each already-released section that has grown.

    A section is released when the base has it and it is not `[Unreleased]`; one
    that only the head has is a release in preparation and is exempt.  Growth is a
    net count, so rewording a bullet, or moving one out to `[Unreleased]`, is fine.
    """
    findings = []
    for key, old in base.items():
        new = head.get(key)
        if key == UNRELEASED or new is None:
            continue
        gained, lost = new.texts() - old.texts(), old.texts() - new.texts()
        if sum(gained.values()) <= sum(lost.values()):
            continue
        for item in new.items:
            if gained[item.text] > 0:
                gained[item.text] -= 1
                findings.append(Finding(
                    KIND_RELEASED, path, item.line,
                    f'`## {old.heading}` was already released, but this change adds a bullet to it: '
                    f'{preview(item.text)}'))
    return findings


def has_new_entry(base: dict[str, Section], head: dict[str, Section]) -> bool:
    """Whether the head adds anything to `[Unreleased]`, or opens a release section."""
    if any(key != UNRELEASED and key not in base for key in head):
        return True
    old = base[UNRELEASED].texts() if UNRELEASED in base else Counter()
    new = head[UNRELEASED].texts() if UNRELEASED in head else Counter()
    return bool(new - old)


def is_user_visible(path: str) -> bool:
    """Whether a changed path can alter what a user of the extension gets.

    Everything under the extension directory is, except tests, the changelog and
    readme, the lockfile, build configuration and dotfiles.  Defaulting to "yes"
    means a new shipped directory is caught rather than silently missed.
    """
    prefix = EXTENSION_DIR + '/'
    if not path.startswith(prefix):
        return False
    relative = path[len(prefix):]
    parts = relative.split('/')
    if relative in EXTENSION_NOT_USER_VISIBLE:
        return False
    if any(part.startswith('.') for part in parts):
        return False
    return not (TEST_FILE.search(parts[-1]) or TEST_DIRS.intersection(parts[:-1]))


def extension_entry_missing(changed: list[str], base: dict[str, Section], head: dict[str, Section]) -> list[Finding]:
    visible = sorted(path for path in changed if is_user_visible(path))
    if not visible or has_new_entry(base, head):
        return []
    shown = ', '.join(visible[:3]) + (f' and {len(visible) - 3} more' if len(visible) > 3 else '')
    return [Finding(
        KIND_EXTENSION, EXTENSION_CHANGELOG, 1,
        f'this change touches the extension ({shown}) but adds no bullet under `## [Unreleased]`')]


def waivers(messages: str) -> set[str]:
    """The `Changelog: none` / `Changelog: amend-released` trailers in commit messages."""
    return {match.group('kind').lower() for match in TRAILER.finditer(messages)}


def git(repo: str, *args: str) -> str:
    proc = subprocess.run(
        ['git', '-C', repo, *args], capture_output=True, text=True, encoding='utf-8', check=False)
    if proc.returncode != 0:
        raise GitError(f'git {" ".join(args)} failed: {proc.stderr.strip() or proc.stdout.strip()}')
    return proc.stdout


def read_at(repo: str, rev: str | None, path: str) -> str | None:
    """A file's text at `rev`, or in the working tree when `rev` is None; None if absent."""
    if rev is None:
        target = Path(repo, path)
        return target.read_text(encoding='utf-8') if target.is_file() else None
    probe = subprocess.run(
        ['git', '-C', repo, 'cat-file', '-e', f'{rev}:{path}'], capture_output=True, check=False)
    return git(repo, 'show', f'{rev}:{path}') if probe.returncode == 0 else None


def merge_base(repo: str, base: str, head: str) -> str:
    try:
        return git(repo, 'merge-base', base, head).strip()
    except GitError as err:
        raise GitError(
            f'cannot find a common ancestor of {base} and {head}: {err}. '
            'A shallow checkout hides it; fetch the full history (`fetch-depth: 0`).') from err


def changed_files(repo: str, since: str, head: str | None) -> list[str]:
    names = git(repo, 'diff', '--name-only', '--no-renames', since, *([head] if head else []),
                '--', EXTENSION_DIR).splitlines()
    if head is None:
        names += git(repo, 'ls-files', '--others', '--exclude-standard', '--', EXTENSION_DIR).splitlines()
    return names


def check(repo: str, base: str, head: str | None, extension_check: bool = True) -> list[Finding]:
    """Run both checks.  `head` is a revision, or None for the working tree."""
    tip = head or 'HEAD'
    since = merge_base(repo, base, tip)
    allowed = waivers(git(repo, 'log', '--format=%B', f'{since}..{tip}'))

    findings: list[Finding] = []
    pairs = {}
    for path in (ROOT_CHANGELOG, EXTENSION_CHANGELOG):
        pairs[path] = (parse(read_at(repo, since, path) or ''), parse(read_at(repo, head, path) or ''))
        findings += added_to_released(path, *pairs[path])
    if extension_check:
        findings += extension_entry_missing(changed_files(repo, since, head), *pairs[EXTENSION_CHANGELOG])
    return [finding for finding in findings if WAIVER[finding.kind] not in allowed]


def escape(value: str, *, property_value: bool = False) -> str:
    value = value.replace('%', '%25').replace('\r', '%0D').replace('\n', '%0A')
    return value.replace(':', '%3A').replace(',', '%2C') if property_value else value


def report(findings: list[Finding]) -> None:
    on_github = os.environ.get('GITHUB_ACTIONS') == 'true'
    for finding in findings:
        if on_github:
            print(f'::error file={escape(finding.path, property_value=True)},line={finding.line}'
                  f'::{escape(finding.message)}')
        else:
            print(f'error: {finding.path}:{finding.line}: {finding.message}')
    kinds = {finding.kind for finding in findings}
    if KIND_RELEASED in kinds:
        print('\nA released section is closed.  Move the bullet under `## [Unreleased]`.  If you mean to '
              'amend released notes, add a `Changelog: amend-released <reason>` trailer to a commit.')
    if KIND_EXTENSION in kinds:
        print(f'\nAdd an entry for the change to {EXTENSION_CHANGELOG}.  If it is not user-visible after all, '
              'add a `Changelog: none <reason>` trailer to a commit.')


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=__doc__.split('\n\n')[0], formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog='Opt-outs are commit trailers: `Changelog: none <reason>` waives the extension entry, '
               '`Changelog: amend-released <reason>` waives the released-section rule.')
    parser.add_argument('--base', default='origin/main',
                        help='revision to compare against (default: origin/main)')
    parser.add_argument('--head', help='revision to check (default: the working tree)')
    parser.add_argument('--repo', default='.', help='repository root (default: the current directory)')
    parser.add_argument('--no-extension-check', action='store_true',
                        help='skip the extension entry requirement (the crate-release audit)')
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        findings = check(args.repo, args.base, args.head, extension_check=not args.no_extension_check)
    except (GitError, OSError) as err:
        print(f'check_changelog: {err}', file=sys.stderr)
        return 2
    if findings:
        report(findings)
        return 1
    print('check_changelog: ok')
    return 0


if __name__ == '__main__':
    sys.exit(main())
