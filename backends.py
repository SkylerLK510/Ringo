"""Ringo backend commands, isolated subprocess execution and strict reply-object parsing.

Extracted from the original design council runtime; no council scoring or CLI dependency.
"""
import contextlib
import errno
import functools
import json
import math
import os
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

try:
    import fcntl
except ImportError:  # Windows: file_lock uses msvcrt instead
    fcntl = None
    import msvcrt

JUDGE_PRESETS = {
    'claude': ('claude -p --setting-sources project --strict-mcp-config --disable-slash-commands --tools "" '
               '--no-session-persistence --model sonnet'),
    'codex': ('codex exec --ignore-user-config --ignore-rules --disable apps --disable plugins '
              '''-c 'web_search="disabled"' -c 'model_reasoning_effort="high"' --skip-git-repo-check --ephemeral '''
              '-s read-only --color never --json -'),
    'codex-chatgpt': ('codex exec --ignore-user-config --ignore-rules --disable apps --disable plugins '
                     '--disable hooks --disable shell_tool --disable unified_exec --disable skill_search '
                     '--enable skip_host_skill_discovery --disable unbounded_connection_retries '
                     # Ringo's calls are single tool-free turns: drop the agent scaffolding Codex adds to each one
                     '--disable sleep_tool --disable goals --disable view_image --disable image_generation '
                     '--disable default_mode_request_user_input -c include_environment_context=false '
                     '-c include_permissions_instructions=false -c skills.include_instructions=false '
                     '-c include_collaboration_mode_instructions=false '
                     '''-c 'project_doc_max_bytes=0' -c 'forced_login_method="chatgpt"' '''
                     '''-c 'model_provider="openai"' -c 'web_search="disabled"' '''
                     '--skip-git-repo-check --ephemeral -s read-only --color never --json -'),
}
RAW_PEEK = 1_000_000  # bytes of a running call's output read for its soft-limit checks
# Replaces Codex's agent prompt (about 22 KB) for lean calls; Ringo's own instructions travel in the message.
CODEX_INSTRUCTIONS = 'Follow the instructions in the user message exactly.\n'
# Catalog fields that attach the code-mode `exec` tool, the multi-agent tools and their instructions to every
# request; no config flag removes them in codex-cli 0.164.
CATALOG_TOOL_FIELDS = ('tool_mode', 'multi_agent_version', 'multi_agent_reasoning_effort', 'apply_patch_tool_type')


class Stopped(subprocess.TimeoutExpired):
    """A call stopped on the person's request after it passed its soft limit. It is a TimeoutExpired, so the
    one-retry rule applies; a call killed at its hard limit raises a plain TimeoutExpired with .limit set."""


class JudgeError(Exception):
    """The judge could not answer. Carries a message that is safe to store, and `fatal`
    when no later request can succeed either (a refused key), so the run stops."""

    def __init__(self, message, fatal=False):
        super().__init__(message)
        self.fatal = fatal


@contextlib.contextmanager
def file_lock(f, blocking=True):
    """Hold an exclusive lock on the open file f (a descriptor or file object) against every other process and open
    file that locks the same path. POSIX: flock. Windows: a msvcrt lock on the file's first byte, retried until it is
    free. With blocking=False a lock that is already held raises BlockingIOError."""
    fd = f if isinstance(f, int) else f.fileno()
    if fcntl:
        fcntl.flock(fd, fcntl.LOCK_EX | (0 if blocking else fcntl.LOCK_NB))
        try:
            yield
        finally:
            fcntl.flock(fd, fcntl.LOCK_UN)
        return
    while True:
        os.lseek(fd, 0, os.SEEK_SET)
        try:
            msvcrt.locking(fd, msvcrt.LK_NBLCK, 1)
            break
        except OSError as exc:
            if exc.errno != errno.EACCES:
                raise
            if not blocking:
                raise BlockingIOError(errno.EAGAIN, 'the lock is held by another process') from None
        time.sleep(0.05)  # ceiling: polls every 50 ms; LockFileEx through ctypes would wait without polling
    try:
        yield
    finally:
        os.lseek(fd, 0, os.SEEK_SET)
        msvcrt.locking(fd, msvcrt.LK_UNLCK, 1)


