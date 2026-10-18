//! evdev keycodes and a US-layout character map.

use anyhow::{Result, bail};

pub const SHIFT: u32 = 42;

pub fn named(name: &str) -> Option<u32> {
    Some(match name {
        "esc" | "escape" => 1,
        "minus" => 12,
        "equal" => 13,
        "backspace" => 14,
        "tab" => 15,
        "leftbrace" => 26,
        "rightbrace" => 27,
        "enter" | "return" => 28,
        "ctrl" | "control" | "leftctrl" => 29,
        "semicolon" => 39,
        "apostrophe" => 40,
        "grave" => 41,
        "shift" | "leftshift" => 42,
        "backslash" => 43,
        "comma" => 51,
        "dot" | "period" => 52,
        "slash" => 53,
        "rightshift" => 54,
        "alt" | "leftalt" => 56,
        "space" => 57,
        "capslock" => 58,
        "f1" => 59,
        "f2" => 60,
        "f3" => 61,
        "f4" => 62,
        "f5" => 63,
        "f6" => 64,
        "f7" => 65,
        "f8" => 66,
        "f9" => 67,
        "f10" => 68,
        "f11" => 87,
        "f12" => 88,
        "rightctrl" => 97,
        "print" | "printscreen" => 99,
        "rightalt" | "altgr" => 100,
        "home" => 102,
        "up" => 103,
        "pageup" | "pgup" => 104,
        "left" => 105,
        "right" => 106,
        "end" => 107,
        "down" => 108,
        "pagedown" | "pgdn" => 109,
        "insert" => 110,
        "delete" | "del" => 111,
        "mute" => 113,
        "volumedown" => 114,
        "volumeup" => 115,
        "super" | "meta" | "win" | "cmd" | "leftmeta" => 125,
        "rightmeta" => 126,
        "menu" => 139,
        _ => return None,
    })
}

const ROWS: &[(&str, &str, &[u32])] = &[
    ("1234567890-=", "!@#$%^&*()_+", &[2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]),
    ("qwertyuiop[]", "QWERTYUIOP{}", &[16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27]),
    ("asdfghjkl;'`\\", "ASDFGHJKL:\"~|", &[30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 43]),
    ("zxcvbnm,./", "ZXCVBNM<>?", &[44, 45, 46, 47, 48, 49, 50, 51, 52, 53]),
];

/// (keycode, needs_shift) for a character on a US layout.
pub fn char_key(ch: char) -> Option<(u32, bool)> {
    match ch {
        ' ' => return Some((57, false)),
        '\t' => return Some((15, false)),
        '\n' => return Some((28, false)),
        _ => {}
    }
    for (lo, hi, codes) in ROWS {
        if let Some(i) = lo.chars().position(|c| c == ch) {
            return Some((codes[i], false));
        }
        if let Some(i) = hi.chars().position(|c| c == ch) {
            return Some((codes[i], true));
        }
    }
    None
}

/// "ctrl+shift+t" -> [29, 42, 20]
pub fn parse_combo(combo: &str) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    for part in combo.to_lowercase().split('+').map(str::trim).filter(|p| !p.is_empty()) {
        if let Some(k) = named(part) {
            out.push(k);
        } else if let (Some(ch), 1) = (part.chars().next(), part.chars().count()) {
            match char_key(ch) {
                Some((k, false)) => out.push(k),
                _ => bail!("unknown key {part:?}"),
            }
        } else {
            bail!("unknown key {part:?}");
        }
    }
    if out.is_empty() {
        bail!("empty key combo");
    }
    Ok(out)
}

pub fn button(name: &str) -> Result<u32> {
    Ok(match name {
        "left" => 0x110,
        "right" => 0x111,
        "middle" => 0x112,
        "back" => 0x113,
        "forward" => 0x114,
        _ => bail!("unknown button {name:?}"),
    })
}
