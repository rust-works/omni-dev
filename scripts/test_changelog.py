#!/usr/bin/env python3
"""Tests for changelog.py: python3 -m unittest discover -s scripts -p 'test_*.py'."""

import contextlib
import io
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

import changelog as cl

CRATE = cl.COMPONENTS['crate']
VSCODE = cl.COMPONENTS['vscode']

CRATE_CHANGELOG = """\
# Changelog

A preamble line.

## [Unreleased]

## [0.2.0] - 2026-01-02

### Added
- **Two** (#2).

## [0.1.0] - 2026-01-01

### Added
- **One** (#1).

[Unreleased]: https://github.com/o/r/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/o/r/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/o/r/releases/tag/v0.1.0
"""

VSCODE_CHANGELOG = """\
# Changelog

## [Unreleased]

## [0.1.0] - 2026-01-01

### Added
- **One**.
"""


class TempRoot:
    """A scratch repository root with both changelogs and the fragment directories."""

    def __init__(self, crate=CRATE_CHANGELOG, vscode=VSCODE_CHANGELOG):
        self.dir = tempfile.TemporaryDirectory()
        self.root = Path(self.dir.name)
        self.write(CRATE.changelog, crate)
        self.write(VSCODE.changelog, vscode)
        self.write(f'{CRATE.fragments}/README.md', 'not a fragment\n')
        self.write(f'{VSCODE.fragments}/README.md', 'not a fragment\n')

    def write(self, rel, text):
        path = self.root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding='utf-8')
        return path

    def read(self, rel):
        return (self.root / rel).read_text(encoding='utf-8')

    def close(self):
        self.dir.cleanup()


def run(argv):
    out, err = io.StringIO(), io.StringIO()
    with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
        status = cl.main(argv)
    return status, out.getvalue(), err.getvalue()


class NameTests(unittest.TestCase):
    def test_valid_names(self):
        self.assertEqual(cl.validate_name('2213.ci.md'), ('2213', 'ci', 0))
        self.assertEqual(cl.validate_name('2213.fixed.2.md'), ('2213', 'fixed', 2))
        self.assertEqual(cl.validate_name('+fix-typo.docs.md'), ('+fix-typo', 'docs', 0))

    def test_every_type_is_accepted(self):
        for frag_type, _ in cl.TYPES:
            self.assertEqual(cl.validate_name(f'1.{frag_type}.md')[1], frag_type)

    def test_a_misspelt_type_is_rejected_by_name(self):
        with self.assertRaisesRegex(cl.ChangelogError, "unknown type 'chnaged'"):
            cl.validate_name('3767.chnaged.md')

    def test_malformed_names_are_rejected(self):
        for name in ('0.fixed.md', '12.fixed.0.md', '12.fixed', '12.fixed.txt', 'fixed.md',
                     '+Upper.fixed.md', '12.Fixed.md', '#12.fixed.md', '12.fixed.md.bak'):
            with self.subTest(name=name), self.assertRaises(cl.ChangelogError):
                cl.validate_name(name)

    def test_performance_is_not_a_type(self):
        with self.assertRaises(cl.ChangelogError):
            cl.validate_name('1.performance.md')


class BodyTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = Path(self.tmp.name, 'f.md')

    def tearDown(self):
        self.tmp.cleanup()

    def body(self, text):
        self.path.write_bytes(text.encode('utf-8'))
        return cl.read_body(self.path, 'f.md')

    def test_a_bullet_with_continuations_and_sub_bullets(self):
        text = '- **Lead** (#1): prose\n  continued\n  - nested\n- second bullet\n'
        self.assertEqual(self.body(text), text.rstrip('\n'))

    def test_surrounding_blank_lines_are_trimmed(self):
        self.assertEqual(self.body('\n- entry\n\n\n'), '- entry')

    def test_rejections(self):
        cases = {
            '': 'empty',
            '\n  \n': 'empty',
            'entry\n': 'must start with a bullet',
            '* entry\n': 'must start with a bullet',
            '- a\r\n': 'carriage return',
            '- a\n\n- b\n': 'blank',
            '- a\n### Heading\n': "starts with '#'",
            '- a\n[x]: https://example.com\n': "starts with '#' or a '\\[x\\]: ' link",
            '- a\nunindented\n': 'must be another bullet',
        }
        for text, message in cases.items():
            with self.subTest(text=text), self.assertRaisesRegex(cl.ChangelogError, message):
                self.body(text)

    def test_invalid_utf8(self):
        self.path.write_bytes(b'- \xff\n')
        with self.assertRaisesRegex(cl.ChangelogError, 'UTF-8'):
            cl.read_body(self.path, 'f.md')


