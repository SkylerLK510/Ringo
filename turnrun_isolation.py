"""Disposable, network-disabled task and checker containers. No provider credentials or Docker
socket enter either container. Only the trusted controller selects images, checks and executors.

This adapter is executable without inference. It does not certify a live provider broker.
"""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import queue
import re
import shutil
import subprocess
import sys
import tarfile
import threading
import time
import uuid

MAX_BYTES = 64 * 1024 * 1024
MAX_FILES = 20000
IMAGE = re.compile(r'(?:sha256:[0-9a-f]{64}|[^\s]+@sha256:[0-9a-f]{64})')


def bounded(command, *, data=None, timeout=60, limit=MAX_BYTES):
    """Bound subprocess output before accepting it; drain stderr with stdout to avoid deadlock."""
    with subprocess.Popen(command, stdin=subprocess.PIPE if data is not None else subprocess.DEVNULL,
                          stdout=subprocess.PIPE, stderr=subprocess.STDOUT) as proc:
        chunks = queue.Queue(maxsize=4)

        def read():
            try:
                while block := proc.stdout.read(65536):
                    chunks.put(block)
            finally:
                chunks.put(None)

        thread = threading.Thread(target=read, daemon=True)
        thread.start()
        if data is not None:
            def write():
                try:
                    proc.stdin.write(data)
                    proc.stdin.close()
                except (BrokenPipeError, OSError, ValueError):
                    pass
            threading.Thread(target=write, daemon=True).start()
        output = bytearray()
        deadline = time.monotonic() + timeout
        try:
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError('container command exceeded its wall limit')
                try:
                    block = chunks.get(timeout=remaining)
                except queue.Empty:
                    raise TimeoutError('container command exceeded its wall limit') from None
                if block is None:
                    break
                output.extend(block)
                if len(output) > limit:
                    raise ValueError('container output exceeded its size limit')
            code = proc.wait(timeout=max(0.1, deadline - time.monotonic()))
            return code, bytes(output)
        finally:
            if proc.poll() is None:
                proc.kill()
                proc.wait()


def bundle(root):
    """Archive a trusted fixture without following symlinks or including special files."""
    root = Path(root).resolve(strict=True)
    out = io.BytesIO()
    count = size = 0
    with tarfile.open(fileobj=out, mode='w') as tar:
        for folder, dirs, files in os.walk(root, followlinks=False):
            for name in sorted(dirs + files):
                path = Path(folder) / name
                if path.is_symlink() or not (path.is_dir() or path.is_file()):
                    raise ValueError('fixtures may contain only ordinary files and directories')
                count += 1
                size += path.stat().st_size if path.is_file() else 0
                if count > MAX_FILES or size > MAX_BYTES:
                    raise ValueError('fixture exceeds its size limit')
                tar.add(path, arcname=path.relative_to(root).as_posix(), recursive=False)
    if out.tell() > MAX_BYTES:
        raise ValueError('fixture archive exceeds its size limit')
    return out.getvalue()


def extract_submission(data, output):
    """Accept ordinary submission files only, into a new folder. Never run Git or code on them.

    Reject links, traversal, devices, duplicate paths, excessive sizes and mutable git metadata.
    Validate the entire archive before writing any part of it.
    """
    output = Path(output)
    if output.exists():
        raise ValueError('submission destination must not exist')
    if len(data) > MAX_BYTES:
        raise ValueError('submission archive exceeds its size limit')
    entries, seen, size = [], set(), 0
    with tarfile.open(fileobj=io.BytesIO(data)) as tar:
        for entry in tar:
            path = PurePosixPath(entry.name)
            if path.is_absolute() or '..' in path.parts or '\\' in entry.name or ':' in entry.name:
                raise ValueError('unsafe submission path')
            if not path.parts:
                continue
            if any(p.casefold() == '.git' for p in path.parts):
                continue
            name = path.as_posix()
            if not (entry.isfile() or entry.isdir()) or name.casefold() in seen:
                raise ValueError('submission has a link, special file or duplicate path')
            seen.add(name.casefold())
            size += entry.size
            if len(seen) > MAX_FILES or size > MAX_BYTES or entry.size < 0:
                raise ValueError('submission exceeds its size limit')
            content = tar.extractfile(entry).read() if entry.isfile() else None
            entries.append((path, content, bool(entry.mode & 0o111)))
    # A file may not be an ancestor of another entry, regardless of archive order.
    files = {p for p, content, _ in entries if content is not None}
    if any(parent in files for p, _, _ in entries for parent in p.parents):
        raise ValueError('submission file is used as a directory')
    output.mkdir(parents=True)
    try:
        for path, content, executable in entries:
            dest = output.joinpath(*path.parts)
            if content is None:
                dest.mkdir(parents=True, exist_ok=True)
            else:
                dest.parent.mkdir(parents=True, exist_ok=True)
                dest.write_bytes(content)
                dest.chmod(0o755 if executable else 0o644)
    except BaseException:
        shutil.rmtree(output)
        raise
    return output


