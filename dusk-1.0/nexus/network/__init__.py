"""
nexus.network.core
──────────────────
Reliable TCP messaging layer over Tailscale.

Wire format (length-prefixed JSON frames):
  [4 bytes big-endian uint32: payload length][payload bytes (UTF-8 JSON)]

Binary frames (for audio PCM):
  type field == "audio" → payload is base64-encoded PCM in the JSON value,
  OR we use a parallel raw UDP socket for audio to avoid head-of-line blocking.

Heartbeat: ping/pong every HEARTBEAT_INTERVAL seconds.
If HEARTBEAT_TIMEOUT seconds pass with no pong, the connection is declared dead
and autoreconnect fires.
"""

import socket
import struct
import json
import threading
import time
import logging
import base64
from typing import Callable, Optional

log = logging.getLogger(__name__)

HEARTBEAT_INTERVAL = 5   # seconds between pings
HEARTBEAT_TIMEOUT  = 15  # seconds before we consider conn dead
RECONNECT_BASE     = 2   # seconds, doubles each attempt
RECONNECT_MAX      = 30  # cap
RECV_BUFSIZE       = 65536

# ── Frame helpers ────────────────────────────────────────────────────────────

def _encode_frame(msg: dict) -> bytes:
    payload = json.dumps(msg, ensure_ascii=False).encode("utf-8")
    header  = struct.pack(">I", len(payload))
    return header + payload


def _recv_exactly(sock: socket.socket, n: int) -> Optional[bytes]:
    """Block until exactly n bytes received, or return None on EOF/error."""
    buf = b""
    while len(buf) < n:
        try:
            chunk = sock.recv(n - len(buf))
        except (OSError, ConnectionResetError):
            return None
        if not chunk:
            return None
        buf += chunk
    return buf


# ── Connection wrapper ───────────────────────────────────────────────────────

class Connection:
    """
    Wraps a single TCP socket. Provides:
      - send(msg_dict)         — thread-safe framed JSON send
      - start_reader(cb)       — background thread calling cb(msg_dict) per frame
      - heartbeat loop
      - close()
    """

    def __init__(self, sock: socket.socket, peer_addr: str):
        self.sock      = sock
        self.peer_addr = peer_addr
        self._send_lock = threading.Lock()
        self._alive     = True
        self._last_pong = time.monotonic()
        self._on_disconnect: Optional[Callable] = None

        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE, 1)

    # ── Public ────────────────────────────────────────────────────────────────

    def send(self, msg: dict) -> bool:
        if not self._alive:
            return False
        frame = _encode_frame(msg)
        with self._send_lock:
            try:
                self.sock.sendall(frame)
                return True
            except OSError as e:
                log.warning("send error to %s: %s", self.peer_addr, e)
                self._declare_dead()
                return False

    def start_reader(self, on_message: Callable[[dict], None],
                     on_disconnect: Optional[Callable] = None):
        self._on_disconnect = on_disconnect
        t = threading.Thread(target=self._reader_loop,
                             args=(on_message,), daemon=True)
        t.start()
        hb = threading.Thread(target=self._heartbeat_loop, daemon=True)
        hb.start()

    def close(self):
        self._alive = False
        try:
            self.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        try:
            self.sock.close()
        except OSError:
            pass

    # ── Internal ──────────────────────────────────────────────────────────────

    def _reader_loop(self, on_message: Callable[[dict], None]):
        log.info("reader started for %s", self.peer_addr)
        while self._alive:
            header = _recv_exactly(self.sock, 4)
            if header is None:
                log.info("connection to %s closed (EOF)", self.peer_addr)
                self._declare_dead()
                break
            (length,) = struct.unpack(">I", header)
            if length == 0 or length > 10 * 1024 * 1024:
                log.warning("bogus frame length %d from %s", length, self.peer_addr)
                self._declare_dead()
                break
            payload = _recv_exactly(self.sock, length)
            if payload is None:
                self._declare_dead()
                break
            try:
                msg = json.loads(payload.decode("utf-8"))
            except json.JSONDecodeError as e:
                log.warning("bad JSON from %s: %s", self.peer_addr, e)
                continue
            self._dispatch(msg, on_message)
        log.info("reader exiting for %s", self.peer_addr)

    def _dispatch(self, msg: dict, on_message: Callable):
        mtype = msg.get("type")
        if mtype == "ping":
            self.send({"type": "pong"})
        elif mtype == "pong":
            self._last_pong = time.monotonic()
        else:
            try:
                on_message(msg)
            except Exception as e:
                log.exception("on_message error: %s", e)

    def _heartbeat_loop(self):
        while self._alive:
            time.sleep(HEARTBEAT_INTERVAL)
            if not self._alive:
                break
            self.send({"type": "ping"})
            age = time.monotonic() - self._last_pong
            if age > HEARTBEAT_TIMEOUT:
                log.warning("heartbeat timeout for %s (%.0fs)", self.peer_addr, age)
                self._declare_dead()
                break

    def _declare_dead(self):
        if not self._alive:
            return
        self._alive = False
        try:
            self.sock.close()
        except OSError:
            pass
        if self._on_disconnect:
            threading.Thread(target=self._on_disconnect, daemon=True).start()


