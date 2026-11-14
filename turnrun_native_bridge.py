"""Immutable container-side native provider bridge. No owner files or task code belong here.

The container has --network none. Its only network path is a framed, bounded CONNECT
relay over docker exec stdio; the controller independently fixes the upstream authority.
MCP requests use the same private channel. This program never opens an upstream socket.
"""
import base64
import hashlib
import http.server
import json
import os
from pathlib import Path
import queue
import socket
import subprocess
import sys
import threading
import time

FRAME_BYTES = 512 * 1024
CHUNK_BYTES = 16384
AUTHORITY = 'api.anthropic.com:443'
CLI = '/opt/claude'
SETTINGS = {'disableAllHooks': True, 'enabledPlugins': {}, 'forceLoginMethod': 'claudeai',
            'permissions': {'allow': ['mcp__trial__worker']}}


def environment(token, proxy):
    # No inherited provider routing, API keys, project settings or owner home.
    return {'PATH': '/usr/local/bin:/usr/bin:/bin', 'HOME': '/tmp/native/home',
            'CLAUDE_CONFIG_DIR': '/tmp/native/config', 'TMPDIR': '/tmp',
            'CLAUDE_CODE_OAUTH_TOKEN': token, 'HTTPS_PROXY': proxy, 'HTTP_PROXY': proxy,
            'NO_PROXY': '127.0.0.1,localhost', 'RINGO_CHILD': '1',
            'CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC': '1', 'DISABLE_AUTOUPDATER': '1',
            'CLAUDE_CODE_SKIP_PROMPT_HISTORY': '1', 'ENABLE_TOOL_SEARCH': 'false',
            'MCP_TIMEOUT': '10000', 'MCP_TOOL_TIMEOUT': '300000', 'CLAUDE_CODE_MAX_RETRIES': '0',
            'MCP_CONNECTION_NONBLOCKING': '0', 'CLAUDE_CODE_MAX_OUTPUT_TOKENS': '4096'}


def arguments(model, mcp, instruction, turns):
    return [CLI, '--print', '--verbose', '--output-format', 'stream-json', '--model', model,
            '--tools', '', '--disable-slash-commands', '--no-chrome', '--no-session-persistence',
            '--setting-sources', '', '--settings', json.dumps(SETTINGS), '--strict-mcp-config',
            '--mcp-config', json.dumps(mcp), '--permission-mode', 'dontAsk',
            '--max-turns', str(turns), '--append-system-prompt', instruction]


