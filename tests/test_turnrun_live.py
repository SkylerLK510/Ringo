"""Live integration gates using deterministic transports; no provider inference."""
import copy
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import turnrun_live as live
from turnrun_campaign import demonstration_plan, digest
from turnrun_native import NativeAttemptError
from turnrun_native_bridge import arguments
from turnrun import ledger_entries, usage_of


def result(tokens=20):
    return {'outcome': 'done', 'usage': {'input': tokens//2, 'cache_write': 0,
            'cache_read': tokens-tokens//2, 'output': 0, 'counted': True, 'uncounted_calls': 0}}


def events(sid, tokens=20):
    return [{'type': 'system', 'subtype': 'init', 'session_id': sid},
            {'type': 'assistant', 'parent_tool_use_id': None, 'message': {'id': 'm',
                'content': [{'type': 'text', 'text': 'done'}],
                'usage': {'input_tokens': tokens//2, 'cache_read_input_tokens': tokens-tokens//2, 'output_tokens': 0}}},
            {'type': 'result', 'subtype': 'success', 'is_error': False, 'result': 'done',
                'usage': {'input_tokens': tokens//2, 'cache_creation_input_tokens': 0,
                          'cache_read_input_tokens': tokens-tokens//2, 'output_tokens': 0}}]


@unittest.skipUnless(hasattr(socket, 'AF_UNIX') and os.name == 'posix', 'Unix broker socket')
class TransportTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='rrsi-test-', dir='/tmp')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.calls = []

    def invoke(self, prompt, resume, remaining, seconds, emit):
        self.calls.append((prompt, resume, remaining))
        sid = resume or 'session-'+str(len(self.calls))
        for event in events(sid):
            emit(event)
        return result()

    def service(self, invoke=None, cap=100, attempts=4):
        return live.SessionTransport(self.root / 'broker.sock', invoke or self.invoke, cap=cap,
                                     seconds=10, attempts=attempts, ledger=self.root / 'ledger')

    def request(self, service, prompt='next', resume=None, extra=None):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.connect(str(service.path))
            client.settimeout(5)
            client.sendall(json.dumps({'prompt': prompt, 'resume': resume, **(extra or {})}).encode()+b'\n')
            data = b''
            while chunk := client.recv(65536):
                data += chunk
            return [json.loads(line) for line in data.splitlines()]

    def test_continuation_and_fresh_turn_share_one_budget(self):
        with self.service() as service:
            self.request(service, 'first')
            self.request(service, 'continued', 'session-1')
            self.request(service, 'fresh')
        self.assertEqual(self.calls, [('first', None, 100), ('continued', 'session-1', 80), ('fresh', None, 60)])
        self.assertEqual(usage_of(service.aggregate())[1], 60)
        self.assertEqual([e['kind'] for e in ledger_entries(self.root / 'ledger')], ['start', 'finish']*3)

    def test_unobserved_continuation_and_extra_authority_refuse_before_provider(self):
        for request in ({'resume': 'invented'}, {'extra': {'model': 'fallback'}}):
            with self.subTest(request=request), self.service() as service:
                output = self.request(service, **request)
                self.assertTrue(output[-1]['is_error'])
                self.assertEqual(self.calls, [])
                self.assertIsNone(usage_of(service.aggregate())[1])
            (self.root / 'ledger').unlink()

    def test_partial_failure_retains_complete_reported_spend_and_halts(self):
        def fail(*args):
            failure = NativeAttemptError('test', [], 1)
            failure.result = result(120)
            failure.result['outcome'] = 'failed'
            raise failure
        with self.service(fail) as service:
            self.assertTrue(self.request(service)[-1]['is_error'])
        self.assertEqual(usage_of(service.aggregate())[1], 120)
        self.assertEqual(service.aggregate()['outcome'], 'failed')

    def test_unknown_usage_keeps_reservation_instead_of_becoming_zero(self):
        def fail(*args):
            raise RuntimeError('interrupted')
        with self.service(fail) as service:
            self.assertTrue(self.request(service)[-1]['is_error'])
        self.assertIsNone(usage_of(service.aggregate())[1])
        self.assertEqual(ledger_entries(self.root / 'ledger')[1]['reserved_tokens'], 100)

    def test_overshoot_and_attempt_limit_stop_additional_calls(self):
        with self.service(cap=10) as service:
            self.assertTrue(self.request(service)[-1]['is_error'])
        self.assertEqual(usage_of(service.aggregate())[1], 20)
        self.assertEqual(len(self.calls), 1)

    def test_native_resume_must_match_the_requested_session(self):
        def mismatch(prompt, resume, cap, seconds, emit):
            for event in events('unexpected' if resume else 'session-1'):
                emit(event)
            return result()
        with self.service(mismatch) as service:
            self.request(service)
            self.assertTrue(self.request(service, resume='session-1')[-1]['is_error'])
        self.assertEqual(usage_of(service.aggregate())[1], 40)
        self.assertTrue(service.failed)

    @unittest.skipUnless(os.environ.get('RINGO_SCENARIO_BINARY'), 'set RINGO_SCENARIO_BINARY for real Session integration')
    def test_real_session_resume_restart_clear_and_policy_use_the_broker(self):
        spec = {'agent': 'claude', 'seconds': 10, 'actions': [
            {'op': 'turn', 'message': 'first'}, {'op': 'turn', 'message': 'continued'},
            {'op': 'restart'}, {'op': 'turn', 'message': 'after restart'},
            {'op': 'clear'}, {'op': 'turn', 'message': 'fresh'}]}
        live.write_json(self.root / 'scenario.json', spec)
        live.write_json(self.root / 'policy.json', {'instruction': 'Check results.'})
        with self.service() as service:
            command = [os.environ['RINGO_SCENARIO_BINARY'], '--project', str(self.root),
                '--scenario', str(self.root / 'scenario.json'), '--policy', str(self.root / 'policy.json'),
                '--broker-socket', str(service.path), '--budget-tokens', '100',
                '--state', str(self.root / 'state'), '--home', str(self.root / 'home')]
            env = {k:v for k,v in os.environ.items() if k not in ('RINGO_CHILD', 'RINGO_HOME', 'CODEX_HOME')}
            env['RINGO_NO_UPDATE_CHECK'] = '1'
            completed = subprocess.run(command, capture_output=True, env=env, timeout=15)
        self.assertEqual(completed.returncode, 0, completed.stdout.decode()+completed.stderr.decode())
        record = json.loads(completed.stdout)
        self.assertFalse(record['offline'])
        self.assertEqual([call[1] for call in self.calls], [None, 'session-1', 'session-1', None])
        self.assertTrue(all(call[0].startswith('Check results.\n\n') for call in self.calls))
        self.assertEqual(sum(sum(e.get('usage', {}).get(k, 0) for k in live.USAGE_KEYS) for e in record['events']), 80)
        self.assertEqual(usage_of(service.aggregate())[1], 80)

    @unittest.skipUnless(os.environ.get('RINGO_SCENARIO_BINARY'), 'requires scenario binary')
    def test_real_session_threshold_handoff_and_compact_control(self):
        original = self.root
        for threshold, expected in ((16000, None), (128000, 'session-1')):
            self.root = original / str(threshold)
            self.root.mkdir()
            self.calls = []
            spec = {'agent': 'claude', 'seconds': 10, 'actions': [
                {'op': 'turn', 'message': 'Remember the immutable contract.'},
                {'op': 'seed', 'threads': [['claude', '$current']],
                 'records': [['claude', 32000, 'Earlier constraint: keep the immutable contract.']]},
                {'op': 'turn', 'message': 'Next message.\nKeep these bytes.'}, {'op': 'compact'}]}
            live.write_json(self.root / 'scenario.json', spec)
            live.write_json(self.root / 'policy.json', {'handoff_at': threshold, 'instruction': 'Check.',
                                                       'handoff_instruction': 'Preserve constraints.'})
            with self.service() as service:
                command = [os.environ['RINGO_SCENARIO_BINARY'], '--project', str(self.root),
                    '--scenario', str(self.root / 'scenario.json'), '--policy', str(self.root / 'policy.json'),
                    '--broker-socket', str(service.path), '--budget-tokens', '100',
                    '--state', str(self.root / 'state'), '--home', str(self.root / 'home')]
                env = {k:v for k,v in os.environ.items() if k not in ('RINGO_CHILD', 'RINGO_HOME', 'CODEX_HOME')}
                env['RINGO_NO_UPDATE_CHECK'] = '1'
                completed = subprocess.run(command, capture_output=True, env=env, timeout=15)
            self.assertEqual(completed.returncode, 0, completed.stdout.decode()+completed.stderr.decode())
            self.assertEqual(self.calls[1][1], expected)
            self.assertTrue(self.calls[1][0].endswith('Next message.\nKeep these bytes.'))
            if expected is None:
                self.assertIn('Preserve constraints.', self.calls[1][0])
                self.assertIn('keep the immutable contract', self.calls[1][0])
            self.assertEqual(self.calls[2][0], '/compact')
        self.root = original


class PreparationTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / 'binary').write_text('trusted fake binary')
        (self.root / 'check.py').write_text('raise SystemExit(0)')
        (self.root / 'fixture').mkdir()
        (self.root / 'fixture/main.py').write_text('print(1)')
        live.write_json(self.root / 'scenario.json', {'agent': 'claude', 'actions': [{'op': 'turn', 'message': 'go'}]})
        plan = demonstration_plan()
        plan['seconds'] = 120
        self.config = {'version': 1, 'plan': plan, 'image': 'sha256:'+'a'*64, 'worker_image': 'sha256:'+'b'*64,
                       'model': 'claude-test-pinned', 'binary': str(self.root / 'binary'),
                       'tasks': {task: {'fixture': str(self.root / 'fixture'), 'positive': str(self.root / 'fixture'),
                          'negative': str(self.root / 'fixture'), 'scenario': str(self.root / 'scenario.json'),
                          'check': str(self.root / 'check.py')} for task in plan['clusters']}}
        self.work = self.root / 'prepared'

    def test_prepare_sizes_without_inference_and_refuses_changed_inputs(self):
        report = live.prepare(self.config, self.work)
        self.assertFalse(report['spending_authorized'])
        self.assertEqual(report['budget']['maximum_attempts'], 76)
        live.frozen(self.work)
        (self.root / 'fixture/main.py').write_text('changed')
        with self.assertRaisesRegex(ValueError, 'changed'):
            live.frozen(self.work)

    def test_invalid_split_cap_and_fictional_session_are_refused(self):
        for change in ('split', 'cap', 'session'):
            config = copy.deepcopy(self.config)
            if change == 'split':
                config['tasks']['extra'] = next(iter(config['tasks'].values()))
            elif change == 'cap':
                config['plan']['budgets']['controls'] = 1
            else:
                live.write_json(self.root / 'fictional.json', {'agent': 'claude', 'actions': [
                    {'op': 'seed', 'threads': [['claude', 'fabricated']], 'records': []},
                    {'op': 'turn', 'message': 'go'}]})
                next(iter(config['tasks'].values()))['scenario'] = str(self.root / 'fictional.json')
            with self.subTest(change=change), self.assertRaises(ValueError):
                live.validate_config(config)

    def test_phase_authorization_and_prerequisites_are_fail_closed(self):
        live.prepare(self.config, self.work)
        with patch.dict(os.environ, {'RINGO_CHILD': '1'}):
            report = live.run(self.work, '/missing/token', {}, 'controls')
        self.assertFalse(report['ready'])
        self.assertEqual(report['provider_calls'], 0)
        # fail-closed on every platform: Windows adds its own Unix-transport blocker to these
        self.assertEqual(len(report['blockers']), 5 + (os.name != 'posix'))
        self.assertFalse((self.work / 'campaign').exists())

    def test_an_offline_campaign_cannot_supply_live_control_evidence(self):
        live.prepare(self.config, self.work)
        _, manifest, config = live.frozen(self.work)
        live.Campaign.create(self.work / 'campaign', config['plan'])
        ready = {'ready': True, 'manifest_sha256': digest(manifest)}
        with patch.object(live, 'prerequisites', return_value=ready):
            with self.assertRaisesRegex(ValueError, 'live manifest binding'):
                live.run(self.work, '/unused/token', {}, 'search')

    @unittest.skipUnless(os.name == 'posix', 'a ready preflight requires the Unix native transport')
    def test_preflight_accepts_only_exact_bound_phase_approval(self):
        live.prepare(self.config, self.work)
        _, manifest, config = live.frozen(self.work)
        pin = digest(manifest)
        live.write_json(self.work / 'grading.json', {'manifest_sha256': pin, 'checks_discriminate': True})
        live.write_json(self.work / 'panel-review.json', {'manifest_sha256': pin, 'reviewer': 'test reviewer',
                         'fresh_sealed': True, 'family_disjoint': True, 'policy_coverage': True})
        live.write_json(self.work / 'process-proof.json', {'process_profile_certified': True,
                         'image': config['image'], 'worker_image': config['worker_image'], 'model': config['model'],
                         'sources': {name: manifest['sources'][name] for name in ('turnrun_native.py',
                             'turnrun_native_bridge.py', 'turnrun_broker.py', 'turnrun_isolation.py')}})
        authorization = {'manifest_sha256': pin, 'owner_approved': True, 'owner_statement': 'test only',
                         'phase_budgets': {'controls': config['plan']['budgets']['controls']}}
        with patch.dict(os.environ, {}, clear=True):
            self.assertTrue(live.prerequisites(self.work, authorization, 'controls')['ready'])
            self.assertFalse(live.prerequisites(self.work, authorization, 'search')['ready'])
            authorization['phase_budgets']['controls'] += 1
            self.assertFalse(live.prerequisites(self.work, authorization, 'controls')['ready'])

    def test_proposal_context_excludes_sealed_results_paths_and_provider_replies(self):
        history = [{'kind': 'finish', 'metadata': {'split': 'dev', 'task': 'D'}, 'tokens': 10,
                    'result': {'passed': True, 'reply': 'private-marker', 'path': '/private/check'}},
                   {'kind': 'finish', 'metadata': {'split': 'holdout', 'task': 'H'}, 'result': {'passed': False}},
                   {'kind': 'prepared', 'plan': 'sealed-marker'},
                   {'kind': 'decision', 'round': 1, 'incumbent': {}, 'hidden': 'private-marker'}]
        projected = live.projected_history(history)
        self.assertEqual(len(projected), 2)
        self.assertNotIn('private', json.dumps(projected))
        self.assertNotIn('sealed', json.dumps(projected))
        self.assertNotIn('holdout', json.dumps(projected))

    def test_native_persistence_and_compact_flags_are_explicit(self):
        once = arguments('claude-test', {}, '', 4)
        self.assertIn('--no-session-persistence', once)
        persistent = arguments('claude-test', {}, '', 4, persistent=True, resume='observed', compact=True)
        self.assertNotIn('--no-session-persistence', persistent)
        self.assertNotIn('--disable-slash-commands', persistent)
        self.assertEqual(persistent[-2:], ['--resume', 'observed'])

    @unittest.skipUnless(os.name == 'posix' and all(os.environ.get(k) for k in
        ('RINGO_SCENARIO_BINARY', 'RINGO_NATIVE_IMAGE', 'RINGO_TEST_IMAGE')),
        'requires native replay images and scenario binary')
    def test_contained_real_native_cli_and_session_resume_then_grade(self):
        from turnrun_native import NativeBroker
        config = copy.deepcopy(self.config)
        config.update(binary=os.environ['RINGO_SCENARIO_BINARY'], image=os.environ['RINGO_NATIVE_IMAGE'],
                      worker_image=os.environ['RINGO_TEST_IMAGE'], model='claude-sonnet-4-6')
        config['plan']['seconds'] = 120
        live.write_json(self.root / 'replay.json', {'agent': 'claude', 'seconds': 120, 'actions': [
            {'op': 'turn', 'message': 'Write the native transport proof using the worker.'},
            {'op': 'restart'}, {'op': 'turn', 'message': 'Continue.'},
            {'op': 'clear'}, {'op': 'turn', 'message': 'Write the native transport proof using the worker.'}]})
        for row in config['tasks'].values():
            row['scenario'] = str(self.root / 'replay.json')
        # The grader sees the trusted Session record only after all task processes stop.
        (self.root / 'check.py').write_text('''import json, pathlib, sys
p = pathlib.Path(sys.argv[1])
record = json.loads((p / '.ringo-evidence/session.json').read_text())
assert record['transport'] == 'isolated-native-broker'
assert [e['op'] for e in record['events']] == ['turn','restart','turn','clear','turn']
assert record['events'][0]['after'] == record['events'][2]['after']
assert record['events'][0]['after'] != record['events'][4]['after']
assert (p / 'native-transport.txt').read_text() == 'native-transport-proof'
''')
        live.prepare(config, self.work)
        def replay(broker, **kwargs):
            return broker.replay(prompt=kwargs['prompt'], endpoint=kwargs['endpoint'], token_cap=kwargs['token_cap'],
                seconds=kwargs['seconds'], resume=kwargs['resume'], on_event=kwargs['on_event'], error_file=kwargs['error_file'])
        adapter = live.LiveAdapter(self.work, '/unused/no-real-token')
        with patch.object(NativeBroker, 'run', replay):
            observation = adapter.evaluate({'instruction': 'Verify 世界.'}, next(iter(config['tasks'])), 1, 1000, 120)
        self.assertEqual(observation['outcome'], 'done', observation)
        self.assertTrue(observation['passed'])
        self.assertTrue(observation['protected_pass'])
        self.assertEqual(observation['native_attempts'], 3)
        self.assertIsNotNone(usage_of(observation)[1])


if __name__ == '__main__':
    unittest.main()
