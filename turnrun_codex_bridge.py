"""Container-side Codex app-server bridge. No task files or owner profile enter here."""
import hashlib
import http.server
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import threading

sys.path.insert(0, str(Path(__file__).resolve().parent))
from turnrun_native_bridge import Bridge, FRAME_BYTES

CLI = '/opt/codex'
MODEL = 'gpt-6-luna'
DISABLED = ('shell_tool', 'apps', 'plugins', 'hooks', 'multi_agent', 'multi_agent_v2',
            'code_mode', 'code_mode_only', 'code_mode_host', 'image_generation',
            'view_image', 'goals', 'memories', 'browser_use', 'computer_use',
            'default_mode_request_user_input', 'request_permissions_tool', 'tool_suggest',
            'skill_search', 'token_budget', 'current_time_reminder', 'sleep_tool',
            'send_message_to_user_async', 'enable_request_compression', 'shell_snapshot')


def environment(proxy):
    return {'PATH': '/usr/local/bin:/usr/bin:/bin', 'HOME': '/tmp/native/home',
            'CODEX_HOME': '/tmp/native/config', 'TMPDIR': '/tmp', 'RINGO_CHILD': '1',
            'HTTPS_PROXY': proxy, 'HTTP_PROXY': proxy, 'NO_PROXY': '127.0.0.1,localhost'}


def arguments(offline_url=None):
    args = [CLI, 'app-server', '--strict-config']
    config = {'model': MODEL, 'model_provider': 'openai', 'model_reasoning_effort': 'low',
              'model_catalog_json': '/opt/ringo/models.json', 'web_search': 'disabled',
              'analytics.enabled': False, 'tools.update_plan.enabled': False,
              'tools.experimental_request_user_input.enabled': False, 'agents.enabled': False,
              'include_collaboration_mode_instructions': False, 'project_doc_max_bytes': 0,
              'skills.include_instructions': False, 'skills.bundled.enabled': False,
              'features': {name: False for name in DISABLED}}
    # TOML inline feature tables use '='; emit individual overrides instead.
    features = config.pop('features')
    config.update({'features.'+name: value for name, value in features.items()})
    if offline_url is not None:
        config.update(model_provider='replay', **{
            'model_providers.replay.name': 'offline-replay',
            'model_providers.replay.base_url': offline_url,
            'model_providers.replay.wire_api': 'responses',
            'model_providers.replay.request_max_retries': 0,
            'model_providers.replay.stream_max_retries': 0,
            'model_providers.replay.supports_websockets': False})
    else:
        config['forced_login_method'] = 'chatgpt'
    for key, value in config.items():
        args.extend(['-c', key+'='+json.dumps(value)])
    return args


