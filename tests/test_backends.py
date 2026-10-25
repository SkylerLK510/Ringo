"""Offline contracts for the extracted backend runtime."""
import json
import os
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import backends


class BackendRuntimeTest(unittest.TestCase):
    def test_subprocess_receives_stdin_in_empty_directory_without_typesafe_key(self):
        code = ('import json, os, sys; print(json.dumps({"input":sys.stdin.read(), '
                '"entries":os.listdir(), "key":os.environ.get("TYPESAFE_API_KEY")}))')
        with mock.patch.dict(os.environ, {'TYPESAFE_API_KEY': 'not-for-the-child'}):
            result = json.loads(backends.run_isolated([sys.executable, '-c', code], 'fixture', 5))
        self.assertEqual(result, {'input': 'fixture', 'entries': [], 'key': None})

    def test_failed_command_preserves_stdout_for_usage_recovery(self):
        code = 'import sys; print("usage-fixture"); print("failed",file=sys.stderr); sys.exit(7)'
        with self.assertRaises(backends.JudgeError) as caught:
            backends.run_isolated([sys.executable, '-c', code], '', 5)
        self.assertIn('exited with 7', str(caught.exception))
        self.assertIn('usage-fixture', caught.exception.stdout)

    def test_subscription_call_scrubs_api_auth_and_endpoint_overrides(self):
        code = ('import json, os; print(json.dumps({k:os.environ.get(k) for k in '
                '["OPENAI_API_KEY","CODEX_API_KEY","OPENAI_BASE_URL","OPENAI_ACCESS_TOKEN",'
                '"OPENAI_IDENTITY_TOKEN_FILE","ANTHROPIC_AUTH_TOKEN","RINGO_TEST_MARKER"]}))')
        overrides = dict.fromkeys(['OPENAI_API_KEY', 'CODEX_API_KEY', 'OPENAI_BASE_URL',
                                  'OPENAI_ACCESS_TOKEN', 'OPENAI_IDENTITY_TOKEN_FILE', 'ANTHROPIC_AUTH_TOKEN'], 'fixture')
        with mock.patch.dict(os.environ, {**overrides, 'RINGO_TEST_MARKER': 'keep'}):
            result = json.loads(backends.run_isolated([sys.executable, '-c', code], '', 5, subscription_auth=True))
        self.assertEqual(result.pop('RINGO_TEST_MARKER'), 'keep')
        self.assertTrue(all(v is None for v in result.values()))

    def test_timeout_stops_the_child(self):
        with self.assertRaises(subprocess.TimeoutExpired):
            backends.run_isolated([sys.executable, '-c', 'import time; time.sleep(10)'], '', 0.1)

    def test_timeout_keeps_what_the_child_wrote_before_the_kill(self):
        code = 'import time; print("event-before-hang", flush=True); time.sleep(10)'
        with self.assertRaises(subprocess.TimeoutExpired) as caught:
            backends.run_isolated([sys.executable, '-c', code], '', 1)
        self.assertIn('event-before-hang', caught.exception.stdout)

    def test_codex_home_false_keeps_the_callers_home(self):
        code = 'import json, os; print(json.dumps([os.environ.get("HOME"), os.environ.get("CODEX_HOME")]))'
        with mock.patch.dict(os.environ, {'HOME': '/fixture/home', 'CODEX_HOME': '/fixture/codex'}):
            result = json.loads(backends.run_isolated([sys.executable, '-c', code], '', 5))
        self.assertEqual(result, ['/fixture/home', '/fixture/codex'])

    def test_json_object_parser_keeps_ambiguity_and_duplicate_evidence(self):
        objects = list(backends.top_level_objects('prefix {"x":{"nested":1}} {"x":1,"x":2} suffix'))
        self.assertEqual(len(objects), 2)
        self.assertFalse(objects[0].repeats_a_key)
        self.assertTrue(objects[1].repeats_a_key)
        self.assertEqual(list(backends.top_level_objects('{"x":NaN}')), [])
        self.assertEqual(list(backends.top_level_objects('{"x":1e400}')), [])


# A stand-in Codex child: reports what it can see, then optionally "refreshes" its login (argv[1]: the new
# refresh token, or "garbage"), writes over the owner's file (argv[2]) and exits with argv[3].
CHILD = """
import json, os, stat, sys
from pathlib import Path
c, h = Path(os.environ['CODEX_HOME']), Path(os.environ['HOME'])
a = c / 'auth.json'
seen = {'codex_files': sorted(os.listdir(c)), 'home_files': sorted(os.listdir(h)), 'cwd': os.listdir(),
        'auth': json.loads(a.read_text()) if a.exists() else None,
        'mode': oct(stat.S_IMODE(a.stat().st_mode)) if a.exists() else None,
        'dirs': [oct(stat.S_IMODE(p.stat().st_mode)) for p in (c, h)], 'api_key': os.environ.get('CODEX_API_KEY')}
print(json.dumps(seen))
new, owner, code = (sys.argv[1:] + ['', '', ''])[:3]
if new:
    tmp = c / 'tmp'
    tmp.write_text('{"tokens": ' if new == 'garbage' else json.dumps({'tokens': {'refresh_token': new}}))
    os.replace(tmp, a)
if owner:
    Path(owner).write_text(json.dumps({'tokens': {'refresh_token': 'owner'}}))
sys.exit(int(code or 0))
"""


