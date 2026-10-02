#!/usr/bin/env python3
"""Exercise the Stop hook without AI requests: python3 scripts/test-commit-message-hook.py."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

HOOK = Path(__file__).resolve().parents[1] / '.claude/hooks/check-commit-messages.sh'


class CommitMessageHookTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='commit-hook-')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = self.root / 'repo with spaces'
        self.repo.mkdir()
        self.git('init', '-q', '-b', 'main')
        # Make disposable fixture commits without invoking git commit hooks.
        tree = self.git('hash-object', '-t', 'tree', '--stdin', input='')
        self.tree = tree
        self.new_head('fixture')
        self.bin = self.root / 'bin'
        self.bin.mkdir()
        # Resolve dependencies explicitly so absent-binary tests cannot find an
        # installed omni-dev later in PATH (or inherit the user's credentials).
        for tool in ('bash', 'cat', 'jq', 'git', 'mkdir', 'mktemp', 'rm', 'mv'):
            (self.bin / tool).symlink_to(shutil.which(tool))
        self.cli = self.bin / 'omni-dev'
        self.cli.write_text('''#!/bin/bash
printf '%s\\n' "$*" >> "$CALLS"
case "$MODE" in
  clean) echo '{"commits":[],"summary":{"error_count":0,"warning_count":0}}'; exit 0 ;;
  partial) echo '{"commits":[],"summary":{"error_count":0,"warning_count":0}}'; echo 'warning: 1 commit ultimately failed to check' >&2; exit 0 ;;
  malformed-clean) echo 'not JSON'; exit 0 ;;
  empty) echo 'error: no commits found in range' >&2; exit 3 ;;
  error) echo '{"commits":[{"issues":[{"severity":"error","rule":"invalid scope"}]}],"summary":{"error_count":1,"warning_count":0}}'; exit 1 ;;
  warning) echo '{"commits":[{"issues":[{"severity":"warning"}]}],"summary":{"error_count":0,"warning_count":1}}'; exit 2 ;;
  credentials) echo 'credentials unavailable' >&2; exit 1 ;;
  api) echo 'API credit balance exhausted' >&2; exit 1 ;;
  malformed) echo 'not JSON'; exit 1 ;;
  wrong-count) echo '{"commits":[],"summary":{"error_count":0,"warning_count":0}}'; exit 1 ;;
  unexpected) echo 'unavailable' >&2; exit 124 ;;
esac
''')
        self.cli.chmod(0o755)
        self.calls = self.root / 'calls'
        self.env = dict(os.environ, PATH=str(self.bin), CLAUDE_PROJECT_DIR=str(self.repo),
                        AI_SCRATCH=str(self.root / 'scratch with spaces'),
                        CALLS=str(self.calls), MODE='clean')
        self.env.pop('ANTHROPIC_API_KEY', None)

    def git(self, *args, input=None):
        return subprocess.run(['git', '-C', str(self.repo), *args], input=input,
                              text=True, capture_output=True, check=True).stdout.strip()

    def new_head(self, message):
        sha = self.git('commit-tree', '-S', self.tree,
                       '-m', message)
        self.git('update-ref', 'HEAD', sha)
        return sha

    def run_hook(self, mode='clean', active=False):
        self.env['MODE'] = mode
        return subprocess.run([str(self.bin / 'bash'), str(HOOK)], env=self.env,
                              cwd=self.root, input=json.dumps({'stop_hook_active': active}),
                              capture_output=True, text=True, timeout=10)

    def call_count(self):
        return len(self.calls.read_text().splitlines()) if self.calls.exists() else 0

    def assert_clean(self, result):
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout + result.stderr, '')

    def test_clean_and_unchanged_head_skip_ai(self):
        self.assert_clean(self.run_hook())
        self.assert_clean(self.run_hook('error'))
        self.assertEqual(self.call_count(), 1)
        self.assertEqual(self.calls.read_text().strip(),
                         'git commit message check --strict --quiet -o json')

    def test_amended_head_checks_again(self):
        self.assert_clean(self.run_hook())
        self.new_head('amended')
        self.assert_clean(self.run_hook())
        self.assertEqual(self.call_count(), 2)

    def test_findings_block_and_are_not_cached(self):
        for mode in ('error', 'warning'):
            with self.subTest(mode=mode):
                for _ in range(2):
                    result = self.run_hook(mode)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertIn('commit-twiddle', result.stderr)
                    self.assertIn('"severity"', result.stderr)
                    self.assertEqual(result.stdout, '')
        self.assertEqual(self.call_count(), 4)
        self.assert_clean(self.run_hook())

    def test_empty_range_is_silent_and_cached(self):
        self.assert_clean(self.run_hook('empty'))
        self.assert_clean(self.run_hook('error'))
        self.assertEqual(self.call_count(), 1)

    def test_reentrancy_skips_cli_and_cache(self):
        self.assert_clean(self.run_hook('error', active=True))
        self.assertEqual(self.call_count(), 0)
        self.assert_clean(self.run_hook())
        self.assertEqual(self.call_count(), 1)

    def test_infrastructure_fails_open_and_retries(self):
        for mode in ('credentials', 'api', 'malformed', 'wrong-count', 'unexpected',
                     'partial', 'malformed-clean'):
            with self.subTest(mode=mode):
                result = self.run_hook(mode)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn('check skipped', result.stderr)
                self.assertEqual(result.stdout, '')
        self.assertEqual(self.call_count(), 7)
        self.assert_clean(self.run_hook())
        self.assertEqual(self.call_count(), 8)

    def test_absent_binary_fails_open(self):
        self.cli.unlink()
        result = self.run_hook()
        self.assertEqual(result.returncode, 0)
        self.assertIn('omni-dev is unavailable', result.stderr)
        self.assertEqual(self.call_count(), 0)

    def test_absent_jq_fails_open(self):
        (self.bin / 'jq').unlink()
        result = self.run_hook()
        self.assertEqual(result.returncode, 0)
        self.assertIn('jq is unavailable', result.stderr)
        self.assertEqual(self.call_count(), 0)

    def test_unavailable_scratch_fails_open(self):
        blocked = self.root / 'not-a-directory'
        blocked.write_text('occupied')
        self.env['AI_SCRATCH'] = str(blocked / 'scratch')
        result = self.run_hook()
        self.assertEqual(result.returncode, 0)
        self.assertIn('scratch directory is unavailable', result.stderr)
        self.assertEqual(self.call_count(), 0)

    def test_git_root_scratch(self):
        self.env['AI_SCRATCH'] = 'git-root:local scratch'
        self.assert_clean(self.run_hook())
        self.assertEqual(len(list((self.repo / 'local scratch').glob('*.head'))), 1)
        self.assert_clean(self.run_hook('error'))
        self.assertEqual(self.call_count(), 1)

    def test_tmpdir_fallback(self):
        del self.env['AI_SCRATCH']
        self.env['TMPDIR'] = str(self.root / 'fallback')
        self.assert_clean(self.run_hook())
        self.assertEqual(len(list((self.root / 'fallback').glob('*.head'))), 1)

    def test_shared_scratch_isolates_worktrees(self):
        self.assert_clean(self.run_hook())
        other = self.root / 'other worktree'
        self.git('worktree', 'add', '-q', '-b', 'other', str(other))
        self.env['CLAUDE_PROJECT_DIR'] = str(other)
        self.assert_clean(self.run_hook())
        self.assert_clean(self.run_hook('error'))
        self.assertEqual(self.call_count(), 2)
        self.assertEqual(len(list((self.root / 'scratch with spaces').glob('*.head'))), 2)
        self.assertFalse(list((self.root / 'scratch with spaces').glob('*.????????')))


if __name__ == '__main__':
    unittest.main(verbosity=2)