class LoadTests(unittest.TestCase):
    def setUp(self):
        self.t = TempRoot()

    def tearDown(self):
        self.t.close()

    def test_the_crate_directory_skips_its_readme_and_the_extension_subdirectory(self):
        self.t.write('changelog.d/5.fixed.md', '- crate\n')
        self.t.write('changelog.d/vscode/6.fixed.md', '- extension\n')
        crate, errors = cl.load_fragments(self.t.root, CRATE)
        self.assertEqual(errors, [])
        self.assertEqual([f.body for f in crate], ['- crate'])
        vscode, errors = cl.load_fragments(self.t.root, VSCODE)
        self.assertEqual(errors, [])
        self.assertEqual([f.body for f in vscode], ['- extension'])

    def test_errors_name_the_path(self):
        self.t.write('changelog.d/1.chnaged.md', '- x\n')
        self.t.write('changelog.d/vscode/2.fixed.md', 'no bullet\n')
        (self.t.root / 'changelog.d/other').mkdir()
        _, crate_errors = cl.load_fragments(self.t.root, CRATE)
        _, vscode_errors = cl.load_fragments(self.t.root, VSCODE)
        self.assertEqual(len(crate_errors), 2)
        self.assertTrue(crate_errors[0].startswith('changelog.d/1.chnaged.md: unknown type'))
        self.assertTrue(crate_errors[1].startswith('changelog.d/other: not a regular file'))
        self.assertTrue(vscode_errors[0].startswith('changelog.d/vscode/2.fixed.md: an entry'))

    def test_dotfiles_are_ignored(self):
        self.t.write('changelog.d/.DS_Store', 'junk')
        self.t.write('changelog.d/vscode/.gitkeep', '')
        for component in (CRATE, VSCODE):
            self.assertEqual(cl.load_fragments(self.t.root, component), ([], []))

    def test_a_subdirectory_of_the_extension_directory_is_an_error(self):
        (self.t.root / 'changelog.d/vscode/vscode').mkdir()
        _, errors = cl.load_fragments(self.t.root, VSCODE)
        self.assertEqual(len(errors), 1)

    def test_order_is_issue_number_then_slug_then_suffix(self):
        for name in ('+b.fixed.md', '10.fixed.md', '+a.fixed.md', '9.fixed.2.md', '9.fixed.md'):
            self.t.write(f'changelog.d/{name}', f'- {name}\n')
        fragments, _ = cl.load_fragments(self.t.root, CRATE)
        self.assertEqual([f.path.name for f in fragments],
                         ['9.fixed.md', '9.fixed.2.md', '10.fixed.md', '+a.fixed.md', '+b.fixed.md'])

    def test_check_command_reports_and_fails(self):
        self.t.write('changelog.d/1.fixed.md', '- ok\n')
        status, out, _ = run(['check', '--root', str(self.t.root)])
        self.assertEqual((status, out.strip()), (0, '1 fragment(s) valid'))
        self.t.write('changelog.d/vscode/2.fixd.md', '- typo\n')
        status, _, err = run(['check', '--root', str(self.t.root)])
        self.assertEqual(status, 1)
        self.assertIn("changelog.d/vscode/2.fixd.md: unknown type 'fixd'", err)
        status, _, _ = run(['check', '--root', str(self.t.root), '--component', 'crate'])
        self.assertEqual(status, 0)


