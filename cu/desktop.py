"""Stateful desktop controller: one portal session + EI connection, screenshot mapping."""
import io
import os
import shutil
import subprocess
import threading
import time
from dataclasses import dataclass

from PIL import Image as PILImage

from . import ei
from .keys import BUTTONS, CHARS, KEYS, parse_combo
from .portal import Portal, RemoteSession

SHIFT = KEYS["shift"]


@dataclass
class Shot:
    """Maps pixels of the last returned image back to logical desktop coordinates."""
    origin_x: float
    origin_y: float
    scale: float  # image px per logical px
    width: int
    height: int

    def to_desktop(self, x, y):
        return self.origin_x + x / self.scale, self.origin_y + y / self.scale


class Desktop:
    def __init__(self):
        self.lock = threading.RLock()
        self._portal = None
        self.session = None
        self.client = None
        self.last_shot = None

    # -- connection -----------------------------------------------------------------------
    @property
    def portal(self):
        if self._portal is None:
            self._portal = Portal()
        return self._portal

    def _ensure(self):
        if self.client:
            self.client.pump(0)
            if not self.client.disconnected:
                return
            self._teardown()
        self.session = RemoteSession(self.portal).start()
        self.client = ei.EIClient(self.session.connect_to_eis())
        self.client.wait_ready([ei.CAP_POINTER_ABSOLUTE, ei.CAP_BUTTON, ei.CAP_KEYBOARD])

    def _teardown(self):
        if self.client:
            self.client.close()
        if self.session:
            self.session.close()
        self.client = self.session = None

    def monitors(self):
        """Logical monitor rects [(name, x, y, w, h, scale)], sorted left to right."""
        with self.lock:
            self._ensure()
            regs = self.client.device_for(ei.CAP_POINTER_ABSOLUTE).regions
            return sorted([(r.mapping_id, r.x, r.y, r.w, r.h, r.scale) for r in regs],
                          key=lambda m: (m[1], m[2]))

    # -- screenshots ----------------------------------------------------------------------
    def screenshot(self, monitor=None, region=None, max_width=1920, max_height=1200):
        """Return (png bytes, Shot). region=(x, y, w, h) in logical desktop coords."""
        with self.lock:
            path = self.portal.screenshot()
            try:
                img = PILImage.open(path).convert("RGB")
            finally:
                try:
                    os.unlink(path)
                except OSError:
                    pass
            if monitor is not None:
                mons = self.monitors()
                m = next((m for m in mons if m[0] == monitor), None)
                if m is None and str(monitor).isdigit() and int(monitor) < len(mons):
                    m = mons[int(monitor)]
                if m is None:
                    raise ValueError(f"unknown monitor {monitor!r}; have {[m[0] for m in mons]}")
                region = m[1:5]
            ox, oy = 0, 0
            if region:
                x, y, w, h = (int(v) for v in region)
                img = img.crop((x, y, x + w, y + h))
                ox, oy = x, y
            scale = min(1.0 if not region else 4.0, max_width / img.width, max_height / img.height)
            if scale != 1.0:
                img = img.resize((round(img.width * scale), round(img.height * scale)),
                                 PILImage.LANCZOS)
            buf = io.BytesIO()
            img.save(buf, "PNG", optimize=False)
            self.last_shot = Shot(ox, oy, scale, img.width, img.height)
            return buf.getvalue(), self.last_shot

    def resolve(self, x, y, space):
        if space == "desktop":
            return float(x), float(y)
        if self.last_shot is None:
            raise RuntimeError("no screenshot taken yet; take one or pass space='desktop'")
        return self.last_shot.to_desktop(x, y)

    # -- input ----------------------------------------------------------------------------
    def move(self, x, y):
        with self.lock:
            self._ensure()
            self.client.move_abs(x, y)

    def click(self, x, y, button="left", count=1, modifiers=""):
        code = BUTTONS[button]
        mods = parse_combo(modifiers) if modifiers else []
        with self.lock:
            self._ensure()
            c = self.client
            c.move_abs(x, y)
            time.sleep(0.03)
            for m in mods:
                c.key(m, True)
            for _ in range(count):
                c.button(code, True)
                time.sleep(0.02)
                c.button(code, False)
                time.sleep(0.06)
            for m in reversed(mods):
                c.key(m, False)
            c.pump(0.02)

    def drag(self, x1, y1, x2, y2, button="left", steps=20):
        code = BUTTONS[button]
        with self.lock:
            self._ensure()
            c = self.client
            c.move_abs(x1, y1)
            time.sleep(0.03)
            c.button(code, True)
            for i in range(1, steps + 1):
                t = i / steps
                c.move_abs(x1 + (x2 - x1) * t, y1 + (y2 - y1) * t)
                time.sleep(0.015)
            time.sleep(0.05)
            c.button(code, False)
            c.pump(0.02)

    def scroll(self, x, y, dx, dy):
        with self.lock:
            self._ensure()
            self.client.move_abs(x, y)
            time.sleep(0.02)
            self.client.scroll(dx, dy)
            self.client.pump(0.02)

    def key(self, combo, repeat=1):
        codes = parse_combo(combo)
        with self.lock:
            self._ensure()
            c = self.client
            for _ in range(repeat):
                for k in codes:
                    c.key(k, True)
                time.sleep(0.02)
                for k in reversed(codes):
                    c.key(k, False)
                time.sleep(0.02)
            c.pump(0.02)

    def type_text(self, text, delay=0.008):
        """US-layout keystrokes for ASCII; anything else is pasted via wl-copy + ctrl+v."""
        if not all(ch in CHARS for ch in text):
            return self._paste(text)
        with self.lock:
            self._ensure()
            c = self.client
            for ch in text:
                code, shift = CHARS[ch]
                if shift:
                    c.key(SHIFT, True)
                c.key(code, True)
                c.key(code, False)
                if shift:
                    c.key(SHIFT, False)
                time.sleep(delay)
            c.pump(0.02)
        return "typed"

    def _paste(self, text):
        if not shutil.which("wl-copy"):
            raise RuntimeError("non-ASCII text needs wl-copy (wl-clipboard)")
        subprocess.run(["wl-copy", "--", text], check=True, timeout=5)
        time.sleep(0.1)
        self.key("ctrl+v")
        return "pasted via clipboard (ctrl+v); terminals may need ctrl+shift+v instead"