@functools.cache
def _win32():
    """kernel32 and ntdll with the signatures Ringo calls (Windows only)."""
    import ctypes
    from ctypes import wintypes
    k32, nt = ctypes.WinDLL('kernel32', use_last_error=True), ctypes.WinDLL('ntdll')
    for name, res, args in (
            ('CreateJobObjectW', wintypes.HANDLE, (wintypes.LPVOID, wintypes.LPCWSTR)),
            ('OpenProcess', wintypes.HANDLE, (wintypes.DWORD, wintypes.BOOL, wintypes.DWORD)),
            ('AssignProcessToJobObject', wintypes.BOOL, (wintypes.HANDLE, wintypes.HANDLE)),
            ('TerminateJobObject', wintypes.BOOL, (wintypes.HANDLE, wintypes.UINT)),
            ('QueryInformationJobObject', wintypes.BOOL,
             (wintypes.HANDLE, ctypes.c_int, wintypes.LPVOID, wintypes.DWORD, wintypes.LPVOID)),
            ('CloseHandle', wintypes.BOOL, (wintypes.HANDLE,))):
        fn = getattr(k32, name)
        fn.restype, fn.argtypes = res, args
    nt.NtResumeProcess.restype, nt.NtResumeProcess.argtypes = ctypes.c_long, (wintypes.HANDLE,)

    class Accounting(ctypes.Structure):  # JOBOBJECT_BASIC_ACCOUNTING_INFORMATION
        _fields_ = [('times', ctypes.c_int64 * 4), ('page_faults', wintypes.DWORD), ('total', wintypes.DWORD),
                    ('active', wintypes.DWORD), ('terminated', wintypes.DWORD)]
    return ctypes, k32, nt, Accounting


def spawn(argv, **kw):
    """subprocess.Popen(argv, **kw) in a process tree of its own, which kill_tree ends with everything it started.
    POSIX: a new session. Windows: a new process group inside a job object; the command starts suspended and runs
    only once it is in the job, so every process it starts is in the job too. A bare name is not looked up with
    PATHEXT: a .cmd or .bat file would get its arguments through cmd.exe, which Popen does not quote for."""
    if os.name != 'nt':
        return subprocess.Popen(argv, start_new_session=True, **kw)
    ctypes, k32, nt, _ = _win32()
    job = k32.CreateJobObjectW(None, None)
    if not job:
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        proc = subprocess.Popen(argv, creationflags=subprocess.CREATE_NEW_PROCESS_GROUP | 0x4, **kw)  # CREATE_SUSPENDED
    except BaseException:
        k32.CloseHandle(job)
        raise
    proc.ringo_job = job
    # PROCESS_TERMINATE | PROCESS_SET_QUOTA (to join a job) | PROCESS_SUSPEND_RESUME
    handle = k32.OpenProcess(0x0001 | 0x0100 | 0x0800, False, proc.pid)
    try:
        if not handle or not k32.AssignProcessToJobObject(job, handle):
            error = ctypes.WinError(ctypes.get_last_error())
        else:  # NtResumeProcess (also what psutil's resume() calls): Popen keeps no handle to the main thread
            status = nt.NtResumeProcess(handle)
            error = OSError(f'cannot resume the command (NTSTATUS {status & 0xFFFFFFFF:#x})') if status else None
    finally:
        if handle:
            k32.CloseHandle(handle)
    if error:
        try:
            kill_tree(proc)
        finally:
            proc.kill()  # it may never have joined the job
            proc.wait()
        raise error
    return proc


def kill_tree(proc, settle_s=10):
    """Kill proc and everything it started (see spawn), without reaping proc. POSIX: SIGKILL to its process group,
    raising as os.killpg does. Windows: terminate its job and wait up to settle_s for the job to empty, so no
    process it held still has a file open when the caller removes them; a process spawn did not start is killed
    alone."""
    if os.name != 'nt':
        os.killpg(proc.pid, signal.SIGKILL)
        return
    job = getattr(proc, 'ringo_job', None)
    if job is None:
        proc.kill()
        return
    ctypes, k32, _, Accounting = _win32()
    try:
        if not k32.TerminateJobObject(job, 1):
            raise ctypes.WinError(ctypes.get_last_error())
        info, deadline = Accounting(), time.monotonic() + settle_s
        while True:  # ceiling: polls every 10 ms; a completion port would report the empty job at once
            if not k32.QueryInformationJobObject(job, 1, ctypes.byref(info), ctypes.sizeof(info), None):
                raise ctypes.WinError(ctypes.get_last_error())
            if not info.active or time.monotonic() >= deadline:
                break
            time.sleep(0.01)
    finally:
        proc.ringo_job = None
        k32.CloseHandle(job)


def _no_constant(name):
    raise ValueError(f'{name} is not a JSON number')


def _finite_float(text):
    value = float(text)
    if not math.isfinite(value):
        raise ValueError(f'{text} is not a finite number')
    return value


class _Object(dict):
    repeats_a_key = False


def _object(pairs):
    obj = _Object(pairs)
    obj.repeats_a_key = len(obj) < len(pairs)
    return obj


