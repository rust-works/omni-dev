#!/usr/bin/env python3
"""Changelog fragments: validate them, check a pull request carries one, assemble a release.

Every pull request used to add its bullet at the top of `## [Unreleased]`, so
any two in flight conflicted on the same hunk, and a branch forked before a
release could have git apply its bullet inside the section the release had
just closed (#2129).  Each pull request now adds its own file instead (#2213),
and this script renders the files into the release section at release time.

    changelog.d/<id>.<type>[.<n>].md           the crate, CHANGELOG.md
    changelog.d/vscode/<id>.<type>[.<n>].md    the extension, editors/vscode/CHANGELOG.md

  <id>    an issue number (`2213`), or `+<slug>` (`+fix-typo`) for a pull request with none
  <type>  added | changed | deprecated | removed | fixed | security | ci | docs
  <n>     optional positive integer, for a second entry of the same id and type

The body is the entry exactly as it reads in the changelog: a bullet (`- `),
with any continuation or nested-bullet lines indented.

Subcommands:

  check      validate every fragment's name, type and body
  check-pr   also require the pull request to add or edit a fragment (and an
             extension fragment for a user-visible `editors/vscode/` change),
             and to leave the changelogs themselves alone, unless waived
  collect    render one component's fragments into `## [X.Y.Z] - DATE`, carry
             whatever is under `## [Unreleased]` into it verbatim, update the
             compare links and delete the consumed fragments (`--dry-run` previews)

Waivers are commit trailers: `Changelog: none <reason>` for a change with no
user-visible effect, `Changelog: amend-released <reason>` to correct published
notes in a changelog directly.  Standard library only; see changelog.d/README.md.

Exit status: 0 clean, 1 findings, 2 usage or git error.
"""

from __future__ import annotations

import argparse
import datetime
import fnmatch
import json
import os
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


@dataclass(frozen=True)
class Component:
    """One changelog, the directory its fragments live in, and its release tags."""

    name: str
    changelog: str
    fragments: str
    tag_prefix: str
    # Whether the changelog keeps a `[Unreleased]: .../compare/...` link footer that
    # a release must update.  The extension's changelog has none.
    links: bool


COMPONENTS = {
    'crate': Component('crate', 'CHANGELOG.md', 'changelog.d', 'v', links=True),
    'vscode': Component('vscode', 'editors/vscode/CHANGELOG.md', 'changelog.d/vscode', 'vscode-v', links=False),
}

# (type, section heading), in the order a rendered release lists them: Keep a
# Changelog's six, then the two sections omni-dev adds after them.
TYPES = [
    ('added', 'Added'),
    ('changed', 'Changed'),
    ('deprecated', 'Deprecated'),
    ('removed', 'Removed'),
    ('fixed', 'Fixed'),
    ('security', 'Security'),
    ('ci', 'CI/CD'),
    ('docs', 'Documentation'),
]
TYPE_NAMES = dict(TYPES)

# Entries in a fragment directory that are not fragments.  The crate's directory
# holds the extension's as a subdirectory.
NOT_FRAGMENTS = {'README.md'}
SUBDIRECTORIES = {'crate': {'vscode'}, 'vscode': set()}

FRAGMENT_RE = re.compile(
    r'^(?P<id>[1-9][0-9]*|\+[a-z0-9][a-z0-9-]*)'
    r'\.(?P<type>[a-z]+)'
    r'(?:\.(?P<n>[1-9][0-9]*))?'
    r'\.md$'
)
VERSION_RE = re.compile(r'^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?$')
DATE_RE = re.compile(r'^[0-9]{4}-[0-9]{2}-[0-9]{2}$')
RELEASE_HEADING_RE = re.compile(r'^## \[(?P<version>[0-9][^\]]*)\]', re.MULTILINE)
# A line the changelog's own structure is found by: a heading or a link definition.
STRUCTURAL_LINE_RE = re.compile(r'^(?:#|\[[^\]]+\]: )')
TRAILER_RE = re.compile(r'^Changelog:[ \t]*(?P<kind>none|amend-released)(?:[ \t].*)?$', re.IGNORECASE)

EXTENSION_DIR = 'editors/vscode'
EXTENSION_MANIFEST = f'{EXTENSION_DIR}/package.json'
# Top-level names in the extension directory that never ship behaviour: documents,
# the lockfile, and build or lint configuration.
EXTENSION_NOT_USER_VISIBLE = (
    'CHANGELOG.md', 'README.md', 'LICENSE*', 'package-lock.json',
    'tsconfig*.json', 'esbuild*', '*.config.*', '.*',
)
# Manifest keys that change how the extension is built, not what it does.
MANIFEST_DEV_KEYS = ('devDependencies', 'scripts', 'version')
TEST_FILE = re.compile(r'\.(test|spec)\.[cm]?[jt]sx?$')
TEST_DIRS = frozenset({'test', 'tests', '__tests__'})