class CollectTests(unittest.TestCase):
    def setUp(self):
        self.t = TempRoot()

    def tearDown(self):
        self.t.close()

    def collect(self, component='crate', version='0.3.0', *extra):
        return run(['collect', '--root', str(self.t.root), '--component', component,
                    '--version', version, '--date', '2026-02-03', *extra])

    def test_renders_the_hand_written_format(self):
        self.t.write('changelog.d/20.fixed.md', '- **Fix twenty** (#20).\n')
        self.t.write('changelog.d/3.added.md', '- **Add three** (#3): prose\n  - nested\n')
        self.t.write('changelog.d/+tidy.docs.md', '- **Tidy docs**.\n')
        self.t.write('changelog.d/7.ci.md', '- **CI seven** (#7).\n')
        self.t.write('changelog.d/4.added.md', '- **Add four** (#4).\n')
        self.t.write('changelog.d/5.security.md', '- **Secure five** (#5).\n')
        status, _, err = self.collect()
        self.assertEqual(status, 0, err)
        self.assertEqual(self.t.read('CHANGELOG.md'), """\
# Changelog

A preamble line.

## [Unreleased]

## [0.3.0] - 2026-02-03

### Added
- **Add three** (#3): prose
  - nested
- **Add four** (#4).

### Fixed
- **Fix twenty** (#20).

### Security
- **Secure five** (#5).

### CI/CD
- **CI seven** (#7).

### Documentation
- **Tidy docs**.

## [0.2.0] - 2026-01-02

### Added
- **Two** (#2).

## [0.1.0] - 2026-01-01

### Added
- **One** (#1).

[Unreleased]: https://github.com/o/r/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/o/r/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/o/r/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/o/r/releases/tag/v0.1.0
""")
        self.assertEqual(sorted(p.name for p in (self.t.root / 'changelog.d').iterdir()),
                         ['README.md', 'vscode'])

    def test_sections_follow_keep_a_changelog_order_then_ci_and_docs(self):
        for frag_type, _ in reversed(cl.TYPES):
            self.t.write(f'changelog.d/1.{frag_type}.md', f'- {frag_type}\n')
        fragments, _ = cl.load_fragments(self.t.root, CRATE)
        headings = [s.split('\n')[0] for s in cl.render_sections(fragments)]
        self.assertEqual(headings, ['### Added', '### Changed', '### Deprecated', '### Removed',
                                    '### Fixed', '### Security', '### CI/CD', '### Documentation'])

    def test_legacy_unreleased_text_is_carried_verbatim_after_the_fragments(self):
        legacy = '### Removed\n- **Legacy removal**.\n\n### Added\n- **Legacy add**.\n'
        self.t.write('CHANGELOG.md', CRATE_CHANGELOG.replace(
            '## [Unreleased]\n', f'## [Unreleased]\n\n{legacy}'))
        self.t.write('changelog.d/9.added.md', '- **Fragment add**.\n')
        status, _, err = self.collect()
        self.assertEqual(status, 0, err)
        text = self.t.read('CHANGELOG.md')
        self.assertIn('## [Unreleased]\n\n## [0.3.0] - 2026-02-03\n\n### Added\n- **Fragment add**.\n\n'
                      + legacy + '\n## [0.2.0]', text)

    def test_legacy_text_alone_is_released(self):
        self.t.write('CHANGELOG.md', CRATE_CHANGELOG.replace(
            '## [Unreleased]\n', '## [Unreleased]\n\n### Fixed\n- **Old**.\n'))
        status, _, err = self.collect()
        self.assertEqual(status, 0, err)
        self.assertIn('## [Unreleased]\n\n## [0.3.0] - 2026-02-03\n\n### Fixed\n- **Old**.\n\n## [0.2.0]',
                      self.t.read('CHANGELOG.md'))

    def test_the_extension_changelog_has_no_links_to_update(self):
        self.t.write('changelog.d/vscode/12.fixed.md', '- **Ext fix** (#12).\n')
        self.t.write('changelog.d/13.fixed.md', '- **Crate fix** (#13).\n')
        status, _, err = self.collect('vscode', '0.2.0')
        self.assertEqual(status, 0, err)
        self.assertEqual(self.t.read('editors/vscode/CHANGELOG.md'), """\
# Changelog

## [Unreleased]

## [0.2.0] - 2026-02-03

### Fixed
- **Ext fix** (#12).

## [0.1.0] - 2026-01-01

### Added
- **One**.
""")
        self.assertFalse((self.t.root / 'changelog.d/vscode/12.fixed.md').exists())
        self.assertTrue((self.t.root / 'changelog.d/13.fixed.md').exists(), 'the crate fragment is untouched')

    def test_a_first_release_links_to_its_tag(self):
        self.t.write('CHANGELOG.md', '# C\n\n## [Unreleased]\n\n[Unreleased]: https://g/o/r/compare/v0.0.0...HEAD\n')
        self.t.write('changelog.d/1.added.md', '- first\n')
        status, _, err = self.collect(version='0.1.0')
        self.assertEqual(status, 0, err)
        self.assertEqual(self.t.read('CHANGELOG.md'), '# C\n\n## [Unreleased]\n\n## [0.1.0] - 2026-02-03\n\n'
                         '### Added\n- first\n\n[Unreleased]: https://g/o/r/compare/v0.1.0...HEAD\n'
                         '[0.1.0]: https://g/o/r/releases/tag/v0.1.0\n')

    def test_dry_run_prints_the_section_and_changes_nothing(self):
        self.t.write('CHANGELOG.md', CRATE_CHANGELOG.replace(
            '## [Unreleased]\n', '## [Unreleased]\n\n### Fixed\n- **Old**.\n'))
        self.t.write('changelog.d/9.added.md', '- **New**.\n')
        before = self.t.read('CHANGELOG.md')
        status, out, err = self.collect('crate', '0.3.0', '--dry-run')
        self.assertEqual(status, 0, err)
        self.assertEqual(out, '## [0.3.0] - 2026-02-03\n\n### Added\n- **New**.\n\n### Fixed\n- **Old**.\n')
        self.assertIn('dry run', err)
        self.assertEqual(self.t.read('CHANGELOG.md'), before)
        self.assertTrue((self.t.root / 'changelog.d/9.added.md').exists())

    def test_refusals(self):
        cases = [
            (('crate', '0.2.0'), 'already has a section'),
            (('crate', 'v0.3.0'), 'not a semantic version'),
            (('crate', '0.3.0'), 'nothing to release'),
        ]
        for args, message in cases:
            with self.subTest(args=args):
                status, _, err = self.collect(*args)
                self.assertEqual(status, 1)
                self.assertIn(message, err)
        self.t.write('changelog.d/1.added.md', '- x\n')
        self.t.write('CHANGELOG.md', CRATE_CHANGELOG.split('[Unreleased]:')[0])
        status, _, err = self.collect()
        self.assertEqual(status, 1)
        self.assertIn('link to update', err)

    def test_an_invalid_fragment_blocks_the_release(self):
        self.t.write('changelog.d/1.added.md', '- x\n')
        self.t.write('changelog.d/2.addd.md', '- y\n')
        before = self.t.read('CHANGELOG.md')
        status, _, err = self.collect()
        self.assertEqual(status, 1)
        self.assertIn('fix them before releasing', err)
        self.assertEqual(self.t.read('CHANGELOG.md'), before)

    def test_the_file_mode_is_kept(self):
        path = self.t.root / 'CHANGELOG.md'
        os.chmod(path, 0o644)
        self.t.write('changelog.d/1.added.md', '- x\n')
        self.assertEqual(self.collect()[0], 0)
        self.assertEqual(path.stat().st_mode & 0o777, 0o644)


