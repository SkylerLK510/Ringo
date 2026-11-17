"""Durable operation records kept outside the repository checkout."""

from __future__ import annotations

import json
import os
import tempfile
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Any
from uuid import uuid4

from .schemas import _token, digest_json

_STATES = {"running", "candidate_ready", "check_failed", "malformed_candidate", "timeout", "unavailable", "failed", "unknown"}
_UPDATABLE = {"state", "attempts", "result_ref", "error", "finished_at"}


class OperationConflict(Exception):
    """A request ID was reused with a different immutable assignment/config."""


@dataclass(frozen=True)
class OperationRecord:
    request_id: str
    operation_id: str
    assignment_digest: str
    config_fingerprint: str
    state: str
    started_at: float
    updated_at: float
    attempts: int = 0
    result_ref: str | None = None
    error: str | None = None
    finished_at: float | None = None

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_dict(cls, raw: Any) -> "OperationRecord":
        return cls(**raw)


class OperationLedger:
    def __init__(self, root: str | Path):
        self.root = Path(root).resolve()
        self.ops_dir = self.root / "ops"
        self.ops_dir.mkdir(parents=True, exist_ok=True)

    def _path(self, request_id: str) -> Path:
        safe = _token(request_id, "request_id")
        return self.ops_dir / f"{safe}.json"

    @staticmethod
    def fingerprint(request_id: str, assignment_digest: str, config_fingerprint: str) -> str:
        return digest_json(
            {
                "request_id": request_id,
                "assignment_digest": assignment_digest,
                "config_fingerprint": config_fingerprint,
            }
        )

    def begin(
        self, request_id: str, assignment_digest: str, config_fingerprint: str
    ) -> OperationRecord:
        """Start an operation; an identical retry returns the existing record."""
        path = self._path(request_id)
        fingerprint = self.fingerprint(request_id, assignment_digest, config_fingerprint)
        if path.exists():
            existing = self.load(request_id)
            if OperationLedger.fingerprint(
                existing.request_id, existing.assignment_digest, existing.config_fingerprint
            ) != fingerprint:
                raise OperationConflict(f"request_id {request_id!r} was reused with different inputs")
            return existing
        now = time.time()
        record = OperationRecord(
            request_id=request_id,
            operation_id="op-" + uuid4().hex,
            assignment_digest=assignment_digest,
            config_fingerprint=config_fingerprint,
            state="running",
            started_at=now,
            updated_at=now,
        )
        self._write(path, record)
        return record

    def load(self, request_id: str) -> OperationRecord:
        path = self._path(request_id)
        try:
            raw = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as exc:
            raise ValueError(f"cannot read operation {request_id!r}: {exc}") from exc
        return OperationRecord.from_dict(raw)

    def find(self, operation_id: str) -> OperationRecord | None:
        for path in sorted(self.ops_dir.glob("*.json")):
            try:
                record = OperationRecord.from_dict(json.loads(path.read_text(encoding="utf-8")))
            except (OSError, json.JSONDecodeError, TypeError):
                continue
            if record.operation_id == operation_id:
                return record
        return None

    def update(self, operation_id: str, **fields: Any) -> OperationRecord:
        unknown = set(fields) - _UPDATABLE
        if unknown:
            raise ValueError(f"unsupported operation fields: {', '.join(sorted(unknown))}")
        current = self.find(operation_id)
        if current is None:
            raise ValueError(f"unknown operation {operation_id!r}")
        if "state" in fields and fields["state"] not in _STATES:
            raise ValueError(f"unsupported operation state: {fields['state']!r}")
        updated = OperationRecord(
            **{**current.to_dict(), **fields, "updated_at": time.time()}
        )
        self._write(self._path(current.request_id), updated)
        return updated

    def reconcile(self) -> list[OperationRecord]:
        """Mark interrupted running operations as unknown; never blindly rerun."""
        changed: list[OperationRecord] = []
        for path in sorted(self.ops_dir.glob("*.json")):
            try:
                record = OperationRecord.from_dict(json.loads(path.read_text(encoding="utf-8")))
            except (OSError, json.JSONDecodeError, TypeError):
                continue
            if record.state == "running":
                updated = self.update(
                    record.operation_id, state="unknown", error="interrupted; outcome unresolved"
                )
                changed.append(updated)
        return changed

    def private_dir(self, operation_id: str) -> Path:
        path = self.root / "private" / _token(operation_id, "operation_id")
        path.mkdir(parents=True, exist_ok=True)
        return path

    def _write(self, path: Path, record: OperationRecord) -> None:
        payload = json.dumps(record.to_dict(), sort_keys=True, separators=(",", ":")).encode("utf-8")
        fd, temporary = tempfile.mkstemp(prefix=".op-", suffix=".tmp", dir=str(self.ops_dir))
        try:
            with os.fdopen(fd, "wb") as handle:
                handle.write(payload)
                handle.flush()
                os.fsync(handle.fileno())
            os.replace(temporary, path)
        except BaseException:
            try:
                os.unlink(temporary)
            except OSError:
                pass
            raise
