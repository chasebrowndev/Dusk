#!/usr/bin/env python3
import sys
import os
import argparse
import logging
import signal

logging.basicConfig(
    level=logging.DEBUG,
    format="%(asctime)s [%(levelname)s] %(name)s: %(message)s"
)

log = logging.getLogger(__name__)

from nexus.api import NexusAPI


def _load_config():
    """Load config.toml from the same directory as this script."""
    config_path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "config.toml")
    cfg = {
        "name":    None,
        "host_ip": None,
        "port":    7337,
        "width":   900,
        "height":  640,
        "env":     {},
    }
    if not os.path.exists(config_path):
        log.warning("config.toml not found at %s — using defaults", config_path)
        return cfg
    try:
        try:
            import tomllib
        except ImportError:
            import tomli as tomllib  # pip install tomli for Python <3.11
        with open(config_path, "rb") as f:
            raw = tomllib.load(f)
        cfg["name"]    = raw.get("name",    cfg["name"])
        cfg["host_ip"] = raw.get("host_ip", cfg["host_ip"])
        cfg["port"]    = int(raw.get("port",   cfg["port"]))
        cfg["width"]   = int(raw.get("width",  cfg["width"]))
        cfg["height"]  = int(raw.get("height", cfg["height"]))
        cfg["env"]     = {k: str(v) for k, v in raw.get("env", {}).items()}
    except Exception as e:
        log.error("failed to parse config.toml: %s — using defaults", e)
    return cfg


def main():
    cfg = _load_config()

    # Apply env vars from config (launcher also sets them, but this is a
    # safety net when launched directly without the wrapper script).
    for k, v in cfg["env"].items():
        os.environ.setdefault(k, v)

    parser = argparse.ArgumentParser(description="Dusk P2P communicator")
    parser.add_argument("--host",    action="store_true",
                        help="Force host role (overrides config.toml)")
    parser.add_argument("--connect", metavar="IP",
                        help="Force client role, connect to IP (overrides config.toml)")
    parser.add_argument("--port",    type=int, default=None)
    parser.add_argument("--name",    default=None)
    args = parser.parse_args()

    # Role is determined by the launcher script (--host / --connect).
    # Bare fallback: no flag → client, connect to host_ip from config.
    if args.host:
        is_host   = True
        remote_ip = None
    elif args.connect:
        is_host   = False
        remote_ip = args.connect
    else:
        is_host   = False
        remote_ip = cfg["host_ip"]

    port         = args.port or cfg["port"]
    display_name = args.name or cfg["name"]

    import webview

    api = NexusAPI(
        is_host=is_host,
        remote_ip=remote_ip,
        port=port,
        display_name=display_name,
    )

    # Host 8080 / client 8081 — only matters if both run on same machine
    http_port = 8080 if is_host else 8081

    window = webview.create_window(
        "Dusk" + (" [Host]" if is_host else " [Client]"),
        url="nexus/ui/index.html",
        js_api=api,
        width=cfg["width"],
        height=cfg["height"],
        min_size=(720, 480),
        background_color="#0a0a0a",
        text_select=True,
        easy_drag=False,        # don't intercept mouse events for window dragging
    )

    api.set_window(window)

    def on_closing():
        api.disconnect()

    window.events.closing += on_closing

    def _shutdown(signum, frame):
        log.info("signal %d received — shutting down", signum)
        api.disconnect()
        # Hard-exit watchdog: if GTK loop is frozen and never drains the
        # idle_add below, this daemon thread forces an exit after 2 seconds.
        import threading
        def _force_exit():
            import time
            time.sleep(2)
            log.warning("GTK loop did not exit cleanly — forcing sys.exit")
            sys.exit(0)
        threading.Thread(target=_force_exit, daemon=True).start()
        # window.destroy() must run on the GTK main thread.
        try:
            import gi
            gi.require_version("GLib", "2.0")
            from gi.repository import GLib
            def _do_destroy():
                try:
                    window.destroy()
                except Exception:
                    pass
                return False
            GLib.idle_add(_do_destroy)
        except Exception:
            sys.exit(0)

    signal.signal(signal.SIGINT,  _shutdown)
    signal.signal(signal.SIGTERM, _shutdown)

    def _setup_webview():
        """
        Runs inside the GTK main loop (via webview.start func= arg).
        1. Disables WebKit2 hardware acceleration via the settings API — fixes
           blank white screen / GBM errors on Hyprland/XWayland without using
           WEBKIT_DISABLE_COMPOSITING_MODE (which breaks input).
        2. Hooks permission-request signal for camera/screen-share.
        """
        try:
            import gi
            for ver in ("4.1", "4.0"):
                try:
                    gi.require_version("WebKit2", ver)
                    break
                except ValueError:
                    continue
            from gi.repository import WebKit2
            gi.require_version("GLib", "2.0")
            from gi.repository import GLib as _GLib

            def _on_permission_request(webview, request):
                if isinstance(request, (
                    WebKit2.UserMediaPermissionRequest,
                    WebKit2.DeviceInfoPermissionRequest,
                )):
                    request.allow()
                    return True
                display_cls = getattr(WebKit2, "DisplayCapturePermissionRequest", None)
                if display_cls and isinstance(request, display_cls):
                    request.allow()
                    return True
                return False

            def _attach_to_webview(wv):
                settings = wv.get_settings()
                try:
                    settings.set_property("hardware-acceleration-policy",
                                          WebKit2.HardwareAccelerationPolicy.NEVER)
                    log.info("WebKit2: hardware acceleration disabled via settings API")
                except Exception as e:
                    log.warning("could not set hardware-acceleration-policy: %s", e)
                wv.connect("permission-request", _on_permission_request)
                log.info("permission-request signal connected")

            def _attach_signal():
                gi.require_version("Gtk", "3.0")
                from gi.repository import Gtk
                def _walk(widget):
                    if isinstance(widget, WebKit2.WebView):
                        _attach_to_webview(widget)
                        return
                    if hasattr(widget, "get_children"):
                        for child in widget.get_children():
                            _walk(child)
                for win in Gtk.Window.list_toplevels():
                    _walk(win)

            _GLib.timeout_add(300, lambda: (_attach_signal(), False)[1])

        except Exception as e:
            log.warning("could not configure WebKit2: %s", e)

    webview.start(
        _setup_webview,
        debug=False,
        private_mode=False,
        http_port=http_port,
    )


if __name__ == "__main__":
    main()
