import copy
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest
from unittest.mock import patch

import branch_cleanup as cleanup


def command(repo, *args, env=None):
    result = subprocess.run(args, cwd=repo, env=env, text=True, capture_output=True)
    if result.returncode:
        raise RuntimeError(result.stderr)
    return result.stdout.strip()


class FakeAPI:
    def __init__(self, repo):
        refs = command(repo, 'git', 'for-each-ref',
                       '--format=%(refname:strip=3) %(objectname)', 'refs/remotes/origin')
        self.branches = {line.split()[0]: {'sha': line.split()[1], 'protected': False}
                         for line in refs.splitlines() if not line.startswith('HEAD ')}
        self.branches['protected-old']['protected'] = True
        self.pulls = {'open-old'}
        self.calls = 0
        self.second = None

    def snapshot(self):
        self.calls += 1
        if self.calls == 2 and self.second:
            self.second(self)
        return 'main', copy.deepcopy(self.branches), set(self.pulls)


class PolicyTests(unittest.TestCase):
    def test_age_boundary_and_protection(self):
        branch = {'protected': False}
        args = ('feature', branch, 'main', set())
        self.assertIsNone(cleanup.exclusion(*args, 999, 1000, cleanup.PROTECTED))
        self.assertEqual(cleanup.exclusion(*args, 1000, 1000, cleanup.PROTECTED),
                         'younger-than-minimum-age')
        for name in ['main', 'master', 'dev', 'develop', 'release/v1', 'hotfix/fix']:
            self.assertEqual(cleanup.exclusion(name, branch, 'main', set(), 0, 1,
                                              cleanup.PROTECTED), 'protected-name')

    def test_pagination_and_api_error(self):
        api = cleanup.GitHub('owner/repo', 'test')
        with patch.object(api, 'get', side_effect=[list(range(100)), [100]]):
            self.assertEqual(len(api.pages('/branches')), 101)
        with patch.object(api, 'get', side_effect=[list(range(100)), RuntimeError('API down')]):
            with self.assertRaisesRegex(RuntimeError, 'API down'):
                api.pages('/branches')

    def test_missing_protection_data_fails_closed(self):
        api = cleanup.GitHub('owner/repo', 'test')
        with patch.object(api, 'get', return_value={'default_branch': 'main'}), \
             patch.object(api, 'pages', side_effect=[[{'name': 'main', 'commit': {'sha': 'a'}}], []]):
            with self.assertRaises(KeyError):
                api.snapshot()


