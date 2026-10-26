"""Live-binary canary: what the installed Codex CLI adds to each lean Ringo call.

Offline. The CLI is pointed at a local capture server that answers every request with HTTP 400, so no
provider is reached and no tokens are spent. Skipped when no `codex` is installed. It fails when a Codex
upgrade starts sending its agent prompt or tool schemas again, before that shows up as token bloat.
"""
import http.server
import json
import os
import shlex
import shutil
import tempfile
import threading
import unittest
from pathlib import Path
from unittest import mock

import backends

# Measured with codex-cli 0.164.0 on this prompt: the previous preset added 52,859 bytes; the lean one 4,146.
OVERHEAD_LIMIT_BYTES = 6_000


@unittest.skipUnless(shutil.which('codex'), 'needs the codex CLI')
class CodexOverheadCanary(unittest.TestCase):
    def capture(self, prompt):
        bodies = []

        class Capture(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                bodies.append(self.rfile.read(int(self.headers.get('content-length') or 0)))
                self.send_response(400)
                self.send_header('content-type', 'application/json')
                self.end_headers()
                self.wfile.write(b'{"error":{"message":"capture only","type":"invalid_request_error"}}')

            def do_GET(self):
                self.send_response(404)
                self.end_headers()

            def log_message(self, *args):
                pass

        server = http.server.HTTPServer(('127.0.0.1', 0), Capture)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        self.addCleanup(server.server_close)
        self.addCleanup(server.shutdown)
        argv = shlex.split(backends.JUDGE_PRESETS['codex-chatgpt'])
        for pinned in ('forced_login_method="chatgpt"', 'model_provider="openai"'):  # the login stays out of reach
            k = argv.index(pinned)
            del argv[k - 1:k + 1]
        provider = {'name': '"capture"', 'base_url': f'"http://127.0.0.1:{server.server_port}/v1"',
                    'wire_api': '"responses"', 'env_key': '"RINGO_CAPTURE_KEY"',
                    'request_max_retries': '0', 'stream_max_retries': '0'}
        extra = [x for k, v in provider.items() for x in ('-c', f'model_providers.capture.{k}={v}')]
        argv[-1:-1] = [*extra, '-c', 'model_provider="capture"']
        with tempfile.TemporaryDirectory() as d, \
                mock.patch.dict(os.environ, {'HOME': d, 'CODEX_HOME': str(Path(d, 'codex')), 'RINGO_CAPTURE_KEY': 'x'}):
            with self.assertRaises(backends.JudgeError):  # the capture server refuses the turn
                backends.run_isolated(argv, prompt, 60, codex_home=True, lean=True)
        self.assertEqual(len(bodies), 1)
        return bodies[0].decode('utf-8')

    def test_lean_preset_sends_little_beyond_the_ringo_prompt(self):
        prompt = 'Reply with the single word OK.'
        body = self.capture(prompt)
        self.assertIn(json.dumps(prompt), body)
        self.assertLess(len(body.encode()) - len(json.dumps(prompt).encode()), OVERHEAD_LIMIT_BYTES)
        for scaffolding in ('You are Codex', 'spawn_agent', '<skills_instructions>', '<environment_context>',
                            '<collaboration_mode>', '"name": "exec"', '"name":"exec"'):
            self.assertNotIn(scaffolding, body)


if __name__ == '__main__':
    unittest.main()
