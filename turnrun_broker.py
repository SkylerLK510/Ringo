"""Controller-side MCP worker transport; not a certified native subscription launcher.

The future isolated provider process receives only this endpoint's private MCP configuration.
The controller retains Docker authority, fixture/check selection and evaluator state. No provider
is launched here. Native authentication, builtin-tool removal and discovery still need certification.
"""
import hmac
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
import secrets
import socket
import threading
import time

MAX_REQUEST = 128*1024
REQUEST_SECONDS = 5
PROTOCOL = '2025-03-26'
TOOL = {'name': 'worker', 'description': 'Run a command in this trial\'s isolated task workspace.',
        'inputSchema': {'type': 'object', 'properties': {
            'command': {'type': 'string', 'maxLength': 65536},
            'seconds': {'type': 'integer', 'minimum': 1, 'maximum': 300}},
            'required': ['command', 'seconds'], 'additionalProperties': False}}


def unique_object(pairs):
    result = {}
    for key, value in pairs:
        if key in result:
            raise ValueError('duplicate JSON field')
        result[key] = value
    return result


def reject_constant(value):
    raise ValueError('nonfinite JSON')


class WorkerEndpoint:
    """One leased, authenticated endpoint with a fixed worker and bounded serial tool calls.

    This does not grant any host filesystem, evaluator, executor or container selection route.
    Loopback and authentication protect the transport; they do not isolate the provider process.
    """
    def __init__(self, worker, *, max_calls=128, lifetime=900):
        if worker.check or worker.stopped or not worker.id:
            raise ValueError('endpoint requires an active task worker without protected checks')
        if type(max_calls) is not int or not 1 <= max_calls <= 1024:
            raise ValueError('tool call limit must be from 1 through 1024')
        if type(lifetime) is not int or not 1 <= lifetime <= worker.lifetime:
            raise ValueError('endpoint lifetime must fit the worker lease')
        self.worker, self.max_calls, self.lifetime = worker, max_calls, lifetime
        self.calls, self.failed, self.initialized, self.ready = 0, False, False, False
        self.token = secrets.token_urlsafe(32)
        self.server = self.thread = None
        self.deadline = 0
        self.used, self.seen = False, set()

    def dispatch(self, message):
        request_id = message.get('id') if isinstance(message, dict) else None

        def error(code, reason):
            return {'jsonrpc': '2.0', 'id': request_id, 'error': {'code': code, 'message': reason}}

        if not isinstance(message, dict) or message.get('jsonrpc') != '2.0' or set(message) - {
                'jsonrpc', 'id', 'method', 'params'} or not isinstance(message.get('method'), str):
            return error(-32600, 'invalid request')
        if 'id' in message and (type(request_id) not in (str, int) or len(str(request_id)) > 256):
            return error(-32600, 'invalid request id')
        method, params = message['method'], message.get('params', {})
        if not isinstance(params, dict):
            return error(-32602, 'invalid parameters')
        if time.monotonic() >= self.deadline or self.failed:
            return error(-32000, 'worker endpoint is unavailable')
        if method == 'notifications/initialized' and 'id' not in message:
            self.ready = self.initialized
            return None
        if 'id' not in message:
            # Unknown notifications have no authority and must never execute a tool.
            return None
        identity = (type(request_id), request_id)
        if identity in self.seen or len(self.seen) >= 4*self.max_calls+64:
            return error(-32600, 'duplicate request id or request limit reached')
        self.seen.add(identity)
        if method == 'initialize':
            if self.initialized or not isinstance(params.get('protocolVersion'), str):
                return error(-32602, 'invalid initialization')
            self.initialized = True
            result = {'protocolVersion': PROTOCOL, 'capabilities': {'tools': {}},
                      'serverInfo': {'name': 'ringo-isolated-worker', 'version': '1'}}
        elif method == 'ping':
            result = {}
        elif not self.ready:
            return error(-32000, 'initialization required')
        elif method == 'tools/list':
            if set(params) - {'_meta'}:
                return error(-32602, 'invalid tool list parameters')
            result = {'tools': [TOOL]}
        elif method == 'tools/call':
            args = params.get('arguments')
            if set(params) - {'name', 'arguments', '_meta'} or params.get('name') != 'worker' or not isinstance(
                    args, dict) or set(args) != {'command', 'seconds'}:
                return error(-32602, 'only the fixed worker tool is available')
            try:
                size = len(args['command'].encode()) if isinstance(args['command'], str) else 65537
            except UnicodeError:
                size = 65537
            if size > 65536 or type(
                    args['seconds']) is not int or not 1 <= args['seconds'] <= min(300, self.worker.lifetime):
                return error(-32602, 'invalid worker arguments')
            if self.calls >= self.max_calls or args['seconds']+15 > self.deadline-time.monotonic():
                return error(-32000, 'worker tool admission limit reached')
            # Reserve before execution; errors consume the call and cannot cause an implicit retry.
            self.calls += 1
            try:
                observation = self.worker.tool(args)
                text = json.dumps(observation, allow_nan=False)
                if len(text.encode()) > 300000:
                    raise ValueError('worker result too large')
                result = {'content': [{'type': 'text', 'text': text}],
                          'isError': bool(observation['timeout'] or observation['exit'])}
            except Exception:
                self.failed = True
                return error(-32000, 'worker execution failed; endpoint halted')
        else:
            return error(-32601, 'method unavailable')
        return {'jsonrpc': '2.0', 'id': request_id, 'result': result}

    def __enter__(self):
        if self.used:
            raise ValueError('endpoint cannot be reused')
        self.used = True
        endpoint = self

        class Handler(BaseHTTPRequestHandler):
            # One connection per request, no unbounded persistent client or worker threads.
            def setup(self):
                super().setup()
                self.connection.settimeout(3)
                # A socket inactivity timeout alone permits a client to trickle headers forever.
                def expire():
                    try:
                        self.connection.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass
                self.read_deadline = threading.Timer(REQUEST_SECONDS, expire)
                self.read_deadline.daemon = True
                self.read_deadline.start()

            def finish(self):
                self.read_deadline.cancel()
                super().finish()

            def log_message(self, *args):
                pass  # Requests and bearer credentials never enter generic HTTP logs.

            def reply(self, status, body=None):
                data = b'' if body is None else json.dumps(body, allow_nan=False).encode()
                self.close_connection = True
                try:
                    self.send_response(status)
                    self.send_header('Content-Type', 'application/json')
                    self.send_header('Content-Length', str(len(data)))
                    self.send_header('Connection', 'close')
                    self.end_headers()
                    self.wfile.write(data)
                except OSError:
                    pass  # A disconnect never retries a worker mutation.

            def do_POST(self):
                if self.path != '/mcp' or self.headers.get('Origin') is not None or self.headers.get(
                        'Host') != endpoint.authority:
                    self.reply(403)
                    return
                auth = self.headers.get('Authorization', '')
                if not hmac.compare_digest(auth.encode(), ('Bearer '+endpoint.token).encode()):
                    self.reply(401)
                    return
                try:
                    length = int(self.headers.get('Content-Length', '0'))
                    if not 0 < length <= MAX_REQUEST or self.headers.get('Transfer-Encoding') or self.headers.get(
                            'Content-Type', '').split(';')[0] != 'application/json':
                        raise ValueError('invalid body framing')
                    message = json.loads(self.rfile.read(length), object_pairs_hook=unique_object,
                                         parse_constant=reject_constant)
                except (ValueError, OSError):
                    self.reply(400)
                    return
                self.read_deadline.cancel()
                response = endpoint.dispatch(message)
                self.reply(202 if response is None else 200, response)

            def do_GET(self):
                self.reply(405)

            do_DELETE = do_GET

        self.server = HTTPServer(('127.0.0.1', 0), Handler)
        self.authority = f'127.0.0.1:{self.server.server_port}'
        self.deadline = min(time.monotonic()+self.lifetime, self.worker.lease_deadline)
        self.thread = threading.Thread(target=self.server.serve_forever, kwargs={'poll_interval': 0.05}, daemon=True)
        self.thread.start()
        return self

    def mcp_config(self):
        if not self.server:
            raise ValueError('endpoint is not active')
        return {'mcpServers': {'worker': {'type': 'http', 'url': f'http://{self.authority}/mcp',
                'headers': {'Authorization': 'Bearer '+self.token}}}}

    def __exit__(self, *exc):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        self.server = None
        self.token = secrets.token_urlsafe(32)