class ChangelogError(Exception):
    """A problem the user must fix; printed without a traceback."""


class GitError(Exception):
    """A git invocation failed; the message is fit to show the user."""


@dataclass
class Fragment:
    path: Path
    id: str
    type: str
    n: int
    body: str

    @property
    def sort_key(self):
        # Issue numbers ascending, `+slug` entries after them, then the suffix.
        if self.id.startswith('+'):
            return (1, 0, self.id, self.n)
        return (0, int(self.id), '', self.n)


# --- fragments -----------------------------------------------------------------

def validate_name(name: str) -> tuple[str, str, int]:
    """Return (id, type, n) for a fragment filename, or raise ChangelogError."""
    m = FRAGMENT_RE.match(name)
    if not m:
        raise ChangelogError(
            f'{name}: not a valid fragment name; expected <issue>.<type>[.<n>].md '
            'or +<slug>.<type>[.<n>].md')
    if m['type'] not in TYPE_NAMES:
        raise ChangelogError(
            f"{name}: unknown type '{m['type']}'; expected one of " + ', '.join(t for t, _ in TYPES))
    return m['id'], m['type'], int(m['n'] or 0)


def read_body(path: Path, name: str) -> str:
    """The fragment's entry, validated so it renders as list items under its heading."""
    try:
        text = path.read_bytes().decode('utf-8')
    except UnicodeDecodeError:
        raise ChangelogError(f'{name}: not valid UTF-8') from None
    if '\r' in text:
        raise ChangelogError(f'{name}: contains a carriage return; use LF line endings')
    body = text.strip('\n').rstrip()
    if not body:
        raise ChangelogError(f'{name}: empty')
    if not body.startswith('- '):
        raise ChangelogError(f"{name}: an entry must start with a bullet ('- '), as in the changelog")
    for number, line in enumerate(body.split('\n')[1:], start=2):
        if not line.strip():
            raise ChangelogError(
                f'{name}: line {number} is blank; a blank line would split the section\'s list')
        if STRUCTURAL_LINE_RE.match(line):
            raise ChangelogError(
                f"{name}: line {number} starts with '#' or a '[x]: ' link, which would corrupt "
                f'the changelog: {line[:40]!r}; indent it or reword')
        if not (line.startswith('- ') or line.startswith(' ')):
            raise ChangelogError(
                f"{name}: line {number} must be another bullet ('- ') or be indented, "
                f'as a continuation of the one above: {line[:40]!r}')
    return body


def load_fragments(root: Path, component: Component) -> tuple[list[Fragment], list[str]]:
    """Parse every fragment of `component`; return (fragments, errors)."""
    directory = root / component.fragments
    fragments: list[Fragment] = []
    errors: list[str] = []
    if not directory.is_dir():
        return fragments, errors
    for path in sorted(directory.iterdir()):
        name = path.name
        shown = f'{component.fragments}/{name}'
        if name in NOT_FRAGMENTS or (path.is_dir() and name in SUBDIRECTORIES[component.name]):
            continue
        if not path.is_file() or path.is_symlink():
            errors.append(f'{shown}: not a regular file')
            continue
        try:
            frag_id, frag_type, n = validate_name(name)
        except ChangelogError as err:
            errors.append(f'{component.fragments}/{err}')
            continue
        try:
            body = read_body(path, shown)
        except ChangelogError as err:
            errors.append(str(err))
            continue
        fragments.append(Fragment(path, frag_id, frag_type, n, body))
    fragments.sort(key=lambda f: f.sort_key)
    return fragments, errors


def render_sections(fragments: list[Fragment]) -> list[str]:
    """One `### Heading` block per non-empty type, its bullets directly beneath it."""
    sections = []
    for frag_type, heading in TYPES:
        bodies = [f.body for f in fragments if f.type == frag_type]
        if bodies:
            sections.append(f'### {heading}\n' + '\n'.join(bodies))
    return sections


# --- collect -------------------------------------------------------------------

@dataclass
class Release:
    text: str      # the whole new changelog
    section: str   # just the new `## [X.Y.Z] - DATE` section, for a dry run


