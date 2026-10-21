"""Ringo backend commands, isolated subprocess execution and strict reply-object parsing.

Extracted from the original design council runtime; no council scoring or CLI dependency.
"""
import json
import math
import os
import signal
import subprocess
import tempfile
from pathlib import Path

JUDGE_PRESETS = {
    'claude': ('claude -p --setting-sources project --strict-mcp-config --disable-slash-commands --tools "" '
               '--no-session-persistence --model sonnet'),
    'codex': ('codex exec --ignore-user-config --ignore-rules --disable apps --disable plugins '
              '''-c 'web_search="disabled"' -c 'model_reasoning_effort="high"' --skip-git-repo-check --ephemeral '''
              '-s read-only --color never -'),
}


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


def top_level_objects(text):
    """Every top-level JSON object in text, bare or inside prose or a code fence. An object nested in another is
    never yielded on its own, and each object records whether it repeats a key (repeats_a_key)."""
    decoder = json.JSONDecoder(parse_constant=_no_constant, parse_float=_finite_float, object_pairs_hook=_object)
    start = 0
    while (start := text.find('{', start)) != -1:
        try:
            obj, end = decoder.raw_decode(text, start)
        except ValueError:
            start += 1
            continue
        start = end  # skip the object's inside: a nested object is never the answer
        if isinstance(obj, dict):
            yield obj


def run_isolated(argv, prompt, timeout_s):
    """stdout of one run. The working folder is empty, so no project instructions or files
    load from it; it is not a file-system boundary. The command gets the caller's
    environment without the TypeSafe key, in a session of its own, so a timeout kills it and
    everything it started, not only the direct child."""
    # ceiling: stdout is read whole; a command that floods it fills disk and memory
    env = {k: v for k, v in os.environ.items() if k != 'TYPESAFE_API_KEY'}
    # Output goes to files, not pipes, so the wait ends when the command exits, even if something it
    # started still holds stdout open; whatever it started is then killed with its session.
    with tempfile.TemporaryDirectory(prefix='council-judge-') as cwd, tempfile.TemporaryFile() as out, \
            tempfile.TemporaryFile() as err:
        proc = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=out, stderr=err, cwd=cwd, env=env,
                                start_new_session=True)
        try:
            proc.communicate(prompt.encode('utf-8'), timeout=timeout_s)
        finally:
            try:
                os.killpg(proc.pid, signal.SIGKILL) if hasattr(os, 'killpg') else proc.kill()
            except (ProcessLookupError, PermissionError):
                pass
            proc.wait()
        out.seek(0)
        err.seek(0)
        stdout, stderr = out.read().decode('utf-8', 'replace'), err.read().decode('utf-8', 'replace')
    if proc.returncode:
        tail = ' '.join(stderr.split())[-300:]
        exc = JudgeError(f'{Path(argv[0]).name} exited with {proc.returncode}' + (f': {tail}' if tail else ''))
        exc.stdout = stdout  # a CLI can report on stdout what the failed call cost
        raise exc
    return stdout
