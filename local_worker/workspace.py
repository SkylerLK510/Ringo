"""Isolated workspace materialization and candidate application.

Candidates are never written into the owner's working tree. The real provider
uses a detached git worktree at the assignment's base revision; the snapshot
provider materializes only supplied slices for offline fixtures.
"""

from __future__ import annotations

import shutil
import subprocess
import tempfile
from abc import ABC, abstractmethod
from pathlib import Path

from .candidate import Candidate
from .config import Config
from .control import Deadline
from .schemas import Assignment, digest_bytes

_CLEANUP_MESSAGE_LIMIT = 500


class WorkspaceError(Exception):
    """An isolated workspace could not be prepared or a preimage did not match."""


def _safe_target(root: Path, relative: str) -> Path:
    root_resolved = root.resolve()
    target = (root_resolved / relative).resolve()
    if target != root_resolved and root_resolved not in target.parents:
        raise WorkspaceError(f"candidate path escapes the workspace: {relative}")
    current = root_resolved
    for part in Path(relative).parts:
        current = current / part
        if current.is_symlink():
            raise WorkspaceError(f"candidate path crosses a symlink: {relative}")
    return target


class Workspace(ABC):
    def __init__(self, assignment: Assignment, config: Config):
        self.assignment = assignment
        self.config = config
        self.root: Path | None = None
        self.cleanup_detail = ""
        self.deadline: Deadline | None = None

    @abstractmethod
    def __enter__(self) -> Path:
        ...

    @abstractmethod
    def __exit__(self, exc_type: object, exc: object, tb: object) -> None:
        ...

    def verify_preimages(self) -> None:
        if self.root is None:
            raise WorkspaceError("workspace is not prepared")
        for item in self.assignment.file_slices:
            target = _safe_target(self.root, item.path)
            try:
                actual = digest_bytes(target.read_bytes())
            except OSError as exc:
                raise WorkspaceError(f"cannot read preimage for {item.path}: {exc}") from exc
            if actual != item.digest:
                raise WorkspaceError(f"stale preimage for {item.path}")

    def apply(self, candidate: Candidate) -> tuple[str, ...]:
        if self.root is None:
            raise WorkspaceError("workspace is not prepared")
        self.verify_preimages()
        written: list[str] = []
        for item in candidate.files:
            if item.path not in self.assignment.allowed_paths:
                raise WorkspaceError(f"candidate path is not allowed: {item.path}")
            target = _safe_target(self.root, item.path)
            data = item.content.encode("utf-8")
            if len(data) > self.config.max_file_bytes:
                raise WorkspaceError(f"candidate file is too large: {item.path}")
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(data)
            written.append(item.path)
        return tuple(written)


class SnapshotWorkspace(Workspace):
    """Offline fixture workspace containing only the supplied slices."""

    def __init__(self, assignment: Assignment, config: Config, private_root: Path):
        super().__init__(assignment, config)
        self._private_root = private_root
        self._temp: tempfile.TemporaryDirectory[str] | None = None

    def __enter__(self) -> Path:
        self._private_root.mkdir(parents=True, exist_ok=True)
        self._temp = tempfile.TemporaryDirectory(prefix="lw-snapshot-", dir=str(self._private_root))
        root = Path(self._temp.name)
        self.root = root
        for item in self.assignment.file_slices:
            target = _safe_target(root, item.path)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(item.content.encode("utf-8"))
        try:
            self.verify_preimages()
        except BaseException:
            # __exit__ is not called when __enter__ raises.
            self._temp.cleanup()
            self._temp = None
            self.root = None
            raise
        return root

    def __exit__(self, exc_type: object, exc: object, tb: object) -> None:
        if self._temp is not None:
            self._temp.cleanup()
            self._temp = None
        self.root = None


class GitWorktreeWorkspace(Workspace):
    """Detached git worktree at the assignment's base revision."""

    def __init__(
        self,
        assignment: Assignment,
        config: Config,
        repo_path: Path,
        private_root: Path,
    ):
        super().__init__(assignment, config)
        if not assignment.base_revision:
            raise WorkspaceError("a base_revision is required for a git worktree")
        self._repo = repo_path.resolve()
        self._private_root = private_root.resolve()
        self._private_root.mkdir(parents=True, exist_ok=True)
        self.root = Path(
            tempfile.mkdtemp(prefix="lw-worktree-", dir=str(self._private_root))
        )
        self._added = False

    def _git(self, args: list[str], timeout: float) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["git", "-C", str(self._repo), *args],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )

    def __enter__(self) -> Path:
        assert self.root is not None
        revision = self.assignment.base_revision
        remaining = self.deadline.remaining() if self.deadline else 30.0
        if remaining <= 0:
            self._cleanup()
            raise WorkspaceError("job deadline expired before workspace setup")
        try:
            completed = self._git(
                ["worktree", "add", "--detach", str(self.root), str(revision)],
                timeout=min(30.0, remaining),
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            self._added = True  # add may have registered the worktree before timing out
            self._cleanup()
            raise WorkspaceError(f"git worktree add failed: {exc}") from exc
        if completed.returncode != 0:
            self._cleanup()
            raise WorkspaceError(f"git worktree add failed: {completed.stderr.strip()[:_CLEANUP_MESSAGE_LIMIT]}")
        self._added = True
        try:
            self.verify_preimages()
        except BaseException:
            # Python does not call __exit__ when __enter__ raises; clean the
            # freshly registered worktree here so repeated bad assignments do
            # not accumulate checkout copies.
            self._cleanup()
            raise
        return self.root

    def _cleanup(self) -> None:
        if self._added and self.root is not None:
            try:
                completed = self._git(
                    ["worktree", "remove", "--force", str(self.root)],
                    timeout=max(1.0, self.config.cleanup_allowance_s),
                )
                if completed.returncode != 0:
                    self.cleanup_detail = completed.stderr.strip()[:_CLEANUP_MESSAGE_LIMIT]
            except (OSError, subprocess.TimeoutExpired) as exc:
                self.cleanup_detail = str(exc)[:_CLEANUP_MESSAGE_LIMIT]
        if self.root is not None and self.root.exists():
            shutil.rmtree(self.root, ignore_errors=True)
        self._added = False
        self.root = None

    def __exit__(self, exc_type: object, exc: object, tb: object) -> None:
        self._cleanup()