def build_release(changelog_text: str, fragments: list[Fragment], version: str, date: str,
                  component: Component) -> Release:
    """Return the changelog with a new release section; raises ChangelogError if it cannot."""
    if not VERSION_RE.match(version):
        raise ChangelogError(f"'{version}' is not a semantic version (X.Y.Z)")
    if not DATE_RE.match(date):
        raise ChangelogError(f"'{date}' is not a YYYY-MM-DD date")

    lines = changelog_text.splitlines(keepends=True)
    heads = [line.rstrip('\n') for line in lines]
    if '## [Unreleased]' not in heads:
        raise ChangelogError(f"{component.changelog} has no '## [Unreleased]' heading")
    if any(h.startswith(f'## [{version}]') for h in heads):
        raise ChangelogError(f'{component.changelog} already has a section for {version}')
    start = heads.index('## [Unreleased]')

    # The Unreleased body ends at the next release heading, or at the link footer
    # when nothing has been released yet.
    end = len(lines)
    for i in range(start + 1, len(lines)):
        if heads[i].startswith('## [') or re.match(r'^\[[^\]]+\]: ', heads[i]):
            end = i
            break

    body = lines[start + 1:end]
    while body and not body[0].strip():
        body.pop(0)
    while body and not body[-1].strip():
        body.pop()
    legacy = ''.join(body)
    if legacy and not legacy.endswith('\n'):
        legacy += '\n'

    sections = render_sections(fragments)
    if not sections and not legacy:
        raise ChangelogError('nothing to release: no fragments and an empty [Unreleased]')

    content = '\n\n'.join(sections) + '\n' if sections else ''
    if legacy:
        content += ('\n' if content else '') + legacy
    section = f'## [{version}] - {date}\n\n{content}'

    tail = ''.join(lines[end:])
    tail = update_links(tail, version, component)
    head = ''.join(lines[:start + 1])
    return Release(f'{head}\n{section}\n{tail}', section)


def update_links(tail: str, version: str, component: Component) -> str:
    """Point `[Unreleased]` at the new tag and add the new version's compare link."""
    link_re = re.compile(
        r'^\[Unreleased\]: (?P<base>\S+?)/compare/' + re.escape(component.tag_prefix)
        + r'[^\s]+\.\.\.HEAD[ \t]*$', re.MULTILINE)
    m = link_re.search(tail)
    if not m:
        if component.links:
            raise ChangelogError(
                f"{component.changelog} has no '[Unreleased]: .../compare/{component.tag_prefix}X...HEAD' "
                'link to update')
        return tail
    previous = RELEASE_HEADING_RE.search(tail)
    base, tag = m['base'], component.tag_prefix
    links = f'[Unreleased]: {base}/compare/{tag}{version}...HEAD\n'
    if previous:
        links += f"[{version}]: {base}/compare/{tag}{previous['version']}...{tag}{version}"
    else:
        links += f'[{version}]: {base}/releases/tag/{tag}{version}'
    return tail[:m.start()] + links + tail[m.end():]


def write_atomically(path: Path, text: str) -> None:
    """Replace `path` with `text`, keeping its mode (a temp file is 0600)."""
    mode = path.stat().st_mode & 0o7777
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=path.name + '.')
    try:
        with os.fdopen(fd, 'w', encoding='utf-8', newline='') as f:
            f.write(text)
        os.chmod(tmp, mode)
        os.replace(tmp, path)
    except BaseException:
        if os.path.exists(tmp):
            os.unlink(tmp)
        raise


# --- check-pr ------------------------------------------------------------------

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
            trailer = TRAILER_RE.match(line.strip())
            if trailer:
                found.add(trailer.group('kind').lower())
    return found


def released_versions(text: str | None) -> set[str]:
    return {m['version'] for m in RELEASE_HEADING_RE.finditer(text or '')}


def fragment_component(path: str) -> str | None:
    """The component a changed path is a fragment of, or None if it is not a fragment."""
    parent, _, name = path.rpartition('/')
    for component in COMPONENTS.values():
        if parent == component.fragments and name not in NOT_FRAGMENTS and FRAGMENT_RE.match(name):
            return component.name
    return None


def git(repo: Path, *args: str) -> str:
    proc = subprocess.run(['git', '-C', str(repo), *args], capture_output=True, text=True,
                          encoding='utf-8', check=False)
    if proc.returncode != 0:
        raise GitError(f'git {" ".join(args)} failed: {proc.stderr.strip() or proc.stdout.strip()}')
    return proc.stdout


