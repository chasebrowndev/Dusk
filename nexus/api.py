import base64
import json
import logging
import os
import queue
import sqlite3
import subprocess
import threading
import time
from typing import Optional

from nexus.network import Server, Client, Connection
from nexus.audio import AudioEngine, list_audio_devices

log = logging.getLogger(__name__)

# ── Log filters ───────────────────────────────────────────────────────────────

class _NoiseFilter(logging.Filter):
    """Suppress noisy drain-thread lines that would spam at INFO even when healthy."""
    _SKIP = ("drain heartbeat tick=", "drain: ok result=", "drain: evaluate_js")
    def filter(self, record):
        if record.levelno <= logging.DEBUG:
            return False
        msg = record.getMessage()
        return not any(s in msg for s in self._SKIP)

logging.getLogger(__name__).addFilter(_NoiseFilter())
logging.getLogger("nexus.audio").addFilter(_NoiseFilter())

# ── Config paths ──────────────────────────────────────────────────────────────

_CONFIG_DIR = os.path.expanduser("~/.config/dusk")
_HISTORY_DB = os.path.join(_CONFIG_DIR, "history.db")
_THEME_FILE  = os.path.join(_CONFIG_DIR, "theme_custom.json")

def _ensure_config_dir():
    os.makedirs(_CONFIG_DIR, exist_ok=True)

# ── History DB ────────────────────────────────────────────────────────────────

def _open_history():
    _ensure_config_dir()
    conn = sqlite3.connect(_HISTORY_DB, check_same_thread=False)
    conn.execute("""
        CREATE TABLE IF NOT EXISTS messages (
            id      INTEGER PRIMARY KEY AUTOINCREMENT,
            ts      REAL NOT NULL,
            sender  TEXT NOT NULL,
            text    TEXT NOT NULL,
            self    INTEGER NOT NULL DEFAULT 0,
            image   INTEGER NOT NULL DEFAULT 0
        )
    """)
    conn.commit()
    return conn

# ── Video capture ─────────────────────────────────────────────────────────────

class VideoCapture:
    def __init__(self, broadcast_fn, push_fn, fps=10):
        self._broadcast = broadcast_fn
        self._push      = push_fn
        self._fps       = fps
        self._thread    = None
        self._running   = False
        self._label     = ""

    def start_camera(self):
        try:
            import cv2
        except ImportError:
            return {"ok": False, "error": "opencv-python not installed"}
        self._label = "Camera"
        self._start_loop(self._camera_loop)
        return {"ok": True}

    def start_screen(self):
        try:
            import mss
        except ImportError:
            return {"ok": False, "error": "mss not installed — pip install mss"}
        self._label = "Screen"
        self._start_loop(self._screen_loop)
        return {"ok": True}

    def stop(self):
        self._running = False
        if self._thread and self._thread.is_alive():
            self._thread.join(timeout=2)
        self._thread = None

    @property
    def active(self):
        return self._running

    def _start_loop(self, target):
        self.stop()
        self._running = True
        self._thread  = threading.Thread(target=target, daemon=True, name="video-capture")
        self._thread.start()

    def _camera_loop(self):
        import cv2
        cap = None
        for backend, idx in [(cv2.CAP_ANY, 0), (cv2.CAP_ANY, 1),
                              (cv2.CAP_V4L2, 0), (cv2.CAP_V4L2, 1)]:
            c = cv2.VideoCapture(idx, backend)
            if c.isOpened():
                cap = c
                log.info("VideoCapture: opened camera idx=%d backend=%d", idx, backend)
                break
            c.release()
        if cap is None:
            log.error("VideoCapture: no camera device could be opened")
            self._push("error", {"message": "camera: no device found"})
            self._running = False
            return
        cap.set(cv2.CAP_PROP_FRAME_WIDTH,  640)
        cap.set(cv2.CAP_PROP_FRAME_HEIGHT, 360)
        interval = 1.0 / self._fps
        log.info("VideoCapture: camera loop started")
        try:
            while self._running:
                t0 = time.time()
                ok, frame = cap.read()
                if ok:
                    self._send_frame(frame)
                else:
                    time.sleep(0.5)
                time.sleep(max(0.0, interval - (time.time() - t0)))
        finally:
            cap.release()
            log.info("VideoCapture: camera loop ended")

    def _screen_loop(self):
        import subprocess, tempfile, os
        import cv2
        if subprocess.run(["which", "grim"], capture_output=True).returncode != 0:
            log.error("VideoCapture: grim not found")
            self._push("error", {"message": "screen share requires grim — sudo pacman -S grim"})
            self._running = False
            return
        interval = 1.0 / self._fps
        log.info("VideoCapture: screen loop started (grim/Wayland)")
        with tempfile.TemporaryDirectory() as tmpdir:
            tmpfile = os.path.join(tmpdir, "frame.png")
            while self._running:
                t0 = time.time()
                try:
                    r = subprocess.run(["grim", "-t", "png", "-l", "0", "-c", tmpfile],
                                       capture_output=True, timeout=2)
                    if r.returncode == 0:
                        frame = cv2.imread(tmpfile)
                        if frame is not None:
                            self._send_frame(frame)
                except Exception as e:
                    log.error("VideoCapture: screen error: %s", e)
                    time.sleep(0.5)
                time.sleep(max(0.0, interval - (time.time() - t0)))
        log.info("VideoCapture: screen loop ended")

    def _send_frame(self, bgr_frame):
        import cv2
        frame = cv2.resize(bgr_frame, (640, 360))
        ok, buf = cv2.imencode(".jpg", frame, [cv2.IMWRITE_JPEG_QUALITY, 55])
        if not ok:
            return
        b64 = base64.b64encode(buf.tobytes()).decode("ascii")
        self._broadcast({"type": "video_frame", "data": b64, "label": self._label})
        now = time.time()
        if not hasattr(self, "_last_active_push") or now - self._last_active_push >= 1.0:
            self._last_active_push = now
            self._push("video_sender_active", {"label": self._label})


