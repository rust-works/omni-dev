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
import difflib
import fnmatch
import json
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

EXTENSION_MANIFEST = f'{EXTENSION_DIR}/package.json'
# Top-level names in the extension directory that never ship behaviour: documents,
# the lockfile, and build or lint configuration.
EXTENSION_NOT_USER_VISIBLE = (
    'CHANGELOG.md', 'README.md', 'LICENSE*', 'package-lock.json',
    'tsconfig*.json', 'esbuild*', '*.config.*', '.*',
)
# Manifest keys that change how the extension is built, not what it does.
MANIFEST_DEV_KEYS = ('devDependencies', 'scripts', 'version')
# A reworded bullet is not an added one; below this similarity it is a new bullet.
REWORD_SIMILARITY = 0.8
TEST_FILE = re.compile(r'\.(test|spec)\.[cm]?[jt]sx?$')
TEST_DIRS = frozenset({'test', 'tests', '__tests__'})

HEADING = re.compile(r'^##\s+(?P<text>.+?)\s*$')
BRACKETED = re.compile(r'^\[(?P<label>[^\]]+)\]')
ITEM = re.compile(r'^\s*[-*+]\s+(?P<text>\S.*?)\s*$')
FENCE = re.compile(r'^\s*(?P<run>`{3,}|~{3,})(?P<rest>.*)$')
TRAILER = re.compile(r'^Changelog:[ \t]*(?P<kind>none|amend-released)(?:[ \t].*)?$', re.IGNORECASE)

KIND_RELEASED = 'released'
KIND_EXTENSION = 'extension'
WAIVER = {KIND_RELEASED: 'amend-released', KIND_EXTENSION: 'none'}


class GitError(Exception):
    """A git invocation failed; the message is fit to show the user."""


class ChangelogError(Exception):
    """A changelog cannot be read reliably; the message is fit to show the user."""


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


@dataclass
class Outcome:
    since: str
    commits: int
    findings: list[Finding]


def section_key(heading: str) -> str:
    """Identify a section by its version label, so a date edit is not a new section."""
    bracketed = BRACKETED.match(heading)
    label = bracketed.group('label') if bracketed else heading.split()[0]
    return label.strip().lower()


def parse(text: str, source: str = 'changelog') -> dict[str, Section]:
    """Split a changelog into its `## ` sections and collect each one's list items.

    Fenced code, the preamble above the first heading and link reference
    definitions are ignored: none of them is a bullet.  A fence left open would
    hide every later section, and a check that sees nothing passes, so that is an
    error rather than a quiet pass.
    """
    sections: dict[str, Section] = {}
    current: Section | None = None
    fence: tuple[str, int, int] | None = None  # (character, length, opening line)
    for number, line in enumerate(text.splitlines(), start=1):
        marker = FENCE.match(line)
        if marker:
            run, rest = marker.group('run'), marker.group('rest')
            if fence is None:
                fence = (run[0], len(run), number)
            elif run[0] == fence[0] and len(run) >= fence[1] and not rest.strip():
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
    if fence is not None:
        raise ChangelogError(f'{source}: the code fence opened on line {fence[2]} is never closed, '
                             'so everything after it would be skipped')
    return sections


def preview(text: str, limit: int = 110) -> str:
    return text if len(text) <= limit else text[:limit - 1].rstrip() + '…'


def new_bullets(old: Section, new: Section) -> list[Item]:
    """The items of `new` that are neither in `old` nor a light rewording of one `old` lost.

    Pairing by similarity, not by count, keeps a typo fix from hiding a bullet added
    beside it, and keeps a bullet swapped for an unrelated one from passing as a reword.
    """
    unmatched = new.texts() - old.texts()
    lost = list((old.texts() - new.texts()).elements())
    added = []
    for item in new.items:
        if unmatched[item.text] <= 0:
            continue
        unmatched[item.text] -= 1
        best, best_ratio = None, REWORD_SIMILARITY
        for candidate in lost:
            matcher = difflib.SequenceMatcher(None, candidate, item.text, autojunk=False)
            if matcher.real_quick_ratio() >= best_ratio and matcher.quick_ratio() >= best_ratio:
                ratio = matcher.ratio()
                if ratio >= best_ratio:
                    best, best_ratio = candidate, ratio
        if best is None:
            added.append(item)
        else:
            lost.remove(best)
    return added


def added_to_released(path: str, base: dict[str, Section], head: dict[str, Section]) -> list[Finding]:
    """A finding for each bullet added to an already-released section.

    A section is released when the base has it and it is not `[Unreleased]`; one
    that only the head has is a release in preparation and is exempt.  Rewording a
    bullet, or moving one out to `[Unreleased]`, is not an addition.
    """
    return [
        Finding(KIND_RELEASED, path, item.line,
                f'`## {old.heading}` was already released, but this change adds a bullet to it: '
                f'{preview(item.text)}')
        for key, old in base.items()
        if key != UNRELEASED and key in head
        for item in new_bullets(old, head[key])
    ]


def has_new_entry(base: dict[str, Section], head: dict[str, Section]) -> bool:
    """Whether the head adds anything to `[Unreleased]`, or opens a release section that holds a bullet."""
    if any(key != UNRELEASED and key not in base and section.items for key, section in head.items()):
        return True
    old = base[UNRELEASED].texts() if UNRELEASED in base else Counter()
    new = head[UNRELEASED].texts() if UNRELEASED in head else Counter()
    return bool(new - old)