# Where an object can start under json's own grammar: '{', its whitespace, then a key or '}'. Any other '{' fails
# at once, but building that failure's JSONDecodeError counts lines up to it, so prose full of braces cost time
# quadratic in its length.
OBJECT_START = re.compile(r'\{[ \t\n\r]*["}]')
FAILED_PARSE = 64      # failed attempts may parse this many times the text's length in all
FAILED_STARTS = 1_000  # and number at most this many; each builds an error message that counts lines up to it


def top_level_objects(text):
    """Every top-level JSON object in text, bare or inside prose or a code fence. An object nested in another is
    never yielded on its own, and each object records whether it repeats a key (repeats_a_key). Linear in the
    text's length: a '{' that cannot start an object is skipped without parsing, nesting too deep to parse is a
    failed attempt, not an error, and failed attempts are bounded in the distance they parse (FAILED_PARSE times
    the length) and in number (FAILED_STARTS). Past either bound the scan ends, so a long unterminated reply
    costs linear time instead of quadratic, and objects after that point are not found."""
    refused = []  # NaN, Infinity or a number that overflows: noted, so the attempt still ends at a known position
    def constant(name):
        refused.append(name)
        return 0.0
    def number(token):
        value = float(token)
        if not math.isfinite(value):
            refused.append(token)
        return value
    decoder = json.JSONDecoder(parse_constant=constant, parse_float=number, object_pairs_hook=_object)
    parse, starts, start = FAILED_PARSE * (len(text) + 1), FAILED_STARTS, 0
    while (m := OBJECT_START.search(text, start)):
        at = m.start()
        refused.clear()
        try:
            obj, end = decoder.raw_decode(text, at)
        except json.JSONDecodeError as exc:
            parse -= exc.pos - at
        except RecursionError:
            parse -= len(text) - at
        else:
            if not refused:
                start = end  # skip the object's inside: a nested object is never the answer
                if isinstance(obj, dict):
                    yield obj
                continue
            parse -= end - at  # it parsed, but a refused number makes it no answer
        starts -= 1
        if parse < 0 or starts < 0:
            return
        start = at + 1


def _owner_auth():
    """The login file a Codex child would otherwise share: $CODEX_HOME/auth.json, else ~/.codex/auth.json."""
    return Path(os.environ.get('CODEX_HOME') or Path.home() / '.codex') / 'auth.json'


def _codex_home(root, env):
    """Give a Codex child a private CODEX_HOME holding only a copy of the owner's login, and an empty HOME, both
    inside the call's temporary folder. Flags such as --ignore-user-config do not stop the CLI from loading the
    global $CODEX_HOME/AGENTS.md, ~/.codex/skills, its memories or ~/.agents/skills; a fresh home does. Returns
    (owner path, snapshot bytes or None, private copy path) for the write-back after the call."""
    home, private = root / 'home', root / 'codex'
    # Windows ignores these modes; the folders keep the access list of %TEMP%, which is the user's own.
    home.mkdir(mode=0o700)
    private.mkdir(mode=0o700)
    owner = _owner_auth()
    try:
        snapshot = owner.read_bytes()
    except OSError:  # no login to copy: the call runs without one and fails the normal way
        snapshot = None
    copy = private / 'auth.json'
    if snapshot is not None:
        fd = os.open(copy, os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, 'O_NOFOLLOW', 0), 0o600)
        with os.fdopen(fd, 'wb') as f:
            f.write(snapshot)
    # The owner's model catalog cache: without it every call's `codex debug models` (see _lean_codex) fetches the
    # same catalog over the network, about 0.5-1 s; with it, about 15 ms. It holds no instructions. Not copied back.
    try:
        catalog = (owner.parent / 'models_cache.json').read_bytes()
    except OSError:
        catalog = None
    if catalog is not None:
        fd = os.open(private / 'models_cache.json', os.O_WRONLY | os.O_CREAT | os.O_EXCL | getattr(os, 'O_NOFOLLOW', 0), 0o600)
        with os.fdopen(fd, 'wb') as f:
            f.write(catalog)
    env.update(HOME=str(home), CODEX_HOME=str(private))
    if os.name == 'nt':  # Windows programs find the home folder in USERPROFILE, not HOME
        env['USERPROFILE'] = str(home)
    return owner, snapshot, copy


