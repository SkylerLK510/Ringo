"""Fixtures for the standalone local-worker tests."""

from __future__ import annotations

import json
import sys
import tempfile
from pathlib import Path

from local_worker.config import Config
from local_worker.schemas import Assignment, digest_bytes

MODEL_DIGEST = "sha256:" + "a" * 64
SLICE_CONTENT = "original text\n"


def make_config(**overrides: object) -> Config:
    base: dict[str, object] = {
        "schema_version": 1,
        "policy": "local",
        "endpoint": "http://127.0.0.1:11434",
        "model": "qwen3.5:4b-q4_k_m",
        "model_digest": MODEL_DIGEST,
        "runtime_version": "0.0.0-test",
        "quantization": "Q4_K_M",
        "context_tokens": 4096,
        "max_output_tokens": 1024,
        "job_deadline_s": 30.0,
        "cleanup_allowance_s": 1.0,
        "attempt_deadline_s": 20.0,
    }
    base.update(overrides)
    return Config.from_dict(base)


def passing_check(name: str = "syntax") -> dict[str, object]:
    return {"id": name, "argv": [sys.executable, "-c", "raise SystemExit(0)"], "timeout_s": 5.0}


def failing_check(name: str = "syntax") -> dict[str, object]:
    return {"id": name, "argv": [sys.executable, "-c", "raise SystemExit(1)"], "timeout_s": 5.0}


def make_assignment(
    *,
    checks: list[dict[str, object]] | None = None,
    objective: str = "Draft a short replacement for notes.txt.",
    allowed_paths: list[str] | None = None,
    file_content: str = SLICE_CONTENT,
    policy: str = "local",
) -> Assignment:
    digest = digest_bytes(file_content.encode("utf-8"))
    raw = {
        "schema_version": 1,
        "assignment_id": "assignment-1",
        "task_id": "task-1",
        "base_revision": "deadbeef",
        "objective": objective,
        "constraints": ["Keep the text under one paragraph."],
        "allowed_paths": allowed_paths or ["notes.txt"],
        "file_slices": [{"path": "notes.txt", "content": file_content, "digest": digest}],
        "checks": [passing_check()] if checks is None else checks,
        "routing_policy": policy,
    }
    return Assignment.from_dict(raw)


def candidate_json(
    *,
    kind: str = "patch",
    content: str = "replacement text\n",
    assignment: Assignment | None = None,
    uncertainties: list[str] | None = None,
) -> str:
    if assignment is None:
        assignment = make_assignment()
    slice_item = assignment.file_slices[0]
    payload: dict[str, object] = {
        "schema_version": 1,
        "kind": kind,
        "answer": "Draft complete.",
        "files": [
            {
                "path": slice_item.path,
                "content": content,
                "preimage_digest": slice_item.digest,
            }
        ]
        if kind == "patch"
        else [],
        "uncertainties": uncertainties or [],
        "citations": [slice_item.path],
    }
    return json.dumps(payload)


def workspace_factory(assignment: Assignment, config: Config, root: Path):
    from local_worker.workspace import SnapshotWorkspace

    def factory() -> SnapshotWorkspace:
        return SnapshotWorkspace(assignment, config, root)

    return factory


def sandbox_available():
    """Probe host namespace support independently of the production launcher."""
    import subprocess
    import shutil
    import os
    bwrap = shutil.which('bwrap', path='/usr/bin:/bin')
    if sys.platform != 'linux' or not bwrap:
        if os.environ.get('RINGO_REQUIRE_WORKER_SANDBOX') == '1':
            raise RuntimeError('required Linux bubblewrap is unavailable')
        return False
    # Production exposes system installations only, not arbitrary virtualenvs.
    if not Path(sys.executable).resolve().is_relative_to('/usr'):
        if os.environ.get('RINGO_REQUIRE_WORKER_SANDBOX') == '1':
            raise RuntimeError('required sandbox system interpreter is unavailable')
        return False
    try:
        supported = subprocess.run([bwrap, '--unshare-all', '--ro-bind', '/', '/',
                                    sys.executable, '-c', 'pass'], capture_output=True,
                                   timeout=3).returncode == 0
    except (OSError, subprocess.TimeoutExpired):
        supported = False
    if not supported and os.environ.get('RINGO_REQUIRE_WORKER_SANDBOX') == '1':
        raise RuntimeError('required worker sandbox host support is unavailable')
    return supported

SANDBOX_AVAILABLE = sandbox_available()
