"""Push-to-talk dictation for Linux X11, with a tray menu for settings.

Hold the push-to-talk key (default Right Ctrl), speak, and each final transcript is
typed into the focused window. Settings live in ~/.config/voicecode/config.json.
Run: .venv/bin/voicecode   (see README for the venv setup)
"""
import asyncio
import json
import logging
import logging.handlers
import os
import shutil
import subprocess
import sys
import threading
import time
import urllib.parse
from pathlib import Path

from websockets.asyncio.client import connect

ENDPOINT_ENV = "VOICECODE_ENDPOINT"  # default endpoint; the tray setting overrides it
AUDIO_CMD = ["arecord", "-q", "-t", "raw", "-f", "S16_LE", "-r", "16000", "-c", "1"]
CHUNK_BYTES = 3200  # 100 ms of 16 kHz mono 16-bit audio
FINAL_TIMEOUT_S = 5.0
ERROR_SHOW_MS = 3000
PTT_POLL_S = 0.02

CONFIG_DIR = Path.home() / ".config" / "voicecode"
CONFIG_FILE = CONFIG_DIR / "config.json"
VOCAB_FILE = CONFIG_DIR / "vocabulary.txt"
LOG_FILE = Path.home() / ".cache" / "voicecode.log"

# Menu label -> stored value. "" language means auto-detect (no ?language= param).
LANGUAGES = {"English (en-US)": "en-US", "Chinese (zh-CN)": "zh-CN", "Auto-detect": ""}
PTT_KEYS = {"Right Ctrl": "Control_R", "Right Alt": "Alt_R", "F9": "F9", "Pause": "Pause"}
DEFAULT_CONFIG = {"language": "en-US", "ptt_key": "Control_R", "paused": False, "endpoint": ""}  # "" = use $VOICECODE_ENDPOINT

ICON_IDLE = "audio-input-microphone-symbolic"
ICON_RECORDING = "audio-input-microphone-high-symbolic"
ICON_PAUSED = "microphone-disabled-symbolic"

_type_lock = asyncio.Lock()
ptt_down = threading.Event()  # set by the key watcher while the PTT key is held
log = logging.getLogger("voicecode")


def app_version():
    """Version from the installed package metadata (pyproject.toml); "dev" if not installed."""
    from importlib.metadata import PackageNotFoundError, version
    try:
        return version("voicecode")
    except PackageNotFoundError:
        return "dev"


def setup_logging():
    """Always log to LOG_FILE (so the tray's "Open log" works); also to the terminal if any."""
    LOG_FILE.parent.mkdir(parents=True, exist_ok=True)
    handlers = [logging.handlers.RotatingFileHandler(LOG_FILE, maxBytes=1_000_000, backupCount=1)]
    if sys.stderr.isatty():
        handlers.append(logging.StreamHandler())
    logging.basicConfig(level=logging.INFO, handlers=handlers,
                        format="%(asctime)s %(levelname)s %(message)s")
    sys.excepthook = lambda *exc: log.critical("crash", exc_info=exc)
    threading.excepthook = lambda a: log.critical(
        "crash in thread %s", a.thread.name, exc_info=(a.exc_type, a.exc_value, a.exc_traceback))


def fail(msg):
    log.error(msg)
    if not sys.stderr.isatty():  # started by double-click or autostart: nobody sees stderr
        try:
            import gi
            gi.require_version("Gtk", "3.0")
            from gi.repository import Gtk
            dlg = Gtk.MessageDialog(message_type=Gtk.MessageType.ERROR, buttons=Gtk.ButtonsType.CLOSE,
                                    text="voicecode cannot start", secondary_text=msg)
            dlg.run()
        except Exception:
            pass  # no display or no GTK: the log file still has the message
    sys.exit(msg)


def single_instance_lock():
    """Two copies would both hear the key and type everything twice."""
    import fcntl
    lock = open(LOG_FILE.with_name("voicecode.lock"), "w")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
    except BlockingIOError:
        fail("voicecode is already running. Use its tray icon, or quit it first.")
    return lock  # keep the file open: the lock lasts as long as the process


