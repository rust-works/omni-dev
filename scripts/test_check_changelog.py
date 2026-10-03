#!/usr/bin/env python3
"""Tests for check_changelog.py: python3 -m unittest discover -s scripts -p 'test_*.py'."""

import contextlib
import io
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

import check_changelog as cc

EXT = cc.EXTENSION_CHANGELOG


def render(unreleased=(), *released):
    """A changelog with one `### Added` list per section: `released` is (version, bullets) pairs."""
    lines = ['# Changelog', '', 'A preamble line.', '', '## [Unreleased]', '']
    if unreleased:
        lines += ['### Added', *[f'- {bullet}' for bullet in unreleased], '']
    for version, bullets in released:
        lines += [f'## [{version}] - 2026-01-01', '', '### Added', *[f'- {bullet}' for bullet in bullets], '']
    return '\n'.join(lines) + '\n'


def findings(base, head, path=cc.ROOT_CHANGELOG):
    return cc.added_to_released(path, cc.parse(base), cc.parse(head))


class ParseTests(unittest.TestCase):
    def test_sections_are_keyed_by_their_version_label(self):
        sections = cc.parse(render(['a'], ('0.2.0', ['b']), ('0.1.0', ['c'])))
        self.assertEqual(list(sections), ['unreleased', '0.2.0', '0.1.0'])
        self.assertEqual([item.text for item in sections['0.2.0'].items], ['b'])

    def test_a_date_edit_keeps_the_section_key(self):
        before = cc.parse('## [0.1.0] - 2026-01-01\n- a\n')
        after = cc.parse('## [0.1.0] - 2026-01-02\n- a\n')
        self.assertEqual(list(before), list(after))

    def test_unbracketed_headings_use_their_first_word(self):
        self.assertEqual(list(cc.parse('## Unreleased\n- a\n## 1.0.0 (2020)\n- b\n')), ['unreleased', '1.0.0'])

    def test_items_carry_their_line_numbers(self):
        items = cc.parse('# T\n\n## [Unreleased]\n\n### Fixed\n- one\n- two\n')['unreleased'].items
        self.assertEqual([(item.text, item.line) for item in items], [('one', 6), ('two', 7)])

    def test_nested_numbered_and_alternate_markers_are_items(self):
        text = '## [Unreleased]\n- a\n  - nested\n* star\n+ plus\n1. one\n2) two\n'
        self.assertEqual(len(cc.parse(text)['unreleased'].items), 6)

    def test_preamble_link_references_and_rules_are_not_items(self):
        text = '- stray before any heading\n## [Unreleased]\n---\n[0.1.0]: https://example.com/a...b\n'
        self.assertEqual(cc.parse(text)['unreleased'].items, [])

    def test_fenced_code_is_ignored_until_the_matching_fence(self):
        text = '## [Unreleased]\n```md\n## [9.9.9]\n- inside\n```\n~~~\n```\n- still inside\n~~~\n- outside\n'
        sections = cc.parse(text)
        self.assertEqual(list(sections), ['unreleased'])
        self.assertEqual([item.text for item in sections['unreleased'].items], ['outside'])

    def test_whitespace_is_normalised(self):
        self.assertEqual(cc.parse('## [Unreleased]\n-   a   b  \n')['unreleased'].items[0].text, 'a b')