# The fixed worker-side supervisor limits output and kills the command's process group on timeout.
# The command string never reaches a host shell. A worker can affect only its own container.
EXEC = r'''
import json, os, signal, subprocess, sys, tempfile
command, seconds = sys.argv[1], int(sys.argv[2])
with tempfile.TemporaryFile() as out:
    p = subprocess.Popen(['/bin/sh', '-c', command], stdout=out, stderr=out, start_new_session=True)
    timed_out = False
    try:
        p.wait(timeout=seconds)
    except subprocess.TimeoutExpired:
        timed_out = True
    finally:
        try: os.killpg(p.pid, signal.SIGKILL)
        except ProcessLookupError: pass
        p.wait()
    out.seek(0)
    data = out.read(65537)
    print(json.dumps({'exit': p.returncode, 'timeout': timed_out, 'truncated': len(data)>65536,
                      'output': data[:65536].decode('utf-8', 'replace')}))
'''


class DockerSandbox:
    """One controller-owned worker. No tool parameter can select a host path or container."""
    def __init__(self, image, *, check=None, lifetime=900, docker=None):
        if not isinstance(image, str) or not IMAGE.fullmatch(image):
            raise ValueError('use an immutable image ID or repository digest')
        if type(lifetime) is not int or not 1 <= lifetime <= 3600:
            raise ValueError('container lifetime must be from 1 through 3600 seconds')
        self.image, self.check, self.lifetime = image, Path(check).resolve(strict=True) if check else None, lifetime
        if self.check and not self.check.is_file():
            raise ValueError('protected check must be an ordinary file')
        self.check_sha256 = hashlib.sha256(self.check.read_bytes()).hexdigest() if self.check else None
        if self.check and ',' in str(self.check):
            raise ValueError('check path cannot contain a comma')
        self.docker = docker or shutil.which('docker')
        if not self.docker:
            raise ValueError('Docker is unavailable')
        self.id, self.watchdog, self.stopped, self.volume = None, None, False, None

    def _call(self, args, **kwargs):
        code, out = bounded([self.docker, *args], **kwargs)
        if code:
            raise RuntimeError(f'Docker operation failed ({code}): {out[:2048].decode(errors="replace")}')
        return out

    def __enter__(self):
        name = 'ringo-rrsi-' + uuid.uuid4().hex
        self.volume = name + '-work'
        self._call(['volume', 'create', '--label', 'ringo.rrsi.disposable=true', self.volume], limit=8192)
        try:
            self._call(['run', '--rm', '--network', 'none', '--read-only', '--cap-drop', 'ALL',
                '--security-opt', 'no-new-privileges:true', '--mount', f'type=volume,source={self.volume},target=/work,volume-nocopy',
                self.image, 'python3', '-I', '-c', 'import os; os.chmod("/work", 0o777)'], limit=8192)
        except BaseException:
            self.close()
            raise
        args = ['create', '--name', name, '--label', 'ringo.rrsi.disposable=true', '--init',
                '--read-only', '--network', 'none', '--cap-drop', 'ALL', '--security-opt', 'no-new-privileges:true',
                '--pids-limit', '128', '--memory', '2g', '--cpus', '2', '--user', '65532:65532',
                '--mount', f'type=volume,source={self.volume},target=/work,volume-nocopy',
                '--tmpfs', '/tmp:rw,exec,nosuid,nodev,size=256m,mode=1777',
                '--workdir', '/work', '--env', 'HOME=/tmp/home', '--env', 'RINGO_CHILD=1']
        if self.check:
            args += ['--mount', f'type=bind,source={self.check},target=/protected/check.py,readonly']
        args += [self.image, 'python3', '-I', '-c', f'import time; time.sleep({self.lifetime})']
        try:
            self.id = self._call(args, limit=8192).decode().strip()
            if not re.fullmatch('[0-9a-f]{64}', self.id):
                raise ValueError('Docker returned an invalid container identity')
            self.watchdog = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), '_watch',
                self.docker, self.id, str(os.getpid()), str(self.lifetime), self.volume], stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=os.name != 'nt')
            self._call(['start', self.id], limit=8192)
            inspection = json.loads(self._call(['inspect', self.id], limit=1024*1024))[0]
            host = inspection['HostConfig']
            if host['NetworkMode'] != 'none' or host['Privileged'] or not host['ReadonlyRootfs']:
                raise ValueError('Docker did not establish the requested isolation')
            check_sources = {str(self.check)} if self.check else set()
            if self.check and sys.platform == 'darwin':
                check_sources.update(prefix+str(self.check) for prefix in ('/host_mnt', '/run/desktop/mnt/host'))
            if any(m['Type'] != 'tmpfs' and not (m['Type'] == 'volume' and m['Name'] == self.volume
                    and m['Destination'] == '/work') and not (self.check and m['Destination'] == '/protected/check.py'
                    and m['Type'] == 'bind' and not m['RW'] and m['Source'] in check_sources) for m in inspection['Mounts']):
                raise ValueError('unexpected container mount')
            if self.check:
                actual = self._call(['exec', self.id, 'python3', '-I', '-c',
                    'import hashlib; print(hashlib.sha256(open("/protected/check.py","rb").read()).hexdigest())'], limit=8192)
                if actual.decode().strip() != self.check_sha256:
                    raise ValueError('protected check content changed')
            return self
        except BaseException:
            self.close()
            raise

    def seed(self, fixture):
        self._call(['exec', '-i', self.id, 'tar', '--no-same-owner', '-xf', '-', '-C', '/work'],
                   data=bundle(fixture))

    def execute(self, command, seconds=30):
        if self.stopped or not self.id:
            raise ValueError('worker is stopped')
        if not isinstance(command, str) or len(command.encode()) > 65536:
            raise ValueError('command must be at most 65536 bytes')
        if type(seconds) is not int or not 1 <= seconds <= min(300, self.lifetime):
            raise ValueError('invalid command wall limit')
        return json.loads(self._call(['exec', self.id, 'python3', '-I', '-c', EXEC, command, str(seconds)],
                                     timeout=seconds + 15, limit=300000))

    def tool(self, request):
        """The entire model-visible contract. No paths, mounts, executors or Docker arguments."""
        if not isinstance(request, dict) or set(request) != {'command', 'seconds'}:
            raise ValueError('tool accepts only command and seconds')
        return self.execute(request['command'], request['seconds'])

    def submission(self, output):
        # Stop *every* task process before exporting, including detached descendants. A private
        # per-trial volume preserves the files; no task process is resumed for grading.
        self._call(['stop', '--time', '1', self.id], limit=8192)
        self.stopped = True
        data = self._call(['cp', f'{self.id}:/work/.', '-'], limit=MAX_BYTES)
        return extract_submission(data, output)

    def grade(self, fixture, seconds=300):
        if not self.check:
            raise ValueError('this is not a checker container')
        self.seed(fixture)
        return self.execute('python3 /protected/check.py /work', seconds)

    def close(self):
        try:
            if self.id:
                self._call(['rm', '--force', self.id], timeout=30, limit=8192)
        finally:
            self.id = None
            if self.volume:
                self._call(['volume', 'rm', '--force', self.volume], timeout=30, limit=8192)
                self.volume = None
            if self.watchdog:
                self.watchdog.terminate()
                self.watchdog.wait(timeout=10)

    def __exit__(self, *exc):
        self.close()