def read_at(repo: Path, rev: str, path: str) -> str | None:
    """A file's text at `rev`, or None if it does not exist there."""
    proc = subprocess.run(['git', '-C', str(repo), 'cat-file', '-e', f'{rev}:{path}'],
                          capture_output=True, check=False)
    if proc.returncode != 0:
        return None
    return git(repo, 'show', f'{rev}:{path}')


@dataclass
class Change:
    status: str  # A, M, D or R (a copy is reported as an add)
    path: str


def changes(repo: Path, since: str, head: str) -> list[Change]:
    out = git(repo, 'diff', '--name-status', '-M', '--no-ext-diff', since, head)
    result = []
    for line in out.splitlines():
        fields = line.split('\t')
        status = fields[0][0]
        if status == 'C':
            status = 'A'
        result.append(Change(status, fields[-1]))
        if status == 'R':
            result.append(Change('D', fields[1]))
    return result


@dataclass
class PrCheck:
    since: str
    notes: list[str]
    errors: list[str]


def check_pr(repo: Path, base: str, head: str, author: str = '') -> PrCheck:
    """Judge a pull request's changes (the merge base of `base` and `head` .. `head`)."""
    try:
        since = git(repo, 'merge-base', base, head).strip()
    except GitError as err:
        raise GitError(f'cannot find a common ancestor of {base} and {head}: {err}. '
                       'A shallow checkout hides it; fetch the full history (`fetch-depth: 0`).') from err
    allowed = waivers(git(repo, 'log', '--format=%B%x00', f'{since}..{head}').split('\0'))
    changed = changes(repo, since, head)
    notes: list[str] = []
    errors: list[str] = []

    # A release pull request opens a new version section in the changelog it releases;
    # the title is not consulted, so naming a pull request cannot skip the gate.
    released = set()
    for component in COMPONENTS.values():
        if any(c.path == component.changelog for c in changed):
            before, after = (read_at(repo, rev, component.changelog) for rev in (since, head))
            if released_versions(after) - released_versions(before):
                released.add(component.name)
                notes.append(f'release of the {component.name} ({component.changelog} gains a version section)')

    for component in COMPONENTS.values():
        if component.name in released or not any(c.path == component.changelog for c in changed):
            continue
        if 'amend-released' in allowed:
            notes.append(f'{component.changelog} edited directly, waived by `Changelog: amend-released`')
        else:
            errors.append(
                f'{component.changelog} is edited directly. Add a fragment under {component.fragments}/ '
                'instead (see changelog.d/README.md); the release assembles the changelog. To correct '
                'notes already published, add a `Changelog: amend-released <reason>` trailer to a commit.')

    fragments = {c.path: fragment_component(c.path) for c in changed if c.status in 'AMR'}
    present = {name for name in fragments.values() if name}
    if present:
        notes.append('fragment(s): ' + ', '.join(sorted(p for p, name in fragments.items() if name)))

    waived = None
    if released:
        waived = 'release pull request'
    elif author.endswith('[bot]'):
        waived = f'opened by a bot ({author})'
    elif 'none' in allowed:
        waived = '`Changelog: none` trailer'
    elif 'amend-released' in allowed:
        # Correcting published notes is itself the changelog change.
        waived = '`Changelog: amend-released` trailer'

    if not present and not waived:
        errors.append(
            'this pull request adds no changelog fragment. Add changelog.d/<issue>.<type>.md '
            '(see changelog.d/README.md), or, for a change with no user-visible effect, add a '
            '`Changelog: none <reason>` trailer to a commit.')

    visible = [c.path for c in changed if is_user_visible(c.path)]
    if EXTENSION_MANIFEST in visible and not manifest_ships(
            read_at(repo, since, EXTENSION_MANIFEST), read_at(repo, head, EXTENSION_MANIFEST)):
        visible.remove(EXTENSION_MANIFEST)
    if visible and 'vscode' not in present and not waived:
        shown = ', '.join(sorted(visible)[:3]) + (f' and {len(visible) - 3} more' if len(visible) > 3 else '')
        errors.append(
            f'this pull request changes the VS Code extension ({shown}) but adds no fragment under '
            f'{COMPONENTS["vscode"].fragments}/. If the change is not user-visible after all, add a '
            '`Changelog: none <reason>` trailer to a commit.')
    elif visible and waived:
        notes.append(f'extension fragment waived: {waived}')
    if waived and not errors:
        notes.append(f'fragment requirement waived: {waived}')
    return PrCheck(since, notes, errors)


# --- commands ------------------------------------------------------------------

