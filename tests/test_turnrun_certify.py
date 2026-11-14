"""Grader failures cannot become certification; opt-in Docker test runs both snapshots."""
import json
import os
from pathlib import Path
import tempfile
import unittest

from turnrun import clean_env, FIXTURE_ENV, git
import turnrun_certify as certify


class VerdictTest(unittest.TestCase):
    def test_infrastructure_failure_and_inconsistent_unavailability_fail_closed(self):
        valid_base, valid_fix = {'exit': 1}, {'exit': 0}
        self.assertEqual(certify.verdict(valid_base, valid_fix), 'discriminates')
        for failure in ({'exit': 0, 'timeout': True}, {'exit': 0, 'truncated': True},
                        {'error': 'lost Docker'}, {'exit': 4}, {'exit': 3}):
            with self.subTest(failure=failure):
                self.assertEqual(certify.verdict(valid_base, failure), 'BROKEN')
        self.assertEqual(certify.verdict({'exit': 3}, {'exit': 3}), 'unavailable here')


@unittest.skipUnless(os.environ.get('RINGO_TEST_IMAGE'), 'requires a pinned Docker image')
class ContainedCertificationTest(unittest.TestCase):
    def test_pristine_snapshots_and_hashed_private_evidence(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source, tasks, work = root / 'source', root / 'tasks', root / 'evidence'
            source.mkdir()
            tasks.mkdir()
            env = clean_env(FIXTURE_ENV)
            git(source, 'init', '-q', '--template=', env=env)
            commits = []
            for value in ('broken', 'fixed'):
                (source / 'value.txt').write_text(value)
                git(source, 'add', '.', env=env)
                git(source, 'commit', '-qm', value, '--no-gpg-sign', env=env)
                commits.append(git(source, 'rev-parse', 'HEAD', env=env))
            (tasks / 'check.py').write_text('import sys\nfrom pathlib import Path\n'
                'raise SystemExit(0 if (Path(sys.argv[1])/"value.txt").read_text()=="fixed" else 1)\n')
            (tasks / 'tasks.json').write_text(json.dumps({'tasks': [{'id': 'T01', 'cluster': 'one',
                'brief': 'Repair the value', 'title': 'Repair', 'runtime': 'python', 'date': '2026-01-01',
                'parent_commit': commits[0], 'fix_commits': [commits[1]], 'check': 'check.py'}]}))
            # A thin image may lack git, which must be reported as unavailable, never passed.
            result = certify.verify(tasks=tasks, source=source, work=work,
                                    image=os.environ['RINGO_TEST_IMAGE'])
            self.assertFalse(result['live_certified'])
            self.assertEqual(result['provider_calls'], 0)
            self.assertEqual(json.loads((work/'report.json').read_text()), result)
            if result.get('profile', {}).get('git'):
                self.assertTrue(result['all_selected_discriminate'], result)
                self.assertEqual((result['rows'][0]['base']['exit'], result['rows'][0]['fix']['exit']), (1, 0))
                self.assertFalse((work/'T01-base').exists())
            else:
                self.assertFalse(result['all_selected_discriminate'])
            with self.assertRaisesRegex(ValueError, 'must not exist'):
                certify.verify(tasks=tasks, source=source, work=work, image=os.environ['RINGO_TEST_IMAGE'])


if __name__ == '__main__':
    unittest.main()