class Bridge:
    def __init__(self):
        self.lock = threading.Lock()
        self.pending = {}
        self.next_id = 0
        self.state_lock = threading.Lock()
        self.closed = threading.Event()

    def emit(self, **frame):
        data = json.dumps(frame, separators=(',', ':'), allow_nan=False).encode() + b'\n'
        if len(data) > FRAME_BYTES:
            raise ValueError('bridge frame too large')
        with self.lock:
            while data:
                data = data[os.write(1, data):]

    def channel(self):
        with self.state_lock:
            if len(self.pending) >= 8 or self.next_id >= 256:
                raise ValueError('bridge channel limit reached')
            self.next_id += 1
            key = self.next_id
            self.pending[key] = queue.Queue(maxsize=8)
            return key, self.pending[key]

    def forget(self, key):
        with self.state_lock:
            self.pending.pop(key, None)

    def receive(self):
        try:
            buffer = b''
            while block := os.read(0, 65536):
                buffer += block
                while b'\n' in buffer:
                    line, buffer = buffer.split(b'\n', 1)
                    if len(line) >= FRAME_BYTES:
                        raise ValueError('invalid controller frame')
                    frame = json.loads(line)
                    with self.state_lock:
                        target = self.pending.get(frame.get('id'))
                    if target is not None:
                        target.put(frame, timeout=5)
                if len(buffer) >= FRAME_BYTES:
                    raise ValueError('invalid controller frame')
        finally:
            self.closed.set()
            # EOF or malformed input must not leave an authenticated process alive.
            os._exit(1)

    def rpc(self, message):
        key, replies = self.channel()
        try:
            self.emit(type='mcp', id=key, message=message)
            frame = replies.get(timeout=315)
            if frame.get('type') != 'mcp':
                raise ValueError('invalid MCP response')
            return frame.get('message')
        finally:
            self.forget(key)

    def server(self, proxy=False):
        bridge = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = 'HTTP/1.0'

            def setup(self):
                super().setup()
                self.connection.settimeout(10)

            def log_message(self, *args):
                pass

            def do_POST(self):
                try:
                    if proxy or self.path != '/mcp' or self.headers.get('Origin') is not None:
                        self.send_error(403)
                        return
                    length = int(self.headers.get('Content-Length', '0'))
                    if not 0 < length <= 128 * 1024 or self.headers.get('Transfer-Encoding'):
                        raise ValueError('invalid MCP framing')
                    reply = bridge.rpc(json.loads(self.rfile.read(length)))
                    data = b'' if reply is None else json.dumps(reply).encode()
                    self.send_response(202 if reply is None else 200)
                    self.send_header('Content-Type', 'application/json')
                    self.send_header('Content-Length', str(len(data)))
                    self.end_headers()
                    self.wfile.write(data)
                except Exception:
                    self.close_connection = True

            def do_CONNECT(self):
                if not proxy or self.path != AUTHORITY:
                    self.send_error(403)
                    return
                key, replies = bridge.channel()
                try:
                    bridge.emit(type='open', id=key, authority=self.path)
                    if replies.get(timeout=15).get('type') != 'opened':
                        return
                    self.send_response(200, 'Connection established')
                    self.end_headers()
                    self.wfile.flush()

                    def read_local():
                        try:
                            while data := self.connection.recv(CHUNK_BYTES):
                                bridge.emit(type='data', id=key, data=base64.b64encode(data).decode())
                        except OSError:
                            pass
                        finally:
                            bridge.emit(type='close', id=key)

                    threading.Thread(target=read_local, daemon=True).start()
                    while True:
                        frame = replies.get(timeout=120)
                        if frame.get('type') == 'close':
                            break
                        if frame.get('type') != 'data':
                            raise ValueError('invalid tunnel frame')
                        data = base64.b64decode(frame['data'], validate=True)
                        if len(data) > CHUNK_BYTES:
                            raise ValueError('oversized tunnel chunk')
                        self.connection.sendall(data)
                except (OSError, ValueError, queue.Empty):
                    pass
                finally:
                    bridge.forget(key)
                    bridge.emit(type='close', id=key)
                    self.close_connection = True

        server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        server.daemon_threads = True
        threading.Thread(target=server.serve_forever, daemon=True).start()
        return server

    def run(self, request, offline=False):
        proxy, mcp = self.server(proxy=True), self.server()
        proxy_url = f'http://127.0.0.1:{proxy.server_port}'
        config = {'mcpServers': {'trial': {'type': 'http',
                  'url': f'http://127.0.0.1:{mcp.server_port}/mcp'}}}
        for folder in ('home', 'config', 'empty'):
            Path('/tmp/native', folder).mkdir(parents=True, exist_ok=False)
        env = environment(request['oauth_token'], proxy_url)
        fake = None
        if offline:
            fake = fake_provider()
            env['ANTHROPIC_BASE_URL'] = f'http://127.0.0.1:{fake.server_port}'
        args = arguments(request['model'], config, request['instruction'], request['max_turns'])
        # No session profile, hook, project instruction or credential file is copied in.
        with subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL, cwd='/tmp/native/empty', env=env) as proc:
            proc.stdin.write(request['prompt'].encode())
            proc.stdin.close()
            total = 0
            while line := proc.stdout.readline(FRAME_BYTES + 1):
                total += len(line)
                if len(line) > FRAME_BYTES or total > 8 * 1024 * 1024:
                    proc.kill()
                    raise ValueError('provider output exceeded limit')
                self.emit(type='event', event=json.loads(line))
            self.emit(type='exit', code=proc.wait())
        if fake:
            self.emit(type='offline-native', synthetic=True, provider_calls=0,
                      replay_requests=fake.requests, tool_result_seen=fake.tool_result_seen)
            fake.shutdown()

    def exercise(self):
        """Deterministic transport exercise. Never launch the native CLI or any upstream."""
        def rpc(method, key=None, **params):
            message = {'jsonrpc': '2.0', 'method': method, 'params': params}
            if key is not None:
                message['id'] = key
            return self.rpc(message)
        init = rpc('initialize', 1, protocolVersion='2025-03-26', capabilities={},
                   clientInfo={'name': 'zero-inference-native-transport-probe', 'version': '1'})
        rpc('notifications/initialized')
        tools = rpc('tools/list', 2)
        result = rpc('tools/call', 3, name='worker', arguments={
            'command': 'printf native-transport-proof > /work/native-transport.txt', 'seconds': 5})
        self.emit(type='exercise', initialization=init, tools=tools, result=result,
                  provider_calls=0, synthetic=True)


