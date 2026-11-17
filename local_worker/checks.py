"""Host-configured deterministic checks with bounded output and real timeouts."""

from __future__ import annotations

import os
import signal
import subprocess
import threading
import time
from pathlib import Path
from uuid import uuid4

from .config import Config
from .control import Deadline
from .schemas import CheckResult, CheckSpec, digest_bytes

_CAPTURE_ENV = ("PATH", "HOME", "LANG", "LC_ALL", "SYSTEMROOT", "TEMP", "TMP")
_MAX_DETAIL_CHARS = 500


class CheckSetupError(ValueError):
    """A check specification cannot be run safely."""


def _safe_cwd(spec: CheckSpec, workspace_root: Path) -> Path:
    root = workspace_root.resolve()
    target = (root / spec.cwd).resolve()
    if target != root and root not in target.parents:
        raise CheckSetupError(f"check {spec.id} cwd escapes the isolated workspace")
    if not target.is_dir():
        raise CheckSetupError(f"check {spec.id} cwd is not a directory")
    return target


def _environment(spec: CheckSpec) -> dict[str, str]:
    env = {key: os.environ[key] for key in _CAPTURE_ENV if key in os.environ}
    env.update(spec.env)
    return env


def _kill_process(proc: subprocess.Popen[bytes]) -> None:
    if os.name == "posix":
        try:
            os.killpg(proc.pid, signal.SIGKILL)
            return
        except (ProcessLookupError, PermissionError, OSError):
            pass
    try:
        proc.kill()
    except OSError:
        pass


def run_check(
    spec: CheckSpec,
    workspace_root: Path,
    private_dir: Path,
    deadline: Deadline,
    config: Config,
) -> tuple[CheckResult, Path | None]:
    """Run one argv list with no shell, capped output and process cleanup.

    Raw output is written only to a private diagnostics directory. The returned
    result carries a digest and a short status detail, never the raw bytes.
    """
    started = time.monotonic()
    private_dir.mkdir(parents=True, exist_ok=True)
    output_path: Path | None = None

    try:
        cwd = _safe_cwd(spec, workspace_root)
    except CheckSetupError as exc:
        elapsed = int((time.monotonic() - started) * 1000)
        return CheckResult(spec.id, "unavailable", None, digest_bytes(b""), elapsed, str(exc)[:_MAX_DETAIL_CHARS]), None

    capture_cap = min(65_536, config.max_stream_bytes)
    captured: list[bytes] = []
    captured_total = 0
    capture_lock = threading.Lock()

    def read(stream: object) -> None:
        nonlocal captured_total
        assert stream is not None
        while True:
            chunk = stream.read(4_096)  # type: ignore[attr-defined]
            if not chunk:
                break
            with capture_lock:
                remaining = capture_cap - captured_total
                if remaining > 0:
                    captured.append(chunk[:remaining])
                    captured_total += min(len(chunk), remaining)
        stream.close()  # type: ignore[attr-defined]

    popen_kwargs: dict[str, object] = {
        "cwd": str(cwd),
        "stdin": subprocess.DEVNULL,
        "stdout": subprocess.PIPE,
        "stderr": subprocess.PIPE,
        "env": _environment(spec),
    }
    if os.name == "posix":
        popen_kwargs["start_new_session"] = True
    elif hasattr(subprocess, "CREATE_NEW_PROCESS_GROUP"):
        popen_kwargs["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP

    proc: subprocess.Popen[bytes] | None = None
    status = "passed"
    exit_code: int | None = None
    detail = ""
    try:
        proc = subprocess.Popen(list(spec.argv), **popen_kwargs)  # type: ignore[arg-type]
        readers = [
            threading.Thread(target=read, args=(proc.stdout,), daemon=True),
            threading.Thread(target=read, args=(proc.stderr,), daemon=True),
        ]
        for thread in readers:
            thread.start()
        timeout = min(float(spec.timeout_s), deadline.remaining())
        if timeout <= 0:
            _kill_process(proc)
            status = "timeout"
            detail = "job deadline expired before the check started"
        else:
            try:
                exit_code = proc.wait(timeout=timeout)
            except subprocess.TimeoutExpired:
                _kill_process(proc)
                status = "timeout"
                detail = f"check exceeded {timeout:.3f}s"
            if status == "passed" and exit_code != 0:
                status = "failed"
                detail = f"check exited with {exit_code}"
        for thread in readers:
            thread.join(timeout=1.0)
    except (OSError, ValueError) as exc:
        status = "unavailable"
        detail = f"check could not start: {exc}"[:_MAX_DETAIL_CHARS]
        if proc is not None:
            _kill_process(proc)
    finally:
        if proc is not None and proc.poll() is None:
            _kill_process(proc)
            try:
                proc.wait(timeout=1.0)
            except subprocess.TimeoutExpired:
                pass

    raw = b"".join(captured)
    name = f"{spec.id}-{uuid4().hex[:12]}.txt"
    output_path = private_dir / name
    fd = os.open(str(output_path), os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as handle:
        handle.write(raw)
    elapsed = int((time.monotonic() - started) * 1000)
    result = CheckResult(
        check_id=spec.id,
        status=status,
        exit_code=exit_code,
        output_digest=digest_bytes(raw),
        duration_ms=elapsed,
        detail=detail[:_MAX_DETAIL_CHARS],
    )
    return result, output_path
