import threading
import socket
import time
import queue
import logging
from typing import Optional, Callable

log = logging.getLogger(__name__)

SAMPLE_RATE    = 48000
CHANNELS       = 1
FRAME_DURATION = 20
FRAME_SAMPLES  = SAMPLE_RATE * FRAME_DURATION // 1000  # 960
SAMPLE_WIDTH   = 2
FRAME_BYTES    = FRAME_SAMPLES * CHANNELS * SAMPLE_WIDTH
UDP_MTU        = 4096

# Drop frames if playback queue exceeds this (~100ms at 20ms/frame)
MAX_QUEUE_FRAMES = 5

# Throttle mic level pushes to UI (ms)
MIC_LEVEL_INTERVAL_MS = 80

try:
    import opuslib
    _OPUS_OK = True
except ImportError:
    log.warning("opuslib not available")
    _OPUS_OK = False

try:
    import pyaudio
    _PA_OK = True
except ImportError:
    log.warning("pyaudio not available")
    _PA_OK = False


class AudioEngine:
    def __init__(self, remote_ip: str, udp_port: int,
                 on_level: Optional[Callable[[float], None]] = None):
        self.remote_ip  = remote_ip
        self.udp_port   = udp_port
        self.on_level   = on_level
        self.muted      = False
        self.deafened   = False
        self._running   = False
        self._pa        = None
        self._enc       = None
        self._dec       = None
        self._recv_sock = None   # bound to udp_port, recv only
        self._send_sock = None   # ephemeral, send only
        self._play_q    = queue.Queue()
        self._available = _OPUS_OK and _PA_OK

    @property
    def available(self) -> bool:
        return self._available

    def start(self) -> bool:
        if not self._available:
            log.error("audio not available (missing pyaudio or opuslib)")
            return False
        if self._running:
            return True
        try:
            self._enc = opuslib.Encoder(SAMPLE_RATE, CHANNELS, opuslib.APPLICATION_VOIP)
            self._dec = opuslib.Decoder(SAMPLE_RATE, CHANNELS)
            # Suppress the wall of ALSA error messages that PortAudio emits to
            # stderr during device enumeration — redirect fd 2 to /dev/null for
            # the duration of PyAudio() init, then restore it.
            import os as _os
            _devnull = _os.open(_os.devnull, _os.O_WRONLY)
            _saved   = _os.dup(2)
            _os.dup2(_devnull, 2)
            try:
                self._pa = pyaudio.PyAudio()
            finally:
                _os.dup2(_saved, 2)
                _os.close(_saved)
                _os.close(_devnull)

            self._recv_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            self._recv_sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            self._recv_sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
            self._recv_sock.bind(("", self.udp_port))
            self._recv_sock.settimeout(0.5)

            self._send_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)

            self._running = True
            threading.Thread(target=self._capture_loop,  daemon=True, name="audio-cap").start()
            threading.Thread(target=self._receive_loop,  daemon=True, name="audio-recv").start()
            threading.Thread(target=self._playback_loop, daemon=True, name="audio-play").start()
            log.info("audio engine started -> %s:%d", self.remote_ip, self.udp_port)
            return True
        except Exception as e:
            log.exception("audio start failed: %s", e)
            self._running = False
            return False

    def stop(self):
        self._running = False
        for s in (self._recv_sock, self._send_sock):
            if s:
                try: s.close()
                except OSError: pass
        self._recv_sock = None
        self._send_sock = None
        self._play_q.put(None)  # unblock playback thread
        time.sleep(0.15)
        if self._pa:
            try: self._pa.terminate()
            except Exception: pass
        self._pa = None
        log.info("audio engine stopped")

    def _capture_loop(self):
        try:
            stream = self._pa.open(
                format=pyaudio.paInt16, channels=CHANNELS,
                rate=SAMPLE_RATE, input=True,
                frames_per_buffer=FRAME_SAMPLES,
            )
        except Exception as e:
            log.error("mic open failed: %s", e)
            return
        log.info("capture loop started")
        _last_level_t = 0.0
        while self._running:
            try:
                pcm = stream.read(FRAME_SAMPLES, exception_on_overflow=False)
            except Exception as e:
                log.warning("mic read error: %s", e)
                time.sleep(0.02)
                continue

            # Throttled level meter — don't push every 20ms frame to JS
            if self.on_level:
                now = time.monotonic()
                if (now - _last_level_t) * 1000 >= MIC_LEVEL_INTERVAL_MS:
                    _last_level_t = now
                    try:
                        import audioop
                        rms = audioop.rms(pcm, SAMPLE_WIDTH)
                        self.on_level(min(1.0, rms / 8000.0))
                    except Exception:
                        pass

            if self.muted:
                continue
            try:
                encoded = self._enc.encode(pcm, FRAME_SAMPLES)
                self._send_sock.sendto(encoded, (self.remote_ip, self.udp_port))
            except Exception as e:
                log.debug("encode/send error: %s", e)
        try:
            stream.stop_stream()
            stream.close()
        except Exception:
            pass
        log.info("capture loop exited")

    def _receive_loop(self):
        log.info("receive loop started")
        while self._running:
            try:
                data, _ = self._recv_sock.recvfrom(UDP_MTU)
            except socket.timeout:
                continue
            except OSError:
                break
            if self.deafened:
                continue
            try:
                pcm = self._dec.decode(data, FRAME_SAMPLES)
            except Exception as e:
                log.debug("decode error: %s", e)
                continue

            # Drop stale frames to prevent latency buildup
            while self._play_q.qsize() >= MAX_QUEUE_FRAMES:
                try:
                    self._play_q.get_nowait()
                    log.debug("dropped stale audio frame (queue=%d)", self._play_q.qsize())
                except queue.Empty:
                    break
            self._play_q.put(pcm)
        log.info("receive loop exited")

    def _playback_loop(self):
        try:
            stream = self._pa.open(
                format=pyaudio.paInt16, channels=CHANNELS,
                rate=SAMPLE_RATE, output=True,
                frames_per_buffer=FRAME_SAMPLES,
            )
        except Exception as e:
            log.error("speaker open failed: %s", e)
            return
        log.info("playback loop started")
        while self._running:
            try:
                pcm = self._play_q.get(timeout=0.5)
            except queue.Empty:
                continue
            if pcm is None:  # stop sentinel
                break
            try:
                stream.write(pcm)
            except Exception as e:
                log.debug("playback write error: %s", e)
        try:
            stream.stop_stream()
            stream.close()
        except Exception:
            pass
        log.info("playback loop exited")


def list_audio_devices() -> dict:
    if not _PA_OK:
        return {"inputs": [], "outputs": [], "error": "pyaudio not available"}
    try:
        pa = pyaudio.PyAudio()
        inputs, outputs = [], []
        for i in range(pa.get_device_count()):
            info = pa.get_device_info_by_index(i)
            entry = {"index": i, "name": info["name"]}
            if info["maxInputChannels"] > 0:
                inputs.append(entry)
            if info["maxOutputChannels"] > 0:
                outputs.append(entry)
        pa.terminate()
        return {"inputs": inputs, "outputs": outputs}
    except Exception as e:
        return {"inputs": [], "outputs": [], "error": str(e)}
