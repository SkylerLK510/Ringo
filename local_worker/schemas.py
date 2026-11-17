"""Versioned assignment and result contracts for the local worker.

Only plain JSON is accepted. Unknown fields are rejected so a caller cannot
smuggle instructions or capabilities past validation. Digests are canonical
sha256 digests of UTF-8 JSON with sorted keys and no whitespace.
"""

from __future__ import annotations

import hashlib
import json
import ntpath
import os
import re
from dataclasses import asdict, dataclass, field
from pathlib import PurePosixPath
from typing import Any, Mapping

ASSIGNMENT_SCHEMA_VERSION = 1
RESULT_SCHEMA_VERSION = 1

MAX_ASSIGNMENT_BYTES = 1_048_576
MAX_SLICE_BYTES = 131_072
MAX_OBJECTIVE_CHARS = 20_000
MAX_CONSTRAINTS = 100
MAX_PATHS = 200
MAX_SLICES = 200
MAX_CHECKS = 2
MAX_ARGV_ITEMS = 32
MAX_ENV_ITEMS = 32
MAX_ERRORS = 24
MAX_ERROR_CHARS = 2_000

_TOKEN_RE = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$")
_DIGEST_RE = re.compile(r"^sha256:[0-9a-f]{64}$")
_ROUTING_POLICIES = ("local", "cloud", "auto")


class SchemaError(ValueError):
    """Raised when an assignment, result or contained value is not valid."""


def canonical_bytes(value: Any) -> bytes:
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=False
    ).encode("utf-8")


def digest_bytes(data: bytes) -> str:
    return "sha256:" + hashlib.sha256(data).hexdigest()


def digest_json(value: Any) -> str:
    return digest_bytes(canonical_bytes(value))


def strict_json_loads(text: str) -> Any:
    """Parse JSON with duplicate keys and NaN/Infinity rejected."""

    def pairs(items: list[tuple[str, Any]]) -> dict[str, Any]:
        result: dict[str, Any] = {}
        for key, value in items:
            if key in result:
                raise SchemaError(f"duplicate JSON key: {key}")
            result[key] = value
        return result

    def constant(value: str) -> Any:
        raise SchemaError(f"invalid JSON constant: {value}")

    try:
        return json.loads(text, object_pairs_hook=pairs, parse_constant=constant)
    except (ValueError, RecursionError) as exc:
        raise SchemaError(f"invalid JSON: {exc}") from exc


def _dict(value: Any, name: str) -> dict[str, Any]:
    if not isinstance(value, dict):
        raise SchemaError(f"{name} must be a JSON object")
    return value


def _exact_keys(raw: Mapping[str, Any], allowed: set[str], required: set[str], name: str) -> None:
    unknown = sorted(set(raw) - allowed)
    if unknown:
        raise SchemaError(f"{name} has unknown fields: {', '.join(unknown)}")
    missing = sorted(required - set(raw))
    if missing:
        raise SchemaError(f"{name} is missing fields: {', '.join(missing)}")


def _text(value: Any, name: str, *, max_chars: int, allow_empty: bool = False) -> str:
    if not isinstance(value, str):
        raise SchemaError(f"{name} must be a string")
    if "\x00" in value:
        raise SchemaError(f"{name} contains a NUL byte")
    if len(value) > max_chars:
        raise SchemaError(f"{name} exceeds {max_chars} characters")
    if not value and not allow_empty:
        raise SchemaError(f"{name} must not be empty")
    return value


def _token(value: Any, name: str) -> str:
    text = _text(value, name, max_chars=128)
    if not _TOKEN_RE.fullmatch(text):
        raise SchemaError(f"{name} is not a safe identifier")
    return text


def _digest(value: Any, name: str) -> str:
    text = _text(value, name, max_chars=71)
    if not _DIGEST_RE.fullmatch(text):
        raise SchemaError(f"{name} must be a sha256 hex digest")
    return text


def _list(value: Any, name: str, *, limit: int) -> list[Any]:
    if not isinstance(value, list):
        raise SchemaError(f"{name} must be a JSON array")
    if len(value) > limit:
        raise SchemaError(f"{name} exceeds {limit} entries")
    return value


def safe_relative_path(value: Any, name: str) -> str:
    """Normalize a POSIX-style relative path and reject escapes or ambiguity."""
    text = _text(value, name, max_chars=512)
    if "\\" in text or "\x00" in text:
        raise SchemaError(f"{name} must use POSIX separators")
    if ntpath.isabs(text) or os.path.isabs(text) or text.startswith("/"):
        raise SchemaError(f"{name} must be relative")
    parts = text.split("/")
    if any(part in ("", ".", "..") for part in parts):
        raise SchemaError(f"{name} contains an empty, dot or parent segment")
    if any(part == ".git" for part in parts):
        raise SchemaError(f"{name} may not address git metadata")
    normalized = str(PurePosixPath(*parts))
    if normalized != text:
        raise SchemaError(f"{name} is not normalized")
    return normalized