class ExtensionVisibilityTests(unittest.TestCase):
    def test_shipped_paths_are_user_visible(self):
        for path in ('editors/vscode/src/extension.ts', 'editors/vscode/package.json',
                     'editors/vscode/media/icon.svg'):
            self.assertTrue(cl.is_user_visible(path), path)

    def test_build_test_and_document_paths_are_not(self):
        for path in ('editors/vscode/CHANGELOG.md', 'editors/vscode/README.md', 'editors/vscode/package-lock.json',
                     'editors/vscode/tsconfig.json', 'editors/vscode/esbuild.mjs', 'editors/vscode/.vscodeignore',
                     'editors/vscode/src/tree.test.ts', 'editors/vscode/test/x.ts', 'src/main.rs',
                     'changelog.d/vscode/1.fixed.md'):
            self.assertFalse(cl.is_user_visible(path), path)

    def test_manifest_dev_keys_do_not_ship(self):
        self.assertFalse(cl.manifest_ships('{"version": "1", "scripts": {}}', '{"version": "2", "scripts": {"a": 1}}'))
        self.assertTrue(cl.manifest_ships('{"contributes": {}}', '{"contributes": {"x": 1}}'))
        self.assertTrue(cl.manifest_ships('{', '{}'))


class WaiverTests(unittest.TestCase):
    def test_trailers_in_the_last_paragraph_only(self):
        self.assertEqual(cl.waivers(['subject\n\nChangelog: none docs only']), {'none'})
        self.assertEqual(cl.waivers(['subject\n\nbody\n\nSigned-off-by: x\nChangelog: amend-released typo']),
                         {'amend-released'})
        self.assertEqual(cl.waivers(['subject\n\nChangelog: none in the body\n\nTrailer: x']), set())
        self.assertEqual(cl.waivers(['Changelog: none']), set(), 'a subject line is not a trailer')
        self.assertEqual(cl.waivers(['s\n\nChangelog: nonsense']), set())


