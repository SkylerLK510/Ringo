"""Exercise real HTTP/MCP boundaries without provider inference."""
import http.client
import json
import os
from pathlib import Path
import socket
import tempfile
import time
import unittest
from unittest.mock import patch

from turnrun_broker import WorkerEndpoint
from turnrun_isolation import DockerSandbox


class Worker:
    check, stopped, id, lifetime = None, False, 'fixed-worker', 900

    def __init__(self):
        self.lease_deadline = time.monotonic()+self.lifetime
        self.commands = []

    def tool(self, request):
        self.commands.append(request)
        return {'exit': 0, 'timeout': False, 'output': 'task data', 'truncated': False}


def send(endpoint, message, *, headers=None, method='POST', body=None):
    connection = http.client.HTTPConnection(endpoint.authority, timeout=10)
    base = {'Authorization': 'Bearer '+endpoint.token, 'Content-Type': 'application/json'}
    base.update(headers or {})
    try:
        connection.request(method, '/mcp', json.dumps(message) if body is None else body, base)
        response = connection.getresponse()
        data = response.read()
        return response.status, json.loads(data) if data else None
    finally:
        connection.close()


def rpc(method, request_id=1, **params):
    return {'jsonrpc': '2.0', 'id': request_id, 'method': method, 'params': params}


def initialize(endpoint):
    status, message = send(endpoint, rpc('initialize', 1, protocolVersion='2025-03-26', capabilities={},
                                       clientInfo={'name': 'offline-test', 'version': '1'}))
    assert status == 200 and set(message['result']['capabilities']) == {'tools'}, message
    assert send(endpoint, {'jsonrpc': '2.0', 'method': 'notifications/initialized'}) == (202, None)


class WorkerTransportTest(unittest.TestCase):
    def test_incomplete_headers_cannot_hold_endpoint_open(self):
        with patch('turnrun_broker.REQUEST_SECONDS', 0.1), WorkerEndpoint(Worker()) as endpoint:
            with socket.create_connection(endpoint.server.server_address, timeout=2) as client:
                client.sendall(b'POST /mcp HTTP/1.1\r\nHost: ')
                self.assertEqual(client.recv(1), b'')
            # The serial server remains usable after closing the incomplete request.
            initialize(endpoint)

    def test_auth_origin_host_and_json_framing_are_enforced_before_dispatch(self):
        worker = Worker()
        with WorkerEndpoint(worker) as endpoint:
            for headers, expected in [({'Authorization': ''}, 401), ({'Origin': 'https://attacker.test'}, 403),
                                      ({'Origin': 'null'}, 403), ({'Host': 'attacker.test'}, 403)]:
                self.assertEqual(send(endpoint, rpc('initialize'), headers=headers)[0], expected)
            for body in ('not JSON', '{"jsonrpc":"2.0","id":1,"id":2}', '{"id":NaN}'):
                self.assertEqual(send(endpoint, None, body=body)[0], 400)
            self.assertEqual(send(endpoint, None, method='GET')[0], 405)
            self.assertEqual(send(endpoint, rpc('tools/list'))[1]['error']['message'], 'initialization required')
            self.assertEqual(worker.commands, [])

    def test_fixed_tool_schema_rejects_authority_changes_and_duplicate_execution(self):
        worker = Worker()
        with WorkerEndpoint(worker, max_calls=1) as endpoint:
            initialize(endpoint)
            tools = send(endpoint, rpc('tools/list', 2))[1]['result']['tools']
            self.assertEqual([t['name'] for t in tools], ['worker'])
            attempts = [rpc('resources/read', 3, uri='file:///owner'),
                rpc('tools/call', 4, name='Bash', arguments={'command': 'id', 'seconds': 1}),
                rpc('tools/call', 5, name='worker', arguments={'command': 'id', 'seconds': 1, 'container': 'neighbor'}),
                rpc('tools/call', 6, name='worker', arguments={'command': 'id', 'seconds': True}),
                rpc('tools/call', 7, name='worker', arguments={'command': '\ud800', 'seconds': 1})]
            for message in attempts:
                self.assertIn('error', send(endpoint, message)[1])
            self.assertEqual(worker.commands, [])
            call = rpc('tools/call', 8, name='worker', arguments={'command': 'id', 'seconds': 1})
            self.assertIn('result', send(endpoint, call)[1])
            self.assertIn('error', send(endpoint, call)[1])
            call['id'] = 9
            self.assertIn('error', send(endpoint, call)[1])
            self.assertEqual(len(worker.commands), 1)
        with self.assertRaisesRegex(ValueError, 'reused'):
            endpoint.__enter__()

    def test_backend_failure_halts_endpoint_and_lease_admission_stops_late_work(self):
        worker = Worker()
        with WorkerEndpoint(worker) as endpoint:
            initialize(endpoint)
            endpoint.deadline = time.monotonic()+10
            call = rpc('tools/call', 2, name='worker', arguments={'command': 'id', 'seconds': 1})
            self.assertIn('error', send(endpoint, call)[1])
            self.assertEqual(worker.commands, [])
            endpoint.deadline = time.monotonic()+120
            def lost(request):
                raise RuntimeError('private host details must not reach the model')
            worker.tool = lost
            call['id'] = 3
            response = send(endpoint, call)[1]
            self.assertNotIn('private host details', json.dumps(response))
            self.assertTrue(endpoint.failed)
            self.assertEqual(endpoint.calls, 1)
            call['id'] = 4
            self.assertIn('error', send(endpoint, call)[1])
            self.assertEqual(endpoint.calls, 1)

    def test_checker_cannot_be_exposed_as_worker(self):
        worker = Worker()
        worker.check = Path('/protected/check.py')
        with self.assertRaisesRegex(ValueError, 'without protected checks'):
            WorkerEndpoint(worker)


@unittest.skipUnless(os.environ.get('RINGO_TEST_IMAGE'), 'requires a pinned Docker image')
class DockerTransportTest(unittest.TestCase):
    def test_mcp_executes_in_worker_and_exports_only_after_shutdown(self):
        with tempfile.TemporaryDirectory() as temp:
            with DockerSandbox(os.environ['RINGO_TEST_IMAGE']) as worker:
                with WorkerEndpoint(worker) as endpoint:
                    initialize(endpoint)
                    call = rpc('tools/call', 2, name='worker', arguments={
                        'command': 'test ! -e /protected/check.py && echo contained > answer.txt', 'seconds': 5})
                    result = send(endpoint, call)[1]['result']
                    self.assertEqual(json.loads(result['content'][0]['text'])['exit'], 0)
                    self.assertFalse(result['isError'])
                    # The endpoint capability stays in the controller, never the task's environment.
                    env = worker.execute('env')['output']
                    self.assertNotIn(endpoint.token, env)
                submitted = worker.submission(Path(temp)/'submission')
                self.assertEqual((submitted/'answer.txt').read_text(), 'contained\n')


if __name__ == '__main__':
    unittest.main()