class CodexHomeTest(unittest.TestCase):
    """Codex children get a private CODEX_HOME and HOME; a refreshed login goes back only when that is safe."""

    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp())
        self.home, self.owner = self.tmp / 'home', self.tmp / 'home' / '.codex'
        (self.home / '.agents' / 'skills' / 'ringo-harness').mkdir(parents=True)
        self.owner.mkdir()
        (self.owner / 'AGENTS.md').write_text('Use Ringo for everything')
        (self.owner / 'memories_1.sqlite').write_text('owner memories')
        self.auth = self.owner / 'auth.json'
        self.auth.write_text(json.dumps({'tokens': {'refresh_token': 'r1'}}))
        os.chmod(self.auth, 0o600)
        env = {k: v for k, v in os.environ.items() if k != 'CODEX_HOME'}
        patcher = mock.patch.dict(os.environ, {**env, 'HOME': str(self.home), 'CODEX_API_KEY': 'kept'}, clear=True)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.addCleanup(lambda: subprocess.run(['rm', '-rf', str(self.tmp)]))

    def run_child(self, *args, **kw):
        return json.loads(backends.run_isolated([sys.executable, '-c', CHILD, *args], '', 10, codex_home=True, **kw))

    def token(self):
        return json.loads(self.auth.read_text())['tokens']['refresh_token']

    def test_child_sees_only_a_private_login_and_a_refresh_reaches_the_owner(self):
        seen = self.run_child('r2')
        self.assertEqual(seen['codex_files'], ['auth.json'])  # no AGENTS.md, skills or memories
        self.assertEqual((seen['home_files'], seen['cwd']), ([], []))  # no ~/.agents, empty working folder
        self.assertEqual((seen['auth']['tokens']['refresh_token'], seen['mode']), ('r1', '0o600'))
        self.assertEqual(seen['dirs'], ['0o700', '0o700'])
        self.assertEqual(seen['api_key'], 'kept')  # the plain preset keeps the caller's configured auth
        self.assertEqual(self.token(), 'r2')
        self.assertEqual(stat.S_IMODE(self.auth.stat().st_mode), 0o600)
        self.assertFalse(self.auth.is_symlink())
        self.assertEqual(sorted(p.name for p in self.owner.iterdir()),
                         ['.ringo-auth.lock', 'AGENTS.md', 'auth.json', 'memories_1.sqlite'])  # no temp file left

    def test_codex_home_variable_names_the_owner(self):
        other = self.tmp / 'other'
        other.mkdir()
        (other / 'auth.json').write_text(json.dumps({'tokens': {'refresh_token': 'o1'}}))
        with mock.patch.dict(os.environ, {'CODEX_HOME': str(other)}):
            seen = self.run_child('o2')
        self.assertEqual(seen['auth']['tokens']['refresh_token'], 'o1')
        self.assertEqual(json.loads((other / 'auth.json').read_text())['tokens']['refresh_token'], 'o2')
        self.assertEqual(self.token(), 'r1')

    def test_owner_login_changed_during_the_call_wins(self):
        self.run_child('r2', str(self.auth))
        self.assertEqual(self.token(), 'owner')

    def test_refresh_is_written_back_when_the_call_fails(self):
        with self.assertRaisesRegex(backends.JudgeError, 'exited with 7'):
            self.run_child('r2', '', '7')
        self.assertEqual(self.token(), 'r2')

    def test_unchanged_or_invalid_login_never_touches_the_owner(self):
        before = self.auth.stat().st_mtime_ns
        self.run_child()
        self.run_child('garbage')
        self.assertEqual((self.token(), self.auth.stat().st_mtime_ns), ('r1', before))
        self.assertFalse((self.owner / '.ringo-auth.lock').exists())

    def test_missing_owner_login_runs_without_one(self):
        self.auth.unlink()
        seen = self.run_child('r2')
        self.assertEqual((seen['codex_files'], seen['auth']), ([], None))
        self.assertFalse(self.auth.exists())

    def test_symlinked_owner_login_is_never_written_through_or_replaced(self):
        target = self.tmp / 'vault.json'
        target.write_text(self.auth.read_text())
        self.auth.unlink()
        self.auth.symlink_to(target)
        with mock.patch('sys.stderr') as err:
            seen = self.run_child('r2')
        self.assertEqual(seen['auth']['tokens']['refresh_token'], 'r1')
        self.assertTrue(self.auth.is_symlink())
        self.assertEqual(json.loads(target.read_text())['tokens']['refresh_token'], 'r1')
        self.assertIn('could not be saved', ''.join(c.args[0] for c in err.write.call_args_list))


if __name__ == '__main__':
    unittest.main()