class CheckPrTests(unittest.TestCase):
    """`check-pr` against a throwaway repository with a `main` and a feature branch."""

    def setUp(self):
        self.t = TempRoot()
        self.t.write('editors/vscode/package.json', '{"version": "0.1.0", "contributes": {}}\n')
        self.t.write('editors/vscode/src/extension.ts', 'export {};\n')
        self.t.write('src/main.rs', 'fn main() {}\n')
        self.git('init', '-q', '-b', 'main')
        self.git('config', 'user.email', 't@example.com')
        self.git('config', 'user.name', 't')
        self.git('config', 'commit.gpgsign', 'false')
        self.commit('initial')
        self.git('checkout', '-q', '-b', 'feature')

    def tearDown(self):
        self.t.close()

    def git(self, *args):
        return subprocess.run(['git', '-C', str(self.t.root), *args], check=True,
                              capture_output=True, text=True).stdout

    def commit(self, message):
        self.git('add', '-A')
        self.git('commit', '-q', '--allow-empty', '-m', message)

    def check(self, author=''):
        return cl.check_pr(self.t.root, 'main', 'HEAD', author)

    def assertPasses(self, result):
        self.assertEqual(result.errors, [])

    def assertFails(self, result, *fragments):
        self.assertEqual(len(result.errors), len(fragments), result.errors)
        for error, fragment in zip(result.errors, fragments):
            self.assertIn(fragment, error)

    def test_a_change_with_no_fragment_fails(self):
        self.t.write('src/main.rs', 'fn main() { }\n')
        self.commit('feat: x')
        self.assertFails(self.check(), 'adds no changelog fragment')

    def test_an_added_fragment_passes(self):
        self.t.write('src/main.rs', 'fn main() { }\n')
        self.t.write('changelog.d/42.added.md', '- **X** (#42).\n')
        self.commit('feat: x')
        result = self.check()
        self.assertPasses(result)
        self.assertIn('fragment(s): changelog.d/42.added.md', result.notes)

    def fragment_on_main(self):
        self.git('checkout', '-q', 'main')
        self.t.write('changelog.d/42.added.md', '- **X** (#42).\n')
        self.commit('feat: x')
        self.git('checkout', '-q', '-B', 'feature')

    def test_editing_an_existing_fragment_counts(self):
        self.fragment_on_main()
        self.t.write('changelog.d/42.added.md', '- **X, corrected** (#42).\n')
        self.commit('fix: correct the note')
        self.assertPasses(self.check())

    def test_renaming_an_existing_fragment_counts(self):
        self.fragment_on_main()
        self.git('mv', 'changelog.d/42.added.md', 'changelog.d/42.changed.md')
        self.commit('fix: retype the note')
        result = self.check()
        self.assertPasses(result)
        self.assertIn('fragment(s): changelog.d/42.changed.md', result.notes)

    def test_deleting_a_fragment_is_not_adding_one(self):
        self.fragment_on_main()
        self.git('rm', '-q', 'changelog.d/42.added.md')
        self.commit('chore: drop')
        self.assertFails(self.check(), 'adds no changelog fragment')

    def test_a_readme_edit_is_not_a_fragment(self):
        self.t.write('changelog.d/README.md', 'edited\n')
        self.commit('docs: readme')
        self.assertFails(self.check(), 'adds no changelog fragment')

    def test_changelog_none_waives(self):
        self.t.write('src/main.rs', 'fn main() { }\n')
        self.commit('refactor: x\n\nChangelog: none internal only')
        result = self.check()
        self.assertPasses(result)
        self.assertIn('fragment requirement waived: `Changelog: none` trailer', result.notes)

    def test_a_bot_is_waived(self):
        self.t.write('src/main.rs', 'fn main() { }\n')
        self.commit('chore(cargo): bump')
        self.assertPasses(self.check('dependabot[bot]'))

    def test_editing_a_changelog_directly_fails_even_with_a_fragment(self):
        self.t.write('CHANGELOG.md', CRATE_CHANGELOG.replace('## [Unreleased]\n', '## [Unreleased]\n\n- hand\n'))
        self.t.write('changelog.d/42.added.md', '- x\n')
        self.commit('feat: x')
        self.assertFails(self.check(), 'CHANGELOG.md is edited directly')

    def test_editing_a_changelog_directly_with_changelog_none_still_fails(self):
        self.t.write('editors/vscode/CHANGELOG.md', VSCODE_CHANGELOG.replace('- **One**.', '- **One!**.'))
        self.commit('docs: x\n\nChangelog: none docs')
        self.assertFails(self.check(), 'editors/vscode/CHANGELOG.md is edited directly')

    def test_amend_released_waives_a_direct_edit(self):
        self.t.write('CHANGELOG.md', CRATE_CHANGELOG.replace('**Two** (#2).', '**Two, corrected** (#2).'))
        self.commit('docs(changelog): fix a released note\n\nChangelog: amend-released wrong claim in 0.2.0')
        result = self.check()
        self.assertPasses(result)
        self.assertIn('CHANGELOG.md edited directly, waived by `Changelog: amend-released`', result.notes)

    def test_a_release_pull_request_is_recognised_by_its_new_section(self):
        self.t.write('changelog.d/42.added.md', '- x\n')
        self.commit('feat: x')
        self.git('checkout', '-q', '-B', 'main')
        self.git('checkout', '-q', '-b', 'release')
        status, _, err = run(['collect', '--root', str(self.t.root), '--component', 'crate',
                              '--version', '0.3.0', '--date', '2026-02-03'])
        self.assertEqual(status, 0, err)
        self.t.write('src/main.rs', 'fn main() { }\n')
        self.commit('chore(release): prepare release v0.3.0')
        result = self.check()
        self.assertPasses(result)
        self.assertIn('fragment requirement waived: release pull request', result.notes)

    def release(self, version='0.3.0', component='crate'):
        status, _, err = run(['collect', '--root', str(self.t.root), '--component', component,
                              '--version', version, '--date', '2026-02-03'])
        self.assertEqual(status, 0, err)

    def test_a_release_that_leaves_a_fragment_unconsumed_fails_in_the_queue_too(self):
        self.t.write('changelog.d/42.added.md', '- x\n')
        self.commit('feat: x')
        self.git('checkout', '-q', '-B', 'main')
        self.git('checkout', '-q', '-b', 'release')
        self.release()
        self.commit('chore(release): prepare release v0.3.0')
        # A fragment merges into main while the release waits; the queue rebases onto it.
        self.git('checkout', '-q', 'main')
        self.t.write('changelog.d/43.fixed.md', '- late\n')
        self.commit('fix: late')
        self.git('checkout', '-q', 'release')
        self.git('rebase', '-q', 'main')
        for queue in (False, True):
            with self.subTest(queue=queue):
                result = cl.check_pr(self.t.root, 'main', 'HEAD', queue=queue)
                self.assertFails(result, 'leaves 1 fragment(s) unconsumed (changelog.d/43.fixed.md)')

    def test_a_crate_release_ignores_extension_fragments(self):
        self.t.write('changelog.d/vscode/44.fixed.md', '- ext\n')
        self.t.write('changelog.d/42.added.md', '- x\n')
        self.commit('feat: x')
        self.git('checkout', '-q', '-B', 'main')
        self.git('checkout', '-q', '-b', 'release')
        self.release()
        self.commit('chore(release): prepare release v0.3.0')
        self.assertPasses(self.check())

    def test_a_crate_release_does_not_waive_the_extension_fragment(self):
        self.t.write('changelog.d/42.added.md', '- x\n')
        self.commit('feat: x')
        self.git('checkout', '-q', '-B', 'main')
        self.git('checkout', '-q', '-b', 'release')
        self.release()
        self.t.write('editors/vscode/src/extension.ts', 'export const x = 1;\n')
        self.commit('chore(release): prepare release v0.3.0')
        self.assertFails(self.check(), 'changes the VS Code extension')

    def test_an_extension_release_waives_the_extension_fragment(self):
        self.t.write('changelog.d/vscode/42.fixed.md', '- x\n')
        self.commit('fix(vscode): x')
        self.git('checkout', '-q', '-B', 'main')
        self.git('checkout', '-q', '-b', 'release')
        self.release('0.2.0', 'vscode')
        self.t.write('editors/vscode/package.json', '{"version": "0.2.0", "contributes": {}}\n')
        self.commit('chore(release): prepare vscode extension release v0.2.0')
        self.assertPasses(self.check())

    def test_amend_released_does_not_cover_unreleased(self):
        self.t.write('CHANGELOG.md', CRATE_CHANGELOG.replace('## [Unreleased]\n', '## [Unreleased]\n\n- hand\n'))
        self.commit('feat: x\n\nChangelog: amend-released not really')
        self.assertFails(self.check(), 'changes `## [Unreleased]` by hand')

    def test_amend_released_does_not_waive_the_extension_fragment(self):
        self.t.write('CHANGELOG.md', CRATE_CHANGELOG.replace('**Two** (#2).', '**Two, corrected** (#2).'))
        self.t.write('editors/vscode/src/extension.ts', 'export const x = 1;\n')
        self.commit('fix: x\n\nChangelog: amend-released wrong claim')
        self.assertFails(self.check(), 'changes the VS Code extension')

    def test_the_queue_skips_the_fragment_requirement_but_not_a_direct_edit(self):
        self.t.write('src/main.rs', 'fn main() { }\n')
        self.commit('feat: x')
        self.assertPasses(cl.check_pr(self.t.root, 'main', 'HEAD', queue=True))
        self.t.write('CHANGELOG.md', CRATE_CHANGELOG.replace('## [Unreleased]\n', '## [Unreleased]\n\n- hand\n'))
        self.commit('feat: y')
        self.assertFails(cl.check_pr(self.t.root, 'main', 'HEAD', queue=True), 'CHANGELOG.md is edited directly')

    def test_a_non_ascii_extension_path_is_seen(self):
        self.t.write('editors/vscode/media/icône.svg', '<svg/>\n')
        self.t.write('changelog.d/42.fixed.md', '- crate note\n')
        self.commit('feat(vscode): icon')
        self.assertFails(self.check(), 'editors/vscode/media/icône.svg')

    def test_a_release_title_alone_does_not_waive(self):
        self.t.write('src/main.rs', 'fn main() { }\n')
        self.commit('chore(release): prepare release v0.3.0')
        self.assertFails(self.check(), 'adds no changelog fragment')

    def test_an_extension_change_needs_an_extension_fragment(self):
        self.t.write('editors/vscode/src/extension.ts', 'export const x = 1;\n')
        self.t.write('changelog.d/42.fixed.md', '- crate note\n')
        self.commit('feat(vscode): x')
        self.assertFails(self.check(), 'changes the VS Code extension (editors/vscode/src/extension.ts)')

    def test_an_extension_fragment_alone_satisfies_both_rules(self):
        self.t.write('editors/vscode/src/extension.ts', 'export const x = 1;\n')
        self.t.write('changelog.d/vscode/42.fixed.md', '- ext note\n')
        self.commit('feat(vscode): x')
        self.assertPasses(self.check())

    def test_an_extension_change_with_changelog_none_passes(self):
        self.t.write('editors/vscode/src/extension.ts', 'export const x = 1;\n')
        self.commit('refactor(vscode): x\n\nChangelog: none no behaviour change')
        self.assertPasses(self.check())

    def test_extension_tests_and_dev_manifest_keys_need_no_extension_fragment(self):
        self.t.write('editors/vscode/src/extension.test.ts', 'test\n')
        self.t.write('editors/vscode/package.json', '{"version": "0.2.0", "contributes": {}}\n')
        self.t.write('changelog.d/42.fixed.md', '- crate note\n')
        self.commit('test(vscode): x')
        self.assertPasses(self.check())

    def test_a_shipping_manifest_change_needs_an_extension_fragment(self):
        self.t.write('editors/vscode/package.json', '{"version": "0.1.0", "contributes": {"x": 1}}\n')
        self.t.write('changelog.d/42.fixed.md', '- crate note\n')
        self.commit('feat(vscode): x')
        self.assertFails(self.check(), 'editors/vscode/package.json')

    def test_judged_from_the_merge_base(self):
        self.t.write('changelog.d/42.added.md', '- x\n')
        self.commit('feat: x')
        self.git('checkout', '-q', 'main')
        self.t.write('src/main.rs', 'fn main() { }\n')
        self.commit('main moved without a fragment')
        self.git('checkout', '-q', 'feature')
        self.assertPasses(self.check())

    def test_check_pr_command_exit_status(self):
        self.t.write('src/main.rs', 'fn main() { }\n')
        self.commit('feat: x')
        status, _, err = run(['check-pr', '--root', str(self.t.root), '--base', 'main', '--head', 'HEAD'])
        self.assertEqual(status, 1)
        self.assertIn('adds no changelog fragment', err)
        status, _, err = run(['check-pr', '--root', str(self.t.root), '--base', 'nope', '--head', 'HEAD'])
        self.assertEqual(status, 2)
        self.assertIn('cannot find a common ancestor', err)

    def test_an_invalid_fragment_fails_check_pr(self):
        self.t.write('changelog.d/42.addd.md', '- x\n')
        self.commit('feat: x')
        status, _, err = run(['check-pr', '--root', str(self.t.root), '--base', 'main', '--head', 'HEAD'])
        self.assertEqual(status, 1)
        self.assertIn("unknown type 'addd'", err)


class RepositoryTests(unittest.TestCase):
    """The checked-in fragments and changelogs are well formed."""

    def test_the_repository_fragments_are_valid(self):
        count, errors = cl.validate_all(cl.ROOT, list(cl.COMPONENTS.values()))
        self.assertEqual(errors, [])

    def test_both_changelogs_can_be_released(self):
        for component in cl.COMPONENTS.values():
            with self.subTest(component=component.name):
                fragments, _ = cl.load_fragments(cl.ROOT, component)
                text = (cl.ROOT / component.changelog).read_text(encoding='utf-8')
                try:
                    cl.build_release(text, fragments, '999.0.0', '2026-01-01', component)
                except cl.ChangelogError as err:
                    self.assertIn('nothing to release', str(err))


if __name__ == '__main__':
    unittest.main()
