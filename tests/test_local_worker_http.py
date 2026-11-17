import json
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from local_worker.adapters import InferenceCancelled, OllamaHttpAdapter
from local_worker.control import Deadline, StopControl

from local_worker_support import make_config


class _OllamaHandler(BaseHTTPRequestHandler):
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
    def __init__(self, *, complete: bool = False):
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
