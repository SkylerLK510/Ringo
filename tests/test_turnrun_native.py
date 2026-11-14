"""Native adapter boundary and accounting tests. No provider inference."""
import copy
import http.client
import json
import os
from pathlib import Path
import socket
import tempfile
import unittest
from unittest.mock import patch

from turnrun_native import NativeBroker, certify, native_result, oauth_token, provider_socket, retained_usage
from turnrun_native_bridge import Bridge, arguments, environment
from turnrun_broker import WorkerEndpoint
from turnrun_isolation import DockerSandbox


MODEL = 'claude-test-pinned'


def events():
    return [{'type': 'system', 'subtype': 'init', 'model': MODEL,
             'tools': ['mcp__trial__worker'], 'mcp_servers': [{'name': 'trial', 'status': 'connected'}]},
            {'type': 'assistant', 'message': {'usage': {'input_tokens': 999999}}},
            {'type': 'result', 'subtype': 'success', 'is_error': False, 'result': 'done', 'num_turns': 2,
             'modelUsage': {MODEL: {}},
             'usage': {'input_tokens': 10, 'cache_creation_input_tokens': 20,
                       'cache_read_input_tokens': 30, 'output_tokens': 40}}]


class NativeProfileTest(unittest.TestCase):
    def test_interrupted_usage_retains_deduplicated_message_lower_bound(self):
        partial = [{'type': 'assistant', 'message': {'id': 'one', 'usage': {
            'input_tokens': 200, 'cache_read_input_tokens': 500, 'output_tokens': 10}}},
            {'type': 'assistant', 'message': {'id': 'one', 'usage': {'output_tokens': 15}}},
            {'type': 'assistant', 'message': {'id': 'two', 'usage': {'input_tokens': 300}}}]
        usage = retained_usage(partial)
        self.assertFalse(usage['counted'])
        self.assertEqual(usage['input'], 500)
        self.assertEqual(usage['cache_read'], 500)
        self.assertEqual(usage['output'], 15)
        self.assertEqual(retained_usage(events())['input'], 10)
        self.assertTrue(retained_usage(events())['counted'])

    def test_aggregate_usage_counts_cache_without_double_counting_messages(self):
        result = native_result(events(), 0, MODEL)
        self.assertEqual(result['outcome'], 'done')
        self.assertEqual(sum(result['usage'][k] for k in ('input', 'cache_write', 'cache_read', 'output')), 100)
        self.assertEqual(result['model_turns'], 2)
        self.assertIsNone(result['provider_requests'])
        self.assertEqual(native_result(events(), 1, MODEL)['outcome'], 'failed')

    def test_missing_usage_tools_model_connection_or_final_refuses(self):
        mutations = [lambda e: e.pop(), lambda e: e.append(copy.deepcopy(e[-1])),
                     lambda e: e[0].update(tools=['Bash', 'mcp__trial__worker']),
                     lambda e: e[0].update(model='fallback'), lambda e: e[0].update(mcp_servers=[]),
                     lambda e: e[-1]['modelUsage'].update({'fallback': {}}),
                     lambda e: e[-1]['usage'].pop('cache_read_input_tokens'),
                     lambda e: e[-1]['usage'].update(output_tokens=True)]
        for mutate in mutations:
            sample = events()
            mutate(sample)
            with self.subTest(sample=sample), self.assertRaises(ValueError):
                native_result(sample, 0, MODEL)

    def test_environment_has_no_inherited_keys_routing_or_owner_configuration(self):
        with patch.dict(os.environ, {'ANTHROPIC_API_KEY': 'private', 'HOME': '/owner',
                                     'ANTHROPIC_BASE_URL': 'https://evil.test', 'AWS_SECRET_ACCESS_KEY': 'private'}):
            env = environment('subscription-placeholder', 'http://127.0.0.1:9000')
        self.assertNotIn('ANTHROPIC_API_KEY', env)
        self.assertNotIn('AWS_SECRET_ACCESS_KEY', env)
        self.assertNotIn('ANTHROPIC_BASE_URL', env)
        self.assertNotIn('/owner', json.dumps(env))
        self.assertEqual(env['CLAUDE_CODE_MAX_RETRIES'], '0')
        args = arguments(MODEL, {'mcpServers': {}}, 'test', 2)
        self.assertEqual(args[args.index('--tools')+1], '')
        self.assertEqual(args[args.index('--setting-sources')+1], '')
        self.assertIn('--strict-mcp-config', args)
        self.assertNotIn('--bare', args)
        self.assertNotIn('--safe-mode', args)
        self.assertNotIn('--fallback-model', args)

    def test_live_child_guard_precedes_any_token_read_or_launch(self):
        broker = object.__new__(NativeBroker)
        with patch.dict(os.environ, {'RINGO_CHILD': '1'}), patch('turnrun_native.oauth_token') as token:
            with self.assertRaisesRegex(ValueError, 'standalone'):
                broker.run(prompt='x', instruction='', model=MODEL, token_file='/missing', endpoint=None, token_cap=100)
            token.assert_not_called()
        with patch.dict(os.environ, {'RINGO_CHILD': '1'}), patch('turnrun_native.oauth_token') as token:
            with self.assertRaisesRegex(ValueError, 'outside'):
                certify(image='x', worker_image='x', model=MODEL, token_file='/missing',
                        budget_tokens=100, seconds=30, work='/missing')
            token.assert_not_called()

    @unittest.skipIf(os.name == 'nt', 'native OAuth file isolation requires POSIX permissions')
    def test_token_file_requires_subscription_token_and_private_mode(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'token'
            path.write_text('sk-ant-api-placeholder')
            path.chmod(0o600)
            with self.assertRaisesRegex(ValueError, 'OAuth'):
                oauth_token(path)
            path.write_text('sk-ant-oat01-placeholder\n')
            self.assertEqual(oauth_token(path), 'sk-ant-oat01-placeholder')
            path.chmod(0o644)
            with self.assertRaisesRegex(ValueError, 'private'):
                oauth_token(path)

    def test_network_allowlist_rejects_other_hosts_and_private_dns_before_connect(self):
        with patch('turnrun_native.socket.socket') as sock:
            for host in ('localhost:443', 'api.anthropic.com:80', 'evil.test:443', 'api.anthropic.com:443@evil'):
                with self.assertRaises(ValueError):
                    provider_socket(host)
            with patch('turnrun_native.socket.getaddrinfo', return_value=[
                    (socket.AF_INET, socket.SOCK_STREAM, 6, '', ('127.0.0.1', 443))]):
                with self.assertRaisesRegex(ValueError, 'non-public'):
                    provider_socket('api.anthropic.com:443')
            sock.assert_not_called()


class BridgeBoundaryTest(unittest.TestCase):
    def test_proxy_refuses_unapproved_authorities_without_relay(self):
        bridge = Bridge()
        with patch.object(bridge, 'emit') as emit:
            server = bridge.server(proxy=True)
            try:
                conn = http.client.HTTPConnection('127.0.0.1', server.server_port, timeout=3)
                conn.request('CONNECT', 'evil.test:443')
                self.assertEqual(conn.getresponse().status, 403)
                conn.close()
                emit.assert_not_called()
            finally:
                server.shutdown()
                server.server_close()

    def test_mcp_http_relay_carries_only_json_rpc_response(self):
        bridge = Bridge()
        messages = []

        def emit(**frame):
            messages.append(frame)
            bridge.pending[frame['id']].put({'type': 'mcp', 'id': frame['id'],
                                           'message': {'jsonrpc': '2.0', 'id': 1, 'result': {'tools': []}}})

        with patch.object(bridge, 'emit', side_effect=emit):
            server = bridge.server()
            try:
                conn = http.client.HTTPConnection('127.0.0.1', server.server_port, timeout=3)
                conn.request('POST', '/mcp', json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': 'tools/list'}),
                             {'Content-Type': 'application/json'})
                response = conn.getresponse()
                self.assertEqual(response.status, 200)
                self.assertEqual(json.loads(response.read())['result'], {'tools': []})
                self.assertEqual(messages[0]['type'], 'mcp')
                self.assertEqual(bridge.pending, {})
                conn.close()
            finally:
                server.shutdown()
                server.server_close()

    def test_channel_limit_bounds_concurrent_and_total_admission(self):
        bridge = Bridge()
        for _ in range(8):
            bridge.channel()
        with self.assertRaisesRegex(ValueError, 'limit'):
            bridge.channel()


@unittest.skipUnless(os.environ.get('RINGO_NATIVE_IMAGE'), 'requires the pinned native broker image')
class NativeContainerTest(unittest.TestCase):
    def test_real_native_binary_starts_without_inference_in_isolated_container(self):
        with NativeBroker(os.environ['RINGO_NATIVE_IMAGE'], lifetime=60) as broker:
            result = broker.probe()
            self.assertTrue(result['direct_network_denied'])
            self.assertIn('Claude Code', result['version'])
            self.assertEqual(result['provider_calls'], 0)
            self.assertFalse(result['live_certified'])

    def test_real_broker_container_calls_only_assigned_worker_without_inference(self):
        with tempfile.TemporaryDirectory() as folder:
            with DockerSandbox(os.environ.get('RINGO_TEST_IMAGE', os.environ['RINGO_NATIVE_IMAGE']), lifetime=90) as worker:
                with WorkerEndpoint(worker, lifetime=60) as endpoint:
                    with NativeBroker(os.environ['RINGO_NATIVE_IMAGE'], lifetime=60) as broker:
                        result = broker.exercise_transport(endpoint)
                        self.assertEqual([t['name'] for t in result['tools']['result']['tools']], ['worker'])
                        self.assertEqual(result['provider_calls'], 0)
                        self.assertTrue(result['synthetic'])
                        self.assertEqual(json.loads(result['result']['result']['content'][0]['text'])['exit'], 0)
                        self.assertEqual(endpoint.calls, 1)
                        self.assertNotEqual(worker.id, broker.id)
                submission = worker.submission(Path(folder)/'submission')
                self.assertEqual((submission/'native-transport.txt').read_text(), 'native-transport-proof')

    def test_actual_native_cli_discovers_and_executes_worker_with_offline_protocol_replay(self):
        with tempfile.TemporaryDirectory() as folder:
            with DockerSandbox(os.environ.get('RINGO_TEST_IMAGE', os.environ['RINGO_NATIVE_IMAGE']), lifetime=120) as worker:
                with WorkerEndpoint(worker, lifetime=100) as endpoint:
                    with NativeBroker(os.environ['RINGO_NATIVE_IMAGE'], lifetime=90) as broker:
                        result = broker.exercise_transport(endpoint, native=True)
                        self.assertEqual(result['provider_calls'], 0)
                        self.assertTrue(result['synthetic'])
                        self.assertEqual(endpoint.calls, 1)
                        self.assertEqual(result['result']['outcome'], 'done')
                submission = worker.submission(Path(folder)/'submission')
                self.assertEqual((submission/'native-transport.txt').read_text(), 'native-transport-proof')


if __name__ == '__main__':
    unittest.main()
