"""Codex profile accounting, authority and real offline container/Session gates."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch, MagicMock

from turnrun import clean_env, usage_of
from turnrun_broker import WorkerEndpoint
from turnrun_codex import CodexBroker, MODEL, provider_socket, retained_usage, subscription_auth
from turnrun_codex_bridge import arguments, environment, fake_provider, CodexBridge
from turnrun_isolation import DockerSandbox
from turnrun_live import SessionTransport, scenario, write_json


def events(sid='thread-1', inputs=30, cached=10, output=10):
    return [{'id': 2, 'result': {'thread': {'id': sid}, 'model': MODEL, 'modelProvider': 'openai', 'reasoningEffort': 'low'}},
        {'id': 3, 'result': {'turn': {'id': 'turn-current'}}},
        {'method': 'item/completed', 'params': {'threadId': sid, 'turnId': 'turn-current', 'item': {'type': 'agentMessage', 'text': 'done'}}},
        {'method': 'thread/tokenUsage/updated', 'params': {'threadId': sid, 'turnId': 'turn-current', 'tokenUsage': {'total': {
            'inputTokens': inputs, 'cachedInputTokens': cached, 'outputTokens': output}}}},
        {'method': 'turn/completed', 'params': {'threadId': sid, 'turn': {'id': 'turn-current', 'status': 'completed'}}}]


class CodexProfileTests(unittest.TestCase):
    def broker(self):
        # Pure profile/accounting tests never launch a container. Keep them
        # runnable on macOS/Windows CI hosts without Docker installed.
        return CodexBroker('sha256:'+'a'*64, docker='unit-test-never-executed')

    @unittest.skipIf(os.name == 'nt', 'POSIX credential permissions')
    def test_only_subscription_access_and_account_are_forwarded(self):
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp)/'auth.json'
            value = {'auth_mode': 'chatgpt', 'tokens': {'access_token': 'access', 'account_id': 'account',
                'refresh_token': 'never-forward-this', 'id_token': 'never-forward-that'}, 'OPENAI_API_KEY': None}
            path.write_text(json.dumps(value)); path.chmod(0o600)
            self.assertEqual(subscription_auth(path), {'access_token': 'access', 'account_id': 'account'})
            path.chmod(0o644)
            with self.assertRaises(ValueError): subscription_auth(path)
            path.chmod(0o600)
            for change in ({'auth_mode': 'apikey'}, {'OPENAI_API_KEY': 'billing-key'}):
                path.write_text(json.dumps({**value, **change}))
                with self.assertRaises(ValueError): subscription_auth(path)
            link = Path(temp)/'link'
            link.symlink_to(path)
            with self.assertRaises(ValueError): subscription_auth(link)

    def test_complete_turn_and_resume_difference_usage_once(self):
        broker = self.broker()
        first = broker.parse_result(events(), 0, MODEL)
        self.assertEqual(usage_of(first)[1], 40)
        broker._remember(first, events())
        later = events(inputs=60, cached=25, output=15)
        resumed = broker.parse_result(later, 0, MODEL)
        self.assertEqual(resumed['usage'], {'input': 15, 'cache_read': 15, 'cache_write': 0,
                                          'output': 5, 'counted': True, 'uncounted_calls': 0})
        self.assertEqual(usage_of(broker.parse_result(events('fresh'), 0, MODEL))[1], 40)

    def test_failure_keeps_observed_spend_and_missing_or_bad_usage_remains_uncounted(self):
        broker = self.broker()
        failed = events(); failed[-1]['params']['turn']['status'] = 'failed'
        result = broker.parse_result(failed, 1, MODEL)
        self.assertEqual(result['outcome'], 'failed')
        self.assertEqual(usage_of(result)[1], 40)
        for bad in (events()[:2], events(inputs=2, cached=3), events(inputs=True)):
            self.assertIsNone(usage_of({'usage': retained_usage(bad)})[1])
        interrupted = broker.attempt_error('interrupted', events()[:-1])
        self.assertIsNone(usage_of(interrupted.result)[1])
        self.assertEqual(sum(interrupted.result['usage'][k] for k in ('input','cache_read','cache_write','output')),40)
        decreased = retained_usage(events(inputs=1, cached=0), dict(input=20,cache_read=10,cache_write=0,output=10))
        self.assertFalse(decreased['counted'])

    def test_stale_turn_usage_and_completion_cannot_complete_current_turn(self):
        for field in ('usage', 'completion', 'reply'):
            rows = events()
            if field == 'usage':
                rows[-2]['params']['turnId'] = 'previous-turn'
            elif field == 'completion':
                rows[-1]['params']['turn']['id'] = 'previous-turn'
            else:
                rows[2]['params']['turnId'] = 'previous-turn'
            self.assertEqual(self.broker().parse_result(rows, 0, MODEL)['outcome'], 'failed')

    def test_usage_before_final_reply_is_only_a_lower_bound(self):
        rows = events()
        rows[2], rows[3] = rows[3], rows[2]
        result = self.broker().parse_result(rows, 0, MODEL)
        self.assertEqual(result['outcome'], 'failed')
        self.assertIsNone(usage_of(result)[1])
        self.assertEqual(result['usage']['input'] + result['usage']['cache_read'] + result['usage']['output'], 40)

    def test_public_replay_remembers_cumulative_baseline(self):
        broker = self.broker()
        def replay(parent, **kwargs):
            rows = events(inputs=60, cached=25, output=15) if kwargs.get('resume') else events()
            for row in rows:
                kwargs['on_event'](row)
            return broker.parse_result(rows, 0, MODEL)
        with patch('turnrun_native.NativeBroker.replay', replay):
            first = broker.replay(prompt='x', endpoint=None, token_cap=100, seconds=1)
            second = broker.replay(prompt='y', endpoint=None, token_cap=100, seconds=1, resume='thread-1')
        self.assertEqual(usage_of(first)[1], 40)
        self.assertEqual(usage_of(second)[1], 35)

    def test_pinned_model_tools_network_and_environment(self):
        broker = self.broker()
        with self.assertRaises(ValueError): broker.validate_model('gpt-6-astra')
        bad = events()[0]; bad['result']['model'] = 'fallback'
        with self.assertRaises(ValueError): broker.validate_event(bad, MODEL)
        for kind in ('commandExecution', 'fileChange', 'webSearch', 'collabAgentToolCall', 'mcpToolCall'):
            with self.assertRaises(ValueError): broker.validate_event({'params': {'item': {'type': kind}}}, MODEL)
        with patch('turnrun_codex.socket.socket') as sock:
            for authority in ('api.openai.com:443', 'api.anthropic.com:443', 'localhost:443'):
                with self.assertRaises(ValueError): provider_socket(authority)
            sock.assert_not_called()
        env = environment('http://127.0.0.1:1')
        self.assertNotIn('OPENAI_API_KEY',env)
        self.assertNotIn('OPENAI_BASE_URL',env)
        self.assertEqual(env['RINGO_CHILD'],'1')
        argv = arguments()
        self.assertIn('forced_login_method="chatgpt"', argv)
        self.assertIn('model_reasoning_effort="low"',argv)
        self.assertIn('features.multi_agent=false',argv)

    def test_bridge_accepts_completion_on_last_allowed_usage_update(self):
        import io
        rows = [{'id': 1, 'result': {}}, {'id': 10, 'result': {}}, *events()]
        proc = MagicMock()
        proc.__enter__.return_value = proc
        proc.stdout = io.BytesIO(b''.join(json.dumps(row).encode() + b'\n' for row in rows))
        proc.stdin = io.BytesIO()
        bridge = CodexBridge()
        bridge.server = MagicMock(return_value=MagicMock(server_port=1))
        bridge.rpc = MagicMock(return_value={'result': {'tools': [{'name': 'worker', 'inputSchema': {}}]}})
        bridge.emit = MagicMock()
        with patch('turnrun_codex_bridge.subprocess.Popen', return_value=proc), patch.object(Path, 'mkdir'):
            bridge.run({'model': MODEL, 'prompt': 'x', 'instruction': '', 'resume': None,
                        'oauth_token': {'access_token': 'test', 'account_id': 'test'}, 'max_turns': 1})
        self.assertIn(unittest.mock.call(type='exit', code=0), bridge.emit.call_args_list)

    def test_replay_rejects_worker_schema_drift(self):
        import http.client
        schema = {'type': 'object', 'properties': {'command': {'type': 'string', 'maxLength': 8},
                  'seconds': {'type': 'integer', 'minimum': 1, 'maximum': 300}},
                  'required': ['command', 'seconds'], 'additionalProperties': False}
        # Codex 0.169.0 strips validation bounds but must keep the structure.
        stripped = {'type': 'object', 'properties': {'command': {'type': 'string'}, 'seconds': {'type': 'integer'}},
                    'required': ['command', 'seconds'], 'additionalProperties': False}
        retyped = json.loads(json.dumps(stripped)); retyped['properties']['seconds']['type'] = 'string'
        optional = {**stripped, 'required': ['command']}
        open_ended = {k: v for k, v in stripped.items() if k != 'additionalProperties'}
        for actual, status in ((stripped, 200), ({'type': 'object'}, 400), (retyped, 400),
                               (optional, 400), (open_ended, 400)):
            server = fake_provider()
            self.addCleanup(server.server_close)
            self.addCleanup(server.shutdown)
            server.worker_schema = schema
            client = http.client.HTTPConnection('127.0.0.1', server.server_port, timeout=5)
            self.addCleanup(client.close)
            client.request('POST', '/v1/responses', json.dumps({'model': MODEL,
                'reasoning': {'effort': 'low'}, 'tools': [{'type': 'function', 'name': 'worker', 'parameters': actual}]}),
                {'Content-Type': 'application/json'})
            response = client.getresponse()
            self.assertEqual(response.status, status)
            response.read()
            self.assertEqual(server.requests, 1)
            self.assertEqual(server.rejected, status == 400)

    def test_child_guard_precedes_credentials_and_container_probe(self):
        broker=self.broker()
        with patch.dict(os.environ, {'RINGO_CHILD':'1'}), patch.object(broker,'credential') as auth:
            with self.assertRaisesRegex(ValueError,'RINGO_CHILD'):
                broker.run(prompt='x',instruction='',model=MODEL,token_file='/missing',endpoint=None,token_cap=50)
            auth.assert_not_called()

    def test_codex_scenarios_reject_claude_only_operations_and_wrong_seed_agents(self):
        scenario({'agent':'codex','actions':[{'op':'turn','message':'hello'}]})
        for action in ({'op':'compact'}, {'op':'seed','threads':[['claude','$current']],'records':[]}):
            with self.assertRaises(ValueError):
                scenario({'agent':'codex','actions':[action,{'op':'turn','message':'x'}]})

    def test_codex_manifest_cannot_use_a_claude_process_proof(self):
        import turnrun_live as live
        from turnrun_campaign import demonstration_plan
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp)
            (root/'binary').write_text('offline fake binary')
            (root/'check.py').write_text('raise SystemExit(0)')
            (root/'fixture').mkdir()
            write_json(root/'scenario.json',{'agent':'codex','seconds':20,'actions':[{'op':'turn','message':'go'}]})
            plan=demonstration_plan()
            config={'version':2,'profile':'codex-luna-low','model':MODEL,'plan':plan,
                'image':'sha256:'+'a'*64,'worker_image':'sha256:'+'b'*64,'binary':str(root/'binary'),
                'tasks':{task:{'fixture':str(root/'fixture'),'positive':str(root/'fixture'),
                    'negative':str(root/'fixture'),'scenario':str(root/'scenario.json'),
                    'check':str(root/'check.py')} for task in plan['clusters']}}
            prepared=live.prepare(config,root/'prepared')
            manifest=live.read_json(root/'prepared/manifest.json')
            self.assertIn('turnrun_codex_bridge.py',manifest['sources'])
            write_json(root/'prepared/process-proof.json',{'process_profile_certified':True,
                'model':'claude-sonnet-5-5','sources':{},'image':config['image'],'worker_image':config['worker_image']})
            auth={'manifest_sha256':prepared['manifest_sha256'],'owner_approved':True,
                  'owner_statement':'offline test','phase_budgets':{'controls':plan['budgets']['controls']}}
            result=live.prerequisites(root/'prepared',auth,'controls')
            self.assertIn('a successful process proof pinned to the current profile is required',result['blockers'])
            for change in ({'model':'gpt-6-astra'},{'profile':'unreviewed-profile'}):
                with self.assertRaises(ValueError): live.validate_config({**config,**change})


@unittest.skipUnless(os.environ.get('RINGO_CODEX_IMAGE') and os.environ.get('RINGO_TEST_IMAGE'),
                     'requires pinned Codex and worker images; no inference')
class CodexNativeReplayTests(unittest.TestCase):
    def test_native_codex_discovers_worker_and_continues_with_exact_accounting(self):
        with DockerSandbox(os.environ['RINGO_TEST_IMAGE'],lifetime=180) as worker:
            with WorkerEndpoint(worker,lifetime=150) as endpoint:
                with CodexBroker(os.environ['RINGO_CODEX_IMAGE'],lifetime=160) as broker:
                    self.assertEqual(broker.probe()['provider_calls'],0)
                    captured=[]
                    first=broker.exercise_session(endpoint,{'prompt':'Write the transport proof.','resume':None},on_event=captured.append)
                    sid=captured[0]['result']['thread']['id']
                    self.assertEqual(first['outcome'],'done')
                    self.assertTrue(first['synthetic'])
                    endpoint.reconnect()
                    resumed=broker.exercise_session(endpoint,{'prompt':'Do it again.','resume':sid})
                    self.assertEqual(resumed['outcome'],'done')
                    self.assertEqual(usage_of(resumed)[1],usage_of(first)[1])
                    self.assertEqual(endpoint.calls,2)

    @unittest.skipUnless(os.environ.get('RINGO_SCENARIO_BINARY'),'requires real scenario binary')
    def test_actual_codex_session_restart_handoff_and_followup_resume(self):
        with tempfile.TemporaryDirectory(prefix='rrsi-codex-',dir='/tmp') as tmp:
            root=Path(tmp)
            actions=[{'op':'turn','message':'Remember the earlier contract.'},{'op':'restart'}, {'op':'start_over'},
                     {'op':'turn','message':'Implement it now.'},{'op':'turn','message':'Verify it.'}]
            write_json(root/'scenario.json',{'agent':'codex','seconds':90,'actions':actions})
            write_json(root/'policy.json',{'handoff_instruction':'Recover the earlier contract.'})
            captured=[]
            with DockerSandbox(os.environ['RINGO_TEST_IMAGE'],lifetime=180) as worker:
                with WorkerEndpoint(worker,lifetime=150) as endpoint:
                    with CodexBroker(os.environ['RINGO_CODEX_IMAGE'],lifetime=160) as broker:
                        def invoke(prompt,resume,remaining,left,emit):
                            captured.append((prompt,resume))
                            endpoint.reconnect()
                            return broker.exercise_session(endpoint,{'prompt':prompt,'resume':resume},
                                token_cap=remaining,seconds=left,on_event=emit)
                        with SessionTransport(root/'broker.sock',invoke,cap=100000,seconds=90,attempts=3,
                                              ledger=root/'ledger',agent='codex') as transport:
                            run=subprocess.run([os.environ['RINGO_SCENARIO_BINARY'],'--project',str(root),
                                '--scenario',str(root/'scenario.json'),'--policy',str(root/'policy.json'),
                                '--broker-socket',str(transport.path),'--budget-tokens','100000',
                                '--state',str(root/'state'),'--home',str(root/'home')],
                                capture_output=True,env=clean_env({'RINGO_NO_UPDATE_CHECK':'1'}),timeout=100)
                        record=json.loads(run.stdout)
                        self.assertEqual(run.returncode,0,record)
                        self.assertEqual(len(captured),3)
                        self.assertIsNone(captured[0][1]);self.assertIsNone(captured[1][1])
                        self.assertTrue(captured[1][0].startswith('Recover the earlier contract.\n\nRingo\'s record'))
                        self.assertIn(actions[0]['message'],captured[1][0])
                        self.assertIsNotNone(captured[2][1])
                        total=sum(sum(e.get('usage',{}).get(k,0) for k in ('input','cache_write','cache_read','output'))
                                  for e in record['events'])
                        self.assertEqual(total,usage_of(transport.aggregate())[1])
