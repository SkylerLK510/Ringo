import json
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from local_worker.adapters import InferenceCancelled, InferenceUnavailable, OllamaHttpAdapter
from local_worker.control import Deadline, StopControl

from local_worker_support import MODEL_DIGEST, make_config


class _OllamaHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path != "/api/tags":
            self.send_error(404)
            return
        payload = {
            "models": [
                {
                    "name": "qwen3.5:4b-q4_k_m",
                    "model": "qwen3.5:4b-q4_k_m",
                    "digest": self.server.tags_digest,  # type: ignore[attr-defined]
                }
            ]
        }
        raw = json.dumps(payload).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        self.rfile.read(length)
        self.send_response(200)
        self.send_header("Content-Type", "application/x-ndjson")
        self.end_headers()
        self.wfile.write(b'{"response":"partial","done":false}\n')
        self.wfile.flush()
        self.server.started.set()  # type: ignore[attr-defined]
        if self.server.complete:  # type: ignore[attr-defined]
            final = {
                "response": " done",
                "done": True,
                "prompt_eval_count": 12,
                "prompt_eval_cached_count": 3,
                "eval_count": 4,
                "total_duration": 1000,
                "load_duration": 100,
            }
            self.wfile.write(json.dumps(final).encode() + b"\n")
            self.wfile.flush()
            self.server.stopped.set()  # type: ignore[attr-defined]
            return
        while not self.server.release.is_set():  # type: ignore[attr-defined]
            try:
                self.wfile.write(b'{"response":"more","done":false}\n')
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, OSError):
                self.server.disconnected.set()  # type: ignore[attr-defined]
                break
            time.sleep(0.02)
        self.server.stopped.set()  # type: ignore[attr-defined]

    def log_message(self, *args):
        pass


class _Server:
    def __init__(self, *, complete: bool = False, tags_digest: str | None = None):
        self.tags_digest = tags_digest or MODEL_DIGEST.removeprefix("sha256:")
        self.started = threading.Event()
        self.disconnected = threading.Event()
        self.stopped = threading.Event()
        self.release = threading.Event()

        def handler(*args, **kwargs):
            instance = _OllamaHandler(*args, **kwargs)
            instance.server.complete = complete  # type: ignore[attr-defined]
            return instance

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        self.httpd.started = self.started  # type: ignore[attr-defined]
        self.httpd.disconnected = self.disconnected  # type: ignore[attr-defined]
        self.httpd.stopped = self.stopped  # type: ignore[attr-defined]
        self.httpd.release = self.release  # type: ignore[attr-defined]
        self.httpd.complete = complete  # type: ignore[attr-defined]
        self.httpd.tags_digest = self.tags_digest  # type: ignore[attr-defined]
        self.thread = threading.Thread(target=self.httpd.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *args):
        self.release.set()
        self.httpd.shutdown()
        self.httpd.server_close()
        self.thread.join(timeout=2.0)

    @property
    def endpoint(self) -> str:
        host, port = self.httpd.server_address[:2]
        return f"http://{host}:{port}"


class OllamaHttpTests(unittest.TestCase):
    def test_deadline_severs_request_and_server_stops_generation(self):
        with _Server() as server:
            adapter = OllamaHttpAdapter(make_config(endpoint=server.endpoint))
            stop = StopControl(Deadline(0.3))
            with self.assertRaises(InferenceCancelled):
                adapter.generate("prompt", stop)
            self.assertTrue(server.started.wait(2.0))
            self.assertTrue(server.disconnected.wait(2.0), "server never observed the client disconnect")
            self.assertTrue(server.stopped.wait(2.0), "server-side generation did not stop")

    def test_streamed_usage_fields_are_reported(self):
        with _Server(complete=True) as server:
            adapter = OllamaHttpAdapter(make_config(endpoint=server.endpoint))
            result = adapter.generate("prompt", StopControl(Deadline(5.0)))
            self.assertEqual(result.text, "partial done")
            self.assertTrue(result.usage.reported)
            self.assertEqual(result.usage.prompt_eval_count, 12)
            self.assertEqual(result.usage.prompt_eval_cached_count, 3)
            self.assertEqual(result.usage.eval_count, 4)

    def test_model_digest_is_verified_before_generation(self):
        with _Server(complete=True) as server:
            adapter = OllamaHttpAdapter(make_config(endpoint=server.endpoint))
            adapter.generate("prompt", StopControl(Deadline(5.0)))
            self.assertTrue(adapter.describe()["digest_verified"])

    def test_mismatched_model_digest_refuses_without_generation(self):
        with _Server(complete=True, tags_digest="b" * 64) as server:
            adapter = OllamaHttpAdapter(make_config(endpoint=server.endpoint))
            with self.assertRaises(InferenceUnavailable):
                adapter.generate("prompt", StopControl(Deadline(5.0)))
            self.assertFalse(server.started.is_set(), "/api/generate ran despite a digest mismatch")
            self.assertFalse(adapter.describe()["digest_verified"])