def load_config(path=CONFIG_FILE):
    cfg = dict(DEFAULT_CONFIG)
    try:
        saved = json.loads(path.read_text())
    except FileNotFoundError:
        return cfg
    except (OSError, ValueError) as exc:
        log.warning("Ignoring unreadable %s: %s", path, exc)
        return cfg
    # Keep only known keys with valid values, so a hand-edited typo cannot crash startup.
    if saved.get("language") in LANGUAGES.values():
        cfg["language"] = saved["language"]
    if saved.get("ptt_key") in PTT_KEYS.values():
        cfg["ptt_key"] = saved["ptt_key"]
    if isinstance(saved.get("paused"), bool):
        cfg["paused"] = saved["paused"]
    if isinstance(saved.get("endpoint"), str) and valid_endpoint(saved["endpoint"]):  # else "" = env
        cfg["endpoint"] = saved["endpoint"]
    return cfg


def valid_endpoint(url):
    """wss:// anywhere; plain ws:// only to localhost, since the API key rides in a header."""
    try:
        p = urllib.parse.urlsplit(url)
        host = p.hostname
    except ValueError:
        return False
    if p.scheme == "wss" and host:
        return True
    return p.scheme == "ws" and host in ("localhost", "127.0.0.1", "::1")


def save_config(cfg, path=CONFIG_FILE):
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".tmp")
    tmp.write_text(json.dumps(cfg, indent=2) + "\n")
    tmp.replace(path)  # atomic, so a crash never leaves a half-written config


def resolve_endpoint(cfg):
    return cfg["endpoint"] or os.environ.get(ENDPOINT_ENV, "").strip()


def stream_url(language, endpoint):
    if not language:
        return endpoint
    sep = "&" if "?" in endpoint else "?"
    return f"{endpoint}{sep}language={urllib.parse.quote(language)}"


def load_vocabulary(path=VOCAB_FILE):
    if not path.exists():
        return []
    lines = (line.strip() for line in path.read_text().splitlines())
    return [line for line in lines if line and not line.startswith("#")]


async def xdotool_type(text):
    # Serialize so two sessions never interleave keystrokes.
    async with _type_lock:
        # Wait for the PTT key to be released: typing while Ctrl is held turns letters into
        # shortcuts, and xdotool --clearmodifiers re-presses Ctrl afterwards, which can leave it stuck.
        while ptt_down.is_set():
            await asyncio.sleep(PTT_POLL_S)
        proc = await asyncio.create_subprocess_exec("xdotool", "type", "--delay", "5", "--", text)
        await proc.wait()


async def run_session(stop, ui, *, key, url, type_text=xdotool_type,
                      audio_cmd=AUDIO_CMD, vocabulary=()):
    """One push-to-talk press: record until `stop` is set, stream, type finals."""
    log.info("session start: %s", url)  # never log the key or transcript text
    proc = await asyncio.create_subprocess_exec(
        *audio_cmd, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.DEVNULL)
    chunks = asyncio.Queue()  # buffers audio while the handshake (~1.4 s) runs

    async def capture():
        while chunk := await proc.stdout.read(CHUNK_BYTES):
            chunks.put_nowait(chunk)
        chunks.put_nowait(None)

    async def stopper():
        await stop.wait()
        if proc.returncode is None:
            proc.terminate()

    tasks = [asyncio.create_task(capture()), asyncio.create_task(stopper())]
    try:
        async with connect(url, additional_headers={"Authorization": f"Bearer {key}"},
                           open_timeout=10) as ws:
            if vocabulary:
                await ws.send(json.dumps({"type": "vocabulary", "phrases": list(vocabulary)}))

            async def sender():
                while (chunk := await chunks.get()) is not None:
                    await ws.send(chunk)
                await ws.send(json.dumps({"type": "audio.done"}))

            async def receiver():
                async for raw in ws:  # ends when the server closes after session.ended
                    msg = json.loads(raw)
                    kind = msg.get("type")
                    if kind == "transcript.partial":
                        ui.show_partial(msg.get("text", ""))
                    elif kind == "transcript.final":
                        text = msg.get("text", "").strip()
                        if text:
                            ui.add_final(text)
                            await type_text(text + " ")
                    elif kind == "error":
                        raise RuntimeError(msg.get("message", "unknown API error"))

            send_task = asyncio.create_task(sender())
            recv_task = asyncio.create_task(receiver())
            tasks += [send_task, recv_task]
            await asyncio.wait({send_task, recv_task}, return_when=asyncio.FIRST_COMPLETED)
            if recv_task.done():
                recv_task.result()  # surfaces an API error or a dropped connection
            else:
                send_task.result()
                await asyncio.wait_for(recv_task, FINAL_TIMEOUT_S)
        ui.hide()
        log.info("session ok")
    except asyncio.TimeoutError:
        log.warning("session timeout: no final transcript within %s s", FINAL_TIMEOUT_S)
        ui.error("No final transcript within 5 s")
    except Exception as exc:  # network, auth, or API error: show it, type nothing more
        log.warning("session failed: %s: %s", type(exc).__name__, exc)
        ui.error(f"{type(exc).__name__}: {exc}")
    finally:
        if proc.returncode is None:
            proc.kill()
        for t in tasks:
            t.cancel()
        await proc.wait()


