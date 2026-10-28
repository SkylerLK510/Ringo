"""Portable fixtures shared by the tests: fake command-line programs, and what this platform's file system allows."""
import os
import sys
import tempfile
from pathlib import Path

POSIX_MODES = 'Windows has no POSIX permission bits'


def python_cli(path, source):
    """Return argv for a Python fixture. Windows uses the interpreter directly, never a batch wrapper."""
    path = Path(path)
    if os.name != 'nt':
        path.write_text(f'#!{sys.executable}\n{source}', encoding='utf-8')
        path.chmod(0o755)
        return [str(path)]
    script = path.with_name(path.name + '.py')
    script.write_text(source, encoding='utf-8')
    return [sys.executable, str(script)]


def echo_cli(path, text, note=''):
    """Make path a one-file command that prints text (a note only changes its content), and return its argv[0]:
    path as a #!/bin/sh script on POSIX, path.cmd on Windows."""
    path, extra = Path(path), f'\n{"@rem" if os.name == "nt" else "#"} {note}' if note else ''
    if os.name == 'nt':
        path = path.with_name(path.name + '.cmd')
        path.write_text(f'@echo {text}{extra}\n', encoding='utf-8')
    else:
        path.write_text(f'#!/bin/sh\necho "{text}"{extra}\n', encoding='utf-8')
        path.chmod(0o755)
    return str(path)


def _can_symlink():
    with tempfile.TemporaryDirectory() as d:
        try:
            Path(d, 'link').symlink_to(Path(d, 'target'))
        except OSError:
            return False
    return True


SYMLINKS = _can_symlink()
NO_SYMLINKS = 'creating a symlink needs administrator rights or developer mode on Windows'