class ReleasedSectionTests(unittest.TestCase):
    def test_a_bullet_added_to_a_released_section_is_a_finding(self):
        base = render(['new'], ('0.1.0', ['old']))
        head = render(['new'], ('0.1.0', ['old', 'late']))
        [found] = findings(base, head)
        self.assertEqual((found.kind, found.path), (cc.KIND_RELEASED, 'CHANGELOG.md'))
        self.assertIn('## [0.1.0] - 2026-01-01', found.message)
        self.assertIn('late', found.message)
        self.assertEqual(found.line, 14)

    def test_a_bullet_added_to_unreleased_is_fine(self):
        self.assertEqual(findings(render(['a'], ('0.1.0', ['x'])), render(['a', 'b'], ('0.1.0', ['x']))), [])

    def test_release_prep_is_fine(self):
        base = render(['a', 'b'], ('0.1.0', ['x']))
        head = render([], ('0.2.0', ['a', 'b']), ('0.1.0', ['x']))
        self.assertEqual(findings(base, head), [])

    def test_a_section_only_the_head_has_is_exempt_whatever_it_holds(self):
        self.assertEqual(findings(render([], ('0.1.0', ['x'])), render([], ('0.2.0', ['y']), ('0.1.0', ['x']))), [])

    def test_removing_a_bullet_is_fine(self):
        self.assertEqual(findings(render([], ('0.1.0', ['x', 'y'])), render([], ('0.1.0', ['x']))), [])

    def test_the_corrective_move_back_to_unreleased_is_fine(self):
        base = render([], ('0.1.0', ['x', 'late']))
        head = render(['late'], ('0.1.0', ['x']))
        self.assertEqual(findings(base, head), [])

    def test_rewording_a_bullet_is_fine(self):
        self.assertEqual(findings(render([], ('0.1.0', ['a typo'])), render([], ('0.1.0', ['a fix']))), [])

    def test_the_same_text_added_twice_counts_twice(self):
        base = render([], ('0.1.0', ['x']))
        self.assertEqual(len(findings(base, render([], ('0.1.0', ['x', 'x', 'x'])))), 2)

    def test_a_bullet_moved_between_released_sections_is_flagged_where_it_lands(self):
        base = render([], ('0.2.0', ['x', 'moved']), ('0.1.0', ['y']))
        head = render([], ('0.2.0', ['x']), ('0.1.0', ['y', 'moved']))
        [found] = findings(base, head)
        self.assertIn('## [0.1.0]', found.message)

    def test_a_deleted_section_is_not_this_checks_business(self):
        self.assertEqual(findings(render([], ('0.1.0', ['x'])), render([])), [])

    def test_a_changelog_that_did_not_exist_has_no_released_sections(self):
        self.assertEqual(findings('', render([], ('0.1.0', ['x']))), [])

    def test_a_long_bullet_is_abbreviated_in_the_message(self):
        [found] = findings(render([], ('0.1.0', [])), render([], ('0.1.0', ['w' * 400])))
        self.assertLess(len(found.message), 250)
        self.assertIn('…', found.message)


class ExtensionRuleTests(unittest.TestCase):
    def test_user_visible_classification(self):
        visible = [
            'editors/vscode/src/extension.ts', 'editors/vscode/package.json', 'editors/vscode/media/icon.png',
            'editors/vscode/snippets/rust.json', 'editors/vscode/src/test-helpers.ts',
        ]
        hidden = [
            'src/main.rs', 'editors/vscode-other/src/a.ts', 'editors/vscode/CHANGELOG.md',
            'editors/vscode/README.md', 'editors/vscode/package-lock.json', 'editors/vscode/tsconfig.json',
            'editors/vscode/esbuild.js', 'editors/vscode/.vscodeignore', 'editors/vscode/src/tree.test.ts',
            'editors/vscode/src/tree.spec.mts', 'editors/vscode/test/fixture.ts', 'editors/vscode/src/__tests__/a.ts',
            'editors/vscode/.vscode-test/x.js',
        ]
        for path in visible:
            self.assertTrue(cc.is_user_visible(path), path)
        for path in hidden:
            self.assertFalse(cc.is_user_visible(path), path)

    def test_a_new_unreleased_bullet_is_an_entry(self):
        self.assertTrue(cc.has_new_entry(cc.parse(render(['a'])), cc.parse(render(['a', 'b']))))

    def test_an_edited_unreleased_bullet_is_an_entry(self):
        self.assertTrue(cc.has_new_entry(cc.parse(render(['a'])), cc.parse(render(['a, extended']))))

    def test_a_release_section_is_an_entry(self):
        self.assertTrue(cc.has_new_entry(cc.parse(render(['a'])), cc.parse(render([], ('0.2.0', ['a'])))))

    def test_nothing_new_is_not_an_entry(self):
        self.assertFalse(cc.has_new_entry(cc.parse(render(['a'])), cc.parse(render(['a']))))
        self.assertFalse(cc.has_new_entry(cc.parse(render(['a'])), cc.parse(render([]))))

    def test_a_recreated_empty_unreleased_heading_is_not_an_entry(self):
        self.assertFalse(cc.has_new_entry(cc.parse(''), cc.parse('## [Unreleased]\n')))

    def missing(self, changed, base=(), head=()):
        return cc.extension_entry_missing(changed, cc.parse(render(base)), cc.parse(render(head)))

    def test_a_user_visible_change_without_an_entry_is_a_finding(self):
        [found] = self.missing(['editors/vscode/src/tree.ts'])
        self.assertEqual((found.kind, found.path, found.line), (cc.KIND_EXTENSION, EXT, 1))
        self.assertIn('editors/vscode/src/tree.ts', found.message)

    def test_an_entry_satisfies_it(self):
        self.assertEqual(self.missing(['editors/vscode/src/tree.ts'], head=['**Tree**: a fix']), [])

    def test_changes_that_are_not_user_visible_need_no_entry(self):
        self.assertEqual(self.missing(['editors/vscode/src/tree.test.ts', 'src/lib.rs']), [])

    def test_the_message_names_at_most_three_files(self):
        files = [f'editors/vscode/src/f{n}.ts' for n in range(5)]
        [found] = self.missing(files)
        self.assertIn('f0.ts', found.message)
        self.assertNotIn('f3.ts', found.message)
        self.assertIn('and 2 more', found.message)