@unittest.skipUnless(os.environ.get('GIT_BARBER_BINARY'), 'Set GIT_BARBER_BINARY to v0.3.0')
class IntegrationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.source = self.root / 'source'
        self.remote = self.root / 'origin.git'
        self.repo = self.root / 'runner'
        self.source.mkdir()
        command(self.source, 'git', 'init', '-b', 'main')
        command(self.source, 'git', 'config', 'user.name', 'Cleanup Test')
        command(self.source, 'git', 'config', 'user.email', 'cleanup@example.invalid')
        self.now = int(time.time())
        self.old = self.now - 10 * 86400
        self.commit('seed', self.old)
        for name in ['merged-old', 'squash-old', 'rebase-old', 'recent',
                     'release/v1', 'hotfix/fix', 'dev', 'develop', 'master',
                     'protected-old', 'open-old', 'unmerged-old']:
            command(self.source, 'git', 'checkout', 'main')
            command(self.source, 'git', 'checkout', '-b', name)
            self.commit(name.replace('/', '-'), self.now if name == 'recent' else self.old)
            if name == 'rebase-old':
                self.commit('rebase-second', self.old + 1)
            command(self.source, 'git', 'checkout', 'main')
            if name == 'unmerged-old':
                continue
            if name == 'squash-old':
                command(self.source, 'git', 'merge', '--squash', name)
                env = dict(os.environ, GIT_COMMITTER_DATE=f'@{self.now} +0000')
                command(self.source, 'git', 'commit', '-m', 'squash result', env=env)
            elif name == 'rebase-old':
                # Change ancestry before replaying both patches.
                self.commit('diverge', self.now)
                shas = command(self.source, 'git', 'rev-list', '--reverse',
                               'main..' + name).splitlines()
                env = dict(os.environ, GIT_COMMITTER_DATE=f'@{self.now} +0000')
                command(self.source, 'git', 'cherry-pick', *shas, env=env)
            else:
                command(self.source, 'git', 'merge', '--no-ff', '-m', 'merge ' + name, name)
        command(self.root, 'git', 'clone', '--bare', str(self.source), str(self.remote))
        command(self.root, 'git', 'clone', str(self.remote), str(self.repo))
        command(self.repo, 'git', 'checkout', '--detach')
        command(self.repo, 'git', 'branch', '-D', 'main')
        self.api = FakeAPI(self.repo)
        self.binary = str(Path(os.environ['GIT_BARBER_BINARY']).resolve())

    def tearDown(self):
        self.tmp.cleanup()

    def commit(self, name, timestamp):
        (self.source / (name + '.txt')).write_text(name + '\n')
        command(self.source, 'git', 'add', '.')
        env = dict(os.environ, GIT_AUTHOR_DATE=f'@{timestamp} +0000',
                   GIT_COMMITTER_DATE=f'@{timestamp} +0000')
        command(self.source, 'git', 'commit', '-m', name, env=env)

    def refs(self):
        return set(command(self.remote, 'git', 'for-each-ref',
                           '--format=%(refname:strip=2)', 'refs/heads').splitlines())

    def clean(self, dry_run=False):
        report = {'status': 'failed'}
        code = cleanup.clean(self.repo, self.api, self.binary, dry_run, 7,
                             cleanup.PROTECTED, report, now=self.now)
        return code, report

    def test_dry_run_preserves_every_remote_branch(self):
        before = self.refs()
        code, report = self.clean(True)
        self.assertEqual(code, 0)
        self.assertEqual(self.refs(), before)
        candidates = {r['name']: r['kind'] for r in report['branches']
                      if r['status'] == 'candidate'}
        self.assertEqual(candidates, {'merged-old': 'merged', 'squash-old': 'squash',
                                      'rebase-old': 'rebase'})

    def test_deletes_three_merge_kinds_and_preserves_exceptions(self):
        before = self.refs()
        code, report = self.clean()
        self.assertEqual(code, 0)
        self.assertEqual(before - self.refs(), {'merged-old', 'squash-old', 'rebase-old'})
        self.assertEqual(len(report['results']), 3)
        output = self.root / 'report'
        cleanup.write_report(output, report)
        self.assertEqual(len(json.loads((output / 'report.json').read_text())['results']), 3)
        self.assertIn('git push origin ', (output / 'undo.txt').read_text())

    def test_api_failure_before_delete_preserves_remote(self):
        before = self.refs()
        def fail(api):
            raise RuntimeError('API unavailable')
        self.api.second = fail
        with self.assertRaisesRegex(RuntimeError, 'API unavailable'):
            self.clean()
        self.assertEqual(self.refs(), before)

    def test_new_pr_new_protection_and_moved_tip_are_rechecked(self):
        def change(api):
            api.pulls.add('merged-old')
            api.branches['squash-old']['protected'] = True
            api.branches['rebase-old']['sha'] = api.branches['main']['sha']
        self.api.second = change
        before = self.refs()
        code, report = self.clean()
        self.assertEqual(code, 0)
        self.assertEqual(self.refs(), before)
        reasons = {r['name']: r['reason'] for r in report['branches']}
        self.assertEqual(reasons['merged-old'], 'open-pull-request')
        self.assertEqual(reasons['squash-old'], 'github-protected')
        self.assertEqual(reasons['rebase-old'], 'branch-moved-or-gone')

    def test_lease_refuses_push_after_final_api_check(self):
        original_run = cleanup.run
        new_sha = self.api.branches['unmerged-old']['sha']
        def race(repo, *args):
            if '--yes' in args:
                command(self.remote, 'git', 'update-ref', 'refs/heads/merged-old', new_sha)
            return original_run(repo, *args)
        with patch.object(cleanup, 'run', side_effect=race):
            code, report = self.clean()
        self.assertEqual(code, 1)
        self.assertEqual(command(self.remote, 'git', 'rev-parse', 'merged-old'), new_sha)
        failed = [r for r in report['results'] if r['name'] == 'merged-old'][0]
        self.assertEqual(failed['remote']['status'], 'failed')
        self.assertEqual(report['status'], 'failed')

    def test_refuses_developer_checkout(self):
        command(self.repo, 'git', 'branch', 'personal-work')
        with self.assertRaisesRegex(RuntimeError, 'no local branches'):
            self.clean(True)

    def test_refuses_shallow_checkout(self):
        shallow = self.root / 'shallow'
        command(self.root, 'git', 'clone', '--no-local', '--depth=1',
                str(self.remote), str(shallow))
        command(shallow, 'git', 'checkout', '--detach')
        command(shallow, 'git', 'branch', '-D', 'main')
        self.repo = shallow
        with self.assertRaisesRegex(RuntimeError, 'Full Git history'):
            self.clean(True)


if __name__ == '__main__':
    unittest.main()
