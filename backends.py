"""Ringo backend commands, isolated subprocess execution and strict reply-object parsing.

Extracted from the original design council runtime; no council scoring or CLI dependency.
"""
import contextlib
import errno
import functools
import json
import math
import ntpath
import os
import re
import secrets
import shutil
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
def _win_security():
    """Typed Win32 security APIs; loaded only on native Windows."""
    import ctypes
    from ctypes import wintypes as w
    kernel = ctypes.WinDLL('kernel32', use_last_error=True)
    adv = ctypes.WinDLL('advapi32', use_last_error=True)
    ptr = w.LPVOID
    for dll, name, result, args in (
            (kernel, 'GetCurrentProcess', w.HANDLE, ()),
            (kernel, 'CloseHandle', w.BOOL, (w.HANDLE,)),
            (kernel, 'LocalFree', ptr, (ptr,)),
            (kernel, 'CreateDirectoryW', w.BOOL, (w.LPCWSTR, ptr)),
            (adv, 'OpenProcessToken', w.BOOL, (w.HANDLE, w.DWORD, ctypes.POINTER(w.HANDLE))),
            (adv, 'GetTokenInformation', w.BOOL,
             (w.HANDLE, ctypes.c_int, ptr, w.DWORD, ctypes.POINTER(w.DWORD))),
            (adv, 'GetLengthSid', w.DWORD, (ptr,)),
            (adv, 'ConvertSidToStringSidW', w.BOOL, (ptr, ctypes.POINTER(w.LPWSTR))),
            (adv, 'ConvertStringSecurityDescriptorToSecurityDescriptorW', w.BOOL,
             (w.LPCWSTR, w.DWORD, ctypes.POINTER(ptr), ctypes.POINTER(w.DWORD))),
            (adv, 'InitializeAcl', w.BOOL, (ptr, w.DWORD, w.DWORD)),
            (adv, 'AddAccessAllowedAceEx', w.BOOL, (ptr, w.DWORD, w.DWORD, w.DWORD, ptr)),
            (adv, 'SetNamedSecurityInfoW', w.DWORD,
             (w.LPWSTR, ctypes.c_int, w.DWORD, ptr, ptr, ptr, ptr)),
            (adv, 'GetNamedSecurityInfoW', w.DWORD,
             (w.LPWSTR, ctypes.c_int, w.DWORD, ptr, ptr, ctypes.POINTER(ptr), ptr, ctypes.POINTER(ptr))),
            (adv, 'GetSecurityDescriptorControl', w.BOOL,
             (ptr, ctypes.POINTER(w.WORD), ctypes.POINTER(w.DWORD))),
            (adv, 'GetAclInformation', w.BOOL, (ptr, ptr, w.DWORD, ctypes.c_int)),
            (adv, 'GetAce', w.BOOL, (ptr, w.DWORD, ctypes.POINTER(ptr)))):
        fn = getattr(dll, name)
        fn.restype, fn.argtypes = result, args
    return ctypes, w, kernel, adv


def _windows_user_sid():
    c, w, kernel, adv = _win_security()
    token, size = w.HANDLE(), w.DWORD()
    if not adv.OpenProcessToken(kernel.GetCurrentProcess(), 0x0008, c.byref(token)):  # TOKEN_QUERY
        raise c.WinError(c.get_last_error())
    try:
        adv.GetTokenInformation(token, 1, None, 0, c.byref(size))  # TokenUser
        if c.get_last_error() != 122:  # ERROR_INSUFFICIENT_BUFFER
            raise c.WinError(c.get_last_error())
        user = c.create_string_buffer(size.value)
        if not adv.GetTokenInformation(token, 1, user, size, c.byref(size)):
            raise c.WinError(c.get_last_error())
        sid = c.cast(user, c.POINTER(w.LPVOID))[0]  # TOKEN_USER starts with SID_AND_ATTRIBUTES.Sid
        return c.string_at(sid, adv.GetLengthSid(sid))
    finally:
        kernel.CloseHandle(token)