def _peer_id_to_ip(peer_id: str) -> str:
    return peer_id.split(":")[0]


EVT_STATUS     = "status"
EVT_MESSAGE    = "message"
EVT_PEER_JOIN  = "peer_join"
EVT_PEER_LEAVE = "peer_leave"
EVT_CALL_START = "call_start"
EVT_CALL_END   = "call_end"
EVT_MIC_LEVEL  = "mic_level"
EVT_ERROR      = "error"


class NexusAPI:
    def __init__(self, is_host, remote_ip, port, display_name):
        self.is_host            = is_host
        self.remote_ip          = remote_ip
        self._initial_remote_ip = remote_ip
        self.port               = port
        self.display_name       = display_name or self._default_name()
        self._window            = None
        self._server            = None
        self._client            = None
        self._peers             = {}
        self._audio             = None
        self._video             = None
        self._in_call           = False
        self._muted             = False
        self._deafened          = False
        self._lock              = threading.Lock()
        self._push_queue        = queue.Queue()
        self._loaded_once       = False
        self._drain_running     = False
        self._history_db        = _open_history()
        self._history_lock      = threading.Lock()
        # File transfer state: transfer_id → {meta, chunks}
        self._incoming_files    = {}
        # Ping RTT tracking
        self._last_ping_ts      = {}   # peer_id → float (time.monotonic())
        self._rtt_ms            = {}   # peer_id → float

    # ── Window setup ─────────────────────────────────────────────────────────

    def set_window(self, window):
        self._window = window
        window.events.loaded += self._on_window_loaded

    def _on_window_loaded(self):
        if self._loaded_once:
            return
        self._loaded_once   = True
        self._drain_running = True
        self._drain_ticks   = 0
        self._drain_execs   = 0
        log.info("window loaded — starting network (is_host=%s)", self.is_host)

        def _drain_thread():
            while self._drain_running:
                self._drain_ticks += 1
                if self._drain_ticks % 40 == 0:
                    log.debug("drain heartbeat tick=%d execs=%d qsize=%d",
                              self._drain_ticks, self._drain_execs, self._push_queue.qsize())
                while not self._push_queue.empty():
                    try:
                        js = self._push_queue.get_nowait()
                    except Exception:
                        break
                    try:
                        self._window.evaluate_js(js)
                        self._drain_execs += 1
                    except Exception as e:
                        log.error("drain: evaluate_js EXCEPTION %s: %s", type(e).__name__, e)
                time.sleep(0.05)

        t = threading.Thread(target=_drain_thread, daemon=True, name="js-drain")
        t.start()
        log.info("JS drain thread started")

        if self.is_host:
            self.start_host(self.port)
        elif self._initial_remote_ip:
            self.connect_to(self._initial_remote_ip, self.port)
        else:
            self._push(EVT_STATUS, {"state": "idle", "message": "Ready"})

    # ── Identity ──────────────────────────────────────────────────────────────

    def get_identity(self):
        return {"name": self.display_name, "is_host": self.is_host, "port": self.port}

    def set_name(self, name):
        name = name.strip()
        if not name:
            return
        log.info("name changed: %s → %s", self.display_name, name)
        self.display_name = name
        self._broadcast({"type": "name_change", "name": self.display_name})

    # ── Network ───────────────────────────────────────────────────────────────

    def start_host(self, port=None):
        if self._server is not None:
            return {"ok": True, "port": self.port}
        port = port or self.port
        try:
            self._server = Server(port, self._on_peer_connect)
            self._server.start()
            self.is_host = True
            self.port    = port
            log.info("hosting on port %d", port)
            self._push(EVT_STATUS, {"state": "hosting", "message": f"Hosting on port {port}"})
            return {"ok": True, "port": port}
        except Exception as e:
            log.exception("start_host failed")
            return {"ok": False, "error": str(e)}

    def connect_to(self, ip, port=None):
        port = port or self.port
        log.info("connecting to %s:%d", ip, port)
        self.remote_ip = ip
        self._client = Client(
            host=ip, port=port,
            on_connect=self._on_peer_connect,
            on_status=lambda s: self._push(EVT_STATUS, {"state": s, "message": s}),
        )
        self._client.start()
        return {"ok": True}

    def disconnect(self):
        log.info("disconnect called")
        self._drain_running = False
        if self._client:
            self._client.stop()
            self._client = None
        if self._server:
            self._server.stop()
            self._server = None
        with self._lock:
            for conn in self._peers.values():
                conn.close()
            self._peers.clear()
        self.end_call()
        self._push(EVT_STATUS, {"state": "idle", "message": "Disconnected"})

    # ── Messaging ─────────────────────────────────────────────────────────────

    def send_message(self, text):
        text = text.strip()
        if not text:
            return {"ok": False, "error": "empty"}
        with self._lock:
            peers = dict(self._peers)
        if not peers:
            log.warning("send_message: no peers connected")
            return {"ok": False, "error": "no peers connected"}
        ts  = time.time()
        msg = {"type": "chat", "from": self.display_name, "text": text, "ts": ts}
        log.info("sending message to peer: %r", text[:80])
        for conn in peers.values():
            conn.send(msg)
        self._save_message(ts, self.display_name, text, self_sent=True)
        self._push(EVT_MESSAGE, {"from": self.display_name, "text": text, "ts": ts, "self": True})
        return {"ok": True}

    def send_image(self, b64_data, mime="image/png"):
        """Send a pasted/captured image as a chat message."""
        with self._lock:
            peers = dict(self._peers)
        if not peers:
            return {"ok": False, "error": "no peers connected"}
        ts  = time.time()
        msg = {"type": "chat_image", "from": self.display_name, "data": b64_data,
               "mime": mime, "ts": ts}
        for conn in peers.values():
            conn.send(msg)
        self._save_message(ts, self.display_name, f"[image]", self_sent=True, image=True)
        self._push("message_image", {"from": self.display_name, "data": b64_data,
                                     "mime": mime, "ts": ts, "self": True})
        return {"ok": True}

    # ── Message history ───────────────────────────────────────────────────────

    def _save_message(self, ts, sender, text, self_sent=False, image=False):
        with self._history_lock:
            try:
                self._history_db.execute(
                    "INSERT INTO messages (ts, sender, text, self, image) VALUES (?,?,?,?,?)",
                    (ts, sender, text, int(self_sent), int(image))
                )
                self._history_db.commit()
            except Exception as e:
                log.warning("history save failed: %s", e)

    def get_history(self, limit=100):
        """Called from JS on load to restore recent messages."""
        with self._history_lock:
            try:
                rows = self._history_db.execute(
                    "SELECT ts, sender, text, self, image FROM messages "
                    "ORDER BY ts DESC LIMIT ?", (limit,)
                ).fetchall()
                return [{"ts": r[0], "from": r[1], "text": r[2],
                         "self": bool(r[3]), "image": bool(r[4])}
                        for r in reversed(rows)]
            except Exception as e:
                log.warning("history fetch failed: %s", e)
                return []

    def clear_history(self):
        with self._history_lock:
            try:
                self._history_db.execute("DELETE FROM messages")
                self._history_db.commit()
                return {"ok": True}
            except Exception as e:
                return {"ok": False, "error": str(e)}

    def _send_history_to(self, peer_id, limit=100):
        """Host sends recent message history to a newly connected peer."""
        with self._lock:
            conn = self._peers.get(peer_id)
        if not conn:
            return
        history = self.get_history(limit)
        conn.send({"type": "history_sync", "messages": history})
        log.info("sent %d history messages to %s", len(history), peer_id)

    # ── Typing indicator ──────────────────────────────────────────────────────

    def send_typing(self, typing: bool):
        self._broadcast({"type": "typing" if typing else "typing_stop",
                         "from": self.display_name})
        return {"ok": True}

    # ── File transfer ─────────────────────────────────────────────────────────

    def send_file(self, filename: str, b64_data: str, mime: str = ""):
        """JS reads a file as base64 and hands it here to chunk and send."""
        with self._lock:
            peers = dict(self._peers)
        if not peers:
            return {"ok": False, "error": "no peers connected"}
        transfer_id = f"ft-{int(time.time()*1000)}"
        chunk_size  = 32 * 1024   # 32 KB chunks in base64
        chunks      = [b64_data[i:i+chunk_size] for i in range(0, len(b64_data), chunk_size)]
        total       = len(chunks)
        log.info("file transfer %s: %s (%d chunks)", transfer_id, filename, total)
        # Send header
        for conn in peers.values():
            conn.send({"type": "file_header", "id": transfer_id,
                       "name": filename, "mime": mime, "chunks": total,
                       "size": len(b64_data)})
        # Send chunks with small sleep to avoid flooding
        for i, chunk in enumerate(chunks):
            for conn in peers.values():
                conn.send({"type": "file_chunk", "id": transfer_id,
                           "index": i, "data": chunk})
            self._push("file_send_progress",
                       {"id": transfer_id, "name": filename,
                        "progress": round((i + 1) / total * 100)})
            time.sleep(0.002)
        for conn in peers.values():
            conn.send({"type": "file_done", "id": transfer_id})
        self._push("file_send_done", {"id": transfer_id, "name": filename})
        return {"ok": True}

    def save_file(self, transfer_id: str, save_path: str):
        """Save a completed incoming file to disk."""
        if transfer_id not in self._incoming_files:
            return {"ok": False, "error": "transfer not found"}
        ft = self._incoming_files[transfer_id]
        chunks = ft.get("chunks_data", {})
        total  = ft.get("total", 0)
        if len(chunks) < total:
            return {"ok": False, "error": "transfer incomplete"}
        try:
            data = base64.b64decode("".join(chunks[i] for i in range(total)))
            os.makedirs(os.path.dirname(save_path) if os.path.dirname(save_path) else ".", exist_ok=True)
            with open(save_path, "wb") as f:
                f.write(data)
            del self._incoming_files[transfer_id]
            log.info("saved file %s → %s", transfer_id, save_path)
            return {"ok": True}
        except Exception as e:
            log.error("save_file failed: %s", e)
            return {"ok": False, "error": str(e)}

    def get_download_path(self, filename: str) -> str:
        """Return a safe default download path."""
        dl = os.path.expanduser("~/Downloads")
        os.makedirs(dl, exist_ok=True)
        return os.path.join(dl, filename)

    # ── Notifications ─────────────────────────────────────────────────────────

    def notify(self, title: str, body: str):
        """Send a desktop notification via notify-send."""
        try:
            subprocess.Popen(
                ["notify-send", "--app-name=Dusk",
                 "--icon=dialog-information",
                 "--urgency=normal",
                 title, body],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
            )
        except FileNotFoundError:
            log.warning("notify-send not found — install libnotify")
        return {"ok": True}

    # ── Connection quality (RTT) ──────────────────────────────────────────────

    def _start_ping_loop(self, peer_id: str, conn):
        """Send application-level pings to measure RTT, separate from heartbeat."""
        def _loop():
            while peer_id in self._peers:
                ts = time.monotonic()
                ok = conn.send({"type": "rtt_ping", "ts": ts})
                if not ok:
                    break
                time.sleep(5)
        threading.Thread(target=_loop, daemon=True, name=f"rtt-{peer_id}").start()

    # ── Voice call ────────────────────────────────────────────────────────────

    def start_call(self):
        if self._in_call:
            return {"ok": False, "error": "already in call"}
        with self._lock:
            peers = dict(self._peers)
        if not peers:
            return {"ok": False, "error": "no peers"}
        peer_id   = next(iter(peers))
        remote_ip = _peer_id_to_ip(peer_id)
        udp_port  = self.port + 1
        log.info("starting call → %s udp/%d", remote_ip, udp_port)
        self._audio = AudioEngine(
            remote_ip=remote_ip, udp_port=udp_port,
            on_level=lambda v: self._push(EVT_MIC_LEVEL, {"level": round(v, 3)}),
        )
        self._broadcast({"type": "call_request", "udp_port": udp_port, "from": self.display_name})
        if not self._audio.available:
            return {"ok": False, "error": "pyaudio/opuslib not available"}
        ok = self._audio.start()
        if ok:
            self._in_call = True
            self._push(EVT_CALL_START, {"peer": remote_ip})
            return {"ok": True}
        return {"ok": False, "error": "audio engine failed"}

    def end_call(self):
        if self._audio:
            self._audio.stop()
            self._audio = None
        if self._video and self._video.active:
            self._video.stop()
            self._video = None
            self._broadcast({"type": "video_stop"})
        if self._in_call:
            log.info("call ended")
        self._in_call = False
        self._broadcast({"type": "call_end"})
        self._push(EVT_CALL_END, {})
        return {"ok": True}

    def set_muted(self, muted):
        self._muted = muted
        if self._audio:
            self._audio.muted = muted

    def set_deafened(self, deafened):
        self._deafened = deafened
        if self._audio:
            self._audio.deafened = deafened

    def get_audio_devices(self):
        return list_audio_devices()

    # ── Custom theme ──────────────────────────────────────────────────────────

    def load_custom_theme(self):
        """Load saved custom theme from ~/.config/dusk/theme_custom.json"""
        try:
            if os.path.exists(_THEME_FILE):
                with open(_THEME_FILE) as f:
                    return {"ok": True, "theme": json.load(f)}
        except Exception as e:
            log.warning("load_custom_theme failed: %s", e)
        return {"ok": False, "theme": {}}

    def save_custom_theme(self, theme: dict):
        """Save custom theme variables to disk."""
        _ensure_config_dir()
        try:
            with open(_THEME_FILE, "w") as f:
                json.dump(theme, f, indent=2)
            return {"ok": True}
        except Exception as e:
            log.warning("save_custom_theme failed: %s", e)
            return {"ok": False, "error": str(e)}

    # ── Peer events ───────────────────────────────────────────────────────────

    def _on_peer_connect(self, conn):
        peer_id = conn.peer_addr
        log.info("peer connected: %s", peer_id)
        with self._lock:
            self._peers[peer_id] = conn
        conn.send({"type": "hello", "name": self.display_name, "version": "1.0"})
        self._push(EVT_PEER_JOIN, {"peer_id": peer_id, "name": peer_id})
        conn.start_reader(
            on_message=lambda m: self._on_message(peer_id, m),
            on_disconnect=lambda: self._on_peer_disconnect(peer_id),
        )
        self._start_ping_loop(peer_id, conn)
        if not self.is_host:
            threading.Timer(0.5, self._auto_start_call).start()

    def _auto_start_call(self):
        if self._in_call:
            return
        log.info("auto-starting voice call")
        self.start_call()

    def _on_peer_disconnect(self, peer_id):
        log.info("peer disconnected: %s", peer_id)
        with self._lock:
            self._peers.pop(peer_id, None)
        self._rtt_ms.pop(peer_id, None)
        self._push(EVT_PEER_LEAVE, {"peer_id": peer_id})
        if self._video and self._video.active:
            self._video.stop()
            self._video = None
        if self._in_call and not self._peers:
            self.end_call()

    def _on_message(self, peer_id, msg):
        mtype = msg.get("type")
        if mtype == "hello":
            name = msg.get("name", peer_id)
            log.info("peer %s identified as %r", peer_id, name)
            self._push(EVT_PEER_JOIN, {"peer_id": peer_id, "name": name})
            # Host sends recent history to client so they're in sync
            if self.is_host:
                self._send_history_to(peer_id)

        elif mtype == "history_sync":
            # Client receives history from host — push to UI
            messages = msg.get("messages", [])
            log.info("received history sync: %d messages", len(messages))
            self._push("history_sync", {"messages": messages})

        elif mtype == "chat":
            text = msg.get("text", "")
            ts   = msg.get("ts", time.time())
            sender = msg.get("from", peer_id)
            log.info("chat from %s: %r", peer_id, text[:80])
            self._save_message(ts, sender, text, self_sent=False)
            self._push(EVT_MESSAGE, {k: v for k, v in msg.items() if k != "type"} | {"self": False})
            # Desktop notification if message from peer
            self.notify(sender, text[:80])

        elif mtype == "chat_image":
            sender = msg.get("from", peer_id)
            ts     = msg.get("ts", time.time())
            self._save_message(ts, sender, "[image]", self_sent=False, image=True)
            self._push("message_image", {k: v for k, v in msg.items() if k != "type"} | {"self": False})
            self.notify(sender, "sent an image")

        elif mtype == "typing":
            self._push("typing", {"from": msg.get("from", peer_id)})

        elif mtype == "typing_stop":
            self._push("typing_stop", {"from": msg.get("from", peer_id)})

        elif mtype == "file_header":
            fid = msg.get("id")
            self._incoming_files[fid] = {
                "name":        msg.get("name", "file"),
                "mime":        msg.get("mime", ""),
                "total":       msg.get("chunks", 0),
                "size":        msg.get("size", 0),
                "chunks_data": {},
            }
            self._push("file_incoming", {"id": fid, "name": msg.get("name"),
                                          "size": msg.get("size", 0)})
            self.notify("Dusk", f"Incoming file: {msg.get('name', 'file')}")

        elif mtype == "file_chunk":
            fid = msg.get("id")
            if fid in self._incoming_files:
                ft = self._incoming_files[fid]
                ft["chunks_data"][msg.get("index")] = msg.get("data", "")
                received = len(ft["chunks_data"])
                total    = ft["total"]
                self._push("file_progress", {"id": fid, "name": ft["name"],
                                              "progress": round(received / max(total, 1) * 100)})

        elif mtype == "file_done":
            fid = msg.get("id")
            if fid in self._incoming_files:
                ft = self._incoming_files[fid]
                self._push("file_ready", {"id": fid, "name": ft["name"],
                                           "size": ft.get("size", 0)})

        elif mtype == "rtt_ping":
            # Reflect back immediately
            with self._lock:
                conn = self._peers.get(peer_id)
            if conn:
                conn.send({"type": "rtt_pong", "ts": msg.get("ts")})

        elif mtype == "rtt_pong":
            sent_ts = msg.get("ts")
            if sent_ts:
                rtt = (time.monotonic() - sent_ts) * 1000
                self._rtt_ms[peer_id] = rtt
                self._push("ping_rtt", {"rtt_ms": round(rtt, 1)})

        elif mtype == "call_request":
            udp_port  = msg.get("udp_port", self.port + 1)
            remote_ip = _peer_id_to_ip(peer_id)
            log.info("incoming call from %s udp/%d", remote_ip, udp_port)
            self._audio = AudioEngine(
                remote_ip=remote_ip, udp_port=udp_port,
                on_level=lambda v: self._push(EVT_MIC_LEVEL, {"level": round(v, 3)}),
            )
            if self._audio.available:
                self._audio.start()
                self._in_call = True
                self._push(EVT_CALL_START, {"peer": remote_ip, "from": msg.get("from", peer_id)})

        elif mtype == "call_end":
            if self._in_call:
                self.end_call()

        elif mtype == "name_change":
            name = msg.get("name", peer_id)
            log.info("peer %s changed name to %r", peer_id, name)
            self._push(EVT_PEER_JOIN, {"peer_id": peer_id, "name": name})

        elif mtype == "video_frame":
            self._push("video_frame", {"data": msg.get("data", ""), "label": msg.get("label", "")})

        elif mtype == "video_stop":
            self._push("video_stop", {})

        elif mtype == "admin_cmd":
            cmd = msg.get("cmd")
            log.info("admin_cmd from %s: %s", peer_id, cmd)
            if cmd == "mute":
                self._muted = True
                if self._audio:
                    self._audio.muted = True
                self._push("admin_cmd", {"cmd": "mute"})
            elif cmd == "unmute":
                self._muted = False
                if self._audio:
                    self._audio.muted = False
                self._push("admin_cmd", {"cmd": "unmute"})
            elif cmd == "camera_on":
                self._push("admin_cmd", {"cmd": "camera_on"})

    # ── Admin ─────────────────────────────────────────────────────────────────

    def start_camera_stream(self):
        with self._lock:
            peers = dict(self._peers)
        if not peers:
            return {"ok": False, "error": "no peers connected"}
        if self._video and self._video.active:
            self._video.stop()
        self._video = VideoCapture(broadcast_fn=self._broadcast, push_fn=self._push)
        result = self._video.start_camera()
        if result["ok"]:
            log.info("Python camera stream started")
        return result

    def start_screen_stream(self):
        with self._lock:
            peers = dict(self._peers)
        if not peers:
            return {"ok": False, "error": "no peers connected"}
        if self._video and self._video.active:
            self._video.stop()
        self._video = VideoCapture(broadcast_fn=self._broadcast, push_fn=self._push)
        result = self._video.start_screen()
        if result["ok"]:
            log.info("Python screen stream started")
        return result

    def stop_video_stream(self):
        if not self._video:
            return {"ok": True}
        self._video.stop()
        self._video = None
        self._broadcast({"type": "video_stop"})
        self._push("video_sender_stopped", {})
        log.info("video stream stopped")
        return {"ok": True}

    def send_video_frame(self, b64_jpeg, label=""):
        with self._lock:
            peers = dict(self._peers)
        if not peers:
            return {"ok": False, "error": "no peers"}
        msg = {"type": "video_frame", "data": b64_jpeg, "label": label}
        for conn in peers.values():
            conn.send(msg)
        return {"ok": True}

    def admin_mute_peer(self, peer_id):
        if not self.is_host:
            return {"ok": False, "error": "not host"}
        with self._lock:
            conn = self._peers.get(peer_id)
        if not conn:
            return {"ok": False, "error": "peer not found"}
        conn.send({"type": "admin_cmd", "cmd": "mute"})
        return {"ok": True}

    def admin_unmute_peer(self, peer_id):
        if not self.is_host:
            return {"ok": False, "error": "not host"}
        with self._lock:
            conn = self._peers.get(peer_id)
        if not conn:
            return {"ok": False, "error": "peer not found"}
        conn.send({"type": "admin_cmd", "cmd": "unmute"})
        return {"ok": True}

    def admin_request_camera(self, peer_id):
        if not self.is_host:
            return {"ok": False, "error": "not host"}
        with self._lock:
            conn = self._peers.get(peer_id)
        if not conn:
            return {"ok": False, "error": "peer not found"}
        conn.send({"type": "admin_cmd", "cmd": "camera_on"})
        return {"ok": True}

    def get_peers(self):
        with self._lock:
            return [{"peer_id": pid} for pid in self._peers]

    # ── Internal ──────────────────────────────────────────────────────────────

    def _broadcast(self, msg):
        with self._lock:
            peers = dict(self._peers)
        for conn in peers.values():
            conn.send(msg)

    def _push(self, event_type, data):
        if not self._window:
            return
        payload = json.dumps({"type": event_type, **data})
        js      = f"window.nexus && window.nexus.onEvent({payload})"
        self._push_queue.put(js)
        # High-frequency events → DEBUG so they don't spam the terminal
        _noisy = {"ping_rtt", "mic_level", "video_frame", "video_sender_active",
                  "typing", "typing_stop"}
        if event_type in _noisy:
            log.debug("_push: queued %s (qsize=%d)", event_type, self._push_queue.qsize())
        else:
            log.info("_push: queued %s (qsize=%d)", event_type, self._push_queue.qsize())

    @staticmethod
    def _default_name():
        return os.environ.get("USER", "user")