@dataclass(frozen=True)
class FileSlice:
    """An owner-supplied, identity-bound text slice of a source file."""

    path: str
    content: str
    digest: str

    @classmethod
    def from_dict(cls, raw: Any) -> "FileSlice":
        value = _dict(raw, "file slice")
        _exact_keys(value, {"path", "content", "digest"}, {"path", "content", "digest"}, "file slice")
        path = safe_relative_path(value["path"], "file slice.path")
        content = _text(value["content"], "file slice.content", max_chars=MAX_SLICE_BYTES, allow_empty=True)
        expected = digest_bytes(content.encode("utf-8"))
        supplied = _digest(value["digest"], "file slice.digest")
        if supplied != expected:
            raise SchemaError(f"file slice digest mismatch for {path}")
        return cls(path=path, content=content, digest=supplied)

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True)
class CheckSpec:
    """A host-configured deterministic check. The model cannot change it."""

    id: str
    argv: tuple[str, ...]
    cwd: str = "."
    env: dict[str, str] = field(default_factory=dict)
    timeout_s: float = 60.0

    @classmethod
    def from_dict(cls, raw: Any) -> "CheckSpec":
        value = _dict(raw, "check")
        _exact_keys(value, {"id", "argv", "cwd", "env", "timeout_s"}, {"id", "argv"}, "check")
        check_id = _token(value["id"], "check.id")
        items = _list(value["argv"], "check.argv", limit=MAX_ARGV_ITEMS)
        if not items:
            raise SchemaError("check.argv must not be empty")
        argv = tuple(
            _text(item, "check.argv item", max_chars=1_024, allow_empty=False) for item in items
        )
        cwd_raw = value.get("cwd", ".")
        cwd = "." if cwd_raw == "." else safe_relative_path(cwd_raw, "check.cwd")
        env_raw = _dict(value.get("env", {}), "check.env")
        if len(env_raw) > MAX_ENV_ITEMS:
            raise SchemaError(f"check.env exceeds {MAX_ENV_ITEMS} entries")
        env: dict[str, str] = {}
        for key, item in env_raw.items():
            safe_key = _text(key, "check.env key", max_chars=128)
            if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", safe_key):
                raise SchemaError("check.env key is not a safe variable name")
            env[safe_key] = _text(item, "check.env value", max_chars=4_096, allow_empty=True)
        timeout = value.get("timeout_s", 60.0)
        if isinstance(timeout, bool) or not isinstance(timeout, (int, float)):
            raise SchemaError("check.timeout_s must be a number")
        timeout_s = float(timeout)
        if not 0 < timeout_s <= 180:
            raise SchemaError("check.timeout_s must be between 0 and 180 seconds")
        return cls(id=check_id, argv=argv, cwd=cwd, env=env, timeout_s=timeout_s)

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True)
class Assignment:
    """Immutable, versioned assignment input for one bounded local job."""

    schema_version: int
    assignment_id: str
    objective: str
    constraints: tuple[str, ...]
    allowed_paths: tuple[str, ...]
    file_slices: tuple[FileSlice, ...]
    checks: tuple[CheckSpec, ...]
    routing_policy: str
    task_id: str | None = None
    base_revision: str | None = None
    requested_by: str | None = None

    @classmethod
    def from_dict(cls, raw: Any) -> "Assignment":
        value = _dict(raw, "assignment")
        _exact_keys(
            value,
            {
                "schema_version",
                "assignment_id",
                "task_id",
                "base_revision",
                "requested_by",
                "objective",
                "constraints",
                "allowed_paths",
                "file_slices",
                "checks",
                "routing_policy",
            },
            {
                "schema_version",
                "assignment_id",
                "objective",
                "constraints",
                "allowed_paths",
                "file_slices",
                "checks",
                "routing_policy",
            },
            "assignment",
        )
        version = value["schema_version"]
        if version != ASSIGNMENT_SCHEMA_VERSION:
            raise SchemaError(f"unsupported assignment schema version: {version!r}")
        assignment_id = _token(value["assignment_id"], "assignment.assignment_id")
        objective = _text(value["objective"], "assignment.objective", max_chars=MAX_OBJECTIVE_CHARS)
        constraints = tuple(
            _text(item, "assignment.constraints item", max_chars=2_000)
            for item in _list(value["constraints"], "assignment.constraints", limit=MAX_CONSTRAINTS)
        )
        allowed = tuple(
            safe_relative_path(item, "assignment.allowed_paths item")
            for item in _list(value["allowed_paths"], "assignment.allowed_paths", limit=MAX_PATHS)
        )
        if not allowed:
            raise SchemaError("assignment.allowed_paths must not be empty")
        slices = tuple(
            FileSlice.from_dict(item)
            for item in _list(value["file_slices"], "assignment.file_slices", limit=MAX_SLICES)
        )
        slice_paths = [item.path for item in slices]
        if len(slice_paths) != len(set(slice_paths)):
            raise SchemaError("assignment.file_slices has duplicate paths")
        for path in slice_paths:
            if path not in allowed:
                raise SchemaError(f"file slice path is not allowed: {path}")
        if sum(len(item.content.encode("utf-8")) for item in slices) > MAX_ASSIGNMENT_BYTES:
            raise SchemaError("assignment file slices exceed the total byte limit")
        checks = tuple(
            CheckSpec.from_dict(item)
            for item in _list(value["checks"], "assignment.checks", limit=MAX_CHECKS)
        )
        check_ids = [item.id for item in checks]
        if len(check_ids) != len(set(check_ids)):
            raise SchemaError("assignment.checks has duplicate ids")
        policy = _text(value["routing_policy"], "assignment.routing_policy", max_chars=16)
        if policy not in _ROUTING_POLICIES:
            raise SchemaError(f"unsupported routing policy: {policy}")

        def optional(name: str) -> str | None:
            if name not in value:
                return None
            return _text(value[name], f"assignment.{name}", max_chars=512)

        return cls(
            schema_version=version,
            assignment_id=assignment_id,
            objective=objective,
            constraints=constraints,
            allowed_paths=allowed,
            file_slices=slices,
            checks=checks,
            routing_policy=policy,
            task_id=optional("task_id"),
            base_revision=optional("base_revision"),
            requested_by=optional("requested_by"),
        )

    def to_dict(self) -> dict[str, Any]:
        data = asdict(self)
        return {key: value for key, value in data.items() if value is not None}

    def slice_by_path(self, path: str) -> FileSlice | None:
        for item in self.file_slices:
            if item.path == path:
                return item
        return None

    def digest(self) -> str:
        return digest_json(self.to_dict())


