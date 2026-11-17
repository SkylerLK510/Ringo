"""Git-worktree workspace lifecycle, including failed-entry cleanup."""

import subprocess
import tempfile
import unittest
from pathlib import Path

from local_worker.schemas import Assignment, digest_bytes
from local_worker.workspace import GitWorktreeWorkspace, WorkspaceError

from local_worker_support import make_config


def _git(repo: Path, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["git", "-C", str(repo), *args],
        check=True,
        capture_output=True,
        text=True,
    )


class GitWorktreeWorkspaceTests(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        _git(self.repo, "init", "-q")
        (self.repo / "notes.txt").write_text("repo content\n", encoding="utf-8")
        _git(self.repo, "add", "notes.txt")
        _git(
            self.repo, "-c", "user.name=Test", "-c", "user.email=test@example.com",
            "commit", "-q", "-m", "init",
        )
        self.commit = _git(self.repo, "rev-parse", "HEAD").stdout.strip()
        self.private = self.root / "private"
        self.config = make_config()

    def _assignment(self, *, content: str, base_revision: str | None = None) -> Assignment:
        raw = {
            "schema_version": 1,
            "assignment_id": "assignment-1",
            "task_id": "task-1",
            "base_revision": base_revision if base_revision is not None else self.commit,
            "objective": "Draft a replacement for notes.txt.",
            "constraints": [],
            "allowed_paths": ["notes.txt"],
            "file_slices": [
                {
                    "path": "notes.txt",
                    "content": content,
                    "digest": digest_bytes(content.encode("utf-8")),
                }
            ],
            "checks": [],
            "routing_policy": "local",
        }
        return Assignment.from_dict(raw)

    def _worktrees(self) -> list[str]:
        listing = _git(self.repo, "worktree", "list", "--porcelain").stdout
        return [line for line in listing.splitlines() if line.startswith("worktree ")]

    def test_stale_preimage_cleans_up_the_worktree(self):
        # A slice whose digest does not match the file at the base revision
        # makes verification raise after Git has registered the worktree.
        assignment = self._assignment(content="stale content\n")
        before = self._worktrees()
        workspace = GitWorktreeWorkspace(assignment, self.config, self.repo, self.private)
        with self.assertRaises(WorkspaceError):
            with workspace as root:
                self.fail("workspace should not have been prepared")
        self.assertEqual(
            self._worktrees(), before, "a git worktree leaked after failed preimage verification"
        )
        self.assertIsNone(workspace.root)
        self.assertEqual(list(self.private.glob("lw-worktree-*")), [], "a checkout directory leaked")

    def test_valid_worktree_is_prepared_and_cleaned_up(self):
        assignment = self._assignment(content="repo content\n")
        before = self._worktrees()
        workspace = GitWorktreeWorkspace(assignment, self.config, self.repo, self.private)
        with workspace as root:
            self.assertEqual((root / "notes.txt").read_text(encoding="utf-8"), "repo content\n")
            self.assertEqual(len(self._worktrees()), len(before) + 1)
        self.assertEqual(self._worktrees(), before)
        self.assertFalse(root.exists())