def probe():
    """Zero-inference runtime evidence. Tool discovery still needs a live initialization."""
    version = subprocess.check_output([CLI, '--version'], timeout=15, env=environment('', '')).decode().strip()
    denied = False
    try:
        socket.create_connection(('1.1.1.1', 443), timeout=1).close()
    except OSError:
        denied = True
    print(json.dumps({'version': version, 'uid': os.getuid(), 'direct_network_denied': denied,
                      'bridge_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                      'binary_sha256': hashlib.sha256(Path(CLI).read_bytes()).hexdigest(),
                      'owner_home_absent': not Path('/Users').exists(),
                      'docker_socket_absent': not Path('/var/run/docker.sock').exists(),
                      'managed_settings_absent': not Path('/etc/claude-code').exists(),
                      'task_workspace_empty': not any(Path('/work').iterdir())}))


def fake_provider():
    """Local deterministic protocol replay, exclusively for --exercise-native.

    It runs inside the same network-disabled container. No real credential, upstream
    connection, API key or inference is used. It is never selected by normal startup.
    """
    class Replay(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.end_headers()
            self.wfile.write(b'{}')

        def do_POST(self):
            length = int(self.headers.get('Content-Length', '0'))
            if not 0 < length < FRAME_BYTES:
                self.send_error(400)
                return
            body = json.loads(self.rfile.read(length))
            if self.path.split('?')[0] != '/v1/messages':
                self.do_GET()
                return
            self.server.requests += 1
            tool_reply = any(part.get('type') == 'tool_result'
                             for message in body.get('messages', [])
                             if isinstance(message.get('content'), list)
                             for part in message['content'] if isinstance(part, dict))
            self.server.tool_result_seen |= tool_reply
            tool = not tool_reply
            block = ({'type': 'tool_use', 'id': 'offline-worker-call', 'name': 'mcp__trial__worker', 'input': {}}
                     if tool else {'type': 'text', 'text': ''})
            delta = ({'type': 'input_json_delta', 'partial_json': json.dumps({
                'command': 'printf native-transport-proof > /work/native-transport.txt', 'seconds': 5})}
                     if tool else {'type': 'text_delta', 'text': 'Offline native protocol replay complete.'})
            stream = [
                ('message_start', {'type': 'message_start', 'message': {'id': f'offline-{self.server.requests}',
                    'type': 'message', 'role': 'assistant', 'model': body.get('model'), 'content': [],
                    'stop_reason': None, 'stop_sequence': None,
                    'usage': {'input_tokens': 10, 'output_tokens': 0, 'cache_creation_input_tokens': 0,
                              'cache_read_input_tokens': 0}}}),
                ('content_block_start', {'type': 'content_block_start', 'index': 0, 'content_block': block}),
                ('content_block_delta', {'type': 'content_block_delta', 'index': 0, 'delta': delta}),
                ('content_block_stop', {'type': 'content_block_stop', 'index': 0}),
                ('message_delta', {'type': 'message_delta', 'delta': {
                    'stop_reason': 'tool_use' if tool else 'end_turn', 'stop_sequence': None},
                    'usage': {'output_tokens': 10}}),
                ('message_stop', {'type': 'message_stop'})]
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.end_headers()
            for kind, event in stream:
                self.wfile.write(f'event: {kind}\ndata: {json.dumps(event)}\n\n'.encode())
                self.wfile.flush()

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Replay)
    server.daemon_threads = True
    server.requests, server.tool_result_seen = 0, False
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


if __name__ == '__main__':
    if sys.argv[1:] == ['--probe']:
        probe()
    else:
        bridge = Bridge()
        try:
            exercise = sys.argv[1:] == ['--exercise']
            offline_native = sys.argv[1:] == ['--exercise-native']
            if not exercise and not offline_native:
                line = sys.stdin.buffer.readline(FRAME_BYTES + 1)
                if len(line) > FRAME_BYTES or not line.endswith(b'\n'):
                    raise ValueError('invalid startup frame')
                request = json.loads(line)
            threading.Thread(target=bridge.receive, daemon=True).start()
            if exercise:
                bridge.exercise()
            elif offline_native:
                bridge.run({'oauth_token': 'sk-ant-oat01-offline-placeholder', 'model': 'claude-sonnet-4-6',
                            'max_turns': 4, 'instruction': 'Use only the provided worker.',
                            'prompt': 'Write the requested native transport proof using the worker.'}, offline=True)
            else:
                bridge.run(request)
        except Exception:
            bridge.emit(type='fault', reason='native bridge failed; usage may be unknown')
            sys.exit(1)