class CodexBridge(Bridge):
    authority = 'chatgpt.com:443'

    def run(self, request, offline=False):
        if request['model'] != MODEL or request['prompt'] == '/compact':
            raise ValueError('unsupported Codex profile request')
        persistent = request.get('persistent', False)
        for folder in ('home', 'config', 'empty'):
            Path('/tmp/native', folder).mkdir(parents=True, exist_ok=persistent)
        proxy = self.server(proxy=True)
        fake = fake_provider() if offline else None
        args = arguments(f'http://127.0.0.1:{fake.server_port}/v1' if fake else None)
        # Fetch the exact worker schema over the existing controller-only MCP wire.
        # Codex receives it as one dynamic app-server tool; there are no MCP plugins,
        # native shell tools, file editors, connectors, skills or model delegation.
        self.rpc({'jsonrpc': '2.0', 'id': 1, 'method': 'initialize', 'params': {
            'protocolVersion': '2025-03-26', 'capabilities': {},
            'clientInfo': {'name': 'ringo-codex', 'version': '1'}}})
        self.rpc({'jsonrpc': '2.0', 'method': 'notifications/initialized'})
        listing = self.rpc({'jsonrpc': '2.0', 'id': 2, 'method': 'tools/list', 'params': {}})
        tools = listing.get('result', {}).get('tools', [])
        if len(tools) != 1 or tools[0].get('name') != 'worker':
            raise ValueError('unexpected worker discovery')
        tool = tools[0]
        with subprocess.Popen(args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                              stderr=subprocess.DEVNULL, cwd='/tmp/native/empty',
                              env=environment(f'http://127.0.0.1:{proxy.server_port}')) as proc:
            def send(message):
                line = json.dumps(message, allow_nan=False).encode()+b'\n'
                if len(line) > FRAME_BYTES:
                    raise ValueError('oversized Codex input')
                proc.stdin.write(line)
                proc.stdin.flush()
            def read():
                line = proc.stdout.readline(FRAME_BYTES+1)
                if not line or len(line) > FRAME_BYTES or not line.endswith(b'\n'):
                    raise ValueError('invalid Codex output framing')
                return json.loads(line)
            def call(key, method, params):
                send({'id': key, 'method': method, 'params': params})
                for _ in range(512):
                    message = read()
                    if message.get('id') == key and 'method' not in message:
                        if 'error' in message:
                            raise ValueError('Codex initialization or authentication failed')
                        return message
                    if 'id' in message and 'method' in message:
                        raise ValueError('unexpected Codex initialization request')
                raise ValueError('Codex initialization event limit')
            try:
                call(1, 'initialize', {'clientInfo': {'name': 'ringo-rrsi', 'version': '1'},
                                      'capabilities': {'experimentalApi': True}})
                send({'method': 'initialized'})
                if not offline:
                    auth = request['oauth_token']
                    call(10, 'account/login/start', {'type': 'chatgptAuthTokens',
                         'accessToken': auth['access_token'], 'chatgptAccountId': auth['account_id']})
                    del auth
                request.pop('oauth_token', None)
                resume = request.get('resume')
                params = {'model': MODEL, 'modelProvider': 'replay' if offline else 'openai',
                          'cwd': '/tmp/native/empty', 'approvalPolicy': 'never', 'sandbox': 'read-only',
                          'developerInstructions': request['instruction']}
                if resume is None:
                    params.update(ephemeral=not persistent, allowProviderModelFallback=False,
                                  dynamicTools=[{'type': 'function', **tool}])
                    method = 'thread/start'
                else:
                    params.update(threadId=resume, excludeTurns=True)
                    method = 'thread/resume'
                thread = call(2, method, params)
                result = thread['result']
                if result.get('model') != MODEL or result.get('modelProvider') != ('replay' if offline else 'openai'):
                    raise ValueError('Codex model or provider changed')
                sid = result.get('thread', {}).get('id')
                if not sid or resume is not None and sid != resume:
                    raise ValueError('Codex thread identity changed')
                # Offline transport metadata is marked separately and is never live
                # evidence. Normalize just the local provider name for the same gate.
                if offline:
                    result['modelProvider'] = 'openai'
                self.emit(type='event', event=thread)
                send({'id': 3, 'method': 'turn/start', 'params': {'threadId': sid,
                    'input': [{'type': 'text', 'text': request['prompt']}], 'effort': 'low', 'model': MODEL}})
                total, calls, reports = 0, 0, 0
                for _ in range(4096):
                    message = read()
                    total += len(json.dumps(message).encode())
                    if total > 8*1024*1024:
                        raise ValueError('Codex event limit exceeded')
                    method = message.get('method')
                    if 'id' in message and method:
                        p = message.get('params', {})
                        if (method != 'item/tool/call' or p.get('tool') != 'worker'
                                or p.get('namespace') is not None or p.get('threadId') != sid
                                or calls >= 128):
                            raise ValueError('Codex requested a tool outside the profile')
                        calls += 1
                        reply = self.rpc({'jsonrpc': '2.0', 'id': calls+10, 'method': 'tools/call',
                                          'params': {'name': 'worker', 'arguments': p['arguments']}})
                        content = reply.get('result', {})
                        if 'error' in reply:
                            raise ValueError('worker tool failed')
                        send({'id': message['id'], 'result': {'success': not content.get('isError', False),
                              'contentItems': [{'type': 'inputText', 'text': json.dumps(content)}]}})
                        continue
                    self.emit(type='event', event=message)
                    if method == 'thread/tokenUsage/updated':
                        reports += 1
                        if reports >= request['max_turns']:
                            # Each reported model request counts against this profile's
                            # soft request bound. No implicit repair/retry is permitted.
                            raise ValueError('Codex model request admission limit reached')
                    if method == 'turn/completed' or method == 'error' or 'error' in message and 'id' in message:
                        if fake and (fake.rejected or fake.requests < 2 or not fake.tool_result_seen):
                            raise ValueError('native Codex replay did not exercise the worker')
                        self.emit(type='exit', code=0 if method == 'turn/completed' else 1)
                        break
                else:
                    raise ValueError('Codex event count exceeded')
            finally:
                if proc.poll() is None:
                    proc.terminate()
                try:
                    proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    proc.kill()
                    proc.wait(timeout=5)
        if fake:
            fake.shutdown()


