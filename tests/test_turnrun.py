"""The offline turn-run driver: inventory validation, history-free fixtures, the hash-chained ledger,
pinned campaigns, budget reservation and outcome classes. No model call: a fake runner stands in for
the Rust binary and writes the record a scenario names."""
import json
import os
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
import turnrun

GIT_ENV = turnrun.FIXTURE_ENV

# The fake runner: reads its own flags, edits the fixture (or not) and writes the record that
# $SCENARIO_FILE names for this run, the way cli/src/bin/turnrun.rs would.
FAKE_RUNNER = r'''
import json, os, sys, time
from pathlib import Path
args = sys.argv[1:]
opt = {args[i]: args[i + 1] for i in range(0, len(args) - 1) if args[i].startswith('--')}
Path(opt['--record']).parent.mkdir(parents=True, exist_ok=True)
Path(opt['--record'] + '.argv').write_text(json.dumps(args))
plan = json.loads(Path(os.environ['SCENARIO_FILE']).read_text())
if plan.get('fix'):
    Path(opt['--project'], 'value.py').write_text('def f(x):\n    return 2 * x\n')
if plan.get('sleep'):
    time.sleep(plan['sleep'])
if plan.get('no_record'):
    sys.exit(4)
Path(opt['--record']).write_text(json.dumps(plan['record']))
sys.exit(plan.get('exit', 0))
'''

CHECK = r'''
import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from value import f
if f(5) != 10:
    print('f(5) is not 10')
    sys.exit(1)
print('f(5) is 10')
'''


def usage(total=100, counted=True, uncounted=0, cache_read=0):
    rest = total - cache_read
    return {'input': rest // 2, 'cache_write': 0, 'cache_read': cache_read, 'output': rest - rest // 2,
            'calls': 1, 'uncounted_calls': uncounted, 'counted': counted}


def git(repo, *args):
    return subprocess.run(['git', '-C', str(repo), *args], capture_output=True, text=True, check=True,
                          env={**os.environ, **GIT_ENV}).stdout.strip()