def _windows_acl(path):
    """Read back (descriptor control, [(ACE type, flags, mask, SID bytes)]), never a cached assertion."""
    import struct
    c, w, kernel, adv = _win_security()
    acl, descriptor = w.LPVOID(), w.LPVOID()
    error = adv.GetNamedSecurityInfoW(str(path), 1, 4, None, None, c.byref(acl), None, c.byref(descriptor))
    if error:
        raise c.WinError(error)
    try:
        control, revision = w.WORD(), w.DWORD()
        if not adv.GetSecurityDescriptorControl(descriptor, c.byref(control), c.byref(revision)):
            raise c.WinError(c.get_last_error())
        if not acl:
            raise OSError(f'{path}: missing private DACL')
        info = (w.DWORD * 3)()  # ACL_SIZE_INFORMATION: AceCount, AclBytesInUse, AclBytesFree
        if not adv.GetAclInformation(acl, info, c.sizeof(info), 2):  # AclSizeInformation
            raise c.WinError(c.get_last_error())
        aces = []
        for i in range(info[0]):
            ace = w.LPVOID()
            if not adv.GetAce(acl, i, c.byref(ace)):
                raise c.WinError(c.get_last_error())
            kind, flags, size, mask = struct.unpack('<BBHI', c.string_at(ace, 8))
            # Only ACCESS_ALLOWED_ACE has the layout we accept. Don't interpret another ACE's data as a SID.
            sid = c.string_at(ace.value + 8, size - 8) if kind == 0 and size >= 8 else None
            aces.append((kind, flags, mask, sid))
        return control.value, aces
    finally:
        kernel.LocalFree(descriptor)


def private_directory(path, *, create=False, **mkdir):
    """Secure a private directory before writing; optionally create it with the caller's existing mkdir options.

    Windows: create atomically with a protected user-only DACL, or tighten an existing directory, and verify it.
    Earlier handles to existing directories are not revoked. This is not a boundary against the same account,
    administrators taking ownership, or pre-existing files with explicit ACLs.
    """
    if os.name != 'nt':
        if create:
            Path(path).mkdir(**mkdir)
        return
    c, w, kernel, adv = _win_security()
    sid = _windows_user_sid()

    def tighten(folder):
        acl = c.create_string_buffer(8 + 8 + len(sid))  # ACL header + ACCESS_ALLOWED_ACE header/mask + SID
        if not adv.InitializeAcl(acl, len(acl), 2) or not adv.AddAccessAllowedAceEx(acl, 2, 3, 0x1F01FF, sid):
            raise c.WinError(c.get_last_error())
        error = adv.SetNamedSecurityInfoW(str(folder), 1, 4 | 0x80000000, None, None, acl, None)
        if error:  # SE_FILE_OBJECT, DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION
            raise c.WinError(error)

    def verify(folder):
        control, aces = _windows_acl(folder)
        if not control & 0x1000 or aces != [(0, 3, 0x1F01FF, sid)]:  # SE_DACL_PROTECTED, OI | CI
            raise OSError(f'{folder}: owner-only protected DACL verification failed')

    if not create:
        tighten(path)
        verify(path)
        return

    class SecurityAttributes(c.Structure):
        _fields_ = [('nLength', w.DWORD), ('lpSecurityDescriptor', w.LPVOID), ('bInheritHandle', w.BOOL)]

    text, descriptor = w.LPWSTR(), w.LPVOID()
    try:
        if not adv.ConvertSidToStringSidW(sid, c.byref(text)):
            raise c.WinError(c.get_last_error())
        if not adv.ConvertStringSecurityDescriptorToSecurityDescriptorW(
                f'D:P(A;OICI;FA;;;{text.value})', 1, c.byref(descriptor), None):
            raise c.WinError(c.get_last_error())
        attributes = SecurityAttributes(c.sizeof(SecurityAttributes), descriptor, False)

        def make(folder, mode=0o777, parents=False, exist_ok=False, ancestor=False):
            try:
                if not kernel.CreateDirectoryW(str(folder), c.byref(attributes)):
                    raise c.WinError(c.get_last_error())
            except FileNotFoundError:
                if not parents or folder.parent == folder:
                    raise
                make(folder.parent, parents=True, exist_ok=True, ancestor=True)
                make(folder, exist_ok=exist_ok, ancestor=ancestor)
                return
            except OSError:
                if not exist_ok or not folder.is_dir():
                    raise
                if ancestor:  # an existing ancestor is the caller's folder: like Path.mkdir, leave it as it is
                    return
                tighten(folder)
            verify(folder)

        make(Path(path), **mkdir)
    finally:
        if descriptor:
            kernel.LocalFree(descriptor)
        if text:
            kernel.LocalFree(text)