def is_user_visible(path: str) -> bool:
    """Whether a changed path can alter what a user of the extension gets.

    Everything under the extension directory is, except tests, documents, the
    lockfile, build and lint configuration and dotfiles.  Defaulting to "yes"
    means a new shipped directory is caught rather than silently missed.
    """
    prefix = EXTENSION_DIR + '/'
    if not path.startswith(prefix):
        return False
    parts = path[len(prefix):].split('/')
    if any(part.startswith('.') for part in parts):
        return False
    if len(parts) == 1 and any(fnmatch.fnmatchcase(parts[0], pattern) for pattern in EXTENSION_NOT_USER_VISIBLE):
        return False
    return not (TEST_FILE.search(parts[-1]) or TEST_DIRS.intersection(parts[:-1]))


def manifest_ships(before: str | None, after: str | None) -> bool:
    """Whether a `package.json` change alters more than the extension's build.

    Dev dependencies, scripts and the version do not reach a user, so a bump of any
    of them needs no entry.  A manifest that cannot be read counts as shipping.
    """
    try:
        trimmed = [{k: v for k, v in json.loads(text or '{}').items() if k not in MANIFEST_DEV_KEYS}
                   for text in (before, after)]
    except (ValueError, AttributeError):
        return True
    return trimmed[0] != trimmed[1]


def extension_entry_missing(visible: list[str], base: dict[str, Section], head: dict[str, Section]) -> list[Finding]:
    """A finding when the user-visible `visible` paths changed but the changelog gained no entry."""
    if not visible or has_new_entry(base, head):
        return []
    shown = ', '.join(sorted(visible)[:3]) + (f' and {len(visible) - 3} more' if len(visible) > 3 else '')
    return [Finding(
        KIND_EXTENSION, EXTENSION_CHANGELOG, 1,
        f'this change touches the extension ({shown}) but adds no bullet under `## [Unreleased]`')]


def waivers(messages: list[str]) -> set[str]:
    """The `Changelog: none` / `Changelog: amend-released` trailers of commit messages.

    Only the last paragraph of a message is its trailer block, so a line in the body that
    happens to start `Changelog: none` waives nothing.
    """
    found = set()
    for message in messages:
        paragraphs = [p for p in re.split(r'\n[ \t]*\n', message.strip().replace('\r\n', '\n')) if p.strip()]
        if len(paragraphs) < 2:
            continue
        for line in paragraphs[-1].split('\n'):
            trailer = TRAILER.match(line.strip())
            if trailer:
                found.add(trailer.group('kind').lower())
    return found


def git(repo: str, *args: str) -> str:
    proc = subprocess.run(
        ['git', '-C', repo, *args], capture_output=True, text=True, encoding='utf-8', check=False)
    if proc.returncode != 0:
        raise GitError(f'git {" ".join(args)} failed: {proc.stderr.strip() or proc.stdout.strip()}')
    return proc.stdout


def read_at(repo: str, rev: str | None, path: str) -> str | None:
    """A file's text at `rev`, or in the working tree when `rev` is None; None if absent.

    The revisions are validated by `merge_base` first, so a failed `show` is a missing path.
    """
    if rev is None:
        target = Path(repo, path)
        return target.read_text(encoding='utf-8') if target.is_file() else None
    proc = subprocess.run(['git', '-C', repo, 'show', f'{rev}:{path}'], capture_output=True, check=False)
    if proc.returncode != 0:
        absent = (b'does not exist', b'exists on disk, but not in')
        if any(marker in proc.stderr for marker in absent):
            return None
        raise GitError(f'git show {rev}:{path} failed: {proc.stderr.decode(errors="replace").strip()}')
    return proc.stdout.decode('utf-8')


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


def check(repo: str, base: str, head: str | None, extension_check: bool = True) -> Outcome:
    """Run both checks.  `head` is a revision, or None for the working tree."""
    tip = head or 'HEAD'
    since = merge_base(repo, base, tip)
    messages = git(repo, 'log', '--format=%B%x00', f'{since}..{tip}').split('\0')
    allowed = waivers(messages)

    findings: list[Finding] = []
    sections = {}
    for path in (ROOT_CHANGELOG, EXTENSION_CHANGELOG):
        before, after = read_at(repo, since, path), read_at(repo, head, path)
        sections[path] = (parse(before or '', f'{path} at {since[:10]}'),
                          parse(after or '', f'{path} at {head or "the working tree"}'))
        findings += added_to_released(path, *sections[path])
    if extension_check:
        visible = [path for path in changed_files(repo, since, head) if is_user_visible(path)]
        if EXTENSION_MANIFEST in visible and not manifest_ships(
                read_at(repo, since, EXTENSION_MANIFEST), read_at(repo, head, EXTENSION_MANIFEST)):
            visible.remove(EXTENSION_MANIFEST)
        findings += extension_entry_missing(visible, *sections[EXTENSION_CHANGELOG])
    commits = int(git(repo, 'rev-list', '--count', f'{since}..{tip}').strip())
    return Outcome(since, commits, [f for f in findings if WAIVER[f.kind] not in allowed])


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
        outcome = check(args.repo, args.base, args.head, extension_check=not args.no_extension_check)
    except (GitError, ChangelogError, OSError) as err:
        print(f'check_changelog: {err}', file=sys.stderr)
        return 2
    # Say what was compared: on a merge-queue entry this is the evidence that the base is the
    # previous entry's tip, not the branch's, which is what keeps one entry's changes off another.
    print(f'check_changelog: {outcome.commits} commit(s) since {outcome.since[:10]} '
          f'({args.base} .. {args.head or "the working tree"})')
    if outcome.findings:
        report(outcome.findings)
        return 1
    print('check_changelog: ok')
    return 0


if __name__ == '__main__':
    sys.exit(main())