# ── Host (server) ────────────────────────────────────────────────────────────

class Server:
    """
    Listens for incoming connections.  Calls on_connect(Connection) for each.
    Multiple clients supported (group chat / conferencing backbone).
    """

    def __init__(self, port: int, on_connect: Callable[[Connection], None]):
        self.port       = port
        self.on_connect = on_connect
        self._sock: Optional[socket.socket] = None
        self._running   = False

    def start(self):
        self._sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._sock.bind(("0.0.0.0", self.port))
        self._sock.listen(8)
        self._running = True
        t = threading.Thread(target=self._accept_loop, daemon=True)
        t.start()
        log.info("server listening on port %d", self.port)

    def stop(self):
        self._running = False
        if self._sock:
            try:
                self._sock.close()
            except OSError:
                pass

    def _accept_loop(self):
        while self._running:
            try:
                client_sock, addr = self._sock.accept()
            except OSError:
                break
            peer = addr[0]   # plain IPv4 string, no port suffix
            log.info("accepted connection from %s", peer)
            conn = Connection(client_sock, peer)
            threading.Thread(target=self.on_connect, args=(conn,), daemon=True).start()


# ── Client (with autoreconnect) ──────────────────────────────────────────────

class Client:
    """
    Connects to a remote host and autoreconnects with exponential backoff.
    Calls on_connect(Connection) each time a connection is established.
    Calls on_status(status_str) for UI feedback.
    """

    def __init__(self, host: str, port: int,
                 on_connect: Callable[[Connection], None],
                 on_status: Optional[Callable[[str], None]] = None):
        self.host       = host
        self.port       = port
        self.on_connect = on_connect
        self.on_status  = on_status or (lambda s: None)
        self._running   = False
        self._current: Optional[Connection] = None

    def start(self):
        self._running = True
        t = threading.Thread(target=self._connect_loop, daemon=True)
        t.start()

    def stop(self):
        self._running = False
        if self._current:
            self._current.close()

    def _connect_loop(self):
        delay = RECONNECT_BASE
        attempt = 0
        while self._running:
            attempt += 1
            self.on_status(f"connecting" if attempt == 1 else f"reconnecting (attempt {attempt})")
            log.info("connecting to %s:%d (attempt %d)", self.host, self.port, attempt)
            try:
                sock = socket.create_connection((self.host, self.port), timeout=10)
            except (OSError, socket.timeout) as e:
                log.warning("connect failed: %s — retrying in %ds", e, delay)
                self.on_status(f"unreachable — retrying in {delay}s")
                time.sleep(delay)
                delay = min(delay * 2, RECONNECT_MAX)
                continue

            delay = RECONNECT_BASE  # reset on success
            self.on_status("connected")
            log.info("connected to %s:%d", self.host, self.port)
            conn = Connection(sock, f"{self.host}:{self.port}")
            self._current = conn
            dead_evt = threading.Event()
            # on_connect calls conn.start_reader() which sets conn._on_disconnect.
            # We must NOT overwrite it afterwards; instead chain into it by
            # passing dead_evt.set as the on_disconnect arg via a wrapper.
            _original_start_reader = conn.start_reader
            def _start_reader_and_signal(on_message, on_disconnect=None):
                def _chained_disconnect():
                    if on_disconnect:
                        on_disconnect()
                    dead_evt.set()
                _original_start_reader(on_message, _chained_disconnect)
            conn.start_reader = _start_reader_and_signal
            self.on_connect(conn)
            dead_evt.wait()
            self._current = None
            if self._running:
                self.on_status(f"disconnected — reconnecting in {delay}s")
                log.info("connection lost, reconnecting in %ds", delay)
                time.sleep(delay)
                delay = min(delay * 2, RECONNECT_MAX)