@dataclass(frozen=True)
class CheckResult:
    check_id: str
    status: str
    exit_code: int | None
    output_digest: str
    duration_ms: int
    detail: str = ""

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True)
class AttemptRecord:
    index: int
    status: str
    operation_id: str
    fingerprint: str
    duration_ms: int
    usage: dict[str, Any]
    check_ids: tuple[str, ...] = ()

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True)
class Result:
    schema_version: int
    assignment_id: str
    assignment_digest: str
    operation_id: str
    status: str
    phase: str
    policy: str
    worker: dict[str, Any]
    deadline_s: float
    elapsed_ms: int
    attempts: tuple[AttemptRecord, ...]
    check_results: tuple[CheckResult, ...]
    changed_files: tuple[str, ...]
    candidate_digest: str | None
    candidate_ref: str | None
    escalation_needed: bool
    errors: tuple[str, ...]
    usage: dict[str, Any]
    cleanup: dict[str, Any]

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)

    @classmethod
    def from_dict(cls, raw: Any) -> "Result":
        value = _dict(raw, "result")
        if value.get("schema_version") != RESULT_SCHEMA_VERSION:
            raise SchemaError("unsupported result schema version")
        return cls(
            schema_version=RESULT_SCHEMA_VERSION,
            assignment_id=_token(value["assignment_id"], "result.assignment_id"),
            assignment_digest=_digest(value["assignment_digest"], "result.assignment_digest"),
            operation_id=_token(value["operation_id"], "result.operation_id"),
            status=_text(value["status"], "result.status", max_chars=64),
            phase=_text(value["phase"], "result.phase", max_chars=64),
            policy=_text(value["policy"], "result.policy", max_chars=16),
            worker=_dict(value["worker"], "result.worker"),
            deadline_s=float(value["deadline_s"]),
            elapsed_ms=int(value["elapsed_ms"]),
            attempts=tuple(AttemptRecord(**item) for item in value["attempts"]),
            check_results=tuple(CheckResult(**item) for item in value["check_results"]),
            changed_files=tuple(value["changed_files"]),
            candidate_digest=value.get("candidate_digest"),
            candidate_ref=value.get("candidate_ref"),
            escalation_needed=bool(value["escalation_needed"]),
            errors=tuple(value["errors"]),
            usage=_dict(value["usage"], "result.usage"),
            cleanup=_dict(value["cleanup"], "result.cleanup"),
        )
