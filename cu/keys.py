"""evdev keycodes and a US-layout character map."""

KEYS = {
    "esc": 1, "escape": 1,
    "1": 2, "2": 3, "3": 4, "4": 5, "5": 6, "6": 7, "7": 8, "8": 9, "9": 10, "0": 11,
    "minus": 12, "equal": 13, "backspace": 14, "tab": 15,
    "q": 16, "w": 17, "e": 18, "r": 19, "t": 20, "y": 21, "u": 22, "i": 23, "o": 24, "p": 25,
    "leftbrace": 26, "rightbrace": 27, "enter": 28, "return": 28,
    "ctrl": 29, "control": 29, "leftctrl": 29,
    "a": 30, "s": 31, "d": 32, "f": 33, "g": 34, "h": 35, "j": 36, "k": 37, "l": 38,
    "semicolon": 39, "apostrophe": 40, "grave": 41, "shift": 42, "leftshift": 42, "backslash": 43,
    "z": 44, "x": 45, "c": 46, "v": 47, "b": 48, "n": 49, "m": 50,
    "comma": 51, "dot": 52, "period": 52, "slash": 53, "rightshift": 54, "kpasterisk": 55,
    "alt": 56, "leftalt": 56, "space": 57, "capslock": 58,
    "f1": 59, "f2": 60, "f3": 61, "f4": 62, "f5": 63, "f6": 64, "f7": 65, "f8": 66, "f9": 67,
    "f10": 68, "f11": 87, "f12": 88,
    "rightctrl": 97, "rightalt": 100, "altgr": 100,
    "home": 102, "up": 103, "pageup": 104, "pgup": 104, "left": 105, "right": 106,
    "end": 107, "down": 108, "pagedown": 109, "pgdn": 109, "insert": 110, "delete": 111, "del": 111,
    "super": 125, "meta": 125, "win": 125, "cmd": 125, "leftmeta": 125, "rightmeta": 126,
    "menu": 139, "print": 99, "printscreen": 99,
    "mute": 113, "volumedown": 114, "volumeup": 115,
}

_UNSHIFTED = "1234567890-=qwertyuiop[]asdfghjkl;'`\\zxcvbnm,./ \t\n"
_SHIFTED = "!@#$%^&*()_+QWERTYUIOP{}ASDFGHJKL:\"~|ZXCVBNM<>?"
_CODES = ([2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]
          + [16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27]
          + [30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 43]
          + [44, 45, 46, 47, 48, 49, 50, 51, 52, 53])

CHARS = {}
for ch, code in zip(_UNSHIFTED, _CODES + [57, 15, 28]):
    CHARS[ch] = (code, False)
for ch, code in zip(_SHIFTED, _CODES):
    CHARS[ch] = (code, True)

BUTTONS = {"left": 0x110, "right": 0x111, "middle": 0x112, "back": 0x113, "forward": 0x114}


def parse_combo(combo: str) -> list[int]:
    """'ctrl+shift+t' -> [29, 42, 20]."""
    codes = []
    for part in combo.lower().replace(" ", "").split("+"):
        if not part:
            continue
        if part in KEYS:
            codes.append(KEYS[part])
        elif len(part) == 1 and part in CHARS and not CHARS[part][1]:
            codes.append(CHARS[part][0])
        else:
            raise ValueError(f"unknown key {part!r}")
    return codes