class App:
    """GTK overlay + tray. Public methods are safe to call from any thread (GLib.idle_add)."""

    def __init__(self, cfg):
        import gi
        gi.require_version("Gtk", "3.0")
        gi.require_version("AyatanaAppIndicator3", "0.1")
        from gi.repository import AyatanaAppIndicator3 as AI, GLib, Gtk
        self.Gtk, self.GLib = Gtk, GLib
        self.cfg = cfg
        self.recording = False
        self.hide_job = None

        # Overlay: a POPUP window is override-redirect on X11, so it never takes focus.
        self.win = Gtk.Window(type=Gtk.WindowType.POPUP)
        self.win.set_keep_above(True)
        self.win.set_accept_focus(False)
        css = Gtk.CssProvider()
        css.load_from_data(b"""
            window { background: #1e1e1e; }
            label { font-size: 14pt; padding: 0 14px; }
            .final { color: white; } .partial { color: #9a9a9a; } .error { color: #ff6b6b; }
        """)
        Gtk.StyleContext.add_provider_for_screen(
            self.win.get_screen(), css, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, margin_top=8, margin_bottom=8)
        self.final = Gtk.Label(xalign=0, wrap=True, max_width_chars=70)
        self.partial = Gtk.Label(xalign=0, wrap=True, max_width_chars=70)
        self.final.get_style_context().add_class("final")
        self.partial.get_style_context().add_class("partial")
        box.add(self.final)
        box.add(self.partial)
        self.win.add(box)

        # Tray
        self.ind = AI.Indicator.new("voicecode", ICON_IDLE, AI.IndicatorCategory.APPLICATION_STATUS)
        self.ind.set_status(AI.IndicatorStatus.ACTIVE)
        self.ind.set_title("voicecode")
        self.ind.set_menu(self._build_menu())
        self._update_icon()

    def _build_menu(self):
        Gtk = self.Gtk
        menu = Gtk.Menu()

        about = Gtk.MenuItem(label=f"voicecode {app_version()}")
        about.set_sensitive(False)  # info only
        menu.append(about)
        menu.append(Gtk.SeparatorMenuItem())

        pause = Gtk.CheckMenuItem(label="Paused")
        pause.set_active(self.cfg["paused"])
        pause.connect("toggled", lambda w: self._set("paused", w.get_active()))
        menu.append(pause)

        for title, setting, choices in (("Language", "language", LANGUAGES),
                                        ("Push-to-talk key", "ptt_key", PTT_KEYS)):
            sub, group = Gtk.Menu(), None
            for label, value in choices.items():
                item = Gtk.RadioMenuItem.new_with_label_from_widget(group, label)
                group = item
                item.set_active(self.cfg[setting] == value)
                # "toggled" fires for the item turning off too; act only on the one turning on.
                item.connect("toggled", lambda w, s=setting, v=value: w.get_active() and self._set(s, v))
                sub.append(item)
            parent = Gtk.MenuItem(label=title)
            parent.set_submenu(sub)
            menu.append(parent)

        menu.append(Gtk.SeparatorMenuItem())
        for label, action in (("Endpoint URL…", self._edit_endpoint),
                              ("Edit vocabulary…", self._edit_vocabulary),
                              ("Open log", lambda: self._open(LOG_FILE)),
                              ("Quit", Gtk.main_quit)):
            item = Gtk.MenuItem(label=label)
            item.connect("activate", lambda w, a=action: a())
            menu.append(item)
        menu.show_all()
        return menu

    def _set(self, setting, value):
        self.cfg[setting] = value
        save_config(self.cfg)
        self._update_icon()

    def _edit_endpoint(self):
        Gtk = self.Gtk
        RESET = 1
        dlg = Gtk.Dialog(title="voicecode: endpoint URL", modal=True)
        dlg.add_buttons("_Reset to default", RESET, "_Cancel", Gtk.ResponseType.CANCEL,
                        "_Save", Gtk.ResponseType.OK)
        dlg.set_default_response(Gtk.ResponseType.OK)
        env_url = os.environ.get(ENDPOINT_ENV, "").strip()
        entry = Gtk.Entry(text=resolve_endpoint(self.cfg), width_chars=50, activates_default=True)
        label = Gtk.Label(label="_WebSocket endpoint:", use_underline=True, xalign=0)
        label.set_mnemonic_widget(entry)  # Alt+W focuses it; screen readers read the label
        hint = Gtk.Label(xalign=0, label="wss:// URL. Plain ws:// is allowed only for localhost.\n"
                         f"Default (${ENDPOINT_ENV}): {env_url or 'not set'}")
        box = dlg.get_content_area()
        box.set_spacing(6)
        box.set_border_width(12)
        for w in (label, entry, hint):
            box.add(w)
        dlg.show_all()
        while True:
            resp = dlg.run()
            if resp == RESET:
                if valid_endpoint(env_url):
                    self._set("endpoint", "")  # drop the override, follow the env variable
                    break
                hint.set_markup(f'<span foreground="#d33">${ENDPOINT_ENV} is not set or not '
                                'a valid wss:// URL, so there is no default to reset to.</span>')
                continue
            if resp != Gtk.ResponseType.OK:
                break
            url = entry.get_text().strip()
            if valid_endpoint(url):
                self._set("endpoint", url)
                break
            hint.set_markup('<span foreground="#d33">Not a valid URL. Use wss://host/path '
                            '(ws:// only for localhost).</span>')
        dlg.destroy()

    def _edit_vocabulary(self):
        if not VOCAB_FILE.exists():
            VOCAB_FILE.parent.mkdir(parents=True, exist_ok=True)
            VOCAB_FILE.write_text("# One phrase per line. Lines starting with # are ignored.\n")
        self._open(VOCAB_FILE)

    def _open(self, path):
        if path.exists():
            subprocess.Popen(["xdg-open", str(path)])
        else:
            self._error(f"{path} does not exist yet")

    def _update_icon(self):
        icon = ICON_PAUSED if self.cfg["paused"] else ICON_RECORDING if self.recording else ICON_IDLE
        self.ind.set_icon_full(icon, icon)

    # --- thread-safe API used by run_session and the key watcher ---
    def start(self): self.GLib.idle_add(self._start)
    def show_partial(self, text): self.GLib.idle_add(self._partial, text)
    def add_final(self, text): self.GLib.idle_add(self._final, text)
    def error(self, text): self.GLib.idle_add(self._error, text)
    def hide(self): self.GLib.idle_add(self._hide)

    def _cancel_hide(self):
        if self.hide_job:
            self.GLib.source_remove(self.hide_job)
            self.hide_job = None

    def _start(self):
        self._cancel_hide()
        self.recording = True
        self._update_icon()
        self._set_final("", "final")
        self.partial.set_text("Listening…")
        self._show()

    def _partial(self, text):
        self.partial.set_text(text)
        self._show()  # re-center as the text grows

    def _final(self, text):
        self._set_final(f"{self.final.get_text()} {text}".strip(), "final")
        self.partial.set_text("")
        self._show()

    def _error(self, text):
        self._cancel_hide()
        self.recording = False
        self._update_icon()
        self._set_final(text, "error")
        self.partial.set_text("")
        self._show()
        self.hide_job = self.GLib.timeout_add(ERROR_SHOW_MS, self._hide)

    def _hide(self):
        self._cancel_hide()
        self.recording = False
        self._update_icon()
        self.win.hide()
        return False  # also used as a GLib timeout callback: run once

    def _set_final(self, text, cls):
        ctx = self.final.get_style_context()
        ctx.remove_class("final")
        ctx.remove_class("error")
        ctx.add_class(cls)
        self.final.set_text(text)
        self.final.set_visible(bool(text))

    def _show(self):
        # A wrapping GtkLabel asks for its *minimum* width (one word), so set the width explicitly.
        chars = min(70, max(10, len(self.final.get_text()), len(self.partial.get_text())))
        self.final.set_width_chars(chars)
        self.partial.set_width_chars(chars)
        self.win.resize(1, 1)  # shrink to fit the current text
        self.win.show_all()
        self.final.set_visible(bool(self.final.get_text()))
        self.partial.set_visible(bool(self.partial.get_text()))
        w, h = self.win.get_size()
        screen = self.win.get_screen()
        self.win.move((screen.get_width() - w) // 2, screen.get_height() - h - 80)


def watch_ptt_key(app, loop, key):
    """Poll the X11 keymap for the configured key; start a session on press, stop on release."""
    from Xlib import XK, display
    d = display.Display()
    keysym_name, code = None, 0
    stop = None  # stop event of the session being recorded, None when idle
    while True:
        if keysym_name != app.cfg["ptt_key"]:  # setting changed from the tray
            keysym_name = app.cfg["ptt_key"]
            code = d.keysym_to_keycode(XK.string_to_keysym(keysym_name))
            if not code:
                app.error(f"Key {keysym_name} is not on this keyboard layout")
        down = bool(code) and bool(d.query_keymap()[code // 8] & (1 << (code % 8)))
        ptt_down.set() if down else ptt_down.clear()
        if down and stop is None and not app.cfg["paused"]:
            stop = asyncio.Event()
            app.start()
            asyncio.run_coroutine_threadsafe(
                run_session(stop, app, key=key, url=stream_url(app.cfg["language"], resolve_endpoint(app.cfg)),
                            vocabulary=load_vocabulary()), loop)
        elif not down and stop is not None:
            loop.call_soon_threadsafe(stop.set)
            stop = None
        time.sleep(PTT_POLL_S)


def main():
    setup_logging()
    _lock = single_instance_lock()  # noqa: F841
    key = os.environ.get("VOICECODE_API_KEY")
    if not key:
        fail("VOICECODE_API_KEY is not set. Export it, then run again.")
    for tool in ("arecord", "xdotool"):
        if not shutil.which(tool):
            fail(f"'{tool}' not found. Install it: sudo apt install {'alsa-utils' if tool == 'arecord' else tool}")

    cfg = load_config()
    if not valid_endpoint(resolve_endpoint(cfg)):
        fail(f"No valid endpoint. Export {ENDPOINT_ENV}=wss://<host>/<path>, then run again.")

    import signal
    app = App(cfg)
    loop = asyncio.new_event_loop()
    threading.Thread(target=loop.run_forever, daemon=True).start()
    threading.Thread(target=watch_ptt_key, args=(app, loop, key), daemon=True).start()
    app.GLib.unix_signal_add(app.GLib.PRIORITY_DEFAULT, signal.SIGINT, app.Gtk.main_quit)
    log.info("voicecode %s ready (endpoint %s). Use the tray icon for settings. Ctrl+C to quit.",
             app_version(), resolve_endpoint(cfg))
    app.Gtk.main()


if __name__ == "__main__":
    main()