def _lean_codex(argv, private, env, timeout_s=15):
    """argv with Codex's agent prompt replaced by CODEX_INSTRUCTIONS and, when the installed CLI can print its
    model catalog, a copy without the tool modes, inserted before the trailing stdin marker. Both files live
    in the call's private CODEX_HOME. A CLI without `debug models`, or a catalog in another format, keeps only
    the instructions override."""
    instructions = private / 'ringo-instructions.md'
    instructions.write_text(CODEX_INSTRUCTIONS, encoding='utf-8', newline='')  # LF on Windows too
    extra = ['-c', f'model_instructions_file={json.dumps(str(instructions))}']
    try:
        listed = subprocess.run([argv[0], 'debug', 'models'], env=env, cwd=private, stdin=subprocess.DEVNULL,
                                capture_output=True, timeout=timeout_s, check=True).stdout
        catalog = json.loads(listed)
        for model in catalog['models']:
            for field in CATALOG_TOOL_FIELDS:
                model.pop(field, None)
            model['experimental_supported_tools'] = []
        path = private / 'ringo-catalog.json'
        path.write_text(json.dumps(catalog), encoding='utf-8')
        extra += ['-c', f'model_catalog_json={json.dumps(str(path))}']
    except (OSError, subprocess.SubprocessError, ValueError, KeyError, TypeError, AttributeError):
        pass
    at = len(argv) - 1 if argv[-1:] == ['-'] else len(argv)
    return [*argv[:at], *extra, *argv[at:]]


@contextlib.contextmanager
def _owner_lock(owner):
    """Serializes write-backs between Ringo processes. The owner's own Codex does not take it, so a login it
    saves in the instant between the final comparison and the rename can be overwritten; only Codex itself could
    close that window."""
    fd = os.open(owner.parent / '.ringo-auth.lock', os.O_RDWR | os.O_CREAT | getattr(os, 'O_NOFOLLOW', 0), 0o600)
    try:
        with file_lock(fd):
            yield
    finally:
        os.close(fd)


def _sync_codex_auth(owner, snapshot, copy):
    """Copy a login the child refreshed back to the owner, so the owner is not left holding a rotated refresh
    token. Compare-and-swap: only when the owner's file still equals the snapshot copied in; if the owner
    changed it meanwhile (another session refreshed or logged in), theirs is kept. The write is atomic (temp
    file in the same folder, then os.replace), mode 0600, and never goes through a symlink."""
    if snapshot is None:
        return
    try:
        new = copy.read_bytes()
    except OSError:
        return
    if new == snapshot:
        return
    try:
        json.loads(new)
    except ValueError:  # half-written or garbage: never hand it to the owner
        return
    try:
        with _owner_lock(owner):
            if owner.is_symlink():
                raise OSError(f'{owner} is a symlink')
            if owner.read_bytes() != snapshot:
                return
            fd, tmp = tempfile.mkstemp(prefix='.ringo-auth-', dir=owner.parent)  # created 0600
            try:
                with os.fdopen(fd, 'wb') as f:
                    f.write(new)
                    f.flush()
                    os.fsync(f.fileno())
                os.replace(tmp, owner)
            except BaseException:
                with contextlib.suppress(OSError):
                    os.unlink(tmp)
                raise
    except OSError as exc:
        print(f'ringo: Codex refreshed its login but it could not be saved to {owner} ({exc}); '
              'run `codex login` if later calls are refused', file=sys.stderr)


def _wait(proc, timeout_s, watch, snapshot, started, threads):
    """How the wait ended: None (the command exited), 'limit' or 'stop'. Times count from started, setup
    included. Without a watch, timeout_s is a hard limit. With one it is soft: past it the command keeps running,
    watch.soft(elapsed, snapshot) runs once in a thread (appended to threads, for the caller to join), and the
    command is stopped when watch.stop is set or at watch.limit_s."""
    limit = watch.limit_s if watch else timeout_s
    fired, exited = False, threading.Event()
    # A blocking wait in a thread wakes the moment the command exits; Popen.wait(timeout) would poll with sleeps
    # of up to 50 ms and add that to every call.
    threading.Thread(target=lambda: (proc.wait(), exited.set()), daemon=True).start()
    while True:
        elapsed = time.monotonic() - started
        if elapsed >= limit:
            return 'limit'
        if watch and watch.stop.is_set():
            return 'stop'
        if watch and not fired and elapsed >= timeout_s:
            fired = True
            threads.append(threading.Thread(target=watch.soft, args=(elapsed, snapshot), daemon=True))
            threads[-1].start()
        if exited.wait(limit - elapsed if watch is None else min(0.25, limit - elapsed)):
            return None


def _spool(folder, name):
    """A file a command writes to (or reads its stdin from) that _peek can read while the command runs. POSIX: an
    anonymous temporary file. Windows has no os.pread, and the command's handle shares the file position with
    ours, so the file is named, in folder, for _peek to open a handle of its own."""
    return tempfile.TemporaryFile() if hasattr(os, 'pread') else open(Path(folder) / name, 'w+b')


