"""Archive attacks plus actual Docker containment tests (opt in with RINGO_TEST_IMAGE).
The image must contain python3, /bin/sh and tar. No provider calls or credentials are used.
"""
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest

import turnrun_isolation as isolation


class ArchiveTest(unittest.TestCase):
    def test_rejects_links_traversal_special_files_duplicates_and_oversize(self):
        with tempfile.TemporaryDirectory() as temp:
            for index, attack in enumerate(('../escape', '/absolute', 'C:/escape', 'back\\slash', 'symlink', 'hardlink', 'device', 'duplicate')):
                with self.subTest(attack=attack):
                    buf = io.BytesIO()
                    with tarfile.open(fileobj=buf, mode='w') as tar:
                        entry = tarfile.TarInfo(attack)
                        if attack == 'symlink':
                            entry.type, entry.linkname = tarfile.SYMTYPE, '/tmp/elsewhere'
                        elif attack == 'hardlink':
                            entry.type, entry.linkname = tarfile.LNKTYPE, '../elsewhere'
                        elif attack == 'device':
                            entry.type = tarfile.CHRTYPE
                        tar.addfile(entry)
                        if attack == 'duplicate':
                            tar.addfile(entry)
                    dest = Path(temp) / f'out-{index}'
                    with self.assertRaises(ValueError):
                        isolation.extract_submission(buf.getvalue(), dest)
                    self.assertFalse(dest.exists())

    def test_export_removes_git_metadata_and_preserves_executable_files(self):
        with tempfile.TemporaryDirectory() as temp:
            src = Path(temp) / 'source'
            (src / '.git').mkdir(parents=True)
            (src / '.git/config').write_text('[core]\nfsmonitor=malicious\n')
            (src / 'run.sh').write_text('#!/bin/sh\nexit 0\n')
            (src / 'run.sh').chmod(0o755)
            dest = isolation.extract_submission(isolation.bundle(src), Path(temp) / 'out')
            self.assertFalse((dest / '.git').exists())
            self.assertEqual((dest / 'run.sh').read_bytes(), (src / 'run.sh').read_bytes())
            if os.name != 'nt':
                self.assertTrue((dest / 'run.sh').stat().st_mode & 0o111)

    def test_requires_pinned_images(self):
        for image in ('python:latest', 'python:3.11', '--privileged'):
            with self.assertRaises(ValueError):
                isolation.DockerSandbox(image)

    def test_owner_liveness_query_is_non_destructive(self):
        self.assertTrue(isolation.process_alive(os.getpid()))


@unittest.skipUnless(os.environ.get('RINGO_TEST_IMAGE'), 'set RINGO_TEST_IMAGE for actual Docker isolation tests')
class DockerTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.image = os.environ['RINGO_TEST_IMAGE']

    def test_worker_cannot_read_host_checks_credentials_neighbors_or_access_network(self):
        canaries = []
        for name in ('owner-home', 'check-secret', 'credential', 'neighbor', 'prior-run'):
            path = self.root / name
            path.write_text('private canary')
            canaries.append(str(path))
        with isolation.DockerSandbox(self.image) as worker:
            for path in canaries:
                result = worker.execute(f'cat {path}')
                self.assertNotEqual(result['exit'], 0)
            for request in ({'command': 'true', 'seconds': 1, 'container': 'other'},
                            {'method': 'read_file', 'path': canaries[0]}):
                with self.assertRaises(ValueError):
                    worker.tool(request)
            result = worker.execute("python3 - <<'PY'\n"
                "import os, socket\n"
                "assert not any(k.endswith(('API_KEY', 'AUTH_TOKEN')) for k in os.environ)\n"
                "assert not os.path.exists('/var/run/docker.sock')\n"
                "assert not os.path.exists('/protected/check.py')\n"
                "for host in ['1.1.1.1', 'github.com', 'host.docker.internal', 'api.anthropic.com', '::1']:\n"
                " try:\n"
                "  with socket.create_connection((host, 443), timeout=1): pass\n"
                " except OSError: continue\n"
                " raise AssertionError('unexpected network access: '+host)\n"
                "print('all boundaries denied')\nPY", 15)
            self.assertEqual(result['exit'], 0, result)
            self.assertIn('all boundaries denied', result['output'])
            identity = worker.id
        self.assertNotEqual(subprocess.run(['docker', 'inspect', identity], capture_output=True).returncode, 0)

    def test_submission_and_malicious_grading_are_contained_and_do_not_persist(self):
        fixture = self.root / 'fixture'
        fixture.mkdir()
        (fixture / 'value.py').write_text('value = 1\n')
        host_marker = self.root / 'must-not-be-created'
        check = self.root / 'check.py'
        check.write_text('import sys\nsys.path.insert(0, sys.argv[1])\nfrom value import value\n'
                         'assert value == 2\nprint("protected check passed")\n')
        with isolation.DockerSandbox(self.image) as worker:
            worker.seed(fixture)
            malicious = f'from pathlib import Path\ntry:\n Path({str(host_marker)!r}).write_text("escaped")\nexcept OSError: pass\nvalue = 2\n'
            # JSON quoting is not shell quoting; use shlex for this fixed test command.
            import shlex
            command = 'python3 -c ' + shlex.quote('from pathlib import Path; Path("value.py").write_text(' + repr(malicious) + ')')
            self.assertEqual(worker.execute(command)['exit'], 0)
            submitted = worker.submission(self.root / 'submitted')
            with self.assertRaisesRegex(ValueError, 'stopped'):
                worker.execute('true')
        self.assertFalse(host_marker.exists())
        with isolation.DockerSandbox(self.image, check=check) as checker:
            result = checker.grade(submitted, 30)
            self.assertEqual(result['exit'], 0, result)
            self.assertNotEqual(checker.execute('echo changed > /protected/check.py')['exit'], 0)
        self.assertFalse(host_marker.exists(), 'submission code was never executed on host')
        self.assertIn('protected check passed', result['output'])
        with isolation.DockerSandbox(self.image) as fresh:
            self.assertNotEqual(fresh.execute('cat /work/value.py')['exit'], 0)

    def test_symlink_submission_is_rejected(self):
        with isolation.DockerSandbox(self.image) as worker:
            self.assertEqual(worker.execute('ln -s /etc/passwd stolen')['exit'], 0)
            with self.assertRaisesRegex(ValueError, 'link'):
                worker.submission(self.root / 'out')

    @unittest.skipIf(os.name == 'nt', 'Unix kill behavior; Docker lease still bounds Windows workers')
    def test_watchdog_removes_worker_after_controller_is_killed(self):
        code = ('import time\nfrom turnrun_isolation import DockerSandbox\n'
                f'with DockerSandbox({self.image!r}, lifetime=30) as w:\n'
                ' print(w.id, flush=True)\n time.sleep(30)\n')
        proc = subprocess.Popen([sys.executable, '-c', code], stdout=subprocess.PIPE, text=True)
        identity = None
        try:
            identity = proc.stdout.readline().strip()
            self.assertEqual(len(identity), 64)
            proc.kill()
            proc.wait(timeout=5)
            deadline = time.monotonic()+10
            while time.monotonic() < deadline:
                if subprocess.run(['docker', 'inspect', identity], capture_output=True).returncode:
                    break
                time.sleep(0.25)
            else:
                self.fail('worker survived controller termination')
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
            proc.stdout.close()
            if identity:
                subprocess.run(['docker', 'rm', '-f', identity], capture_output=True)


if __name__ == '__main__':
    unittest.main()
