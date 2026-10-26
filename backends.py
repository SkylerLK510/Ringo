"""Ringo backend commands, isolated subprocess execution and strict reply-object parsing.

Extracted from the original design council runtime; no council scoring or CLI dependency.
"""
import contextlib
import json
import math
import os
import re
import signal
import subprocess
import sys
import tempfile
from pathlib import Path

try:
    import fcntl
except ImportError:  # not POSIX: Ringo processes are not serialized against each other
    fcntl = None

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
# Replaces Codex's agent prompt (about 22 KB) for lean calls; Ringo's own instructions travel in the message.
CODEX_INSTRUCTIONS = 'Follow the instructions in the user message exactly.\n'
# Catalog fields that attach the code-mode `exec` tool, the multi-agent tools and their instructions to every
# request; no config flag removes them in codex-cli 0.164.
CATALOG_TOOL_FIELDS = ('tool_mode', 'multi_agent_version', 'multi_agent_reasoning_effort', 'apply_patch_tool_type')


class JudgeError(Exception):
    """The judge could not answer. Carries a message that is safe to store, and `fatal`
    when no later request can succeed either (a refused key), so the run stops."""

    def __init__(self, message, fatal=False):
        super().__init__(message)
        self.fatal = fatal


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
    env.update(HOME=str(home), CODEX_HOME=str(private))
    return owner, snapshot, copy


def _lean_codex(argv, private, env):
    """argv with Codex's agent prompt replaced by CODEX_INSTRUCTIONS and, when the installed CLI can print its
    model catalog, a copy without the tool modes, inserted before the trailing stdin marker. Both files live
    in the call's private CODEX_HOME. A CLI without `debug models`, or a catalog in another format, keeps only
    the instructions override."""
    instructions = private / 'ringo-instructions.md'
    instructions.write_text(CODEX_INSTRUCTIONS, encoding='utf-8')
    extra = ['-c', f'model_instructions_file={json.dumps(str(instructions))}']
    try:
        listed = subprocess.run([argv[0], 'debug', 'models'], env=env, cwd=private, stdin=subprocess.DEVNULL,
                                capture_output=True, timeout=15, check=True).stdout
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
        if fcntl:
            fcntl.flock(fd, fcntl.LOCK_EX)
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


def run_isolated(argv, prompt, timeout_s, *, subscription_auth=False, codex_home=False, lean=False):
    """stdout of one run. The working folder is empty, so no project instructions or files
    load from it; it is not a file-system boundary. The command gets the caller's
    environment without the TypeSafe key, in a session of its own, so a timeout kills it and
    everything it started, not only the direct child. codex_home: the command is a Codex CLI, so it
    gets a private CODEX_HOME and HOME (see _codex_home); a refreshed login is written back even
    when the call fails or times out. lean (with codex_home): the call is a single tool-free Codex turn, so
    Codex's agent prompt and tool modes are replaced (see _lean_codex). A failure (JudgeError) or timeout
    (TimeoutExpired) carries the stdout written so far as .stdout."""
    # ceiling: stdout is read whole; a command that floods it fills disk and memory
    env = {k: v for k, v in os.environ.items() if k != 'TYPESAFE_API_KEY'}
    if subscription_auth:
        env = {k: v for k, v in env.items() if not k.endswith('API_KEY') and k not in {
            'OPENAI_BASE_URL', 'OPENAI_API_BASE', 'OPENAI_ACCESS_TOKEN', 'ANTHROPIC_AUTH_TOKEN',
            'OPENAI_IDENTITY_TOKEN_FILE', 'OPENAI_WORKLOAD_IDENTITY_PROVIDER_ID'}}
    # Output goes to files, not pipes, so the wait ends when the command exits, even if something it
    # started still holds stdout open; whatever it started is then killed with its session.
    with tempfile.TemporaryDirectory(prefix='council-judge-') as tmp, tempfile.TemporaryFile() as out, \
            tempfile.TemporaryFile() as err:
        cwd = Path(tmp) / 'work'
        cwd.mkdir()
        login = _codex_home(Path(tmp), env) if codex_home else None
        if codex_home and lean:
            argv = _lean_codex(argv, Path(tmp) / 'codex', env)
        timed_out = None
        try:
            proc = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=out, stderr=err, cwd=cwd, env=env,
                                    start_new_session=True)
            try:
                proc.communicate(prompt.encode('utf-8'), timeout=timeout_s)
            except subprocess.TimeoutExpired as exc:  # raised below, once what it wrote before the kill is read
                timed_out = exc
            finally:
                try:
                    os.killpg(proc.pid, signal.SIGKILL) if hasattr(os, 'killpg') else proc.kill()
                except (ProcessLookupError, PermissionError):
                    pass
                proc.wait()
        finally:
            if login:
                _sync_codex_auth(*login)
        out.seek(0)
        err.seek(0)
        stdout, stderr = out.read().decode('utf-8', 'replace'), err.read().decode('utf-8', 'replace')
    if timed_out:
        timed_out.stdout = stdout  # the events before the kill: what the call cost, and why it hung
        raise timed_out
    if proc.returncode:
        tail = ' '.join(stderr.split())[-300:]
        exc = JudgeError(f'{Path(argv[0]).name} exited with {proc.returncode}' + (f': {tail}' if tail else ''))
        exc.stdout = stdout  # a CLI can report on stdout what the failed call cost
        raise exc
    return stdout