class WaiverTests(unittest.TestCase):
    def test_trailers_are_recognised_case_insensitively(self):
        message = 'fix(vscode): x\n\nBody.\n\nchangelog: NONE not user visible\nChangelog: amend-released backfill\n'
        self.assertEqual(cc.waivers(message), {'none', 'amend-released'})

    def test_other_text_is_not_a_waiver(self):
        for message in ('Changelog: nonexistent\n', 'See the Changelog: none here\n', 'Changelog:\n', ''):
            self.assertEqual(cc.waivers(message), set(), message)


class ReportTests(unittest.TestCase):
    released = cc.Finding(cc.KIND_RELEASED, 'CHANGELOG.md', 42, 'late: bullet, here')
    extension = cc.Finding(cc.KIND_EXTENSION, EXT, 1, 'no entry')

    def report(self, found, github):
        env = {'GITHUB_ACTIONS': 'true'} if github else {}
        out = io.StringIO()
        with mock.patch.dict(os.environ, env), contextlib.redirect_stdout(out):
            if not github:
                os.environ.pop('GITHUB_ACTIONS', None)
            cc.report(found)
        return out.getvalue()

    def test_plain_output_points_at_the_line(self):
        self.assertIn('error: CHANGELOG.md:42: late: bullet, here', self.report([self.released], github=False))

    def test_github_output_is_an_escaped_annotation(self):
        out = self.report([self.released], github=True)
        self.assertIn('::error file=CHANGELOG.md,line=42::late: bullet, here', out)
        self.assertEqual(cc.escape('a:b,c\n100%', property_value=True), 'a%3Ab%2Cc%0A100%25')
        self.assertEqual(cc.escape('a:b,c\n'), 'a:b,c%0A')

    def test_each_kind_gets_its_own_guidance(self):
        self.assertIn('amend-released', self.report([self.released], github=False))
        self.assertNotIn('Changelog: none', self.report([self.released], github=False))
        self.assertIn('Changelog: none', self.report([self.extension], github=False))
        self.assertNotIn('amend-released', self.report([self.extension], github=False))


# Git must neither read the user's config (commit signing, templates) nor need an identity.
GIT_ENV = {
    **os.environ,
    'GIT_CONFIG_GLOBAL': os.devnull,
    'GIT_CONFIG_SYSTEM': os.devnull,
    'GIT_AUTHOR_NAME': 'Test', 'GIT_AUTHOR_EMAIL': 'test@example.com',
    'GIT_COMMITTER_NAME': 'Test', 'GIT_COMMITTER_EMAIL': 'test@example.com',
}


