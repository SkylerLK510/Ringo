"""Command line entry point for the standalone local worker."""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

from .adapters import InferenceUnavailable, UnsupportedPolicy, build_adapter
from .config import Config, ConfigError, load_assignment_file
from .ledger import OperationLedger, OperationConflict
from .prompts import AssignmentTooLarge, build_prompt
from .runner import RunRequest, run
from .schemas import Assignment, SchemaError
from .workspace import GitWorktreeWorkspace, WorkspaceError

_EXIT_INPUT = 2
_EXIT_JOB = 4


def _outside_repo(path: Path, repo: Path) -> None:
    target = path.resolve()
    root = repo.resolve()
    if target == root or root in target.parents:
        raise ValueError(f"{path} must be outside the repository checkout")


def _emit(payload: dict[str, object]) -> None:
    print(json.dumps(payload, sort_keys=True, separators=(",", ":")))


def _validate(args: argparse.Namespace) -> int:
    config = Config.from_file(args.config)
    assignment = Assignment.from_dict(load_assignment_file(args.assignment))
    if assignment.routing_policy != config.policy:
        raise ValueError("assignment and config routing policies differ")
    prompt = build_prompt(assignment, config)
    _emit(
        {
            "command": "validate",
            "status": "ok",
            "assignment_digest": assignment.digest(),
            "config_fingerprint": config.fingerprint(),
            "policy": config.policy,
            "checks": [item.id for item in assignment.checks],
            "prompt_bytes": len(prompt.encode("utf-8")),
            "context_tokens": config.context_tokens,
            "model": config.model,
        }
    )
    return 0


def _run(args: argparse.Namespace) -> int:
    config = Config.from_file(args.config)
    assignment = Assignment.from_dict(load_assignment_file(args.assignment))
    if assignment.routing_policy != config.policy:
        raise ValueError("assignment and config routing policies differ")
    repo = Path(args.repo)
    ledger_root = Path(args.ledger_root)
    result_dir = Path(args.result_dir)
    _outside_repo(ledger_root, repo)
    _outside_repo(result_dir, repo)

    adapter = build_adapter(config)
    ledger = OperationLedger(ledger_root)
    workspace_root = ledger_root / "workspaces"

    def factory() -> GitWorktreeWorkspace:
        return GitWorktreeWorkspace(assignment, config, repo, workspace_root)

    request = RunRequest(
        assignment=assignment,
        config=config,
        adapter=adapter,
        workspace_factory=factory,
        ledger=ledger,
        request_id=args.request_id or assignment.assignment_id,
        result_dir=result_dir,
    )
    result, result_path = run(request)
    _emit(
        {
            "command": "run",
            "status": result.status,
            "result": str(result_path),
            "operation_id": result.operation_id,
            "calls": len(result.attempts),
            "checks": len(result.check_results),
            "usage": result.usage,
            "errors": list(result.errors),
        }
    )
    return 0 if result.status == "candidate_ready" else _EXIT_JOB


def _reconcile(args: argparse.Namespace) -> int:
    ledger = OperationLedger(args.ledger_root)
    changed = ledger.reconcile()
    _emit({"command": "reconcile", "marked_unknown": [item.operation_id for item in changed]})
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="ringo-local-worker", description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)

    validate = sub.add_parser("validate", help="validate an assignment/config pair without inference")
    validate.add_argument("--assignment", required=True)
    validate.add_argument("--config", required=True)
    validate.set_defaults(handler=_validate)

    execute = sub.add_parser("run", help="run one bounded local job")
    execute.add_argument("--assignment", required=True)
    execute.add_argument("--config", required=True)
    execute.add_argument("--ledger-root", required=True)
    execute.add_argument("--result-dir", required=True)
    execute.add_argument("--repo", default=".", help="repository to snapshot into a detached worktree")
    execute.add_argument("--request-id", default=None)
    execute.set_defaults(handler=_run)

    reconcile = sub.add_parser("reconcile", help="mark interrupted operations unknown")
    reconcile.add_argument("--ledger-root", required=True)
    reconcile.set_defaults(handler=_reconcile)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        return int(args.handler(args))
    except (InferenceUnavailable, ConfigError, SchemaError, UnsupportedPolicy, ValueError, WorkspaceError, AssignmentTooLarge, OperationConflict, OSError) as exc:
        print(f"ringo local-worker: {exc}", file=sys.stderr)
        return _EXIT_INPUT
    except KeyboardInterrupt:
        print("ringo local-worker: interrupted", file=sys.stderr)
        return 130
