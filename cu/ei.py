"""Minimal ctypes bindings for libei (sender side)."""
import ctypes as C
import select
import time

lib = C.CDLL("libei.so.1")

CAP_POINTER = 1 << 0
CAP_POINTER_ABSOLUTE = 1 << 1
CAP_KEYBOARD = 1 << 2
CAP_TOUCH = 1 << 3
CAP_SCROLL = 1 << 4
CAP_BUTTON = 1 << 5
CAP_TEXT = 1 << 6

EV_CONNECT, EV_DISCONNECT, EV_SEAT_ADDED, EV_SEAT_REMOVED = 1, 2, 3, 4
EV_DEVICE_ADDED, EV_DEVICE_REMOVED, EV_DEVICE_PAUSED, EV_DEVICE_RESUMED = 5, 6, 7, 8

P = C.c_void_p


def _fn(name, res, *args):
    f = getattr(lib, name)
    f.restype, f.argtypes = res, list(args)
    return f


ei_new_sender = _fn("ei_new_sender", P, P)
ei_unref = _fn("ei_unref", P, P)
ei_configure_name = _fn("ei_configure_name", None, P, C.c_char_p)
ei_setup_backend_fd = _fn("ei_setup_backend_fd", C.c_int, P, C.c_int)
ei_get_fd = _fn("ei_get_fd", C.c_int, P)
ei_dispatch = _fn("ei_dispatch", None, P)
ei_get_event = _fn("ei_get_event", P, P)
ei_now = _fn("ei_now", C.c_uint64, P)
ei_event_unref = _fn("ei_event_unref", P, P)
ei_event_get_type = _fn("ei_event_get_type", C.c_int, P)
ei_event_get_seat = _fn("ei_event_get_seat", P, P)
ei_event_get_device = _fn("ei_event_get_device", P, P)
ei_seat_get_name = _fn("ei_seat_get_name", C.c_char_p, P)
ei_seat_has_capability = _fn("ei_seat_has_capability", C.c_bool, P, C.c_int)
ei_seat_bind_capabilities = lib.ei_seat_bind_capabilities  # variadic, NULL-terminated
ei_seat_bind_capabilities.restype = None
ei_device_ref = _fn("ei_device_ref", P, P)
ei_device_get_name = _fn("ei_device_get_name", C.c_char_p, P)
ei_device_has_capability = _fn("ei_device_has_capability", C.c_bool, P, C.c_int)
ei_device_get_region = _fn("ei_device_get_region", P, P, C.c_size_t)
ei_region_get_x = _fn("ei_region_get_x", C.c_uint32, P)
ei_region_get_y = _fn("ei_region_get_y", C.c_uint32, P)
ei_region_get_width = _fn("ei_region_get_width", C.c_uint32, P)
ei_region_get_height = _fn("ei_region_get_height", C.c_uint32, P)
ei_region_get_physical_scale = _fn("ei_region_get_physical_scale", C.c_double, P)
ei_region_get_mapping_id = _fn("ei_region_get_mapping_id", C.c_char_p, P)
ei_device_start_emulating = _fn("ei_device_start_emulating", None, P, C.c_uint32)
ei_device_stop_emulating = _fn("ei_device_stop_emulating", None, P)
ei_device_frame = _fn("ei_device_frame", None, P, C.c_uint64)
ei_device_pointer_motion = _fn("ei_device_pointer_motion", None, P, C.c_double, C.c_double)
ei_device_pointer_motion_absolute = _fn("ei_device_pointer_motion_absolute", None,
                                        P, C.c_double, C.c_double)
ei_device_button_button = _fn("ei_device_button_button", None, P, C.c_uint32, C.c_bool)
ei_device_scroll_delta = _fn("ei_device_scroll_delta", None, P, C.c_double, C.c_double)
ei_device_scroll_discrete = _fn("ei_device_scroll_discrete", None, P, C.c_int32, C.c_int32)
ei_device_keyboard_key = _fn("ei_device_keyboard_key", None, P, C.c_uint32, C.c_bool)

_ALL_CAPS = [CAP_POINTER, CAP_POINTER_ABSOLUTE, CAP_KEYBOARD, CAP_SCROLL, CAP_BUTTON]


class Region:
    def __init__(self, ptr):
        self.x = ei_region_get_x(ptr)
        self.y = ei_region_get_y(ptr)
        self.w = ei_region_get_width(ptr)
        self.h = ei_region_get_height(ptr)
        self.scale = ei_region_get_physical_scale(ptr)
        mid = ei_region_get_mapping_id(ptr)
        self.mapping_id = mid.decode() if mid else None

    def contains(self, x, y):
        return self.x <= x < self.x + self.w and self.y <= y < self.y + self.h

    def __repr__(self):
        return f"Region({self.x},{self.y} {self.w}x{self.h} scale={self.scale} id={self.mapping_id})"


