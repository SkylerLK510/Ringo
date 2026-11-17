"""Local execution configuration.

Configuration never names a cloud endpoint. `cloud` and `auto` are recognized
policy values so callers get an explicit refusal, not a silent remote call.
"""

from __future__ import annotations

from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import Any
from urllib.parse import urlsplit

from .schemas import SchemaError, _dict, _digest, _exact_keys, _text, digest_json, strict_json_loads

CONFIG_SCHEMA_VERSION = 1
MAX_CONFIG_BYTES = 65_536
_LOOPBACK_HOSTS = {"localhost", "127.0.0.1", "::1"}


class ConfigError(SchemaError):
    """Raised for an invalid or unsupported local-worker configuration."""


@dataclass(frozen=True)
class Config:
    schema_version: int
    policy: str
    endpoint: str
    model: str
    model_digest: str | None
    runtime_version: str | None
    quantization: str | None
    allow_unpinned_model: bool
    context_tokens: int
    max_output_tokens: int
    temperature: float
    seed: int
    keep_alive: str
    job_deadline_s: float
    cleanup_allowance_s: float
    attempt_deadline_s: float
    max_attempts: int
    max_check_suites: int
    max_candidate_bytes: int
    max_file_bytes: int
    max_files: int
    max_stream_bytes: int
    options: dict[str, Any] = field(default_factory=dict)

    @classmethod
    def from_dict(cls, raw: Any) -> "Config":
        try:
            return cls._from_dict(raw)
        except SchemaError as exc:
            if isinstance(exc, ConfigError):
                raise
            raise ConfigError(str(exc)) from exc

    @classmethod
    def _from_dict(cls, raw: Any) -> "Config":
        value = _dict(raw, "config")
        _exact_keys(
            value,
            {
                "schema_version",
                "policy",
                "endpoint",
                "model",
                "model_digest",
                "runtime_version",
                "quantization",
                "allow_unpinned_model",
                "context_tokens",
                "max_output_tokens",
                "temperature",
                "seed",
                "keep_alive",
                "job_deadline_s",
                "cleanup_allowance_s",
                "attempt_deadline_s",
                "max_attempts",
                "max_check_suites",
                "max_candidate_bytes",
                "max_file_bytes",
                "max_files",
                "max_stream_bytes",
                "options",
            },
            {"schema_version", "policy", "endpoint", "model"},
            "config",
        )
        if value["schema_version"] != CONFIG_SCHEMA_VERSION:
            raise ConfigError("unsupported config schema version")

        policy = _text(value["policy"], "config.policy", max_chars=16) if "policy" in value else "local"
        if policy not in ("local", "cloud", "auto"):
            raise ConfigError(f"unsupported routing policy: {policy}")

        endpoint = _text(value["endpoint"], "config.endpoint", max_chars=512)
        parsed = urlsplit(endpoint)
        if parsed.scheme != "http" or parsed.hostname not in _LOOPBACK_HOSTS:
            raise ConfigError("config.endpoint must be a loopback http URL")
        if parsed.username or parsed.password or parsed.path not in ("", "/") or parsed.query or parsed.fragment:
            raise ConfigError("config.endpoint must not carry credentials, a path, query or fragment")

        model = _text(value["model"], "config.model", max_chars=256)
        if model.endswith(":latest") or ":" not in model:
            raise ConfigError("config.model must be pinned to an explicit tag")
        allow_unpinned = bool(value.get("allow_unpinned_model", False))
        model_digest = value.get("model_digest")
        if model_digest is not None:
            model_digest = _digest(model_digest, "config.model_digest")
        elif not allow_unpinned:
            raise ConfigError("config.model_digest is required unless allow_unpinned_model is true")

        def number(name: str, default: float, *, low: float, high: float) -> float:
            raw = value.get(name, default)
            if isinstance(raw, bool) or not isinstance(raw, (int, float)):
                raise ConfigError(f"config.{name} must be a number")
            result = float(raw)
            if not low <= result <= high:
                raise ConfigError(f"config.{name} must be between {low} and {high}")
            return result

        def integer(name: str, default: int, *, low: int, high: int) -> int:
            result = int(number(name, default, low=low, high=high))
            return result

        context_tokens = integer("context_tokens", 4096, low=512, high=32_768)
        max_output_tokens = integer(
            "max_output_tokens", 1_024, low=1, high=max(1, context_tokens // 2)
        )
        job_deadline_s = number("job_deadline_s", 180.0, low=1.0, high=180.0)
        cleanup_allowance_s = number("cleanup_allowance_s", 10.0, low=0.0, high=30.0)
        attempt_deadline_s = number("attempt_deadline_s", 120.0, low=1.0, high=180.0)
        if attempt_deadline_s > job_deadline_s:
            raise ConfigError("config.attempt_deadline_s must not exceed job_deadline_s")

        max_attempts = integer("max_attempts", 2, low=2, high=2)
        max_check_suites = integer("max_check_suites", 2, low=2, high=2)
        options = _dict(value.get("options", {}), "config.options")
        reserved = {"num_ctx", "num_predict", "temperature", "seed"}
        if reserved.intersection(options):
            raise ConfigError("config.options cannot override validated generation settings")
        if len(options) > 32:
            raise ConfigError("config.options exceeds 32 entries")

        def optional_text(name: str) -> str | None:
            if name not in value or value[name] is None:
                return None
            return _text(value[name], f"config.{name}", max_chars=256)

        return cls(
            schema_version=CONFIG_SCHEMA_VERSION,
            policy=policy,
            endpoint=endpoint,
            model=model,
            model_digest=model_digest,
            runtime_version=optional_text("runtime_version"),
            quantization=optional_text("quantization"),
            allow_unpinned_model=allow_unpinned,
            context_tokens=context_tokens,
            max_output_tokens=max_output_tokens,
            temperature=number("temperature", 0.0, low=0.0, high=2.0),
            seed=integer("seed", 0, low=0, high=2_147_483_647),
            keep_alive=_text(value.get("keep_alive", "5m"), "config.keep_alive", max_chars=32),
            job_deadline_s=job_deadline_s,
            cleanup_allowance_s=cleanup_allowance_s,
            attempt_deadline_s=attempt_deadline_s,
            max_attempts=max_attempts,
            max_check_suites=max_check_suites,
            max_candidate_bytes=integer(
                "max_candidate_bytes", 65_536, low=1_024, high=1_048_576
            ),
            max_file_bytes=integer("max_file_bytes", 131_072, low=1_024, high=1_048_576),
            max_files=integer("max_files", 32, low=1, high=200),
            max_stream_bytes=integer(
                "max_stream_bytes", 1_048_576, low=4_096, high=16_777_216
            ),
            options=options,
        )

    @classmethod
    def from_file(cls, path: str | Path) -> "Config":
        data = Path(path).read_bytes()
        if len(data) > MAX_CONFIG_BYTES:
            raise ConfigError("config file is too large")
        try:
            raw = strict_json_loads(data.decode("utf-8"))
        except (UnicodeDecodeError, SchemaError) as exc:
            raise ConfigError(f"config file is not valid JSON: {exc}") from exc
        return cls.from_dict(raw)

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)

    def fingerprint(self) -> str:
        return digest_json(self.to_dict())

    def worker_description(self) -> dict[str, Any]:
        return {
            "policy": self.policy,
            "endpoint": self.endpoint,
            "model": self.model,
            "model_digest": self.model_digest,
            "runtime_version": self.runtime_version,
            "quantization": self.quantization,
            "digest_configured": self.model_digest is not None,
            "context_tokens": self.context_tokens,
            "max_output_tokens": self.max_output_tokens,
            "temperature": self.temperature,
            "seed": self.seed,
            "keep_alive": self.keep_alive,
        }


def load_assignment_file(path: str | Path) -> dict[str, Any]:
    from .schemas import MAX_ASSIGNMENT_BYTES

    data = Path(path).read_bytes()
    if len(data) > MAX_ASSIGNMENT_BYTES:
        raise SchemaError("assignment file is too large")
    try:
        return _dict(strict_json_loads(data.decode("utf-8")), "assignment")
    except (UnicodeDecodeError, SchemaError) as exc:
        raise SchemaError(f"assignment file is not valid JSON: {exc}") from exc
