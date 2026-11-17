"""Bounded orchestration of one local inference job."""

from __future__ import annotations

import json
import os
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

from .adapters import InferenceAdapter, InferenceCancelled, InferenceUnavailable
from .candidate import Candidate, CandidateError, parse_candidate
from .checks import run_check
from .config import Config
from .control import Deadline, StopControl
from .ledger import OperationLedger, OperationRecord
from .prompts import AssignmentTooLarge, build_prompt
from .schemas import (
    MAX_ERRORS,
    MAX_ERROR_CHARS,
    RESULT_SCHEMA_VERSION,
    Assignment,
    AttemptRecord,
    CheckResult,
    Result,
    digest_json,
)
from .workspace import Workspace, WorkspaceError

_TERMINAL_SUCCESS = "candidate_ready"


@dataclass(frozen=True)
class RunRequest:
    assignment: Assignment
    config: Config
    adapter: InferenceAdapter
    workspace_factory: Callable[[], Workspace]
    ledger: OperationLedger
    request_id: str
    result_dir: Path


def _write_json(path: Path, payload: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    data = json.dumps(payload, sort_keys=True, separators=(",", ":")).encode("utf-8")
    fd, temporary = tempfile.mkstemp(prefix=".result-", suffix=".tmp", dir=str(path.parent))
    try:
        with os.fdopen(fd, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
    except BaseException:
        try:
            os.unlink(temporary)
        except OSError:
            pass
        raise


def _usage(attempts: list[AttemptRecord]) -> dict[str, Any]:
    fields = (
        "prompt_eval_count",
        "prompt_eval_cached_count",
        "eval_count",
        "total_duration_ns",
        "load_duration_ns",
    )
    usage: dict[str, Any] = {field: 0 for field in fields}
    for attempt in attempts:
        for field in fields:
            value = attempt.usage.get(field)
            if isinstance(value, int):
                usage[field] += value
    usage["reported"] = bool(attempts) and all(bool(item.usage.get("reported")) for item in attempts)
    usage["attempts"] = len(attempts)
    if not usage["reported"]:
        usage["unavailable"] = [field for field in fields if usage[field] == 0]
    return usage


def _feedback(errors: list[str], checks: list[CheckResult]) -> str:
    lines = [f"previous check {item.check_id}: {item.status} ({item.detail})" for item in checks[-2:]]
    lines.extend(f"validation: {item}" for item in errors[-3:])
    return "\n".join(line[:500] for line in lines)[:2_000]


def _empty_result(
    *,
    request: RunRequest,
    operation_id: str,
    status: str,
    phase: str,
    errors: tuple[str, ...],
    elapsed_ms: int,
) -> Result:
    return Result(
        schema_version=RESULT_SCHEMA_VERSION,
        assignment_id=request.assignment.assignment_id,
        assignment_digest=request.assignment.digest(),
        operation_id=operation_id,
        status=status,
        phase=phase,
        policy=request.config.policy,
        worker=request.adapter.describe(),
        deadline_s=request.config.job_deadline_s,
        elapsed_ms=elapsed_ms,
        attempts=(),
        check_results=(),
        changed_files=(),
        candidate_digest=None,
        candidate_ref=None,
        escalation_needed=True,
        errors=errors[:MAX_ERRORS],
        usage=_usage([]),
        cleanup={},
    )


def _persist(
    request: RunRequest, record: OperationRecord, result: Result, *, finalize: bool
) -> Path:
    """Write a result; only a freshly claimed operation may be finalized."""
    stem = f"{request.request_id}-{record.operation_id[-8:]}"
    if not finalize:
        from uuid import uuid4
        stem += "-refusal-" + uuid4().hex
    result_path = request.result_dir / f"{stem}.result.json"
    _write_json(result_path, result.to_dict())
    if finalize:
        request.ledger.update(
            record.operation_id,
            state=result.status,
            attempts=len(result.attempts),
            result_ref=result_path.name,
            error=result.errors[0] if result.errors else None,
            finished_at=time.time(),
        )
    return result_path


def run(request: RunRequest) -> tuple[Result, Path]:
    # Hold a per-request OS lease throughout execution. Reconciliation cannot
    # classify an active operation as interrupted; process death releases it.
    with request.ledger.execution_lock(request.request_id) as owned:
        if not owned:
            from .ledger import OperationConflict
            raise OperationConflict("operation is currently owned by another worker")
        return _run(request, owned)


def _run(request: RunRequest, owned: bool) -> tuple[Result, Path]:
    """Execute one assignment and return its result plus the result file path."""
    started = time.monotonic()
    request.result_dir.mkdir(parents=True, exist_ok=True)
    assignment_digest = request.assignment.digest()
    config_fingerprint = request.config.fingerprint()
    record, created = request.ledger.claim(
        request.request_id, assignment_digest, config_fingerprint
    )

    if record.result_ref:
        existing_path = request.result_dir / record.result_ref
        if existing_path.exists():
            return Result.from_dict(json.loads(existing_path.read_text(encoding="utf-8"))), existing_path

    if not created or not owned:
        # A prior or concurrent run owns this request id. Never start inference
        # for it: with no retrievable result its outcome is unresolved until a
        # person reconciles the ledger.
        result = _empty_result(
            request=request,
            operation_id=record.operation_id,
            status="unavailable",
            phase="reconcile",
            errors=("operation already exists without a retrievable result; reconcile before retrying",),
            elapsed_ms=int((time.monotonic() - started) * 1000),
        )
        return result, _persist(request, record, result, finalize=False)

    result = _run_job(request, record.operation_id, started)
    return result, _persist(request, record, result, finalize=True)


def _run_job(request: RunRequest, operation_id: str, started: float) -> Result:
    deadline = Deadline(request.config.job_deadline_s)
    stop = StopControl(deadline)
    private_dir = request.ledger.private_dir(operation_id)
    attempts: list[AttemptRecord] = []
    checks: list[CheckResult] = []
    errors: list[str] = []
    changed_files: tuple[str, ...] = ()
    candidate: Candidate | None = None
    candidate_ref: str | None = None
    cleanup: dict[str, Any] = {}
    status = "failed"
    phase = "candidate"
    verified_on_time = False

    def finish() -> Result:
        final_status = status
        if (stop.stopped and final_status not in ("timeout", "unavailable")
                and not (final_status == _TERMINAL_SUCCESS and verified_on_time)):
            final_status = "timeout"
        return Result(
            schema_version=RESULT_SCHEMA_VERSION,
            assignment_id=request.assignment.assignment_id,
            assignment_digest=request.assignment.digest(),
            operation_id=operation_id,
            status=final_status,
            phase=phase,
            policy=request.config.policy,
            worker=request.adapter.describe(),
            deadline_s=request.config.job_deadline_s,
            elapsed_ms=int((time.monotonic() - started) * 1000),
            attempts=tuple(attempts),
            check_results=tuple(checks),
            changed_files=changed_files,
            candidate_digest=candidate.digest() if candidate is not None else None,
            candidate_ref=candidate_ref,
            escalation_needed=final_status != _TERMINAL_SUCCESS
            or (candidate is not None and bool(candidate.uncertainties)),
            errors=tuple(errors[:MAX_ERRORS]),
            usage=_usage(attempts),
            cleanup=cleanup,
        )

    try:
        prompt = build_prompt(request.assignment, request.config)
    except AssignmentTooLarge as exc:
        errors.append(str(exc)[:MAX_ERROR_CHARS])
        return finish()

    feedback: str | None = None
    for index in range(1, request.config.max_attempts + 1):
        if stop.stopped:
            status = "timeout"
            break
        reserve = request.config.cleanup_allowance_s + sum(
            item.timeout_s for item in request.assignment.checks
        )
        available = deadline.remaining() - reserve
        if available <= 0:
            status = "timeout"
            errors.append("insufficient time remains for an inference attempt and its check")
            break
        attempt_stop = StopControl(
            Deadline(min(available, request.config.attempt_deadline_s)), parent=stop
        )
        attempt_started = time.monotonic()
        call_prompt = prompt
        if feedback is not None:
            try:
                call_prompt = build_prompt(request.assignment, request.config, feedback=feedback)
            except AssignmentTooLarge as exc:
                errors.append(str(exc)[:MAX_ERROR_CHARS])
                status = "failed"
                break

        try:
            inference = request.adapter.generate(call_prompt, attempt_stop)
        except InferenceCancelled:
            attempts.append(
                AttemptRecord(
                    index=index,
                    status="timeout",
                    operation_id="call-" + operation_id[-8:],
                    fingerprint=digest_json({"prompt": call_prompt}),
                    duration_ms=int((time.monotonic() - attempt_started) * 1000),
                    usage={},
                )
            )
            errors.append("local inference hit its deadline; the attempt was consumed")
            status = "timeout"
            break
        except InferenceUnavailable as exc:
            attempts.append(
                AttemptRecord(
                    index=index,
                    status="unavailable",
                    operation_id="call-" + operation_id[-8:],
                    fingerprint=digest_json({"prompt": call_prompt}),
                    duration_ms=int((time.monotonic() - attempt_started) * 1000),
                    usage={},
                )
            )
            errors.append(str(exc)[:MAX_ERROR_CHARS])
            status = "unavailable"
            break

        attempts.append(
            AttemptRecord(
                index=index,
                status="completed",
                operation_id=inference.operation_id,
                fingerprint=inference.fingerprint,
                duration_ms=int((time.monotonic() - attempt_started) * 1000),
                usage=inference.usage.to_dict(),
            )
        )

        try:
            candidate = parse_candidate(inference.text, request.assignment, request.config)
        except CandidateError as exc:
            errors.append(f"{exc.code}: {exc.detail}"[:MAX_ERROR_CHARS])
            candidate = None
            status = "malformed_candidate"
            break

        phase = "checks"
        if not request.assignment.checks:
            if candidate.kind == "patch":
                errors.append("a patch candidate requires at least one configured check")
                status = "failed"
                candidate = None
                break
            verified_on_time = not stop.stopped
            status = _TERMINAL_SUCCESS
            break

        # The whole configured suite runs for every parsed candidate. A passing
        # first check never makes a later one optional, and a repair cannot be
        # accepted by passing only the check it happened to reach.
        suite: list[CheckResult] = []
        workspace = request.workspace_factory()
        workspace.deadline = deadline
        try:
            with workspace as root:
                if candidate.kind == "patch":
                    changed_files = workspace.apply(candidate)
                for spec in request.assignment.checks:
                    check_result, output_path = run_check(
                        spec, root, private_dir, deadline, request.config
                    )
                    suite.append(check_result)
                    checks.append(check_result)
                    if output_path is not None:
                        cleanup.setdefault("check_logs", []).append(output_path.name)
                verified_on_time = not stop.stopped
        except WorkspaceError as exc:
            errors.append(str(exc)[:MAX_ERROR_CHARS])
            status = "failed"
            candidate = None
            break
        cleanup["workspace"] = workspace.cleanup_detail or "ok"

        failed = [item for item in suite if item.status != "passed"]
        if not failed:
            status = _TERMINAL_SUCCESS
            break
        for item in failed:
            errors.append(f"check {item.check_id}: {item.status}: {item.detail}"[:MAX_ERROR_CHARS])
        status = "check_failed"
        if index < request.config.max_attempts and not stop.stopped:
            feedback = _feedback(errors, checks)

    if status == _TERMINAL_SUCCESS and candidate is not None:
        try:
            candidate_ref = f"{request.request_id}-{operation_id[-8:]}.candidate.json"
            _write_json(
                request.result_dir / candidate_ref,
                {
                    "candidate_schema_version": candidate.schema_version,
                    "assignment_id": request.assignment.assignment_id,
                    "assignment_digest": request.assignment.digest(),
                    "candidate_digest": candidate.digest(),
                    "candidate": candidate.to_dict(),
                },
            )
        except OSError as exc:
            errors.append(f"candidate artifact could not be written: {exc}"[:MAX_ERROR_CHARS])
            status = "failed"
            candidate = None
            candidate_ref = None

    return finish()