def fake_provider():
    """Network-disabled local protocol fixture, with no credentials or inference."""
    class Replay(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            self.send_response(404)
            self.end_headers()

        def do_POST(self):
            length = int(self.headers.get('Content-Length', 0))
            if not 0 < length <= 2*1024*1024:
                self.send_error(400)
                return
            body = json.loads(self.rfile.read(length))
            self.server.requests += 1
            specs = list(body.get('tools', []))
            for item in body.get('input', []):
                if item.get('type') == 'additional_tools':
                    specs.extend(item.get('tools', []))
            names = []
            for tool in specs:
                if tool.get('type') == 'namespace' and tool.get('name') == 'functions':
                    names.extend(t.get('name') for t in tool.get('tools', []))
                else:
                    names.append(tool.get('name'))
            if names != ['worker'] or body.get('model') != MODEL or body.get('reasoning', {}).get('effort') != 'low':
                self.server.rejected = True
                data = json.dumps({'error': {'message': 'Offline profile rejected tools '+json.dumps(names)
                    +' model '+str(body.get('model'))+' keys '+json.dumps(list(body))}}).encode()
                self.send_response(400)
                self.send_header('Content-Type', 'application/json')
                self.send_header('Content-Length', str(len(data)))
                self.end_headers()
                self.wfile.write(data)
                return
            called = any(i.get('type') == 'function_call_output' and i.get('call_id') == self.server.prefix+'call-1'
                         for i in body.get('input', []))
            self.server.tool_result_seen |= called
            count = self.server.requests
            output = ({'id': self.server.prefix+'call-item-'+str(count), 'type': 'function_call', 'name': 'worker',
                       'call_id': self.server.prefix+'call-'+str(count), 'arguments': json.dumps({
                           'command': 'printf codex-transport-proof > /work/codex-transport.txt', 'seconds': 5})}
                      if not called else {'id': self.server.prefix+'message-'+str(count), 'type': 'message', 'role': 'assistant',
                          'status': 'completed', 'content': [{'type': 'output_text', 'text': 'Offline proof complete.', 'annotations': []}]})
            response = {'id': self.server.prefix+'resp-'+str(count), 'object': 'response', 'model': MODEL,
                        'status': 'completed', 'output': [output], 'usage': {
                            'input_tokens': 20, 'input_tokens_details': {'cached_tokens': 5},
                            'output_tokens': 5, 'output_tokens_details': {'reasoning_tokens': 0}, 'total_tokens': 25}}
            events = [{'type': 'response.created', 'response': {**response, 'status': 'in_progress', 'output': []}},
                      {'type': 'response.output_item.added', 'output_index': 0, 'item': output},
                      {'type': 'response.output_item.done', 'output_index': 0, 'item': output},
                      {'type': 'response.completed', 'response': response}]
            self.send_response(200)
            self.send_header('Content-Type', 'text/event-stream')
            self.end_headers()
            for event in events:
                self.wfile.write(('data: '+json.dumps(event)+'\n\n').encode())
                self.wfile.flush()

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Replay)
    server.daemon_threads = True
    server.requests, server.tool_result_seen, server.rejected = 0, False, False
    server.prefix = 'offline-'+os.urandom(8).hex()+'-'
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def probe():
    denied = False
    try:
        socket.create_connection(('1.1.1.1', 443), timeout=1).close()
    except OSError:
        denied = True
    print(json.dumps({'version': subprocess.check_output([CLI, '--version'], timeout=15,
                      stderr=subprocess.DEVNULL, env=environment('')).decode().strip(), 'uid': os.getuid(),
        'direct_network_denied': denied, 'bridge_sha256': hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        'transport_sha256': hashlib.sha256(Path(__file__).with_name('turnrun_native_bridge.py').read_bytes()).hexdigest(),
        'binary_sha256': hashlib.sha256(Path(CLI).read_bytes()).hexdigest(),
        'owner_home_absent': not Path('/Users').exists(), 'docker_socket_absent': not Path('/var/run/docker.sock').exists(),
        'managed_settings_absent': not Path('/etc/codex').exists(), 'task_workspace_empty': not any(Path('/work').iterdir())}))


if __name__ == '__main__':
    if sys.argv[1:] == ['--probe']:
        probe()
    else:
        bridge = CodexBridge()
        try:
            line = sys.stdin.buffer.readline(FRAME_BYTES+1)
            if len(line) > FRAME_BYTES or not line.endswith(b'\n'):
                raise ValueError('invalid startup frame')
            request = json.loads(line)
            threading.Thread(target=bridge.receive, daemon=True).start()
            bridge.run(request, offline=sys.argv[1:] == ['--exercise-session'])
        except Exception:
            bridge.emit(type='fault', reason='Codex bridge failed; usage may be unknown')
            sys.exit(1)