class Device:
    def __init__(self, ptr):
        self.ptr = ei_device_ref(ptr)
        self.name = (ei_device_get_name(ptr) or b"").decode()
        self.caps = {c for c in _ALL_CAPS if ei_device_has_capability(ptr, c)}
        self.regions = []
        i = 0
        while (r := ei_device_get_region(ptr, i)):
            self.regions.append(Region(r))
            i += 1
        self.resumed = False
        self.emulating = False

    def __repr__(self):
        return f"Device({self.name!r} caps={sorted(self.caps)} regions={self.regions})"


class EIClient:
    """Sender-side EI connection. Call pump() to process compositor events."""

    def __init__(self, fd: int, name="claude-computer-use"):
        self.ei = ei_new_sender(None)
        ei_configure_name(self.ei, name.encode())
        rc = ei_setup_backend_fd(self.ei, fd)
        if rc != 0:
            raise OSError(-rc, "ei_setup_backend_fd failed")
        self.devices = {}  # ptr value -> Device
        self.connected = False
        self.disconnected = False
        self._seq = 1

    def pump(self, timeout=0.0):
        fd = ei_get_fd(self.ei)
        r, _, _ = select.select([fd], [], [], timeout)
        if r:
            ei_dispatch(self.ei)
        while (ev := ei_get_event(self.ei)):
            t = ei_event_get_type(ev)
            if t == EV_CONNECT:
                self.connected = True
            elif t == EV_DISCONNECT:
                self.disconnected = True
            elif t == EV_SEAT_ADDED:
                seat = ei_event_get_seat(ev)
                caps = [c for c in _ALL_CAPS if ei_seat_has_capability(seat, c)]
                ei_seat_bind_capabilities(C.c_void_p(seat), *[C.c_int(c) for c in caps], C.c_void_p(None))
            elif t == EV_DEVICE_ADDED:
                d = ei_event_get_device(ev)
                self.devices[d] = Device(d)
            elif t == EV_DEVICE_REMOVED:
                self.devices.pop(ei_event_get_device(ev), None)
            elif t == EV_DEVICE_RESUMED:
                if (dev := self.devices.get(ei_event_get_device(ev))):
                    dev.resumed = True
            elif t == EV_DEVICE_PAUSED:
                if (dev := self.devices.get(ei_event_get_device(ev))):
                    dev.resumed = False
                    dev.emulating = False
            ei_event_unref(ev)

    def wait_ready(self, caps, timeout=5.0):
        """Pump until resumed devices cover all requested capabilities."""
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            self.pump(0.05)
            if self.disconnected:
                raise ConnectionError("EIS disconnected")
            have = set().union(*[d.caps for d in self.devices.values() if d.resumed])
            if set(caps) <= have:
                return
        raise TimeoutError(f"EIS devices not ready; have {list(self.devices.values())}")

    def device_for(self, cap) -> Device:
        for d in self.devices.values():
            if d.resumed and cap in d.caps:
                return d
        raise LookupError(f"no resumed device with capability {cap}")

    def _emulate(self, dev: Device):
        if not dev.emulating:
            ei_device_start_emulating(dev.ptr, self._seq)
            self._seq += 1
            dev.emulating = True

    def _frame(self, dev: Device):
        ei_device_frame(dev.ptr, ei_now(self.ei))
        self.pump(0)

    def move_abs(self, x, y):
        dev = self.device_for(CAP_POINTER_ABSOLUTE)
        self._emulate(dev)
        ei_device_pointer_motion_absolute(dev.ptr, x, y)
        self._frame(dev)

    def button(self, code, press):
        dev = self.device_for(CAP_BUTTON)
        self._emulate(dev)
        ei_device_button_button(dev.ptr, code, press)
        self._frame(dev)

    def scroll(self, dx, dy):
        """dx/dy in discrete wheel clicks (positive = down/right)."""
        dev = self.device_for(CAP_SCROLL)
        self._emulate(dev)
        ei_device_scroll_discrete(dev.ptr, int(dx * 120), int(dy * 120))
        self._frame(dev)

    def key(self, keycode, press):
        """keycode is an evdev keycode (KEY_*), not offset by 8."""
        dev = self.device_for(CAP_KEYBOARD)
        self._emulate(dev)
        ei_device_keyboard_key(dev.ptr, keycode, press)
        self._frame(dev)

    def close(self):
        if self.ei:
            ei_unref(self.ei)
            self.ei = None
