"""Strict parsing and validation of model-produced candidate output."""

from __future__ import annotations

import json
from dataclasses import asdict, dataclass, field
from typing import Any

from .config import Config
from .schemas import (
    Assignment,
    SchemaError,
    _dict,
    _digest,
    _exact_keys,
    _list,
    _text,
    digest_json,
    safe_relative_path,
    strict_json_loads,
)

CANDIDATE_SCHEMA_VERSION = 1
_KINDS = ("text", "patch")
_BINARY_SUFFIXES = (
    ".png", ".jpg", ".jpeg", ".gif", ".pdf", ".zip", ".gz", ".exe", ".dll",
    ".so", ".bin", ".pyc", ".woff", ".woff2", ".ttf", ".ico",
)


class CandidateError(ValueError):
    """A model output was well-formed transport but not an acceptable candidate."""

    def __init__(self, code: str, detail: str):
        super().__init__(f"{code}: {detail}")
        self.code = code
        self.detail = detail


@dataclass(frozen=True)
class CandidateFile:
    path: str
    content: str
    preimage_digest: str

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True)
class Candidate:
    schema_version: int
    kind: str
    answer: str
    files: tuple[CandidateFile, ...] = ()
    uncertainties: tuple[str, ...] = ()
    citations: tuple[str, ...] = ()

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)

    def digest(self) -> str:
        return digest_json(self.to_dict())

    def changed_paths(self) -> tuple[str, ...]:
        return tuple(item.path for item in self.files)


def parse_candidate(raw_text: str, assignment: Assignment, config: Config) -> Candidate:
    """Parse one model response. No second model call is made to repair output."""
    encoded = raw_text.encode("utf-8")
    if len(encoded) > config.max_candidate_bytes:
        raise CandidateError("oversized_candidate", "model output exceeds the byte limit")
    try:
        raw = strict_json_loads(raw_text)
    except SchemaError as exc:
        raise CandidateError("malformed_json", f"output is not valid JSON: {exc}") from exc
    value = _dict(raw, "candidate")
    _exact_keys(
        value,
        {"schema_version", "kind", "answer", "files", "uncertainties", "citations"},
        {"schema_version", "kind", "answer"},
        "candidate",
    )
    if value["schema_version"] != CANDIDATE_SCHEMA_VERSION:
        raise CandidateError("unsupported_version", "unsupported candidate schema version")
    kind = _text(value["kind"], "candidate.kind", max_chars=16)
    if kind not in _KINDS:
        raise CandidateError("unsupported_kind", f"unsupported candidate kind: {kind}")
    answer = _text(value["answer"], "candidate.answer", max_chars=20_000, allow_empty=True)

    files: tuple[CandidateFile, ...] = ()
    raw_files = _list(value.get("files", []), "candidate.files", limit=config.max_files)
    if kind == "patch" and not raw_files:
        raise CandidateError("empty_patch", "a patch candidate must contain at least one file")
    if kind == "text" and raw_files:
        raise CandidateError("unexpected_files", "a text candidate may not contain files")
    parsed_files: list[CandidateFile] = []
    seen: set[str] = set()
    for item in raw_files:
        entry = _dict(item, "candidate file")
        _exact_keys(
            entry,
            {"path", "content", "preimage_digest"},
            {"path", "content", "preimage_digest"},
            "candidate file",
        )
        try:
            path = safe_relative_path(entry["path"], "candidate file.path")
        except SchemaError as exc:
            raise CandidateError("unsafe_path", str(exc)) from exc
        if path in seen:
            raise CandidateError("duplicate_path", f"duplicate file path: {path}")
        seen.add(path)
        if path not in assignment.allowed_paths:
            raise CandidateError("unexpected_path", f"path is not allowed: {path}")
        preimage = assignment.slice_by_path(path)
        if preimage is None:
            raise CandidateError("unknown_preimage", f"no supplied preimage for {path}")
        supplied_digest = _digest(entry["preimage_digest"], "candidate file.preimage_digest")
        if supplied_digest != preimage.digest:
            raise CandidateError("stale_preimage", f"preimage digest mismatch for {path}")
        content = _text(
            entry["content"], "candidate file.content",
            max_chars=config.max_file_bytes, allow_empty=True,
        )
        if path.lower().endswith(_BINARY_SUFFIXES):
            raise CandidateError("binary_edit", f"binary file edits are not supported: {path}")
        parsed_files.append(
            CandidateFile(path=path, content=content, preimage_digest=supplied_digest)
        )
    files = tuple(parsed_files)

    uncertainties = tuple(
        _text(item, "candidate.uncertainties item", max_chars=500)
        for item in _list(value.get("uncertainties", []), "candidate.uncertainties", limit=20)
    )
    citations = tuple(
        _text(item, "candidate.citations item", max_chars=512)
        for item in _list(value.get("citations", []), "candidate.citations", limit=50)
    )
    for citation in citations:
        cited_path = citation.split(":", 1)[0]
        if cited_path not in assignment.allowed_paths:
            raise CandidateError("unsafe_citation", f"citation is outside allowed paths: {citation}")

    return Candidate(
        schema_version=CANDIDATE_SCHEMA_VERSION,
        kind=kind,
        answer=answer,
        files=files,
        uncertainties=uncertainties,
        citations=citations,
    )