class GitTestCase(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory(prefix='check-changelog-')
        self.addCleanup(temp.cleanup)
        self.repo = Path(temp.name)
        self.git('init', '-q', '-b', 'main')

    def git(self, *args):
        proc = subprocess.run(['git', '-C', str(self.repo), *args], env=GIT_ENV, text=True, check=True,
                              capture_output=True)
        return proc.stdout.strip()

    def write(self, path, text):
        target = self.repo / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text, encoding='utf-8')

    def commit(self, message, files):
        for path, text in files.items():
            self.write(path, text)
        self.git('add', '-A')
        self.git('commit', '-q', '-m', message)

    def check(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        with mock.patch.dict(os.environ), contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            os.environ.pop('GITHUB_ACTIONS', None)
            code = cc.main(['--repo', str(self.repo), *argv])
        return code, out.getvalue(), err.getvalue()


class EndToEndTests(GitTestCase):
    def test_a_late_bullet_in_a_released_section_fails(self):
        self.commit('base', {'CHANGELOG.md': render(['a'], ('0.1.0', ['x']))})
        self.git('checkout', '-q', '-b', 'feature')
        self.commit('feat: late', {'CHANGELOG.md': render(['a'], ('0.1.0', ['x', 'late']))})
        code, out, _ = self.check('--base', 'main', '--head', 'feature')
        self.assertEqual(code, 1)
        self.assertIn('## [0.1.0] - 2026-01-01', out)
        self.assertIn('late', out)

    def test_a_bullet_in_unreleased_passes(self):
        self.commit('base', {'CHANGELOG.md': render(['a'], ('0.1.0', ['x']))})
        self.git('checkout', '-q', '-b', 'feature')
        self.commit('feat: ok', {'CHANGELOG.md': render(['a', 'b'], ('0.1.0', ['x']))})
        self.assertEqual(self.check('--base', 'main', '--head', 'feature')[0], 0)

    def test_release_prep_passes(self):
        self.commit('base', {
            'CHANGELOG.md': render(['a', 'b'], ('0.1.0', ['x'])),
            EXT: render(['ext'], ('0.1.0', ['y'])),
            'editors/vscode/package.json': '{"version": "0.1.0"}\n',
        })
        self.git('checkout', '-q', '-b', 'release')
        self.commit('chore(release): prepare', {
            'CHANGELOG.md': render([], ('0.2.0', ['a', 'b']), ('0.1.0', ['x'])),
            EXT: render([], ('0.2.0', ['ext']), ('0.1.0', ['y'])),
            'editors/vscode/package.json': '{"version": "0.2.0"}\n',
        })
        code, out, _ = self.check('--base', 'main', '--head', 'release')
        self.assertEqual((code, out), (0, 'check_changelog: ok\n'))

    def test_rebasing_a_stale_branch_over_a_release_lands_the_bullet_in_the_released_section(self):
        """The mechanism of #2129, reproduced with real git rather than assumed."""
        self.commit('base', {'CHANGELOG.md': render(['existing'], ('0.1.0', ['first']))})
        self.git('checkout', '-q', '-b', 'feature')
        self.commit('feat: x', {'CHANGELOG.md': render(['existing', 'feature'], ('0.1.0', ['first']))})
        self.git('checkout', '-q', 'main')
        self.commit('chore(release): prepare 0.2.0', {
            'CHANGELOG.md': render([], ('0.2.0', ['existing']), ('0.1.0', ['first']))})

        # Seen from its own fork point the branch is innocent...
        self.assertEqual(self.check('--base', 'main', '--head', 'feature')[0], 0)

        # ...but the rebase applies its hunk cleanly inside the section that was just released.
        self.git('checkout', '-q', 'feature')
        self.git('rebase', '-q', 'main')
        landed = cc.parse((self.repo / 'CHANGELOG.md').read_text(encoding='utf-8'))
        self.assertEqual([item.text for item in landed['0.2.0'].items], ['existing', 'feature'])

        code, out, _ = self.check('--base', 'main', '--head', 'feature')
        self.assertEqual(code, 1)
        self.assertIn('## [0.2.0] - 2026-01-01', out)
        self.assertIn('feature', out)

    def test_amend_released_trailer_waives_the_rule(self):
        self.commit('base', {'CHANGELOG.md': render([], ('0.1.0', ['x']))})
        self.git('checkout', '-q', '-b', 'feature')
        self.commit('docs(changelog): backfill\n\nChangelog: amend-released it shipped in 0.1.0 unrecorded',
                          {'CHANGELOG.md': render([], ('0.1.0', ['x', 'backfill']))})
        self.assertEqual(self.check('--base', 'main', '--head', 'feature')[0], 0)

    def test_the_none_trailer_does_not_waive_the_released_section_rule(self):
        self.commit('base', {'CHANGELOG.md': render([], ('0.1.0', ['x']))})
        self.git('checkout', '-q', '-b', 'feature')
        self.commit('docs: x\n\nChangelog: none wrong waiver',
                          {'CHANGELOG.md': render([], ('0.1.0', ['x', 'late']))})
        self.assertEqual(self.check('--base', 'main', '--head', 'feature')[0], 1)

    def extension_branch(self, message, files):
        self.commit('base', {
            'CHANGELOG.md': render([]), EXT: render(['old'], ('0.1.0', ['y'])),
            'editors/vscode/src/tree.ts': 'a\n', 'editors/vscode/src/tree.test.ts': 'a\n',
        })
        self.git('checkout', '-q', '-b', 'feature')
        self.commit(message, files)
        return self.check('--base', 'main', '--head', 'feature')

    def test_an_extension_change_without_an_entry_fails(self):
        code, out, _ = self.extension_branch('fix(vscode): x', {'editors/vscode/src/tree.ts': 'b\n'})
        self.assertEqual(code, 1)
        self.assertIn(EXT, out)
        self.assertIn('editors/vscode/src/tree.ts', out)

    def test_an_extension_change_with_an_entry_passes(self):
        code, _, _ = self.extension_branch('fix(vscode): x', {
            'editors/vscode/src/tree.ts': 'b\n', EXT: render(['old', 'new'], ('0.1.0', ['y']))})
        self.assertEqual(code, 0)

    def test_a_test_only_extension_change_passes(self):
        code, _, _ = self.extension_branch('test(vscode): x', {'editors/vscode/src/tree.test.ts': 'b\n'})
        self.assertEqual(code, 0)

    def test_the_none_trailer_waives_the_extension_entry(self):
        code, _, _ = self.extension_branch('chore(vscode): bump\n\nChangelog: none dev dependency only',
                                           {'editors/vscode/src/tree.ts': 'b\n'})
        self.assertEqual(code, 0)

    def test_no_extension_check_skips_the_entry_requirement(self):
        self.commit('base', {'CHANGELOG.md': render([]), 'editors/vscode/src/tree.ts': 'a\n'})
        self.git('checkout', '-q', '-b', 'feature')
        self.commit('fix(vscode): x', {'editors/vscode/src/tree.ts': 'b\n'})
        self.assertEqual(self.check('--base', 'main', '--head', 'feature')[0], 1)
        self.assertEqual(self.check('--base', 'main', '--head', 'feature', '--no-extension-check')[0], 0)

    def test_the_working_tree_is_checked_when_no_head_is_given(self):
        self.commit('base', {'CHANGELOG.md': render([], ('0.1.0', ['x']))})
        self.git('tag', 'v0.1.0')
        self.assertEqual(self.check('--base', 'v0.1.0', '--no-extension-check')[0], 0)
        self.write('CHANGELOG.md', render([], ('0.1.0', ['x', 'late'])))
        code, out, _ = self.check('--base', 'v0.1.0', '--no-extension-check')
        self.assertEqual(code, 1)
        self.assertIn('late', out)

    def test_an_untracked_extension_file_counts_in_the_working_tree(self):
        self.commit('base', {'CHANGELOG.md': render([])})
        self.write('editors/vscode/src/new.ts', 'x\n')
        self.assertEqual(self.check('--base', 'main')[0], 1)

    def test_the_audit_blames_only_what_the_tag_did_not_have(self):
        self.commit('base', {'CHANGELOG.md': render(['a'], ('0.1.0', ['x']))})
        self.git('tag', 'v0.1.0')
        self.commit('feat: more', {'CHANGELOG.md': render(['a', 'b'], ('0.1.0', ['x']))})
        self.assertEqual(self.check('--base', 'v0.1.0', '--head', 'HEAD', '--no-extension-check')[0], 0)

    def test_missing_changelogs_are_fine(self):
        self.commit('base', {'README.md': 'x\n'})
        self.assertEqual(self.check('--base', 'main')[0], 0)

    def test_an_unknown_base_is_a_usage_error(self):
        self.commit('base', {'CHANGELOG.md': render([])})
        code, out, err = self.check('--base', 'no-such-rev')
        self.assertEqual((code, out), (2, ''))
        self.assertIn('no-such-rev', err)

    def test_unrelated_histories_say_how_to_get_the_full_history(self):
        self.commit('base', {'CHANGELOG.md': render([])})
        self.git('checkout', '-q', '--orphan', 'other')
        self.commit('other', {'CHANGELOG.md': render([])})
        code, _, err = self.check('--base', 'main')
        self.assertEqual(code, 2)
        self.assertIn('fetch-depth: 0', err)


if __name__ == '__main__':
    unittest.main()