class DriverTest(unittest.TestCase):
    def setUp(self):
        child = os.environ.pop('RINGO_CHILD', None)  # tests run inside a Ringo turn too
        if child is not None:
            self.addCleanup(os.environ.__setitem__, 'RINGO_CHILD', child)
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()
        self.source = self.root / 'source'
        self.source.mkdir()
        git(self.source, 'init', '-q')
        (self.source / 'value.py').write_text('def f(x):\n    return x\n')
        (self.source / 'README').write_text('a project\n')
        git(self.source, 'add', '-A')
        git(self.source, 'commit', '-q', '-m', 'start')
        self.parent = git(self.source, 'rev-parse', 'HEAD')
        (self.source / 'value.py').write_text('def f(x):\n    return x + x\n')
        git(self.source, 'commit', '-qam', 'half the fix')
        self.fix1 = git(self.source, 'rev-parse', 'HEAD')
        (self.source / 'value.py').write_text('def f(x):\n    return 2 * x\n')
        git(self.source, 'commit', '-qam', 'the fix')
        self.fix2 = git(self.source, 'rev-parse', 'HEAD')
        self.day = git(self.source, 'log', '-1', '--format=%cs', self.fix2)

        self.tasks = self.root / 'tasks'
        (self.tasks / 'checks').mkdir(parents=True)
        (self.tasks / 'checks' / 'T01_double.py').write_text(CHECK)
        self.write_tasks([self.task('T01')])

        self.runner = self.root / 'fake_runner.py'
        self.runner.write_text(FAKE_RUNNER)
        self.program = self.root / 'fake-agent'
        self.program.write_text('#!/bin/sh\n')
        self.scenario = self.root / 'scenario.json'
        self.campaign = self.root / 'campaign'
        os.environ['SCENARIO_FILE'] = str(self.scenario)
        self.addCleanup(os.environ.pop, 'SCENARIO_FILE', None)

    def task(self, tid, **changes):
        return {'id': tid, 'cluster': 'doubling', 'date': self.day, 'runtime': 'python', 'title': 'f doubles',
                'brief': 'f(5) should be 10 but is 5.', 'parent_commit': self.parent,
                'fix_commits': [self.fix1, self.fix2], 'check': 'checks/T01_double.py', **changes}

    def write_tasks(self, items):
        (self.tasks / 'tasks.json').write_text(json.dumps({'tasks': items}))

    def cli(self, *argv):
        out, err = StringIO(), StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = turnrun.main(['--tasks', str(self.tasks), '--source', str(self.source), *argv])
        return code, out.getvalue(), err.getvalue()

    def run_task(self, plan, *extra, task='T01', tokens=1000):
        self.scenario.write_text(json.dumps(plan))
        return self.cli('run', task, '--campaign', str(self.campaign), '--agent', 'claude',
                        '--program', str(self.program), '--binary', str(self.runner),
                        '--budget-tokens', str(tokens), '--budget-seconds', '60', *extra)

    def ledger(self):
        return turnrun.ledger_entries(self.campaign / 'ledger.jsonl')

    def runs(self):
        return [e for e in self.ledger() if e['kind'] == 'run']

    # --- inventory ---

    def test_inventory_checks_history(self):
        self.assertEqual(len(turnrun.load_inventory(self.tasks, self.source)), 1)
        side = git(self.source, 'commit-tree', f'{self.parent}^{{tree}}', '-p', self.parent, '-m', 'side')
        cases = {
            'not the first fix\'s parent': self.task('T01', fix_commits=[self.fix2]),
            'does not follow': self.task('T01', fix_commits=[self.fix1, side]),
            'also a fix': self.task('T01', fix_commits=[self.fix1, self.parent]),
            'last fix\'s date': self.task('T01', date='2001-01-01'),
            'carries a commit id': self.task('T01', brief=f'see {self.fix2[:8]}'),
            'full 40-character': self.task('T01', parent_commit=self.parent[:12]),
            'runtime': self.task('T01', runtime='go'),
            'inside the inventory': self.task('T01', check='../escape.py'),
            'is not in': self.task('T01', fix_commits=['0' * 40]),
        }
        for why, bad in cases.items():
            with self.subTest(why):
                self.write_tasks([bad])
                with self.assertRaisesRegex(ValueError, why):
                    turnrun.load_inventory(self.tasks, self.source)
        self.write_tasks([self.task('T01'), self.task('T01')])
        with self.assertRaisesRegex(ValueError, 'unique'):
            turnrun.load_inventory(self.tasks, self.source)

    def test_private_paths_are_refused_inside_the_repository(self):
        for argv in (['inventory'], ['totals', str(turnrun.REPO / 'x')],
                     ['verify-inventory', '--work', str(turnrun.REPO / 'scratch')]):
            with self.subTest(argv[0]):
                tasks = str(turnrun.REPO / 'tasks') if argv[0] == 'inventory' else str(self.tasks)
                out, err = StringIO(), StringIO()
                with redirect_stdout(out), redirect_stderr(err):
                    code = turnrun.main(['--tasks', tasks, '--source', str(self.source), *argv])
                self.assertEqual(code, 2)
                self.assertIn('inside the repository', err.getvalue())

    # --- fixtures ---

    def test_fixture_has_one_commit_and_no_way_back_to_the_fix(self):
        a = turnrun.build_fixture(self.source, self.parent, self.root / 'a')
        b = turnrun.build_fixture(self.source, self.parent, self.root / 'b')
        self.assertEqual(a['setup_commit'], b['setup_commit'])
        self.assertEqual(a['setup_tree'], git(self.source, 'rev-parse', f'{self.parent}^{{tree}}'))
        fixture = Path(a['dir'])
        self.assertEqual(git(fixture, 'rev-list', '--all', '--count'), '1')
        self.assertEqual(git(fixture, 'log', '--format=%s'), 'task snapshot')
        self.assertEqual(git(fixture, 'remote'), '')
        for sha in (self.parent, self.fix1, self.fix2):
            self.assertNotEqual(subprocess.run(['git', '-C', str(fixture), 'cat-file', '-e', sha],
                                               capture_output=True).returncode, 0)
        self.assertIn('return x\n', (fixture / 'value.py').read_text())
        with self.assertRaisesRegex(ValueError, 'not empty'):
            turnrun.build_fixture(self.source, self.parent, fixture)

    def test_fixture_ignores_inherited_git_configuration_and_templates(self):
        from unittest.mock import patch
        template = self.root / 'git-template'
        template.mkdir()
        (template / 'leaked-owner-config').write_text('must not be copied')
        with patch.dict(os.environ, {'GIT_DIR': str(self.source / '.git'),
                                     'GIT_TEMPLATE_DIR': str(template), 'GIT_CONFIG_COUNT': '1',
                                     'GIT_CONFIG_KEY_0': 'core.autocrlf', 'GIT_CONFIG_VALUE_0': 'true'}):
            fixture = turnrun.build_fixture(self.source, self.parent, self.root / 'isolated')
        self.assertFalse((Path(fixture['dir']) / '.git' / 'leaked-owner-config').exists())
        self.assertEqual(git(Path(fixture['dir']), 'rev-list', '--all', '--count'), '1')

    def test_verify_inventory_needs_fail_at_base_and_pass_at_fix(self):
        work = self.root / 'work'
        code, out, _ = self.cli('verify-inventory', '--work', str(work), '--report', str(self.root / 'r.json'))
        self.assertEqual(code, 0, out)
        report = json.loads((self.root / 'r.json').read_text())
        self.assertEqual(report[0]['verdict'], 'discriminates')
        self.assertEqual((report[0]['base']['exit'], report[0]['fix']['exit']), (1, 0))
        self.assertFalse((work / 'T01-base').exists())
        (self.tasks / 'checks' / 'T01_double.py').write_text('print("always fine")\n')
        code, out, _ = self.cli('verify-inventory', '--work', str(work))
        self.assertEqual((code, json.loads(out)['verdict']), (1, 'BROKEN'))

    # --- runs ---

    def test_a_fixing_run_passes_its_check_and_is_ledgered_with_its_record(self):
        code, out, _ = self.run_task({'fix': True, 'record': {'outcome': 'done', 'usage': usage(300, cache_read=200)}})
        self.assertEqual(code, 0)
        self.assertEqual(json.loads(out)['class'], 'pass')
        kinds = [e['kind'] for e in self.ledger()]
        self.assertEqual(kinds, ['campaign', 'start', 'run'])
        run = self.runs()[0]
        self.assertEqual((run['usage_total'], run['check_exit'], run['evidence']), (300, 0, 'f(5) is 10'))
        argv = json.loads(Path(run['record_path'] + '.argv').read_text())
        self.assertEqual(argv[argv.index('--mode') + 1], 'auto')
        self.assertNotIn('--write', argv)
        self.assertEqual(argv[argv.index('--budget-tokens') + 1], '1000')
        brief = Path(argv[argv.index('--message-file') + 1]).read_text()
        self.assertEqual(brief.strip(), 'f(5) should be 10 but is 5.')
        totals = turnrun.totals_of(self.ledger())
        self.assertEqual((totals['known'], totals['known_without_cache_read'], totals['committed']), (300, 100, 300))

    def test_classes(self):
        cases = [
            ({'record': {'outcome': 'done', 'usage': usage()}}, 'treatment_failure'),
            ({'record': {'outcome': 'cap_breach', 'usage': usage(1200)}, 'exit': 3}, 'cap_breach'),
            ({'record': {'outcome': 'timeout', 'usage': usage()}, 'exit': 3}, 'timeout'),
            ({'record': {'outcome': 'failed', 'reason': 'You have hit your usage limit', 'usage': usage(0)}, 'exit': 1},
             'quota_pause'),
            ({'record': {'outcome': 'failed', 'reason': 'the agent exited 2', 'usage': usage()}, 'exit': 1},
             'delivery_failure'),
            ({'no_record': True}, 'delivery_failure'),
        ]
        for n, (plan, want) in enumerate(cases, 1):
            with self.subTest(want):
                code, out, err = self.run_task(plan, '--rep', str(n))
                self.assertEqual((code, json.loads(out)['class']), (0, want), err)
        self.assertEqual(turnrun.classify({'outcome': 'done'}, 2), 'quality_failure')
        self.assertEqual(turnrun.classify({'outcome': 'done'}, 3), 'check_unavailable')
        self.assertEqual(turnrun.classify({'outcome': 'done'}, 7), 'check_error')
        self.assertEqual(turnrun.classify({'outcome': 'done'}, None), 'check_error')
        self.assertEqual(turnrun.classify({'outcome': 'done'}, checked=False), 'unchecked')

    def test_unknown_usage_reserves_the_whole_cap(self):
        self.run_task({'record': {'outcome': 'done', 'usage': usage(50, counted=False)}}, '--rep', '1')
        self.run_task({'record': {'outcome': 'done', 'usage': usage(50, uncounted=1)}}, '--rep', '2')
        self.run_task({'no_record': True}, '--rep', '3')
        self.run_task({'record': {'outcome': 'done', 'usage': usage(40)}}, '--rep', '4', '--warmup')
        self.assertEqual([r['usage_total'] for r in self.runs()], [None, None, None, 40])
        totals = turnrun.totals_of(self.ledger())
        self.assertEqual((totals['known'], totals['warmup'], totals['reserved'], totals['unknown_runs']),
                         (40, 40, 3000, 3))
        self.assertEqual(turnrun.usage_of({'outcome': 'refused'}), ({k: 0 for k in turnrun.USAGE_KEYS}, 0))

    def test_partial_usage_above_the_cap_is_reserved_at_its_known_lower_bound(self):
        self.run_task({'record': {'outcome': 'done', 'usage': usage(1500, uncounted=1)}})
        self.assertIsNone(self.runs()[0]['usage_total'])
        self.assertEqual(turnrun.totals_of(self.ledger())['reserved'], 1500)
        for bad in (-1, '10', None, True):
            with self.subTest(bad=bad), self.assertRaisesRegex(ValueError, 'invalid usage'):
                turnrun.usage_of({'usage': {**usage(), 'input': bad}})

    def test_total_budget_stops_before_a_run_that_might_not_fit(self):
        done = {'record': {'outcome': 'done', 'usage': usage(600)}}
        self.assertEqual(self.run_task(done, '--rep', '1', '--total-budget-tokens', '2000')[0], 0)
        code, out, _ = self.run_task(done, '--rep', '2', '--total-budget-tokens', '2000', tokens=1500)
        self.assertEqual((code, json.loads(out)['class']), (3, 'budget_stop'))
        self.assertEqual(self.ledger()[-1]['kind'], 'budget_stop')
        self.assertFalse((self.campaign / 'runs' / 'T01-2').exists())
        self.assertEqual(self.run_task(done, '--rep', '2', '--total-budget-tokens', '2000')[0], 0)

    def test_a_campaign_cannot_drop_or_increase_its_total_budget(self):
        done = {'record': {'outcome': 'done', 'usage': usage(600)}}
        self.assertEqual(self.run_task(done, '--total-budget-tokens', '2000')[0], 0)
        for extra in ([], ['--total-budget-tokens', '3000']):
            code, _, err = self.run_task(done, '--rep', '2', *extra)
            self.assertEqual(code, 2)
            self.assertIn('total_budget_tokens', err)
        self.assertEqual(len(self.runs()), 1)

    def test_invalid_repetition_cannot_escape_or_alias_a_run_folder(self):
        for rep in ('../../escape', '1/warmup', '1:warmup', '1-warmup', '0', '-1'):
            with self.subTest(rep=rep):
                code, _, err = self.run_task({'record': {'outcome': 'done'}}, '--rep', rep)
                self.assertEqual(code, 2)
                self.assertIn('positive integer', err)
        self.assertFalse(self.campaign.exists())

    def test_a_key_runs_once_and_an_interrupted_one_is_retried_once(self):
        done = {'fix': True, 'record': {'outcome': 'done', 'usage': usage()}}
        self.run_task(done)
        code, _, err = self.run_task(done)
        self.assertEqual(code, 2)
        self.assertIn('already ledgered', err)
        # A start with no run after it: the driver died mid-run.
        turnrun.ledger_append(self.campaign / 'ledger.jsonl', {
            'kind': 'start', 'key': 'T01:2', 'task': 'T01', 'warmup': False,
            'setup_commit': self.ledger()[1]['setup_commit'], 'budget': {'tokens': 1000, 'seconds': 60}})
        (self.campaign / 'runs' / 'T01-2').mkdir(parents=True)
        self.assertEqual(turnrun.totals_of(self.ledger())['reserved'], 1000)
        code, _, err = self.run_task(done, '--rep', '2')
        self.assertEqual(code, 2)
        self.assertIn('never finished', err)
        code, out, err = self.run_task(done, '--rep', '2', '--retry-interrupted')
        self.assertEqual((code, json.loads(out)['class']), (0, 'pass'), err)
        self.assertTrue((self.campaign / 'runs' / 'T01-2.abandoned').is_dir())
        self.assertEqual([e['kind'] for e in self.ledger()][-4:], ['start', 'abandoned', 'start', 'run'])
        self.assertEqual(turnrun.totals_of(self.ledger())['reserved'], 1000)

    def test_the_campaign_pins_its_setup(self):
        done = {'record': {'outcome': 'done', 'usage': usage()}}
        self.run_task(done, '--rep', '1')
        self.program.write_text('#!/bin/sh\n# changed\n')
        code, _, err = self.run_task(done, '--rep', '2')
        self.assertEqual(code, 2)
        self.assertIn('program_sha256', err)
        self.program.write_text('#!/bin/sh\n')
        (self.tasks / 'checks' / 'T01_double.py').write_text(CHECK + '\n')
        code, _, err = self.run_task(done, '--rep', '2')
        self.assertIn('inventory_sha256', err)

    def test_ledger_chain_and_records_are_tamper_evident(self):
        self.run_task({'record': {'outcome': 'done', 'usage': usage()}})
        record = Path(self.runs()[0]['record_path'])
        record.write_text(record.read_text().replace('"done"', '"done" '))
        with self.assertRaisesRegex(ValueError, 'gone or changed'):
            self.ledger()
        record.unlink()
        self.campaign.joinpath('ledger.jsonl').write_text(
            '\n'.join(self.campaign.joinpath('ledger.jsonl').read_text().splitlines()[1:]) + '\n')
        with self.assertRaisesRegex(ValueError, 'breaks the chain'):
            self.ledger()

    def test_live_programs_and_a_held_lock_are_refused(self):
        live = self.root / 'bin' / 'codex'
        live.parent.mkdir()
        live.write_text('#!/bin/sh\n')
        self.scenario.write_text(json.dumps({'record': {'outcome': 'done'}}))
        code, _, err = self.cli('run', 'T01', '--campaign', str(self.campaign), '--agent', 'codex',
                                '--program', str(live), '--binary', str(self.runner),
                                '--budget-tokens', '10', '--budget-seconds', '10')
        self.assertEqual(code, 2)
        self.assertIn('installed agent CLI', err)
        self.assertFalse((self.campaign / 'ledger.jsonl').exists())
        os.environ['RINGO_CHILD'] = '1'
        try:
            code, _, err = self.run_task({'record': {'outcome': 'done'}})
        finally:
            os.environ.pop('RINGO_CHILD')
        self.assertEqual(code, 2)
        self.assertIn('RINGO_CHILD is set', err)
        self.campaign.mkdir()
        (self.campaign / '.lock').write_text('1')
        code, _, err = self.run_task({'record': {'outcome': 'done'}})
        self.assertEqual(code, 2)
        self.assertIn('another driver', err)

    def test_the_driver_stops_a_runner_that_outlives_its_wall_cap(self):
        old = turnrun.RUNNER_GRACE
        turnrun.RUNNER_GRACE = 1
        self.addCleanup(setattr, turnrun, 'RUNNER_GRACE', old)
        self.scenario.write_text(json.dumps({'sleep': 30, 'record': {'outcome': 'done'}}))
        code, out, _ = self.cli('run', 'T01', '--campaign', str(self.campaign), '--agent', 'claude',
                                '--program', str(self.program), '--binary', str(self.runner),
                                '--budget-tokens', '10', '--budget-seconds', '1')
        self.assertEqual((code, json.loads(out)['class']), (0, 'timeout'))
        self.assertIn('stopped the runner', json.loads(Path(self.runs()[0]['record_path']).read_text())['reason'])
        self.assertIsNone(self.runs()[0]['usage_total'])

    def test_split_keeps_clusters_whole_by_their_latest_date(self):
        self.write_tasks([self.task('T01', cluster='a'), self.task('T02', cluster='b')])
        items = json.loads((self.tasks / 'tasks.json').read_text())['tasks']
        items[0]['date'], items[1]['date'] = '2026-01-01', '2026-03-01'
        items.append({**items[0], 'id': 'T03', 'date': '2026-04-01'})
        self.write_tasks(items)
        code, out, _ = self.cli('split', '--dev-until', '2026-02-01')
        self.assertEqual(code, 0)
        split = json.loads(out)
        self.assertEqual(split['tasks'], {'T01': 'validation', 'T02': 'validation', 'T03': 'validation'})
        code, out, _ = self.cli('split', '--dev-until', '2026-03-15')
        self.assertEqual(json.loads(out)['tasks'], {'T01': 'validation', 'T02': 'dev', 'T03': 'validation'})


if __name__ == '__main__':
    unittest.main()
