"""Offline contracts for the extracted backend runtime."""
import json
import os
import subprocess
import sys
import unittest
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

    def test_timeout_stops_the_child(self):
        with self.assertRaises(subprocess.TimeoutExpired):
            backends.run_isolated([sys.executable, '-c', 'import time; time.sleep(10)'], '', 0.1)

    def test_json_object_parser_keeps_ambiguity_and_duplicate_evidence(self):
        objects = list(backends.top_level_objects('prefix {"x":{"nested":1}} {"x":1,"x":2} suffix'))
        self.assertEqual(len(objects), 2)
        self.assertFalse(objects[0].repeats_a_key)
        self.assertTrue(objects[1].repeats_a_key)
        self.assertEqual(list(backends.top_level_objects('{"x":NaN}')), [])
        self.assertEqual(list(backends.top_level_objects('{"x":1e400}')), [])


if __name__ == '__main__':
    unittest.main()
