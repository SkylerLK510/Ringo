"""Wall-clock deadlines and socket-level cancellation.

The runtime uses monotonic time for a real job deadline. A blocking HTTP read
cannot be interrupted by a flag, so the owned socket is shut down from a
watcher thread when the deadline or an explicit stop fires. Only sockets owned
by this request are touched; the shared Ollama daemon is never killed.
"""

from __future__ import annotations

import socket
import threading
import time


class Deadline:
    def __init__(self, duration_s: float):
        self.duration_s = float(duration_s)
        self._end = time.monotonic() + self.duration_s

    def remaining(self) -> float:
        return max(0.0, self._end - time.monotonic())

    def expired(self) -> bool:
        return time.monotonic() >= self._end


class StopControl:
    """Combines the job deadline with an explicit cancellation request."""

    def __init__(self, deadline: Deadline, parent: "StopControl | None" = None):
        self.deadline = deadline
        self.parent = parent
        self._event = threading.Event()

    def cancel(self) -> None:
        self._event.set()

    @property
    def stopped(self) -> bool:
        return (
            self._event.is_set()
            or self.deadline.expired()
            or (self.parent is not None and self.parent.stopped)
        )

    @property
    def explicitly_cancelled(self) -> bool:
        return self._event.is_set() or (
            self.parent is not None and self.parent.explicitly_cancelled
        )

    def poll_interval(self) -> float:
        return min(0.1, max(0.005, self.deadline.remaining()))


class SocketGuard:
    """Shut down one owned socket when a stop is requested."""

    def __init__(self, sock: socket.socket, stop: StopControl):
        self._sock = sock
        self._stop = stop
        self._done = threading.Event()
        self._triggered = threading.Event()
        self._thread = threading.Thread(target=self._watch, daemon=True, name="local-worker-cancel")

    def start(self) -> None:
        self._thread.start()

    def _watch(self) -> None:
        while not self._done.wait(self._stop.poll_interval()):
            if not self._stop.stopped:
                continue
            try:
                self._sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            try:
                self._sock.close()
            except OSError:
                pass
            self._triggered.set()
            return

    @property
    def triggered(self) -> bool:
        return self._triggered.is_set()

    def finish(self) -> None:
        self._done.set()
        self._thread.join(timeout=1.0)
