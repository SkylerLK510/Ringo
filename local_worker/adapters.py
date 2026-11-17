"""Inference adapters with explicit local-only routing and real cancellation."""

from __future__ import annotations

import http.client
import json
from abc import ABC, abstractmethod
from dataclasses import asdict, dataclass, field
from typing import Any, Callable
from urllib.parse import urlsplit
from uuid import uuid4

from .config import Config
from .control import StopControl, SocketGuard
from .schemas import digest_json


class InferenceCancelled(Exception):
    """A call was stopped by the job deadline or an explicit cancellation."""


class InferenceUnavailable(Exception):
    """The configured local runtime did not produce a usable response."""


class UnsupportedPolicy(Exception):
    """A routing policy is recognized but not implemented yet."""


@dataclass(frozen=True)
class InferenceUsage:
    prompt_eval_count: int | None = None
    prompt_eval_cached_count: int | None = None
    eval_count: int | None = None
    total_duration_ns: int | None = None
    load_duration_ns: int | None = None
    reported: bool = False

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


@dataclass(frozen=True)
class InferenceResult:
    text: str
    usage: InferenceUsage
    operation_id: str
    fingerprint: str
    backend: str


class InferenceAdapter(ABC):
    @abstractmethod
    def generate(self, prompt: str, stop: StopControl) -> InferenceResult:
        """Make one bounded call. A timeout consumes the attempt."""

    @abstractmethod
    def describe(self) -> dict[str, Any]:
        """Return worker identity and digest-verification status for provenance."""


class OllamaHttpAdapter(InferenceAdapter):
    """Stdlib adapter for the documented local Ollama /api/generate API.

    Streaming NDJSON is used deliberately: a disconnect is the only documented
    way to stop server-side generation, and the SocketGuard makes that
    disconnect happen even while a read is blocked.
    """

    def __init__(self, config: Config):
        if config.policy != "local":
            raise UnsupportedPolicy(f"policy {config.policy!r} is not implemented")
        self._config = config
        parsed = urlsplit(config.endpoint)
        self._host = parsed.hostname or "127.0.0.1"
        self._port = parsed.port or 11434
        self._digest_verified = False

    def describe(self) -> dict[str, Any]:
        return {
            "adapter": "ollama-http",
            **self._config.worker_description(),
            "digest_verified": self._digest_verified,
        }

    def verify_model_digest(self, stop: StopControl) -> None:
        """Confirm the configured digest against the local runtime before inference.

        Ollama's documented /api/tags response reports each model's content
        digest. A tag can be repointed to different weights, so recording a
        digest in the config is not a pin unless the runtime agrees with it.
        Verification succeeds once per adapter (one loaded model is assumed);
        an absent, malformed or mismatched digest fails closed.
        """
        if self._digest_verified or self._config.model_digest is None:
            return
        timeout = max(0.1, min(5.0, stop.deadline.remaining()))
        conn = http.client.HTTPConnection(self._host, self._port, timeout=timeout)
        guard: SocketGuard | None = None
        try:
            conn.connect()
            owned = conn.sock
            if owned is None:
                raise InferenceUnavailable("no owned HTTP socket to verify the model")
            guard = SocketGuard(owned, stop)
            guard.start()
            conn.request("GET", "/api/tags")
            response = conn.getresponse()
            if response.status != 200:
                response.read(min(4_096, self._config.max_stream_bytes))
                raise InferenceUnavailable(f"Ollama /api/tags returned HTTP {response.status}")
            raw = response.read(min(1_048_576, self._config.max_stream_bytes))
            if stop.stopped or guard.triggered:
                raise InferenceCancelled("model verification stopped by the deadline")
            try:
                payload = json.loads(raw.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError) as exc:
                raise InferenceUnavailable(f"invalid /api/tags response: {exc}") from exc
            reported: str | None = None
            if isinstance(payload, dict) and isinstance(payload.get("models"), list):
                for entry in payload["models"]:
                    if not isinstance(entry, dict):
                        continue
                    if entry.get("name") == self._config.model or entry.get("model") == self._config.model:
                        candidate = entry.get("digest")
                        reported = candidate if isinstance(candidate, str) else None
                        break
            expected = self._config.model_digest.lower().removeprefix("sha256:")
            actual = reported.lower().removeprefix("sha256:") if reported is not None else None
            if actual != expected:
                raise InferenceUnavailable(
                    "configured model digest does not match the runtime model; "
                    "refusing to attribute output to unverified weights"
                )
            self._digest_verified = True
        except (OSError, http.client.HTTPException) as exc:
            if stop.stopped or (guard is not None and guard.triggered):
                raise InferenceCancelled(f"model verification stopped: {exc}") from exc
            raise InferenceUnavailable(f"model verification failed: {exc}") from exc
        finally:
            if guard is not None:
                guard.finish()
            conn.close()

    def generate(self, prompt: str, stop: StopControl) -> InferenceResult:
        if stop.stopped:
            raise InferenceCancelled("deadline expired before the call started")
        self.verify_model_digest(stop)
        if stop.stopped:
            raise InferenceCancelled("deadline expired before generation started")
        request: dict[str, Any] = {
            "model": self._config.model,
            "prompt": prompt,
            "stream": True,
            "format": "json",
            "keep_alive": self._config.keep_alive,
            "options": {
                "num_ctx": self._config.context_tokens,
                "num_predict": self._config.max_output_tokens,
                "temperature": self._config.temperature,
                "seed": self._config.seed,
            },
        }
        request["options"].update(self._config.options)
        fingerprint = digest_json(request)
        operation_id = "call-" + uuid4().hex[:16]
        body = json.dumps(request).encode("utf-8")
        conn = http.client.HTTPConnection(self._host, self._port, timeout=5.0)
        guard: SocketGuard | None = None
        try:
            conn.connect()
            owned = conn.sock
            if owned is None:
                raise InferenceUnavailable("no owned HTTP socket to guard")
            guard = SocketGuard(owned, stop)
            guard.start()
            conn.request("POST", "/api/generate", body=body, headers={"Content-Type": "application/json"})
            response = conn.getresponse()
            if response.status != 200:
                response.read(min(4_096, self._config.max_stream_bytes))
                raise InferenceUnavailable(f"Ollama returned HTTP {response.status}")

            chunks: list[str] = []
            total = 0
            usage = InferenceUsage()
            while True:
                if stop.stopped:
                    raise InferenceCancelled("deadline or explicit stop fired during generation")
                line = response.readline()
                if not line:
                    break
                total += len(line)
                if total > self._config.max_stream_bytes:
                    raise InferenceUnavailable("stream output exceeded the byte limit")
                if not line.strip():
                    continue
                try:
                    event = json.loads(line.decode("utf-8"))
                except (UnicodeDecodeError, json.JSONDecodeError) as exc:
                    raise InferenceUnavailable(f"invalid stream event: {exc}") from exc
                if not isinstance(event, dict):
                    raise InferenceUnavailable("stream event is not an object")
                piece = event.get("response")
                if isinstance(piece, str):
                    chunks.append(piece)
                if event.get("done") is True:
                    usage = InferenceUsage(
                        prompt_eval_count=_count(event.get("prompt_eval_count")),
                        prompt_eval_cached_count=_count(event.get("prompt_eval_cached_count")),
                        eval_count=_count(event.get("eval_count")),
                        total_duration_ns=_count(event.get("total_duration")),
                        load_duration_ns=_count(event.get("load_duration")),
                        reported=True,
                    )
                    break
            if stop.stopped:
                raise InferenceCancelled("deadline fired before the stream completed")
            return InferenceResult(
                text="".join(chunks),
                usage=usage,
                operation_id=operation_id,
                fingerprint=fingerprint,
                backend="ollama-http",
            )
        except (OSError, http.client.HTTPException) as exc:
            if stop.stopped or (guard is not None and guard.triggered):
                raise InferenceCancelled(f"generation stopped: {exc}") from exc
            raise InferenceUnavailable(f"Ollama request failed: {exc}") from exc
        finally:
            if guard is not None:
                guard.finish()
            conn.close()