@contextlib.contextmanager
def private_tempdir(prefix, dir=None, *, cleanup=True):
    """Private temporary root; cleanup=False preserves printed incident evidence like mkdtemp."""
    if os.name != 'nt':
        if cleanup:
            with tempfile.TemporaryDirectory(prefix=prefix, dir=dir) as folder:
                yield folder
        else:
            yield tempfile.mkdtemp(prefix=prefix, dir=dir)
        return
    for _ in range(tempfile.TMP_MAX):
        folder = Path(dir if dir is not None else tempfile.gettempdir()) / (prefix + secrets.token_hex(8))
        try:
            private_directory(folder, create=True)
        except FileExistsError:
            continue
        break
    else:
        raise FileExistsError(errno.EEXIST, 'no usable private temporary directory name')
    try:
        yield str(folder)
    finally:
        if cleanup:
            def retry_readonly(fn, path, exc):
                os.chmod(path, 0o700)
                fn(path)
            with contextlib.suppress(FileNotFoundError):  # release staging may have moved the directory
                shutil.rmtree(folder, onerror=retry_readonly)


def _check_windows_program(argv, executable=None):
    """Refuse batch programs altogether: CRT argument quoting cannot safely feed cmd.exe."""
    if os.name != 'nt':
        return
    if isinstance(argv, (str, bytes)):
        raise ValueError('Windows commands must use an argument list; call the underlying executable')
    program = os.fsdecode(executable if executable is not None else argv[0])
    if ntpath.splitext(program.rstrip(' ."'))[1].lower() in ('.cmd', '.bat'):
        raise ValueError(f'Windows batch program argument {program!r} is refused (arguments {list(argv[1:])!r}); '
                         'call the underlying executable instead')


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
    PATHEXT. Explicit .cmd/.bat programs are refused before Popen because it cannot quote their arguments safely."""
    if os.name != 'nt':
        return subprocess.Popen(argv, start_new_session=True, **kw)
    _check_windows_program(argv, kw.get('executable'))
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
    # Callers can supply the root; protect both homes before copying credentials.
    private_directory(home, create=True, mode=0o700)
    private_directory(private, create=True, mode=0o700)
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
    _check_windows_program(argv)  # this direct subprocess must obey the same rule as spawn
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
            with contextlib.ExitStack() as stack:
                parent = owner.parent
                if os.name == 'nt':  # don't inherit a possibly shared owner's folder ACL for refreshed credentials
                    parent = Path(stack.enter_context(private_tempdir(prefix='.ringo-auth-', dir=parent)))
                fd, tmp = tempfile.mkstemp(prefix='.ringo-auth-', dir=parent)  # created 0600 on POSIX
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
    with private_tempdir(prefix='council-judge-') as tmp, contextlib.ExitStack() as files:
        out, err, stdin = (files.enter_context(_spool(tmp, name)) for name in ('out', 'err', 'stdin'))
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