def process_alive(pid):
    if os.name == 'nt':
        # os.kill(pid, 0) terminates a process on Windows; query its exit status instead.
        import ctypes
        from ctypes import wintypes
        kernel = ctypes.WinDLL('kernel32', use_last_error=True)
        kernel.OpenProcess.restype = wintypes.HANDLE
        kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        kernel.GetExitCodeProcess.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
        kernel.CloseHandle.argtypes = [wintypes.HANDLE]
        handle = kernel.OpenProcess(0x1000, False, pid)
        if not handle:
            return False
        try:
            code = wintypes.DWORD()
            return bool(kernel.GetExitCodeProcess(handle, ctypes.byref(code))) and code.value == 259
        finally:
            kernel.CloseHandle(handle)
    try:
        os.kill(pid, 0)
        return True
    except PermissionError:
        return True
    except ProcessLookupError:
        return False


def watch(docker, container, owner, lifetime, volume):
    """Independent cleanup survives the controller being killed. Hard lease bounds PID reuse."""
    deadline = time.monotonic() + lifetime
    while time.monotonic() < deadline:
        if not process_alive(owner):
            break
        time.sleep(0.25)
    subprocess.run([docker, 'rm', '--force', container], stdout=subprocess.DEVNULL,
                   stderr=subprocess.DEVNULL, timeout=30)
    subprocess.run([docker, 'volume', 'rm', '--force', volume], stdout=subprocess.DEVNULL,
                   stderr=subprocess.DEVNULL, timeout=30)


def main():
    if len(sys.argv) == 7 and sys.argv[1] == '_watch':
        watch(sys.argv[2], sys.argv[3], int(sys.argv[4]), int(sys.argv[5]), sys.argv[6])
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', required=True)
    parser.add_argument('--check', required=True)
    parser.add_argument('--fixture', required=True)
    args = parser.parse_args()
    with DockerSandbox(args.image, check=args.check) as checker:
        print(json.dumps(checker.grade(args.fixture)))


if __name__ == '__main__':
    main()