def _peek(f):
    """The first RAW_PEEK bytes of a _spool file, without moving the position the command writes at."""
    if hasattr(os, 'pread'):
        return os.pread(f.fileno(), RAW_PEEK, 0)
    with open(f.name, 'rb') as own:
        return own.read(RAW_PEEK)


def run_isolated(argv, prompt, timeout_s, *, subscription_auth=False, codex_home=False, lean=False, watch=None):
    """stdout of one run. The working folder is empty, so no project instructions or files
    load from it; it is not a file-system boundary. The command gets the caller's
    environment without the TypeSafe key, in a process tree of its own (see spawn), so a timeout kills
    it and everything it started, not only the direct child. codex_home: the command is a Codex CLI, so it
    gets a private CODEX_HOME and HOME (see _codex_home); a refreshed login is written back even
    when the call fails or times out. lean (with codex_home): the call is a single tool-free Codex turn, so
    Codex's agent prompt and tool modes are replaced (see _lean_codex). watch: timeout_s becomes a soft limit
    (see _wait); the call is never killed at it. A failure (JudgeError), a stop (Stopped) or a timeout
    (TimeoutExpired, .limit True) carries the stdout written so far as .stdout."""
    # ceiling: stdout is read whole; a command that floods it fills disk and memory
    env = {k: v for k, v in os.environ.items() if k != 'TYPESAFE_API_KEY'}
    if subscription_auth:
        env = {k: v for k, v in env.items() if not k.endswith('API_KEY') and k not in {
            'OPENAI_BASE_URL', 'OPENAI_API_BASE', 'OPENAI_ACCESS_TOKEN', 'ANTHROPIC_AUTH_TOKEN',
            'OPENAI_IDENTITY_TOKEN_FILE', 'OPENAI_WORKLOAD_IDENTITY_PROVIDER_ID'}}
    # Output goes to files, not pipes, so the wait ends when the command exits, even if something it
    # started still holds stdout open; whatever it started is then killed with its session.
    # stdin is a file too, so a long wait is plain slices of proc.wait() with nothing left to write.
    with tempfile.TemporaryDirectory(prefix='council-judge-') as tmp, _spool(tmp, 'out') as out, \
            _spool(tmp, 'err') as err, _spool(tmp, 'stdin') as stdin:
        stdin.write(prompt.encode('utf-8'))
        stdin.seek(0)
        cwd = Path(tmp) / 'work'
        cwd.mkdir()
        started, ended, threads = time.monotonic(), None, []  # the limit counts setup too
        limit = watch.limit_s if watch else timeout_s
        login = _codex_home(Path(tmp), env) if codex_home else None
        snapshot = lambda: tuple(_peek(f).decode('utf-8', 'replace') for f in (out, err))
        try:
            if codex_home and lean:
                argv = _lean_codex(argv, Path(tmp) / 'codex', env, max(0.1, min(15, limit)))
            if time.monotonic() - started >= limit:
                ended = 'limit'  # setup used the whole window: the model is never launched
            else:
                proc = spawn(argv, stdin=stdin, stdout=out, stderr=err, cwd=cwd, env=env)
                try:
                    ended = _wait(proc, timeout_s, watch, snapshot, started, threads)
                finally:
                    try:
                        kill_tree(proc)
                    except (ProcessLookupError, PermissionError):
                        pass
                    proc.wait()
        finally:
            if watch:
                watch.done.set()
            for thread in threads:  # its probe stops on done, and is recorded, before this call returns
                thread.join()
            if login:
                _sync_codex_auth(*login)
        out.seek(0)
        err.seek(0)
        stdout, stderr = out.read().decode('utf-8', 'replace'), err.read().decode('utf-8', 'replace')
    if os.name == 'nt':  # a Windows program's text-mode output ends lines with CRLF: read the LF text it printed
        stdout = stdout.replace('\r\n', '\n')
    if ended:
        exc = (Stopped if ended == 'stop' else subprocess.TimeoutExpired)(argv, round(time.monotonic() - started, 1))
        exc.limit = ended == 'limit'
        exc.stdout = stdout  # the events before the kill: what the call cost, and why it hung
        raise exc
    if proc.returncode:  # proc exists: without a launch, ended is 'limit' and the call raised above
        tail = ' '.join(stderr.split())[-300:]
        exc = JudgeError(f'{Path(argv[0]).name} exited with {proc.returncode}' + (f': {tail}' if tail else ''))
        exc.stdout = stdout  # a CLI can report on stdout what the failed call cost
        raise exc
    return stdout
