"""Native adapter boundary and accounting tests. No provider inference."""
import copy
import http.client
import io
import json
import os
from pathlib import Path
import socket
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from turnrun_native import (NativeAttemptError, NativeBroker, NativeProfileError, certify, main,
                            native_diagnostics, native_result, oauth_token, provider_socket, retained_usage,
                            save_native_error)
from turnrun_native_bridge import Bridge, arguments, environment
from turnrun_broker import WorkerEndpoint
from turnrun_isolation import DockerSandbox
from turnrun import usage_of


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
    def test_rejected_model_keeps_actionable_diagnostics_and_zero_usage_without_response_text(self):
        sample = events()
        sample[-1].update(is_error=True, api_error_status=404, modelUsage={},
                          result="There's an issue with the selected model. private-response-marker",
                          usage={'input_tokens': 0, 'cache_creation_input_tokens': 0,
                                 'cache_read_input_tokens': 0, 'output_tokens': 0})
        with self.assertRaises(NativeProfileError) as caught:
            native_result(sample, 1, MODEL)
        failure = NativeAttemptError(str(caught.exception), sample, 1)
        self.assertEqual(failure.result['usage']['input'], 0)
        self.assertTrue(failure.result['usage']['counted'])
        details = failure.result['diagnostics']
        self.assertEqual(details['provider_error_category'], 'model_unavailable')
        self.assertEqual(details['api_error_status'], 404)
        self.assertEqual(details['native_exit_code'], 1)
        self.assertEqual(details['reason'], 'native provider failed before reporting model usage')
        self.assertNotIn('private-response-marker', json.dumps(failure.result))

    def test_failure_categories_are_bounded_and_never_save_credentials_or_raw_messages(self):
        for status, category in ((400, 'invalid_request'), (401, 'authentication_failed'), (403, 'permission_denied'),
                                 (404, 'resource_unavailable'), (429, 'rate_limited'),
                                 (500, 'unclassified_provider_error')):
            sample = events()
            sample[-1].update(is_error=True, api_error_status=status,
                              result='sk-ant-oat01-private-marker', errors=['private-error-marker'])
            details = native_diagnostics(sample, 'fixed controller reason', 1)
            with self.subTest(status=status):
                self.assertEqual(details['provider_error_category'], category)
                self.assertNotIn('private-marker', json.dumps(details))
                self.assertNotIn('private-error-marker', json.dumps(details))
        sample[-1].update(api_error_status='private-status-marker', subtype='private-subtype-marker')
        details = native_diagnostics(sample, 'fixed controller reason')
        self.assertIsNone(details['api_error_status'])
        self.assertEqual(details['final_subtype'], 'unknown')
        self.assertNotIn('private-', json.dumps(details))

    def test_native_400_without_model_usage_is_a_provider_failure_and_keeps_zero_usage(self):
        sample = events()
        sample[-1].update(is_error=True, api_error_status=400, modelUsage={},
                          result='API Error: 400 private-rejection-marker',
                          usage={field: 0 for field in ('input_tokens', 'cache_creation_input_tokens',
                                                       'cache_read_input_tokens', 'output_tokens')})
        with self.assertRaisesRegex(NativeProfileError, 'before reporting model usage') as caught:
            native_result(sample, 1, MODEL)
        failure = NativeAttemptError(str(caught.exception), sample, 1)
        self.assertEqual(failure.result['diagnostics']['provider_error_category'], 'invalid_request')
        self.assertTrue(failure.result['usage']['counted'])
        self.assertEqual(failure.result['usage']['input'], 0)
        self.assertNotIn('private-rejection-marker', json.dumps(failure.result))
        sample[-1]['is_error'] = False
        with self.assertRaisesRegex(NativeProfileError, 'unpinned or unknown model'):
            native_result(sample, 0, MODEL)

    def test_native_version_rejection_has_a_specific_category_without_raw_error_text(self):
        sample = events()
        sample[-1].update(is_error=True, api_error_status=400, modelUsage={},
                          result='API Error: 400 Claude Code 2.1.153 does not support this model; '
                                 'version 2.1.290 or newer is required. private-marker')
        details = native_diagnostics(sample, 'native provider failure', 1)
        self.assertEqual(details['provider_error_category'], 'native_cli_incompatible')
        self.assertNotIn('private-marker', json.dumps(details))

    def test_invalid_model_usage_keeps_the_complete_reported_spend_on_rejection(self):
        for usage in (None, [], [MODEL], 'invalid', {}, {MODEL: {}, 'fallback': {}}):
            sample = events()
            sample[-1]['modelUsage'] = usage
            with self.subTest(model_usage=usage):
                with self.assertRaisesRegex(NativeProfileError, 'unpinned or unknown model') as caught:
                    native_result(sample, 0, MODEL)
                failure = NativeAttemptError(str(caught.exception), sample, 0)
                self.assertTrue(failure.result['usage']['counted'])
                self.assertEqual(sum(failure.result['usage'][key] for key in
                                     ('input', 'cache_write', 'cache_read', 'output')), 100)

    @unittest.skipIf(os.name == 'nt', 'private evidence permissions require POSIX')
    def test_private_error_is_bounded_redacted_and_created_without_overwriting(self):
        sample = events()
        sample[-1].update(is_error=True, api_error_status=400,
                          result='Invalid request: private-rejection-marker credential-value '
                                 'sk-ant-oat01-secret Bearer private-bearer',
                          errors=['sk-ant-api01-other-secret'])
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'provider-error.json'
            self.assertTrue(save_native_error(sample, path, 'credential-value'))
            row = json.loads(path.read_text())
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)
            self.assertEqual(row['api_error_status'], 400)
            self.assertIn('private-rejection-marker', row['error_excerpt'])
            for secret in ('credential-value', 'sk-ant-', 'private-bearer', 'other-secret'):
                self.assertNotIn(secret, path.read_text())
            self.assertFalse(row['truncated'])
            before = path.read_bytes()
            with self.assertRaises(FileExistsError):
                save_native_error(sample, path, 'credential-value')
            self.assertEqual(path.read_bytes(), before)
            sample[-1]['result'] = 'x' * 20000
            large = Path(folder) / 'bounded.json'
            save_native_error(sample, large, '')
            row = json.loads(large.read_text())
            self.assertTrue(row['truncated'])
            self.assertLessEqual(len(row['error_excerpt']), 8192)
            link = Path(folder) / 'link.json'
            link.symlink_to(path)
            with self.assertRaises(FileExistsError):
                save_native_error(sample, link, '')
            self.assertEqual(path.read_bytes(), before)

    def test_private_error_requires_one_error_result(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'provider-error.json'
            self.assertFalse(save_native_error(events(), path, 'unused'))
            self.assertFalse(save_native_error(events()[:1], path, 'unused'))
            sample = events()
            sample[-1]['is_error'] = True
            sample.append(copy.deepcopy(sample[-1]))
            self.assertFalse(save_native_error(sample, path, 'unused'))
            self.assertFalse(path.exists())

    def test_failed_result_with_pinned_usage_does_not_put_provider_text_in_ledger_result(self):
        sample = events()
        sample[-1].update(is_error=True, api_error_status=400,
                          result='private-rejection-marker', errors=['private-error-marker'])
        result = native_result(sample, 1, MODEL)
        self.assertEqual(result['outcome'], 'failed')
        self.assertEqual(result['diagnostics']['provider_error_category'], 'invalid_request')
        self.assertEqual(result['usage']['input'], 10)
        self.assertNotIn('private-', json.dumps(result))

    def test_interrupted_failure_diagnostics_preserve_unknown_usage(self):
        failure = NativeAttemptError('TimeoutError during native attempt', events()[:1])
        self.assertFalse(failure.result['usage']['counted'])
        self.assertEqual(failure.result['usage']['uncounted_calls'], 1)
        self.assertTrue(failure.result['diagnostics']['initialization_seen'])
        self.assertEqual(failure.result['diagnostics']['final_result_count'], 0)
        self.assertIsNone(failure.result['diagnostics']['provider_error_category'])

    def test_cli_prints_failure_diagnostics_without_printing_provider_result(self):
        details = native_diagnostics([], 'native container cleanup failed')
        report = {'process_profile_certified': False, 'diagnostics': details,
                  'result': {'reply': 'private-response-marker'}}
        argv = ['turnrun_native.py', 'certify', '--image', 'unused', '--worker-image', 'unused',
                '--model', MODEL, '--token-file', '/unused', '--work', '/unused', '--budget-tokens', '100']
        output = io.StringIO()
        with patch('sys.argv', argv), patch('turnrun_native.certify', return_value=report), \
                patch('sys.stdout', output):
            self.assertEqual(main(), 2)
        printed = json.loads(output.getvalue())
        self.assertEqual(printed['diagnostics'], details)
        self.assertNotIn('private-response-marker', output.getvalue())

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

    def test_partial_aggregate_keeps_valid_categories_without_double_counting_messages(self):
        for invalid_output in (None, True, -1, 'unknown'):
            sample = [
                {'type': 'assistant', 'message': {'id': 'one', 'usage': {
                    'input_tokens': 10, 'cache_read_input_tokens': 20000, 'output_tokens': 7}}},
                {'type': 'result', 'usage': {'input_tokens': 150000,
                    'cache_read_input_tokens': 30000, 'output_tokens': invalid_output}}]
            with self.subTest(output=invalid_output):
                usage = retained_usage(sample)
                self.assertEqual(usage, {'input': 150000, 'cache_write': 0, 'cache_read': 30000,
                                        'output': 7, 'counted': False, 'uncounted_calls': 1})
                self.assertIsNone(usage_of({'usage': usage})[1])
                self.assertEqual(retained_usage(sample[1:])['input'], 150000)
        sample.append(copy.deepcopy(sample[-1]))
        self.assertEqual(retained_usage(sample)['cache_read'], 30000)

    def test_aggregate_usage_counts_cache_without_double_counting_messages(self):
        result = native_result(events(), 0, MODEL)
        self.assertEqual(result['outcome'], 'done')
        self.assertEqual(sum(result['usage'][k] for k in ('input', 'cache_write', 'cache_read', 'output')), 100)
        self.assertEqual(result['model_turns'], 2)
        self.assertIsNone(result['provider_requests'])
        self.assertEqual(native_result(events(), 1, MODEL)['outcome'], 'failed')

    def test_mcp_connection_metadata_preserves_the_single_fixed_server_gate(self):
        sample = events()
        sample[0]['mcp_servers'][0]['source'] = 'dynamic'
        self.assertEqual(native_result(sample, 0, MODEL)['outcome'], 'done')
        for servers in (None, {}, ['trial'],
                        [{'name': 'other', 'status': 'connected', 'source': 'dynamic'}],
                        [{'name': 'trial', 'status': 'failed', 'source': 'dynamic'}],
                        [{'name': 'trial', 'source': 'dynamic'}],
                        [{'status': 'connected', 'source': 'dynamic'}],
                        sample[0]['mcp_servers'] * 2,
                        sample[0]['mcp_servers'] + [{'name': 'other', 'status': 'connected'}]):
            with self.subTest(servers=servers):
                sample[0]['mcp_servers'] = servers
                with self.assertRaisesRegex(NativeProfileError, 'fixed worker MCP server'):
                    native_result(sample, 0, MODEL)

    def test_profile_without_worker_requires_no_tools_and_no_mcp_servers(self):
        sample = events()
        sample[0].update(tools=[], mcp_servers=[])
        self.assertEqual(native_result(sample, 0, MODEL, worker_required=False)['outcome'], 'done')
        for servers in (None, {}, [{'name': 'other', 'status': 'connected'}]):
            sample[0]['mcp_servers'] = servers
            with self.subTest(servers=servers):
                with self.assertRaisesRegex(NativeProfileError, 'unexpected MCP server'):
                    native_result(sample, 0, MODEL, worker_required=False)

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


class CertificationFinalizationTest(unittest.TestCase):
    def test_cleanup_failure_refuses_certification_and_preserves_known_usage(self):
        # All boundaries and authentication are mocks. No live session or inherited
        # environment is changed while exercising standalone-only finalization.
        cases = [(boundary, 'success') for boundary in (None, 'broker', 'endpoint', 'worker')]
        cases += [('broker', 'overshoot'), (None, 'native-error')]
        cases += [(boundary, 'unknown-native-error') for boundary in ('broker', 'endpoint', 'worker')]
        for failing_boundary, kind in cases:
            with self.subTest(boundary=failing_boundary, kind=kind), tempfile.TemporaryDirectory() as folder:
                work = Path(folder) / 'proof'
                with patch('turnrun_native.os', SimpleNamespace(environ={})), \
                     patch('turnrun_native.oauth_token'), \
                     patch('turnrun_native.DockerSandbox') as sandbox, \
                     patch('turnrun_native.NativeBroker') as native, \
                     patch('turnrun_broker.WorkerEndpoint') as endpoint, \
                     patch('secrets.token_hex', return_value='fixed-test-nonce'):
                    worker = sandbox.return_value.__enter__.return_value
                    service = endpoint.return_value.__enter__.return_value
                    service.calls, service.failed = 1, False
                    broker = native.return_value.__enter__.return_value
                    native_error = 'native-error' in kind
                    unknown = kind == 'unknown-native-error'
                    tokens = 11 if kind == 'success' else 151
                    result = {'outcome': 'failed' if native_error else 'done', 'usage': {
                        'input': tokens-6, 'cache_write': 0, 'cache_read': 0, 'output': 6,
                        'counted': not unknown, 'uncounted_calls': int(unknown)}}
                    if native_error:
                        failure = NativeAttemptError('mock provider failure', [], 1)
                        failure.result = result
                        broker.run.side_effect = failure
                    else:
                        broker.run.return_value = result

                    def submission(output):
                        output.mkdir()
                        (output / 'native-proof.txt').write_bytes(b'fixed-test-nonce\n')
                        return output

                    worker.submission.side_effect = submission
                    boundaries = {'worker': sandbox, 'endpoint': endpoint, 'broker': native}
                    if failing_boundary:
                        boundaries[failing_boundary].return_value.__exit__.side_effect = RuntimeError('cleanup')
                    report = certify(image='mock-native', worker_image='mock-worker', model=MODEL,
                                     token_file='never-read', budget_tokens=100, seconds=30, work=work)
                self.assertEqual(report['process_profile_certified'], failing_boundary is None and kind == 'success')
                self.assertEqual(report['known_tokens'], None if unknown else tokens)
                self.assertEqual(report['reserved_tokens'], tokens if unknown else 0)
                self.assertEqual(report['result'], result)
                rows = [json.loads(line) for line in (work / 'ledger.jsonl').read_text().splitlines()]
                kinds = ['start', 'finish']
                if kind == 'overshoot':
                    kinds.append('halt')
                kinds.append('halt' if failing_boundary or native_error else 'complete')
                self.assertEqual([row['kind'] for row in rows], kinds)
                self.assertEqual(rows[1]['tokens'], None if unknown else tokens)
                self.assertEqual(rows[1]['lower_bound'], tokens)
                self.assertEqual(json.loads((work / 'report.json').read_text()), report)
                if failing_boundary:
                    self.assertEqual(report['error'], 'RuntimeError')
                elif native_error:
                    self.assertEqual(report['error'], 'NativeAttemptError')


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