def _count(value: Any) -> int | None:
    if isinstance(value, bool):
        return None
    if isinstance(value, int) and value >= 0:
        return value
    return None


class ScriptedAdapter(InferenceAdapter):
    """Deterministic in-process adapter used by fake-runtime tests.

    Each script item is either a string result, an exception to raise, or a
    callable ``(prompt, stop) -> str``. A callable may block until the stop is
    signalled, which exercises the deadline path without a live model.
    """

    def __init__(self, script: list[Any], *, name: str = "fake-local"):
        self._script = list(script)
        self._name = name
        self.prompts: list[str] = []
        self.cancelled = 0

    @property
    def remaining(self) -> int:
        return len(self._script)

    def describe(self) -> dict[str, Any]:
        return {
            "adapter": self._name,
            "model": "fake",
            "policy": "local",
            "digest_configured": True,
            "digest_verified": True,
        }

    def generate(self, prompt: str, stop: StopControl) -> InferenceResult:
        if stop.stopped:
            self.cancelled += 1
            raise InferenceCancelled("deadline expired before a fake call")
        if not self._script:
            raise InferenceUnavailable("fake adapter script exhausted")
        self.prompts.append(prompt)
        item = self._script.pop(0)
        try:
            if isinstance(item, BaseException):
                raise item
            if callable(item):
                text = item(prompt, stop)
            elif isinstance(item, str):
                text = item
            else:
                raise TypeError("script items must be strings, exceptions or callables")
        except InferenceCancelled:
            self.cancelled += 1
            raise
        if stop.stopped:
            self.cancelled += 1
            raise InferenceCancelled("fake call outlived the deadline")
        return InferenceResult(
            text=text,
            usage=InferenceUsage(reported=False),
            operation_id="call-" + uuid4().hex[:16],
            fingerprint=digest_json({"prompt": prompt, "text": text}),
            backend=self._name,
        )


def build_adapter(config: Config) -> InferenceAdapter:
    """Build the local adapter only. Cloud/auto are explicit refusals."""
    if config.policy == "local":
        return OllamaHttpAdapter(config)
    raise UnsupportedPolicy(
        f"policy {config.policy!r} is defined but not implemented; local-only is the default"
    )