def selected(args) -> list[Component]:
    return [COMPONENTS[args.component]] if getattr(args, 'component', None) else list(COMPONENTS.values())


def validate_all(root: Path, components: list[Component]) -> tuple[int, list[str]]:
    count, errors = 0, []
    for component in components:
        fragments, problems = load_fragments(root, component)
        count += len(fragments)
        errors += problems
    return count, errors


def cmd_check(args) -> int:
    count, errors = validate_all(Path(args.root), selected(args))
    for err in errors:
        print(f'error: {err}', file=sys.stderr)
    if errors:
        print(f'{len(errors)} invalid fragment(s)', file=sys.stderr)
        return 1
    print(f'{count} fragment(s) valid')
    return 0


def cmd_check_pr(args) -> int:
    status = cmd_check(args)
    result = check_pr(Path(args.root), args.base, args.head, args.author)
    print(f'changes since {result.since[:10]} ({args.base} .. {args.head})')
    for note in result.notes:
        print(f'  {note}')
    for err in result.errors:
        print(f'error: {err}', file=sys.stderr)
    return 1 if result.errors or status else 0


def cmd_collect(args) -> int:
    root = Path(args.root)
    component = COMPONENTS[args.component]
    changelog = root / component.changelog
    fragments, errors = load_fragments(root, component)
    if errors:
        for err in errors:
            print(f'error: {err}', file=sys.stderr)
        raise ChangelogError(f'{len(errors)} invalid fragment(s); fix them before releasing')
    date = args.date or datetime.date.today().isoformat()
    text = changelog.read_bytes().decode('utf-8')
    if '\r' in text:
        raise ChangelogError(f'{component.changelog} has CRLF line endings; refusing to mix them')
    release = build_release(text, fragments, args.version, date, component)

    names = ', '.join(f.path.name for f in fragments) or 'none'
    if args.dry_run:
        print(release.section, end='')
        print(f'\n-- dry run: would consume {len(fragments)} fragment(s) ({names}) into '
              f'{component.changelog}; nothing written or deleted --', file=sys.stderr)
        return 0

    directory = root / component.fragments
    if fragments and not os.access(directory, os.W_OK):
        raise ChangelogError(f'{component.fragments} is not writable; fragments could not be deleted')
    write_atomically(changelog, release.text)
    stuck = []
    for f in fragments:
        try:
            f.path.unlink()
        except OSError as err:
            stuck.append(f'{f.path.name}: {err}')
    if stuck:
        raise ChangelogError(
            f'{component.changelog} was written, but these fragments could not be deleted; delete them '
            'by hand (a re-run would refuse, the section exists): ' + '; '.join(stuck))
    print(f'wrote {component.changelog}: [{args.version}] - {date}; consumed {len(fragments)} fragment(s)')
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=__doc__.split('\n\n')[0], formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog='Waivers are commit trailers: `Changelog: none <reason>` and '
               '`Changelog: amend-released <reason>`.  See changelog.d/README.md.')
    sub = parser.add_subparsers(dest='command', required=True)

    def common(sp, component_required=False):
        sp.add_argument('--root', default=str(ROOT), help='repository root (default: this checkout)')
        sp.add_argument('--component', choices=sorted(COMPONENTS), required=component_required,
                        help='crate (CHANGELOG.md) or vscode (editors/vscode/CHANGELOG.md)'
                             + ('' if component_required else '; default: both'))

    sp = sub.add_parser('check', help='validate every fragment')
    common(sp)
    sp.set_defaults(func=cmd_check)

    sp = sub.add_parser('check-pr', help='require a fragment (or a waiver) of a pull request')
    common(sp)
    sp.add_argument('--base', default='origin/main', help='revision the pull request targets')
    sp.add_argument('--head', default='HEAD', help='the pull request head')
    sp.add_argument('--author', default='', help="the pull request author's login (a bot is waived)")
    sp.set_defaults(func=cmd_check_pr)

    sp = sub.add_parser('collect', help='render one component\'s fragments into a release section')
    common(sp, component_required=True)
    sp.add_argument('--version', required=True)
    sp.add_argument('--date', default='', help='YYYY-MM-DD (default: today)')
    sp.add_argument('--dry-run', action='store_true', help='print the section; write and delete nothing')
    sp.set_defaults(func=cmd_collect)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return args.func(args)
    except ChangelogError as err:
        print(f'error: {err}', file=sys.stderr)
        return 1
    except (GitError, OSError) as err:
        print(f'changelog: {err}', file=sys.stderr)
        return 2


if __name__ == '__main__':
    sys.exit(main())
